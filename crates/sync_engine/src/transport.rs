//! Client-side sync transport: push / pull encrypted deltas through
//! an **untrusted** relay.
//!
//! [`crate::delta`] defines *what* travels on the wire (a
//! [`DeltaEnvelope`] of post-watermark [`SyncOp`]s). This module
//! defines *how* a replica ships those envelopes to its peers
//! through a transport that is **never trusted with plaintext**:
//!
//! 1. A [`SyncClient`] derives, from the substrate master key and a
//!    [`SyncScopeId`], an opaque [`TopicId`] (the relay's routing
//!    key) and a per-scope AEAD seal key.
//! 2. On [`SyncClient::push`] it encodes its **own** new ops
//!    (via [`crate::delta::encode_own_delta_since`]),
//!    XChaCha20-Poly1305-seals the envelope under the seal key
//!    (binding the [`TopicId`] into the AAD), and hands the relay an
//!    opaque [`SealedDelta`] — nonce + ciphertext, nothing else.
//! 3. On [`SyncClient::pull`] it fetches every [`SealedDelta`] the
//!    relay has accumulated past its cursor, opens each one, and
//!    folds the recovered envelope into its [`SyncEngine`] via the
//!    existing [`crate::delta::apply_delta`] path (which dedups by
//!    `(replica_id, seq)` and is therefore idempotent).
//!
//! The relay only ever sees the [`TopicId`] and the ciphertext: it
//! cannot decrypt, cannot link a topic back to a scope without the
//! master key, and cannot resolve or reorder anything. Convergence
//! is a property of the CRDT merge, not of the transport — so the
//! relay is a dumb, replaceable store-and-forward buffer, exactly
//! as `docs/technical/sync-protocol.md` describes.
//!
//! The [`SyncTransport`] trait abstracts the buffer. This crate
//! ships [`InMemoryTransport`] (a process-local implementation used
//! by tests and single-process multi-replica scenarios); the
//! `sync_relay` crate ships the authenticated HTTP relay server and
//! a matching HTTP [`SyncTransport`] client.

use std::collections::HashMap;
use std::hash::Hash;
use std::sync::Mutex;

use rand::TryRng;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use zeroize::Zeroize;

use crypto::{decrypt_aead, derive_key, encrypt_aead, AeadKey, MasterKey, AEAD_NONCE_LEN};

use crate::delta::{apply_delta, encode_own_delta_since};
use crate::error::{Result, SyncError};
use crate::{SyncEngine, SyncScopeId};

/// Length of a [`TopicId`] in bytes (256 bits — a full HKDF block).
pub const TOPIC_ID_LEN: usize = 32;

/// Opaque routing key a relay uses to bucket [`SealedDelta`]s.
///
/// Derived from the substrate master key and a [`SyncScopeId`] via
/// HKDF, so every device that shares the user's master key derives
/// the **same** topic for a scope while the relay — which never
/// holds the master key — cannot link a topic back to the scope it
/// represents. The topic is therefore both a routing key and a
/// read capability: holding it lets a party fetch a scope's
/// ciphertext, but not decrypt it.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TopicId([u8; TOPIC_ID_LEN]);

impl TopicId {
    /// Derive the topic for `scope` under `master_key`.
    ///
    /// Deterministic: the same `(master_key, scope)` always yields
    /// the same topic, which is what lets a user's devices rendezvous
    /// on the relay without coordinating out of band.
    pub fn derive(master_key: &MasterKey, scope: SyncScopeId) -> Result<Self> {
        let context = format!("sync:relay:topic:{}:v1", scope.as_uuid());
        let bytes = derive_key(master_key, context.as_bytes())?;
        Ok(Self(bytes))
    }

    /// Borrow the raw 32 topic bytes (used as AEAD associated data
    /// so a ciphertext cannot be replayed under a different topic).
    pub fn as_bytes(&self) -> &[u8; TOPIC_ID_LEN] {
        &self.0
    }

    /// Lowercase-hex rendering, suitable as a URL path segment.
    pub fn to_hex(&self) -> String {
        let mut out = String::with_capacity(TOPIC_ID_LEN * 2);
        for b in &self.0 {
            out.push(char::from(HEX[usize::from(b >> 4)]));
            out.push(char::from(HEX[usize::from(b & 0x0f)]));
        }
        out
    }

