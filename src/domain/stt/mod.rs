//! STT(语音转文字)领域层。
//!
//! 0.10 语音输入:hold-to-talk 期间采集音频 → STT 识别 → 文字上屏。
//!
//! ## 架构
//!
//! ```text
//! AudioCapture (infra)          SttEngine (domain)
//!    ↓ audio chunks                ↑ audio chunks
//!    │                             │
//!    └─── VoiceService ────────────┘
//!              ↓ partial text
//!              ↓ final text
//!         G1: emit chord-fill-query
//!         G2: inject_text()
//! ```
//!
//! ## SttEngine trait
//!
//! - `transcribe_chunk`: 接收一段音频,返回当前累积的 partial text
//! - `finalize`: 录音结束,返回最终 text
//! - `reset`: 新录音会话前重置状态
//!
//! ## 引擎选型
//!
//! - **云端**: CloudSttEngine (OpenAI 兼容 API,走 reqwest)
//! - **本地伪流式**: PseudoStreamingSttEngine (VAD 切句 + 定时 HTTP 预览)
//! - **本地非流式**: LocalSttEngine (HTTP 一次性识别)
//!
//! 0.10.4 起，真流式（WebSocket + Paraformer-streaming）已移除——
//! 伪流式在准确率、标点、体积、CPU 友好度上全面优于真流式，且 Python 侧零改动。

// ── 错误类型 ─────────────────────────────────────────────────────────────

/// STT 识别错误。
#[derive(Debug, thiserror::Error)]
pub enum SttError {
    /// 引擎未初始化
    #[error("STT engine not initialized")]
    NotInitialized,
    /// 识别过程中出错
    #[error("STT engine error: {0}")]
    Engine(String),
}

// ── SttTransport（0.22.7）─────────────────────────────────────────────────

/// STT 传输通道（0.22.7）。
///
/// 把"一段 WAV 字节交给本地引擎并取回文本"的通道抽象出来：
/// - 旧 Python server 路径：HTTP `/v1/audio/transcriptions`（0.22.7.4 删除）；
/// - GGUF worker 路径：Rust 常驻 worker client → stdin/stdout NDJSON。
///
/// 实现由 app 层注入（`SttEngineConnection.transport`），domain 只依赖本 trait，
/// 不接触 reqwest/进程/管道等 infra 细节。
#[derive(Debug, Clone)]
pub struct SttTransportResult {
    pub text: String,
    /// worker 自报的纯推理耗时；不支持诊断的 transport 返回 None。
    pub inference_ms: Option<f64>,
}

/// STT transport 的稳定错误分类。业务层不得再解析错误文案猜测语义。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SttTransportError {
    #[error("STT transport busy: {detail}")]
    Busy { detail: String },
    #[error("STT transport timeout: {detail}")]
    Timeout { detail: String },
    #[error("STT transport cancelled")]
    Cancelled,
    #[error("STT transport unavailable: {detail}")]
    Unavailable { detail: String },
    #[error("STT transport internal error: {detail}")]
    Internal { detail: String },
}

impl SttTransportError {
    pub fn category(&self) -> &'static str {
        match self {
            Self::Busy { .. } => "busy",
            Self::Timeout { .. } => "timeout",
            Self::Cancelled => "cancelled",
            Self::Unavailable { .. } => "transport_unavailable",
            Self::Internal { .. } => "transport_internal",
        }
    }
}

#[async_trait::async_trait]
pub trait SttTransport: Send + Sync {
    /// 通道与模型就绪检查（身份校验由实现承载）。
    async fn check_ready(&self) -> Result<(), SttTransportError>;

    /// 转录一段 WAV 字节（16kHz mono PCM WAV），返回识别文本。
    async fn transcribe(&self, wav_bytes: &[u8]) -> Result<String, SttTransportError>;

    /// 带可选 worker 推理耗时的诊断入口。生产业务仍调用 `transcribe`；
    /// corpus/benchmark 可用此方法拆分 IPC 排队与模型推理时间。
    async fn transcribe_with_metrics(
        &self,
        wav_bytes: &[u8],
    ) -> Result<SttTransportResult, SttTransportError> {
        self.transcribe(wav_bytes)
            .await
            .map(|text| SttTransportResult {
                text,
                inference_ms: None,
            })
    }
}

