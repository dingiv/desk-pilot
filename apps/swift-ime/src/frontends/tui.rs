//! TUI frontend — swift-ime 的调试前端三合一:
//!
//! 1. **socket 调试 server**([`run_server`],main.rs 默认):同一引擎同时
//!    监听 TUI 键盘与 Unix socket —— `swift_cli` 从命令行发按键、收回
//!    JSON 视图,多行调试核心功能(链式预测等);
//! 2. **TUI 渲染**(ratatui):按键忠实喂引擎,渲染 [`ImeView`];
//! 3. **评测工具**(`--input` / `--cases`):单输入 / 批量评估。
//!
//! The caller (main.rs) parses CLI args and passes them as a [`TuiConfig`].

use std::sync::Arc;

/// 键盘事件轮询节拍(voice 推送 / socket 喂键 ≤50ms 内被感知)。
const POLL_MS: u64 = 50;
use std::time::{Duration, Instant};

use crate::constants;
use ime_core::engine::{ImeEngine, KeyEvent};
use ime_core::ImeView;

// ── Config ─────────────────────────────────────────────────────────────

/// All configuration for the mock frontend, passed from main.rs.
#[derive(Clone)]
pub struct TuiConfig {
    pub cases: Option<String>,
    pub input: Option<String>,
    pub top_n: usize,
    pub verbose: bool,
    pub config: Option<String>,
    pub asr_text: Option<String>,
    pub commit: bool,
    pub async_wait: u64,
    pub voice_aura_base: String,
    /// 语音连接空闲自动断连时长(秒,0 = 永不主动断);默认 30。
    pub voice_idle_time: u64,
    pub en_user_dict: Option<String>,
    pub en_dicts: Vec<String>,
    /// `#req` backend base URL — CLI override of `magic.req_base` config。
    pub req_base: Option<String>,
    /// 前端句柄(引擎 I/O 线程推送刷新)。None → NoopFrontend。
    pub frontend: Option<Arc<dyn ime_core::frontend::FrontEndHandle>>,
    /// dev 构建下日志是否 tee 到 stderr。CLI/mock 可以;TUI 必须 false
    /// (stderr 会打坏 alternate screen 界面)。
    pub tee_stderr: bool,
}

impl Default for TuiConfig {
    fn default() -> Self {
        TuiConfig {
            cases: None, input: None,
            top_n: 160, verbose: true,
            req_base: None,
            config: None, asr_text: None,
            commit: false, async_wait: 0,
            voice_aura_base: ime_core::engine::DEFAULT_VOICE_AURA_BASE.to_string(),
            voice_idle_time: ime_core::io_thread::DEFAULT_IDLE_TIMEOUT_SECS,
            en_user_dict: None, en_dicts: Vec::new(),
            frontend: None,
            tee_stderr: true,
        }
    }
}

// ── Entry points ──────────────────────────────────────────────────────

/// Run batch evaluation against a test cases file.
pub fn run_cases_mode(cfg: &TuiConfig, cases_path: &str) {
    let (mut engine, _) = build_engine(cfg);
    run_cases(&mut engine, cases_path, cfg.verbose);
}

/// Run single-input mode (show candidates or commit).
pub fn run_input_mode(cfg: &TuiConfig) {
    let (mut engine, state) = build_engine(cfg);
    let input = cfg.input.as_deref().unwrap_or("");
    if cfg.async_wait > 0 { wait_for_voice(&state, cfg.async_wait); }
    if cfg.commit {
        show_commit(&mut engine, input, cfg.verbose);
    } else {
        show_candidates_with_async(&mut engine, &state, input, cfg.top_n, cfg.verbose, cfg.async_wait);
    }
}

