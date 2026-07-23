// swift-ime fcitx5 addon — engine header
//
// Thin C++ glue between fcitx5's InputMethodEngineV2 and the Rust ime-core C ABI.
// API is verified against fcitx5 5.1.14 (libfcitx5core-dev 5.1.14-1).

#pragma once

#include <fcitx/inputmethodengine.h>
#include <fcitx/addonfactory.h>

// Rust C ABI return types — ImeActionFFI enum (0=PassThrough, 1=Preedit, 2=Commit, 3=Candidates).
// Defined in crates/ime-core-ffi/src/lib.rs. The enum is #[repr(C)] → int in C.
extern "C" {
    int  swift_ime_init(const char *config_path);
    int  swift_ime_process_key(unsigned int ch, char *out_text,
                               unsigned int out_cap, unsigned int *out_len);
    int  swift_ime_select_candidate(unsigned int index);
    int  swift_ime_candidates(void *out_items, unsigned int max_items);
    void swift_ime_activate(void);
    void swift_ime_deactivate(void);
    void swift_ime_reset(void);
}

/// fcitx5 engine addon — the ONLY class we need to write.
class SwiftImeEngine : public fcitx::InputMethodEngineV2 {
public:
    // ── InputMethodEngineV2 interface ──
    void keyEvent(const fcitx::InputMethodEntry &entry,
                  fcitx::KeyEvent &keyEvent) override;
    void activate(const fcitx::InputMethodEntry &entry,
                  fcitx::InputContextEvent &event) override;
    void deactivate(const fcitx::InputMethodEntry &entry,
                    fcitx::InputContextEvent &event) override;
    void reset(const fcitx::InputMethodEntry &entry,
               fcitx::InputContextEvent &event) override;
};

/// Factory registered via FCITX_ADDON_FACTORY macro.
class SwiftImeFactory : public fcitx::AddonFactory {
    fcitx::AddonInstance *create(fcitx::AddonManager *manager) override;
};
