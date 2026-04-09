#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(dirname "$SCRIPT_DIR")"
RUST_DIR="$PROJECT_ROOT/rust"
BUILD_DIR="$PROJECT_ROOT/build"

echo "Building sakuin native library..."

# Detect platform
case "$(uname -s)" in
Linux) LIB_NAME="libsakuin.so" ;;
Darwin) LIB_NAME="libsakuin.dylib" ;;
MINGW* | MSYS* | CYGWIN*) LIB_NAME="sakuin.dll" ;;
*)
	echo "Unsupported OS: $(uname -s)"
	exit 1
	;;
esac

# Build release (library + CLI binary)
cargo build --manifest-path "$RUST_DIR/Cargo.toml" --release --lib --bin sakuin-cli

# Copy to build/
mkdir -p "$BUILD_DIR"
cp "$RUST_DIR/target/release/$LIB_NAME" "$BUILD_DIR/"
cp "$RUST_DIR/target/release/sakuin-cli" "$BUILD_DIR/"

# On macOS, ad-hoc re-sign so the kernel accepts the new binary.
# Without this, replacing a previously-loaded dylib triggers a code-signing
# mtime mismatch and macOS sends SIGKILL on the next dlopen.
if [ "$(uname -s)" = "Darwin" ]; then
	codesign -s - -f "$BUILD_DIR/$LIB_NAME" 2>/dev/null || true
fi

echo "Built $BUILD_DIR/$LIB_NAME"
ls -lh "$BUILD_DIR/$LIB_NAME"
echo "Built $BUILD_DIR/sakuin-cli"
ls -lh "$BUILD_DIR/sakuin-cli"