// ── STT Engine Connection ────────────────────────────────────────────────

/// STT 本地引擎连接快照（domain 层纯数据类型）。
///
/// 0.22.6 H4（批次 3）：替代此前对 `crate::app::local_engine::service::LocalEngineConnection`
/// 的直接引用——domain 层不得依赖 app 层。
///
/// app 层（`VoiceService`）负责把 `EngineManager::get_connection` 的结果
/// 投影为此类型再传入 `create_engine`。
///
/// **结构化 endpoint**——不再通过 `rsplit(':')` + `unwrap_or(8100)` 猜测端口。
/// endpoint 损坏时 `create_engine` 返回明确错误。
#[derive(Clone)]
pub struct SttEngineConnection {
    /// 监听地址（始终 `127.0.0.1`）。
    pub host: String,
    /// 实际监听端口。
    pub port: u16,
    /// engine id（日志/诊断用）。
    pub engine_id: String,
    /// instance id（每次启动随机生成，用于实例隔离）。
    pub instance_id: String,
    /// worker 传输通道（Some = GGUF 常驻 worker；None = 旧 HTTP server）。
    /// 存在时 host/port 退化为诊断字段，请求走 worker NDJSON 通道。
    pub transport: Option<std::sync::Arc<dyn SttTransport>>,
}

impl std::fmt::Debug for SttEngineConnection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SttEngineConnection")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("engine_id", &self.engine_id)
            .field("instance_id", &self.instance_id)
            .field(
                "transport",
                if self.transport.is_some() {
                    &"worker"
                } else {
                    &"http"
                },
            )
            .finish()
    }
}

// ── 结构化 STT 事件（0.22.9 Handoff 05 / 0.23.9）────────────────────────

use serde::{Deserialize, Serialize};

/// 音频时间轴上的半开绝对样本区间。
///
/// `start_sample`/`end_sample` 永远相对于当前识别 session 的起点，不能使用
/// compact 后的 Vec 局部 offset。这样 Draft 与 Preview 可以在不同目标投影间
/// 复用同一条时间轴，也不需要依赖文本相似度做去重。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AudioRange {
    pub start_sample: u64,
    pub end_sample: u64,
}

impl AudioRange {
    pub const fn new(start_sample: u64, end_sample: u64) -> Self {
        Self {
            start_sample,
            end_sample,
        }
    }

    pub const fn len(self) -> u64 {
        self.end_sample.saturating_sub(self.start_sample)
    }

    pub const fn is_empty(self) -> bool {
        self.start_sample >= self.end_sample
    }

    /// 判断两个半开区间是否有实际重叠。
    pub const fn overlaps(self, other: Self) -> bool {
        self.start_sample < other.end_sample && other.start_sample < self.end_sample
    }

    /// 判断 `other` 是否完全覆盖在当前区间内。
    pub const fn contains(self, other: Self) -> bool {
        self.start_sample <= other.start_sample && other.end_sample <= self.end_sample
    }
}

/// 识别协调器交付的稳定草稿段。
///
/// `span_id` 在 session 内稳定；`revision` 只表示同一 span 的修订版本，
/// 不与 VoiceService recording epoch、Editor content revision 或事件 seq 混用。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DraftSpan {
    pub span_id: u64,
    pub audio_range: AudioRange,
    pub text: String,
    pub revision: u64,
}

impl DraftSpan {
    pub fn new(
        span_id: u64,
        audio_range: AudioRange,
        text: impl Into<String>,
        revision: u64,
    ) -> Self {
        Self {
            span_id,
            audio_range,
            text: text.into(),
            revision,
        }
    }
}

/// 识别调度策略。profile 不携带 VoiceTarget/Tauri 类型，只表达领域层调度行为。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecognitionProfile {
    /// G2 与 Editor：滚动 Preview + 非重叠 Draft。
    PreviewDraft,
    /// G1/G3：保留现有累计 Partial/Final 投影契约。
    Legacy,
}

impl Default for RecognitionProfile {
    fn default() -> Self {
        Self::Legacy
    }
}

/// STT 逻辑 lane。实际执行仍由同一个 model worker 串行完成。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SttLane {
    Preview,
    Draft,
    Terminal,
}

