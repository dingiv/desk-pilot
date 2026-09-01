//! spec — 选型描述(daemon resolve() 产出,assemble() 消费):纯数据,零逻辑。
//! 分层:daemon 负责"从哪儿读配置"(yaml/json/CLI/默认值),这里只认
//! fully-resolved 的具体值 —— 线协议/文件格式不进 core。
//! VadSpec::default 与 Stage1Config::new 的内置默认一致(单测钉死,防两处漂移)。
//!(唯一外部类型引用:LlmInput,calibrator 的输入源旋钮。)

use crate::pipeline::calibrator::LlmInput;

// 具体值 —— 线协议/文件格式不进 core。VadSpec::default 与 Stage1Config::new 的内置
// 默认一致(单测钉死,防两处漂移)。

/// Fully-resolved pipeline 选型:音频源、种子热词、VAD/分句参数、流式 ASR、Stage1 batch
/// ASR、Stage2 LLM。[`Pipeline::assemble`] 的唯一输入(运行时共享句柄除外)。
#[derive(Debug, Clone, PartialEq)]
pub struct PipelineSpec {
    /// omni-scout `/audio` 地址。
    pub scout_addr: String,
    /// 客户端请求 scout 的推流 cadence(ms):`?chunk_ms=N` 让 scout 把源 buffer 聚合成
    /// N-ms 的 HTTP chunk 再推(不能快过 scout 的 node.quantum)。None = 不传参,scout
    /// 按自身 quantum 速率推。纯网络层优化——daemon 侧照样重切成 32ms 窗。
    pub scout_chunk_ms: Option<u64>,
    /// 种子热词:烘烤进流式 recognizer(beam bias),并预载 Stage2 共享 store。
    pub hotwords: Vec<String>,
    pub vad: VadSpec,
    pub stream: StreamSpec,
    pub asr: AsrSpec,
    pub llm: LlmSpec,
    /// Stage2 纠偏的输入源（`llm.input`）：batch（默认）| stream | both。
    pub llm_input: LlmInput,
}

/// 流式 ASR 选型(**恒本地** —— 实时 partial 要低延迟,不走 remote)。当前唯一引擎
/// zipformer;新引擎落地时在 [`stage1_config`] 的 match 里扩臂。
#[derive(Debug, Clone, PartialEq)]
pub struct StreamSpec {
    /// "zipformer" (当前唯一;未知值 assemble 直接报错)。
    pub model: String,
}

/// VAD/分句参数(具体值)。[`Default`] 与 [`Stage1Config::new`] 的内置默认逐字段一致。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VadSpec {
    /// Silero speech-probability threshold(0.5)。高=不敏感,低=易误触。
    pub threshold: f32,
    /// 切句间隔秒(1.0)——短于此的停顿不切句。
    pub min_silence: f32,
    /// 短于该时长的句被 Silero 丢弃(0.3)。
    pub min_speech: f32,
    /// 超长强切兜底秒(28.0)。
    pub max_speech: f32,
    /// ★merge 段落间隔秒(5.0)——"什么算一句话"的上界;0 = 每句独立成窗。
    pub merge_gap: f64,
    /// 句边界扩展秒(0.3;0=off)——补 Silero 切掉的软起音/尾音。
    pub edge_margin: f32,
}

impl Default for VadSpec {
    fn default() -> Self {
        VadSpec {
            threshold: 0.5,
            min_silence: 1.0,
            min_speech: 0.3,
            max_speech: 28.0,
            merge_gap: 5.0,
            edge_margin: 0.3,
        }
    }
}

/// Stage1 batch ASR 选型。流式 ASR + VAD 恒为本地 sherpa(实时 partial 要低延迟),
/// 这里只选 batch 通道。
#[derive(Debug, Clone, PartialEq)]
pub enum AsrSpec {
    /// 本地 ONNX:backend "sensevoice"(默认) | "whisper" | "qwen3-asr";
    /// hardware "cpu"(默认) | "cuda"(仅 batch;cuDNN 9.25+);threads = intra-op 并行;
    /// model_dir = 模型根目录覆盖(None → MODELS 命名空间,含流式/VAD 路径)。
    Local {
        backend: String,
        language: String,
        hardware: String,
        threads: i32,
        model_dir: Option<String>,
    },
    /// 远程 HTTP(OpenAI 兼容 `/v1/audio/transcriptions`)。`endpoint` = base URL,
    /// `model` = 服务端模型名(必须与 dp-router.yaml `models[].name` 对齐;OpenAI 规范
    /// 要求 multipart form 里带 `model` 字段)。流式/VAD 仍走 MODELS 命名空间。
    Remote { endpoint: String, model: String },
    /// 批式整体禁用(纯流式模式):不加载批式模型,`batch_text` 恒 `None` —— 消费方
    /// 按设计回退流式文本。省掉句级/段落级 batch 调用(远程 ~3.5s/次)。
    Disabled,
}

impl AsrSpec {
    /// "local" | "remote" | "disabled" — 配置快照(ConfigView)的显示标签。
    pub fn kind(&self) -> &'static str {
        match self {
            AsrSpec::Local { .. } => "local",
            AsrSpec::Remote { .. } => "remote",
            AsrSpec::Disabled => "disabled",
        }
    }
}

/// Stage2 LLM 选型。
#[derive(Debug, Clone, PartialEq)]
pub enum LlmSpec {
    /// 远程 HTTP(OpenAI 兼容 `/v1/chat/completions`,目标为 dp-router 或 vLLM / SGLang / 任意
    /// OpenAI 兼容服务)。`model` = 服务端模型名;`endpoint` = base URL(不带 `/v1`)。
    Remote { endpoint: String, model: String },
    /// Stage2 整体禁用:不加载任何 LLM,校准 = 恒等(`calibrated` 直接承载原文)。
    /// 纯 ASR 部署 / 对照 Stage2 贡献用。
    Disabled,
}

impl LlmSpec {
    /// "remote" | "disabled" — 配置快照(ConfigView)的显示标签。
    pub fn kind(&self) -> &'static str {
        match self {
            LlmSpec::Remote { .. } => "remote",
            LlmSpec::Disabled => "disabled",
        }
    }
}