/// Build the IME engine with all config applied. Returns the engine and the shared voice state
/// (for callers that want to inspect / seed voice data — TUI mocks can `seed_final` here).
pub fn build_engine(cfg: &TuiConfig) -> (ImeEngine, Arc<ime_core::voice_state::SharedVoiceState>) {
    let sw_cfg = if let Some(ref path) = cfg.config {
        match std::fs::read_to_string(path) {
            Ok(yaml) => match serde_yaml::from_str::<crate::config::SwiftImeConfig>(&yaml) {
                Ok(c) => { crate::ime_log!("loaded config from {path}"); c }
                Err(e) => { tracing::warn!(target: "swift_ime", "config parse error: {e}, using defaults"); crate::config::SwiftImeConfig::default() }
            },
            Err(e) => { tracing::warn!(target: "swift_ime", "config read error: {e}, using defaults"); crate::config::SwiftImeConfig::default() }
        }
    } else {
        crate::config::SwiftImeConfig::load()
    };

    // 装进程级 tracing subscriber —— mock/TUI 的唯一初始化点(Once 幂等,
    // fcitx5 cdylib 在 swift_ime_create 里各自初始化)。引擎与前端日志统一
    // 写进 swift-ime.log,级别取 debug.log_level(默认 info;RUST_LOG 优先)。
    // tee_stderr: CLI 可;TUI 必须 false(否则日志打坏 alternate screen)。
    crate::logger::init_with_log_level(sw_cfg.debug.log_level.as_deref(), cfg.tee_stderr);

    let weights = sw_cfg.weights.pinyin.to_engine();
    let eng_weights = ime_core::family::english::EnglishWeights {
        exact: sw_cfg.weights.english.exact,
        prefix_ratio: sw_cfg.weights.english.prefix_ratio,
        user_boost: sw_cfg.weights.english.user_boost,
        prefix_base: sw_cfg.weights.english.prefix_base,
        prefix_quality: sw_cfg.weights.english.prefix_quality,
        short_word_penalty: sw_cfg.weights.english.short_word_penalty,
};
    // Mock/TUI: default provider (live $DATE, empty clipboard) + SNIP md 片段
    // + 配置片段(重名后者覆盖)。
    use ime_core::store::snippet_md::SnippetEntry;
    let mut snippets: Vec<SnippetEntry> = {
        let dir = crate::config::seed_snippets_dir();
        ime_core::store::snippet_md::load_dir(&dir)
    };
    snippets.extend(sw_cfg.snippets.iter().map(|s| SnippetEntry {
        name: s.trigger.strip_prefix('/').unwrap_or(&s.trigger).to_string(),
        comment: String::new(),
        params: Vec::new(),
        template: s.expand.clone(),
    }));
    // 配置化 addon 插件命令(`magic.addons`):`#eg/name` → GET {url}/eg/name。
    let addons: Vec<ime_core::family::magic::AddonConfig> = sw_cfg
        .magic
        .addons
        .iter()
        .map(|a| ime_core::family::magic::AddonConfig {
            name: a.name.clone(),
            url: a.url.clone(),
            cmds: a.cmds.clone(),
        })
        .collect();
    let mut engine = ImeEngine::with_config(
        weights,
        eng_weights,
        Box::new(ime_core::family::magic::expander::DefaultProvider),
        snippets,
        sw_cfg.weights.to_scoring(),
        cfg.frontend.clone().unwrap_or_else(|| Arc::new(ime_core::frontend::NoopFrontend::default())),
        cfg.voice_aura_base.clone(),
        cfg.voice_idle_time,
        addons,
        sw_cfg.input.page_size,
    );
    engine.set_context_aware(sw_cfg.input.context_aware);
    // 调试 meta(候选来源/分数)—— view_json / TUI 候选注释据此显示。
    engine.set_candidate_meta(sw_cfg.debug.candidate_meta);

    if sw_cfg.dicts.rime_ice {
        let loader = shared::loader!("assets");
        if let Some(p) = loader.resolve(constants::DICT_RIME_ICE) {
            match engine.load_dict(&p.to_string_lossy()) {
                Ok(n) => crate::ime_log!("loaded {n} dict entries from {}", p.display()),
                Err(e) => tracing::warn!(target: "swift_ime", "dict load error: {e}"),
            }
        }
    }

    // Emoji 家族开关:dicts.emoji: false → 整个家族禁用(无 emoji 候选)。
    if !sw_cfg.dicts.emoji {
        engine.set_family_enabled("emoji", false);
    }

    // Emoji keyword table (CLDR-generated) + user mapping.
    if sw_cfg.dicts.emoji {
        let loader = shared::loader!("assets");
        if let Some(p) = loader.resolve(constants::DICT_EMOJI) {
            match engine.load_emoji_dict(&p.to_string_lossy()) {
                Ok(n) => crate::ime_log!("loaded emoji: {n} keyword rows from {}", p.display()),
                Err(e) => tracing::warn!(target: "swift_ime", "emoji dict load error: {e}"),
            }
        }
    }
    let loader = shared::loader!(".");
    if let Some(p) = loader.resolve(constants::CONF_EMOJI_USER) {
        if p.exists() {
            match engine.load_emoji_user_dict(&p.to_string_lossy()) {
                Ok(n) => crate::ime_log!("loaded {n} emoji user rows from {}", p.display()),
                Err(e) => tracing::warn!(target: "swift_ime", "emoji user dict load error: {e}"),
            }
        }
    }

    // English user dictionary — 与 fcitx5 对齐:学过的英文自生词(en_user)
    // 在 TUI 会话同样可见(此前只有 fcitx5 加载,TUI 学词不回显,不一致)。
    let loader = shared::loader!(".");
    if let Some(p) = loader.resolve(constants::CONF_EN_USER) {
        match engine.load_en_user_dict(&p.to_string_lossy()) {
            Ok(n) => crate::ime_log!("loaded {n} en user words from {}", p.display()),
            Err(e) => tracing::warn!(target: "swift_ime", "en user dict load error: {e}"),
        }
    }

    let voice_state = engine.voice_state();
    if let Some(ref text) = cfg.asr_text {
        // mock:先种状态再冻结(listener 不连 aura、conn 不被覆盖),seed 稳定可见。
        voice_state.set_conn(ime_core::voice_state::VoiceConn::Connected);
        voice_state.set_mock(true);
        voice_state.seed_final(text);
        crate::ime_log!("asr mock text: {text}");
    }

    // `#req` backend base URL — CLI `--req-base` overrides `magic.req_base` config.
    let req_base = cfg.req_base.clone().unwrap_or_else(|| sw_cfg.magic.req_base.clone());
    engine.set_req_base(&req_base);
    crate::ime_log!("#req base: {req_base}");

    // voice listener 由 `ImeEngine::with_config` 内部启动,跟随 engine drop
    // 自动 abort AuraClient。CLI `--voice-aura-base` 覆盖 `voice.aura_base`。

    // SQLite weight store — DATA 命名空间(dev: data/, prod: ~/.desk-pilot/)。
    let data = shared::loader!(".");
    let db = data.resolve(constants::DATA_DB)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| constants::DB_FALLBACK_PATH.into());
    engine.init_store(&db);
    (engine, voice_state)
}

