// swift-ime fcitx5 addon — engine implementation
//
// Thin C++ glue: each fcitx5 callback calls the Rust C ABI → executes the action.
// Verified against fcitx5 5.1.14 API.

#include "engine.h"

#include <fcitx/inputcontext.h>
#include <fcitx/inputpanel.h>
#include <fcitx/candidatelist.h>
#include <fcitx-utils/key.h>

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

    case 3: // Candidates — Phase 3 (pinyin engine).
        break;

    default:
        break;
    }
}

// ── Factory ──────────────────────────────────────────────────────────────

fcitx::AddonInstance *SwiftImeFactory::create(fcitx::AddonManager *manager) {
    FCITX_UNUSED(manager);
    return new SwiftImeEngine;
}

FCITX_ADDON_FACTORY(SwiftImeFactory);
