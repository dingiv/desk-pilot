#!/usr/bin/env bash
# build_fcitx.sh — Build the fcitx5 addon (.so) and package it as a .deb.
#
# Run from apps/swift-ime/ (this script's directory). All cargo workspace
# dependency resolution is handled automatically — no repo-root paths needed.
#
# Usage:
#   cd apps/swift-ime && ./scripts/build_fcitx.sh [--release|--debug] [--no-deb] [--install]
#
# Prerequisites:
#   sudo apt install build-essential cmake cargo rustc \
#     libfcitx5core-dev fcitx5-modules-dev dpkg-dev
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"   # apps/swift-ime/
cd "$PROJECT_DIR"                              # ensure cargo commands work

# ── Parse flags ────────────────────────────────────────────────────────────
BUILD_TYPE="Release"
DO_DEB=true
DO_INSTALL=false

for arg in "$@"; do
    case "$arg" in
        --release) BUILD_TYPE="Release" ;;
        --debug)   BUILD_TYPE="Debug"   ;;
        --no-deb)  DO_DEB=false         ;;
        --install) DO_INSTALL=true       ;;
        -h|--help)
            echo "Usage: $0 [--release|--debug] [--no-deb] [--install]"
            echo "  --release  Release build (default)"
            echo "  --debug    Debug build"
            echo "  --no-deb   Skip .deb packaging"
            echo "  --install  sudo make install after build"
            exit 0
            ;;
    esac
done

# Cargo always writes to <workspace_root>/target, never to a sub-crate's local dir.
# From apps/swift-ime/ that's ../../target. Respect $CARGO_TARGET_DIR if set.
TARGET_DIR="${CARGO_TARGET_DIR:-../../target}"

if [ "$BUILD_TYPE" = "Release" ]; then
    CARGO_FLAGS="--release"
    RUST_BUILD_DIR="$TARGET_DIR/release"
else
    CARGO_FLAGS=""
    RUST_BUILD_DIR="$TARGET_DIR/debug"
fi

echo "═══════════════════════════════════════════════"
echo " swift-ime fcitx5 build"
echo "   project:    $PROJECT_DIR"
echo "   build type: $BUILD_TYPE"
echo "   cargo dir:  $RUST_BUILD_DIR"
echo "   deb:        $DO_DEB"
echo "   install:    $DO_INSTALL"
echo "═══════════════════════════════════════════════"
echo ""

# ── Step 1: Build the Rust cdylib ──────────────────────────────────────
echo "── [1/4] Building Rust cdylib …"
cargo build -p swift-ime --lib $CARGO_FLAGS
# cargo rejects hyphens in lib target names, so the cdylib is emitted as
# libswift_ime.so — and cargo re-hardlinks that name into place on EVERY
# build (the real artifact lives in target/release/deps/). Copy (not mv)
# to the packaging name the CMake glue links against, so it survives
# cargo's next hard-link restore.
rm -f "$RUST_BUILD_DIR/libswift-ime-core.so"
cp -f "$RUST_BUILD_DIR/libswift_ime.so" "$RUST_BUILD_DIR/libswift-ime-core.so"
echo "   → $RUST_BUILD_DIR/libswift-ime-core.so"
echo ""

# ── Step 2: CMake configure ───────────────────────────────────────────────
echo "── [2/4] Configuring CMake …"
BUILD_DIR="$PROJECT_DIR/build"
rm -rf "$BUILD_DIR"
mkdir -p "$BUILD_DIR"
cmake -S "$PROJECT_DIR" -B "$BUILD_DIR" \
    -DCMAKE_INSTALL_PREFIX=/usr \
    -DCMAKE_BUILD_TYPE="$BUILD_TYPE" \
    -DCMAKE_SKIP_RPATH=ON \
    -DRUST_BUILD_DIR="$RUST_BUILD_DIR"
echo ""

# ── Step 3: Build the C++ addon ───────────────────────────────────────────
echo "── [3/4] Building fcitx5 addon (libswift-ime.so) …"
cmake --build "$BUILD_DIR" -j"$(nproc)"
echo "   → $BUILD_DIR/release/fcitx/libswift-ime.so"
echo ""

# ── Step 4 (optional): Install ────────────────────────────────────────────
if [ "$DO_INSTALL" = true ]; then
    echo "── [install] Installing to /usr …"
    cmake --install "$BUILD_DIR"
    echo "   done. Run 'fcitx5 -rd' to reload."
    echo ""
fi

# ── Step 4 (optional): Debian package ─────────────────────────────────────
# All packaging logic lives in release/debian/ (control template + staging
# script). This step only invokes it and reports the artifact path.
if [ "$DO_DEB" = true ]; then
    echo "── [4/4] Building .deb package (release/debian/build_deb.sh) …"
    DEB_FILE="$("$PROJECT_DIR/release/debian/build_deb.sh" --build-dir "$BUILD_DIR" | tail -n 1)"
    if [ -n "$DEB_FILE" ] && [ -f "$DEB_FILE" ]; then
        echo "   → $DEB_FILE"
    fi
    echo ""
fi

echo "═══════════════════════════════════════════════"
echo " Build complete."
echo ""
echo " Artifacts:"
echo "   $RUST_BUILD_DIR/libswift-ime-core.so"
echo "   $BUILD_DIR/release/fcitx/libswift-ime.so"
if [ "${DEB_FILE:-}" != "" ] && [ -f "$DEB_FILE" ]; then
    echo "   $DEB_FILE"
fi
echo ""
echo " Install:  cd $PROJECT_DIR/build && sudo cmake --install ."
echo " Reload:   fcitx5 -rd"
echo "═══════════════════════════════════════════════"
