//! dp-router 配置 schema (yaml).
//!
//! 默认配置位置由 `shared::loader!()` 解析 — dev: `apps/dp-router/dp-router.yaml`,
//! prod: `~/.desk-pilot/dp-router.yaml`。

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// 顶层配置。
#[derive(Debug, Clone, Deserialize)]
pub struct RouterConfig {
    pub server: ServerConfig,
    /// `llama-server` 二进制路径 + 端口分配策略(子进程管理用)。
    pub llama_server: LlamaServerConfig,
    /// 启动时预加载的本地模型列表(每个 spawn 一个 `llama-server` 子进程)。
    #[serde(default)]
    pub models: Vec<LocalModelConfig>,
    /// 远程 OpenAI 兼容上游(未命中本地时 fallback)。
    /// `base_url` 为空 → 未命中返 404。
    #[serde(default)]
    pub remote_upstream: RemoteUpstreamConfig,
    /// 模型仓库根目录(`/admin/models/load` 按名搜索 GGUF 用)。
    /// 支持 `MODELS::` 命名空间。缺省 → 动态加载必须显式传 `gguf` 路径。
    #[serde(default)]
    pub models_root: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ServerConfig {
    /// dp-router 对外监听地址(默认 :8080)。
    pub addr: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LlamaServerConfig {
    /// `llama-server` 二进制绝对路径。也可以用 `which llama-server` 解析系统 PATH 中的。
    pub path: PathBuf,
    /// 子进程端口分配区间 `[from, to]`。默认 18001-18099(避开常见应用端口段)。
    #[serde(default = "default_port_range")]
    pub port_range: [u16; 2],
    /// 健康检查间隔(秒)。子进程 HTTP `/health` 不通则触发重启。
    #[serde(default = "default_health_check_interval_s")]
    pub health_check_interval_s: u64,
    /// 单次重启最大重试次数,超过则放弃并标记 offline。
    #[serde(default = "default_restart_max_retries")]
    pub restart_max_retries: u32,
    /// 重启退避基数(秒),实际等待 = `base * 2^attempt`。
    #[serde(default = "default_restart_backoff_base_s")]
    pub restart_backoff_base_s: u64,
}

fn default_port_range() -> [u16; 2] { [18001, 18099] }
fn default_health_check_interval_s() -> u64 { 5 }
fn default_restart_max_retries() -> u32 { 3 }
fn default_restart_backoff_base_s() -> u64 { 1 }

/// 一个本地模型声明。
#[derive(Debug, Clone, Deserialize)]
pub struct LocalModelConfig {
    /// 对外暴露的模型名(请求里 `model` 字段命中此值 → 转发到此子进程)。
    /// 习惯上等于 GGUF 文件名去后缀(`qwen2.5-3b-instruct-q4_k_m.gguf` →
    /// `qwen2.5-3b-instruct-q4_k_m`),但不强求。
    pub name: String,
    /// 模型类型:`llm`(文本生成,默认)| `asr`(语音转写)。
    /// 决定路由语义 + 是否需要 mmproj(见下)。
    #[serde(default)]
    pub r#type: ModelType,
    /// GGUF 文件路径。支持 `MODELS::xxx.gguf` 命名空间(dev: `assets/models/xxx.gguf`,
    /// prod: `~/.desk-pilot/models/xxx.gguf`)与绝对路径。
    pub gguf: String,
    /// 多模态投影器路径(`type: asr` 必配;传给 llama-server `--mmproj`,
    /// 加载后自动启用 `/v1/audio/transcriptions` 端点)。支持 `MODELS::` 命名空间。
    #[serde(default)]
    pub mmproj: Option<String>,
    /// 上下文长度(传给 `-c`)。默认 4096。
    #[serde(default = "default_context_size")]
    pub context_size: u32,
    /// 推理线程数(传给 `--threads`)。默认 8。
    #[serde(default = "default_threads")]
    pub threads: u32,
    /// GPU 卸载层数(传给 `-ngl`)。CPU 环境 = 0(默认)。
    #[serde(default)]
    pub gpu_layers: u32,
    /// 批大小(传给 `-b`)。默认 512。
    #[serde(default = "default_batch_size")]
    pub batch_size: u32,
    /// 额外参数(命令行原始附加)。高级用户自定义。
    #[serde(default)]
    pub extra_args: Vec<String>,
}

/// 本地模型类型(`LocalModelConfig.r#type`)。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ModelType {
    /// 文本生成(/v1/chat/completions 路由)。
    #[default]
    Llm,
    /// 语音转写(/v1/audio/transcriptions 路由;spawn 时需 `--mmproj`)。
    Asr,
}

fn default_context_size() -> u32 { 4096 }
fn default_threads() -> u32 { 8 }
fn default_batch_size() -> u32 { 512 }

/// 远程 OpenAI 兼容上游(`base_url` 空 → 不转发,未命中本地直接 404)。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct RemoteUpstreamConfig {
    pub base_url: String,
    /// 透传的 bearer token(可选)。
    #[serde(default)]
    pub api_key: Option<String>,
    /// 缺省 `model` 字段时的兜底模型名(可选)。
    #[serde(default)]
    pub default_model: Option<String>,
}

/// 解析 GGUF 路径:支持 `MODELS::xxx` 命名空间 + 绝对/相对路径。
pub fn resolve_gguf_path(raw: &str) -> anyhow::Result<PathBuf> {
    if let Some(rest) = raw.strip_prefix("MODELS::") {
        let p = shared::loader!()
            .resolve(rest)
            .ok_or_else(|| anyhow::anyhow!("MODELS 命名空间解析失败: {rest}"))?;
        Ok(p)
    } else {
        Ok(PathBuf::from(raw))
    }
}

/// 在 `models_root` 下按模型名搜索 GGUF(`/admin/models/load` 用)。
/// 匹配优先级:精确 `<root>/**/<name>.gguf` → 文件名含 `<name>` 的首个 .gguf
/// (递归遍历,深度限 4;排序保证确定性)。
pub fn resolve_model_in_root(name: &str, root: &Path) -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    walk_ggufs(root, 0, &mut candidates);
    candidates.sort();
    // 1) 文件 stem 与 name 完全一致
    if let Some(exact) = candidates
        .iter()
        .find(|p| p.file_stem().is_some_and(|s| s == name))
    {
        return Some(exact.clone());
    }
    // 2) 文件名包含 name(如 name=qwen3-asr 命中 qwen3-asr-1.7b-q4_k_m.gguf)
    candidates
        .into_iter()
        .find(|p| p.file_name().is_some_and(|f| f.to_string_lossy().contains(name)))
}

/// 深度受限的递归 .gguf 收集(符号链接不跟)。
fn walk_ggufs(dir: &Path, depth: u8, out: &mut Vec<PathBuf>) {
    if depth > 4 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk_ggufs(&path, depth + 1, out);
        } else if path.extension().is_some_and(|e| e == "gguf") {
            // mmproj 投影器也是 .gguf 后缀,按文件名含 mmproj 排除
            let is_mmproj = path
                .file_name()
                .is_some_and(|f| f.to_string_lossy().to_lowercase().contains("mmproj"));
            if !is_mmproj {
                out.push(path);
            }
        }
    }
}

/// 用于 `/admin/models` 的运行时快照。
#[derive(Debug, Clone, Serialize)]
pub struct LocalModelStatus {
    pub name: String,
    pub gguf: String,
    pub port: u16,
    pub status: ModelRuntimeStatus,
    pub restarts: u32,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ModelRuntimeStatus {
    Online,
    Offline,
    Starting,
}
