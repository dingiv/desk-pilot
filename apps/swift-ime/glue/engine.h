// swift-ime fcitx5 addon — engine header
//
// Thin C++ glue between fcitx5's InputMethodEngineV2 and the Rust ime-core C ABI.
// This is the ONLY C++ code in the project (~80 lines of implementation).

#pragma once

#include <fcitx/inputmethodengine.h>
#include <fcitx/addonfactory.h>

// C ABI — these are defined in the Rust crate and linked via cargo-c.
// (Generated header from cbindgen — hand-written for now, ~6 functions.)
extern "C" {
    int  swift_ime_init(const char *config_path);
    int  swift_ime_process_key(unsigned int ch, char *out_text,
                               unsigned int out_cap, unsigned int *out_len);
    int  swift_ime_select_candidate(unsigned int index);
    int  swift_ime_candidates(void *out_items, unsigned int max_items);
    void swift_ime_activate();
    void swift_ime_deactivate();
    void swift_ime_reset();
}

// fcitx5 engine addon class.
class SwiftImeEngine : public fcitx::InputMethodEngineV2 {
public:
    SwiftImeEngine(fcitx::Instance *instance);

    // ── InputMethodEngineV2 interface ──
    void keyEvent(const fcitx::InputMethodEntry &entry,
                  fcitx::KeyEvent &keyEvent) override;
    void activate(const fcitx::InputMethodEntry &entry,
                  fcitx::InputContextEvent &event) override;
    void deactivate(const fcitx::InputMethodEntry &entry,
                    fcitx::InputContextEvent &event) override;
    void reset(const fcitx::InputMethodEntry &entry,
               fcitx::InputContextEvent &event) override;

private:
    fcitx::Instance *instance_;
};

// Factory — registered with the FCITX_ADDON_FACTORY macro.
class SwiftImeFactory : public fcitx::AddonFactory {
    fcitx::AddonInstance *create(fcitx::AddonManager *manager) override;
};
