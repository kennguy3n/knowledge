//! BLAKE3 content hashing.
//!
//! Per `ARCHITECTURE.md` §2.5 / §8 and `PROPOSAL.md` §3.1, BLAKE3 is the
//! single content-hash function used across the substrate (evidence
//! bodies, cold-segment framing, dedup keys).

/// Length of a BLAKE3 content hash in bytes (256 bits).
pub const CONTENT_HASH_LEN: usize = 32;

/// A 32-byte BLAKE3 content hash.
pub type ContentHash = [u8; CONTENT_HASH_LEN];

/// Compute a BLAKE3 content hash for an arbitrary byte slice.
///
/// This is the single hash function used across the evidence plane —
/// inline rows, dedup'd body table rows, ring-buffer entries, and cold
/// segment framing all hash with this function.
pub fn content_hash(data: &[u8]) -> ContentHash {
    *blake3::hash(data).as_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deterministic_hash() {
        let h1 = content_hash(b"hello world");
        let h2 = content_hash(b"hello world");
        assert_eq!(h1, h2, "BLAKE3 must be deterministic for identical input");
    }

    #[test]
    fn different_inputs_produce_different_hashes() {
        let h1 = content_hash(b"hello world");
        let h2 = content_hash(b"hello world!");
        assert_ne!(h1, h2);
    }

    #[test]
    fn empty_input_is_well_defined() {
        // BLAKE3 of empty input is a known constant.
        let h = content_hash(b"");
        // Non-zero, well-defined output.
        assert_ne!(h, [0u8; CONTENT_HASH_LEN]);
    }

    #[test]
    fn hash_length_is_32() {
        let h = content_hash(b"test");
        assert_eq!(h.len(), CONTENT_HASH_LEN);
    }
}