    /// Parse a [`TopicId`] from the lowercase-hex form produced by
    /// [`Self::to_hex`].
    pub fn from_hex(s: &str) -> Result<Self> {
        if s.len() != TOPIC_ID_LEN * 2 {
            return Err(SyncError::Persistence("topic id hex has wrong length"));
        }
        let mut out = [0u8; TOPIC_ID_LEN];
        let bytes = s.as_bytes();
        for (i, chunk) in bytes.chunks_exact(2).enumerate() {
            let hi = hex_val(chunk[0])?;
            let lo = hex_val(chunk[1])?;
            out[i] = (hi << 4) | lo;
        }
        Ok(Self(out))
    }
}

impl std::fmt::Debug for TopicId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // A topic is a read capability — print only a short prefix so
        // it does not land verbatim in logs / panic messages.
        let hex = self.to_hex();
        write!(f, "TopicId({}…)", &hex[..8.min(hex.len())])
    }
}

const HEX: &[u8; 16] = b"0123456789abcdef";

fn hex_val(c: u8) -> Result<u8> {
    match c {
        b'0'..=b'9' => Ok(c - b'0'),
        b'a'..=b'f' => Ok(c - b'a' + 10),
        b'A'..=b'F' => Ok(c - b'A' + 10),
        _ => Err(SyncError::Persistence("topic id hex has invalid digit")),
    }
}

/// An opaque, AEAD-sealed delta as stored and forwarded by a relay.
///
/// The relay treats this as bytes: it can neither decrypt
/// `ciphertext` nor interpret `nonce`. No replica id, scope id, op
/// count, or sequence range is exposed — the only relay-visible
/// metadata is the [`TopicId`] the blob is filed under and the
/// arrival offset the relay itself assigns.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SealedDelta {
    /// XChaCha20-Poly1305 nonce (192-bit, random per seal).
    pub nonce: [u8; AEAD_NONCE_LEN],
    /// AEAD ciphertext of the serialised [`DeltaEnvelope`], with the
    /// 16-byte Poly1305 tag appended.
    ///
    /// [`DeltaEnvelope`]: crate::delta::DeltaEnvelope
    pub ciphertext: Vec<u8>,
}

/// A page of [`SealedDelta`]s returned by [`SyncTransport::pull`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullPage {
    /// Cursor the caller should pass as `since` on its next pull to
    /// fetch only blobs that arrive after this page. Always
    /// `>= since` of the request.
    pub next_cursor: u64,
    /// Blobs with relay offset in `(since, next_cursor]`, in arrival
    /// order.
    pub blobs: Vec<SealedDelta>,
}

/// A store-and-forward buffer for [`SealedDelta`]s, keyed by
/// [`TopicId`].
///
/// Implementations are **untrusted**: they observe only opaque
/// ciphertext and the topic routing key, never plaintext or the
/// scope identity. The contract is deliberately tiny — append blobs,
/// and read blobs past a monotonic cursor — so a relay can be a
/// dumb HTTP buffer, a shared folder, or an in-process queue.
///
/// Required semantics:
///
/// * `push` appends `blobs` to the topic in order and returns the
///   topic's new high-water cursor (the offset after the last
///   appended blob).
/// * `pull` returns every blob whose offset is strictly greater
///   than `since`, plus the new high-water cursor. Offsets are
///   contiguous and monotonic per topic, so `next_cursor ==
///   since + blobs.len()`.
/// * Both operations are idempotent at the CRDT layer: re-pulling
///   already-applied blobs is harmless because
///   [`crate::delta::apply_delta`] dedups by `(replica_id, seq)`.
pub trait SyncTransport {
    /// Transport-specific error (network failure, auth rejection,
    /// quota exhaustion, …).
    type Error: std::error::Error + Send + Sync + 'static;

    /// Append `blobs` to `topic`; returns the topic's new high-water
    /// cursor.
    fn push(&self, topic: &TopicId, blobs: &[SealedDelta])
        -> std::result::Result<u64, Self::Error>;

    /// Fetch every blob for `topic` with offset `> since`.
    fn pull(&self, topic: &TopicId, since: u64) -> std::result::Result<PullPage, Self::Error>;
}

