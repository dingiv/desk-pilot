// swift-ime fcitx5 addon — engine implementation
//
// Thin C++ glue: each fcitx5 callback → calls the Rust C ABI → executes the returned
// action via fcitx5 InputContext API (commitString, updatePreedit, updateUserInterface).

#include "engine.h"
#include <fcitx/inputcontext.h>
#include <fcitx/inputpanel.h>
#include <fcitx/candidatelist.h>
#include <fcitx/userinterfacemanager.h>

// ═══════════════════════════════════════════════════════════════════════
// SwiftImeEngine
// ═══════════════════════════════════════════════════════════════════════

SwiftImeEngine::SwiftImeEngine(fcitx::Instance *instance)
    : instance_(instance) {}

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

void SwiftImeEngine::keyEvent(const fcitx::InputMethodEntry &entry,
                               fcitx::KeyEvent &keyEvent) {
    FCITX_UNUSED(entry);

    // ── 1. Ignore key releases ──
    if (keyEvent.isRelease()) return;

    // ── 2. Get the Unicode char from fcitx5's key ──
    // FcitxKey::toSimpleUTF8() gives us the primary character.
    char utf8_buf[8] = {0};
    uint32_t ch = 0;
    if (auto s = keyEvent.key().toSimpleUTF8()) {
        // Take the first Unicode scalar value.
        const auto &str = *s;
        if (!str.empty()) {
            // FIXME: proper UTF-8 → codepoint conversion for multi-byte.
            ch = static_cast<unsigned char>(str[0]);
        }
    }
    if (ch == 0) return; // modifier-only key

    // ── 3. Call the Rust engine ──
    char out_text[4096] = {0};
    unsigned int out_len = 0;
    int action = swift_ime_process_key(
        ch, out_text, sizeof(out_text), &out_len);

    auto *ic = keyEvent.inputContext();
    if (!ic) return;

    // ── 4. Execute the returned action ──
    switch (action) {
    case 0: // PassThrough — let the key go to the application.
        break;

    case 1: { // Preedit
        keyEvent.filterAndAccept();
        ic->updatePreedit(out_text, out_len);
        break;
    }

    case 2: { // Commit
        keyEvent.filterAndAccept();
        ic->commitString(out_text);
        break;
    }

    case 3: // Candidates — Phase 3 (pinyin).
        // For Phase 1: snippets match uniquely, no candidates needed.
        break;

    default:
        break;
    }
}

// ═══════════════════════════════════════════════════════════════════════
// SwiftImeFactory
// ═══════════════════════════════════════════════════════════════════════

fcitx::AddonInstance *SwiftImeFactory::create(fcitx::AddonManager *manager) {
    FCITX_UNUSED(manager);
    return new SwiftImeEngine(manager->instance());
}

FCITX_ADDON_FACTORY(SwiftImeFactory);