// ── Async wait ─────────────────────────────────────────────────────────

fn wait_for_voice(state: &ime_core::voice_state::SharedVoiceState, timeout_secs: u64) {
    if !state.snapshot().is_empty() { return; }
    let timeout = Duration::from_secs(timeout_secs);
    let start = Instant::now();
    eprintln!("⏳ waiting for aura SSE (up to {timeout_secs}s)...");
    while state.snapshot().is_empty() && start.elapsed() < timeout {
        std::thread::sleep(Duration::from_millis(constants::PROBE_POLL_MS));
    }
    if state.snapshot().is_empty() {
        eprintln!("⏰ timeout — no voice data from aura");
    } else {
        eprintln!("📥 voice data received");
    }
}

// ── Candidate display ──────────────────────────────────────────────────

fn show_candidates_with_async(
    engine: &mut ImeEngine, state: &ime_core::voice_state::SharedVoiceState, input: &str,
    top_n: usize, verbose: bool, async_wait_secs: u64,
) -> Vec<String> {
    for c in input.chars() { engine.predict(KeyEvent::char(c)); }

    let cands = engine.candidates();
    let is_preview = cands.first().is_some_and(|c| c.ends_with("..."));

    if is_preview && async_wait_secs > 0 {
        let timeout = Duration::from_secs(async_wait_secs);
        let start = Instant::now();
        eprintln!("⏳ async wait (up to {async_wait_secs}s) — polling for voice data...");
        while state.snapshot().is_empty() && start.elapsed() < timeout {
            std::thread::sleep(Duration::from_millis(constants::PROBE_POLL_MS));
        }
        let elapsed = start.elapsed();
        if state.snapshot().is_empty() {
            eprintln!("⏰ timeout after {:.1}s — no voice data arrived", elapsed.as_secs_f64());
        } else {
            eprintln!("📥 voice data arrived after {:.1}s", elapsed.as_secs_f64());
        }
        engine.predict(KeyEvent::enter());
        for c in input.chars() { engine.predict(KeyEvent::char(c)); }
    }

    let candidates = engine.candidates();
    if verbose {
        let detailed = engine.candidates_detailed();
        print_candidates_verbose(input, &detailed, top_n);
    } else {
        for c in candidates.iter().take(top_n) {
            let marker = if c.ends_with("...") { "⚡" } else { "" };
            println!("{marker}{c}");
        }
        if candidates.is_empty() { println!("(no candidates)"); }
    }

    engine.predict(KeyEvent::enter());
    candidates
}

