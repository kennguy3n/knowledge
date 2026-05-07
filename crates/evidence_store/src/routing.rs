//! Storage routing — pick the storage path for an incoming evidence
//! body based on size and importance class.
//!
//! Per `ARCHITECTURE.md` §2.2 / §9.1 and `PROPOSAL.md` §3.1:
//!
//! * `Noise` always goes to the ring buffer regardless of size.
//! * Bodies with `len ≤ 512 B` go inline in the evidence row (no
//!   dedup index lookup — optimised for short chat messages).
//! * Bodies with `len > 512 B` go to the deduplicated body table
//!   keyed by BLAKE3 content hash.

use crate::importance::ImportanceClass;

/// Default size threshold (bytes) below which a non-noise body is
/// stored inline. Per `ARCHITECTURE.md` §2.2 and `PROPOSAL.md` §3.1.
pub const DEFAULT_INLINE_THRESHOLD_BYTES: usize = 512;

/// The three storage paths a body can take through the evidence plane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoragePath {
    /// Inline path — encrypted body lives directly in `evidence.body`.
    /// Chosen when the body is short (`≤ inline_threshold`) and the
    /// importance class is non-noise.
    Inline,
    /// Body-table path — encrypted body lives in the deduplicated
    /// `body_store` table keyed by BLAKE3 content hash.
    BodyTable,
    /// Ring-buffer path — body lives only in the FIFO ring buffer
    /// for the current synthesis window. Always chosen for
    /// `ImportanceClass::Noise`.
    RingBuffer,
}

/// Pick the storage path for a body of length `body_len` and
/// importance `importance`, using the default 512-byte inline
/// threshold.
pub fn route_storage(body_len: usize, importance: ImportanceClass) -> StoragePath {
    route_storage_with_threshold(body_len, importance, DEFAULT_INLINE_THRESHOLD_BYTES)
}

/// Lower-level routing function exposing the inline threshold.
pub fn route_storage_with_threshold(
    body_len: usize,
    importance: ImportanceClass,
    inline_threshold: usize,
) -> StoragePath {
    if matches!(importance, ImportanceClass::Noise) {
        return StoragePath::RingBuffer;
    }
    if body_len <= inline_threshold {
        StoragePath::Inline
    } else {
        StoragePath::BodyTable
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn noise_always_routes_to_ring_buffer() {
        assert_eq!(
            route_storage(0, ImportanceClass::Noise),
            StoragePath::RingBuffer
        );
        assert_eq!(
            route_storage(100, ImportanceClass::Noise),
            StoragePath::RingBuffer
        );
        assert_eq!(
            route_storage(10_000, ImportanceClass::Noise),
            StoragePath::RingBuffer
        );
    }

    #[test]
    fn small_non_noise_routes_inline() {
        for class in [
            ImportanceClass::Useful,
            ImportanceClass::Important,
            ImportanceClass::Critical,
        ] {
            assert_eq!(route_storage(1, class), StoragePath::Inline);
            assert_eq!(route_storage(512, class), StoragePath::Inline);
        }
    }

    #[test]
    fn large_non_noise_routes_to_body_table() {
        for class in [
            ImportanceClass::Useful,
            ImportanceClass::Important,
            ImportanceClass::Critical,
        ] {
            assert_eq!(route_storage(513, class), StoragePath::BodyTable);
            assert_eq!(route_storage(10_000, class), StoragePath::BodyTable);
        }
    }

    #[test]
    fn threshold_can_be_overridden() {
        assert_eq!(
            route_storage_with_threshold(64, ImportanceClass::Useful, 32),
            StoragePath::BodyTable
        );
        assert_eq!(
            route_storage_with_threshold(32, ImportanceClass::Useful, 32),
            StoragePath::Inline
        );
    }
}
