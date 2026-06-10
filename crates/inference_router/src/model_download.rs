//! Lazy SLM weight download with SHA-256 verification and progress
//! reporting.
//!
//! The SLM weights (~248 MB MLX on iOS / macOS, ~237 MB GGUF on
//! Android / Windows) are **not** bundled in the app installer. They
//! are fetched on demand the first time synthesis is triggered, so a
//! device that never reaches the synthesis tier never pays the
//! download. This module owns the parts of that flow that must be
//! identical on every platform:
//!
//! * **Verification** — the downloaded bytes are hashed with SHA-256
//!   and compared against the pinned [`ModelSource::expected_sha256`].
//!   A verified-wrong artifact is deleted, never consumed (mirroring
//!   `scripts/download-models.sh`). For a fleet of 5000 SME tenants
//!   this is the line between "lazy-load a model" and "execute
//!   attacker-substituted weights", so the check is mandatory whenever
//!   a hash is pinned.
//! * **Atomicity** — bytes stream into a `*.partial` sidecar and are
//!   only `rename`d into place after verification succeeds, so a
//!   crashed or interrupted download can never leave a truncated file
//!   that a later run mistakes for a complete model.
//! * **Progress** — a [`ModelDownloadProgress`] callback is invoked
//!   with `(bytes_downloaded, total_bytes)` so the host can render a
//!   one-time progress bar instead of a generic "Unavailable".
//!
//! The **byte transport** is deliberately abstracted behind
//! [`ModelFetcher`] so this logic is unit-testable without a network
//! and so each platform supplies the transport that fits its binary
//! budget: desktop / server builds wire the reqwest-backed
//! [`ReqwestFetcher`] automatically (only compiled under the
//! `http-client` feature), while mobile builds — which deliberately
//! exclude the reqwest + TLS stack to keep the artifact small —
//! provision the weights out-of-band (host-managed download / on-demand
//! resources) and skip the in-process fetch.

use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use sha2::{Digest, Sha256};

/// Read/hash/write chunk size. 1 MiB balances syscall overhead against
/// the transient buffer footprint on Low-tier devices (the download
/// runs on the bootstrap thread, where every megabyte is contended).
const DOWNLOAD_CHUNK_BYTES: usize = 1024 * 1024;

/// Progress callback invoked as bytes arrive: `(downloaded, total)`.
///
/// `total` is `0` when the server did not advertise a `Content-Length`
/// (chunked transfer); hosts should render an indeterminate spinner in
/// that case rather than dividing by zero — see [`progress_pct`].
pub type ModelDownloadProgress = Arc<dyn Fn(u64, u64) + Send + Sync>;

/// Where to fetch the SLM weights and the pinned hash to verify the
/// download against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelSource {
    /// Fully-qualified download URL for the platform's weight artifact.
    pub url: String,
    /// Pinned lowercase hex SHA-256. When `Some`, a mismatch aborts the
    /// download and deletes the partial file. When `None`, the bytes
    /// are accepted unverified — only appropriate for trusted-LAN /
    /// development sources, never for the public CDN defaults.
    pub expected_sha256: Option<String>,
}

/// Failure modes for [`download_and_verify`].
#[derive(Debug, thiserror::Error)]
pub enum DownloadError {
    /// The transport (HTTP client) failed to open or read the stream.
    #[error("model download transport error: {0}")]
    Transport(String),

    /// A local filesystem operation failed (create dir, write, rename).
    #[error("model download I/O error: {0}")]
    Io(#[from] io::Error),

    /// The download completed but its SHA-256 did not match the pinned
    /// value. The partial file has already been removed.
    #[error("model checksum mismatch: expected {expected}, got {actual}")]
    ChecksumMismatch {
        /// The pinned hash from [`ModelSource::expected_sha256`].
        expected: String,
        /// The hash actually computed over the downloaded bytes.
        actual: String,
    },

    /// No in-process transport is compiled into this build (the
    /// `http-client` feature is off, e.g. on mobile). The host must
    /// provision the weights out-of-band.
    #[error(
        "in-process model download is unavailable in this build \
         (the `http-client` feature is disabled); provision weights out-of-band"
    )]
    Unsupported,
}