fn print_candidates_verbose(input: &str, detailed: &[ime_core::family::RankedCandidate], top_n: usize) {
    println!();
    println!("── {input} ──");
    for (i, d) in detailed.iter().enumerate().take(top_n) {
        let marker = if i == 0 { "★" } else { " " };
        let kind = if d.text.ends_with("...") { "⚡preview" } else { "" };
        println!("  [{:>2}] {} {:<24} {:>5.3}  {}/{}  {}",
            i + 1, marker, d.text, d.score, d.family, d.source, kind);
    }
    if detailed.is_empty() { println!("  (no candidates)"); }
    else if detailed.len() > top_n { println!("  ... {} more", detailed.len() - top_n); }
}

// ── Commit display ─────────────────────────────────────────────────────

fn show_commit(engine: &mut ImeEngine, input: &str, verbose: bool) {
    let mut last_view = ImeView::empty();
    for c in input.chars() { last_view = engine.predict(KeyEvent::char(c)); }

    if verbose {
        let detailed = engine.candidates_detailed();
        println!();
        println!("── {input} (pre-commit) ──");
        for (i, d) in detailed.iter().enumerate() {
            let marker = if i == 0 { "★" } else { " " };
            let kind = if d.text.ends_with("...") { "⚡preview" } else { "" };
            println!("  [{:>2}] {} {:<24}  {:>5.3}  {}/{}  {}",
                i + 1, marker, d.text, d.score, d.family, d.source, kind);
        }
        if detailed.is_empty() { println!("  (no candidates before commit)"); }
    }

    let commit_view = engine.predict(KeyEvent::space());
    let committed = ImeView::str_field(&commit_view.commit_text);
    println!();
    if committed.is_empty() {
        let was_preview = engine.candidates().first().is_some_and(|c| c.ends_with("..."));
        if was_preview { println!("── {input} (commit) → (empty — preview, no voice data yet)"); }
        else { println!("── {input} (commit) → (empty — no voice data or unknown command)"); }
    } else {
        println!("── {input} (commit) → \"{committed}\"");
    }

    if verbose {
        let preedit = ImeView::str_field(&last_view.preedit_text);
        let aux = ImeView::str_field(&last_view.aux_up);
        if !preedit.is_empty() { println!("   preedit: \"{preedit}\""); }
        if !aux.is_empty() && aux != preedit { println!("   aux:     \"{aux}\""); }
    }
}

// ── Batch evaluation ───────────────────────────────────────────────────

struct TestCase { pinyin: String, expected: String }

fn parse_cases(path: &str) -> Vec<TestCase> {
    let content = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read cases file '{path}': {e}"));
    let mut cases = Vec::new();
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') { continue; }
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() >= 2 {
            cases.push(TestCase { pinyin: parts[0].to_string(), expected: parts[1].to_string() });
        }
    }
    cases
}

fn run_cases(engine: &mut ImeEngine, path: &str, verbose: bool) {
    let cases = parse_cases(path);
    let mut total = 0u32; let mut top1 = 0u32; let mut top3 = 0u32; let mut top10 = 0u32;
    if verbose { println!("Evaluating {} test cases...\n", cases.len()); }

    for tc in &cases {
        total += 1;
        for c in tc.pinyin.chars() { engine.predict(KeyEvent::char(c)); }
        let cands = engine.candidates();
        let pos = cands.iter().position(|c| c == &tc.expected);
        if pos == Some(0) { top1 += 1; }
        if pos.is_some_and(|p| p < 3) { top3 += 1; }
        if pos.is_some_and(|p| p < 10) { top10 += 1; }
        if verbose || pos.is_none() {
            let pos_str = pos.map_or("-".to_string(), |p| format!("#{}", p + 1));
            let icon = match pos { Some(0) => "✅", Some(_) => "⚠️", None => "❌" };
            println!("{icon} {:<20} → {:<12}  ({})", tc.pinyin, tc.expected, pos_str);
            if pos.is_none() && verbose {
                let top = &cands[..cands.len().min(5)];
                println!("     got: {:?}", top);
            }
        }
        engine.predict(KeyEvent::enter());
    }

    println!();
    println!("═══════════════════════════════════");
    println!("  Total:     {:>5}", total);
    println!("  Top-1:     {:>5}  ({:.1}%)", top1, top1 as f64 / total as f64 * 100.0);
    println!("  Top-3:     {:>5}  ({:.1}%)", top3, top3 as f64 / total as f64 * 100.0);
    println!("  Top-10:    {:>5}  ({:.1}%)", top10, top10 as f64 / total as f64 * 100.0);
    println!("═══════════════════════════════════");
}


// ═══ TUI rendering(ratatui;engine 经 SharedIme 与 socket 共享)═════════

use std::io;
use std::os::unix::net::UnixListener;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use crossterm::ExecutableCommand;
use ime_core::fsm::state::KeyKind;
use ime_core::voice_state::SharedVoiceState;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::{Frame, Terminal};


