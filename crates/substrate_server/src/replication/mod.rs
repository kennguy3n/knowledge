//! Active-passive replication for the SQLCipher substrate.
//!
//! The substrate stores everything in a single SQLCipher (SQLite)
//! database. SQLite cannot scale out horizontally, but it *can* be
//! made highly available through **WAL shipping**: the primary runs in
//! WAL journal mode, and every committed transaction appends frames to
//! the `-wal` sidecar file. This module reads those frames, packages
//! the new ones into [`WalSegment`]s, and ships them over a transport
//! ([`WalBus`]) to one or more standbys, which splice the page images
//! directly into a local copy of the database file and serve read-only
//! queries from it (see [`standby`]). On primary failure a standby
//! wins the [`LeaseStore`] lease and promotes itself (see
//! [`failover`]).
//!
//! The engine is deliberately **transport-agnostic**: the
//! primary/standby/failover loops are generic over the [`WalBus`] and
//! [`LeaseStore`] traits. The default build ships an in-process
//! implementation ([`memory`]) used by the unit tests and by
//! single-host dev setups; the production NATS JetStream + KV backing
//! lives in [`nats`] behind the non-default `replication-nats` feature,
//! so the cross-compile and default builds never link the async-nats /
//! TLS stack.
//!
//! ## WAL format
//!
//! The on-disk layout this module parses and re-emits is the SQLite
//! WAL format documented at <https://sqlite.org/walformat.html>: a
//! 32-byte header followed by zero or more frames, each a 24-byte frame
//! header plus one page of data. Integers in both headers are
//! big-endian; the rolling Fibonacci-style checksum is computed in the
//! byte order named by the header magic. [`parse_wal`] validates the
//! checksum chain and returns only the committed, intact prefix —
//! exactly the frames SQLite itself would recover.

pub mod failover;
pub mod memory;
#[cfg(feature = "replication-nats")]
pub mod nats;
pub mod primary;
pub mod standby;

use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, TimeZone, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::watch;
use tokio::task::JoinHandle;

// ───────────────────────────── Errors ───────────────────────────────

/// Errors raised anywhere in the replication subsystem.
#[derive(Debug, thiserror::Error)]
pub enum ReplError {
    /// A WAL byte stream was shorter than its declared structure, or a
    /// segment frame was truncated on the wire.
    #[error("malformed WAL/segment data: {0}")]
    Malformed(String),
    /// The WAL header magic was neither of the two SQLite sentinels.
    #[error("unrecognised WAL magic: {0:#010x}")]
    BadMagic(u32),
    /// A transport (bus or lease store) operation failed.
    #[error("replication transport error: {0}")]
    Transport(String),
    /// The configured role string was not one of `primary`, `standby`,
    /// `auto`, or `disabled`.
    #[error("invalid replication role `{0}` (expected primary|standby|auto|disabled)")]
    BadRole(String),
    /// Replication was enabled with a static cross-node role
    /// (`primary`/`standby`) but no real transport is active, so the
    /// node would silently fail to replicate. Surfaced at startup so the
    /// misconfiguration fails fast instead of running a substrate that
    /// looks healthy but ships frames into a void.
    #[error("replication misconfigured: {0}")]
    Misconfigured(String),
}

/// Convenience alias for replication results.
pub type ReplResult<T> = Result<T, ReplError>;

// ───────────────────────── WAL binary format ────────────────────────

/// Size of the SQLite WAL header in bytes.
pub const WAL_HEADER_SIZE: usize = 32;
/// Size of a single WAL frame header in bytes (precedes each page).
pub const FRAME_HEADER_SIZE: usize = 24;
/// WAL magic selecting **little-endian** checksum computation.
pub const WAL_MAGIC_LE: u32 = 0x377f_0682;
/// WAL magic selecting **big-endian** checksum computation.
pub const WAL_MAGIC_BE: u32 = 0x377f_0683;
/// `KWL1` — wire magic prefixing an encoded [`WalSegment`].
const SEGMENT_MAGIC: &[u8; 4] = b"KWL1";
/// SQLCipher page size the evidence store opens with (the SQLCipher 4.x
/// default, set explicitly in `evidence_store`). Standby page splicing
/// computes file offsets from this; [`spawn`] asserts the open store
/// matches it so replicated pages can never land at the wrong offset.
const EXPECTED_CIPHER_PAGE_SIZE: u32 = 4096;

/// Byte order used to interpret 32-bit words while checksumming, as
/// selected by the WAL header magic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChecksumOrder {
    /// Words are little-endian (`WAL_MAGIC_LE`).
    Little,
    /// Words are big-endian (`WAL_MAGIC_BE`).
    Big,
}

impl ChecksumOrder {
    fn from_magic(magic: u32) -> ReplResult<Self> {
        match magic {
            WAL_MAGIC_LE => Ok(Self::Little),
            WAL_MAGIC_BE => Ok(Self::Big),
            other => Err(ReplError::BadMagic(other)),
        }
    }

    fn magic(self) -> u32 {
        match self {
            Self::Little => WAL_MAGIC_LE,
            Self::Big => WAL_MAGIC_BE,
        }
    }

    fn read_u32(self, b: &[u8]) -> u32 {
        let arr = [b[0], b[1], b[2], b[3]];
        match self {
            Self::Little => u32::from_le_bytes(arr),
            Self::Big => u32::from_be_bytes(arr),
        }
    }
}

/// The SQLite WAL rolling checksum.
///
/// Interprets `data` (whose length must be a multiple of 8) as pairs of
/// 32-bit words in `order` and folds them into the running `(s0, s1)`
/// state using the algorithm from the WAL format spec:
///
/// ```text
/// s0 += x[i]   + s1
/// s1 += x[i+1] + s0
/// ```
///
/// with wrapping (mod 2³²) arithmetic. The caller seeds the state with
/// `(0, 0)` for the header and with the previous frame's checksum for
/// each subsequent frame, forming an integrity chain.
#[must_use]
pub fn wal_checksum(order: ChecksumOrder, init: (u32, u32), data: &[u8]) -> (u32, u32) {
    debug_assert_eq!(data.len() % 8, 0, "checksum input must be 8-byte aligned");
    let (mut s0, mut s1) = init;
    let mut i = 0;
    while i + 8 <= data.len() {
        let x0 = order.read_u32(&data[i..i + 4]);
        let x1 = order.read_u32(&data[i + 4..i + 8]);
        s0 = s0.wrapping_add(x0).wrapping_add(s1);
        s1 = s1.wrapping_add(x1).wrapping_add(s0);
        i += 8;
    }
    (s0, s1)
}

/// Parsed WAL header fields needed to validate and re-emit frames.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WalHeader {
    /// Checksum byte order selected by the header magic.
    pub order: ChecksumOrder,
    /// Database page size in bytes (a power of two; the SQLite sentinel
    /// `1` meaning 65536 is normalised here to `65536`).
    pub page_size: u32,
    /// Checkpoint sequence number.
    pub checkpoint_seq: u32,
    /// Salt-1, bumped on every checkpoint/WAL reset.
    pub salt1: u32,
    /// Salt-2, randomised on every checkpoint/WAL reset.
    pub salt2: u32,
    /// Running checksum over the first 24 header bytes; seeds frame 0.
    pub checksum: (u32, u32),
}

