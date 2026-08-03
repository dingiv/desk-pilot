# swift-ime — DeskPilot input method engine for fcitx5
#
# Builds the fcitx5 addon (.so) by:
#   1. Compiling the C++ thin glue (release/fcitx/engine.cpp)
#   2. Linking against Fcitx5::Core + libswift_ime.so (pre-built by cargo)
#
# Build:
#   ./scripts/build_fcitx.sh [--release|--debug] [--no-deb] [--install]

cmake_minimum_required(VERSION 3.21)
project(swift-ime VERSION 0.1.0 LANGUAGES CXX)

# ── Find dependencies ─────────────────────────────────────────────────────
find_package(Fcitx5Core REQUIRED)
include("${FCITX_INSTALL_CMAKECONFIG_DIR}/Fcitx5Utils/Fcitx5CompilerSettings.cmake")

# Path to the Rust cdylib (pre-built by cargo).
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
include(GNUInstallDirs)
install(FILES "${RUST_BUILD_DIR}/libswift_ime.so"
        DESTINATION "${CMAKE_INSTALL_LIBDIR}")

# ── Install dictionaries ─────────────────────────────────────────────────────
set(SWIFT_IME_DICT_DIR "${CMAKE_INSTALL_DATADIR}/swift-ime/dict")
install(FILES assets/dict/base.tsv
        DESTINATION "${SWIFT_IME_DICT_DIR}")
install(FILES assets/dict/rime-ice.fst
        DESTINATION "${SWIFT_IME_DICT_DIR}")
install(FILES assets/dict/rime-ice.fst.idx
        DESTINATION "${SWIFT_IME_DICT_DIR}")

# ── Install default config ────────────────────────────────────────────────────
install(FILES swift-ime.yaml
        DESTINATION "${CMAKE_INSTALL_DATADIR}/swift-ime")
