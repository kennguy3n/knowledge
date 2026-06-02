# crypto

Post-quantum cryptographic primitives for the Knowledge substrate.

## Purpose

Implements the high-level cryptographic API consumed by the rest of
the Rust shared core. Deliberately exposes a small, opinionated
surface so the rest of the substrate never touches raw cryptographic
state directly.

## Implemented primitives

- **BLAKE3** content hashing.
- **XChaCha20-Poly1305 AEAD** for per-scope symmetric encryption.
- **HKDF-SHA256** key derivation from a per-user master key.
- **Hybrid X25519 + ML-KEM-768** key encapsulation (post-quantum).
- **ML-DSA-65** (FIPS 204) lattice signatures for provenance.
- **SPHINCS+-SHAKE-128f-simple** stateless hash-based signatures for
  archival co-signing.

## Public API summary

| Type / Function | Description |
|---|---|
| `MasterKey` | Per-user master key wrapper. |
| `derive_key` | HKDF-SHA256 context-scoped key derivation. |
| `encrypt` / `decrypt` | XChaCha20-Poly1305 AEAD. |
| `ContentHash` | BLAKE3 content hash. |
| `KemBackend` | Trait for hybrid KEM (swappable for liboqs FFI). |
| `MlDsa65Signer` | ML-DSA-65 provenance signer. |
| `SphincsPlusSigner` / `CoSigner` | SPHINCS+ archival co-signing. |
| `ProvenanceSigner` | Trait abstracting signature backends. |

## Feature flags

| Feature | Description |
|---|---|
| `test-support` | Enables `StubKemBackend`, `TestSigner`, `DeterministicEpochKeySource`, `InMemoryKeyStorage`. |

## Links

- [ARCHITECTURE.md](../../ARCHITECTURE.md) §2.5, §8 — Crypto layer.
- [docs/DESIGN.md](../../docs/DESIGN.md) §5 — Post-quantum cryptography.
- [docs/INTEGRATION_GUIDE.md](../../docs/INTEGRATION_GUIDE.md) — Consumer integration guide.
