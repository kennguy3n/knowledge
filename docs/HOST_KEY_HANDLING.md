# Host-shell key handling guide

This document provides **explicit, per-platform guidance** for
consumer products that embed the `knowledge` substrate via the
FFI boundary (`crates/ffi/`) or the N-API addon (`crates/napi/`).

The substrate's master key is a 32-byte (`MASTER_KEY_LEN`)
symmetric secret. It is the root of trust for every scope DEK,
every AEAD encryption, and every HKDF derivation in the
evidence store. The substrate treats this key as opaque
derive-only bytes and zeroizes its in-process copy on
`close_store`. **Everything above the FFI boundary — generation,
persistence, biometric gating, hardware binding — is the host
shell's responsibility.**

## 1. The `KeyStorageResolver` contract

The substrate exposes a `KeyStorageResolver` callback trait
(see `crates/ffi/src/key_storage.rs`) that host shells implement
against their platform's secure store:

```text
trait KeyStorageResolver {
    fn load_key(key_id: &str) -> Result<MasterKey, FfiError>;
    fn store_key(key_id: &str, key: &MasterKey) -> Result<(), FfiError>;
    fn delete_key(key_id: &str) -> Result<(), FfiError>;
}
```

The resolver is registered once at cold boot via
`open_store_with_resolver`. The substrate calls `load_key` to
obtain the master key, derives all scope DEKs from it, and
zeroizes the master key copy when the store is closed. The
resolver must:

1. Return the key from a hardware-backed store (never from a
   plaintext file or environment variable in production).
2. Gate access behind biometric or passcode authentication where
   the platform supports it.
3. Zeroize any in-flight copy it makes internally before
   returning.

## 2. Threat model

### What the substrate protects

| Guarantee | Mechanism |
|-----------|-----------|
| Data at rest is encrypted | XChaCha20-Poly1305 per scope/epoch DEK |
| DEKs are derived from the master key | HKDF-SHA256 with per-scope context |
| Cryptographic forgetting | DEK destruction → ciphertext permanently unrecoverable |
| Harvest-now-decrypt-later resistance | Hybrid X25519 + ML-KEM-768 key exchange |
| Key material is wiped from process memory | `ZeroizeOnDrop` on `HybridSecretKey`, `MasterKey`, DEK structs |

### What the substrate **cannot** protect against

| Threat | Why it is outside scope |
|--------|------------------------|
| Master key leaked by the host shell | If the key crosses the FFI boundary in plaintext and the host stores it insecurely (e.g. `UserDefaults`, `SharedPreferences` in cleartext, `localStorage`), an attacker who reads the host's storage has the root secret. The substrate never sees the host's storage layer. |
| Host process memory dump | If an attacker can read the host process's heap while the substrate is running, they can recover the master key (or any decrypted plaintext currently in flight). `ZeroizeOnDrop` shrinks the window but does not eliminate it. |
| OS-level filesystem snapshots | If the host OS (or a backup tool) snapshots the SQLCipher database *and* the master key storage at the same time, the snapshot is a coherent encrypted database + key pair. This is an OS-level policy issue. |
| Compromised build chain | If the host shell ships a binary that was tampered with at build time (e.g. the `KeyStorageResolver` implementation is replaced with one that exfiltrates the key), the substrate cannot detect it. Code-signing and supply-chain verification are the host's responsibility. |

### Severity matrix

| Scenario | Impact |
|----------|--------|
| Master key stored in plaintext on disk | **Critical** — full evidence-store compromise; all scope DEKs derivable; cryptographic forgetting guarantee void. |
| Master key in Keychain/Keystore without biometric gate | **High** — any app running under the same user (or a jailbroken/rooted device) can read the key. |
| Master key in hardware-backed store with biometric gate | **Low** — attacker needs physical access + biometric bypass or TEE exploit. |

## 3. Platform integration patterns

### 3.1 iOS — Keychain Services

Use `kSecAttrAccessibleWhenUnlockedThisDeviceOnly` so the key
is not included in iCloud Keychain sync or unencrypted backups.