/// A streaming byte source for a single download.
///
/// Implemented over the real HTTP response on desktop ([`ReqwestFetcher`])
/// and over in-memory bytes in tests, so the verify / progress / rename
/// pipeline in [`download_and_verify`] is exercised identically with and
/// without a network.
pub trait ModelByteStream: Read + Send {
    /// Total content length in bytes, if the transport advertised one.
    fn content_length(&self) -> Option<u64>;
}

/// Abstraction over the HTTP transport. Keeps [`download_and_verify`]
/// network-free for unit tests and lets each platform supply (or omit)
/// a transport that fits its binary budget.
pub trait ModelFetcher: Send + Sync {
    /// Open a streaming read over `url`.
    fn open(&self, url: &str) -> Result<Box<dyn ModelByteStream>, DownloadError>;
}

/// The `*.partial` sidecar path bytes stream into before verification.
fn partial_path(dest: &Path) -> PathBuf {
    let mut s = dest.as_os_str().to_os_string();
    s.push(".partial");
    PathBuf::from(s)
}

/// RAII cleanup for the `*.partial` sidecar.
///
/// Removes the partial file when dropped unless [`Self::disarm`] has
/// been called (which happens only after the verified bytes have been
/// renamed into their final destination). This guarantees that *every*
/// failure path — transport read error, write/flush I/O error,
/// checksum mismatch, or a failed final `rename` — leaves no orphaned
/// `.partial` behind. On low-storage mobile devices an abandoned
/// hundreds-of-MB sidecar from a flaky transfer could otherwise wedge
/// the next download attempt, so the cleanup must be unconditional
/// rather than per-error-site.
struct PartialGuard<'a> {
    path: &'a Path,
    armed: bool,
}

impl<'a> PartialGuard<'a> {
    fn new(path: &'a Path) -> Self {
        Self { path, armed: true }
    }

    /// Hand ownership of the file to the caller: the bytes are now the
    /// real artifact (renamed into place), so the sidecar must NOT be
    /// removed.
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for PartialGuard<'_> {
    fn drop(&mut self) {
        if self.armed {
            // Best-effort: a failure to remove the sidecar must not
            // mask the original error that triggered the unwind.
            let _ = fs::remove_file(self.path);
        }
    }
}

/// Lowercase hex-encode a SHA-256 digest.
fn hex_encode(bytes: impl AsRef<[u8]>) -> String {
    let bytes = bytes.as_ref();
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(char::from_digit((b >> 4) as u32, 16).expect("nibble < 16"));
        out.push(char::from_digit((b & 0x0f) as u32, 16).expect("nibble < 16"));
    }
    out
}

/// Integer percentage (0–100) from `downloaded` / `total`.
///
/// Returns `0` when `total` is unknown (`0`) so the FFI surface can
/// report an indeterminate "downloading" state without dividing by
/// zero; clamps to `100` to absorb a server that streams slightly more
/// than its advertised `Content-Length`.
pub fn progress_pct(downloaded: u64, total: u64) -> u8 {
    if total == 0 {
        return 0;
    }
    // Widen to u128 before the `* 100` so the multiply is exact for
    // every `u64` input. A `u64` `saturating_mul(100)` saturates once
    // `downloaded > u64::MAX / 100` (~0.18 EB) and the subsequent
    // division would then *under*-report — e.g. a complete
    // `downloaded == total` near `u64::MAX` could read back < 100%.
    // Unreachable for ~248 MB weights, but progress_pct is a pure,
    // reusable helper, so it stays correct across the whole domain.
    let pct = (u128::from(downloaded) * 100 / u128::from(total)).min(100);
    // `pct` is clamped to `0..=100`, so the narrowing always succeeds;
    // `try_from` keeps it clippy-clean (no lossy `as` cast) and the
    // `unwrap_or(100)` is unreachable defence-in-depth.
    u8::try_from(pct).unwrap_or(100)
}

