# Compile the C++ thin glue → libswift-ime.so (fcitx5 addon).
#
# fcitx5 resolves the addon as `{Library}.so` verbatim (no probing of a
# "lib"-prefixed variant) — see fcitx5's own addons: file libkimpanel.so,
# conf Library=libkimpanel. The conf.in must therefore write the full
# file name including the "lib" prefix.

set(SOURCES swift-ime.cpp)

add_library(swift-ime SHARED ${SOURCES})
target_link_libraries(swift-ime PRIVATE
    Fcitx5::Core
    "${RUST_BUILD_DIR}/libswift-ime-core.so"
)

# Install to fcitx5's addon directory.
install(TARGETS swift-ime
        DESTINATION "${FCITX_INSTALL_LIBDIR}/fcitx5")