/// A single WAL frame: a page image plus the commit marker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WalFrame {
    /// 1-based page number this frame rewrites.
    pub page_number: u32,
    /// For a commit frame, the database size in pages after the commit;
    /// `0` for a non-commit frame. See [`WalFrame::is_commit`].
    pub db_size_after_commit: u32,
    /// Raw page image, exactly `page_size` bytes.
    pub page_data: Vec<u8>,
}

impl WalFrame {
    /// Whether this frame commits a transaction (non-zero post-commit
    /// database size). WAL shipping is done at transaction granularity,
    /// so segments always end on a commit frame.
    #[must_use]
    pub fn is_commit(&self) -> bool {
        self.db_size_after_commit != 0
    }
}

/// The committed, checksum-valid prefix of a WAL file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedWal {
    /// Decoded header.
    pub header: WalHeader,
    /// Frames whose checksum chain validated, in file order.
    pub frames: Vec<WalFrame>,
}

/// Parse and validate a SQLite WAL byte stream.
///
/// Returns the header plus the **valid prefix** of frames: iteration
/// stops at the first frame whose salts do not match the header or
/// whose checksum does not chain, mirroring how SQLite recovers a WAL.
/// A stream containing only a header (no complete frame) yields an
/// empty `frames` vector.
///
/// # Errors
///
/// Returns [`ReplError::Malformed`] if the stream is shorter than a WAL
/// header or declares an implausible page size, or [`ReplError::BadMagic`]
/// if the header magic is unrecognised.
pub fn parse_wal(bytes: &[u8]) -> ReplResult<ParsedWal> {
    if bytes.len() < WAL_HEADER_SIZE {
        return Err(ReplError::Malformed(format!(
            "WAL is {} bytes, shorter than the {WAL_HEADER_SIZE}-byte header",
            bytes.len()
        )));
    }
    let magic = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
    let order = ChecksumOrder::from_magic(magic)?;
    let raw_page_size = u32::from_be_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]);
    // SQLite encodes a 65536-byte page as the sentinel value 1.
    let page_size = if raw_page_size == 1 {
        65536
    } else {
        raw_page_size
    };
    if page_size < 512 || !page_size.is_power_of_two() {
        return Err(ReplError::Malformed(format!(
            "implausible WAL page size {page_size}"
        )));
    }
    let checkpoint_seq = u32::from_be_bytes([bytes[12], bytes[13], bytes[14], bytes[15]]);
    let salt1 = u32::from_be_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]);
    let salt2 = u32::from_be_bytes([bytes[20], bytes[21], bytes[22], bytes[23]]);
    let stored_hdr_ck = (
        u32::from_be_bytes([bytes[24], bytes[25], bytes[26], bytes[27]]),
        u32::from_be_bytes([bytes[28], bytes[29], bytes[30], bytes[31]]),
    );
    // The header checksum chains from (0, 0) over its own first 24
    // bytes; a mismatch means the header itself is corrupt.
    let computed_hdr_ck = wal_checksum(order, (0, 0), &bytes[0..24]);
    if computed_hdr_ck != stored_hdr_ck {
        return Err(ReplError::Malformed(
            "WAL header checksum mismatch".to_string(),
        ));
    }

    let header = WalHeader {
        order,
        page_size,
        checkpoint_seq,
        salt1,
        salt2,
        checksum: stored_hdr_ck,
    };

    let frame_size = FRAME_HEADER_SIZE + page_size as usize;
    let mut frames = Vec::new();
    let mut running = stored_hdr_ck;
    let mut offset = WAL_HEADER_SIZE;
    while offset + frame_size <= bytes.len() {
        let fh = &bytes[offset..offset + FRAME_HEADER_SIZE];
        let page_number = u32::from_be_bytes([fh[0], fh[1], fh[2], fh[3]]);
        let db_size = u32::from_be_bytes([fh[4], fh[5], fh[6], fh[7]]);
        let fsalt1 = u32::from_be_bytes([fh[8], fh[9], fh[10], fh[11]]);
        let fsalt2 = u32::from_be_bytes([fh[12], fh[13], fh[14], fh[15]]);
        let stored_ck = (
            u32::from_be_bytes([fh[16], fh[17], fh[18], fh[19]]),
            u32::from_be_bytes([fh[20], fh[21], fh[22], fh[23]]),
        );
        // A frame belongs to this WAL generation only if its salts copy
        // the header's; a mismatch marks the end of the valid region.
        if fsalt1 != salt1 || fsalt2 != salt2 {
            break;
        }
        let page = &bytes[offset + FRAME_HEADER_SIZE..offset + frame_size];
        // Frame checksum chains over the first 8 header bytes then the
        // page image, seeded by the previous checksum in the chain.
        let after_prefix = wal_checksum(order, running, &fh[0..8]);
        let computed = wal_checksum(order, after_prefix, page);
        if computed != stored_ck {
            break;
        }
        running = computed;
        frames.push(WalFrame {
            page_number,
            db_size_after_commit: db_size,
            page_data: page.to_vec(),
        });
        offset += frame_size;
    }

    Ok(ParsedWal { header, frames })
}

/// Encode a header + frames back into a valid SQLite WAL byte stream,
/// recomputing every checksum so the result round-trips through
/// [`parse_wal`]. Used by the standby to materialise a shadow WAL from
/// the [`WalSegment`]s it receives.
///
/// # Errors
///
/// Returns [`ReplError::Malformed`] if any frame's page image length
/// does not match `header.page_size`.
pub fn encode_wal(header: &WalHeader, frames: &[WalFrame]) -> ReplResult<Vec<u8>> {
    let order = header.order;
    // Each frame contributes a 24-byte frame header plus a full page
    // image; sizing on `* 8` under-reserved by ~500x at a 4 KiB page
    // and forced a chain of reallocations on this hot path (every
    // primary poll, and the standby's shadow-WAL rebuild).
    let frame_size = FRAME_HEADER_SIZE + header.page_size as usize;
    let mut out = Vec::with_capacity(WAL_HEADER_SIZE + frames.len() * frame_size);
    out.extend_from_slice(&order.magic().to_be_bytes());
    out.extend_from_slice(&3_007_000u32.to_be_bytes());
    let stored_page_size = if header.page_size == 65536 {
        1
    } else {
        header.page_size
    };
    out.extend_from_slice(&stored_page_size.to_be_bytes());
    out.extend_from_slice(&header.checkpoint_seq.to_be_bytes());
    out.extend_from_slice(&header.salt1.to_be_bytes());
    out.extend_from_slice(&header.salt2.to_be_bytes());
    let hdr_ck = wal_checksum(order, (0, 0), &out[0..24]);
    out.extend_from_slice(&hdr_ck.0.to_be_bytes());
    out.extend_from_slice(&hdr_ck.1.to_be_bytes());

    let mut running = hdr_ck;
    for frame in frames {
        if frame.page_data.len() != header.page_size as usize {
            return Err(ReplError::Malformed(format!(
                "frame page is {} bytes, expected page_size {}",
                frame.page_data.len(),
                header.page_size
            )));
        }
        let mut fh = [0u8; FRAME_HEADER_SIZE];
        fh[0..4].copy_from_slice(&frame.page_number.to_be_bytes());
        fh[4..8].copy_from_slice(&frame.db_size_after_commit.to_be_bytes());
        fh[8..12].copy_from_slice(&header.salt1.to_be_bytes());
        fh[12..16].copy_from_slice(&header.salt2.to_be_bytes());
        let after_prefix = wal_checksum(order, running, &fh[0..8]);
        let computed = wal_checksum(order, after_prefix, &frame.page_data);
        running = computed;
        fh[16..20].copy_from_slice(&computed.0.to_be_bytes());
        fh[20..24].copy_from_slice(&computed.1.to_be_bytes());
        out.extend_from_slice(&fh);
        out.extend_from_slice(&frame.page_data);
    }
    Ok(out)
}