```swift
import Foundation
import Security

final class KeychainKeyStorage: KeyStorageResolver {
    private let service = "com.example.knowledge-substrate"

    func loadKey(keyId: String) throws -> Data {
        let query: [String: Any] = [
            kSecClass as String:       kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: keyId,
            kSecReturnData as String:  true,
            kSecMatchLimit as String:  kSecMatchLimitOne,
        ]
        var result: AnyObject?
        let status = SecItemCopyMatching(query as CFDictionary, &result)
        guard status == errSecSuccess, let data = result as? Data else {
            throw KeyStorageError.notFound
        }
        return data
    }

    func storeKey(keyId: String, key: Data) throws {
        // Delete any existing entry first (upsert semantics).
        deleteKey(keyId: keyId)

        let attrs: [String: Any] = [
            kSecClass as String:            kSecClassGenericPassword,
            kSecAttrService as String:      service,
            kSecAttrAccount as String:      keyId,
            kSecValueData as String:        key,
            kSecAttrAccessible as String:   kSecAttrAccessibleWhenUnlockedThisDeviceOnly,
            // On devices with Secure Enclave (A7+), this flag
            // causes the Keychain to wrap the item key with the
            // SE's hardware UID key.
            kSecAttrAccessControl as String:
                SecAccessControlCreateWithFlags(
                    nil,
                    kSecAttrAccessibleWhenUnlockedThisDeviceOnly,
                    .biometryCurrentSet,
                    nil
                )!,
        ]
        let status = SecItemAdd(attrs as CFDictionary, nil)
        guard status == errSecSuccess else {
            throw KeyStorageError.storeFailed(status)
        }
    }

    func deleteKey(keyId: String) {
        let query: [String: Any] = [
            kSecClass as String:       kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: keyId,
        ]
        SecItemDelete(query as CFDictionary)
    }
}
```

**Key points:**
- `kSecAttrAccessibleWhenUnlockedThisDeviceOnly` prevents iCloud
  sync and unencrypted iTunes backups.
- `.biometryCurrentSet` binds the item to the enrolled biometrics;
  re-enrollment invalidates the key (forces re-provisioning).
- On Apple Silicon Macs with Secure Enclave, the same Keychain
  API applies — the SE wraps the item key with its hardware UID
  key automatically when the `kSecAttrTokenID` /
  `kSecAttrAccessControl` flags are set.

### 3.2 Android — Android Keystore

Use `setUserAuthenticationRequired(true)` to bind key access to
device unlock (PIN/pattern/biometric).

```kotlin
import android.security.keystore.KeyGenParameterSpec
import android.security.keystore.KeyProperties
import java.security.KeyStore
import javax.crypto.Cipher
import javax.crypto.KeyGenerator
import javax.crypto.SecretKey
import javax.crypto.spec.GCMParameterSpec

class AndroidKeyStorage {

    companion object {
        private const val KEYSTORE_PROVIDER = "AndroidKeyStore"
        private const val WRAPPER_ALIAS = "knowledge_master_key_wrapper"
        private const val GCM_TAG_LEN = 128
    }

    /**
     * Generate a hardware-backed AES-256-GCM wrapping key in the
     * Android Keystore. The wrapping key never leaves the TEE /
     * StrongBox.
     */
    fun ensureWrapperKey() {
        val ks = KeyStore.getInstance(KEYSTORE_PROVIDER).apply { load(null) }
        if (ks.containsAlias(WRAPPER_ALIAS)) return

        val spec = KeyGenParameterSpec.Builder(
            WRAPPER_ALIAS,
            KeyProperties.PURPOSE_ENCRYPT or KeyProperties.PURPOSE_DECRYPT
        )
            .setBlockModes(KeyProperties.BLOCK_MODE_GCM)
            .setEncryptionPaddings(KeyProperties.ENCRYPTION_PADDING_NONE)
            .setKeySize(256)
            .setUserAuthenticationRequired(true)
            // Require authentication within the last 10 seconds.
            .setUserAuthenticationValidityDurationSeconds(10)
            // Prefer StrongBox (Titan M / equivalent) when
            // available. Falls back to TEE silently.
            .setIsStrongBoxBacked(true)
            .build()

        KeyGenerator.getInstance(
            KeyProperties.KEY_ALGORITHM_AES, KEYSTORE_PROVIDER
        ).apply {
            init(spec)
            generateKey()
        }
    }

    /** Wrap (encrypt) the substrate master key for persistence. */
    fun wrapMasterKey(masterKey: ByteArray): Pair<ByteArray, ByteArray> {
        val cipher = Cipher.getInstance("AES/GCM/NoPadding")
        val key = loadWrapperKey()
        cipher.init(Cipher.ENCRYPT_MODE, key)
        val ciphertext = cipher.doFinal(masterKey)
        return Pair(cipher.iv, ciphertext)
    }

    /** Unwrap (decrypt) the substrate master key. */
    fun unwrapMasterKey(iv: ByteArray, wrapped: ByteArray): ByteArray {
        val cipher = Cipher.getInstance("AES/GCM/NoPadding")
        val key = loadWrapperKey()
        cipher.init(Cipher.DECRYPT_MODE, key, GCMParameterSpec(GCM_TAG_LEN, iv))
        return cipher.doFinal(wrapped)
    }

    private fun loadWrapperKey(): SecretKey {
        val ks = KeyStore.getInstance(KEYSTORE_PROVIDER).apply { load(null) }
        return ks.getKey(WRAPPER_ALIAS, null) as SecretKey
    }
}
```

