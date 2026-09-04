# swift-ime — DeskPilot input method engine for fcitx5
#
# Builds the fcitx5 addon (.so) by:
#   1. Compiling the C++ thin glue (release/fcitx/swift-ime.cpp)
#   2. Linking against Fcitx5::Core + libswift-ime-core.so (pre-built by cargo)
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
    CACHE PATH "Directory containing libswift-ime-core.so")

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
install(FILES "${RUST_BUILD_DIR}/libswift-ime-core.so"
        DESTINATION "${CMAKE_INSTALL_LIBDIR}")

# ── Install dictionaries ─────────────────────────────────────────────────────
# rime-ice.fst.idx is NOT packaged: it's a per-user cache generated at runtime from the .fst
# into ~/.desk-pilot (the system /usr/share/swift-ime/dict is read-only, so shipping it would
# never be written anyway). See LatticeDecoder::cache_path.
set(SWIFT_IME_DICT_DIR "${CMAKE_INSTALL_DATADIR}/swift-ime/dict")
install(FILES assets/dict/base.tsv
        DESTINATION "${SWIFT_IME_DICT_DIR}")
install(FILES assets/dict/rime/rime-ice.fst
        DESTINATION "${SWIFT_IME_DICT_DIR}/rime")
install(FILES assets/dict/emoji/emoji.tsv
        DESTINATION "${SWIFT_IME_DICT_DIR}/emoji")

# ── Install default config ────────────────────────────────────────────────────
install(FILES swift-ime.yaml
        DESTINATION "${CMAKE_INSTALL_DATADIR}/swift-ime")

# ── Install SNIP snippet markdown 模板(FileLoader::SNIP prod 命名空间种子) ──
# demo.md 装到 /usr/share/swift-ime/demo.md,首次运行 seed_snippets_dir 播种。
install(FILES assets/snippets/demo.md
        DESTINATION "${CMAKE_INSTALL_DATADIR}/swift-ime")