/// TUI 前端句柄:引擎 I/O 线程推送刷新时只记一个计数,渲染循环每帧检查并
/// drain(`magic_tick` 拉最新视图)。TUI 是前台应用,自带渲染节奏,不做
/// 剪贴板历史(无 clipboard)。
#[derive(Default)]
pub struct TuiFrontend {
    pending: AtomicU64,
}

impl ime_core::frontend::FrontEndHandle for TuiFrontend {
    fn get_clipboard_item(&self, _count: u32) {}
    fn refresh_ui(&self, _sv: ime_core::frontend::StateView) -> bool {
        self.pending.fetch_add(1, Ordering::Release);
        true // 单上下文,恒"接受"
    }
}

/// server 会话共享状态:engine + 最近视图 + 提交历史 + 脏标记。
/// TUI 线程与 socket 线程都经它喂键(`feed_key`),互斥串行化。
pub struct SharedIme {
    engine: Mutex<ImeEngine>,
    view: Mutex<ImeView>,
    history: Mutex<Vec<String>>,
    /// socket 线程喂过键 → TUI 下个 poll 周期重绘。
    dirty: AtomicBool,
    /// 前端句柄(先于引擎构造注入 cfg,I/O 线程 bump pending;此处共享读)。
    pub frontend: Arc<TuiFrontend>,
}

impl SharedIme {
    fn new(engine: ImeEngine, frontend: Arc<TuiFrontend>) -> Self {
        SharedIme {
            engine: Mutex::new(engine),
            view: Mutex::new(ImeView::empty()),
            history: Mutex::new(Vec::new()),
            dirty: AtomicBool::new(false),
            frontend,
        }
    }

    /// 喂一枚键(TUI / socket 共用):路由 → 记录视图与提交历史 → 标脏。
    fn feed_key(&self, ev: KeyEvent) -> ImeView {
        let v = self.engine.lock().unwrap().key(ev);
        let committed = ImeView::str_field(&v.commit_text);
        if !committed.is_empty() {
            self.history.lock().unwrap().push(committed.to_string());
        }
        *self.view.lock().unwrap() = v;
        self.dirty.store(true, Ordering::Release);
        v
    }

    /// 当前视图快照(socket 响应 / TUI 渲染共用)。
    fn current_view(&self) -> ImeView {
        *self.view.lock().unwrap()
    }

    /// 渲染用元数据(候选来源/分数 + 状态标志),与引擎短暂互斥。
    fn render_meta(&self) -> (Vec<ime_core::family::RankedCandidate>, ime_core::fsm::state::StateFlags) {
        let e = self.engine.lock().unwrap();
        (e.candidates_detailed(), e.state_flags())
    }
}

// ── Server entry(main.rs 默认调用)────────────────────────────────────

/// 调试 server:同一引擎同时监听 TUI 键盘与 Unix socket。
/// `no_tui = true` 时无界面前台运行(无 tty 环境的纯 socket 调试)。
pub fn run_server(mut cfg: TuiConfig, sock_path: Option<String>, no_tui: bool) -> io::Result<()> {
    let sock = sock_path.unwrap_or_else(|| constants::SOCK_PATH.into());
    if !no_tui {
        // TUI 用 alternate screen 渲染,stderr 打日志会破坏界面 → 只写文件。
        cfg.tee_stderr = false;
    }

    // 前端句柄先于引擎构造注入(I/O 线程经它 bump pending,TUI 循环 drain)。
    let frontend = Arc::new(TuiFrontend::default());
    cfg.frontend = Some(Arc::clone(&frontend) as Arc<dyn ime_core::frontend::FrontEndHandle>);
    let (engine, voice_state) = build_engine(&cfg);
    let shared = Arc::new(SharedIme::new(engine, frontend));

    let _ = std::fs::remove_file(&sock); // 旧实例残留
    let listener = UnixListener::bind(&sock)?;
    eprintln!("[swift-ime] debug server on {sock} (TUI: {})", !no_tui);
    {
        let shared = Arc::clone(&shared);
        std::thread::Builder::new()
            .name("swift-sock".into())
            .spawn(move || socket_loop(listener, shared))?;
    }

    let res = if no_tui {
        // 无 TUI:前台挂起,socket 独占调试(Ctrl-C 结束)。
        loop {
            std::thread::sleep(Duration::from_secs(constants::IDLE_TICK_SECS));
        }
    } else {
        run_tui(&shared, &voice_state)
    };
    let _ = std::fs::remove_file(&sock);
    res
}

