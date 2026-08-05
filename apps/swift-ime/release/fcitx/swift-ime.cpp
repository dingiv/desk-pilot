// swift-ime fcitx5 addon — engine implementation
//
// Each SwiftImeEngine owns an opaque ImeHandle (Rust) that encapsulates
// the Dispatcher and per-context state machines. The C++ side keeps a
// per-context lastView_ for diffing. No global state anywhere.

#include "swift-ime.h"

#include <fcitx/inputcontext.h>
#include <fcitx/inputpanel.h>
#include <fcitx/candidatelist.h>
#include <fcitx/userinterfacemanager.h>
#include <fcitx/instance.h>
#include <fcitx-utils/key.h>
#include <fcitx-utils/event.h>
#include <fcitx-utils/log.h>
#include <cstring>
#include <memory>
#include <string>
#include <time.h>

// ── Helper ──────────────────────────────────────────────────────────────

static inline bool str_changed(const char *a, const char *b) {
    return std::strcmp(a, b) != 0;
}

// ── Constructor / Destructor ────────────────────────────────────────────

SwiftImeEngine::SwiftImeEngine(fcitx::Instance *instance)
    : handle_(swift_ime_create(nullptr)), instance_(instance)
{
    fcitx::KeySym syms[] = {
        FcitxKey_1, FcitxKey_2, FcitxKey_3, FcitxKey_4, FcitxKey_5,
        FcitxKey_6, FcitxKey_7, FcitxKey_8, FcitxKey_9, FcitxKey_0};
    for (auto sym : syms) {
        selectionKeys_.emplace_back(sym, fcitx::KeyStates());
    }
}

SwiftImeEngine::~SwiftImeEngine() {
    swift_ime_destroy(handle_);
}

// ── apply_view — diff & reconcile (per-context lastView) ────────────────

void SwiftImeEngine::apply_view(fcitx::InputContext *ic, const ImeView &v) {
    auto &prev = lastViews_[ic];

    // Commit
    if (v.commit_text[0] != 0) {
        ic->inputPanel().reset();
        ic->commitString(std::string(v.commit_text));
        ic->updatePreedit();
        ic->updateUserInterface(fcitx::UserInterfaceComponent::InputPanel);
        prev = v;
        return;
    }

    // Preedit
    if (str_changed(v.preedit_text, prev.preedit_text)
        || v.preedit_cursor != prev.preedit_cursor) {
        if (v.preedit_text[0] != 0) {
            auto text = fcitx::Text(std::string(v.preedit_text));
            text.setCursor(v.preedit_cursor);
            ic->inputPanel().setClientPreedit(text);
        } else if (prev.preedit_text[0] != 0) {
            ic->inputPanel().reset();
        }
    }

    // Candidates. Compare highlight + page too — arrows move the highlight without touching the
    // candidate texts, and a missed diff here would leave the panel stuck on the first candidate
    // (the highlight-apply loop below only runs when the list is rebuilt).
    bool cands_changed =
        v.candidate_count != prev.candidate_count
        || v.candidate_highlight != prev.candidate_highlight
        || v.candidate_page != prev.candidate_page
        || std::memcmp(v.candidates, prev.candidates,
                       sizeof(v.candidates)) != 0;
    if (cands_changed) {
        if (v.candidate_count > 0) {
            auto list = std::make_unique<fcitx::CommonCandidateList>();
            list->setSelectionKey(selectionKeys_);
            list->setPageSize(v.candidate_page_size > 0
                                  ? v.candidate_page_size
                                  : 7);
            list->setCursorPositionAfterPaging(
                fcitx::CursorPositionAfterPaging::ResetToFirst);
            for (unsigned int i = 0;
                 i < v.candidate_count && i < CANDIDATE_SLOTS; i++) {
                list->append<SwiftCandidateWord>(
                    std::string(v.candidates[i].text), (int)i, this);
            }
            if (v.candidate_page > 0 && list->toPageable()) {
                auto *p = list->toPageable();
                for (unsigned int pg = 0;
                     pg < v.candidate_page && p->hasNext(); pg++)
                    p->next();
            }
            if (v.candidate_highlight > 0 && list->toCursorMovable()) {
                auto *m = list->toCursorMovable();
                unsigned int pageSize =
                    v.candidate_page_size > 0 ? v.candidate_page_size : 7;
                for (unsigned int h = 0;
                     h < v.candidate_highlight % pageSize; h++)
                    m->nextCandidate();
            }
            ic->inputPanel().setCandidateList(std::move(list));
            ic->inputPanel().setAuxUp(
                fcitx::Text(std::string(v.aux_up)));
        } else if (prev.candidate_count > 0) {
            ic->inputPanel().setCandidateList(nullptr);
        }
    }

    // Aux up when preedit exists without candidates
    if (v.preedit_text[0] != 0 && v.candidate_count == 0) {
        ic->inputPanel().setAuxUp(
            fcitx::Text(std::string(v.preedit_text)));
    }

    ic->updatePreedit();
    ic->updateUserInterface(fcitx::UserInterfaceComponent::InputPanel);

    // After refresh: log the candidate area when in voice (#asr) mode. Debug-level (gated by
    // FCITX_DEBUG) so it doesn't spam by default — enable when diagnosing the live-refresh path.
    if (std::strstr(v.preedit_text, "asr") != nullptr) {
        FCITX_DEBUG() << "[voice] apply_view ic=" << ic << " count=" << v.candidate_count
                      << " preedit='" << v.preedit_text << "'";
        for (unsigned int i = 0; i < v.candidate_count && i < CANDIDATE_SLOTS; i++) {
            FCITX_DEBUG() << "[voice]   [" << i << "] " << v.candidates[i].text;
        }
    }

    prev = v;
}

