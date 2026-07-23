// swift-ime fcitx5 addon — engine implementation
//
// Thin C++ glue: each fcitx5 callback calls the Rust C ABI → executes the action.
// Verified against fcitx5 5.1.14 API.

#include "engine.h"

#include <fcitx/inputcontext.h>
#include <fcitx/inputpanel.h>
#include <fcitx/candidatelist.h>
#include <fcitx/userinterfacemanager.h>
#include <fcitx-utils/key.h>
#include <memory>

// ── Candidate word — one entry in the pinyin candidate window ────────────
// (defined early so keyEvent can append it to the candidate list)

class SwiftCandidateWord : public fcitx::CandidateWord {
public:
    SwiftCandidateWord(const std::string &text, int index)
        : fcitx::CandidateWord(fcitx::Text(text)), index_(index) {}

    void select(fcitx::InputContext *inputContext) const override {
        char out[256] = {0};
        unsigned int len = 0;
        swift_ime_select_candidate(index_, out, sizeof(out), &len);
        inputContext->commitString(std::string(out, len));
    }

private:
    int index_;
};

// ── Lifecycle ────────────────────────────────────────────────────────────

void SwiftImeEngine::activate(const fcitx::InputMethodEntry &entry,
                               fcitx::InputContextEvent &event) {
    FCITX_UNUSED(entry);
    FCITX_UNUSED(event);
    swift_ime_activate();
}

void SwiftImeEngine::deactivate(const fcitx::InputMethodEntry &entry,
                                 fcitx::InputContextEvent &event) {
    FCITX_UNUSED(entry);
    FCITX_UNUSED(event);
    swift_ime_deactivate();
}

void SwiftImeEngine::reset(const fcitx::InputMethodEntry &entry,
                            fcitx::InputContextEvent &event) {
    FCITX_UNUSED(entry);
    FCITX_UNUSED(event);
    swift_ime_reset();
}

// ── Key event (the only required method) ─────────────────────────────────

void SwiftImeEngine::keyEvent(const fcitx::InputMethodEntry &entry,
                               fcitx::KeyEvent &keyEvent) {
    FCITX_UNUSED(entry);

    if (keyEvent.isRelease()) return;

    // Key symbol → Unicode scalar value.
    auto sym = keyEvent.key().sym();
    uint32_t ch = fcitx::Key::keySymToUnicode(sym);
    if (ch == 0) return; // non-printable key

    // ── Call the Rust engine ──
    char out_text[4096] = {0};
    unsigned int out_len = 0;
    int action = swift_ime_process_key(
        ch, out_text, sizeof(out_text), &out_len);

    auto *ic = keyEvent.inputContext();
    if (!ic) return;

    // ── Execute the returned action ──
    switch (action) {
    case 0: // PassThrough — key goes to the application.
        break;

    case 1: { // Preedit — show composing text inline.
        keyEvent.filterAndAccept();
        ic->inputPanel().setClientPreedit(
            fcitx::Text(std::string(out_text, out_len)));
        ic->updatePreedit();
        break;
    }

    case 2: { // Commit — final text replaces any preedit.
        keyEvent.filterAndAccept();
        ic->commitString(std::string(out_text, out_len));
        break;
    }

    case 3: { // Candidates — build the fcitx5 LookupTable from the pinyin engine.
        keyEvent.filterAndAccept();
        // Show the in-progress pinyin as preedit (out_text holds the buffer).
        ic->inputPanel().setClientPreedit(
            fcitx::Text(std::string(out_text, out_len)));
        // Fetch candidate list from Rust.
        SwiftImeCandidateFFI items[SWIFT_IME_MAX_CANDIDATES];
        unsigned int n = swift_ime_candidates(items, SWIFT_IME_MAX_CANDIDATES);
        if (n > 0) {
            auto list = std::make_unique<fcitx::CommonCandidateList>();
            for (unsigned int i = 0; i < n; i++) {
                std::string text(items[i].text);
                list->append<SwiftCandidateWord>(text, (int)i);
            }
            ic->inputPanel().setCandidateList(std::move(list));
        }
        ic->updateUserInterface(fcitx::UserInterfaceComponent::InputPanel);
        break;
    }

    default:
        break;
    }
}

// ── Factory ──────────────────────────────────────────────────────────────

static bool initialized = false;

fcitx::AddonInstance *SwiftImeFactory::create(fcitx::AddonManager *manager) {
    FCITX_UNUSED(manager);
    if (!initialized) {
        swift_ime_init(nullptr);  // nullptr = use built-in snippets
        initialized = true;
    }
    return new SwiftImeEngine;
}

FCITX_ADDON_FACTORY(SwiftImeFactory);
