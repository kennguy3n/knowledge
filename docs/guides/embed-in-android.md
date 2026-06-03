# Embed Knowledge in Android (Kotlin / JNI)

This guide walks through embedding the Knowledge substrate in an
Android app. Android consumes the `crates/ffi` crate through
UniFFI-generated Kotlin bindings over JNI, so your app talks to a
single, stable surface rather than the internal crates.

## Prerequisites

- Rust **1.85+** (`rustup install stable`), with `clippy` + `rustfmt`.
- A C toolchain for the bundled SQLCipher + OpenSSL sources.
- The Android NDK and `cargo-ndk` (`cargo install cargo-ndk`).

## 1. Add the workspace

For a monorepo, add Knowledge as a submodule:

```bash
git submodule add https://github.com/kennguy3n/knowledge.git deps/knowledge
```

The Android host depends on the `ffi` crate (the stable consumer API).

## 2. Build the shared libraries and Kotlin bindings

```bash
# Install Android targets
rustup target add aarch64-linux-android armv7-linux-androideabi x86_64-linux-android

# Build with cargo-ndk into a jniLibs tree
cargo ndk -p ffi --release \
    -t arm64-v8a -t armeabi-v7a -t x86_64 \
    -o ./jniLibs

# Generate Kotlin bindings
cargo run -p uniffi-bindgen -- generate \
    crates/ffi/src/knowledge.udl \
    --language kotlin \
    --out-dir generated/kotlin/
```

Drop the `jniLibs` output into your app module's
`src/main/jniLibs/` and the generated Kotlin into your source set.

## 3. Feature flags

Build the `ffi` crate with feature flags that gate networking:

- **Full build (with networking):** enable `http-client` for inference,
  connectors, and server synthesis.
- **Minimal offline build:** omit `http-client`; network-dependent
  subsystems return `FfiError::Unavailable`.
- **`tracing-subscriber`:** installs a `tracing` subscriber via
  `try_init_tracing`.

```bash
cargo ndk -p ffi --release -t arm64-v8a -o ./jniLibs \
    -- --features http-client,tracing-subscriber
```

## 4. Use it from Kotlin

The UniFFI surface exposes the full substrate contract — open a store,
ingest, query, trigger synthesis, and forget — matching the
[API reference](../technical/api-reference.md). A typical flow:

1. Resolve the 32-byte master key from the Android Keystore and open
   the store via `open_store_with_resolver` (see
   [key management](../security/key-management.md)).
2. Ingest messages as they arrive.
3. Query for retrieval and trigger synthesis for summaries.
4. Call `close_store` on teardown so the master key is zeroized.

## Key handling

Store the master key in the Android Keystore (StrongBox on Pixel 6+)
and use the resolver-driven cold-boot path so the key never lives in
your app's address space as a long-lived plaintext string. See
[key management](../security/key-management.md).

## Further reading

- [Platform tuning](../technical/platforms.md)
- [Architecture](../technical/architecture.md)
- [Build a chat app](build-a-chat-app.md)
