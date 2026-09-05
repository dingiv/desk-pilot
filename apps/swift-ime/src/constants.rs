//! swift-ime 全局常量 — FileLoader 命名空间键、兜底路径与共享时序参数。
//!
//! 原则:同一资源的路径/键名**全仓只出现一次**(本文件)。前端(tui/fcitx5)、
//! CLI(logger/swift_cli)一律引用这里的常量;新增资源先在这里登记。
//!
//! 命名空间语义见 shared::loader:`DICT::`(assets/)、`CONF::`(conf/)、
//! `DATA::`(dev: data/,prod: ~/.desk-pilot/)。

// ── FileLoader 命名空间键 ──────────────────────────────────────────────

/// rime-ice 全拼 FST 词典(assets/dict/rime/,build_dict.rs 编译产物)。
pub const DICT_RIME_ICE: &str = "DICT::rime/rime-ice.fst";
/// 英文 base 词表(hermitdave en_freq.tsv,assets/dict/hermitdave/)。
pub const DICT_EN_FREQ: &str = "DICT::hermitdave/en_freq.tsv";
/// Emoji 关键词表(CLDR 生成,assets/dict/emoji/)。
pub const DICT_EMOJI: &str = "DICT::emoji/emoji.tsv";
/// Emoji 用户自定义映射(conf)。
pub const CONF_EMOJI_USER: &str = "CONF::emoji_user.tsv";
/// 英文自生词用户词典(conf)。
pub const CONF_EN_USER: &str = "CONF::en_user.tsv";
/// SQLite 权重库(recency / phrase / L0 / en_user 持久化)。
pub const DATA_DB: &str = "DATA::swift-ime.db";
/// 引擎与前端统一日志文件。
pub const DATA_LOG: &str = "DATA::swift-ime.log";
/// 主配置(默认查找键,CLI `--config` 可覆盖)。
pub const CONF_MAIN: &str = "CONF::swift-ime.yaml";

// ── FileLoader 解析失败时的兜底路径(dev / 裸环境)────────────────────

/// DB 兜底:相对工作目录(dev 工作流)。
pub const DB_FALLBACK_PATH: &str = "data/swift-ime.db";
/// 日志兜底:/tmp(无 DATA 命名空间时仍可写日志)。
pub const LOG_FALLBACK_PATH: &str = "/tmp/swift-ime.log";

// ── IPC socket(tui --socket 调试服务)───────────────────────────────

pub const SOCK_PATH: &str = "/tmp/swift-ime.sock";

// ── 共享时序 ───────────────────────────────────────────────────────────

/// 探针/等待循环的轮询间隔(ms)—— voice/async 就绪等待的节拍。
pub const PROBE_POLL_MS: u64 = 200;
/// TUI 空闲 tick(秒)—— 无事件时的保底唤醒(重绘/信号检查)。
pub const IDLE_TICK_SECS: u64 = 3600;
