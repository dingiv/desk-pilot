#!/usr/bin/env bash
# build_deb.sh — Package the fcitx5 swift-ime addon as a .deb.
#
# The single entry point for Debian packaging. Expects the CMake build tree
# to be configured AND built (scripts/build_fcitx.sh steps 2-3); this script
# stages the cmake install output into a deb root and runs dpkg-deb.
#
# Usage:
#   release/debian/build_deb.sh --build-dir <dir> [--version V] [--revision R]
#
#   --build-dir <dir>  CMake build tree (must be configured; install rules
#                      define the package layout, see CMakeLists.txt)
#   --version V        Package version (default: taken from control.in)
#   --revision R       Debian revision (default: 1; produced name is
#                      fcitx5-swift-ime_<version>-<revision>_amd64.deb)
#
# The binary control file is generated from control.in in this directory
# (single source of truth for package metadata); installed-file md5sums are
# generated automatically. Nothing else in the repo is touched.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"   # release/debian/
PROJECT_DIR="$(cd "$SCRIPT_DIR/../.." && pwd)"               # apps/swift-ime/
PKG="fcitx5-swift-ime"

BUILD_DIR=""
VERSION=""    # default: parsed from control.in below
REVISION="1"

while [[ $# -gt 0 ]]; do
    case "$1" in
        --build-dir) BUILD_DIR="$2"; shift 2 ;;
        --version)   VERSION="$2"; shift 2 ;;
        --revision)  REVISION="$2"; shift 2 ;;
        -h|--help)   grep '^#' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) echo "build_deb: unknown argument: $1" >&2; exit 2 ;;
    esac
done

[[ -n "$BUILD_DIR" ]] || { echo "build_deb: --build-dir is required" >&2; exit 2; }
[[ -d "$BUILD_DIR" ]] || { echo "build_deb: build dir not found: $BUILD_DIR" >&2; exit 2; }

# Default version = the Version field in control.in (strip a -revision suffix
# if the template ever carries one).
if [[ -z "$VERSION" ]]; then
    VERSION=$(awk '/^Version:/ {print $2; exit}' "$SCRIPT_DIR/control.in")
    VERSION="${VERSION%%-*}"
fi
[[ -n "$VERSION" ]] || { echo "build_deb: cannot determine version" >&2; exit 2; }

if ! command -v dpkg-deb &>/dev/null; then
    echo "build_deb: ⚠  dpkg-deb not found (apt install dpkg-dev)" >&2
    echo "build_deb: ⚠  the .so files are ready in $BUILD_DIR" >&2
    exit 0
fi

STAGING="$BUILD_DIR/deb-staging"
rm -rf "$STAGING"
mkdir -p "$STAGING/DEBIAN"

# 1. Install cmake outputs into the staging root (usr/... layout comes from
#    the install rules — same tree `cmake --install` would produce).
DESTDIR="$STAGING" cmake --install "$BUILD_DIR" >/dev/null

# 2. Binary control = template + version substitution.
sed -e "s/^Version: .*/Version: $VERSION-$REVISION/" \
    "$SCRIPT_DIR/control.in" > "$STAGING/DEBIAN/control"

# 3. md5sums over every installed file.
(cd "$STAGING" && find usr -type f -exec md5sum {} \;) > "$STAGING/DEBIAN/md5sums"

# 4. Build the package.
DEB_FILE="$PROJECT_DIR/build/${PKG}_${VERSION}-${REVISION}_amd64.deb"
mkdir -p "$(dirname "$DEB_FILE")"
dpkg-deb --build --root-owner-group "$STAGING" "$DEB_FILE"
rm -rf "$STAGING"

echo "$DEB_FILE"
