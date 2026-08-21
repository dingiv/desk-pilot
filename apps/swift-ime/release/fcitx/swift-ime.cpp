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
#include <fcitx/addonmanager.h>
#include <clipboard_public.h>  // fcitx::IClipboard — $CLIPBOARD snippet variable
#include <fcitx-utils/key.h>
#include <fcitx-utils/event.h>
#include <fcitx-utils/log.h>
#include <cstring>
#include <fcntl.h>
#include <memory>
#include <string>
#include <time.h>
#include <unistd.h>
#include <vector>

// ── Helper ──────────────────────────────────────────────────────────────

static inline bool str_changed(const char *a, const char *b) {
    return std::strcmp(a, b) != 0;
}

// ── Constructor / Destructor ────────────────────────────────────────────

SwiftImeEngine::SwiftImeEngine(fcitx::Instance *instance)
    : frontend_{this, uiRefreshCb, uiClipboardCb},
      handle_(swift_ime_create(nullptr, &frontend_)),
      instance_(instance)
{
    fcitx::KeySym syms[] = {
        FcitxKey_1, FcitxKey_2, FcitxKey_3, FcitxKey_4, FcitxKey_5,
        FcitxKey_6, FcitxKey_7, FcitxKey_8, FcitxKey_9, FcitxKey_0};
    for (auto sym : syms) {
        selectionKeys_.emplace_back(sym, fcitx::KeyStates());
    }
    // 前端 UI 回调(刷新 / 剪贴板请求)打包在 frontend_ 里,已随 create 传入。
    // 跨线程唤醒管道:引擎 I/O 线程写,主循环的 fd 就绪事件立即 drain。
    // 这是 fcitx 标准的跨线程姿势 —— 比跨线程 addTimeEvent 可靠(后者主循环
    // poll 睡眠时注意不到新 timer,会延迟数秒)。
    int pipefd[2];
    if (::pipe(pipefd) == 0) {
        wakePipe_ = pipefd[1];
        // 读写端都设非阻塞:
        //  - 写端:管道满时 write 返回 EAGAIN,丢弃即可(下一条刷新会再写);
        //  - 读端:drain 的 while(read) 读空时返回 EAGAIN 退出 —— 若保持阻塞,
        //    写端未关时 read 会永远等数据,主循环卡死(整 fcitx 死锁)。
        int flags = ::fcntl(wakePipe_, F_GETFL);
        ::fcntl(wakePipe_, F_SETFL, flags | O_NONBLOCK);
        ::fcntl(pipefd[0], F_SETFL, flags | O_NONBLOCK);
        wakeSource_ = instance_->eventLoop().addIOEvent(
            pipefd[0], fcitx::IOEventFlag::In,
            [this](fcitx::EventSourceIO *, int fd, fcitx::IOEventFlags) {
                // 清空管道字节(非阻塞,读空即停),然后 drain 所有 pending 刷新。
                char buf[64];
                while (::read(fd, buf, sizeof(buf)) > 0) {
                }
                drainRefresh();
                return true;
            });
    }
}

SwiftImeEngine::~SwiftImeEngine() {
    if (wakePipe_ >= 0) {
        ::close(wakePipe_);
        wakePipe_ = -1;
    }
    // wakeSource_ 随成员析构,fcitx 负责关闭读端 fd。
    swift_ime_destroy(handle_);
}

// ── apply_view — diff & reconcile (per-context lastView) ────────────────