/// Error returned by [`SyncClient`] sync operations: either a local
/// CRDT/crypto failure or a transport failure.
#[derive(Debug, thiserror::Error)]
pub enum ClientSyncError<E>
where
    E: std::error::Error + Send + Sync + 'static,
{
    /// A local sync-engine error: AEAD open failure, delta decode
    /// failure, or a compaction-epoch mismatch (the caller must
    /// bootstrap from a snapshot — see [`SyncEngine::snapshot`]).
    #[error(transparent)]
    Sync(#[from] SyncError),

    /// The underlying [`SyncTransport`] failed.
    #[error("relay transport error: {0}")]
    Transport(#[source] E),
}

/// Outcome of a [`SyncClient::sync`] round trip.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SyncOutcome {
    /// Number of local ops uploaded to the relay this round.
    pub pushed: usize,
    /// Number of remote ops absorbed into the local engine this
    /// round (after `(replica_id, seq)` dedup).
    pub absorbed: usize,
}

/// Outcome of a [`SyncClient::pull_reporting`] call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PullReport {
    /// Remote ops absorbed into the local engine this round (after
    /// `(replica_id, seq)` dedup).
    pub absorbed: usize,
    /// Blobs skipped because they failed to AEAD-open — i.e. corrupt
    /// or forged blobs the untrusted relay served. A non-zero value is
    /// a relay-integrity signal a host may wish to surface.
    pub skipped: usize,
}

/// Drives delta sync for one [`SyncEngine`]/scope through a
/// [`SyncTransport`].
///
/// A client is bound to a single `(master_key, scope)` pair: it
/// derives the [`TopicId`] and AEAD seal key once at construction
/// and then tracks two cursors — how far its own op stream has been
/// pushed, and how far the relay's blob stream has been pulled — so
/// repeated [`Self::sync`] calls move only the incremental delta.
///
/// The client is engine-agnostic over the element type `T`: the
/// engine carries `T`, the client only manages the topic, key, and
/// cursors. This mirrors how [`crate::persist::PersistentSyncEngine`]
/// layers durability over the in-memory engine without owning the
/// element type.
///
/// `Drop` zeroises the cached seal key.
pub struct SyncClient {
    scope: SyncScopeId,
    topic: TopicId,
    seal_key: AeadKey,
    /// Highest *own* op `seq` already pushed to the relay. Own seqs
    /// are gap-free (`OpLog::clock` increments by one per local
    /// mutation), so this doubles as a count watermark.
    own_push_watermark: u64,
    /// Relay offset already consumed by [`Self::pull`].
    pull_cursor: u64,
}

impl std::fmt::Debug for SyncClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SyncClient")
            .field("scope", &self.scope.as_uuid())
            .field("topic", &self.topic)
            .field("seal_key", &"<redacted>")
            .field("own_push_watermark", &self.own_push_watermark)
            .field("pull_cursor", &self.pull_cursor)
            .finish()
    }
}

impl Drop for SyncClient {
    fn drop(&mut self) {
        self.seal_key.zeroize();
    }
}

impl SyncClient {
    /// Build a client for `scope` under `master_key`.
    ///
    /// Derives the routing [`TopicId`] and the per-scope AEAD seal
    /// key. The `master_key` is borrowed, not retained: both derived
    /// values are computed here and the root key may drop out of
    /// scope at the caller, minimising its residency.
    pub fn new(master_key: &MasterKey, scope: SyncScopeId) -> Result<Self> {
        Self::restore(master_key, scope, 0, 0)
    }

    /// Rebuild a client with previously-persisted cursors.
    ///
    /// `SyncClient::new` starts both cursors at 0, so a fresh client
    /// re-pushes every own op and re-pulls every relay blob on its
    /// first sync. That is *safe* — application dedups by
    /// `(replica_id, seq)` — but wasteful for a large log. A host that
    /// persists [`Self::push_watermark`] and [`Self::pull_cursor`]
    /// alongside its engine state can restore them here so a restarted
    /// client resumes from where it left off instead of replaying the
    /// whole topic.
    pub fn restore(
        master_key: &MasterKey,
        scope: SyncScopeId,
        push_watermark: u64,
        pull_cursor: u64,
    ) -> Result<Self> {
        let topic = TopicId::derive(master_key, scope)?;
        let seal_context = format!("sync:relay:seal:{}:v1", scope.as_uuid());
        let seal_key = derive_key(master_key, seal_context.as_bytes())?;
        Ok(Self {
            scope,
            topic,
            seal_key,
            own_push_watermark: push_watermark,
            pull_cursor,
        })
    }

    /// Scope this client syncs.
    pub fn scope(&self) -> SyncScopeId {
        self.scope
    }

    /// Routing topic this client pushes to / pulls from.
    pub fn topic(&self) -> &TopicId {
        &self.topic
    }

    /// Current pull cursor (relay offset already consumed).
    pub fn pull_cursor(&self) -> u64 {
        self.pull_cursor
    }

    /// Current push watermark (highest own op `seq` uploaded).
    pub fn push_watermark(&self) -> u64 {
        self.own_push_watermark
    }

