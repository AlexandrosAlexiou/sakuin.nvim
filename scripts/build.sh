#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(dirname "$SCRIPT_DIR")"
RUST_DIR="$PROJECT_ROOT/rust"
BUILD_DIR="$PROJECT_ROOT/build"

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

TARGET="${1:-all}"

# Single source of truth for the version (matches the name Neovim loads).
read_version() {
	sed -n 's/^M\.version = "\(.*\)"$/\1/p' "$PROJECT_ROOT/lua/sakuin/binary.lua" | head -n1
}

build_lib() {
	echo "Building sakuin native library..."
	cargo build --manifest-path "$RUST_DIR/Cargo.toml" --release --lib

	local version
	version="$(read_version)"
	if [ -z "$version" ]; then
		echo "Could not read M.version from lua/sakuin/binary.lua" >&2
		exit 1
	fi
	# libsakuin.dylib -> libsakuin_<version>.dylib
	local versioned="${LIB_NAME%.*}_${version}.${LIB_NAME##*.}"

	mkdir -p "$BUILD_DIR"
	cp "$RUST_DIR/target/release/$LIB_NAME" "$BUILD_DIR/$versioned"

	# On macOS, ad-hoc re-sign so the kernel accepts the new binary.
	# Without this, replacing a previously-loaded dylib triggers a code-signing
	# mtime mismatch and macOS sends SIGKILL on the next dlopen.
	if [ "$(uname -s)" = "Darwin" ]; then
		codesign -s - -f "$BUILD_DIR/$versioned" 2>/dev/null || true
	fi

	echo "Built $BUILD_DIR/$versioned"
	ls -lh "$BUILD_DIR/$versioned"
}

build_cli() {
	echo "Building sakuin-cli..."
	cargo build --manifest-path "$RUST_DIR/Cargo.toml" --release --bin sakuin-cli

	mkdir -p "$BUILD_DIR"
	cp "$RUST_DIR/target/release/sakuin-cli" "$BUILD_DIR/"

	echo "Built $BUILD_DIR/sakuin-cli"
	ls -lh "$BUILD_DIR/sakuin-cli"
}

case "$TARGET" in
lib) build_lib ;;
cli) build_cli ;;
all)
	build_lib
	build_cli
	;;
*)
	echo "Usage: $0 [lib|cli|all]"
	exit 1
	;;
esac