// ── Async poll timer ────────────────────────────────────────────────────

void SwiftImeEngine::startAsyncPoll() {
    if (pollTimer_) return;
    // fcitx5 recurring-timer idiom: first fire = now + period, interval = dummy, re-arm manually
    // via setTime + setOneShot in the callback (interval arg alone doesn't repeat). See
    // startMagicPoll + fcitx5-chinese-addons/pinyincandidate.cpp.
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    uint64_t now = (uint64_t)ts.tv_sec * 1000000 + ts.tv_nsec / 1000;
    pollTimer_ = instance_->eventLoop().addTimeEvent(
        CLOCK_MONOTONIC, now + 100000, 1,
        [this](fcitx::EventSourceTime *event, uint64_t time) {
            for (uintptr_t probe = 1; probe < 1024; probe++) {
                void *ctx = (void *)probe;
                ImeView view;
                int r = swift_ime_poll_async(handle_, ctx, &view);
                if (r == 0) continue;
                auto *ic =
                    reinterpret_cast<fcitx::InputContext *>(ctx);
                apply_view(ic, view);
                if (r == 2) {
                    pollTimer_.reset();
                    return false;  // #wait demo completed — stop the timer
                }
                break;
            }
            event->setTime(time + 100000);  // re-arm
            event->setOneShot();
            return true;
        });
}

// ── Magic live-command async-refresh timer ──────────────────────────────
//
// A dedicated 100 ms TimeEvent (decoupled from the #wait pollTimer_) that, for every active
// input context, calls swift_ime_magic_tick and applies the view if the active live magic
// member's async state advanced (voice buffer for `#asr`, HTTP result for `#req`). This is
// what makes the candidate area update live WITHOUT a keypress — the Rust engine reads the
// AsrBuffer (written by the background aura SSE thread) / the HTTP worker result and rebuilds
// candidates; we just push them into fcitx5's inputPanel + repaint. Runs on fcitx5's main loop
// → thread-safe.

void SwiftImeEngine::startMagicPoll() {
    if (magicTimer_) return;
    FCITX_INFO() << "magic poll timer started (100ms)";
    // fcitx5's addTimeEvent does NOT auto-repeat by the interval arg. The recurring idiom (per
    // fcitx5-chinese-addons/pinyincandidate.cpp) is: first fire = now + period, interval = dummy,
    // then in the callback manually setTime(next) + setOneShot() to re-arm. Returning true alone
    // fires only once.
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    uint64_t now = (uint64_t)ts.tv_sec * 1000000 + ts.tv_nsec / 1000;
    magicTimer_ = instance_->eventLoop().addTimeEvent(
        CLOCK_MONOTONIC, now + 100000, 1,
        [this](fcitx::EventSourceTime *event, uint64_t time) {
            for (auto *ic : activeContexts_) {
                ImeView view;
                if (swift_ime_magic_tick(handle_, (void *)ic, &view)) {
                    apply_view(ic, view);
                }
            }
            event->setTime(time + 100000);  // re-arm: next fire 100ms after this one
            event->setOneShot();
            return true;
        });
}

// ── Candidate word ──────────────────────────────────────────────────────

SwiftCandidateWord::SwiftCandidateWord(const std::string &text, int index,
                                       SwiftImeEngine *engine)
    : fcitx::CandidateWord(fcitx::Text(text)),
      index_(index),
      engine_(engine) {}

void SwiftCandidateWord::select(
    fcitx::InputContext *inputContext) const
{
    ImeView view;
    swift_ime_select_candidate(engine_->handle_, (void *)inputContext,
                               (unsigned int)index_, &view);
    engine_->apply_view(inputContext, view);
}

// ── Lifecycle ───────────────────────────────────────────────────────────

void SwiftImeEngine::activate(const fcitx::InputMethodEntry &entry,
                               fcitx::InputContextEvent &event) {
    FCITX_UNUSED(entry);
    auto *ic = event.inputContext();
    if (!ic) return;
    // Safety: ensure no stale lastView carries over from a previous session.
    lastViews_.erase(ic);
    activeContexts_.insert(ic);
    if (!magicTimer_) startMagicPoll();
    swift_ime_activate(handle_, (void *)ic);
}