void SwiftImeEngine::apply_view(fcitx::InputContext *ic, const ImeView &v) {
    auto &prev = lastViews_[ic];

    // Delete: `#del` —— 先清 preedit/候选,让应用光标回到已提交文本之后,再
    // 让 scout 用 uinput 注入 count 个 Backspace(硬件级,绕过 Wayland 虚拟键盘
    // 协议 / surrounding-text 依赖)。scout 没起/失败 → 回退 deleteSurroundingText。
    if (v.delete_count > 0) {
        FCITX_INFO() << "#del delete " << v.delete_count;
        ic->inputPanel().reset();
        ic->updatePreedit();
        ic->updateUserInterface(fcitx::UserInterfaceComponent::InputPanel);
        if (swift_ime_inject_backspaces(handle_, v.delete_count) != 1) {
            FCITX_INFO() << "#del scout inject failed, fallback deleteSurroundingText";
            ic->deleteSurroundingText(-(int)v.delete_count, v.delete_count);
        }
        prev = v;
        return;
    }

    // Commit
    if (v.commit_text[0] != 0) {
        ic->inputPanel().reset();
        auto text = std::string(v.commit_text);
        ic->commitString(text);
        // $CURSOR: commitString always leaves the caret at the end — forward
        // Left keystrokes back to the marker position (same trick fcitx5-rime
        // uses for its post-commit cursor moves). No-op when the marker is at
        // the end (commit_cursor == text.size()).
        if (v.commit_cursor < text.size()) {
            auto back = text.size() - v.commit_cursor;
            for (unsigned int i = 0; i < back; i++) {
                ic->forwardKey(fcitx::Key(FcitxKey_Left));
            }
        }
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
                    std::string(v.candidates[i].text),
                    std::string(v.candidates[i].meta), (int)i, this);
            }
            if (list->toPageable()) {
                auto *p = list->toPageable();
                for (unsigned int pg = 0;
                     pg < v.candidate_page && p->hasNext(); pg++)
                    p->next();
            }
            // 高亮:直接用全局光标 API,与引擎内部的 candidate_highlight
            // 精确同步。旧的 nextCandidate 循环从初始 -1 光标起跳,首次调用
            // 只落到页首而不 +1 —— 高亮永远比引擎差 1(跨页后 UI 高亮与
            // 空格提交的选项不一致)。
            if (v.candidate_highlight < v.candidate_count) {
                list->setGlobalCursorIndex(v.candidate_highlight);
            }
            ic->inputPanel().setCandidateList(std::move(list));
        } else if (prev.candidate_count > 0) {
            ic->inputPanel().setCandidateList(nullptr);
        }
    }

    // AuxUp(候选框顶部 preedit):独立检测变化。之前 auxUp 绑在
    // cands_changed 里 —— 当候选词不变但输入增长(如 feichag→feichagn,
    // 前 8 个候选恰好相同)时 cands_changed=false,auxUp 停在旧值
    // ("feichag" 少了一个 n)。
    if (str_changed(v.aux_up, prev.aux_up) || cands_changed) {
        if (v.aux_up[0] != 0) {
            ic->inputPanel().setAuxUp(fcitx::Text(std::string(v.aux_up)));
        }
    }

    // 候选框的 panel preedit 也独立更新(与 clientPreedit 同步)。
    if (str_changed(v.preedit_text, prev.preedit_text)
        || v.preedit_cursor != prev.preedit_cursor || cands_changed) {
        auto preeditText = fcitx::Text(std::string(v.preedit_text));
        preeditText.setCursor(v.preedit_cursor);
        ic->inputPanel().setClientPreedit(preeditText);
    }

    // Aux up when preedit exists without candidates
    if (v.preedit_text[0] != 0 && v.candidate_count == 0
        && v.aux_up[0] == 0) {
        ic->inputPanel().setAuxUp(
            fcitx::Text(std::string(v.preedit_text)));
    }

    ic->updatePreedit();
    ic->updateUserInterface(fcitx::UserInterfaceComponent::InputPanel);

    // After refresh: log the candidate area when in voice (#asr) mode. Debug-level (gated by
    // FCITX_DEBUG) so it doesn't spam by default — enable when diagnosing the live-refresh path.
    // if (std::strstr(v.preedit_text, "asr") != nullptr) {
    //     FCITX_DEBUG() << "[voice] apply_view ic=" << ic << " count=" << v.candidate_count
    //                   << " preedit='" << v.preedit_text << "'";
    //     for (unsigned int i = 0; i < v.candidate_count && i < CANDIDATE_SLOTS; i++) {
    //         FCITX_DEBUG() << "[voice]   [" << i << "] " << v.candidates[i].text;
    //     }
    // }

    prev = v;
}