// ─────────────────────────── WAL segments ───────────────────────────

/// A batch of new WAL frames shipped from primary to standby as one
/// unit. A segment always ends on a commit frame, so applying it leaves
/// the standby on a transaction boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WalSegment {
    /// Monotonic per-primary sequence number (first segment is `1`).
    pub seq: u64,
    /// Total frames shipped by the primary up to **and including** this
    /// segment. Standbys use it as the primary's frame watermark to
    /// compute replication lag without a side channel.
    pub cumulative_frames: u64,
    /// Page size of the source WAL, needed to rebuild the shadow WAL.
    pub page_size: u32,
    /// Source WAL salt-1 (identifies the WAL generation).
    pub salt1: u32,
    /// Source WAL salt-2.
    pub salt2: u32,
    /// Frames carried by this segment, in commit order.
    pub frames: Vec<WalFrame>,
}

impl WalSegment {
    /// Number of frames in this segment.
    #[must_use]
    pub fn frame_count(&self) -> u64 {
        self.frames.len() as u64
    }

    /// Serialise to the self-describing `KWL1` wire format.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        // 36-byte fixed header (see `FIXED` in `decode`) + 8 bytes of
        // per-frame metadata and one page image per frame.
        let mut out = Vec::with_capacity(36 + self.frames.len() * (8 + self.page_size as usize));
        out.extend_from_slice(SEGMENT_MAGIC);
        out.extend_from_slice(&self.seq.to_be_bytes());
        out.extend_from_slice(&self.cumulative_frames.to_be_bytes());
        out.extend_from_slice(&self.page_size.to_be_bytes());
        out.extend_from_slice(&self.salt1.to_be_bytes());
        out.extend_from_slice(&self.salt2.to_be_bytes());
        let frame_count = u32::try_from(self.frames.len())
            .expect("a WAL segment never carries more than u32::MAX frames");
        out.extend_from_slice(&frame_count.to_be_bytes());
        for frame in &self.frames {
            out.extend_from_slice(&frame.page_number.to_be_bytes());
            out.extend_from_slice(&frame.db_size_after_commit.to_be_bytes());
            out.extend_from_slice(&frame.page_data);
        }
        out
    }

    /// Parse a segment from the `KWL1` wire format.
    ///
    /// # Errors
    ///
    /// Returns [`ReplError::Malformed`] if the magic is wrong or the
    /// buffer is truncated relative to its declared frame count.
    pub fn decode(bytes: &[u8]) -> ReplResult<Self> {
        // magic(4) + seq(8) + cumulative(8) + page_size(4) + salts(8) + count(4)
        const FIXED: usize = 4 + 8 + 8 + 4 + 4 + 4 + 4;
        if bytes.len() < FIXED {
            return Err(ReplError::Malformed(format!(
                "segment is {} bytes, shorter than the {FIXED}-byte header",
                bytes.len()
            )));
        }
        if &bytes[0..4] != SEGMENT_MAGIC {
            return Err(ReplError::Malformed("segment magic mismatch".to_string()));
        }
        let seq = u64::from_be_bytes(bytes[4..12].try_into().expect("8 bytes"));
        let cumulative_frames = u64::from_be_bytes(bytes[12..20].try_into().expect("8 bytes"));
        let page_size = u32::from_be_bytes(bytes[20..24].try_into().expect("4 bytes"));
        let salt1 = u32::from_be_bytes(bytes[24..28].try_into().expect("4 bytes"));
        let salt2 = u32::from_be_bytes(bytes[28..32].try_into().expect("4 bytes"));
        let count = u32::from_be_bytes(bytes[32..36].try_into().expect("4 bytes")) as usize;
        if page_size < 512 || !page_size.is_power_of_two() {
            return Err(ReplError::Malformed(format!(
                "segment declares implausible page size {page_size}"
            )));
        }

        // `count` is attacker-controlled (the NATS subscriber decodes
        // every inbound message), so never pre-allocate on its word
        // alone — a forged count of u32::MAX would request ~128 GB and
        // abort the process. Clamp the hint to the frames the buffer
        // could actually contain; the per-frame truncation check below
        // still rejects a count that overstates the payload.
        let per_frame = 8 + page_size as usize;
        let max_possible = (bytes.len() - FIXED) / per_frame;
        let mut frames = Vec::with_capacity(count.min(max_possible));
        let mut offset = FIXED;
        for _ in 0..count {
            if offset + per_frame > bytes.len() {
                return Err(ReplError::Malformed(
                    "segment truncated before declared frame count".to_string(),
                ));
            }
            let page_number = u32::from_be_bytes(bytes[offset..offset + 4].try_into().unwrap());
            let db_size = u32::from_be_bytes(bytes[offset + 4..offset + 8].try_into().unwrap());
            let page = bytes[offset + 8..offset + per_frame].to_vec();
            frames.push(WalFrame {
                page_number,
                db_size_after_commit: db_size,
                page_data: page,
            });
            offset += per_frame;
        }
        Ok(Self {
            seq,
            cumulative_frames,
            page_size,
            salt1,
            salt2,
            frames,
        })
    }
}

/// Stateful extractor that turns successive snapshots of a primary's
/// WAL file into [`WalSegment`]s of only the *new*, committed frames.
///
/// The shipper tracks how many frames it has already emitted for the
/// current WAL generation. When the WAL is checkpointed and reset (its
/// salts change) the cursor restarts, so a fresh generation re-ships
/// from frame zero.
#[derive(Debug, Default)]
pub struct WalShipper {
    shipped_frames: usize,
    cumulative_frames: u64,
    next_seq: u64,
    last_salts: Option<(u32, u32)>,
}

impl WalShipper {
    /// A shipper that has not yet emitted any segment.
    #[must_use]
    pub fn new() -> Self {
        Self {
            shipped_frames: 0,
            cumulative_frames: 0,
            next_seq: 1,
            last_salts: None,
        }
    }