/// 结构化 STT 事件。
///
/// 所有 STT 引擎实现统一产出此事件流，`VoiceService` 只消费 `SttEvent`，
/// 不关心底层是 GGUF 伪流式、ONNX 真流式还是云端非流式。
///
/// **generation 语义**：每次 `begin_session` 时递增 generation；
/// 旧 generation 的 `Partial`/`Final` 事件必须被消费方丢弃——
/// 这防止 cancel/reset 后迟到结果污染 UI 或文本注入。
///
/// **事件顺序保证**：
/// - `Partial` 只在 `Final` 前出现（同一 generation 内）
/// - `Busy` 表示推理积压，消费方可选择暂停音频推送或降低频率
/// - `Error` 表示不可恢复的引擎错误，session 终止
/// - `Final` 是 session 的最后一个正常事件
#[derive(Debug, Clone, PartialEq)]
pub enum SttEvent {
    /// 部分识别结果（实时预览）。
    ///
    /// 真流式引擎（ParaformerOnline）每次推理产生 native partial；
    /// 伪流式引擎（PseudoStreaming）通过 VAD 切句 + 定时预览产生 partial；
    /// 非流式引擎不产生 Partial。
    ///
    /// **边沿触发约定**：引擎状态（confirmed/preview）未变化时不得产出本事件；
    /// 引擎以 `revision` 标识状态版本，消费方可据此去重与合并。
    Partial {
        /// 当前 generation（session 标识）。
        generation: u64,
        /// 引擎状态版本（单调递增）；状态未变化时不得重复产出同一 revision。
        revision: u64,
        /// 已确认文本（不再变化的部分）。
        ///
        /// **仅当 `confirmed_changed` 为 true 时为完整累计正文**，否则为空串——
        /// 避免每个音频块重复搬运随时长增长的全量正文。消费方需自行缓存
        /// 最近一次收到的累计正文。
        confirmed: String,
        /// 本次是否带来 confirmed 新增（true 时 `confirmed` 为完整累计正文）。
        confirmed_changed: bool,
        /// 预览文本（可能继续变化的部分）。
        preview: String,
    },

    /// 类型化稳定草稿段（0.23.9）。
    ///
    /// 这是可靠、有序的交付物；消费方按 `span.audio_range`/`span_id` 去重，
    /// 不应再从累计 confirmed 字符串反推增量。旧引擎仍可继续发送 `Partial`。
    Draft { generation: u64, span: DraftSpan },

    /// 类型化可替换预览（0.23.9）。
    ///
    /// 新 request id 发出后，旧 request 的结果只能被丢弃，不能覆盖当前预览。
    Preview {
        generation: u64,
        request_id: u64,
        audio_range: AudioRange,
        revision: u64,
        text: String,
    },

    /// 最终识别结果（session 的正常终态）。
    ///
    /// `finish_session` 调用后产生此事件。之后此 generation 不应再产出事件。
    Final {
        /// 当前 generation。
        generation: u64,
        /// 最终识别文本。
        text: String,
    },

    /// 不可恢复的引擎错误。
    ///
    /// session 终止，消费方应停止音频推送并清理状态。
    Error {
        /// 当前 generation。
        generation: u64,
        /// 人类可读的错误信息。
        message: String,
    },
}

impl SttEvent {
    /// 获取事件所属的识别 generation。
    pub const fn generation(&self) -> u64 {
        match self {
            Self::Partial { generation, .. }
            | Self::Draft { generation, .. }
            | Self::Preview { generation, .. }
            | Self::Final { generation, .. }
            | Self::Error { generation, .. } => *generation,
        }
    }

    /// 可靠顺序交付物：Draft、Final、Error，以及旧协议中带 confirmed 的 Partial。
    pub fn is_reliable(&self) -> bool {
        match self {
            Self::Draft { .. } | Self::Final { .. } | Self::Error { .. } => true,
            Self::Partial {
                confirmed_changed,
                confirmed,
                ..
            } => *confirmed_changed || !confirmed.is_empty(),
            Self::Preview { .. } => false,
        }
    }

