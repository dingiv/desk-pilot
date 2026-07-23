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

# ── Resolve the cargo target directory (workspace-aware) ───────────────────
# Works from any subdirectory — cargo auto-discovers the workspace root.
WS_ROOT="$(dirname "$(cargo locate-project --workspace 2>/dev/null \
    | python3 -c 'import sys,json; print(json.load(sys.stdin)["root"])' 2>/dev/null)")"
TARGET_DIR="${WS_ROOT:-$PROJECT_DIR/../..}/target"

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

# ── Step 1: Build the Rust cdylib (swift-ime library target) ───────────────
echo "── [1/4] Building Rust cdylib (swift-ime lib) …"
cargo build -p swift-ime --lib $CARGO_FLAGS
echo "   → $RUST_BUILD_DIR/libswift_ime.so"
echo ""

# ── Step 2: CMake configure ───────────────────────────────────────────────
echo "── [2/4] Configuring CMake …"
BUILD_DIR="$PROJECT_DIR/build"
rm -rf "$BUILD_DIR"
mkdir -p "$BUILD_DIR"
cmake -S "$PROJECT_DIR" -B "$BUILD_DIR" \
    -DCMAKE_INSTALL_PREFIX=/usr \
    -DCMAKE_BUILD_TYPE="$BUILD_TYPE" \
    -DRUST_BUILD_DIR="$RUST_BUILD_DIR"
echo ""

# ── Step 3: Build the C++ addon ───────────────────────────────────────────
echo "── [3/4] Building fcitx5 addon (swift-ime.so) …"
cmake --build "$BUILD_DIR" -j"$(nproc)"
echo "   → $BUILD_DIR/release/fcitx/swift-ime.so"
echo ""

# ── Step 4 (optional): Install ────────────────────────────────────────────
if [ "$DO_INSTALL" = true ]; then
    echo "── [install] Installing to /usr …"
    cmake --install "$BUILD_DIR"
    echo "   done. Run 'fcitx5 -rd' to reload."
    echo ""
fi

# ── Step 5 (optional): Debian package ─────────────────────────────────────
if [ "$DO_DEB" = true ]; then
    echo "── [5/5] Building .deb package …"

    if ! command -v dpkg-buildpackage &>/dev/null; then
        echo "   ⚠  dpkg-buildpackage not found (apt install dpkg-dev)"
        echo "   The .so is ready: $BUILD_DIR/release/fcitx/swift-ime.so"
        exit 0
    fi

    cd "$PROJECT_DIR"
    # Assemble .deb from cmake install output (avoids dpkg-buildpackage dep resolution).
    STAGING="$BUILD_DIR/deb-staging"
    rm -rf "$STAGING"
    mkdir -p "$STAGING/DEBIAN"

    # Install cmake outputs into staging
    DESTDIR="$STAGING" cmake --install "$BUILD_DIR"

    # Use the binary-package control template (no source/Build-Depends fields needed)
    cp "$PROJECT_DIR/debian/control.in" "$STAGING/DEBIAN/control"

    # Generate md5sums
    (cd "$STAGING" && find usr -type f -exec md5sum {} \;) > "$STAGING/DEBIAN/md5sums"

    DEB_FILE="$PROJECT_DIR/build/fcitx5-swift-ime_0.1.0-1_amd64.deb"
    dpkg-deb --build --root-owner-group "$STAGING" "$DEB_FILE"
    rm -rf "$STAGING"

    if [ -f "$DEB_FILE" ]; then
        echo "   → $DEB_FILE"
    fi
    echo ""
fi

echo "═══════════════════════════════════════════════"
echo " Build complete."
echo ""
echo " Artifacts:"
echo "   $RUST_BUILD_DIR/libswift_ime.so"
echo "   $BUILD_DIR/release/fcitx/swift-ime.so"
if [ "${DEB_FILE:-}" != "" ] && [ -f "$DEB_FILE" ]; then
    echo "   $DEB_FILE"
fi
echo ""
echo " Install:  cd $PROJECT_DIR/build && sudo cmake --install ."
echo " Reload:   fcitx5 -rd"
echo "═══════════════════════════════════════════════"
