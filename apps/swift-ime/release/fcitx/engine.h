// swift-ime fcitx5 addon — engine header
//
// One opaque `ImeHandle` (Rust) per SwiftImeEngine instance.
// The handle owns the Dispatcher + per-context StateMachine map.
// The C++ side owns per-context lastView_ for diffing — no global state anywhere.

#pragma once

#include <fcitx/inputmethodengine.h>
#include <fcitx/addonfactory.h>
#include <fcitx/instance.h>
#include <fcitx/candidatelist.h>
#include <fcitx-utils/event.h>
#include <stdint.h>
#include <memory>
#include <string>
#include <unordered_map>

// ── Opaque Rust handle ──────────────────────────────────────────────────────

struct ImeHandle; // defined in libswift_ime.so, created/destroyed via C ABI

// ── ImeView: cross-platform UI state snapshot (must match Rust #[repr(C)]) ─

static const unsigned int CANDIDATE_SLOTS = 16;

struct CandidateSlot {
    char text[64];
    char label[8];
};

struct ImeView {
    char           commit_text[512];
    char           preedit_text[256];
    uint32_t       preedit_cursor;
    CandidateSlot  candidates[CANDIDATE_SLOTS];
    uint32_t       candidate_count;
    uint32_t       candidate_highlight;
    uint32_t       candidate_page;
    uint32_t       candidate_page_size;
    char           aux_up[256];
    uint8_t        key_passthrough;
};

// ── C ABI — every function takes the ImeHandle* as first argument ──────────

extern "C" {
    ImeHandle *swift_ime_create(const char *config_path);
    void       swift_ime_destroy(ImeHandle *handle);

    int  swift_ime_process_key(ImeHandle *handle, void *ctx,
                               unsigned int ch, ImeView *out_view);
    int  swift_ime_select_candidate(ImeHandle *handle, void *ctx,
                                    unsigned int index, ImeView *out_view);
    int  swift_ime_commit_pending(ImeHandle *handle, void *ctx,
                                  ImeView *out_view);
    int  swift_ime_poll_async(ImeHandle *handle, void *ctx,
                              ImeView *out_view);
    void swift_ime_reset(ImeHandle *handle, void *ctx);
    void swift_ime_activate(ImeHandle *handle, void *ctx);
    void swift_ime_deactivate(ImeHandle *handle, void *ctx);
    void swift_ime_set_surrounding(ImeHandle *handle, void *ctx,
                                   const char *text);
}

// ── fcitx5 engine ───────────────────────────────────────────────────────────

class SwiftImeEngine : public fcitx::InputMethodEngineV2 {
    friend class SwiftCandidateWord;
public:
    explicit SwiftImeEngine(fcitx::Instance *instance);
    ~SwiftImeEngine() override;

    // InputMethodEngineV2 interface
    void keyEvent(const fcitx::InputMethodEntry &entry,
                  fcitx::KeyEvent &keyEvent) override;
    void activate(const fcitx::InputMethodEntry &entry,
                  fcitx::InputContextEvent &event) override;
    void deactivate(const fcitx::InputMethodEntry &entry,
                    fcitx::InputContextEvent &event) override;
    void reset(const fcitx::InputMethodEntry &entry,
               fcitx::InputContextEvent &event) override;

    /// Diff new_view against the per-context last view and apply changes.
    void apply_view(fcitx::InputContext *ic, const ImeView &v);

private:
    ImeHandle      *handle_;
    fcitx::KeyList  selectionKeys_;
    fcitx::Instance *instance_;

    /// Per-context last view for diffing.
    std::unordered_map<fcitx::InputContext *, ImeView> lastViews_;

    std::unique_ptr<fcitx::EventSourceTime> pollTimer_;
    void startAsyncPoll();
};

// ── Candidate word ──────────────────────────────────────────────────────────

class SwiftCandidateWord : public fcitx::CandidateWord {
public:
    SwiftCandidateWord(const std::string &text, int index,
                       SwiftImeEngine *engine);
    void select(fcitx::InputContext *inputContext) const override;
private:
    int index_;
    SwiftImeEngine *engine_;
};

// ── Factory ─────────────────────────────────────────────────────────────────

class SwiftImeFactory : public fcitx::AddonFactory {
    fcitx::AddonInstance *create(fcitx::AddonManager *manager) override;
};