/// Stream `source.url` into `dest`, hashing as we go, verifying the
/// pinned SHA-256 (when present), and atomically renaming into place.
///
/// On a checksum mismatch the partial file is removed — a
/// verified-wrong artifact is untrustworthy and must never be left on
/// disk where a later run could consume it (mirrors the delete-on-
/// mismatch behaviour of `scripts/download-models.sh`).
///
/// `progress` (when supplied) is called once with `(0, total)` before
/// the first byte and after every chunk, so a host can paint a
/// determinate progress bar (or an indeterminate spinner when `total`
/// is `0`).
pub fn download_and_verify(
    dest: &Path,
    source: &ModelSource,
    fetcher: &dyn ModelFetcher,
    progress: Option<&ModelDownloadProgress>,
) -> Result<(), DownloadError> {
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    let partial = partial_path(dest);

    let mut stream = fetcher.open(&source.url)?;
    let total = stream.content_length().unwrap_or(0);

    // Arm the cleanup *before* the file is created so that any early
    // return below — transport read error, write/flush failure,
    // checksum mismatch, or the final `rename` — drops the guard and
    // removes the sidecar. It is disarmed only once the verified bytes
    // are the real artifact on disk.
    let mut guard = PartialGuard::new(&partial);

    // Scope the file handle so it is flushed + closed before the
    // rename (Windows refuses to rename an open file). Because `guard`
    // is declared *outside* this block, an early `?` return unwinds the
    // block first (closing `file`) and only then drops `guard`, so the
    // sidecar is always removed after its handle is closed — safe on
    // Windows too.
    {
        let mut file = fs::File::create(&partial)?;
        let mut hasher = Sha256::new();
        let mut downloaded: u64 = 0;
        let mut buf = vec![0u8; DOWNLOAD_CHUNK_BYTES];

        if let Some(cb) = progress {
            cb(0, total);
        }
        loop {
            let n = stream
                .read(&mut buf)
                .map_err(|e| DownloadError::Transport(e.to_string()))?;
            if n == 0 {
                break;
            }
            file.write_all(&buf[..n])?;
            hasher.update(&buf[..n]);
            downloaded = downloaded.saturating_add(n as u64);
            if let Some(cb) = progress {
                cb(downloaded, total);
            }
        }
        file.flush()?;

        if let Some(expected) = source.expected_sha256.as_deref() {
            let actual = hex_encode(hasher.finalize());
            if !actual.eq_ignore_ascii_case(expected) {
                // A verified-wrong artifact is untrustworthy: the guard
                // removes the sidecar on the early return below (the
                // `file` handle is dropped first as the block unwinds,
                // so the remove is Windows-safe).
                return Err(DownloadError::ChecksumMismatch {
                    expected: expected.to_owned(),
                    actual,
                });
            }
        }
    }

    fs::rename(&partial, dest)?;
    // The bytes are now the real artifact — keep them.
    guard.disarm();
    Ok(())
}

// ───────────────────────── reqwest transport ─────────────────────────

#[cfg(feature = "http-client")]
mod reqwest_transport {
    use super::{DownloadError, ModelByteStream, ModelFetcher};
    use std::io::Read;

    /// Streaming wrapper around a blocking reqwest [`Response`].
    ///
    /// [`reqwest::blocking::Response`] implements [`Read`], so the
    /// download loop pulls bytes straight off the socket without
    /// buffering the whole (hundreds-of-MB) artifact in memory.
    ///
    /// [`Response`]: reqwest::blocking::Response
    pub struct ReqwestStream {
        resp: reqwest::blocking::Response,
        content_length: Option<u64>,
    }

    impl Read for ReqwestStream {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            self.resp.read(buf)
        }
    }

    impl ModelByteStream for ReqwestStream {
        fn content_length(&self) -> Option<u64> {
            self.content_length
        }
    }

    /// Real HTTP transport for [`super::download_and_verify`], backed by
    /// reqwest's blocking client. Only compiled under the `http-client`
    /// feature, which desktop / server builds enable and mobile builds
    /// deliberately omit.
    #[derive(Debug, Default)]
    pub struct ReqwestFetcher {
        client: reqwest::blocking::Client,
    }

    impl ReqwestFetcher {
        /// Construct a fetcher with a default blocking client.
        pub fn new() -> Self {
            Self::default()
        }
    }

    impl ModelFetcher for ReqwestFetcher {
        fn open(&self, url: &str) -> Result<Box<dyn ModelByteStream>, DownloadError> {
            let resp = self
                .client
                .get(url)
                .send()
                .map_err(|e| DownloadError::Transport(e.to_string()))?
                .error_for_status()
                .map_err(|e| DownloadError::Transport(e.to_string()))?;
            let content_length = resp.content_length();
            Ok(Box::new(ReqwestStream {
                resp,
                content_length,
            }))
        }
    }
}