void SwiftImeEngine::deactivate(const fcitx::InputMethodEntry &entry,
                                 fcitx::InputContextEvent &event) {
    FCITX_UNUSED(entry);
    auto *ic = event.inputContext();
    if (!ic) return;

    // Commit pending before deactivating.
    ImeView view;
    swift_ime_commit_pending(handle_, (void *)ic, &view);
    if (view.commit_text[0] != 0) {
        ic->commitString(std::string(view.commit_text));
    }
    swift_ime_deactivate(handle_, (void *)ic);

    // Clear the UI unconditionally — even if nothing to commit,
    // the input panel may still show stale preedit/candidates.
    ic->inputPanel().reset();
    ic->updatePreedit();
    ic->updateUserInterface(fcitx::UserInterfaceComponent::InputPanel);
    activeContexts_.erase(ic);
    lastViews_.erase(ic);
}

void SwiftImeEngine::reset(const fcitx::InputMethodEntry &entry,
                            fcitx::InputContextEvent &event) {
    FCITX_UNUSED(entry);
    auto *ic = event.inputContext();
    if (!ic) return;
    swift_ime_reset(handle_, (void *)ic);
    // Clear the UI so preedit/candidates don't linger after focus change.
    ic->inputPanel().reset();
    ic->updatePreedit();
    ic->updateUserInterface(fcitx::UserInterfaceComponent::InputPanel);
    lastViews_.erase(ic);
}

// ── Key event ───────────────────────────────────────────────────────────

// Special keys are handled by the engine via swift_ime_special_key (navigation, paging, cursor
// movement, +- paging); everything else falls through to the character path. These must be
// intercepted HERE — keySymToUnicode returns 0 for arrows/PgUp/PgDn (they'd be silently eaten)
// and '+','-','=','[',']' have unicode (they'd commit the preedit and insert a symbol).
static int special_key_code(fcitx::KeySym sym) {
    switch (sym) {
        case FcitxKey_Up:        return 1;   // SpecialKey::Up
        case FcitxKey_Down:      return 2;   // SpecialKey::Down
        case FcitxKey_Left:      return 3;   // SpecialKey::Left
        case FcitxKey_Right:     return 4;   // SpecialKey::Right
        case FcitxKey_Tab:       return 5;   // SpecialKey::Tab
        case FcitxKey_Page_Up:   return 6;   // SpecialKey::PageUp
        case FcitxKey_Page_Down: return 7;   // SpecialKey::PageDown
        case FcitxKey_bracketleft:  return 20; // SpecialKey::BracketLeft (cursor left)
        case FcitxKey_bracketright: return 21; // SpecialKey::BracketRight (cursor right)
        case FcitxKey_plus:
        case FcitxKey_equal:     return 22;  // SpecialKey::Plus (next page)
        case FcitxKey_minus:     return 23;  // SpecialKey::Minus (prev page)
        default:                 return 0;
    }
}

void SwiftImeEngine::keyEvent(const fcitx::InputMethodEntry &entry,
                               fcitx::KeyEvent &keyEvent) {
    FCITX_UNUSED(entry);
    if (keyEvent.isRelease()) return;

    auto *ic = keyEvent.inputContext();
    if (!ic) return;

    // Feed surrounding text to the engine for context-aware prediction.
    if (ic->capabilityFlags().test(fcitx::CapabilityFlag::SurroundingText)) {
        auto &st = ic->surroundingText();
        if (st.isValid()) {
            swift_ime_set_surrounding(handle_, (void *)ic,
                                      st.text().c_str());
        }
    }

    ImeView view;
    auto sym = keyEvent.key().sym();
    int sp = special_key_code(sym);
    if (sp != 0) {
        // Navigation / paging / cursor / +- — never a character.
        swift_ime_special_key(handle_, (void *)ic, sp, &view);
    } else {
        uint32_t ch = fcitx::Key::keySymToUnicode(sym);
        if (ch == 0) return;  // unmapped non-character (F1, …) — let fcitx handle it
        swift_ime_process_key(handle_, (void *)ic, ch, &view);
    }

    if (view.key_passthrough) {
        apply_view(ic, view);
        return;
    }

    keyEvent.filterAndAccept();

    if (!pollTimer_) {
        startAsyncPoll();
    }
    // Track the key-receiving ic + ensure the magic-refresh timer is running. Starting here (not
    // only in activate) is the robust pattern — activate's ic isn't always the keyEvent ic, and
    // activate may not fire before the first key in every fcitx5 flow.
    activeContexts_.insert(ic);
    if (!magicTimer_) {
        startMagicPoll();
    }

    apply_view(ic, view);
}

// ── Factory ─────────────────────────────────────────────────────────────

fcitx::AddonInstance *SwiftImeFactory::create(
    fcitx::AddonManager *manager)
{
    return new SwiftImeEngine(manager->instance());
}

FCITX_ADDON_FACTORY(SwiftImeFactory);
