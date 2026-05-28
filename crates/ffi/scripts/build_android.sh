#!/usr/bin/env bash
#
# Builds the Knowledge Android library: Kotlin bindings + JNI `.so`
# libraries packaged in the `jniLibs/` directory layout the Android
# Gradle Plugin expects.
#
# Output layout (relative to `crates/ffi/`):
#
#   android/
#     uniffi/knowledge/knowledge.kt           Generated Kotlin bindings
#     uniffi/knowledge/jniLibs/
#       arm64-v8a/libffi.so                    JNI lib for ARM64 devices
#       x86_64/libffi.so                       JNI lib for emulators
#
# Consumed by `uneycom/kchat-next-android` via either:
#   1. Local path Gradle dependency for development builds, or
#   2. The pre-built artefact published by the
#      `.github/workflows/ci.yml` `android-build` job.
#
# Pattern mirrored from
# `uneycom/kchat-rust-sdk/scripts/mls_uniffi_build_android.sh`.

set -eo pipefail

# Resolve `crates/ffi/` and the workspace root regardless of where
# the script is invoked from.
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
FFI_DIR="$(cd "${SCRIPT_DIR}/.." && pwd)"
WORKSPACE_ROOT="$(cd "${FFI_DIR}/../.." && pwd)"

# Verify `cargo-ndk` is installed; emit an actionable hint if not.
# The CI job bakes it into the runner image; local devs install it
# once via `cargo install cargo-ndk`.
if ! command -v cargo-ndk >/dev/null 2>&1; then
  echo "error: cargo-ndk is not installed."
  echo "       install it with: cargo install cargo-ndk"
  exit 1
fi

ANDROID_DIR="${FFI_DIR}/android"

# Host library extension: `.so` on Linux, `.dylib` on macOS,
# `.dll` on Windows. The host-side build below produces the
# dynamic library that `uniffi-bindgen --library` walks to
# discover the exported metadata — its filename depends on the
# build host.
case "$(uname -s)" in
  Linux*)   HOST_LIB_EXT=.so ;;
  Darwin*)  HOST_LIB_EXT=.dylib ;;
  *)        HOST_LIB_EXT=.so ;;
esac
HOST_LIB_PATH="${WORKSPACE_ROOT}/target/release/libffi${HOST_LIB_EXT}"

# Clean previous output for a hermetic build.
rm -rf "${ANDROID_DIR}"
mkdir -p "${ANDROID_DIR}"

# Step 1: build the host-architecture release `.so` so
# `uniffi-bindgen` has a library to introspect when emitting
# Kotlin sources. UniFFI's library-mode bindgen reads the metadata
# blob embedded in the compiled artefact rather than from a UDL
# file, which is what makes our proc-macro-only wiring (no UDL)
# work end-to-end.
cd "${WORKSPACE_ROOT}"
cargo build --release -p ffi

# Step 2: generate Kotlin bindings against the host-arch `.so`.
# The output `package_name` and `android = true` flags come from
# `crates/ffi/uniffi.toml`.
cargo run --release -p ffi --bin uniffi-bindgen -- generate \
  --library "${HOST_LIB_PATH}" \
  --language kotlin \
  --out-dir "${ANDROID_DIR}" \
  --config "${FFI_DIR}/uniffi.toml" \
  --no-format

# Step 3: cross-compile the `cdylib` for the two Android ABIs the
# host modules ship. `arm64-v8a` covers every Android device since
# the 64-bit transition (2019+); `x86_64` covers the Android
# emulator on x86_64 dev machines. 32-bit ABIs (`armeabi-v7a`,
# `x86`) are intentionally NOT included — the substrate's
# SQLCipher / liblzma deps do not link cleanly on 32-bit
# Android, and Google Play has required 64-bit since 2019.
cargo ndk \
  --manifest-path "${FFI_DIR}/Cargo.toml" \
  -t arm64-v8a \
  -t x86_64 \
  -o "${ANDROID_DIR}/uniffi/knowledge/jniLibs" \
  build --release

echo
echo "Android library built at ${ANDROID_DIR}"
echo "  Kotlin bindings:  ${ANDROID_DIR}/uniffi/knowledge/"
echo "  JNI libraries:    ${ANDROID_DIR}/uniffi/knowledge/jniLibs/"