    /// Seal `engine`'s own un-pushed ops and upload them to the relay.
    ///
    /// Encodes only the ops `engine` authored itself with
    /// `seq > push_watermark` (see
    /// [`crate::delta::encode_own_delta_since`] for why foreign ops
    /// are not re-forwarded), AEAD-seals the envelope, pushes it, and
    /// advances the push watermark. Returns the number of own ops
    /// actually uploaded (0 — and no relay round trip — when there is
    /// nothing of ours to send).
    pub fn push<T, X>(
        &mut self,
        engine: &SyncEngine<T>,
        transport: &X,
    ) -> std::result::Result<usize, ClientSyncError<X::Error>>
    where
        T: Eq + Hash + Clone + Serialize,
        X: SyncTransport,
    {
        let log = engine.op_log();
        let clock = log.clock;
        if clock <= self.own_push_watermark {
            return Ok(0);
        }

        // Count the ops we will actually encode, rather than the width
        // of the `(watermark, clock]` seq range: compaction rewrites
        // the log (dropping old ops, minting new ones at higher seqs),
        // so the range can be wider than the set of our own live ops —
        // and can even cover zero ops (e.g. compaction to the empty
        // set). Uploading then would seal an empty envelope and report
        // a phantom op count.
        let own_ops = log
            .ops
            .iter()
            .filter(|entry| {
                entry.replica_id == log.replica_id && entry.seq > self.own_push_watermark
            })
            .count();
        if own_ops == 0 {
            // Nothing of ours past the watermark. Advance it so we do
            // not rescan this range again, but make no relay round trip.
            self.own_push_watermark = clock;
            return Ok(0);
        }

        let plaintext = encode_own_delta_since(log, self.own_push_watermark)?;
        let sealed = self.seal(&plaintext)?;
        transport
            .push(&self.topic, std::slice::from_ref(&sealed))
            .map_err(ClientSyncError::Transport)?;

        self.own_push_watermark = clock;
        Ok(own_ops)
    }

    /// Pull, open, and merge every relay blob past the local cursor.
    ///
    /// Each [`SealedDelta`] is AEAD-opened and applied through
    /// [`crate::delta::apply_delta`], so application is idempotent
    /// (own blobs the relay echoes back, and any blob already seen,
    /// dedup to zero absorbed ops). Returns the total ops absorbed.
    ///
    /// A blob authored at a higher compaction epoch than `engine`
    /// surfaces [`SyncError::CompactionEpochBehind`]; the caller must
    /// then bootstrap from a snapshot before resuming delta sync. The
    /// cursor is **not** advanced past such an un-appliable blob, so
    /// the pull can be retried after the bootstrap.
    ///
    /// A blob that fails to AEAD-**open** is treated differently: only
    /// a holder of the per-scope seal key can produce a valid seal for
    /// this topic, so a blob that fails authentication carries no
    /// genuine data to lose — it is corruption or a forgery injected by
    /// the **untrusted** relay. Such a blob is skipped and the cursor
    /// advanced past it, so a hostile or buggy relay cannot wedge a
    /// replica's sync by appending one un-openable blob. The count of
    /// skipped blobs is returned alongside the absorbed-op count.
    pub fn pull<T, X>(
        &mut self,
        engine: &mut SyncEngine<T>,
        transport: &X,
    ) -> std::result::Result<usize, ClientSyncError<X::Error>>
    where
        T: Eq + Hash + Clone + Serialize + DeserializeOwned,
        X: SyncTransport,
    {
        self.pull_reporting(engine, transport).map(|r| r.absorbed)
    }

    /// [`Self::pull`] variant that also reports how many blobs were
    /// skipped because they failed to AEAD-open (forged / corrupt
    /// blobs injected by the untrusted relay). Hosts that want to
    /// surface relay-integrity anomalies can call this instead.
    pub fn pull_reporting<T, X>(
        &mut self,
        engine: &mut SyncEngine<T>,
        transport: &X,
    ) -> std::result::Result<PullReport, ClientSyncError<X::Error>>
    where
        T: Eq + Hash + Clone + Serialize + DeserializeOwned,
        X: SyncTransport,
    {
        let page = transport
            .pull(&self.topic, self.pull_cursor)
            .map_err(ClientSyncError::Transport)?;

        let mut absorbed = 0;
        let mut skipped = 0;
        let mut consumed = self.pull_cursor;
        for blob in &page.blobs {
            match self.open(blob) {
                Ok(plaintext) => absorbed += apply_delta(engine, &plaintext)?,
                // AEAD authentication failure: forged / corrupt blob —
                // skip it (see the doc comment).
                Err(SyncError::Crypto(_)) => skipped += 1,
                // Any other local error (e.g. a malformed plaintext that
                // nonetheless authenticated) is a genuine fault from a
                // key-holding peer; fail closed without advancing.
                Err(other) => return Err(ClientSyncError::Sync(other)),
            }
            consumed += 1;
        }
        // Adopt the relay's high-water cursor (it accounts for any
        // blobs the relay skipped/coalesced); fall back to the count
        // we actually consumed if the relay reported a lower value.
        self.pull_cursor = page.next_cursor.max(consumed);
        Ok(PullReport { absorbed, skipped })
    }

