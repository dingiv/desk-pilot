// swift-ime fcitx5 addon — engine header
//
// Thin C++ glue between fcitx5's InputMethodEngineV2 and the Rust ime-core C ABI.
// API is verified against fcitx5 5.1.14 (libfcitx5core-dev 5.1.14-1).

#pragma once

#include <fcitx/inputmethodengine.h>
#include <fcitx/addonfactory.h>
#include <fcitx/instance.h>
#include <fcitx-utils/event.h>

// Rust C ABI. ImeActionFFI enum: 0=PassThrough, 1=Preedit, 2=Commit, 3=Candidates.
// First arg `ctx` is the fcitx5 InputContext pointer — per-window state isolation.
extern "C" {
    int  swift_ime_init(const char *config_path);
    int  swift_ime_process_key(void *ctx, unsigned int ch, char *out_text,
                               unsigned int out_cap, unsigned int *out_len);
    int  swift_ime_select_candidate(void *ctx, unsigned int index, char *out_text,
                                    unsigned int out_cap, unsigned int *out_len);
    unsigned int swift_ime_candidates(void *ctx, void *out_items, unsigned int max_items);
    void swift_ime_activate(void *ctx);
    void swift_ime_deactivate(void *ctx);
    void swift_ime_commit_pending(void *ctx, char *out_text,
                                   unsigned int out_cap, unsigned int *out_len);
    int  swift_ime_poll_async(void *ctx, uint8_t *out_text,
                               unsigned int out_cap, unsigned int *out_len);
    void swift_ime_reset(void *ctx);
}

// One candidate as returned by swift_ime_candidates — 64-byte NUL-terminated UTF-8 text.
struct SwiftImeCandidateFFI {
    char text[64];
};
static const unsigned int SWIFT_IME_MAX_CANDIDATES = 9;

/// fcitx5 engine addon — the ONLY class we need to write.
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
    /// Candidate selection keys (1-9) — set on the CommonCandidateList so
    /// labels appear and keyListIndex() matches digits.
    fcitx::KeyList selectionKeys_;

    /// fcitx5 instance — needed for event-loop timer registration.
    fcitx::Instance *instance_;

    /// Async poll timer handle (polls rust swift_ime_poll_async every ~100ms).
    std::unique_ptr<fcitx::EventSourceTime> pollTimer_;
    void startAsyncPoll();
};

/// Factory registered via FCITX_ADDON_FACTORY macro.
class SwiftImeFactory : public fcitx::AddonFactory {
    fcitx::AddonInstance *create(fcitx::AddonManager *manager) override;
};