    /// Total frames shipped so far across all generations.
    #[must_use]
    pub fn cumulative_frames(&self) -> u64 {
        self.cumulative_frames
    }

    /// Inspect a current snapshot of the WAL file and, if a new
    /// complete transaction is present, return a segment carrying every
    /// new frame up to and including the latest commit frame.
    ///
    /// Returns `Ok(None)` when there is nothing new to ship, or when the
    /// only new frames belong to a transaction that has not committed
    /// yet (the partial tail is withheld until its commit frame lands).
    ///
    /// # Errors
    ///
    /// Propagates [`parse_wal`] errors for a structurally invalid WAL.
    pub fn next_segment(&mut self, wal_bytes: &[u8]) -> ReplResult<Option<WalSegment>> {
        let parsed = parse_wal(wal_bytes)?;
        let salts = (parsed.header.salt1, parsed.header.salt2);
        if self.last_salts != Some(salts) {
            // New WAL generation (first sight or post-checkpoint reset):
            // restart the per-generation frame cursor.
            self.shipped_frames = 0;
            self.last_salts = Some(salts);
        }
        if parsed.frames.len() <= self.shipped_frames {
            return Ok(None);
        }
        let new_frames = &parsed.frames[self.shipped_frames..];
        // Ship only up to the last commit frame; withhold a partial tail.
        let last_commit = new_frames.iter().rposition(WalFrame::is_commit);
        let Some(last_commit) = last_commit else {
            return Ok(None);
        };
        let to_ship: Vec<WalFrame> = new_frames[..=last_commit].to_vec();
        let shipped = to_ship.len();
        self.shipped_frames += shipped;
        self.cumulative_frames += shipped as u64;
        let seq = self.next_seq;
        self.next_seq += 1;
        Ok(Some(WalSegment {
            seq,
            cumulative_frames: self.cumulative_frames,
            page_size: parsed.header.page_size,
            salt1: parsed.header.salt1,
            salt2: parsed.header.salt2,
            frames: to_ship,
        }))
    }
}

// ───────────────────────── Transport traits ─────────────────────────

/// A subscription handle yielding [`WalSegment`]s in publish order.
///
/// Backed by an in-process channel regardless of transport: the NATS
/// implementation spawns a task that decodes JetStream messages into
/// this channel, so the standby loop stays transport-agnostic.
pub struct WalSubscription {
    rx: tokio::sync::mpsc::Receiver<WalSegment>,
}

impl WalSubscription {
    /// Construct from a receiver. Exposed for transport implementations.
    #[must_use]
    pub fn new(rx: tokio::sync::mpsc::Receiver<WalSegment>) -> Self {
        Self { rx }
    }

    /// Await the next segment, or `None` once the stream closes.
    pub async fn next(&mut self) -> Option<WalSegment> {
        self.rx.recv().await
    }
}

/// The replication transport: a durable, ordered log of WAL segments.
#[async_trait]
pub trait WalBus: Send + Sync {
    /// Append a segment to the log. Must preserve publish order.
    async fn publish(&self, segment: &WalSegment) -> ReplResult<()>;

    /// Begin consuming segments. Implementations replay from the
    /// earliest retained segment so a freshly attached standby catches
    /// up before tailing live traffic.
    async fn subscribe(&self) -> ReplResult<WalSubscription>;

    /// The primary's current frame watermark: `cumulative_frames` of the
    /// most recently published segment, or `0` if the log is empty.
    /// Standbys subtract their applied count from this to derive lag
    /// without a separate side channel.
    async fn latest_watermark(&self) -> ReplResult<u64>;
}

/// A point-in-time view of the leadership lease.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lease {
    /// Node id currently holding the lease.
    pub holder: String,
    /// Fencing token: incremented every time leadership changes hands,
    /// so a promoted standby's writes can be ordered against a
    /// recovered-but-stale ex-primary.
    pub epoch: u64,
    /// Unix-millis instant after which the lease is considered expired
    /// if not renewed.
    pub expires_at_ms: i64,
}

/// A distributed lease used for single-writer leader election.
///
/// Implementations provide compare-and-set semantics so that, under
/// contention, exactly one node holds the lease at a time. The failover
/// coordinator renews it on a timer; a standby that finds the lease
/// expired steals it (bumping [`Lease::epoch`]) and promotes.
#[async_trait]
pub trait LeaseStore: Send + Sync {
    /// Attempt to acquire (or renew, if already held by `node_id`) the
    /// lease for `ttl`. Returns the resulting lease view — leadership is
    /// indicated by `lease.holder == node_id`.
    async fn acquire(&self, node_id: &str, ttl: Duration) -> ReplResult<Lease>;

    /// Release the lease iff `node_id` currently holds it. A no-op
    /// otherwise.
    async fn release(&self, node_id: &str) -> ReplResult<()>;

    /// Read the current (non-expired) lease, if any.
    async fn current(&self) -> ReplResult<Option<Lease>>;
}

// ────────────────────────── Roles & status ──────────────────────────

/// The operational role a substrate node is currently serving.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// Accepts writes and ships WAL frames.
    Primary,
    /// Read-only; replays shipped frames.
    Standby,
    /// Replication is off; the node is a standalone primary.
    Disabled,
}

/// How replication should behave, as selected by config / the `--role`
/// flag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplicationMode {
    /// No replication (default when no NATS URL is configured).
    Disabled,
    /// Statically the primary.
    Primary,
    /// Statically a standby.
    Standby,
    /// Compete for the lease; winner is primary, losers are standbys.
    Auto,
}

impl ReplicationMode {
    /// Parse a role string (`primary`, `standby`, `auto`, `disabled`),
    /// case-insensitively.
    ///
    /// # Errors
    ///
    /// Returns [`ReplError::BadRole`] for any other value.
    pub fn parse(s: &str) -> ReplResult<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "primary" => Ok(Self::Primary),
            "standby" => Ok(Self::Standby),
            "auto" => Ok(Self::Auto),
            "disabled" | "off" | "none" => Ok(Self::Disabled),
            other => Err(ReplError::BadRole(other.to_string())),
        }
    }
}

/// Serialisable replication status, embedded in the `/health` payload
/// under the `replication` key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplicationStatus {
    /// Whether replication is configured at all.
    pub enabled: bool,
    /// Current role.
    pub role: Role,
    /// Frames the standby is behind the primary's watermark (always `0`
    /// on the primary).
    pub lag_frames: u64,
    /// Total frames the primary has shipped (primary only; `0` on a
    /// standby).
    pub published_frames_total: u64,
    /// Total frames a standby has applied (standby only; `0` on the
    /// primary).
    pub applied_frames_total: u64,
    /// Leadership epoch / fencing token.
    pub epoch: u64,
    /// Wall-clock time the standby last applied a segment, if ever.
    pub last_applied_at: Option<DateTime<Utc>>,
}

