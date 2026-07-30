// swift-ime fcitx5 addon — engine implementation
//
// Each SwiftImeEngine owns an opaque ImeHandle (Rust) that encapsulates
// the Dispatcher and per-context state machines. The C++ side keeps a
// per-context lastView_ for diffing. No global state anywhere.

#include "engine.h"

#include <fcitx/inputcontext.h>
#include <fcitx/inputpanel.h>
#include <fcitx/candidatelist.h>
#include <fcitx/userinterfacemanager.h>
#include <fcitx/instance.h>
#include <fcitx-utils/key.h>
#include <fcitx-utils/event.h>
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

    // Candidates
    bool cands_changed =
        v.candidate_count != prev.candidate_count
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
    prev = v;
}

// ── Async poll timer ────────────────────────────────────────────────────

void SwiftImeEngine::startAsyncPoll() {
    if (pollTimer_) return;
    pollTimer_ = instance_->eventLoop().addTimeEvent(
        CLOCK_MONOTONIC, 0, 100000,
        [this](fcitx::EventSourceTime *, uint64_t) {
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
                    return false;
                }
                break;
            }
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
    swift_ime_activate(handle_, (void *)event.inputContext());
}

void SwiftImeEngine::deactivate(const fcitx::InputMethodEntry &entry,
                                 fcitx::InputContextEvent &event) {
    FCITX_UNUSED(entry);
    auto *ic = event.inputContext();
    if (!ic) return;

    // Always commit pending and clean up — regardless of event type.
    // Skipping FocusOut leaves dangling pointers when the InputContext
    // is later destroyed (window closed).
    ImeView view;
    swift_ime_commit_pending(handle_, (void *)ic, &view);
    if (view.commit_text[0] != 0) {
        ic->commitString(std::string(view.commit_text));
    }
    swift_ime_deactivate(handle_, (void *)ic);
    lastViews_.erase(ic);
}

void SwiftImeEngine::reset(const fcitx::InputMethodEntry &entry,
                            fcitx::InputContextEvent &event) {
    FCITX_UNUSED(entry);
    swift_ime_reset(handle_, (void *)event.inputContext());
}

// ── Key event ───────────────────────────────────────────────────────────

void SwiftImeEngine::keyEvent(const fcitx::InputMethodEntry &entry,
                               fcitx::KeyEvent &keyEvent) {
    FCITX_UNUSED(entry);
    if (keyEvent.isRelease()) return;

    auto *ic = keyEvent.inputContext();
    if (!ic) return;

    // ── Candidate-list navigation (local) ──
    auto candList = ic->inputPanel().candidateList();
    if (candList && !candList->empty()) {
        // Space: select the first visible candidate on the current page.
        if (keyEvent.key().check(FcitxKey_space)) {
            candList->candidate(0).select(ic);
            keyEvent.filterAndAccept();
            return;
        }
        if (auto idx = keyEvent.key().keyListIndex(selectionKeys_);
            idx >= 0 && idx < (int)candList->size()) {
            candList->candidate(idx).select(ic);
            keyEvent.filterAndAccept();
            return;
        }
        if (auto *m = candList->toCursorMovable()) {
            if (keyEvent.key().check(FcitxKey_Up)) {
                m->prevCandidate();
                ic->updateUserInterface(
                    fcitx::UserInterfaceComponent::InputPanel);
                keyEvent.filterAndAccept();
                return;
            }
            if (keyEvent.key().check(FcitxKey_Down)) {
                m->nextCandidate();
                ic->updateUserInterface(
                    fcitx::UserInterfaceComponent::InputPanel);
                keyEvent.filterAndAccept();
                return;
            }
        }
        if (auto *p = candList->toPageable()) {
            if (keyEvent.key().check(FcitxKey_minus)
                || keyEvent.key().check(FcitxKey_Page_Up)
                || keyEvent.key().check(FcitxKey_Left)) {
                if (p->hasPrev()) {
                    p->prev();
                    ic->updateUserInterface(
                        fcitx::UserInterfaceComponent::InputPanel);
                }
                keyEvent.filterAndAccept();
                return;
            }
            if (keyEvent.key().check(FcitxKey_equal)
                || keyEvent.key().check(FcitxKey_Page_Down)
                || keyEvent.key().check(FcitxKey_Right)) {
                if (p->hasNext()) {
                    p->next();
                    ic->updateUserInterface(
                        fcitx::UserInterfaceComponent::InputPanel);
                }
                keyEvent.filterAndAccept();
                return;
            }
        }
        if (keyEvent.key().check(FcitxKey_Escape)) {
            swift_ime_reset(handle_, (void *)ic);
            ic->inputPanel().reset();
            ic->updatePreedit();
            ic->updateUserInterface(
                fcitx::UserInterfaceComponent::InputPanel);
            keyEvent.filterAndAccept();
            return;
        }
    }

    // ── Process key through Rust engine ──
    auto sym = keyEvent.key().sym();
    uint32_t ch = fcitx::Key::keySymToUnicode(sym);
    if (ch == 0) return;

    ImeView view;
    swift_ime_process_key(handle_, (void *)ic, ch, &view);

    if (view.key_passthrough) {
        apply_view(ic, view);
        return;
    }

    keyEvent.filterAndAccept();

    if (!pollTimer_) {
        startAsyncPoll();
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