// ── Socket protocol(swift_cli ↔ server)───────────────────────────────
//
// 请求一行一个命令,响应一行 JSON 视图:
//   ti'an          逐字符喂键(普通串)
//   space|enter|backspace|escape|tab|up|down|left|right|pgup|pgdn|home|end|del|ins|f1..f12
//                  特殊键
//   ctrl:<c>       Ctrl 组合(透传路径验证)
//   view           只取当前视图(不发键)
//   reset          Escape 取消当前组合

/// 单个 token → 键事件(特殊键名表 + 其余逐字符由调用方展开)。
fn key_event_for_token(token: &str) -> Option<KeyEvent> {
    let kind = match token {
        "space" => KeyKind::Space,
        "enter" | "ret" => KeyKind::Enter,
        "backspace" | "bs" => KeyKind::Backspace,
        "escape" | "esc" => KeyKind::Escape,
        "tab" => KeyKind::Tab,
        "up" => KeyKind::Up,
        "down" => KeyKind::Down,
        "left" => KeyKind::Left,
        "right" => KeyKind::Right,
        "pgup" => KeyKind::PageUp,
        "pgdn" => KeyKind::PageDown,
        "home" => KeyKind::Home,
        "end" => KeyKind::End,
        "del" => KeyKind::Delete,
        "ins" => KeyKind::Insert,
        _ if token.len() > 1 && token.starts_with('f')
            && token[1..].chars().all(|c| c.is_ascii_digit()) =>
        {
            KeyKind::Function(token[1..].parse().ok()?)
        }
        _ => return None, // 普通串
    };
    Some(KeyEvent { kind, ctrl: false, shift: false, alt: false })
}

/// ImeView → 一行 JSON(swift_cli 的回包)。
fn view_json(v: &ImeView, history: &[String]) -> serde_json::Value {
    use ime_core::frontend::action;
    let cands: Vec<String> = (0..v.candidate_count as usize)
        .map(|i| ImeView::str_field(&v.candidates[i].text).to_string())
        .collect();
    let metas: Vec<String> = (0..v.candidate_count as usize)
        .map(|i| ImeView::str_field(&v.candidates[i].meta).to_string())
        .filter(|m| !m.is_empty())
        .collect();
    serde_json::json!({
        "handled": v.action & action::HANDLED != 0,
        "passthrough": v.action & action::PASSTHROUGH != 0,
        "commit": ImeView::str_field(&v.commit_text),
        "delete_count": v.delete_count,
        "aux": ImeView::str_field(&v.aux_up),
        "preedit": ImeView::str_field(&v.preedit_text),
        "candidates": cands,
        "meta": metas,
        "highlight": v.candidate_highlight,
        "page": v.candidate_page,
        "page_size": v.candidate_page_size,
        "history_tail": history.iter().rev().take(3).collect::<Vec<_>>(),
    })
}

/// socket 接受循环:每连接逐行命令 → 喂键 → 回 JSON 视图。
fn socket_loop(listener: UnixListener, shared: Arc<SharedIme>) {
    use std::io::{BufRead, BufReader, Write};
    for conn in listener.incoming() {
        let Ok(stream) = conn else { continue };
        let Ok(mut writer) = stream.try_clone() else { continue };
        let reader = BufReader::new(stream);
        for line in reader.lines() {
            let Ok(line) = line else { break }; // 对端关闭
            let cmd = line.trim();
            if cmd.is_empty() {
                continue;
            }
            let resp = match cmd {
                "view" => view_json(&shared.current_view(), &shared.history.lock().unwrap()),
                "reset" => {
                    shared.feed_key(KeyEvent::escape());
                    view_json(&shared.current_view(), &shared.history.lock().unwrap())
                }
                _ => {
                    if let Some(arg) = cmd.strip_prefix("ctrl:") {
                        if let Some(ch) = arg.chars().next() {
                            let mut ev = KeyEvent::char(ch);
                            ev.ctrl = true;
                            shared.feed_key(ev);
                        }
                    } else if let Some(ev) = key_event_for_token(cmd) {
                        shared.feed_key(ev);
                    } else {
                        // 普通串:逐字符(经 KeyEvent::char 归一化:数字/符号成类)。
                        for ch in cmd.chars() {
                            shared.feed_key(KeyEvent::char(ch));
                        }
                    }
                    view_json(&shared.current_view(), &shared.history.lock().unwrap())
                }
            };
            let _ = writeln!(writer, "{resp}");
            let _ = writer.flush();
        }
    }
}

// ── TUI rendering loop(共享引擎)───────────────────────────────────────