/// Process-wide, lock-light replication state shared between the
/// background loops and the HTTP handlers (`/health`, `/internal/metrics`).
#[derive(Debug)]
pub struct ReplicationShared {
    enabled: bool,
    role: Mutex<Role>,
    lag_frames: AtomicU64,
    published_frames_total: AtomicU64,
    applied_frames_total: AtomicU64,
    epoch: AtomicU64,
    /// Unix-millis of the last applied segment, or `-1` for "never".
    last_applied_ms: AtomicI64,
}

impl ReplicationShared {
    /// Shared state for a node with replication **disabled** — reports
    /// role [`Role::Disabled`].
    #[must_use]
    pub fn disabled() -> Self {
        Self {
            enabled: false,
            role: Mutex::new(Role::Disabled),
            lag_frames: AtomicU64::new(0),
            published_frames_total: AtomicU64::new(0),
            applied_frames_total: AtomicU64::new(0),
            epoch: AtomicU64::new(0),
            last_applied_ms: AtomicI64::new(-1),
        }
    }

    /// Shared state for a replication-enabled node, starting in `role`.
    #[must_use]
    pub fn enabled(role: Role) -> Self {
        Self {
            enabled: true,
            role: Mutex::new(role),
            lag_frames: AtomicU64::new(0),
            published_frames_total: AtomicU64::new(0),
            applied_frames_total: AtomicU64::new(0),
            epoch: AtomicU64::new(0),
            last_applied_ms: AtomicI64::new(-1),
        }
    }

    /// Whether replication is configured.
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Current role.
    #[must_use]
    pub fn role(&self) -> Role {
        *self.role.lock().expect("replication role mutex poisoned")
    }

    /// Whether this node is currently the primary (and therefore
    /// accepts writes). A disabled node is always a (standalone) writer.
    #[must_use]
    pub fn is_writable(&self) -> bool {
        matches!(self.role(), Role::Primary | Role::Disabled)
    }

    /// Transition to a new role (used by the failover coordinator).
    pub fn set_role(&self, role: Role) {
        *self.role.lock().expect("replication role mutex poisoned") = role;
    }

    /// Set the current replication lag in frames.
    pub fn set_lag_frames(&self, lag: u64) {
        self.lag_frames.store(lag, Ordering::Relaxed);
    }

    /// Current replication lag in frames.
    #[must_use]
    pub fn lag_frames(&self) -> u64 {
        self.lag_frames.load(Ordering::Relaxed)
    }

    /// Record the primary's total shipped-frame count.
    pub fn set_published_frames_total(&self, total: u64) {
        self.published_frames_total.store(total, Ordering::Relaxed);
    }

    /// Record that a standby applied a segment: bumps the applied total
    /// and stamps `last_applied_at` to now.
    pub fn record_applied(&self, applied_total: u64) {
        self.applied_frames_total
            .store(applied_total, Ordering::Relaxed);
        self.last_applied_ms
            .store(Utc::now().timestamp_millis(), Ordering::Relaxed);
    }

    /// Set the leadership epoch / fencing token.
    pub fn set_epoch(&self, epoch: u64) {
        self.epoch.store(epoch, Ordering::Relaxed);
    }

    /// Current leadership epoch.
    #[must_use]
    pub fn epoch(&self) -> u64 {
        self.epoch.load(Ordering::Relaxed)
    }

    /// Snapshot the state for serialisation into `/health`.
    #[must_use]
    pub fn snapshot(&self) -> ReplicationStatus {
        let last_ms = self.last_applied_ms.load(Ordering::Relaxed);
        let last_applied_at = if last_ms < 0 {
            None
        } else {
            Utc.timestamp_millis_opt(last_ms).single()
        };
        ReplicationStatus {
            enabled: self.enabled,
            role: self.role(),
            lag_frames: self.lag_frames.load(Ordering::Relaxed),
            published_frames_total: self.published_frames_total.load(Ordering::Relaxed),
            applied_frames_total: self.applied_frames_total.load(Ordering::Relaxed),
            epoch: self.epoch.load(Ordering::Relaxed),
            last_applied_at,
        }
    }
}

// ──────────────────────────── Config ────────────────────────────────

/// Environment variable selecting the replication role.
pub const ENV_ROLE: &str = "KNOWLEDGE_SUBSTRATE_ROLE";
/// Environment variable carrying the NATS URL for the WAL transport.
pub const ENV_NATS_URL: &str = "KNOWLEDGE_REPLICATION_NATS_URL";
/// Optional override for the JetStream stream name.
pub const ENV_STREAM: &str = "KNOWLEDGE_REPLICATION_STREAM";
/// Optional override for the WAL subject.
pub const ENV_SUBJECT: &str = "KNOWLEDGE_REPLICATION_SUBJECT";
/// Optional override for the leader-election KV bucket.
pub const ENV_KV_BUCKET: &str = "KNOWLEDGE_REPLICATION_KV_BUCKET";
/// Optional stable node id (defaults to the hostname / a random id).
pub const ENV_NODE_ID: &str = "KNOWLEDGE_REPLICATION_NODE_ID";

/// Default JetStream stream name for shipped WAL segments.
pub const DEFAULT_STREAM: &str = "substrate-wal";
/// Default subject WAL segments are published on.
pub const DEFAULT_SUBJECT: &str = "substrate.wal.frames";
/// Default NATS KV bucket backing the leadership lease.
pub const DEFAULT_KV_BUCKET: &str = "substrate-leader";

/// Fully-resolved replication configuration.
#[derive(Debug, Clone)]
pub struct ReplicationConfig {
    /// Selected mode / role.
    pub mode: ReplicationMode,
    /// NATS server URL, when a real transport is configured.
    pub nats_url: Option<String>,
    /// Path of the primary's SQLCipher store (its `-wal` sidecar is the
    /// replication source); on a standby, the shadow copy's path.
    pub store_path: String,
    /// JetStream stream name.
    pub stream: String,
    /// WAL subject.
    pub subject: String,
    /// Leader-election KV bucket.
    pub kv_bucket: String,
    /// This node's id (lease holder identity).
    pub node_id: String,
    /// Lease TTL; renewed at roughly a third of this interval.
    pub lease_ttl: Duration,
    /// How often the primary polls its WAL for new frames.
    pub poll_interval: Duration,
}