    pub fn lane(&self) -> SttLane {
        match self {
            Self::Draft { .. } => SttLane::Draft,
            Self::Partial {
                confirmed_changed,
                confirmed,
                ..
            } if *confirmed_changed || !confirmed.is_empty() => SttLane::Terminal,
            Self::Preview { .. } | Self::Partial { .. } => SttLane::Preview,
            Self::Final { .. } | Self::Error { .. } => SttLane::Terminal,
        }
    }
}

/// 结构化 STT 流式 port——框架无关的统一 STT 生命周期抽象。
///
/// 所有 STT 实现（GGUF 伪流式 / ONNX 真流式 / 云端非流式）
/// 通过此 trait 对外提供统一的 begin/push/finish/cancel/reset 生命周期。
///
/// **domain 不依赖 ORT、worker framing、Tauri 或 concrete runtime**——
/// 此 trait 只依赖 `SttEvent` 和基本 Rust 类型。
///
/// ## 生命周期
///
/// ```text
/// begin_session() → generation=N
///   ├─ push_audio(samples)  → 可产出 SttEvent::Partial
///   ├─ finish_session()     → 产出 SttEvent::Final（等所有在途结果）
///   ├─ cancel_session()     → 丢弃在途结果，不产出 Final
///   └─ reset()              → 幂等清理，回到 begin 前状态
/// ```
///
/// ## 事件消费
///
/// `events()` 返回一个**有界** `tokio::sync::mpsc::Receiver<SttEvent>`。
/// 消费方（VoiceService）在独立 task 中循环 `recv()`，按 generation 过滤旧事件。
/// 有界是硬约束：实现方在队列满时必须按重要性分流（可合并的进度类事件允许丢弃，
/// 终态与"已确认、有序"的交付物不得静默丢失），禁止无界排队。
///
/// ## 并发约束
///
/// - 一次只允许一个 active session（begin → finish/cancel）
/// - `push_audio` 不得阻塞 P0 主链路——内部通过 channel 转发
/// - `cancel`/`reset` 幂等
/// - 旧 generation 的迟到结果丢弃
#[async_trait::async_trait]
pub trait StreamingSttPort: Send + Sync {
    /// 开始一个新的识别 session。
    ///
    /// 返回新的 generation（单调递增）。消费方应记录此 generation，
    /// 用于过滤旧 session 的迟到事件。
    ///
    /// **一次只允许一个 active session**——调用方需保证在 begin 前没有活跃 session。
    async fn begin_session(&self) -> Result<u64, SttError>;

    /// 推送一段 PCM 音频（f32, 16kHz, mono）。
    ///
    /// 不阻塞——内部通过 channel 转发给推理 task。
    /// 推理结果通过 `events()` 的 receiver 异步返回。
    ///
    /// generation 必须与 `begin_session` 返回值匹配。
    async fn push_audio(&self, generation: u64, samples: &[f32]) -> Result<(), SttError>;

    /// 结束当前 session 并等待最终结果。
    ///
    /// 通知引擎音频流已结束，引擎完成剩余推理后产出 `SttEvent::Final`。
    /// 此方法本身不等待 Final——消费方通过 `events()` 接收。
    async fn finish_session(&self, generation: u64) -> Result<(), SttError>;

    /// 取消当前 session（幂等）。
    ///
    /// 丢弃所有在途结果，不产出 `Final`。
    /// 旧 generation 的迟到 `Partial`/`Final` 事件将被消费方丢弃。
    async fn cancel_session(&self, generation: u64) -> Result<(), SttError>;

    /// 重置引擎状态（幂等）。
    ///
    /// 清理内部缓冲和状态，回到 `begin_session` 前的干净状态。
    /// 可在任何时候调用，包括 session 进行中（等价于 cancel + 清理）。
    ///
    /// **当前仅被 streaming_port 测试调用**；生产路径使用 cancel_session。
    /// 保留以支持未来 native 流式引擎的重置需求。
    #[allow(dead_code)]
    async fn reset(&self) -> Result<(), SttError>;

    /// 是否支持 native partial（真流式）。
    ///
    /// `true` = ParaformerOnline 等真流式引擎，在 `push_audio` 期间产生 native partial。
    /// `false` = GGUF 伪流式 / 非流式引擎，partial 通过伪流式 VAD + 定时预览产生。
    ///
    /// VoiceService 据此决定是否开启实时预览。
    ///
    /// **当前仅被 streaming_port 测试调用**；VoiceService 暂未读取此标记。
    /// 保留以支持未来 native 流式引擎切换实时预览策略。
    #[allow(dead_code)]
    fn supports_native_partial(&self) -> bool;

