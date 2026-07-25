// swift-ime fcitx5 addon — engine implementation

#include "engine.h"

#include <fcitx/inputcontext.h>
#include <fcitx/inputpanel.h>
#include <fcitx/candidatelist.h>
#include <fcitx/userinterfacemanager.h>
#include <fcitx/instance.h>
#include <fcitx-utils/key.h>
#include <fcitx-utils/event.h>
#include <memory>
#include <time.h>

// 全局单例锁
static bool initialized = false;

// ── Engine constructor ────────────────────────────────────────────────────

SwiftImeEngine::SwiftImeEngine(fcitx::Instance *instance)
    : instance_(instance) {
    fcitx::KeySym syms[] = {
        FcitxKey_1, FcitxKey_2, FcitxKey_3, FcitxKey_4, FcitxKey_5,
        FcitxKey_6, FcitxKey_7, FcitxKey_8, FcitxKey_9, FcitxKey_0};
    for (auto sym : syms) {
        selectionKeys_.emplace_back(sym, fcitx::KeyStates());
    }
}

// ── Async poll timer (for #wait / #asr background updates) ────────────────

void SwiftImeEngine::startAsyncPoll() {
    if (pollTimer_) return;
    pollTimer_ = instance_->eventLoop().addTimeEvent(
        CLOCK_MONOTONIC, 0, 100000,
        [this](fcitx::EventSourceTime *source, uint64_t) {
            // Try each known ctx in the Rust CONTEXTS map. For the demo,
            // we rely on swift_ime_poll_async returning 0 for idle contexts
            // and 1/2 for active ones. Walk through a small fixed set.
            // In production, the Rust side would expose an iterator.
            for (uintptr_t probe = 1; probe < 1024; probe++) {
                void *ctx = (void *)probe;
                char buf[256] = {0};
                unsigned int len = 0;
                int r = swift_ime_poll_async(ctx, (uint8_t *)buf, sizeof(buf), &len);
                if (r == 0) continue;
                // Validate: find the InputContext for this pointer.
                // fcitx5 doesn't expose a "get IC by pointer" API, so for the
                // demo we approximate: any raw context pointer in the Rust map
                // came from a real InputContext* in keyEvent. We cast back.
                auto *ic = reinterpret_cast<fcitx::InputContext *>(ctx);
                if (r == 2) {
                    // Sequence complete: commit and clear.
                    ic->inputPanel().reset();
                    ic->commitString(std::string(buf, len));
                    ic->updatePreedit();
                    ic->updateUserInterface(fcitx::UserInterfaceComponent::InputPanel);
                    pollTimer_.reset();
                    (void)source;
                    return false;
                }
                // r == 1: preedit update.
                auto text = fcitx::Text(std::string(buf, len));
                text.setCursor(len);
                ic->inputPanel().setClientPreedit(text);
                ic->inputPanel().setAuxUp(text);
                ic->updatePreedit();
                break; // one update per tick
            }
            return true; // keep polling
        });
}

// ── Candidate word — one entry in the pinyin candidate window ────────────

class SwiftCandidateWord : public fcitx::CandidateWord {
public:
    SwiftCandidateWord(const std::string &text, int index)
        : fcitx::CandidateWord(fcitx::Text(text)), index_(index) {}

    void select(fcitx::InputContext *inputContext) const override {
        char out[256] = {0};
        unsigned int len = 0;
        swift_ime_select_candidate((void *)inputContext, index_, out, sizeof(out), &len);
        inputContext->inputPanel().reset();
        inputContext->commitString(std::string(out, len));
        inputContext->updatePreedit();
        inputContext->updateUserInterface(fcitx::UserInterfaceComponent::InputPanel);
    }

private:
    int index_;
};

// ── Lifecycle ────────────────────────────────────────────────────────────

void SwiftImeEngine::activate(const fcitx::InputMethodEntry &entry,
                               fcitx::InputContextEvent &event) {
    FCITX_UNUSED(entry);
    swift_ime_activate((void *)event.inputContext());
}

void SwiftImeEngine::deactivate(const fcitx::InputMethodEntry &entry,
                                 fcitx::InputContextEvent &event) {
    FCITX_UNUSED(entry);
    auto *ic = event.inputContext();
    if (!ic) return;
    if (event.type() != fcitx::EventType::InputContextSwitchInputMethod) {
        return;
    }
    char out[4096] = {0};
    unsigned int len = 0;
    swift_ime_commit_pending((void *)ic, out, sizeof(out), &len);
    if (len > 0) {
        ic->commitString(std::string(out, len));
    }
    swift_ime_deactivate((void *)ic);
}

void SwiftImeEngine::reset(const fcitx::InputMethodEntry &entry,
                            fcitx::InputContextEvent &event) {
    FCITX_UNUSED(entry);
    swift_ime_reset((void *)event.inputContext());
}

// ── Key event (the only required method) ─────────────────────────────────