/// TUI 主循环(engine 由 socket 共享)。Ctrl+Q / Ctrl+C 退出。
fn run_tui(shared: &Arc<SharedIme>, voice_state: &SharedVoiceState) -> io::Result<()> {
    enable_raw_mode()?;
    io::stdout().execute(crossterm::terminal::EnterAlternateScreen)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    let res = tui_loop(&mut terminal, shared, voice_state);
    disable_raw_mode()?;
    io::stdout().execute(crossterm::terminal::LeaveAlternateScreen)?;
    res
}

fn tui_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    shared: &Arc<SharedIme>,
    voice_state: &SharedVoiceState,
) -> io::Result<()> {
    draw_once(terminal, shared, voice_state)?;

    let mut should_quit = false;
    while !should_quit {
        let mut dirty = false;

        // 键盘(最多等 POLL_MS,兼作渲染节拍)。
        if event::poll(Duration::from_millis(POLL_MS))? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    let ev = crossterm_to_key(&key);
                    // TUI 自身是"应用":引擎对 Ctrl 组合返回 PASSTHROUGH,
                    // Ctrl+Q / Ctrl+C 在此退出。
                    if ev.ctrl && matches!(ev.kind, KeyKind::Char('q') | KeyKind::Char('c')) {
                        should_quit = true;
                    } else {
                        shared.feed_key(ev);
                        dirty = true;
                    }
                }
            }
        }

        // 引擎 I/O 线程推送过刷新(voice/req 异步推进)→ 拉最新 live 视图。
        if shared.frontend.pending.swap(0, Ordering::AcqRel) > 0 {
            if let Some(v) = shared.engine.lock().unwrap().magic_tick() {
                *shared.view.lock().unwrap() = v;
            }
            dirty = true;
        }

        // socket 线程喂过键 → 重绘(TUI 与 CLI 看到同一状态)。
        if shared.dirty.swap(false, Ordering::AcqRel) {
            dirty = true;
        }

        if dirty {
            draw_once(terminal, shared, voice_state)?;
        }
    }
    Ok(())
}

fn draw_once(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    shared: &Arc<SharedIme>,
    voice_state: &SharedVoiceState,
) -> io::Result<()> {
    let view = shared.current_view();
    let (detailed, flags) = shared.render_meta();
    let history = shared.history.lock().unwrap().clone();
    terminal.draw(|f| render(f, &view, &history, voice_state, &detailed, flags))?;
    Ok(())
}

/// crossterm 事件 → 统一键事件(忠实转换:键类 + Ctrl/Shift/Alt 状态)。
fn crossterm_to_key(key: &crossterm::event::KeyEvent) -> KeyEvent {
    let kind = match key.code {
        KeyCode::Char(c) => KeyEvent::char(c).kind,
        KeyCode::Enter => KeyKind::Enter,
        KeyCode::Backspace => KeyKind::Backspace,
        KeyCode::Esc => KeyKind::Escape,
        KeyCode::Tab => KeyKind::Tab,
        KeyCode::Up => KeyKind::Up,
        KeyCode::Down => KeyKind::Down,
        KeyCode::Left => KeyKind::Left,
        KeyCode::Right => KeyKind::Right,
        KeyCode::PageUp => KeyKind::PageUp,
        KeyCode::PageDown => KeyKind::PageDown,
        KeyCode::Home => KeyKind::Home,
        KeyCode::End => KeyKind::End,
        KeyCode::Delete => KeyKind::Delete,
        KeyCode::Insert => KeyKind::Insert,
        KeyCode::F(n) => KeyKind::Function(n),
        _ => KeyKind::Other(0),
    };
    let m = key.modifiers;
    KeyEvent {
        kind,
        ctrl: m.contains(KeyModifiers::CONTROL),
        shift: m.contains(KeyModifiers::SHIFT),
        alt: m.contains(KeyModifiers::ALT),
    }
}

fn render(
    f: &mut Frame,
    view: &ImeView,
    history: &[String],
    voice_state: &SharedVoiceState,
    detailed: &[ime_core::family::RankedCandidate],
    flags: ime_core::fsm::state::StateFlags,
) {
    let area = f.area();
    let rows = Layout::vertical([
        Constraint::Length(3),
        Constraint::Length(10),
        Constraint::Min(3),
        Constraint::Length(1),
    ])
    .split(area);

    render_preedit(f, rows[0], view);
    render_candidates(f, rows[1], view, detailed);
    render_history(f, rows[2], history);
    render_status(f, rows[3], voice_state, flags);
}

