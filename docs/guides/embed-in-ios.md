# Embed Knowledge in iOS (Swift / UniFFI)

This guide walks through embedding the Knowledge substrate in an iOS
app. iOS consumes the `crates/ffi` crate through UniFFI-generated Swift
bindings, so your app talks to a single, stable surface rather than the
internal crates.

## Prerequisites

- Rust **1.85+** (`rustup install stable`), with `clippy` + `rustfmt`.
- A C toolchain for the bundled SQLCipher + OpenSSL sources.
- Xcode with the iOS SDK.

## 1. Add the workspace

For a monorepo, add Knowledge as a submodule:

```bash
git submodule add https://github.com/kennguy3n/knowledge.git deps/knowledge
```

The iOS host depends on the `ffi` crate (the stable consumer API) — not
the internal crates directly.

## 2. Build the static library and Swift bindings

```bash
# Install iOS targets
rustup target add aarch64-apple-ios x86_64-apple-ios aarch64-apple-ios-sim

# Build the static library
cargo build -p ffi --release --target aarch64-apple-ios

# Generate Swift bindings (uses crates/uniffi-bindgen)
cargo run -p uniffi-bindgen -- generate \
    crates/ffi/src/knowledge.udl \
    --language swift \
    --out-dir generated/swift/

# Create an xcframework
xcodebuild -create-xcframework \
    -library target/aarch64-apple-ios/release/libknowledge_ffi.a \
    -headers generated/swift/ \
    -output Knowledge.xcframework
```

See `crates/ffi/scripts/build_ios.sh` if present for the canonical
build script.

## 3. Feature flags

The `ffi` crate is built with feature flags that gate networking:

- **Full build (with networking):** enable `http-client` for
  llama.cpp loopback inference, connectors (OAuth2 + sync), and server
  synthesis.
- **Minimal offline build:** omit `http-client`; network-dependent
  subsystems return `FfiError::Unavailable`.
- **`tracing-subscriber`:** installs a `tracing` subscriber via
  `try_init_tracing`; without it, events go nowhere (library default).

```bash
cargo build -p ffi --release --target aarch64-apple-ios \
    --features http-client,tracing-subscriber
```

## 4. Use it from Swift

The UniFFI surface exposes the full substrate contract — open a store,
ingest, query, trigger synthesis, and forget. The logical contract
matches the [API reference](../technical/api-reference.md). A typical
flow:

1. Resolve the 32-byte master key from the Keychain and open the store
   via `open_store_with_resolver` (see
   [key management](../security/key-management.md)).
2. Ingest messages as they arrive.
3. Query for retrieval and trigger synthesis for summaries.
4. Call `close_store` on teardown so the master key is zeroized.

## Key handling

Store the master key in the iOS Keychain
(`kSecAttrAccessibleWhenUnlockedThisDeviceOnly`) and use the
resolver-driven cold-boot path so the key never lives in your app's
address space as a long-lived plaintext string. See
[key management](../security/key-management.md) for the per-platform
provisioning flow.

## Further reading

- [Platform tuning](../technical/platforms.md)
- [Architecture](../technical/architecture.md)
- [Build a chat app](build-a-chat-app.md)
