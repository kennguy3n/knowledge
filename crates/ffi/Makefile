# Knowledge FFI build targets.
#
# Drives the iOS / Android UniFFI build pipelines so platform
# integrators (the host repos `uneycom/kchat-next-ios` and
# `uneycom/kchat-next-android`) can produce the consumable
# artefacts (`.xcframework` + Swift package, `.so` JNI libs +
# Kotlin bindings) with a single command from this crate's
# directory.
#
# Usage:
#   make build-ios        Build iOS Swift package + .xcframework
#   make build-android    Build Android JNI .so + Kotlin bindings
#   make build-all        Build both (sequentially; requires macOS for iOS)
#   make bindgen-kotlin   Re-generate Kotlin bindings against the
#                         host-arch .so without rebuilding it
#   make bindgen-swift    Re-generate Swift bindings against the
#                         host-arch .so without rebuilding it
#   make clean            Remove generated platform output dirs
#
# Prerequisites (each script `command -v`-checks and emits an
# install hint if missing):
#   * `cargo-swift`  for `build-ios`    (cargo install cargo-swift)
#   * `cargo-ndk`    for `build-android` (cargo install cargo-ndk)
#   * Android NDK toolchain on PATH for `build-android`
#   * Xcode + iOS SDK 16.0+ for `build-ios` (Apple platforms only)

# Top-level marker — `bash` is required for the build scripts; we
# pin `SHELL` so distros that default to `dash` still execute the
# `set -eo pipefail` / array syntax correctly.
SHELL := /bin/bash

# Resolve the workspace root relative to this Makefile so targets
# can be invoked from any CWD (Makefiles inherit `$(MAKEFILE_LIST)`).
CRATE_DIR := $(abspath $(dir $(lastword $(MAKEFILE_LIST))))
WORKSPACE_ROOT := $(abspath $(CRATE_DIR)/../..)

.PHONY: build-ios build-android build-all bindgen-kotlin bindgen-swift clean

build-ios:
	$(CRATE_DIR)/scripts/build_ios.sh

build-android:
	$(CRATE_DIR)/scripts/build_android.sh

build-all: build-ios build-android

# `bindgen-kotlin` and `bindgen-swift` rebuild only the generated
# sources — useful when iterating on `uniffi.toml` namespacing.
# Both require an existing host-arch `.so` (or `.dylib` on macOS)
# in `target/release/`. If it isn't there, run `cargo build
# --release -p ffi` first or call the full `build-android` /
# `build-ios` script.
bindgen-kotlin:
	@HOST_LIB_EXT=$$(case "$$(uname -s)" in Linux*) echo .so ;; Darwin*) echo .dylib ;; *) echo .so ;; esac); \
	HOST_LIB_PATH="$(WORKSPACE_ROOT)/target/release/libffi$$HOST_LIB_EXT"; \
	if [ ! -f "$$HOST_LIB_PATH" ]; then \
	  echo "error: $$HOST_LIB_PATH does not exist. Run 'cargo build --release -p ffi' first."; \
	  exit 1; \
	fi; \
	mkdir -p "$(CRATE_DIR)/android"; \
	cargo run --release -p uniffi-bindgen -- generate \
	  --library "$$HOST_LIB_PATH" \
	  --language kotlin \
	  --out-dir "$(CRATE_DIR)/android" \
	  --config "$(CRATE_DIR)/uniffi.toml" \
	  --no-format

bindgen-swift:
	@HOST_LIB_EXT=$$(case "$$(uname -s)" in Linux*) echo .so ;; Darwin*) echo .dylib ;; *) echo .so ;; esac); \
	HOST_LIB_PATH="$(WORKSPACE_ROOT)/target/release/libffi$$HOST_LIB_EXT"; \
	if [ ! -f "$$HOST_LIB_PATH" ]; then \
	  echo "error: $$HOST_LIB_PATH does not exist. Run 'cargo build --release -p ffi' first."; \
	  exit 1; \
	fi; \
	mkdir -p "$(CRATE_DIR)/ios-bindings"; \
	cargo run --release -p uniffi-bindgen -- generate \
	  --library "$$HOST_LIB_PATH" \
	  --language swift \
	  --out-dir "$(CRATE_DIR)/ios-bindings" \
	  --config "$(CRATE_DIR)/uniffi.toml" \
	  --no-format

clean:
	rm -rf "$(CRATE_DIR)/ios" "$(CRATE_DIR)/ios-bindings" "$(CRATE_DIR)/android"