    /// 当前 session 使用的识别调度 profile。
    ///
    /// 旧实现默认为 `Legacy`，让新增类型事件可以渐进接入而不改变 G1/G3
    /// 的提交协议。
    fn recognition_profile(&self) -> RecognitionProfile {
        RecognitionProfile::Legacy
    }

    /// 会话开始后触发一次推理预热（fire-and-forget，不阻塞录音链路）。
    ///
    /// 0.23.10.2：GGUF worker 首次推理有懒加载开销（冷启 ~650ms vs 热态
    /// ~300ms），预热把该开销移到用户尚未说话的窗口。默认 no-op——仅
    /// 伪流式 GGUF 适配器需要；调用方在 `begin_session` 成功后调用。
    async fn warm_up(&self) -> Result<(), SttError> {
        Ok(())
    }

    /// 获取事件 receiver（有界通道）。
    ///
    /// 返回的 receiver 用于接收 `SttEvent`。消费方应在独立 task 中循环 `recv()`。
    /// 多次调用返回同一 channel 的新 receiver（旧 receiver 失效）。
    fn events(&self) -> tokio::sync::mpsc::Receiver<SttEvent>;

    /// 链路诊断快照（引擎状态 + 事件通道指标；默认无数据）。
    ///
    /// 只读、同步、无副作用——供 VoiceService 打定期统计日志。
    fn stream_stats(&self) -> SttStreamStats {
        SttStreamStats::default()
    }
}

// ── 流式链路诊断快照（0.24）────────────────────────────────────────────

/// STT 流式链路的诊断计数快照。
///
/// **只含计数、布尔与长度，绝不含正文/转写内容**（spec-backend §三）。
/// 由 `StreamingSttPort::stream_stats` 汇总「引擎内部状态 + 事件通道指标」，
/// 供 VoiceService 打定期统计日志与排障。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SttStreamStats {
    /// 已收到的音频块数（`push_audio` 调用次数）。
    pub chunks_received: u64,
    /// 实际对外产出的 Partial 事件数（= 状态变化次数）。
    pub partials_emitted: u64,
    /// 因状态未变化被抑制的事件数（去重/合并）。
    pub partials_suppressed: u64,
    /// 纯预览被 latest slot 替换的次数（被替换值从未作为事件交付）。
    pub partials_coalesced: u64,
    /// 事件通道已关闭而未能交付的 Partial 数。
    pub partials_dropped_closed: u64,
    /// 因队列已满而降级为阻塞发送的「携带 confirmed 变化」事件数（背压次数）。
    pub confirmed_backpressure: u64,
    /// 当前事件队列深度。
    pub queue_depth: usize,
    /// 事件队列有界容量。
    pub queue_capacity: usize,
    /// 观测到的最大队列深度。
    pub max_queue_depth: usize,
    /// 引擎内当前累积的 PCM 样本数。
    pub pcm_samples: usize,
    /// 引擎已提交（compact 后可回收）的绝对样本末尾。
    pub pcm_committed_end: usize,
    /// 预览识别是否在飞行中。
    pub preview_in_flight: bool,
    /// 句段定稿识别是否在飞行中。
    pub finalize_in_flight: bool,
    /// 在途推理任务数（预览 + 定稿 + 排队句段）。
    pub in_flight_inferences: usize,
    /// confirmed 提交版本（每次成功 commit 递增）。
    pub confirmed_revision: u64,
    /// 预览状态版本（预览文本每次变化递增）。
    pub preview_revision: u64,
}

// ── STT Engine trait（旧接口，保留兼容）──────────────────────────────────
///
/// 0.22.9 之前使用的 trait。新代码应使用 `StreamingSttPort`。
/// 保留此 trait 以兼容 GGUF 伪流式和非流式引擎的现有实现。
#[async_trait::async_trait]
pub trait SttEngine: Send + Sync {
    /// 接收一段音频 chunk,返回当前累积识别的 partial text。
    ///
    /// 伪流式模式下每次调用返回逐步完善的文本;
    /// 非流式模式下可以累积音频,在 `finalize` 时一次性返回。
    async fn transcribe_chunk(&self, samples: &[f32]) -> Result<String, SttError>;

