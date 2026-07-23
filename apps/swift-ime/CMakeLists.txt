# swift-ime — DeskPilot input method engine for fcitx5
#
# Builds libswift-ime.so (the fcitx5 addon) by:
#   1. Compiling the C++ thin glue (glue/engine.cpp) → object
#   2. Linking against Fcitx5::Core + our Rust libime_core.so (pre-built via cargo)
#
# Prerequisites:
#   sudo apt install fcitx5-modules-dev libfcitx5core-dev cmake cargo
#   cargo build -p ime-core-ffi --release
#
# Build:
#   mkdir -p build && cd build
#   cmake .. -DCMAKE_INSTALL_PREFIX=/usr -DCMAKE_BUILD_TYPE=Release
#   make -j$(nproc)
#   sudo make install
#
# Debian package:
#   dpkg-buildpackage -us -uc -b

cmake_minimum_required(VERSION 3.21)
project(swift-ime VERSION 0.1.0 LANGUAGES CXX)

# ── Find dependencies ─────────────────────────────────────────────────────
find_package(Fcitx5Core REQUIRED)
include("${FCITX_INSTALL_CMAKECONFIG_DIR}/Fcitx5Utils/Fcitx5CompilerSettings.cmake")

# Path to the Rust shared library (pre-built by cargo).
# In dev: target/release/libime_core_ffi.so
# In deb: /usr/lib/x86_64-linux-gnu/libime_core_ffi.so
set(RUST_BUILD_DIR "${CMAKE_SOURCE_DIR}/../../target/release"
    CACHE PATH "Directory containing libswift_ime.so")

# ── Build the fcitx5 addon shared library ─────────────────────────────────
add_subdirectory(release/fcitx)

# ── Install registration files ────────────────────────────────────────────
# Addon conf
configure_file(
    "${CMAKE_SOURCE_DIR}/release/fcitx/fcitx5-swift-ime.addon.conf.in"
    "${CMAKE_CURRENT_BINARY_DIR}/fcitx5-swift-ime.conf"
)
install(FILES "${CMAKE_CURRENT_BINARY_DIR}/fcitx5-swift-ime.conf"
        DESTINATION "${FCITX_INSTALL_PKGDATADIR}/addon"
        RENAME swift-ime.conf)

# Input method registration
install(FILES "${CMAKE_SOURCE_DIR}/release/fcitx/swift-ime.conf"
        DESTINATION "${FCITX_INSTALL_PKGDATADIR}/inputmethod")

# Install the Rust shared library alongside the addon.
# Use GNUInstallDirs for standard paths (not affected by fcitx5's cmake module).
include(GNUInstallDirs)
install(FILES "${RUST_BUILD_DIR}/libswift_ime.so"
        DESTINATION "${CMAKE_INSTALL_LIBDIR}")