    /// Convenience: [`Self::push`] then [`Self::pull`] in one call.
    pub fn sync<T, X>(
        &mut self,
        engine: &mut SyncEngine<T>,
        transport: &X,
    ) -> std::result::Result<SyncOutcome, ClientSyncError<X::Error>>
    where
        T: Eq + Hash + Clone + Serialize + DeserializeOwned,
        X: SyncTransport,
    {
        let pushed = self.push(engine, transport)?;
        let absorbed = self.pull(engine, transport)?;
        Ok(SyncOutcome { pushed, absorbed })
    }

    fn seal(&self, plaintext: &[u8]) -> Result<SealedDelta> {
        let mut nonce = [0u8; AEAD_NONCE_LEN];
        // `SysRng` + the fallible `try_fill_bytes(...).expect(...)`
        // mirrors `persist::random_nonce`: a transient OS-RNG failure
        // must panic rather than silently yield a weak nonce.
        rand::rngs::SysRng
            .try_fill_bytes(&mut nonce)
            .expect("OS RNG failure");
        let ciphertext = encrypt_aead(&self.seal_key, &nonce, plaintext, self.topic.as_bytes())?;
        Ok(SealedDelta { nonce, ciphertext })
    }

    fn open(&self, blob: &SealedDelta) -> Result<Vec<u8>> {
        let plaintext = decrypt_aead(
            &self.seal_key,
            &blob.nonce,
            &blob.ciphertext,
            self.topic.as_bytes(),
        )?;
        Ok(plaintext)
    }
}

/// Process-local [`SyncTransport`]: an in-memory, append-only blob
/// log per [`TopicId`].
///
/// This is the untrusted-relay contract reduced to its essence — it
/// stores opaque [`SealedDelta`]s and never inspects them — and is
/// used by tests and single-process multi-replica scenarios. It is
/// cheap to clone-by-`Arc` and `Send + Sync`, so several
/// [`SyncClient`]s can share one instance to simulate a fleet of
/// devices behind a common relay. Network relays live in the
/// `sync_relay` crate.
#[derive(Debug, Default)]
pub struct InMemoryTransport {
    topics: Mutex<HashMap<TopicId, Vec<SealedDelta>>>,
}

impl InMemoryTransport {
    /// Construct an empty transport.
    pub fn new() -> Self {
        Self::default()
    }

    /// Snapshot the raw stored blobs for `topic`.
    ///
    /// Exposed so tests can assert the store-and-forward buffer holds
    /// only opaque ciphertext (no plaintext element bytes).
    pub fn raw_blobs(&self, topic: &TopicId) -> Vec<SealedDelta> {
        self.topics
            .lock()
            .expect("InMemoryTransport mutex poisoned")
            .get(topic)
            .cloned()
            .unwrap_or_default()
    }
}

impl SyncTransport for InMemoryTransport {
    type Error = std::convert::Infallible;

    fn push(
        &self,
        topic: &TopicId,
        blobs: &[SealedDelta],
    ) -> std::result::Result<u64, Self::Error> {
        let mut topics = self
            .topics
            .lock()
            .expect("InMemoryTransport mutex poisoned");
        let entry = topics.entry(*topic).or_default();
        entry.extend_from_slice(blobs);
        Ok(u64::try_from(entry.len()).unwrap_or(u64::MAX))
    }

    fn pull(&self, topic: &TopicId, since: u64) -> std::result::Result<PullPage, Self::Error> {
        let topics = self
            .topics
            .lock()
            .expect("InMemoryTransport mutex poisoned");
        let all = topics.get(topic);
        let len = all.map_or(0, Vec::len);
        let start = usize::try_from(since).unwrap_or(usize::MAX).min(len);
        let blobs = all.map_or_else(Vec::new, |v| v[start..].to_vec());
        Ok(PullPage {
            next_cursor: u64::try_from(len).unwrap_or(u64::MAX),
            blobs,
        })
    }
}