fn render_preedit(f: &mut Frame, area: Rect, view: &ImeView) {
    let raw = ImeView::str_field(&view.aux_up);
    let result = ImeView::str_field(&view.preedit_text);
    let mut spans: Vec<Span> = Vec::new();
    if !raw.is_empty() {
        spans.push(Span::styled(format!("输入: {raw} "), Style::new().fg(Color::DarkGray)));
    }
    if !result.is_empty() && result != raw {
        spans.push(Span::styled(format!("→ 提交: {result}"), Style::new().fg(Color::Cyan)));
    }
    let text = if spans.is_empty() { " ".into() } else { Line::from(spans) };
    let p = Paragraph::new(text)
        .block(Block::new().borders(Borders::ALL).title("Input"));
    f.render_widget(p, area);
}

fn render_candidates(
    f: &mut Frame,
    area: Rect,
    view: &ImeView,
    detailed: &[ime_core::family::RankedCandidate],
) {
    let mut lines: Vec<Line> = Vec::new();
    let page = view.candidate_page as usize;
    let page_size = view.candidate_page_size.max(1) as usize;
    let start = page * page_size;
    let end = (start + page_size).min(view.candidate_count as usize);

    for i in start..end {
        let label = format!("{}.", (i % page_size) + 1);
        let text = ImeView::str_field(&view.candidates[i].text);
        let label_text = ImeView::str_field(&view.candidates[i].label);
        let is_hl = i == view.candidate_highlight as usize;

        let style = if is_hl {
            Style::new().fg(Color::Black).bg(Color::Cyan).add_modifier(Modifier::BOLD)
        } else if text.ends_with("...") {
            Style::new().fg(Color::Yellow)
        } else {
            Style::new().fg(Color::White)
        };

        let prefix = if label_text.is_empty() { String::new() } else { format!("{label_text} ") };
        let detail = detailed.iter().find(|d| d.text == text);
        let mut spans = vec![
            Span::styled(format!("{label} "), Style::new().fg(Color::DarkGray)),
            Span::styled(format!("{prefix}{text}"), style),
        ];
        if let Some(d) = detail {
            spans.push(Span::styled(
                format!("  [{:.3} {}/{}]", d.score, d.family, d.source),
                Style::new().fg(Color::DarkGray),
            ));
        }
        lines.push(Line::from(spans));
    }

    if lines.is_empty() {
        lines.push(Line::from(Span::styled("(no candidates)", Style::new().fg(Color::DarkGray))));
    }

    let total = if view.candidate_count > 0 {
        (view.candidate_count as usize).div_ceil(page_size)
    } else { 1 };
    let title = format!("Candidates (page {}/{total})", page + 1);

    let p = Paragraph::new(lines).block(Block::new().borders(Borders::ALL).title(title));
    f.render_widget(p, area);
}

fn render_history(f: &mut Frame, area: Rect, history: &[String]) {
    let text = history.join("");
    let p = Paragraph::new(text)
        .block(Block::new().borders(Borders::ALL).title("Committed"))
        .style(Style::new().fg(Color::Gray));
    f.render_widget(p, area);
}

fn render_status(
    f: &mut Frame,
    area: Rect,
    voice_state: &SharedVoiceState,
    flags: ime_core::fsm::state::StateFlags,
) {
    let voice = voice_state.snapshot();
    let vs = if voice.is_empty() {
        "ASR: idle".into()
    } else {
        format!("ASR: {}", ime_core::family::magic::preview_text(&voice, 30))
    };
    let aura = if voice_state.is_connected() {
        Span::styled(" aura:✓ ", Style::new().fg(Color::Green).add_modifier(Modifier::BOLD))
    } else {
        Span::styled(" aura:✗ ", Style::new().fg(Color::Red))
    };
    let flags_str = if flags.labels().is_empty() {
        "IDLE".to_string()
    } else {
        flags.labels().join("|")
    };
    let line = Line::from(vec![
        Span::styled(" Ctrl+Q:quit ", Style::new().fg(Color::DarkGray)),
        Span::styled(" Esc:cancel ", Style::new().fg(Color::DarkGray)),
        Span::styled(" Space:commit ", Style::new().fg(Color::Green)),
        Span::styled(" ↑↓←→:nav ", Style::new().fg(Color::DarkGray)),
        Span::styled(" PgUp/Dn:page ", Style::new().fg(Color::DarkGray)),
        Span::styled(" 1-9:select ", Style::new().fg(Color::DarkGray)),
        Span::styled(format!(" [{flags_str}]"), Style::new().fg(Color::Blue)),
        aura,
        Span::styled(format!(" | {vs}"), Style::new().fg(Color::Gray)),
    ]);
    f.render_widget(Paragraph::new(line), area);
}
