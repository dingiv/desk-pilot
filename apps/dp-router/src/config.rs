//! dp-router 配置 schema (yaml).
//!
//! 默认配置位置由 `shared::loader!()` 解析 — dev: `apps/dp-router/dp-router.yaml`,
//! prod: `~/.desk-pilot/dp-router.yaml`。

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// 顶层配置。
#[derive(Debug, Clone, Deserialize)]
pub struct RouterConfig {
    pub server: ServerConfig,
    /// `llama-server` 二进制路径 + 端口分配策略(子进程管理用)。
    pub llama_server: LlamaServerConfig,
    /// 模型目录:声明"哪些模型可用 + 各自的启动参数"。**启动时一个都不加载** —
    /// 客户端首次请求某模型名时才按需拉起对应 `llama-server` 子进程。
    #[serde(default)]
    pub models: Vec<LocalModelConfig>,
    /// 远程 OpenAI 兼容上游(未命中本地时 fallback)。
    /// `base_url` 为空 → 未命中返 404。
    #[serde(default)]
    pub remote_upstream: RemoteUpstreamConfig,
    /// 模型仓库根目录(`/admin/models/load` 按名搜索 GGUF 用;`gguf`/`mmproj` 的
    /// 相对路径锚点)。支持 `MODELS::` 命名空间。缺省 → 模型路径必须写绝对路径。
    #[serde(default)]
    pub models_root: Option<String>,
    /// `models_root` 解析后的绝对路径(由 [`Self::resolve_paths`] 填充,非配置项)。
    #[serde(default)]
    pub models_root_resolved: Option<PathBuf>,
}

impl RouterConfig {
    /// 把所有路径类字段解析成绝对路径(就地)。在反序列化 + CLI 覆盖之后调用一次:
    ///   - `llama_server.path` → [`resolve_binary_path`](裸命令名搜 $PATH / `LLAMA::` / 绝对)
    ///   - `models_root`       → [`resolve_model_path`]
    ///   - 每个 `models[].gguf` / `mmproj` → [`resolve_model_path`](相对路径拼到 `models_root`)
    ///
    /// 归一化后,下游(process 重启、admin 端点)拿到的都是绝对路径,无需再解析。
    pub fn resolve_paths(&mut self) -> anyhow::Result<()> {
        let root = self
            .models_root
            .as_deref()
            .map(|r| resolve_model_path(r, None))
            .transpose()?;
        self.models_root_resolved = root.clone();
        let path_str = self.llama_server.path.to_string_lossy().into_owned();
        self.llama_server.path = resolve_binary_path(&path_str)?;
        for m in &mut self.models {
            m.gguf = resolve_model_path(&m.gguf, root.as_deref())?
                .to_string_lossy()
                .into_owned();
            if let Some(mm) = m.mmproj.clone() {
                m.mmproj = Some(
                    resolve_model_path(&mm, root.as_deref())?
                        .to_string_lossy()
                        .into_owned(),
                );
            }
        }
        Ok(())
    }
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
    /// 子进程环境变量(传给所有 llama-server 子进程)。
    /// ROCm / AMD 核显场景常用:
    ///   `HSA_OVERRIDE_GFX_VERSION: "10.3.0"` — 旧核显(GFX10/11)HSA 兼容
    ///   `ROCR_VISIBLE_DEVICES: "0"`           — 指定 ROCm 设备编号
    ///   `HSA_FORCE_FINE_GRAIN_HEAP: "1"`      — 核显共享内存场景
    #[serde(default)]
    pub env: HashMap<String, String>,
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
    /// GGUF 文件路径。推荐写**相对 `models_root`** 的路径(如
    /// `qwen2.5-3b-instruct-q4_k_m.gguf`),加载时拼成绝对路径;也支持
    /// `MODELS::xxx.gguf` 命名空间与绝对路径显式覆盖。
    pub gguf: String,
    /// 多模态投影器路径(`type: asr` 必配;传给 llama-server `--mmproj`,
    /// 加载后自动启用 `/v1/audio/transcriptions` 端点)。同 `gguf`:相对
    /// `models_root` 或 `MODELS::` / 绝对路径。
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

/// 解析"已知"路径形态(命名空间 / 绝对 / `~` 家目录)→ 绝对 [`PathBuf`]。
/// 裸相对路径返回 `None`,由调用方决定拼到哪个根上(见 [`resolve_model_path`] /
/// [`resolve_binary_path`])。
fn resolve_known(raw: &str) -> Option<PathBuf> {
    if raw.contains("::") {
        return shared::loader!().resolve(raw);
    }
    if raw == "~" || raw.starts_with("~/") {
        return Some(shared::expand_tilde(raw));
    }
    let p = Path::new(raw);
    if p.is_absolute() {
        return Some(p.to_path_buf());
    }
    None
}

/// 在 `$PATH` 中查找可执行文件。
fn search_path(name: &str) -> Option<PathBuf> {
    let paths = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&paths) {
        let cand = dir.join(name);
        if cand.is_file() {
            return Some(cand);
        }
    }
    None
}