void SwiftImeEngine::keyEvent(const fcitx::InputMethodEntry &entry,
                               fcitx::KeyEvent &keyEvent) {
    FCITX_UNUSED(entry);
    if (keyEvent.isRelease()) return;

    auto *ic = keyEvent.inputContext();
    if (!ic) return;

    // ── Candidate-list navigation ──
    auto candList = ic->inputPanel().candidateList();
    if (candList && !candList->empty()) {
        if (auto maybeIdx = keyEvent.key().keyListIndex(selectionKeys_);
            maybeIdx >= 0 && maybeIdx < candList->size()) {
            candList->candidate(maybeIdx).select(ic);
            keyEvent.filterAndAccept();
            return;
        }
        if (auto *movable = candList->toCursorMovable()) {
            if (keyEvent.key().check(FcitxKey_Up)) {
                movable->prevCandidate();
                ic->updateUserInterface(fcitx::UserInterfaceComponent::InputPanel);
                keyEvent.filterAndAccept(); return;
            }
            if (keyEvent.key().check(FcitxKey_Down)) {
                movable->nextCandidate();
                ic->updateUserInterface(fcitx::UserInterfaceComponent::InputPanel);
                keyEvent.filterAndAccept(); return;
            }
        }
        if (auto *pageable = candList->toPageable()) {
            if (keyEvent.key().check(FcitxKey_minus)
                || keyEvent.key().check(FcitxKey_Page_Up)
                || keyEvent.key().check(FcitxKey_Left)) {
                if (pageable->hasPrev()) {
                    pageable->prev();
                    ic->updateUserInterface(fcitx::UserInterfaceComponent::InputPanel);
                }
                keyEvent.filterAndAccept(); return;
            }
            if (keyEvent.key().check(FcitxKey_equal)
                || keyEvent.key().check(FcitxKey_Page_Down)
                || keyEvent.key().check(FcitxKey_Right)) {
                if (pageable->hasNext()) {
                    pageable->next();
                    ic->updateUserInterface(fcitx::UserInterfaceComponent::InputPanel);
                }
                keyEvent.filterAndAccept(); return;
            }
        }
        if (keyEvent.key().check(FcitxKey_Escape)) {
            swift_ime_reset((void *)ic);
            ic->inputPanel().reset();
            ic->updatePreedit();
            ic->updateUserInterface(fcitx::UserInterfaceComponent::InputPanel);
            keyEvent.filterAndAccept(); return;
        }
    }

    auto sym = keyEvent.key().sym();
    uint32_t ch = fcitx::Key::keySymToUnicode(sym);
    if (ch == 0) return;

    char out_text[4096] = {0};
    unsigned int out_len = 0;

    int action = swift_ime_process_key(
        (void *)ic, ch, out_text, sizeof(out_text), &out_len);

    switch (action) {
    case 0: break; // PassThrough

    case 1: { // Preedit
        keyEvent.filterAndAccept();
        // If #wait just fired, start the async poll timer.
        if (!pollTimer_) SwiftImeEngine::startAsyncPoll();
        auto text = fcitx::Text(std::string(out_text, out_len));
        text.setCursor(out_len);
        ic->inputPanel().setClientPreedit(text);
        ic->inputPanel().setAuxUp(text);
        ic->updatePreedit();
        break;
    }

    case 2: { // Commit
        keyEvent.filterAndAccept();
        ic->inputPanel().reset();
        ic->commitString(std::string(out_text, out_len));
        ic->updatePreedit();
        ic->updateUserInterface(fcitx::UserInterfaceComponent::InputPanel);
        break;
    }

    case 3: { // Candidates
        keyEvent.filterAndAccept();
        auto text = fcitx::Text(std::string(out_text, out_len));
        text.setCursor(out_len);
        ic->inputPanel().setClientPreedit(text);
        ic->inputPanel().setAuxUp(text);
        SwiftImeCandidateFFI items[SWIFT_IME_MAX_CANDIDATES];
        unsigned int n = swift_ime_candidates((void *)ic, items, SWIFT_IME_MAX_CANDIDATES);
        if (n > 0) {
            auto list = std::make_unique<fcitx::CommonCandidateList>();
            list->setSelectionKey(selectionKeys_);
            list->setPageSize(7);
            list->setCursorPositionAfterPaging(
                fcitx::CursorPositionAfterPaging::ResetToFirst);
            for (unsigned int i = 0; i < n; i++) {
                std::string text(items[i].text);
                list->append<SwiftCandidateWord>(text, (int)i);
            }
            ic->inputPanel().setCandidateList(std::move(list));
        }
        ic->updatePreedit();
        ic->updateUserInterface(fcitx::UserInterfaceComponent::InputPanel);
        break;
    }

    default: break;
    }
}

// ── Factory ──────────────────────────────────────────────────────────────
fcitx::AddonInstance *SwiftImeFactory::create(fcitx::AddonManager *manager) {
    FCITX_UNUSED(manager);
    if (!initialized) {
        swift_ime_init(nullptr);
        initialized = true;
    }
    return new SwiftImeEngine(manager->instance());
}

FCITX_ADDON_FACTORY(SwiftImeFactory);