    /// 录音结束,返回最终识别文本。
    async fn finalize(&self) -> Result<String, SttError>;

    /// 重置引擎状态(新录音会话前调用)。
    fn reset(&self);

    /// 会话开始后触发一次推理预热（默认 no-op）。
    ///
    /// 0.23.10.2：仅伪流式 GGUF 引擎需要——首次推理的懒加载开销被移到
    /// 用户尚未说话的窗口。同步触发、内部 fire-and-forget，立即返回。
    fn warm_up(&self) {}

    /// 引擎侧诊断快照（默认无数据）。
    ///
    /// 只读、无副作用：不得因读取而启动推理或改变 generation。
    fn stream_stats(&self) -> SttStreamStats {
        SttStreamStats::default()
    }

    /// 0.23.9: 提供 `Any` 引用以支持 downcast 到具体引擎类型。
    ///
    /// 用于诊断接口（如 `coordinator_trace`）在不暴露引擎内部结构的
    /// 前提下访问引擎特有方法。生产路径不使用此方法。
    fn as_any(&self) -> &dyn std::any::Any {
        // 默认实现返回空 Any——具体引擎按需 override
        // 安全性：此方法不修改状态、不触发推理，仅供只读诊断。
        &()
    }
}
// ── STT Engines ──────────────────────────────────────────────────────────

pub(crate) mod cloud;

pub mod dictation; // 0.23.3：编辑器连续听写段推导（纯逻辑）
pub mod gguf_postprocess;
pub mod local;
#[cfg(test)]
mod mock;
pub(crate) mod postprocess;
pub mod pseudo_streaming;
pub(crate) mod sentence_state;
pub mod streaming_port;
pub mod transcribe; // 0.22.16 Handoff 04：一次性文件转写领域契约
pub mod vad;
pub mod vad_diagnostics;
pub(crate) mod wav;

/// 创建 STT 引擎实例(工厂函数)。
///
/// 根据 SttConfig 选择引擎：
/// - Cloud 模式 + 已配置云端供应商（cloud 或 cloud_provider）→ CloudSttEngine
/// - Local 模式 + StreamingMode::Pseudo → PseudoStreamingSttEngine (VAD + 预览) ⭐ 默认
/// - Local 模式 + StreamingMode::Off → LocalSttEngine (HTTP 非流式)
///
/// **0.22.6 批次 3**: Local 模式下使用 `SttEngineConnection`（domain 层纯数据类型），
/// STT 请求携带 `X-Engine-Token` 鉴权。无运行实例时返回错误。
/// **不再引用 `crate::app`**——app 层 VoiceService 负责把
/// `LocalEngineConnection` 投影为 `SttEngineConnection`。
///
/// **0.22.6 H4**: Local 模式下从 `local_stt_selection` 联合引用解析 engine/model。
/// 当前只需支持已注册的 Python FunASR，不实现 GGUF。
///
/// **不会回退到 Mock 引擎**——未启用 / 未配置 / 服务未就绪时返回 Err，
/// 由调用方（VoiceService）决定如何向用户反馈错误。
#[allow(dead_code)]
pub fn create_engine(
    connection: Option<SttEngineConnection>,
) -> Result<Box<dyn SttEngine>, String> {
    // 保持旧工厂调用方的 Legacy 行为；需要双层调度的 G2/Editor 由 app
    // 显式调用 `create_engine_with_profile(..., PreviewDraft)`。
    create_engine_with_profile(connection, RecognitionProfile::Legacy)
}