#[cfg(feature = "http-client")]
pub use reqwest_transport::ReqwestFetcher;

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// In-memory fetcher: serves fixed bytes, optionally hiding the
    /// content length to exercise the chunked-transfer path.
    struct FakeFetcher {
        body: Vec<u8>,
        advertise_length: bool,
    }

    struct FakeStream {
        cursor: Cursor<Vec<u8>>,
        content_length: Option<u64>,
    }

    impl Read for FakeStream {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            self.cursor.read(buf)
        }
    }

    impl ModelByteStream for FakeStream {
        fn content_length(&self) -> Option<u64> {
            self.content_length
        }
    }

    impl ModelFetcher for FakeFetcher {
        fn open(&self, _url: &str) -> Result<Box<dyn ModelByteStream>, DownloadError> {
            Ok(Box::new(FakeStream {
                cursor: Cursor::new(self.body.clone()),
                content_length: self.advertise_length.then_some(self.body.len() as u64),
            }))
        }
    }

    fn sha256_hex(bytes: &[u8]) -> String {
        hex_encode(Sha256::digest(bytes))
    }

    /// A stream that serves `ok_bytes` of real data and then fails the
    /// next `read` with an I/O error, modelling a transport that drops
    /// mid-transfer (connection reset, TLS error, etc.).
    struct FlakyStream {
        served: Vec<u8>,
        cursor: usize,
        ok_bytes: usize,
        content_length: Option<u64>,
    }

    impl Read for FlakyStream {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.cursor >= self.ok_bytes {
                return Err(io::Error::new(
                    io::ErrorKind::ConnectionReset,
                    "transport dropped mid-download",
                ));
            }
            let end = (self.cursor + buf.len()).min(self.ok_bytes);
            let n = end - self.cursor;
            buf[..n].copy_from_slice(&self.served[self.cursor..end]);
            self.cursor += n;
            Ok(n)
        }
    }

    impl ModelByteStream for FlakyStream {
        fn content_length(&self) -> Option<u64> {
            self.content_length
        }
    }

    struct FlakyFetcher {
        body: Vec<u8>,
        ok_bytes: usize,
    }

    impl ModelFetcher for FlakyFetcher {
        fn open(&self, _url: &str) -> Result<Box<dyn ModelByteStream>, DownloadError> {
            Ok(Box::new(FlakyStream {
                served: self.body.clone(),
                cursor: 0,
                ok_bytes: self.ok_bytes,
                content_length: Some(self.body.len() as u64),
            }))
        }
    }

    #[test]
    fn transport_error_mid_download_removes_partial() {
        // A transport that drops mid-stream must NOT leave an orphaned
        // `.partial` sidecar behind — otherwise a flaky network on a
        // low-storage device could strand hundreds of MB and wedge the
        // retry. The `PartialGuard` removes it on the error unwind.
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("nested").join("slm.gguf");
        let fetcher = FlakyFetcher {
            body: vec![0xAB; 4 * DOWNLOAD_CHUNK_BYTES],
            ok_bytes: DOWNLOAD_CHUNK_BYTES + 17, // a bit past the first chunk
        };
        let source = ModelSource {
            url: "https://example.invalid/slm.gguf".into(),
            expected_sha256: Some(sha256_hex(b"never reached")),
        };

        let err = download_and_verify(&dest, &source, &fetcher, None).unwrap_err();
        assert!(
            matches!(err, DownloadError::Transport(_)),
            "expected Transport error, got {err:?}"
        );
        assert!(!dest.exists(), "no artifact lands on a transport failure");
        assert!(
            !partial_path(&dest).exists(),
            "the .partial sidecar must be removed when the transport drops"
        );
    }

    #[test]
    fn downloads_verifies_and_renames_into_place() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("nested").join("slm.gguf");
        let body = b"the quick brown fox weights".to_vec();
        let fetcher = FakeFetcher {
            body: body.clone(),
            advertise_length: true,
        };
        let source = ModelSource {
            url: "https://example.invalid/slm.gguf".into(),
            expected_sha256: Some(sha256_hex(&body)),
        };

        let seen = Arc::new(AtomicU64::new(0));
        let seen_total = Arc::new(AtomicU64::new(u64::MAX));
        let s = Arc::clone(&seen);
        let st = Arc::clone(&seen_total);
        let progress: ModelDownloadProgress = Arc::new(move |downloaded, total| {
            s.store(downloaded, Ordering::SeqCst);
            st.store(total, Ordering::SeqCst);
        });

        download_and_verify(&dest, &source, &fetcher, Some(&progress)).unwrap();

        assert_eq!(
            fs::read(&dest).unwrap(),
            body,
            "verified bytes land at dest"
        );
        assert!(
            !partial_path(&dest).exists(),
            "the .partial sidecar must be renamed away"
        );
        assert_eq!(
            seen.load(Ordering::SeqCst),
            body.len() as u64,
            "final progress callback reports the full byte count"
        );
        assert_eq!(seen_total.load(Ordering::SeqCst), body.len() as u64);
    }

    #[test]
    fn checksum_mismatch_aborts_and_deletes_partial() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("slm.gguf");
        let fetcher = FakeFetcher {
            body: b"attacker-substituted weights".to_vec(),
            advertise_length: true,
        };
        let source = ModelSource {
            url: "https://example.invalid/slm.gguf".into(),
            // Pin a hash of *different* bytes so verification fails.
            expected_sha256: Some(sha256_hex(b"the legitimate weights")),
        };

        let err = download_and_verify(&dest, &source, &fetcher, None).unwrap_err();
        assert!(
            matches!(err, DownloadError::ChecksumMismatch { .. }),
            "expected ChecksumMismatch, got {err:?}"
        );
        assert!(
            !dest.exists(),
            "a verified-wrong artifact must not land at dest"
        );
        assert!(
            !partial_path(&dest).exists(),
            "the .partial sidecar must be deleted on mismatch"
        );
    }

    #[test]
    fn unverified_source_is_accepted_when_no_hash_pinned() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("slm.gguf");
        let body = b"dev-only unpinned weights".to_vec();
        let fetcher = FakeFetcher {
            body: body.clone(),
            advertise_length: false, // chunked: no Content-Length
        };
        let source = ModelSource {
            url: "https://example.invalid/slm.gguf".into(),
            expected_sha256: None,
        };

        download_and_verify(&dest, &source, &fetcher, None).unwrap();
        assert_eq!(fs::read(&dest).unwrap(), body);
    }

    #[test]
    fn progress_pct_is_bounded_and_zero_when_total_unknown() {
        assert_eq!(progress_pct(0, 0), 0, "unknown total → indeterminate 0");
        assert_eq!(progress_pct(50, 0), 0, "unknown total → indeterminate 0");
        assert_eq!(progress_pct(0, 200), 0);
        assert_eq!(progress_pct(100, 200), 50);
        assert_eq!(progress_pct(200, 200), 100);
        assert_eq!(progress_pct(250, 200), 100, "over-read clamps to 100");
        // Extreme byte counts (u128 widening): a `u64` `* 100` would
        // saturate and mis-report here. `downloaded == total` must read
        // exactly 100%, and a half-complete transfer exactly 50%, even
        // at the top of the `u64` domain.
        assert_eq!(
            progress_pct(u64::MAX, u64::MAX),
            100,
            "complete transfer reports 100% even near u64::MAX"
        );
        // `u64::MAX - 1` is even, so `(u64::MAX - 1) / 2` is *exactly*
        // half of it: the u128 math reports a clean 50% where the old
        // `u64` `saturating_mul(100)` would saturate and report 0%.
        let even_max = u64::MAX - 1;
        assert_eq!(
            progress_pct(even_max / 2, even_max),
            50,
            "half-complete reports exactly 50% even near u64::MAX"
        );
    }
}