// ── 按需 UI 刷新(替代旧的 100ms 轮询)──────────────────────────────────
//
// 引擎 I/O 线程在异步状态推进(voice/req/clipboard)后经 FrontEndHandle
// 调 `onRefresh(ctx)` —— 这里把它 marshal 到 fcitx 主循环:记录 pending ctx,
// 若尚未有 drain 定时器则排一个单发事件,主循环上 `swift_ime_magic_tick`
// 拉最新视图 + apply_view。空闲(无 pending)时零轮询。

void SwiftImeEngine::onRefresh(uintptr_t ctx) {
    // 诊断:voice Attach 的定向 ctx(#asr)/ req / clip 推真实 ic 指针;
    // ctx=0 广播分支留作后备(当前无源会发 0)。
    // FCITX_INFO() << "[refresh] onRefresh ctx=" << (void *)ctx;
    {
        std::lock_guard<std::mutex> lk(refreshMutex_);
        pendingRefreshes_.insert(ctx);
    }
    // 唤醒主循环:跨线程写管道字节 → 主循环 fd 就绪事件立即 drain。
    // 管道满(极罕见)则丢弃本次唤醒 —— 下次写会再触发,pending 不丢。
    if (wakePipe_ >= 0) {
        char b = 1;
        ssize_t r = ::write(wakePipe_, &b, 1);
        (void)r;
    }
}

/// 主循环 drain:清 pending,逐 ctx magic_tick + apply_view。
/// 由管道 fd 就绪事件触发(主循环线程执行)。
void SwiftImeEngine::drainRefresh() {
    std::set<uintptr_t> ctxs;
    {
        std::lock_guard<std::mutex> lk(refreshMutex_);
        ctxs.swap(pendingRefreshes_);
    }
    for (uintptr_t c : ctxs) {
        // ctx 0 = 引擎级广播:遍历所有活动上下文逐出一次 magic_tick —— 只有
        // 处于 live 魔法会话(#asr)的 context 返回新视图,其余返回 0 跳过。
        if (c == 0) {
            // FCITX_DEBUG() << "[refresh] broadcast ctx=0 → activeContexts_="
            //              << activeContexts_.size();
            std::vector<fcitx::InputContext *> all(activeContexts_.begin(),
                                                   activeContexts_.end());
            for (auto *ic : all) {
                ImeView view;
                int r = swift_ime_magic_tick(handle_, (void *)ic, &view);
                FCITX_DEBUG() << "[refresh]   ic=" << ic << " magic_tick=" << r;
                if (r) {
                    apply_view(ic, view);
                }
            }
            continue;
        }
        auto *ic = reinterpret_cast<fcitx::InputContext *>(c);
        ImeView view;
        int r = swift_ime_magic_tick(handle_, (void *)ic, &view);
        // FCITX_INFO() << "[refresh] directed ctx=" << (void *)ic
        //              << " magic_tick=" << r;
        if (r) {
            apply_view(ic, view);
        }
    }
}

/// `#clip` 请求剪贴板:公开接口只给当前值,推给引擎累积历史。
void SwiftImeEngine::onClipboardRequest(uint32_t) {
    if (activeContexts_.empty()) return;
    auto *ic = *activeContexts_.begin();
    if (auto *cb = instance_->addonManager().addon("clipboard")) {
        auto text = cb->call<fcitx::IClipboard::clipboard>(ic);
        if (!text.empty()) {
            swift_ime_set_clipboard(handle_, text.c_str());
        }
    }
}

/// C 回调转发(引擎 I/O 线程调用)。
void SwiftImeEngine::uiRefreshCb(uintptr_t ctx, void *userdata) {
    static_cast<SwiftImeEngine *>(userdata)->onRefresh(ctx);
}

void SwiftImeEngine::uiClipboardCb(uint32_t count, void *userdata) {
    static_cast<SwiftImeEngine *>(userdata)->onClipboardRequest(count);
}