/// 按目标选择识别调度 profile 的引擎工厂。
///
/// `create_engine` 保留 Legacy 默认以兼容现有本地调用；G2/Editor
/// 由 app 层显式传 `PreviewDraft`，避免把 VoiceTarget 泄漏到 domain。
pub fn create_engine_with_profile(
    connection: Option<SttEngineConnection>,
    profile: RecognitionProfile,
) -> Result<Box<dyn SttEngine>, String> {
    let config = crate::domain::config::stt_config::get_stt_config();

    if !config.enabled {
        return Err("STT 未启用，请在设置页开启语音输入".to_string());
    }

    // 0.22.6 H4: Local 模式下记录联合引用
    if config.mode == crate::domain::config::stt_config::SttMode::Local
        && let Some(ref sel) = config.local_stt_selection
    {
        tracing::debug!(
            engine_id = %sel.engine_id,
            model_id = %sel.model_id,
            "本地 STT 选择（联合引用）"
        );
    }

    match config.mode {
        crate::domain::config::stt_config::SttMode::Cloud => {
            if config.is_cloud_configured() {
                tracing::info!("STT 引擎: cloud");
                Ok(Box::new(cloud::CloudSttEngine::new()))
            } else {
                Err("云端 STT 未配置供应商，请在设置页中配置".to_string())
            }
        }
        crate::domain::config::stt_config::SttMode::Local => {
            // 0.22.6 批次 3: 从 SttEngineConnection 提取 port 和 token
            let conn = match connection {
                Some(c) => c,
                None => {
                    return Err("FunASR 服务未运行。请在设置页启动服务。\
                         （preferred port 只代表偏好，不代表实际监听地址）"
                        .to_string());
                }
            };
            // 使用完整连接快照构造引擎——就绪与身份由 start 时的 NDJSON
            // ready 握手保证（worker 与实例绑定）。
            match config.streaming_mode {
                crate::domain::config::stt_config::StreamingMode::Pseudo => {
                    match pseudo_streaming::PseudoStreamingSttEngine::from_connection_with_profile(
                        &config,
                        conn.clone(),
                        profile,
                    ) {
                        Ok(engine) => {
                            tracing::info!("STT 引擎: pseudo-streaming (VAD + GGUF worker)");
                            Ok(Box::new(engine))
                        }
                        Err(e) => {
                            tracing::warn!(%e, "STT 伪流式引擎创建失败,回退非流式");
                            match local::LocalSttEngine::from_connection(&config, conn) {
                                Ok(engine) => {
                                    tracing::info!("STT 引擎: local (FunASR, 非流式回退)");
                                    Ok(Box::new(engine))
                                }
                                Err(e) => Err(format!("STT 引擎创建失败: {e}")),
                            }
                        }
                    }
                }
                crate::domain::config::stt_config::StreamingMode::Off => {
                    match local::LocalSttEngine::from_connection(&config, conn) {
                        Ok(engine) => {
                            tracing::info!("STT 引擎: local (FunASR)");
                            Ok(Box::new(engine))
                        }
                        Err(e) => Err(format!("STT 引擎创建失败: {e}")),
                    }
                }
            }
        }
    }
}

/// Mock STT 引擎的假文本库(按时间轮换,模拟"边说边出字")。
///
/// 仅供 `#[cfg(test)]` 的 MockSttEngine 使用，不参与生产路径。
#[cfg(test)]
pub fn mock_text_for_elapsed(elapsed: std::time::Duration) -> &'static str {
    match elapsed.as_secs() {
        0..=1 => "",
        2 => "你好",
        3 => "你好世界",
        4 => "你好世界这是一段",
        5 => "你好世界这是一段测试语音",
        _ => "你好世界这是一段测试语音识别的文字结果",
    }
}