**Key points:**
- `setUserAuthenticationRequired(true)` ensures the wrapping key
  is only accessible after the user has authenticated (biometric
  or lock-screen credential).
- `setIsStrongBoxBacked(true)` routes key operations through the
  hardware security module (Titan M on Pixel, equivalent on other
  OEMs). The flag is a preference, not a hard requirement — the
  system falls back to the TEE if StrongBox is absent.
- The substrate master key is wrapped (AES-256-GCM) under the
  Keystore-resident key. The wrapped blob + IV are stored in the
  app's private storage; the wrapping key itself never leaves the
  TEE/StrongBox.

### 3.3 macOS — Keychain + Secure Enclave

On Apple Silicon Macs, the Secure Enclave is available via the
same `Security.framework` API used on iOS:

```swift
import Foundation
import Security

final class MacKeyStorage: KeyStorageResolver {
    private let service = "com.example.knowledge-substrate"

    func loadKey(keyId: String) throws -> Data {
        let query: [String: Any] = [
            kSecClass as String:       kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: keyId,
            kSecReturnData as String:  true,
            kSecMatchLimit as String:  kSecMatchLimitOne,
        ]
        var result: AnyObject?
        let status = SecItemCopyMatching(query as CFDictionary, &result)
        guard status == errSecSuccess, let data = result as? Data else {
            throw KeyStorageError.notFound
        }
        return data
    }

    func storeKey(keyId: String, key: Data) throws {
        deleteKey(keyId: keyId)

        var error: Unmanaged<CFError>?
        // On Apple Silicon, kSecAttrTokenIDSecureEnclave routes
        // the item's wrapping key to the Secure Enclave. On
        // Intel Macs without a T2 chip, omit this flag and fall
        // back to the software Keychain.
        let access = SecAccessControlCreateWithFlags(
            nil,
            kSecAttrAccessibleWhenUnlockedThisDeviceOnly,
            [.biometryCurrentSet, .privateKeyUsage],
            &error
        )
        if let error = error {
            throw KeyStorageError.accessControl(error.takeRetainedValue())
        }

        let attrs: [String: Any] = [
            kSecClass as String:          kSecClassGenericPassword,
            kSecAttrService as String:    service,
            kSecAttrAccount as String:    keyId,
            kSecValueData as String:      key,
            kSecAttrAccessControl as String: access!,
            kSecAttrSynchronizable as String: false,
        ]
        let status = SecItemAdd(attrs as CFDictionary, nil)
        guard status == errSecSuccess else {
            throw KeyStorageError.storeFailed(status)
        }
    }

    func deleteKey(keyId: String) {
        let query: [String: Any] = [
            kSecClass as String:       kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: keyId,
        ]
        SecItemDelete(query as CFDictionary)
    }
}
```

**Key points:**
- On Apple Silicon (M1+), the Keychain can route item wrapping
  through the Secure Enclave. The SE never exposes the wrapping
  key — it performs AES-256 wrap/unwrap operations internally.
- On Intel Macs with a T2 chip, `kSecAttrTokenIDSecureEnclave`
  routes through the T2's secure enclave.
- On Intel Macs without a T2, the software Keychain is the best
  available option. Omit the SE-specific flags and accept the
  reduced hardware isolation.
- `kSecAttrSynchronizable: false` prevents iCloud Keychain sync.

### 3.4 Windows — DPAPI + TPM 2.0

```typescript
// TypeScript / Electron example using Node.js native APIs.
// The `electron-dpapi` package wraps `CryptProtectData` /
// `CryptUnprotectData` from `crypt32.dll`.

import { app } from "electron";
import * as path from "path";
import * as fs from "fs";
import * as dpapi from "electron-dpapi";

const WRAPPED_KEY_PATH = path.join(
  app.getPath("userData"),
  "knowledge-master-key.enc"
);

/**
 * Store the master key using DPAPI (user-scope protection).
 * The wrapped blob is bound to the current Windows user's
 * login credentials — a different user on the same machine
 * cannot unwrap it.
 *
 * On Windows 11 with Pluton/TPM 2.0, DPAPI internally routes
 * the wrapping key through the TPM. No code change is required
 * on the application side.
 */
export function storeMasterKey(masterKey: Buffer): void {
  const wrapped = dpapi.protectData(
    masterKey,
    null,   // optional entropy (additional secret)
    "CurrentUser"  // scope: CurrentUser or LocalMachine
  );
  fs.writeFileSync(WRAPPED_KEY_PATH, wrapped);
  // Zeroize the plaintext buffer.
  masterKey.fill(0);
}

/**
 * Load the master key from the DPAPI-wrapped blob.
 */
export function loadMasterKey(): Buffer {
  if (!fs.existsSync(WRAPPED_KEY_PATH)) {
    throw new Error("Master key not found — run first-time setup");
  }
  const wrapped = fs.readFileSync(WRAPPED_KEY_PATH);
  return dpapi.unprotectData(wrapped, null, "CurrentUser");
}

/**
 * Delete the wrapped master key from disk.
 */
export function deleteMasterKey(): void {
  if (fs.existsSync(WRAPPED_KEY_PATH)) {
    // Overwrite before unlinking to prevent recovery from
    // filesystem journal / undelete tools.
    const size = fs.statSync(WRAPPED_KEY_PATH).size;
    fs.writeFileSync(WRAPPED_KEY_PATH, Buffer.alloc(size, 0));
    fs.unlinkSync(WRAPPED_KEY_PATH);
  }
}
```