// ── Candidate word ──────────────────────────────────────────────────────

SwiftCandidateWord::SwiftCandidateWord(const std::string &text,
                                       const std::string &meta, int index,
                                       SwiftImeEngine *engine)
    : fcitx::CandidateWord(fcitx::Text(text)),
      index_(index),
      engine_(engine) {
    // 调试模式(swift-ime.yaml → debug.candidate_meta):meta 显示在候选词
    // 右侧注释(灰色小字),空时 fcitx 不渲染。
    if (!meta.empty()) {
        setComment(fcitx::Text(meta));
    }
}

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
    swift_ime_activate(handle_, (void *)ic);

    // 首次激活同步当前剪贴板 —— 否则用户激活后立即打 #clip 时 hist 为空。
    // 历史累积的主路径仍是按键阶段的按需推送(见 keyEvent)。
    if (auto *cb = instance_->addonManager().addon("clipboard")) {
        auto text = cb->call<fcitx::IClipboard::clipboard>(ic);
        if (!text.empty()) {
            swift_ime_set_clipboard(handle_, text.c_str());
        }
    }
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

// 忠实转发:这里不做任何键拦截或映射 —— 特殊键、Ctrl/Shift/Alt 修饰状态
// 原样打包给引擎的输入路由层(状态机表),由它决定键属于输入法还是应用,
// 返回的 ImeView::action 告诉我们如何反应。keySymToUnicode 对方向键等返回
// 0,所以 keysym 也要一并传递。
void SwiftImeEngine::keyEvent(const fcitx::InputMethodEntry &entry,
                               fcitx::KeyEvent &keyEvent) {
    FCITX_UNUSED(entry);
    if (keyEvent.isRelease()) return;

    auto *ic = keyEvent.inputContext();
    if (!ic) return;

    // $CLIPBOARD 片段变量:仅在组合 `/`/`#` 触发时推送当前剪贴板(既供变量
    // 解析,也顺手累积 #clip 历史)。历史主来源是 `#clip` 的按需请求
    // (onClipboardRequest);这里不再逐键推送。
    auto &prev = lastViews_[ic];
    if (prev.preedit_text[0] == '/' || prev.preedit_text[0] == '#') {
        if (auto *cb = instance_->addonManager().addon("clipboard")) {
            auto text = cb->call<fcitx::IClipboard::clipboard>(ic);
            if (!text.empty()) {
                swift_ime_set_clipboard(handle_, text.c_str());
            }
        }
    }

    // Pack the key faithfully — sym + unicode + modifier states, nothing else.
    auto sym = keyEvent.key().sym();
    auto states = keyEvent.key().states();
    SwiftKeyPacket pkt;
    pkt.sym = static_cast<uint32_t>(sym);
    pkt.unicode = fcitx::Key::keySymToUnicode(sym);
    pkt.ctrl = states.testAny(fcitx::KeyState::Ctrl) ? 1 : 0;
    pkt.shift = states.testAny(fcitx::KeyState::Shift) ? 1 : 0;
    pkt.alt = states.testAny(fcitx::KeyState::Alt) ? 1 : 0;

    ImeView view;
    swift_ime_key(handle_, (void *)ic, &pkt, &view);

    // Action-driven reaction: HANDLED unset → the key belongs to the
    // application (idle Esc/'-'/arrows, Ctrl/Alt shortcuts…). Not calling
    // filterAndAccept lets it fall through untouched.
    if (!(view.action & SWIFT_ACTION_HANDLED)) {
        apply_view(ic, view);
        return;
    }

    keyEvent.filterAndAccept();
    activeContexts_.insert(ic);
    apply_view(ic, view);
}

// ── Factory ─────────────────────────────────────────────────────────────

fcitx::AddonInstance *SwiftImeFactory::create(
    fcitx::AddonManager *manager)
{
    return new SwiftImeEngine(manager->instance());
}

FCITX_ADDON_FACTORY(SwiftImeFactory);
