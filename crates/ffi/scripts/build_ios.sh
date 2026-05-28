#!/usr/bin/env bash
#
# Builds the Knowledge iOS Swift package + XCFramework from the
# UniFFI-wired `ffi` crate.
#
# Output layout (relative to `crates/ffi/`):
#
#   ios/
#     Knowledge/                    Swift package (Sources/, Package.swift)
#     RustXcframework.xcframework/  Static lib bundled for iOS device + sim
#
# The Swift package depends on `RustXcframework.xcframework` and
# re-exports the UniFFI-generated Swift sources under the `Knowledge`
# module (per `crates/ffi/uniffi.toml` `[bindings.swift] module_name`).
#
# Consumed by `uneycom/kchat-next-ios` via either:
#   1. Local path SwiftPM dependency for development builds, or
#   2. The pre-built artefact published by the
#      `.github/workflows/ci.yml` `ios-build` job.
#
# Pattern mirrored from
# `uneycom/kchat-rust-sdk/scripts/mls_uniffi_build_ios.sh`.

set -eo pipefail

# Resolve `crates/ffi/` regardless of where the script is invoked from.
# `realpath -m` keeps this portable across macOS (`coreutils`'s
# `realpath`) and Linux (GNU `realpath`).
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
FFI_DIR="$(cd "${SCRIPT_DIR}/.." && pwd)"

# `cargo swift package` requires `cargo-swift` to be installed.
# Install via `cargo install cargo-swift` if missing; the CI job
# bakes it into the runner image so this `command -v` check
# short-circuits there.
if ! command -v cargo-swift >/dev/null 2>&1; then
  echo "error: cargo-swift is not installed."
  echo "       install it with: cargo install cargo-swift"
  exit 1
fi

# iOS deployment target — matches the workspace-wide target set in
# `kchat-rust-sdk`'s mobile build pipeline (iOS 16.0). Hosts that
# need a different floor can override via the environment, but they
# MUST keep it at iOS 16.0 or newer because the substrate's
# SQLCipher / `liblzma`-based dependencies do not link cleanly
# against earlier iOS SDKs in our CI configuration.
IOS_DEPLOYMENT_TARGET="${IOS_DEPLOYMENT_TARGET:-16.0}"
export IPHONEOS_DEPLOYMENT_TARGET="$IOS_DEPLOYMENT_TARGET"

# Clean any previous output to make the build hermetic — `cargo
# swift package` will fail with confusing errors if a stale
# `ios/Knowledge/Package.swift` is left behind from a previous run
# with a different module name.
rm -rf "${FFI_DIR}/ios"

# `cargo swift package` invokes `cargo build` for each iOS Apple
# target, runs `lipo` to fat-pack the device + simulator slices,
# generates the `.xcframework`, and emits a Swift package skeleton
# wrapping the UniFFI-generated Swift sources.
#
#   -y           Skip the interactive "choose targets" prompt.
#   -p ios       Target the iOS platform (device + simulator slices).
#   --release    Build in release profile (debug symbols stripped).
#   -n ios       Name the output directory "ios/".
#
# The script intentionally does NOT pass `--config uniffi.toml` —
# `cargo swift package` reads the manifest from the crate root and
# our `uniffi.toml` is picked up automatically.
cd "${FFI_DIR}"
cargo swift package -y -p "ios" --release -n ios

echo
echo "iOS Swift package + XCFramework built at ${FFI_DIR}/ios"
echo "  Swift package:    ${FFI_DIR}/ios/Knowledge/"
echo "  XCFramework:      ${FFI_DIR}/ios/RustXcframework.xcframework/"