/// 解析 `llama_server.path`(可执行文件)。支持四种写法:
///   - `LLAMA::llama-server` → 命名空间(dev 指仓库构建,prod 指系统安装位置)
///   - `/abs` 或 `~/x`       → 原样
///   - `llama-server`(不含 `/`)→ 搜 `$PATH`
///   - `rel/x`               → 相对当前工作目录
pub fn resolve_binary_path(raw: &str) -> anyhow::Result<PathBuf> {
    let raw = raw.trim();
    if raw.is_empty() {
        anyhow::bail!("llama_server.path 为空");
    }
    if let Some(p) = resolve_known(raw) {
        return Ok(p);
    }
    if !raw.contains('/') {
        return search_path(raw).ok_or_else(|| {
            anyhow::anyhow!(
                "'{raw}' 不在 $PATH 中(可写绝对路径,或用 LLAMA:: 命名空间)"
            )
        });
    }
    Ok(PathBuf::from(raw))
}

/// 解析模型文件路径(`gguf` / `mmproj`)。支持:
///   - `MODELS::x.gguf` / `/abs` / `~/x` → 原样([`resolve_known`])
///   - `x.gguf` / `sub/x.gguf`(相对)→ 拼到 `models_root` 下
pub fn resolve_model_path(raw: &str, models_root: Option<&Path>) -> anyhow::Result<PathBuf> {
    let raw = raw.trim();
    if let Some(p) = resolve_known(raw) {
        return Ok(p);
    }
    match models_root {
        Some(root) => Ok(root.join(raw)),
        None => anyhow::bail!(
            "模型路径 '{raw}' 是相对路径但未配置 models_root;\
             请写绝对路径、用 MODELS:: 命名空间,或配置 models_root"
        ),
    }
}

/// 解析 GGUF 路径:命名空间 / 绝对 / `~` 原样,裸相对 → 相对 cwd(兼容旧调用点;
/// 加载时归一化后 gguf 已是绝对路径,这里是直通)。
pub fn resolve_gguf_path(raw: &str) -> anyhow::Result<PathBuf> {
    Ok(resolve_known(raw).unwrap_or_else(|| PathBuf::from(raw)))
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
    /// 在配置目录中但尚未加载(仅 /admin/models 展示用;首次请求即拉起)。
    Available,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn resolve_model_path_relative_joins_root() {
        let root = Path::new("/home/u/.desk-pilot/models");
        assert_eq!(
            resolve_model_path("qwen.gguf", Some(root)).unwrap(),
            root.join("qwen.gguf")
        );
        assert_eq!(
            resolve_model_path("sub/m.gguf", Some(root)).unwrap(),
            root.join("sub/m.gguf")
        );
    }

    #[test]
    fn resolve_model_path_absolute_passthrough() {
        assert_eq!(
            resolve_model_path("/abs/x.gguf", None).unwrap(),
            PathBuf::from("/abs/x.gguf")
        );
    }

    #[test]
    fn resolve_model_path_relative_without_root_errors() {
        let err = resolve_model_path("x.gguf", None).unwrap_err().to_string();
        assert!(err.contains("models_root"), "{err}");
    }

    #[test]
    fn resolve_binary_path_absolute_passthrough() {
        assert_eq!(
            resolve_binary_path("/usr/local/bin/llama-server").unwrap(),
            PathBuf::from("/usr/local/bin/llama-server")
        );
    }

    #[test]
    fn resolve_binary_path_bare_name_searches_path() {
        // `sh` is always on PATH.
        let p = resolve_binary_path("sh").unwrap();
        assert!(p.is_file(), "expected an existing file, got {p:?}");
    }

    #[test]
    fn resolve_binary_path_unknown_bare_name_errors() {
        let err = resolve_binary_path("definitely-not-a-real-bin-xyz").unwrap_err().to_string();
        assert!(err.contains("$PATH"), "{err}");
    }

    #[test]
    fn resolve_binary_path_empty_errors() {
        assert!(resolve_binary_path("   ").is_err());
    }

    #[test]
    fn resolve_gguf_path_passthrough_and_relative() {
        assert_eq!(
            resolve_gguf_path("/abs/x.gguf").unwrap(),
            PathBuf::from("/abs/x.gguf")
        );
        // bare relative → cwd-relative (legacy behavior)
        assert_eq!(resolve_gguf_path("rel/x.gguf").unwrap(), PathBuf::from("rel/x.gguf"));
    }

    #[test]
    fn resolve_paths_normalizes_to_absolute() {
        let yaml = r#"
server: { addr: "127.0.0.1:8080" }
llama_server: { path: "/abs/bin/llama-server" }
models_root: "/abs/models"
models:
  - name: m1
    gguf: "m1.gguf"
  - name: m2
    type: asr
    gguf: "sub/m2.gguf"
    mmproj: "sub/m2-mmproj.gguf"
"#;
        let mut cfg: RouterConfig = serde_yaml::from_str(yaml).unwrap();
        cfg.resolve_paths().unwrap();
        assert_eq!(cfg.llama_server.path, PathBuf::from("/abs/bin/llama-server"));
        assert_eq!(cfg.models_root_resolved, Some(PathBuf::from("/abs/models")));
        assert_eq!(cfg.models[0].gguf, "/abs/models/m1.gguf");
        assert_eq!(cfg.models[1].gguf, "/abs/models/sub/m2.gguf");
        assert_eq!(cfg.models[1].mmproj.as_deref(), Some("/abs/models/sub/m2-mmproj.gguf"));
    }
}
