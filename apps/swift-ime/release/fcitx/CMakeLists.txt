# Compile the C++ thin glue → libswift-ime.so (fcitx5 addon).

set(SOURCES engine.cpp)

add_library(swift-ime SHARED ${SOURCES})
target_link_libraries(swift-ime PRIVATE
    Fcitx5::Core
    "${RUST_BUILD_DIR}/libswift_ime.so"
)

# fcitx5 plugins must NOT have a "lib" prefix.
set_target_properties(swift-ime PROPERTIES PREFIX "")

# Install to fcitx5's addon directory.
install(TARGETS swift-ime
        DESTINATION "${FCITX_INSTALL_LIBDIR}/fcitx5")