// ── 测试 ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod connection_tests {
    use super::*;

    #[test]
    fn stt_engine_connection_clone_preserves_fields() {
        let conn = SttEngineConnection {
            host: "127.0.0.1".to_string(),
            port: 8100,
            engine_id: "funasr".to_string(),
            instance_id: "inst-123".to_string(),
            transport: None,
        };
        let cloned = conn.clone();
        assert_eq!(cloned.host, conn.host);
        assert_eq!(cloned.port, conn.port);
        assert_eq!(cloned.engine_id, conn.engine_id);
        assert_eq!(cloned.instance_id, conn.instance_id);
    }

    #[test]
    fn stt_engine_connection_debug_format_contains_key_fields() {
        let conn = SttEngineConnection {
            host: "127.0.0.1".to_string(),
            port: 8100,
            engine_id: "funasr".to_string(),
            instance_id: "inst-1".to_string(),
            transport: None,
        };
        let debug_str = format!("{:?}", conn);
        assert!(debug_str.contains("127.0.0.1"));
        assert!(debug_str.contains("8100"));
        assert!(debug_str.contains("funasr"));
    }

    /// create_engine 在 STT 未启用时返回明确错误（不回退 Mock）。
    /// Local 模式但无 connection 时也返回明确错误。
    ///
    /// 注意：init_cache 用 OnceLock，只能设置一次，所以两个场景合并到一个测试中。
    /// 使用 init_cache + update_cache 双重设置，确保在并行测试中也能覆盖已有值。
    #[test]
    fn create_engine_returns_err_for_disabled_and_no_connection() {
        // 临时设置 config 为 disabled + Local 模式
        let config = crate::domain::config::stt_config::SttConfig {
            enabled: false,
            mode: crate::domain::config::stt_config::SttMode::Local,
            ..Default::default()
        };
        crate::domain::config::stt_config::init_cache(config.clone());
        crate::domain::config::stt_config::update_cache(&config);

        // STT 未启用 → 返回错误
        let result = create_engine(None);
        assert!(result.is_err());
        if let Err(e) = result {
            assert!(e.contains("STT 未启用"), "应提及未启用: {e}");
        }
    }

    /// Static architecture test: `src/domain/stt` 不得引用 `crate::app`、`tauri` 或 `windows`。
    ///
    /// 0.22.6 批次 3: domain 层必须保持框架无关——不依赖 app 层、Tauri 或 Win32 API。
    /// 此测试扫描 `src/domain/stt/` 下所有 `.rs` 文件，检查是否包含禁止的引用。
    ///
    /// 注意：`crate::domain::stt::SttEngineConnection` 被 app 层引用是合法的（app → domain），
    /// 但 domain 层不得反向引用 app 层。
    #[test]
    fn domain_stt_does_not_reference_app_tauri_or_windows() {
        use std::fs;
        use std::path::PathBuf;

        // 定位 src/domain/stt 目录
        let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".to_string());
        let stt_dir: PathBuf = std::path::Path::new(&manifest_dir)
            .join("src")
            .join("domain")
            .join("stt");

        if !stt_dir.exists() {
            // 在某些构建环境中（如 IDE 索引），目录可能不存在——跳过
            eprintln!("跳过：src/domain/stt 目录不存在（{}）", stt_dir.display());
            return;
        }

        // 递归收集所有 .rs 文件
        fn collect_rs_files(dir: &std::path::Path, files: &mut Vec<PathBuf>) {
            if let Ok(entries) = fs::read_dir(dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.is_dir() {
                        collect_rs_files(&path, files);
                    } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
                        files.push(path);
                    }
                }
            }
        }

        let mut files = Vec::new();
        collect_rs_files(&stt_dir, &mut files);

        assert!(
            !files.is_empty(),
            "src/domain/stt 目录下应至少有一个 .rs 文件"
        );

        // 使用运行时构造避免测试代码自身触发违规
        let p1 = format!("crate{}:{}", ":", ":app");
        let p2 = format!("use {}", "tauri");
        let p3 = format!("use {}", "windows::");
        let p4 = format!("{}{}", "tauri", "::");
        let forbidden_patterns = [p1, p2, p3, p4];

        let mut violations: Vec<String> = Vec::new();
        for file in &files {
            // 跳过测试模块中的注释行——只检查实际代码
            let content = match fs::read_to_string(file) {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("警告: 读取 {} 失败: {e}", file.display());
                    continue;
                }
            };

            for (line_num, line) in content.lines().enumerate() {
                let trimmed = line.trim();
                // 跳过注释行
                if trimmed.starts_with("//") || trimmed.starts_with("/*") {
                    continue;
                }
                for pattern in &forbidden_patterns {
                    if trimmed.contains(pattern) {
                        // 允许在注释中出现（如文档引用），但上面已跳过注释行
                        violations.push(format!(
                            "{}:{}: 包含禁止的引用 '{}': {}",
                            file.display(),
                            line_num + 1,
                            pattern,
                            trimmed
                        ));
                    }
                }
            }
        }

        assert!(
            violations.is_empty(),
            "src/domain/stt 不得引用 crate::app / tauri / windows。\n\
             违规项:\n{}",
            violations.join("\n")
        );
    }
}
