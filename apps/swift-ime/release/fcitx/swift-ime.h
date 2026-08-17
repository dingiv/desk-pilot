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
#include <unordered_set>

// ── Opaque Rust handle ──────────────────────────────────────────────────────

struct ImeHandle; // defined in libswift-ime-core.so, created/destroyed via C ABI

// ── ImeView: cross-platform UI state snapshot (must match Rust #[repr(C)]) ─

static const unsigned int CANDIDATE_SLOTS = 16;

struct CandidateSlot {
    char text[128];
    char label[8];
    char meta[32];  // debug mode: "[score family/source]" — rendered as comment
};

struct ImeView {
    char           commit_text[512];
    uint32_t       commit_cursor;      // caret offset in commit_text ($CURSOR marker; else = len)
    char           preedit_text[512];  // expanded magic anchor (e.g. "🎙 #asr <voice>")
    uint32_t       preedit_cursor;
    CandidateSlot  candidates[CANDIDATE_SLOTS];
    uint32_t       candidate_count;
    uint32_t       candidate_highlight;
    uint32_t       candidate_page;
    uint32_t       candidate_page_size;
    char           aux_up[512];
    uint32_t       action;             // action bitflags — see SWIFT_ACTION_* below
};

// ── Action bitflags (mirror of Rust platform::action) ─────────────────────
// The engine owns ALL key policy; C++ reacts to these bits only.
#define SWIFT_ACTION_NONE        0u
#define SWIFT_ACTION_HANDLED     (1u << 0)  // key consumed → filterAndAccept
#define SWIFT_ACTION_PASSTHROUGH (1u << 1)  // key must reach the application
#define SWIFT_ACTION_COMMIT      (1u << 2)  // commit_text produced this round

// ── SwiftKeyPacket: faithful key forwarding (must match Rust CKeyEvent) ────
// The C++ side does NOT intercept or translate keys — it packs sym + unicode +
// modifier states and lets the engine's input router decide.

struct SwiftKeyPacket {
    uint32_t sym;      // X keysym (FcitxKey_*; ASCII printable == unicode)
    uint32_t unicode;  // keySymToUnicode(sym), 0 when unmapped
    uint8_t  ctrl;
    uint8_t  shift;
    uint8_t  alt;
};

// ── C ABI — every function takes the ImeHandle* as first argument ──────────

extern "C" {
    ImeHandle *swift_ime_create(const char *config_path);
    void       swift_ime_destroy(ImeHandle *handle);

    /// Unified key entry: EVERY key (special keys and Ctrl/Shift/Alt states
    /// included) is forwarded faithfully; the returned ImeView::action tells
    /// the caller how to react (HANDLED unset → do NOT filterAndAccept).
    int  swift_ime_key(ImeHandle *handle, void *ctx,
                       const SwiftKeyPacket *ev, ImeView *out_view);
    int  swift_ime_select_candidate(ImeHandle *handle, void *ctx,
                                    unsigned int index, ImeView *out_view);
    int  swift_ime_commit_pending(ImeHandle *handle, void *ctx,
                                  ImeView *out_view);
    int  swift_ime_poll_async(ImeHandle *handle, void *ctx,
                              ImeView *out_view);
    /// Magic live-command async refresh (`#asr` voice anchor, `#req` HTTP request, …):
    /// returns 1 + fills out_view when the active member's async state advanced and the ctx
    /// is in Magic mode. Polled by the C++ TimeEvent so candidates update live.
    int  swift_ime_magic_tick(ImeHandle *handle, void *ctx,
                              ImeView *out_view);
    /// Reconfigure the `#req` backend base URL at runtime (default http://127.0.0.1:14555/api).
    int  swift_ime_set_req_base(ImeHandle *handle, const char *base);
    /// Push the current clipboard text — `$CLIPBOARD` snippet templates resolve to it.
    int  swift_ime_set_clipboard(ImeHandle *handle, const char *text);
    void swift_ime_reset(ImeHandle *handle, void *ctx);
    void swift_ime_activate(ImeHandle *handle, void *ctx);
    void swift_ime_deactivate(ImeHandle *handle, void *ctx);
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

    /// Active input contexts (added on activate, removed on deactivate/reset). The magic poll
    /// timer iterates these to refresh live-command candidates (`#asr`/`#req`) async.
    std::unordered_set<fcitx::InputContext *> activeContexts_;
    std::unique_ptr<fcitx::EventSourceTime> magicTimer_;
    void startMagicPoll();
};

// ── Candidate word ──────────────────────────────────────────────────────────

class SwiftCandidateWord : public fcitx::CandidateWord {
public:
    SwiftCandidateWord(const std::string &text, const std::string &meta,
                       int index, SwiftImeEngine *engine);
    void select(fcitx::InputContext *inputContext) const override;
private:
    int index_;
    SwiftImeEngine *engine_;
};

// ── Factory ─────────────────────────────────────────────────────────────────

class SwiftImeFactory : public fcitx::AddonFactory {
    fcitx::AddonInstance *create(fcitx::AddonManager *manager) override;
};