**Key points:**
- DPAPI (`CryptProtectData`) binds the wrapped blob to the
  current Windows user's DPAPI master key, which is itself
  derived from the user's login credential (password / Windows
  Hello / PIN).
- On Windows 11 with Pluton or a discrete TPM 2.0, DPAPI
  internally uses the TPM as a key-wrapping oracle. No
  application-level change is needed — the platform upgrade is
  transparent.
- The `CurrentUser` scope prevents other accounts on the same
  machine from unwrapping the blob.
- Buffer zeroization (`fill(0)`) is best-effort in
  JavaScript/TypeScript — the GC may have already copied the
  buffer. For defence-in-depth, consider performing the
  unwrap + FFI call in the N-API addon's C++ layer where
  `memset_s` / `SecureZeroMemory` is available.

## 4. Anti-patterns

| Anti-pattern | Risk | Fix |
|-------------|------|-----|
| Storing the master key in `UserDefaults` / `SharedPreferences` (cleartext) | **Critical** — readable by any app with root or backup access | Use Keychain / Keystore as shown above |
| Passing the master key as a CLI argument or environment variable | **High** — visible in `/proc/pid/cmdline`, process listing, shell history | Use `open_store_with_resolver` instead of `open_store` with a hex string |
| Hardcoding a master key in source code | **Critical** — anyone with the binary or source has the key | Generate at first run, store in platform secure store |
| Using `localStorage` or IndexedDB in Electron renderer | **Critical** — XSS → full key exfiltration; see `docs/ELECTRON_SECURITY.md` | Use the main-process `KeyStorageResolver` via IPC |
| Logging the master key (even at `debug` level) | **High** — log aggregation pipelines may persist the key indefinitely | Never log key material; the substrate's own code gates key-related tracing behind `#[cfg(test)]` |
| Skipping biometric/PIN gates on mobile | **High** — any process running as the same user can read the Keychain/Keystore entry | Set `setUserAuthenticationRequired` / `.biometryCurrentSet` |
| Syncing the key via iCloud Keychain or Google Backup | **High** — the key is replicated to cloud infrastructure outside the user's device | Use `kSecAttrAccessibleWhenUnlockedThisDeviceOnly` / `setIsStrongBoxBacked(true)` |

## 5. First-run provisioning flow

```text
┌─────────────────────────────────────────────────────────────────┐
│                     Host shell (first run)                      │
├─────────────────────────────────────────────────────────────────┤
│ 1. Generate 32 bytes from OS CSPRNG                            │
│ 2. Store in platform secure store via KeyStorageResolver        │
│    (Keychain / Keystore / DPAPI)                               │
│ 3. Call open_store_with_resolver(db_path, resolver)            │
│ 4. Substrate calls resolver.load_key(key_id) → master key     │
│ 5. Substrate derives scope DEKs via HKDF                       │
│ 6. Substrate zeroizes its copy of the master key on close      │
└─────────────────────────────────────────────────────────────────┘
```

On subsequent launches, step 1 is skipped — the resolver loads
the existing key from the platform store.

## 6. Key rotation

The substrate does not currently implement master-key rotation
(tracked as a post-1.0 feature). If a host suspects the master
key has been compromised:

1. Generate a new master key.
2. Open a new evidence store with the new key.
3. Re-encrypt all evidence rows from the old store to the new
   store (the substrate exposes the read API for this).
4. Destroy the old master key via `resolver.delete_key`.
5. Destroy the old database file.

This is a destructive migration — the host must ensure it
completes atomically (or at least idempotently) to avoid
data loss.

## 7. Cross-references

- `crates/crypto/src/key_storage.rs` — `KeyStorage` trait and
  `InMemoryKeyStorage` reference implementation.
- `crates/ffi/src/key_storage.rs` — `KeyStorageResolver`
  cross-language callback trait and registration API.
- `crates/crypto/src/kdf.rs` — `MasterKey`, `MASTER_KEY_LEN`,
  `derive_key`.
- `docs/ELECTRON_SECURITY.md` — Electron renderer-process
  threat model (IPC allowlist, CSP, `contextIsolation`).
- `SECURITY.md` — project-wide security policy and audit scope.