impl ReplicationConfig {
    /// Resolve replication config from the environment, layered under an
    /// optional `--role` CLI override.
    ///
    /// Precedence for the mode: explicit `role_override` → [`ENV_ROLE`]
    /// → inferred. When no role is given but a NATS URL is present the
    /// mode defaults to [`ReplicationMode::Auto`]; with neither it is
    /// [`ReplicationMode::Disabled`].
    ///
    /// # Errors
    ///
    /// Returns [`ReplError::BadRole`] if the role string is invalid.
    pub fn from_env(store_path: &str, role_override: Option<&str>) -> ReplResult<Self> {
        let nats_url = non_empty(ENV_NATS_URL);
        let role_str = role_override
            .map(str::to_string)
            .or_else(|| non_empty(ENV_ROLE));
        let mode = match role_str {
            Some(s) => ReplicationMode::parse(&s)?,
            None => {
                if nats_url.is_some() {
                    ReplicationMode::Auto
                } else {
                    ReplicationMode::Disabled
                }
            }
        };
        let node_id = non_empty(ENV_NODE_ID).unwrap_or_else(default_node_id);
        Ok(Self {
            mode,
            nats_url,
            store_path: store_path.to_string(),
            stream: non_empty(ENV_STREAM).unwrap_or_else(|| DEFAULT_STREAM.to_string()),
            subject: non_empty(ENV_SUBJECT).unwrap_or_else(|| DEFAULT_SUBJECT.to_string()),
            kv_bucket: non_empty(ENV_KV_BUCKET).unwrap_or_else(|| DEFAULT_KV_BUCKET.to_string()),
            node_id,
            lease_ttl: Duration::from_secs(15),
            poll_interval: Duration::from_millis(500),
        })
    }

    /// The starting role implied by [`ReplicationConfig::mode`] before
    /// any leader election runs: `Auto` standbys start read-only and are
    /// promoted only on winning the lease.
    #[must_use]
    pub fn initial_role(&self) -> Role {
        match self.mode {
            ReplicationMode::Disabled => Role::Disabled,
            ReplicationMode::Primary => Role::Primary,
            ReplicationMode::Standby | ReplicationMode::Auto => Role::Standby,
        }
    }

    /// Path to the WAL sidecar file (`<store>-wal`) that SQLite writes.
    #[must_use]
    pub fn wal_path(&self) -> String {
        format!("{}-wal", self.store_path)
    }
}

/// Compute a lease expiry instant (`now_ms + ttl`) in Unix millis,
/// saturating instead of overflowing/truncating for absurd TTLs. Shared
/// by both [`LeaseStore`] implementations so their fencing semantics
/// stay identical.
#[must_use]
pub(crate) fn lease_expiry_ms(now_ms: i64, ttl: Duration) -> i64 {
    let ttl_ms = i64::try_from(ttl.as_millis()).unwrap_or(i64::MAX);
    now_ms.saturating_add(ttl_ms)
}

/// Read an environment variable, returning `None` when unset or empty.
fn non_empty(key: &str) -> Option<String> {
    match std::env::var(key) {
        Ok(v) if !v.trim().is_empty() => Some(v),
        _ => None,
    }
}

/// Best-effort stable node id: `$HOSTNAME` if present, else a random
/// hex id (good enough to disambiguate lease holders).
fn default_node_id() -> String {
    if let Some(h) = non_empty("HOSTNAME") {
        return h;
    }
    let n = uuid::Uuid::new_v4();
    format!("node-{}", n.simple())
}

/// Start the replication subsystem for this node, returning the
/// coordinator's join handle (or `None` when replication is disabled).
///
/// Transport selection:
/// * If the `replication-nats` feature is built **and** a NATS URL is
///   configured, the production JetStream + KV transports are used.
/// * Otherwise an in-process transport is used. It only links a primary
///   and standby living in the *same* process, so it is meant for
///   dev/tests. A static [`ReplicationMode::Primary`] /
///   [`ReplicationMode::Standby`] role is a multi-node assignment that
///   cannot work over the in-process bus, so it is rejected outright
///   (see Errors); [`ReplicationMode::Auto`] is allowed (a single node
///   elects itself primary) but logs a warning.
///
/// The caller drives shutdown by flipping `shutdown` to `true` and
/// awaiting the returned handle.
///
/// # Errors
///
/// * Propagates transport connection errors (e.g. NATS unreachable).
/// * [`ReplError::Misconfigured`] when a static `primary`/`standby` role
///   is requested but no real cross-node transport is active — failing
///   fast beats running a substrate that silently does not replicate.
pub fn spawn(
    config: ReplicationConfig,
    shared: Arc<ReplicationShared>,
    shutdown: watch::Receiver<bool>,
    db_handle: Option<ffi::RuntimeHandle>,
) -> ReplResult<Option<JoinHandle<()>>> {
    if matches!(config.mode, ReplicationMode::Disabled) {
        tracing::info!("replication: disabled; running as a standalone substrate");
        return Ok(None);
    }

    // A node that can serve reads as a standby (`Standby`, or `Auto`
    // before it wins the lease) splices raw WAL pages into the database
    // file underneath the open SQLCipher connection, then re-opens that
    // connection so the next read faults the spliced pages back in (see
    // `standby` module docs — the WAL-mode primary freezes the page-1
    // change counter, so SQLite's own cache invalidation cannot be
    // relied on). The re-open only surfaces the spliced *main-file*
    // pages while the standby's own connection is in a rollback-journal
    // mode: in WAL mode SQLite would read from its own `-wal` sidecar —
    // which replication never writes — and serve stale pages until a
    // checkpoint.
    //
    // The store opens in rollback-journal mode (SQLite's default — the
    // evidence store sets only SQLCipher pragmas), which is exactly what
    // a standby needs at startup. A node only ever enters `journal_mode=
    // WAL` *after* this point, when the failover coordinator promotes it
    // to primary (`set_store_journal_for(Role::Primary)`), and it is
    // switched back to rollback on demotion before any standby task runs
    // again. Assert the startup invariant here so a future change that
    // made the (shared, out-of-module) open path default to WAL fails
    // fast instead of silently corrupting standby reads before the first
    // election.
    if matches!(
        config.mode,
        ReplicationMode::Standby | ReplicationMode::Auto
    ) {
        if let Some(handle) = db_handle {
            let mode = ffi::store_journal_mode(handle)
                .map_err(|e| ReplError::Transport(format!("reading store journal mode: {e}")))?;
            if mode == "wal" {
                return Err(ReplError::Misconfigured(format!(
                    "evidence store opened in `journal_mode=wal`, but standby WAL replay requires \
                     a rollback-journal mode ({:?}): raw page applies would be invisible to the \
                     read connection until a checkpoint, serving stale reads",
                    config.mode
                )));
            }
            tracing::debug!(journal_mode = %mode, mode = ?config.mode, "replication: verified rollback-journal store mode for standby reads");

            // The standby splices shipped page images into the database
            // file at byte offset `(page_number - 1) * page_size`, where
            // `page_size` comes from the WAL segment header the primary
            // stamps from *its* page size. If the standby's own store were
            // opened with a different `cipher_page_size`, every spliced
            // page would land at the wrong offset and silently corrupt the
            // file. Both sides default to the SQLCipher 4.x 4096-byte page
            // (set explicitly in `evidence_store`), so assert it here so a
            // future change to that shared open path fails fast instead of
            // misaligning replicated pages.
            let page_size = ffi::store_cipher_page_size(handle).map_err(|e| {
                ReplError::Transport(format!("reading store cipher_page_size: {e}"))
            })?;
            if page_size != EXPECTED_CIPHER_PAGE_SIZE {
                return Err(ReplError::Misconfigured(format!(
                    "evidence store opened with cipher_page_size={page_size}, but WAL replay \
                     assumes {EXPECTED_CIPHER_PAGE_SIZE}-byte pages: spliced page images would \
                     land at the wrong file offset and corrupt the standby database"
                )));
            }
            tracing::debug!(
                cipher_page_size = page_size,
                "replication: verified cipher_page_size for standby page splicing"
            );
        }
    }

    #[cfg(feature = "replication-nats")]
    {
        if config.nats_url.is_some() {
            tracing::info!(
                mode = ?config.mode,
                node = %config.node_id,
                "replication: starting NATS JetStream transport"
            );
            let (bus, lease) = nats::connect(&config).await?;
            let coordinator = failover::FailoverCoordinator::new(
                Arc::new(bus),
                Arc::new(lease),
                shared,
                config,
                db_handle,
            );
            return Ok(Some(tokio::spawn(coordinator.run(shutdown))));
        }
    }

    // We only reach here when no real (NATS) transport was used. A
    // static primary/standby is a multi-node role: the in-process bus
    // links only loops in *this* process, so a pinned primary would ship
    // frames into a void and a pinned standby would never receive any.
    // Fail fast rather than run a substrate that looks healthy while
    // silently not replicating.
    if matches!(
        config.mode,
        ReplicationMode::Primary | ReplicationMode::Standby
    ) {
        return Err(ReplError::Misconfigured(format!(
            "role {:?} needs a cross-node transport, but none is active: build with the \
             `replication-nats` feature and set {ENV_NATS_URL}. Refusing to start on the \
             in-process bus, which cannot replicate across nodes",
            config.mode
        )));
    }

    // Auto with no NATS transport is a legitimate single-process dev/test
    // setup: the node elects itself primary over the in-process lease.
    // Warn so a misconfigured multi-node Auto deployment is at least
    // visible in the logs.
    tracing::warn!(
        mode = ?config.mode,
        nats_env = ENV_NATS_URL,
        "replication: auto mode without a NATS transport; using the in-process \
         bus/lease (single-process only — set the NATS URL env var and build \
         with the `replication-nats` feature for cross-node failover)"
    );
    let bus = Arc::new(memory::InMemoryWalBus::new());
    let lease = Arc::new(memory::InMemoryLeaseStore::new());
    let coordinator = failover::FailoverCoordinator::new(bus, lease, shared, config, db_handle);
    Ok(Some(tokio::spawn(coordinator.run(shutdown))))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal valid WAL with `page_size`-byte pages from a list
    /// of `(page_number, db_size_after_commit, fill_byte)` frame specs.
    fn build_wal(
        order: ChecksumOrder,
        page_size: u32,
        salt1: u32,
        salt2: u32,
        specs: &[(u32, u32, u8)],
    ) -> Vec<u8> {
        let frames: Vec<WalFrame> = specs
            .iter()
            .map(|&(pn, db, fill)| WalFrame {
                page_number: pn,
                db_size_after_commit: db,
                page_data: vec![fill; page_size as usize],
            })
            .collect();
        let header = WalHeader {
            order,
            page_size,
            checkpoint_seq: 0,
            salt1,
            salt2,
            checksum: (0, 0),
        };
        encode_wal(&header, &frames).expect("encode")
    }

    #[test]
    fn header_checksum_round_trips_both_orders() {
        for order in [ChecksumOrder::Little, ChecksumOrder::Big] {
            let wal = build_wal(order, 4096, 0x1111_2222, 0x3333_4444, &[(1, 1, 0xAB)]);
            let parsed = parse_wal(&wal).expect("parse");
            assert_eq!(parsed.header.page_size, 4096);
            assert_eq!(parsed.header.order, order);
            assert_eq!(parsed.frames.len(), 1);
            assert_eq!(parsed.frames[0].page_number, 1);
            assert!(parsed.frames[0].is_commit());
            assert_eq!(parsed.frames[0].page_data, vec![0xAB; 4096]);
        }
    }

    #[test]
    fn parse_stops_at_corrupt_frame() {
        let mut wal = build_wal(
            ChecksumOrder::Little,
            512,
            7,
            9,
            &[(1, 0, 0x01), (2, 2, 0x02)],
        );
        // Corrupt the second frame's page data; the first must survive.
        let first_frame_end = WAL_HEADER_SIZE + FRAME_HEADER_SIZE + 512;
        wal[first_frame_end + FRAME_HEADER_SIZE + 10] ^= 0xFF;
        let parsed = parse_wal(&wal).expect("parse");
        assert_eq!(parsed.frames.len(), 1);
        assert_eq!(parsed.frames[0].page_number, 1);
    }

    #[test]
    fn parse_rejects_bad_magic() {
        let mut wal = build_wal(ChecksumOrder::Big, 512, 1, 2, &[(1, 1, 0)]);
        wal[3] = 0xFF;
        assert!(matches!(parse_wal(&wal), Err(ReplError::BadMagic(_))));
    }

    #[test]
    fn segment_wire_round_trip() {
        let seg = WalSegment {
            seq: 42,
            cumulative_frames: 100,
            page_size: 4096,
            salt1: 0xDEAD_BEEF,
            salt2: 0x0BAD_F00D,
            frames: vec![
                WalFrame {
                    page_number: 1,
                    db_size_after_commit: 0,
                    page_data: vec![0x11; 4096],
                },
                WalFrame {
                    page_number: 2,
                    db_size_after_commit: 2,
                    page_data: vec![0x22; 4096],
                },
            ],
        };
        let bytes = seg.encode();
        let back = WalSegment::decode(&bytes).expect("decode");
        assert_eq!(seg, back);
    }

    #[test]
    fn segment_decode_detects_truncation() {
        let seg = WalSegment {
            seq: 1,
            cumulative_frames: 1,
            page_size: 512,
            salt1: 1,
            salt2: 2,
            frames: vec![WalFrame {
                page_number: 1,
                db_size_after_commit: 1,
                page_data: vec![0x33; 512],
            }],
        };
        let mut bytes = seg.encode();
        bytes.truncate(bytes.len() - 100);
        assert!(matches!(
            WalSegment::decode(&bytes),
            Err(ReplError::Malformed(_))
        ));
    }

    #[test]
    fn segment_decode_rejects_forged_count_without_oom() {
        // A hostile segment whose header claims u32::MAX frames but
        // carries none must fail cleanly as Malformed — it must never
        // pre-allocate gigabytes from the untrusted count and abort.
        let seg = WalSegment {
            seq: 1,
            cumulative_frames: 1,
            page_size: 512,
            salt1: 1,
            salt2: 2,
            frames: vec![WalFrame {
                page_number: 1,
                db_size_after_commit: 1,
                page_data: vec![0x33; 512],
            }],
        };
        let mut bytes = seg.encode();
        // Overwrite the count word (offset 32..36) with u32::MAX.
        bytes[32..36].copy_from_slice(&u32::MAX.to_be_bytes());
        assert!(matches!(
            WalSegment::decode(&bytes),
            Err(ReplError::Malformed(_))
        ));
    }

    #[test]
    fn shipper_emits_only_new_committed_frames() {
        let mut shipper = WalShipper::new();
        // First snapshot: one committed txn (frames 1,2 with commit on 2).
        let wal1 = build_wal(
            ChecksumOrder::Little,
            512,
            10,
            20,
            &[(1, 0, 0xA1), (2, 2, 0xA2)],
        );
        let seg1 = shipper.next_segment(&wal1).expect("ship").expect("segment");
        assert_eq!(seg1.seq, 1);
        assert_eq!(seg1.frame_count(), 2);
        assert_eq!(seg1.cumulative_frames, 2);

        // No change → nothing to ship.
        assert!(shipper.next_segment(&wal1).expect("ship").is_none());

        // Second snapshot appends another committed txn (frame 3).
        let wal2 = build_wal(
            ChecksumOrder::Little,
            512,
            10,
            20,
            &[(1, 0, 0xA1), (2, 2, 0xA2), (3, 3, 0xA3)],
        );
        let seg2 = shipper.next_segment(&wal2).expect("ship").expect("segment");
        assert_eq!(seg2.seq, 2);
        assert_eq!(seg2.frame_count(), 1);
        assert_eq!(seg2.frames[0].page_number, 3);
        assert_eq!(seg2.cumulative_frames, 3);
    }

    #[test]
    fn shipper_withholds_uncommitted_tail() {
        let mut shipper = WalShipper::new();
        // A single non-commit frame: no complete transaction yet.
        let wal = build_wal(ChecksumOrder::Little, 512, 1, 2, &[(1, 0, 0x01)]);
        assert!(shipper.next_segment(&wal).expect("ship").is_none());
    }

    #[test]
    fn shipper_resets_on_wal_generation_change() {
        let mut shipper = WalShipper::new();
        let wal1 = build_wal(ChecksumOrder::Little, 512, 10, 20, &[(1, 1, 0xA1)]);
        let seg1 = shipper.next_segment(&wal1).expect("ship").expect("seg");
        assert_eq!(seg1.frame_count(), 1);
        // New generation (salts changed after a checkpoint): re-ship from 0.
        let wal2 = build_wal(ChecksumOrder::Little, 512, 11, 21, &[(1, 1, 0xB1)]);
        let seg2 = shipper.next_segment(&wal2).expect("ship").expect("seg");
        assert_eq!(seg2.seq, 2);
        assert_eq!(seg2.frame_count(), 1);
        assert_eq!(seg2.cumulative_frames, 2);
    }

    #[test]
    fn mode_parsing() {
        assert_eq!(
            ReplicationMode::parse("primary").unwrap(),
            ReplicationMode::Primary
        );
        assert_eq!(
            ReplicationMode::parse(" Standby ").unwrap(),
            ReplicationMode::Standby
        );
        assert_eq!(
            ReplicationMode::parse("AUTO").unwrap(),
            ReplicationMode::Auto
        );
        assert_eq!(
            ReplicationMode::parse("off").unwrap(),
            ReplicationMode::Disabled
        );
        assert!(matches!(
            ReplicationMode::parse("leader"),
            Err(ReplError::BadRole(_))
        ));
    }

    #[test]
    fn shared_status_snapshot() {
        let shared = ReplicationShared::enabled(Role::Standby);
        assert!(shared.is_enabled());
        assert!(!shared.is_writable());
        shared.set_lag_frames(5);
        shared.record_applied(42);
        let snap = shared.snapshot();
        assert_eq!(snap.role, Role::Standby);
        assert_eq!(snap.lag_frames, 5);
        assert_eq!(snap.applied_frames_total, 42);
        assert!(snap.last_applied_at.is_some());

        shared.set_role(Role::Primary);
        assert!(shared.is_writable());
    }

    #[test]
    fn disabled_shared_is_writable() {
        let shared = ReplicationShared::disabled();
        assert!(!shared.is_enabled());
        assert!(shared.is_writable());
        let snap = shared.snapshot();
        assert_eq!(snap.role, Role::Disabled);
        assert!(snap.last_applied_at.is_none());
    }

    // Build a config for `role` with the in-process transport forced
    // (no NATS URL), independent of the ambient environment so the test
    // is deterministic under both feature builds.
    fn in_process_config(role: &str) -> ReplicationConfig {
        let mut config = ReplicationConfig::from_env("/tmp/spawn-transport-test.db", Some(role))
            .expect("config");
        config.nats_url = None;
        config
    }

    #[tokio::test]
    async fn spawn_rejects_static_role_without_transport() {
        for role in ["primary", "standby"] {
            let config = in_process_config(role);
            let shared = Arc::new(ReplicationShared::enabled(config.initial_role()));
            let (_tx, rx) = watch::channel(false);
            let err = spawn(config, shared, rx, None)
                .expect_err("static role must reject the in-process bus");
            assert!(
                matches!(err, ReplError::Misconfigured(_)),
                "expected Misconfigured for role {role}, got {err:?}"
            );
        }
    }

    #[tokio::test]
    async fn spawn_allows_auto_on_in_process_bus() {
        let config = in_process_config("auto");
        let shared = Arc::new(ReplicationShared::enabled(config.initial_role()));
        let (tx, rx) = watch::channel(false);
        let handle = spawn(config, shared, rx, None)
            .expect("auto may use the in-process bus")
            .expect("auto spawns a coordinator task");
        // Cleanly stop the spawned coordinator.
        tx.send(true).expect("signal shutdown");
        handle.await.expect("coordinator joins");
    }

    // With a real open store handle, a standby-capable node verifies the
    // read connection's journal mode at startup. A freshly opened store
    // is in a rollback-journal mode, so spawn must succeed; the coverage
    // here is the journal-mode probe path (the rejection branch fires
    // only if `evidence_store` ever opens in WAL mode).
    #[tokio::test]
    async fn spawn_auto_accepts_rollback_journal_store() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("evidence.db");
        let path_str = path.to_string_lossy().into_owned();
        // `open_store` builds and drops a short-lived Tokio runtime while
        // rehydrating the store, which trips tokio's "cannot drop a
        // runtime within an async context" guard if called on this test's
        // worker thread (the same reason `lib.rs` opens on a dedicated
        // thread). Open/close off the async runtime.
        let handle = std::thread::spawn(move || ffi::open_store(path_str, "a5".repeat(32)))
            .join()
            .expect("open thread")
            .expect("open_store");

        let config = in_process_config("auto");
        let shared = Arc::new(ReplicationShared::enabled(config.initial_role()));
        let (tx, rx) = watch::channel(false);
        let join = spawn(config, shared, rx, Some(handle))
            .expect("rollback-journal store passes the standby journal-mode check")
            .expect("auto spawns a coordinator task");
        tx.send(true).expect("signal shutdown");
        join.await.expect("coordinator joins");
        std::thread::spawn(move || ffi::close_store(handle))
            .join()
            .expect("close thread")
            .expect("close_store");
    }
}
