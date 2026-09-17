//! 伪流式 STT 引擎——VAD 切句定稿 + 累积预览。
//!
//! ## 设计
//!
//! 在非自回归的 SenseVoice 上实现"边说边出字"体感：
//! - 每 500ms 对累积音频做一次 HTTP 识别 → 预览文本（灰色半透明）
//! - VAD 检测到句尾时对本句音频做定稿识别 → 确认文本（不再变化）
//!
//! 用户体验：
//! ```text
//! 定稿: "你好世界。"          ← 白色，不变
//! 预览: "今天天气"            ← 灰色，可能变化
//! ```
//!
//! ## 0.22.15 事务化句尾
//!
//! 句尾不再立即推进 committed audio end。流程改为：
//! 1. VAD 产出 SentenceEnd → 创建 PendingSegment（冻结候选范围 + preview 快照）
//! 2. finalize task 携带 session_generation + segment_id，返回时校验 identity
//! 3. 非空结果 → commit（追加 confirmed、推进 committed end、清理对应 preview）
//! 4. 空/错误/超时 → rollback（committed end 不变，preview 保留，后续覆盖该段音频）
//!
//! ## 与其他引擎的关系
//!
//! - [`LocalSttEngine`](super::local::LocalSttEngine)：非流式（transcribe_chunk 空转）
//! - **本引擎**：伪流式（VAD 切句 + 定时 HTTP 轮询）⭐ 默认
//!
//! ## transcribe_chunk 返回值
//!
//! 返回 JSON 字符串（协议 v2）：
//!
//! ```json
//! {"v":2,"revision":12,"confirmed_changed":true,"confirmed":"第一句。","preview":"第二句"}
//! ```
//!
//! - `revision`：引擎状态版本，confirmed 提交 / 预览变化时递增；
//! - `confirmed_changed`：本次是否带来 confirmed 增长（false 时 `confirmed` 为空串，
//!   避免每块音频重复搬运随时长增长的全量正文）；
//! - 状态版本未变化（与上次返回相同）时直接返回空字符串，消费方不产生任何事件。
//!
//! 这取代了旧协议「每块音频都返回完整累计 confirmed + preview」的行为——后者
//! 使事件量与正文搬运量随录音时长平方级增长。
//!
//! ## 并发安全
//!
//! 使用 `Arc<std::sync::Mutex>` 保护内部状态。后台 HTTP task 通过 clone 的
//! `Arc` 在完成后短暂加锁写入结果。`transcribe_chunk` 是 async 但不跨 await
//! 持有 `std::sync::Mutex`（先 lock 取数据/写数据，再 drop guard，再 await）。

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

mod coordinator;

pub use self::coordinator::CoordinatorTrace;
use self::coordinator::{
    AudioRange, BoundaryCandidate, DraftRequest, PreviewRequest, RecognitionCoordinator,
    RecognitionProfile, RecognitionSettings, RequestAudioGate,
};
use super::postprocess::{strip_confirmed_prefix, strip_filler_words, trim_trailing_silence};
use super::sentence_state::{FinalizeResult, PendingSegment, SegmentIdentity, SentenceState};
use super::vad::{EnergyVad, VadEvent};
use super::{
    DraftSpan, PreviewSegment, SttEngine, SttError, SttStreamStats, settle_preview_segments,
};

/// 预览识别间隔（毫秒）。
const PREVIEW_INTERVAL_MS: u64 = 500;

/// 累积音频超过此时长时，预览间隔自动拉长（毫秒）。
const PREVIEW_SLOWDOWN_THRESHOLD_MS: u64 = 8000;

/// 预览间隔在慢速模式下的值（毫秒）。
const PREVIEW_SLOW_INTERVAL_MS: u64 = 1000;

/// 两次预览之间至少新增的音频。
const PREVIEW_MIN_NEW_AUDIO_MS: u64 = 500;

/// 预览定稿短语的最小有效有声时长（毫秒）。低于该值的片段并入下一短语，
/// 不单独送识别（过短片段的独立识别质量不可靠）。
const PHRASE_MIN_VOICED_MS: u64 = 500;

/// 0.23.13 时间触发短语冻结的前缀最小长度（毫秒）：滚动窗前缀积满该值
/// 即主动追认为短语，不再依赖停顿候选被作废——停顿检测失灵时预览仍增量。
const TIME_FREEZE_MIN_PREFIX_MS: u64 = 1_200;

/// 0.23.14 长静音终结的可信有声下限（毫秒）：quiet 达到 `long_pause_ms`
/// 时，owned 内有效有声低于该值的候选仍不接受——键盘点击、呼吸等短
/// 脉冲不因长静音升级为 Draft；更长噪声（咳嗽等）由模型 NoSpeech 消费，
/// 不产生 span。
const LONG_PAUSE_MIN_VOICED_MS: u64 = 300;

/// 0.23.14.7 自然句尾独立采纳的停顿下限（毫秒，gate 静默口径）。
///
/// 只作用于 VAD 已完成 `min_sentence_ms` 校验的 `SentenceEnd` 候选。
/// case_12 实测标定（真实回放 decision observer）：体感停顿经 gate 口径
/// 系统性缩短——起音块内的静默前段不参与累计（块内出现有声帧即整块按
/// 有声处理），呼吸/起音帧又消耗其余静默。用户四档停顿的 gate 口径实测
/// 570/590/约 430–470/（5.66s owned 直采），最短一档 gate ≈400–470ms。
/// 400ms = VAD 句尾声明的 `min_silence_ms`（300ms）再确认 100ms，是
/// "每个 VAD 已校验自然句尾都按时定稿"的最低口径；采纳同时要求有声
/// ≥ `min_sentence_ms`（≥800ms 连续有声）——慢速词间停顿跟随的是
/// ShortPhraseEnd（词长 < min_sentence），仍走 long_pause 升级路径，
/// 点击/咳嗽/呼吸也不进本分支。
const NATURAL_PAUSE_MIN_MS: u64 = 400;

/// 0.23.14.7 case_17：可信有声的**连续性**下限（毫秒，连续强有声段）。
///
/// 强有声 = 10ms 帧 RMS ≥ VAD 自适应 on 阈值（scale-free，阈值随底噪浮动）。
/// 真语音的音节是**连续发声段**：一个音节即可给出上百毫秒的连续强帧；而
/// 风扇/键盘这类环境声即使帧总量凑够（甚至越过 on），形态上也只是稀疏
/// 短脉冲，连续段在几十毫秒内就断裂。
///
/// 真实 corpus 全量回放实测（candidate 证据链，17 例）：
/// - 伪文本候选（case_17 尾段 21.60s）连续段 **50ms**；
/// - 纯噪声但模型回 NoSpeech 的候选（case_11 全程静音、case_17 强脉冲
///   19.29s）为 130ms / 140ms（无害，保留原行为）；
/// - **全部真句候选最低 120ms**（case_15 远场轻声最弱一档），其余
///   290ms～4230ms。
///
/// 100ms 取在 50ms 与 120ms 之间：拒绝侧 2.0×、保留侧 1.2×。该判据同时
/// 用于候选采纳证据与送模前的范围门——**不可信范围不产生可靠 Draft，
/// 也不进模型**（避免噪声幻觉进入终态文本）。
const CREDIBLE_VOICED_RUN_MIN_MS: u64 = 100;

/// 候选被生产采纳的依据分支（0.23.14.7 case_17 专项证据链）。
///
/// `candidate.reason == natural_silence` 只说明 VAD 事件类型（SentenceEnd），
/// 不说明最终由 readiness 的哪条门槛放行——诊断必须区分
/// `natural_pause`（400ms 自然句尾新路径）/ `long_pause`（1100ms 长静音）/
/// `strong_pause`（既有强停顿）/ `draft_min`（owned ≥ 5s）/ `hard_window` /
/// `uncommitted_cap`（强制切），否则环境声误切无法归因。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AcceptedVia {
    HardWindow,
    UncommittedCap,
    DraftMin,
    LongPause,
    NaturalSentence,
    StrongPause,
}

impl AcceptedVia {
    fn as_str(self) -> &'static str {
        match self {
            AcceptedVia::HardWindow => "hard_window",
            AcceptedVia::UncommittedCap => "uncommitted_cap",
            AcceptedVia::DraftMin => "draft_min",
            AcceptedVia::LongPause => "long_pause",
            AcceptedVia::NaturalSentence => "natural_pause",
            AcceptedVia::StrongPause => "strong_pause",
        }
    }
}

/// 自适应预览间隔上限。
///
/// 冷却从上一轮推理完成后开始计算；较长上限可避免慢机器在长音频上
/// 刚结束一次重推理就很快开始下一次，形成持续高 CPU 占用。
const PREVIEW_MAX_INTERVAL_MS: u64 = 5000;

/// 未提交音频上限的默认值（毫秒），对应 `VadConfig.max_uncommitted_s = 12`。
///
/// VAD 状态异常或音量长期落在滞回区时的最终保险：未提交音频达到
/// 上限后仍强制切段。这个上限按绝对音频坐标计算，不依赖 speaking 状态。
/// 0.23.7 起上限值可配置，此常量仅作测试基准，生产值经
/// `PseudoStreamingSttEngine::from_connection` 从配置注入。
#[cfg(test)]
const MAX_UNCOMMITTED_AUDIO_MS: u64 = 12_000;

/// finalize 等待 in_flight 请求的最大时间。
const FINALIZE_WAIT_TIMEOUT_MS: u64 = 3000;

/// 0.23.7.2 D：强制切（硬窗口/未提交上限）回退谷底的搜索窗口（毫秒）。
///
/// 有界——同时受 `EnergyVad` 帧历史容量（约 1.5s）限制。内部实验参数，
/// 不进设置页；只移动强制切点位置，不改变兜底触发时机。
const HARD_CUT_VALLEY_WINDOW_MS: u64 = 1_200;

/// 伪流式 STT 引擎。
///
/// 组合 VAD 切句 + 累积预览，在非自回归 SenseVoice 上实现"边说边出字"体感。
///
/// 0.22.6 批次 3: 存储完整 `SttEngineConnection` 快照，确保 health 检查和
/// 转录请求复用同一 worker 通道快照（0.22.7.4 起 StdioWorker 是唯一本地实现）。
pub struct PseudoStreamingSttEngine {
    /// 内部状态
    inner: Arc<Mutex<PseudoInner>>,
    /// 连接快照（engine_id + instance_id + worker transport）
    ///
    /// 0.22.6: health 和 transcribe 共用此快照，保证同一连接。
    /// 服务重启后旧连接的 instance_id 不匹配新实例，请求被拒绝。
    connection: Option<crate::domain::stt::SttEngineConnection>,
    /// 采样率
    sample_rate: u32,
    /// 仅诊断回放设置；生产录音不分配切点记录。
    boundary_observer: Option<Arc<Mutex<Vec<SttBoundaryRecord>>>>,
    /// 仅诊断回放设置；记录候选边界最终“切/等待”的判断依据。
    decision_observer: Option<Arc<Mutex<Vec<SttDecisionRecord>>>>,
    /// 仅诊断回放设置；生产录音不分配定稿阶段记录。
    #[cfg(test)]
    finalize_observer: Option<Arc<Mutex<Vec<SttFinalizeRecord>>>>,
}

/// 伪流式引擎实际产生的边界（包含未提交上限兜底）。
#[derive(Debug, Clone, serde::Serialize)]
pub struct SttBoundaryRecord {
    pub audio_ms: u64,
    pub reason: &'static str,
    /// 边界在引擎内被确认的单调时钟；诊断回放用同一原点换算墙钟延迟。
    #[cfg(test)]
    #[serde(skip)]
    pub observed_at: Instant,
}

/// PreviewDraft 调度层对一个候选边界作出的诊断判断。
///
/// 与 [`SttBoundaryRecord`] 不同，这里既记录已经接受的切点，也记录因上下文
/// 不足而继续累积的候选。所有区间均位于 session 的绝对音频时间轴上。
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SttDecisionRecord {
    pub audio_ms: u64,
    pub owned_start_ms: u64,
    pub owned_end_ms: u64,
    pub reason: String,
    pub outcome: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wait_reason: Option<&'static str>,
    /// 0.23.14.7：`outcome == accepted` 时标明放行分支
    /// （natural_pause / long_pause / strong_pause / draft_min / hard_window /
    /// uncommitted_cap）——candidate reason 只描述 VAD 事件类型，不构成
    /// 采纳证据。等待中的候选为 None。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub accepted_via: Option<&'static str>,
    /// 0.23.14.7 case_17：强有声样本（RMS ≥ on）毫秒数。实测值域：
    /// 误采纳的环境声尾段候选 340ms/1060ms（32%）——**与真句量级重叠**
    /// （远场轻声最低档 600ms/1560ms 为 38.5%），因此占比本身不构成
    /// 判据，只作证据链记录。
    pub strong_ms: u64,
    /// 0.23.14.7 case_17：最长连续强有声段毫秒数——真音节是连续发声段，
    /// 环境声即使能量够也只是稀疏短脉冲（实测伪文本候选 50ms，合法候选
    /// 最低 120ms）。
    pub strong_run_ms: u64,
    pub voiced_ms: u64,
    pub quiet_ms: u64,
}

/// 定稿任务的阶段观察记录（仅诊断回放使用）。
///
/// `observed_at` 使用与回放同源的单调时钟，跳过序列化；调用方可以用同一
/// `Instant` 原点换算墙钟毫秒。生产引擎不挂 observer，因此不会分配记录。
#[derive(Debug, Clone, serde::Serialize)]
#[cfg(test)]
pub struct SttFinalizeRecord {
    /// `created` = 句尾创建定稿 segment；`transport_start` = 即将发起 worker 请求。
    pub phase: &'static str,
    pub session_generation: u64,
    pub commit_generation: u64,
    pub segment_id: u64,
    #[serde(skip)]
    pub observed_at: Instant,
}

/// 伪流式引擎内部状态。
struct PseudoInner {
    /// VAD 切句器
    vad: EnergyVad,
    /// 句子状态管理（0.22.15 事务化）
    sentences: SentenceState,
    /// 累积音频样本
    samples: Vec<f32>,
    /// 上一次触发预览识别的时刻
    last_preview: Instant,
    /// 上一轮预览的墙钟耗时，用于自适应降频。
    last_preview_elapsed: Duration,
    /// 上一轮预览快照的绝对末尾。
    last_preview_sample_end: usize,
    /// 是否有预览识别请求在飞行中
    preview_in_flight: bool,
    /// 当前在途预览任务的所有者 token（0 = 无）。
    ///
    /// 预览任务完成时只允许**所有者本人**清除 `preview_in_flight`。
    /// 旧实现按 `preview_generation` 相等与否决定清除，句尾递增代际后
    /// 旧任务的清除分支被跳过，标志永久为 true，预览从此彻底停止
    /// （0.24 修复）。owner token 在成功、失败、取消、panic 任何路径
    /// 都由 RAII guard 释放，且旧任务无法清除新任务的所有权。
    preview_owner: u64,
    /// 下一个预览请求 id（单调递增，从 1 开始；0 保留给"无 owner"）。
    next_preview_request: u64,
    /// 最新预览文本
    latest_preview: String,
    /// 当前尾部预览的音频范围（尾部是滚动替换值；短语段的范围在
    /// `preview_phrases` 内）。0.23.14.6：组合预览的范围真源是
    /// 短语账本 + 本字段，对外信封按 span 清单推导，不再维护
    /// "组合文本配最后一个局部范围"的旧字段。
    preview_tail_range: Option<AudioRange>,
    /// 当前预览请求 id，供类型化 Preview 事件投影。
    latest_preview_request_id: u64,
    /// 预览状态版本（预览文本每次实际变化递增）。
    preview_revision: u64,
    /// 预览代际计数器（0.10.6 防重复影子）
    ///
    /// 每次 VAD 句尾时递增。`spawn_preview_recognition` 启动时捕获当前代际，
    /// 返回时校验：若代际不匹配（句尾已发生），说明此预览的音频跨越了句子边界，
    /// 包含已定稿句子的内容，直接丢弃避免覆盖 `latest_preview` 造成重复影子。
    preview_generation: u64,
    /// 引擎状态版本（confirmed 提交或预览变化时递增）。
    ///
    /// 对外状态快照携带本版本号；未变化时 `transcribe_chunk` 返回空串，
    /// 使事件量随"状态变化次数"而非音频块数增长。
    state_revision: u64,
    /// 上一次对外上报的状态版本（None = 尚未上报，首个快照必须上报）。
    last_reported_state: Option<u64>,
    /// 上一次对外上报时的 confirmed 版本。
    ///
    /// 用于判断本次快照是否需要携带完整累计正文——仅 confirmed 真正增长时携带。
    last_reported_confirmed_revision: u64,
    /// PreviewDraft typed delivery 的 span cursor；按顺序逐段交付。
    last_reported_span_count: usize,
    /// PreviewDraft typed delivery 的 preview cursor。
    last_reported_preview_revision: u64,
    /// 未提交音频上限（毫秒）：未提交音频达到该值后强制切段（0.23.7 可配置）。
    ///
    /// 按**绝对未提交音频**计算，与 VAD speaking 状态无关——VAD 的
    /// 软/硬窗口只在有声阶段计时，此上限额外覆盖 VAD 计时停滞的场景。
    max_uncommitted_audio_ms: u64,
    /// 0.23.9 双层识别调度器：唯一调度真源（水位、请求槽、overload、candidate）。
    ///
    /// PseudoInner 通过 coordinator 管理所有调度状态；preview_in_flight /
    /// preview_owner / pending_preview 是 PseudoInner 特有的预览传输层状态，
    /// 不在 coordinator 中重复维护。
    coordinator: RecognitionCoordinator,
    /// VAD 产生普通停顿后先保留候选，等待 Draft 上下文/强停顿门槛。
    ///
    /// 0.23.9：该字段现在同步到 coordinator.candidate，由 coordinator 作为
    /// 唯一真源；observe_boundary_candidate 同时更新两者以保持兼容。
    boundary_candidate: Option<BoundaryCandidate>,
    /// 当前未提交音频中按 audio gate 统计的有效有声样本。
    uncommitted_voiced_samples: u64,
    /// 0.23.14.7 case_17：其中强有声样本（RMS ≥ on 阈值）。与
    /// `uncommitted_voiced_samples` 同窗口累计，供候选携带有声可信度证据。
    uncommitted_strong_samples: u64,
    /// 0.23.14.7 case_17：当前仍在延续的强有声段长度（样本），跨块拼接。
    uncommitted_strong_run_samples: u64,
    /// 0.23.14.7 case_17：本窗口内最长连续强有声段（样本）。
    uncommitted_strong_run_max: u64,
    /// 0.23.14.7 case_17：最近一条已记录候选等待原因——同一候选只有原因
    /// 变化时才补记决策记录，保证"候选被可信度门拒绝"有显式证据而不刷屏。
    decision_wait_reason: Option<&'static str>,
    /// 尚未开始的最新 Preview；running Preview 完成后下一次音频块会取走。
    pending_preview: Option<PendingPreview>,
    /// 是否已发出首个 Preview。首个请求必须至少有 1.2s 有效输入。
    preview_started: bool,
    /// 0.23.9 开启时 transport request 共享单 worker gate。
    single_worker: bool,
    worker_gate: Arc<tokio::sync::Mutex<()>>,
    /// 0.22.15 follow-up: session 失败标志。
    ///
    /// 当内部不变量被破坏（如坐标非法）时设为 `true`。
    /// 设为 true 后，`transcribe_chunk` 和 `finalize` 返回 `SttError`，
    /// 不再处理新音频。`reset` 清除此标志。
    session_failed: bool,
    /// 0.23.14 预览短语 span：完整音频范围 + 冻结文本。范围与 Draft 的
    /// owned_range 同在绝对采样坐标系——settle 按范围精确清退，跨界 span
    /// 整条删除（前缀已被 Draft 消费，剩余部分交回尾部窗口重新识别），
    /// 禁止字符串硬裁剪。仅 PreviewDraft 路径填充，Legacy 恒为空。
    preview_phrases: Vec<PreviewSpan>,
    /// 0.23.14 事务式短语在途登记：识别成功非空且代际有效才提交入账；
    /// error/空文本回退锚点（音频并入下一次短语尝试）；stale（代际推进，
    /// 范围已由 Draft/reset 消费）直接丢弃。单槽——在途期间新短语事件
    /// 不再发起，等下一次触发合并覆盖。
    pending_phrase: Option<PendingPhrase>,
    /// 下一个短语请求 id（单调递增，从 1 开始）。
    next_phrase_request: u64,
    /// 当前短语起点（绝对样本）= 最近一次短语冻结推进的位置与已提交
    /// 水位的较大者。0.23.14 起为投机推进：error/空文本时由
    /// [`PendingPhrase::anchor_before`] 回退。尾部预览窗口锚定于此。
    phrase_anchor: u64,
    /// 尾部（未定稿部分）的最新预览文本；对外 `latest_preview` 为
    /// 短语账本拼接 + 本字段的重算结果。
    preview_tail: String,
    /// 上次 settle 时已见过的 committed 水位；再次推进时才清空尾部。
    preview_settled_committed: u64,
    /// 0.23.14 诊断：拿到 worker gate 后、调用模型前被淘汰的 stale
    /// Preview/Phrase 任务数（不占模型时间的提前淘汰）。
    stale_before_worker: u64,
    /// 0.23.14 诊断：尾部预览文本回退的累计字符数（新版字符数少于旧版
    /// 时累计差值；尾部允许改写，该指标只用于观察回缩频率）。
    preview_retreat_chars: u64,
}

#[derive(Debug, Clone)]
struct PendingPreview {
    samples: Vec<f32>,
    snapshot_end: usize,
    audio_range: AudioRange,
    generation: u64,
}

/// 0.23.14 预览短语账本项：完整音频范围 + 冻结文本。
///
/// 0.23.14.6 起与对外协议的 [`PreviewSegment`] 同型（组合预览的组成段）。
type PreviewSpan = PreviewSegment;

/// 0.23.14 事务式短语在途登记。
#[derive(Debug, Clone)]
struct PendingPhrase {
    request_id: u64,
    /// 冻结前的锚点——error/空文本时的回退目标。
    anchor_before: u64,
    /// 短语完整范围（settle / 账本 / 对外 span 口径）。
    range: AudioRange,
    /// 登记时的预览代际；完成时不匹配即 stale。
    generation: u64,
}

/// 0.23.14.7 case_17：单块音频的强有声结构（连音 vs 脉冲）。
///
/// 与 [`PseudoInner::frames_at_least`] 同 10ms 帧口径，区分"总量"与
/// "连续性"：真语音的音节是连续发声段（连续强帧可长达数百毫秒），而
/// 风扇/键盘这类环境声在能量上可能凑够总量，却只是稀疏短脉冲（连续强帧
/// 多在几十毫秒内断裂）。`lead/trail` 供跨块拼接连续段，`max` 是块内最长段。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct StrongRunProfile {
    /// 本块强有声样本数（RMS ≥ on）。
    strong_samples: u64,
    /// 本块内最长连续强有声段（样本）。
    max_run: u64,
    /// 块首连续强有声段（样本），0 表示块首非强帧。
    lead_run: u64,
    /// 块尾连续强有声段（样本），0 表示块尾非强帧。
    trail_run: u64,
}


impl PseudoInner {
    /// 仅供测试：构造干净的内部状态。
    #[cfg(test)]
    fn for_test(samples: Vec<f32>) -> Self {
        Self {
            vad: EnergyVad::new(16_000),
            sentences: SentenceState::new(),
            samples,
            last_preview: Instant::now(),
            last_preview_elapsed: Duration::ZERO,
            last_preview_sample_end: 0,
            preview_in_flight: false,
            preview_owner: 0,
            next_preview_request: 0,
            latest_preview: String::new(),
            preview_tail_range: None,
            latest_preview_request_id: 0,
            preview_revision: 0,
            preview_generation: 0,
            state_revision: 0,
            last_reported_state: None,
            last_reported_confirmed_revision: 0,
            last_reported_span_count: 0,
            last_reported_preview_revision: 0,
            max_uncommitted_audio_ms: MAX_UNCOMMITTED_AUDIO_MS,
            coordinator: RecognitionCoordinator::new(
                16_000,
                RecognitionProfile::Legacy,
                RecognitionSettings::legacy(),
            ),
            boundary_candidate: None,
            uncommitted_voiced_samples: 0,
            uncommitted_strong_samples: 0,
            uncommitted_strong_run_samples: 0,
            uncommitted_strong_run_max: 0,
            decision_wait_reason: None,
            pending_preview: None,
            preview_started: false,
            single_worker: false,
            worker_gate: Arc::new(tokio::sync::Mutex::new(())),
            session_failed: false,
            preview_phrases: Vec::new(),
            pending_phrase: None,
            next_phrase_request: 0,
            phrase_anchor: 0,
            preview_tail: String::new(),
            preview_settled_committed: 0,
            stale_before_worker: 0,
            preview_retreat_chars: 0,
        }
    }

    /// 提交/回滚一个 finalize 结果，并在 confirmed 实际增长时推进状态版本。
    fn commit_or_rollback(&mut self, result: &FinalizeResult) -> Option<PendingSegment> {
        let before = self.sentences.confirmed_revision;
        let deferred = self.sentences.commit_or_rollback(result);
        if self.sentences.confirmed_revision != before {
            self.state_revision = self.state_revision.wrapping_add(1);
        }
        self.settle_preview_after_commit();
        deferred
    }

    /// 0.23.9.10 / 0.23.14：已提交水位推进后的预览账本收敛——按**音频范围**
    /// 清退被 Draft/NoSpeech 消费的短语：完全覆盖（end ≤ committed）退场；
    /// 跨界 span（start < committed < end）整条删除，剩余部分交回尾部窗口
    /// 重新识别，不做字符串硬裁剪。在途短语同样按范围裁决。锚点不低于已
    /// 提交水位。幂等；所有推进 committed 的路径统一调用。
    ///
    /// 0.23.10.2：committed 水位实际推进（跨过上次 settle 水位）时同步清空
    /// 尾部——尾部覆盖的音频已由 Draft/NoSpeech 消费，保留会在 confirmed
    /// 增长后形成新旧文本重复。清空时机从"边界接受"迁移至此，消除接受到
    /// 提交之间（数百毫秒推理窗口）的预览回缩闪烁。
    fn settle_preview_after_commit(&mut self) {
        let committed = self.sentences.committed_sample_end as u64;
        if committed == 0 {
            return;
        }
        let phrases_before = self.preview_phrases.len();
        // 0.23.14.6：与事件消费方共用同一清退语义（见 settle_preview_segments
        // 的契约注释）——完全覆盖与跨界段整条删除，起点在边界之后的段保留。
        settle_preview_segments(&mut self.preview_phrases, committed);
        if let Some(pending) = &self.pending_phrase
            && pending.range.start_sample < committed
        {
            // 在途范围已被 Draft 覆盖或跨界：作废登记，迟到结果按 stale 丢弃。
            self.pending_phrase = None;
        }
        self.phrase_anchor = self.phrase_anchor.max(committed);
        let committed_advanced = committed > self.preview_settled_committed;
        if committed_advanced {
            self.preview_settled_committed = committed;
            self.preview_tail.clear();
            self.preview_tail_range = None;
        }
        if committed_advanced || self.preview_phrases.len() != phrases_before {
            self.rebuild_preview_text();
        }
    }

    /// NoSpeech 也必须消费 owned range，避免纯静音反复重试撑大 backlog。
    fn consume_no_speech(
        &mut self,
        identity: SegmentIdentity,
        range_end: usize,
    ) -> Option<PendingSegment> {
        let deferred = self.sentences.consume_no_speech(identity, range_end);
        self.settle_preview_after_commit();
        deferred
    }

    fn exceeds_uncommitted_hard_limit(
        &self,
        total: usize,
        committed_end: usize,
        sample_rate: u32,
    ) -> Option<bool> {
        let max_samples = (self.max_uncommitted_audio_ms * sample_rate as u64 / 1000) as usize;
        total
            .checked_sub(committed_end)
            .map(|uncommitted| uncommitted >= max_samples)
    }

    /// 在句尾把 VAD 状态对齐到实际切点。
    ///
    /// 强制切点可能从当前 `total` 回退到历史谷底。那段回退后的 PCM 仍会
    /// 留给下一次定稿，但已经被本轮 VAD 消费过；重置句子计数后重放这段
    /// 尾音，才能让下一句的句长、段长和 speaking/silence 状态与真实边界
    /// 一致。`reset_sentence` 保留 adaptive noise history 和 speaking 状态，
    /// 因此连续语音的回退尾段仍可自然接续。
    fn reset_vad_at_boundary(
        &mut self,
        boundary_total: usize,
        total: usize,
    ) -> Result<(), &'static str> {
        self.vad.reset_sentence();
        if boundary_total == total {
            return Ok(());
        }

        let local_range = self
            .sentences
            .abs_to_local_range(&(boundary_total..total), self.samples.len())
            .ok_or("回退边界的 VAD 重放坐标非法")?;
        let replay = self.samples[local_range].to_vec();
        // `replay_chunk` 只恢复句子状态，不重复写入 adaptive energy history
        // 或更新 noise floor；这些帧已经在本轮正常 process_chunk 中消费过。
        self.vad.replay_chunk(&replay);
        Ok(())
    }

    /// 更新预览文本（仅在实际变化时推进状态与预览版本）。
    fn set_preview_if_changed(&mut self, preview: String, range: AudioRange, request_id: u64) {
        // 0.23.14.7 P1-1：尾部候选范围与已入账内容的重叠终审。短语冻结不推进
        // preview_generation（只有 Draft 边界/reset/终态推进），冻结前起飞的
        // 预览不会被代际墙拦住；迟到结果若与短语账本或已提交水位重叠，说明
        // 其文本包含已入账音频——尾部是整串替换值，无法按字符对齐裁剪，
        // 整条丢弃，未覆盖后缀交回尾部窗口（锚点已推进）重识别。这是
        // `明确的多句话。明确的多句话。` 重复投影的第二道闸（第一道是
        // 短语入账时的 settle_tail_for_phrase）。
        if self.tail_range_is_stale(&range) {
            tracing::debug!(
                request_id,
                start = range.start_sample,
                end = range.end_sample,
                "丢弃与已入账内容重叠的迟到预览（范围裁决）"
            );
            return;
        }
        if preview.is_empty() || preview == self.preview_tail {
            return;
        }
        // 0.23.14 诊断：尾部回退字符数（尾部允许改写，只统计不阻断）。
        let old_chars = self.preview_tail.chars().count();
        let new_chars = preview.chars().count();
        if new_chars < old_chars {
            self.preview_retreat_chars = self
                .preview_retreat_chars
                .saturating_add((old_chars - new_chars) as u64);
        }
        self.preview_tail = preview;
        self.preview_tail_range = Some(range);
        self.latest_preview_request_id = request_id;
        self.rebuild_preview_text();
    }

    /// 0.23.14.7 P1-1：尾部候选范围是否已被短语账本/已提交水位覆盖。
    ///
    /// 尾部窗口锚定在 `phrase_anchor`（≥ 全部短语终点与已提交水位），正常
    /// 路径不会重叠；重叠只可能来自冻结前起飞、迟到返回的预览——其文本
    /// 必然包含已由 phrase/Draft 消费的音频。
    fn tail_range_is_stale(&self, range: &AudioRange) -> bool {
        let committed = self.sentences.committed_sample_end as u64;
        if range.start_sample < committed || range.end_sample <= committed {
            return true;
        }
        self.preview_phrases
            .iter()
            .any(|phrase| range.overlaps(phrase.range))
    }

    /// 0.23.14.7 P1-1：冻结短语入账时同步清退与该范围重叠的尾部预览。
    ///
    /// 尾部与短语都锚定同一个 `phrase_anchor` 起步，短语覆盖 [anchor,
    /// quiet_start)，冻结前显示的尾部必然投影了同一音频——不同步清退就会
    /// 出现同一文本双份（case_12 实测时间线）。按音频范围裁决（范围是
    /// 唯一真源，不做字符串相似度/前缀裁剪）：
    /// - 完全覆盖（tail.end ≤ phrase_end）：整条清退，文本由短语 span 接管；
    /// - 跨界（tail.start < phrase_end < tail.end）：整条清退，未覆盖后缀
    ///   [phrase_end, tail.end) 交回尾部窗口重识别——与 Draft settle 的
    ///   "跨界整条删除、剩余交回尾部窗口"同一语义。
    /// 短语之后的真实后缀不靠残留 tail 保留，靠锚点推进后的下一次尾部
    /// 预览重新投影（`retreated_boundary_preserves_uncommitted_preview_suffix`
    /// 的既有语义）。
    fn settle_tail_for_phrase(&mut self, phrase_end: u64) {
        let Some(range) = self.preview_tail_range else {
            return;
        };
        if range.start_sample < phrase_end {
            self.preview_tail.clear();
            self.preview_tail_range = None;
            tracing::debug!(
                phrase_end,
                tail_end = range.end_sample,
                "短语入账：清退与其重叠的尾部预览"
            );
        }
    }

    /// 重算对外预览文本 = 已定稿短语拼接 + 当前尾部。
    ///
    /// 0.23.9.10：预览从"单个可替换值"变为"短语账本 + 尾部"的组合视图，
    /// 但对外契约不变——仍是一个可替换字符串（`latest_preview`），消费方
    /// 无需感知内部分层。文本实际变化才推进版本（边沿触发）。
    fn rebuild_preview_text(&mut self) {
        let mut composed = String::new();
        for span in &self.preview_phrases {
            composed.push_str(&span.text);
        }
        composed.push_str(&self.preview_tail);
        if composed == self.latest_preview {
            return;
        }
        self.latest_preview = composed;
        self.preview_revision = self.preview_revision.wrapping_add(1);
        self.state_revision = self.state_revision.wrapping_add(1);
        tracing::trace!(
            preview_revision = self.preview_revision,
            chars = self.latest_preview.chars().count(),
            phrases = self.preview_phrases.len(),
            "预览版本变化"
        );
    }

    /// 清空尾部预览（句尾/终态）；短语账本保留到真实 Draft 提交覆盖。
    ///
    /// 0.23.9.10：保留 `latest_preview_request_id`——清空信封的 requestId
    /// 取 `max(request_id, revision)`，归零会让信封携带倒退的 id，被消费方
    /// 的 request-id 墙当作过期丢弃，句尾旧虚字因此残留。
    fn clear_preview(&mut self) {
        self.preview_tail.clear();
        self.preview_tail_range = None;
        self.rebuild_preview_text();
    }

    /// 释放预览所有者（仅 owner 本人可释放）。
    fn release_preview_owner(&mut self, request_id: u64) {
        if self.preview_in_flight && self.preview_owner == request_id {
            self.preview_in_flight = false;
            self.preview_owner = 0;
        }
    }

    /// 0.22.15 follow-up: 标记 session 为失败态。
    ///
    /// 检测到内部不变量破坏时调用。失败后 session 不再处理新音频，
    /// 直到 `reset` 清除失败态。日志只含结构化数值，不含音频/转写正文。
    fn mark_session_failed(&mut self, reason: &str) {
        if !self.session_failed {
            tracing::error!(
                reason = reason,
                committed_end = self.sentences.committed_sample_end,
                buffer_base = self.sentences.buffer_base_sample,
                samples_len = self.samples.len(),
                pending = self.sentences.pending.is_some(),
                deferred = self.sentences.deferred.is_some(),
                finalize_in_flight = self.sentences.finalize_in_flight,
                terminal_finalizing = self.sentences.finalizing.is_some(),
                preview_in_flight = self.preview_in_flight,
                "STT session 进入失败态"
            );
        }
        self.session_failed = true;
    }

    /// 在途推理任务数（预览 + 定稿 + 排队句段 + 在途短语；诊断用）。
    fn in_flight_inferences(&self) -> usize {
        usize::from(self.preview_in_flight)
            + usize::from(self.sentences.finalize_in_flight)
            + usize::from(self.sentences.pending.is_some())
            + usize::from(self.sentences.deferred.is_some())
            + usize::from(self.pending_preview.is_some())
            + usize::from(self.pending_phrase.is_some())
    }

    /// 按与 RequestAudioGate 相同的 10ms frame 统计有效有声样本。
    fn voiced_samples(samples: &[f32], off_threshold: f64, sample_rate: u32) -> u64 {
        Self::frames_at_least(samples, off_threshold, sample_rate)
    }

    /// 0.23.14.7 case_17：统计强有声样本（RMS ≥ threshold）——与
    /// [`Self::voiced_samples`] 同帧口径，阈值传 on 即得强帧数。
    fn frames_at_least(samples: &[f32], threshold: f64, sample_rate: u32) -> u64 {
        if samples.is_empty() || sample_rate == 0 {
            return 0;
        }
        let frame_size = (u64::from(sample_rate) / 100).max(1) as usize;
        let threshold = if threshold.is_finite() {
            threshold.max(0.0)
        } else {
            0.0
        };
        let mut voiced = 0u64;
        let mut offset = 0usize;
        while offset < samples.len() {
            let end = (offset + frame_size).min(samples.len());
            let frame = &samples[offset..end];
            let rms = (frame
                .iter()
                .map(|sample| {
                    let value = f64::from(*sample);
                    if value.is_finite() {
                        value * value
                    } else {
                        0.0
                    }
                })
                .sum::<f64>()
                / frame.len() as f64)
                .sqrt();
            if rms >= threshold {
                voiced = voiced.saturating_add((end - offset) as u64);
            }
            offset = end;
        }
        voiced
    }

    /// 0.23.14.7 case_17：单块音频的强有声结构（连音 vs 脉冲）。
    ///
    /// 与 [`Self::frames_at_least`] 同 10ms 帧口径，区分"总量"与"连续性"：
    /// 真语音的音节是连续发声段（连续强帧可长达数百毫秒），而风扇/键盘
    /// 这类环境声在能量上可能凑够总量，却只是稀疏短脉冲（连续强帧多在
    /// 几十毫秒内断裂）。`lead/trail` 供跨块拼接连续段，`max` 是块内最长段。
    fn strong_run_profile(samples: &[f32], threshold: f64, sample_rate: u32) -> StrongRunProfile {
        let mut profile = StrongRunProfile::default();
        if samples.is_empty() || sample_rate == 0 {
            return profile;
        }
        let frame_size = (u64::from(sample_rate) / 100).max(1) as usize;
        let threshold = if threshold.is_finite() {
            threshold.max(0.0)
        } else {
            0.0
        };
        let mut run = 0u64;
        let mut seen_gap = false;
        let mut offset = 0usize;
        while offset < samples.len() {
            let end = (offset + frame_size).min(samples.len());
            let frame = &samples[offset..end];
            let rms = (frame
                .iter()
                .map(|sample| {
                    let value = f64::from(*sample);
                    if value.is_finite() {
                        value * value
                    } else {
                        0.0
                    }
                })
                .sum::<f64>()
                / frame.len() as f64)
                .sqrt();
            let len = (end - offset) as u64;
            if rms >= threshold {
                profile.strong_samples = profile.strong_samples.saturating_add(len);
                run = run.saturating_add(len);
                profile.max_run = profile.max_run.max(run);
            } else {
                if !seen_gap {
                    profile.lead_run = run;
                }
                seen_gap = true;
                run = 0;
            }
            offset = end;
        }
        // 整块无间隔时 lead 即块首段（= run 全程），trail 同值。
        if !seen_gap {
            profile.lead_run = run;
        }
        profile.trail_run = run;
        profile
    }

    /// 0.23.14.6 时间冻结切点：枚举前缀范围内的低能量谷候选。
    ///
    /// 按 10ms frame 扫描，返回每个 ≥120ms 连续低能量段（谷）的**起点**
    /// （绝对采样，升序）。调用方从新到旧寻找"切点前已满足有声门槛"的
    /// 合格谷——不合格的早期谷（切点前有效有声不足）不得阻塞对后续谷
    /// 的考察，也不能永久卡住时间冻结（0.23.14.5 的缺陷：总是选最后一
    /// 个谷 + TooShort 不动锚点 → 无新谷时每次扫描重复同一无效切点）。
    fn low_energy_valley_candidates(
        samples: &[f32],
        range_start_abs: u64,
        off_threshold: f64,
        sample_rate: u32,
    ) -> Vec<u64> {
        const MIN_DIP_MS: u64 = 120;
        let frame = (u64::from(sample_rate) / 100).max(1) as usize;
        let min_frames = ((MIN_DIP_MS * u64::from(sample_rate) / 1000) as usize) / frame;
        let threshold = if off_threshold.is_finite() {
            off_threshold.max(0.0)
        } else {
            0.0
        };
        let frame_rms = |chunk: &[f32]| -> f64 {
            (chunk
                .iter()
                .map(|sample| {
                    let value = f64::from(*sample);
                    if value.is_finite() {
                        value * value
                    } else {
                        0.0
                    }
                })
                .sum::<f64>()
                / chunk.len() as f64)
                .sqrt()
        };
        let mut candidates = Vec::new();
        let mut quiet_run_start: Option<usize> = None;
        let mut frame_idx = 0usize;
        for chunk in samples.chunks(frame) {
            if frame_rms(chunk) < threshold {
                quiet_run_start.get_or_insert(frame_idx);
            } else if let Some(start) = quiet_run_start.take()
                && frame_idx - start >= min_frames
            {
                candidates.push(start);
            }
            frame_idx += 1;
        }
        // 尾部谷（range 末尾仍是低能量）同样有效。
        if let Some(start) = quiet_run_start
            && frame_idx - start >= min_frames
        {
            candidates.push(start);
        }
        candidates.into_iter().map(|start| range_start_abs + (start * frame) as u64).collect()
    }

    /// 0.23.14.6 稳健谷内切点偏移：切点不落在能量刚跌破阈值的下降沿，
    /// 而是谷内约 30ms 平滑 RMS 的最低平台中点——轻声尾音、擦音和自然
    /// 衰减得以保留（`trim_trailing_silence` 的 150ms 尾部缓冲不再被上游
    /// 预先切掉）。
    ///
    /// 返回相对谷起点的采样偏移，约束在 [30ms, 150ms]（谷本身不足 30ms
    /// 时取谷长的一半）。不用单个最低采样点——数字零点/爆音/随机噪声
    /// 容易把逐样本最低点拉偏；10ms frame + 3 帧（约 30ms）平滑后的最低
    /// 平台抗脉冲。
    fn robust_in_valley_offset(valley_samples: &[f32], sample_rate: u32) -> usize {
        const MIN_OFFSET_MS: u64 = 30;
        const MAX_OFFSET_MS: u64 = 150;
        let frame = (u64::from(sample_rate) / 100).max(1) as usize;
        let min_offset = (MIN_OFFSET_MS * u64::from(sample_rate) / 1000) as usize;
        let max_offset = (MAX_OFFSET_MS * u64::from(sample_rate) / 1000) as usize;
        if valley_samples.len() <= min_offset {
            return valley_samples.len() / 2;
        }
        // 1) 10ms frame RMS。
        let mut frame_rms: Vec<f64> = Vec::with_capacity(valley_samples.len() / frame + 1);
        for chunk in valley_samples.chunks(frame) {
            let rms = (chunk
                .iter()
                .map(|sample| {
                    let value = f64::from(*sample);
                    if value.is_finite() {
                        value * value
                    } else {
                        0.0
                    }
                })
                .sum::<f64>()
                / chunk.len() as f64)
                .sqrt();
            frame_rms.push(rms);
        }
        // 2) 约 30ms（3 帧）居中平滑；边界帧取可用窗口均值。
        let smoothed: Vec<f64> = (0..frame_rms.len())
            .map(|i| {
                let lo = i.saturating_sub(1);
                let hi = (i + 2).min(frame_rms.len());
                frame_rms[lo..hi].iter().sum::<f64>() / (hi - lo) as f64
            })
            .collect();
        // 3) 最低平台：与全局最小同水平的连续帧，取中点。
        let min_value = smoothed.iter().copied().fold(f64::INFINITY, f64::min);
        let mut best_start = 0usize;
        let mut best_len = 0usize;
        let mut run_start: Option<usize> = None;
        let tolerance = min_value.max(1e-12) * 1.05;
        for (i, value) in smoothed.iter().enumerate() {
            if *value <= tolerance {
                run_start.get_or_insert(i);
            } else if let Some(start) = run_start.take() {
                let len = i - start;
                if len > best_len {
                    best_len = len;
                    best_start = start;
                }
            }
        }
        if let Some(start) = run_start {
            let len = smoothed.len() - start;
            if len > best_len {
                best_len = len;
                best_start = start;
            }
        }
        let plateau_mid = (best_start + best_len / 2) * frame;
        plateau_mid.clamp(min_offset, max_offset)
    }

    /// 把切点从谷起点（能量刚跌破阈值处）推进到谷内稳健位置。
    ///
    /// `valley_samples` 是从谷起点开始的音频；返回值仍以绝对采样表达。
    /// 谷内偏移受 150ms 上限约束，送模尾部低能量不会突破
    /// `trim_trailing_silence` 的既有缓冲语义。
    fn valley_start_to_in_valley_cut(
        valley_start: u64,
        valley_samples: &[f32],
        sample_rate: u32,
    ) -> u64 {
        valley_start + Self::robust_in_valley_offset(valley_samples, sample_rate) as u64
    }

    /// 把 VAD 停顿的谷起点切点推进到谷内稳健位置（实例方法：读取缓冲内
    /// [quiet_start, total) 的真实谷音频）。坐标越界/空谷时保持原切点。
    fn in_valley_boundary(&self, quiet_start: u64, total: usize, sample_rate: u32) -> u64 {
        let start = (quiet_start as usize).min(total);
        if start >= total {
            return quiet_start;
        }
        // 谷内搜索取到 total 的整段（偏移上限 150ms 约束结果；长谷的
        // plateau 中点不受窗口影响）。
        let Some(local) = self
            .sentences
            .abs_to_local_range(&(start..total), self.samples.len())
        else {
            return quiet_start;
        };
        Self::valley_start_to_in_valley_cut(quiet_start, &self.samples[local], sample_rate)
    }

    /// 0.23.14.6 时间冻结切点选择（纯函数，供单测直接驱动）：
    ///
    /// 1. 枚举前缀内 ≥120ms 低能量谷候选（升序）；
    /// 2. 从新到旧找"切点前有效有声 ≥ `PHRASE_MIN_VOICED_MS`"的合格谷——
    ///    不合格的早期谷跳过，不得阻塞后续谷，也不能永久卡住时间冻结
    ///    （0.23.14.5 缺陷：总选最后一个谷 + TooShort 不动锚点）；
    /// 3. 合格谷的切点推进到谷内稳健位置（30–150ms，
    ///    [`Self::robust_in_valley_offset`]），不贴刚跌破阈值的下降沿；
    /// 4. 无合格谷回退 `roll_start` 安全切点（词中硬切交事务式锚点回退兜底）。
    fn choose_time_freeze_cut(
        prefix_samples: &[f32],
        anchor_before: u64,
        roll_start: u64,
        off_threshold: f64,
        sample_rate: u32,
    ) -> u64 {
        let candidates = Self::low_energy_valley_candidates(
            prefix_samples,
            anchor_before,
            off_threshold,
            sample_rate,
        );
        let min_voiced_samples = PHRASE_MIN_VOICED_MS * u64::from(sample_rate) / 1000;
        let mut chosen_valley = None;
        for &valley_start in candidates.iter().rev() {
            if valley_start <= anchor_before {
                continue;
            }
            let voiced_before = Self::voiced_samples(
                &prefix_samples[..(valley_start - anchor_before) as usize],
                off_threshold,
                sample_rate,
            );
            if voiced_before >= min_voiced_samples {
                chosen_valley = Some(valley_start);
                break;
            }
        }
        match chosen_valley {
            Some(valley_start) => {
                let valley_samples = &prefix_samples[(valley_start - anchor_before) as usize..];
                Self::valley_start_to_in_valley_cut(valley_start, valley_samples, sample_rate)
            }
            None => roll_start,
        }
    }

    /// 记录 VAD 候选而不立即创建 Draft。普通句尾先保留，后续静默帧
    /// 可能把候选升级为 strong pause；恢复有声时该候选作废。
    fn observe_boundary_candidate(
        &mut self,
        event: VadEvent,
        total: usize,
        chunk: &[f32],
        off_threshold: f64,
        sample_rate: u32,
    ) -> Option<u64> {
        let voiced = Self::voiced_samples(chunk, off_threshold, sample_rate);
        self.uncommitted_voiced_samples = self.uncommitted_voiced_samples.saturating_add(voiced);
        // 0.23.14.7 case_17：同帧口径累计强有声样本（RMS ≥ on），候选据此
        // 携带有声可信度证据——稳态噪声多数帧落在 on/off 滞回带，真语音的
        // 音节峰值大量越过 on。除总量外同时累计**最长连续强有声段**：
        // 环境声即使总量够，也只能凑出稀疏短脉冲，真音节是连续发声段。
        let on_threshold = self.vad.current_on_threshold();
        let profile = Self::strong_run_profile(chunk, on_threshold, sample_rate);
        self.uncommitted_strong_samples = self
            .uncommitted_strong_samples
            .saturating_add(profile.strong_samples);
        // 跨块拼接：块首强段与上一块末尾强段相接时合并为一段。整块全强时
        // 当前段必须延续（否则连续段被截断在单块长度，长音节永远测不出来）。
        let chunk_samples = chunk.len() as u64;
        let bridged = if profile.lead_run > 0 {
            self.uncommitted_strong_run_samples
                .saturating_add(profile.lead_run)
        } else {
            0
        };
        self.uncommitted_strong_run_max = self
            .uncommitted_strong_run_max
            .max(profile.max_run)
            .max(bridged);
        self.uncommitted_strong_run_samples = if profile.strong_samples > 0
            && profile.strong_samples == chunk_samples
        {
            bridged.max(profile.trail_run)
        } else {
            profile.trail_run
        };

        // 0.23.14.7：自然句尾采纳判据的常量先于 &mut 借用取出（见下方复语
        // 分支与 natural_sentence_adoptable）。
        let min_sentence_ms = self.vad.min_sentence_ms();

        // 0.23.10.2：短句停顿（句长不足 min_sentence）被 VAD 丢弃——它永远
        // 不会升级为 Draft 切割，无需等复语确认，立即作为短语定稿信号返回。
        // 修复前短首句既进不了短语账本也凑不满首预览 1.2s 门槛，首个可见
        // 文本被推迟到后续语音累计之后（实测 4s+）。
        //
        // 0.23.14：短句同时登记 `short_phrase` 候选——短语冻结只解决预览，
        // 可靠 Draft 需要候选升级路径。后续静默块持续累计 quiet_samples，
        // 达到 long_pause_ms 即按长静音规则接受；复语则候选作废（锚点已
        // 在短语冻结时推进，作废信号为无操作）。
        if event == VadEvent::ShortPhraseEnd {
            let silence_samples = self.vad.dump_state().silence_samples;
            let quiet_start = total.saturating_sub(silence_samples);
            // 0.23.14.6：切点推进到谷内稳健位置（30–150ms）——短句自然
            // 衰减尾音保留给短语识别，不再贴着下降沿切断。
            let boundary = self.in_valley_boundary(quiet_start as u64, total, sample_rate);
            let candidate = BoundaryCandidate {
                boundary_sample: boundary,
                quiet_start_sample: boundary,
                reason: "short_phrase".to_string(),
                voiced_samples: self.uncommitted_voiced_samples,
                strong_samples: self.uncommitted_strong_samples,
                strong_run_max_samples: self.uncommitted_strong_run_max,
                quiet_samples: silence_samples as u64,
            };
            self.boundary_candidate = Some(candidate.clone());
            self.coordinator.set_candidate(candidate);
            return Some(boundary);
        }

        if event.is_boundary() {
            let vad_state = self.vad.dump_state();
            let quiet_samples = match event {
                VadEvent::HardWindow => 0,
                VadEvent::SoftWindow => vad_state.soft_silence_samples,
                VadEvent::SentenceEnd => vad_state.silence_samples,
                // ShortPhraseEnd 在进入本分支前已被单独处理，不会到达这里。
                VadEvent::ShortPhraseEnd | VadEvent::None => 0,
            } as u64;
            let quiet_start_sample = total.saturating_sub(quiet_samples as usize);
            // 0.23.14.6：普通句尾/软窗停顿的切点同样推进到谷内稳健位置
            // （hard/cap 无静音可依，保持当前时刻）。短语与 Draft 因此携带
            // ≤150ms 的自然衰减尾部，`trim_trailing_silence` 的尾部缓冲
            // 不再被上游预先切掉。
            let boundary_sample = if quiet_samples > 0 {
                self.in_valley_boundary(quiet_start_sample as u64, total, sample_rate)
            } else {
                quiet_start_sample as u64
            };
            let candidate = BoundaryCandidate {
                boundary_sample,
                quiet_start_sample: boundary_sample,
                reason: event.reason().to_string(),
                voiced_samples: self.uncommitted_voiced_samples,
                strong_samples: self.uncommitted_strong_samples,
                strong_run_max_samples: self.uncommitted_strong_run_max,
                quiet_samples,
            };
            self.boundary_candidate = Some(candidate.clone());
            self.coordinator.set_candidate(candidate);
            return None;
        }

        let Some(candidate) = self.boundary_candidate.as_mut() else {
            return None;
        };
        if voiced > 0 {
            // 0.23.14.7 复语块裁决：起音块头部的静默仍是停顿的一部分——块内
            // 出现有声帧时整块按有声处理会让 gate 口径的停顿系统性短于真实
            // 值（case_12 实测体感 900ms 的停顿 quiet 只累计到 570ms），这里
            // 把块内静默样本补计入 quiet 再裁决。满足自然采纳条件的 VAD
            // 句尾候选保留给本轮 readiness 采纳（切点仍是创建时的谷内位置，
            // 复语音频经 reset_vad_at_boundary 重放归入下一段）；其余候选
            // 照旧作废并触发短语冻结。
            candidate.quiet_samples = candidate
                .quiet_samples
                .saturating_add((chunk.len() as u64).saturating_sub(voiced));
            if candidate.reason != "natural_silence"
                || !Self::natural_sentence_adoptable(candidate, sample_rate, min_sentence_ms)
            {
                // 普通候选遇到新的有效语音即作废；下一次 VAD 停顿会创建
                // 新候选。0.23.9.10：作废即"预览定稿"信号——该停顿没有
                // 升级为真实切割，[phrase_anchor, quiet_start) 作为短语
                // 冻结进预览账本。
                let invalidated_quiet_start = candidate.quiet_start_sample;
                self.boundary_candidate = None;
                self.coordinator.clear_candidate();
                return Some(invalidated_quiet_start);
            }
            // 保留候选：本轮 readiness 的自然句尾分支立即采纳。
            return None;
        }
        candidate.quiet_samples = candidate.quiet_samples.saturating_add(chunk.len() as u64);
        candidate.boundary_sample = candidate.quiet_start_sample;
        self.coordinator.set_candidate(candidate.clone());
        None
    }

    /// 0.23.14.7 case_17 自然句尾候选是否满足采纳条件：停顿达到
    /// `NATURAL_PAUSE_MIN_MS`、有效有声与 VAD 的 `min_sentence_ms` 句长
    /// 校验同源对齐，且**存在可信的连续发声段**（连续强有声 ≥
    /// `CREDIBLE_VOICED_RUN_MIN_MS`）。`candidate_readiness` 的自然分支与
    /// 复语保留裁决（`observe_boundary_candidate`）共用同一判据，避免双份
    /// 阈值漂移。
    ///
    /// 0.23.14.7 case_17 修复：风扇/键盘稳态噪声可以被 EnergyVad 当成持续
    /// 有声（≥ min_sentence）并在 400ms 停顿后由自然句尾分支放行。能量口径
    /// 的占比判据区分度不足（实测伪文本候选 strong/voiced = 32.1%，而合法
    /// 噪声候选 case_11 仅 21.9%，真句最弱 38.5%——区间重叠，任何占比门槛
    /// 都会误伤）；改用**形态**证据：真音节是连续发声段，环境声只是稀疏
    /// 脉冲（实测 50ms vs 真句最低 120ms）。
    ///
    /// 不可信候选降级回既有路径（继续等待 → long_pause 兜底或复语作废），
    /// 且两类 pause 分支与送模范围门共用同一判据，不会"换个分支再采纳一次"。
    fn natural_sentence_adoptable(
        candidate: &BoundaryCandidate,
        sample_rate: u32,
        min_sentence_ms: u32,
    ) -> bool {
        let sr = u64::from(sample_rate);
        candidate.quiet_samples >= NATURAL_PAUSE_MIN_MS.saturating_mul(sr) / 1000
            && candidate.voiced_samples >= u64::from(min_sentence_ms).saturating_mul(sr) / 1000
            && Self::credible_voicing_run(candidate.strong_run_max_samples, sample_rate)
    }

    /// 0.23.14.7 case_17：连续强有声段是否达到可信发声下限。
    fn credible_voicing_run(run_samples: u64, sample_rate: u32) -> bool {
        run_samples >= CREDIBLE_VOICED_RUN_MIN_MS.saturating_mul(u64::from(sample_rate)) / 1000
    }

    /// 0.23.14.7 case_17：音频范围内是否存在可信发声证据（仅测试消费）。
    ///
    /// 生产路径不用"当下阈值重新测量范围"——VAD 的 on 阈值随底噪自适应，
    /// 尾段静音会让底噪漂移，同一段音频在不同时刻得出矛盾结论（远场低声
    /// 实测 290ms vs 90ms）。生产一律复用流式累计的
    /// [`PseudoInner::uncommitted_strong_run_max`]（见 `finalize_with_wait_timeout`）。
    /// 本函数保留给单元测试直接断言"连续发声段"语义。
    #[cfg(test)]
    fn range_has_credible_voicing(samples: &[f32], on_threshold: f64, sample_rate: u32) -> bool {
        let profile = Self::strong_run_profile(samples, on_threshold, sample_rate);
        Self::credible_voicing_run(profile.max_run, sample_rate)
    }

    /// 返回 `Ok(via)` = 采纳，`via` 标明放行分支（诊断证据链）；
    /// `Err(reason)` = 继续等待。
    fn candidate_readiness(
        &self,
        candidate: &BoundaryCandidate,
        total: usize,
        sample_rate: u32,
    ) -> Result<AcceptedVia, &'static str> {
        let sample_rate = u64::from(sample_rate);
        if sample_rate == 0 {
            return Err("invalid_sample_rate");
        }
        let reserved = self
            .sentences
            .draft_reserved_sample_end
            .max(self.sentences.committed_sample_end);
        let end = (candidate.boundary_sample as usize).min(total);
        let owned_samples = end.saturating_sub(reserved) as u64;
        let min_draft_samples = self
            .coordinator
            .settings
            .draft_min_s
            .saturating_mul(sample_rate);
        let min_strong_owned = 2 * sample_rate;
        let min_strong_voiced = 1_200 * sample_rate / 1000;
        if candidate.reason == "hard_window" {
            return (owned_samples > 0)
                .then_some(AcceptedVia::HardWindow)
                .ok_or("below_draft_min");
        }
        if candidate.reason == "uncommitted_cap" {
            return (owned_samples > 0)
                .then_some(AcceptedVia::UncommittedCap)
                .ok_or("below_draft_min");
        }
        if owned_samples >= min_draft_samples {
            return Ok(AcceptedVia::DraftMin);
        }
        // 0.23.14 长静音独立终结：停顿达到 long_pause_ms 且存在可信有声即可
        // 接受，不再要求 owned ≥ 2s / voiced ≥ 1.2s——0.8～2 秒短句后的
        // 长静音也必须产生可靠 Draft，而不是停在 Preview 等复语。可信有声
        // 下限挡住点击/呼吸级短脉冲；更长的噪声由模型 NoSpeech 结果消费，
        // 不产生 span。句内 200～500ms 停顿（< strong_pause_ms）不受影响。
        let long_pause_samples = self
            .coordinator
            .settings
            .long_pause_ms
            .saturating_mul(sample_rate)
            / 1000;
        if candidate.quiet_samples >= long_pause_samples {
            // 0.23.14.7 case_17：长静音兜底同样要求可信连续发声——否则被自然
            // 句尾分支拒绝的噪声候选会静默增长 quiet，到 1100ms 后由本分支
            // 再次放行（"换个分支再采纳一次"），修复等同无效。
            if !Self::credible_voicing_run(candidate.strong_run_max_samples, sample_rate as u32) {
                return Err("long_pause_voiced_not_credible");
            }
            let min_voiced = LONG_PAUSE_MIN_VOICED_MS.saturating_mul(sample_rate) / 1000;
            if candidate.voiced_samples >= min_voiced {
                return Ok(AcceptedVia::LongPause);
            }
            return Err("long_pause_voiced_too_short");
        }
        // 0.23.14.7 自然句尾独立采纳：`reason == natural_silence` 表示 VAD 已
        // 完成 `SentenceEnd` 的 min_sentence_ms 有声校验——该候选携带的
        // "自然句"证据足以替代 generic strong pause 的 owned ≥ 2s / voiced
        // ≥ 1.2s 硬门槛。停顿达到 NATURAL_PAUSE_MIN_MS 且有效有声与 VAD
        // 句长校验同源对齐（≥ min_sentence_ms）即接受，消除 case_12 实测的
        // 门槛夹缝（1.79s 句 + 门控口径约 570ms 停顿：strong 差 voiced、
        // long 差 quiet，两边都够不着，最终与下一句合并）。点击/咳嗽/呼吸
        // 仍被 voiced ≥ min_sentence_ms 挡住；慢速词间停顿跟随
        // ShortPhraseEnd 候选，不进本分支。generic strong pause 规则对无
        // 自然句尾证据的候选（soft_window/short_phrase）保持原样。
        // case_17 修复：采纳同时要求有声可信度（见 `natural_sentence_adoptable`）
        // ——稳态噪声候选被拒绝并留下 `natural_sentence_voiced_not_credible`
        // 证据；静默继续累计可走 long_pause 兜底，复语作废。
        // 0.23.14.7 case_17：不可信候选（无连续发声段）在这里被拒——判据见
        // `natural_sentence_adoptable`；后续 long_pause 分支共用同一判据。
        if candidate.reason == "natural_silence" {
            let min_sentence_ms = self.vad.min_sentence_ms();
            if Self::natural_sentence_adoptable(candidate, sample_rate as u32, min_sentence_ms) {
                return Ok(AcceptedVia::NaturalSentence);
            }
            let natural_pause_samples = NATURAL_PAUSE_MIN_MS.saturating_mul(sample_rate) / 1000;
            if candidate.quiet_samples < natural_pause_samples {
                return Err("below_natural_pause");
            }
            if candidate.voiced_samples
                < u64::from(min_sentence_ms).saturating_mul(sample_rate) / 1000
            {
                return Err("natural_sentence_voiced_too_short");
            }
            return Err("natural_sentence_voiced_not_credible");
        }
        if !candidate.is_strong(
            sample_rate as u32,
            self.coordinator.settings.strong_pause_ms,
        ) {
            return Err("below_draft_min");
        }
        if owned_samples < min_strong_owned {
            return Err("strong_pause_owned_too_short");
        }
        if candidate.voiced_samples < min_strong_voiced {
            return Err("strong_pause_voiced_too_short");
        }
        Ok(AcceptedVia::StrongPause)
    }

    /// 0.23.14 在途短语按失败收场：作废登记并把锚点回退到冻结前位置，
    /// 音频交回下一次短语尝试合并覆盖（error/空文本/无通道统一路径）。
    fn fail_pending_phrase(&mut self, request_id: u64, anchor_before: u64) {
        if self
            .pending_phrase
            .as_ref()
            .is_some_and(|p| p.request_id == request_id)
        {
            self.pending_phrase = None;
            self.phrase_anchor = self.phrase_anchor.min(anchor_before);
        }
    }

    fn clear_candidate(&mut self) {
        self.boundary_candidate = None;
        self.decision_wait_reason = None;
        self.coordinator.clear_candidate();
    }
}

/// 预览任务所有者守卫：任何退出路径（成功/错误/取消/panic）都释放 owner。
struct PreviewOwnerGuard {
    inner: Arc<Mutex<PseudoInner>>,
    request_id: u64,
}

impl Drop for PreviewOwnerGuard {
    fn drop(&mut self) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.release_preview_owner(self.request_id);
        }
    }
}

impl PseudoStreamingSttEngine {
    /// 从 `SttEngineConnection` 创建伪流式 STT 引擎。
    ///
    /// 连接快照必须携带 worker transport（GGUF 常驻 worker 是唯一本地实现；
    /// 无 transport 的连接是上游接线错误）。就绪由 start 时的 ready 握手
    /// 保证——这里不做端口探测。
    pub fn from_connection(
        config: &crate::domain::config::stt_config::SttConfig,
        conn: crate::domain::stt::SttEngineConnection,
    ) -> Result<Self, String> {
        Self::from_connection_with_profile(config, conn, RecognitionProfile::PreviewDraft)
    }

    /// 按目标选择识别调度 profile。G1/G3 可以显式传 `Legacy`，保持旧的
    /// 累计 Partial 契约；G2/Editor 使用默认 `PreviewDraft`。
    pub fn from_connection_with_profile(
        config: &crate::domain::config::stt_config::SttConfig,
        conn: crate::domain::stt::SttEngineConnection,
        profile: RecognitionProfile,
    ) -> Result<Self, String> {
        let model = config.local_engine.funasr_model.clone();

        if conn.transport.is_none() {
            return Err(
                "本地 STT 连接缺少 worker 通道（GGUF worker 是唯一本地实现）。\
                 请确认语音服务已在设置页启动。"
                    .to_string(),
            );
        }

        let vad_cfg = &config.local_engine.vad;
        let mut recognition_cfg = config.local_engine.recognition.clone();
        // 配置层在持久化时会 sanitize；这里再次收敛是运行时边界，防止
        // 外部构造的 SttConfig 绕过配置命令进入引擎。
        recognition_cfg.sanitize(vad_cfg.max_uncommitted_s);
        let recognition = RecognitionSettings {
            preview_window_ms: u64::from(recognition_cfg.preview_window_ms),
            preview_refresh_ms: u64::from(recognition_cfg.preview_refresh_ms),
            draft_min_s: u64::from(recognition_cfg.draft_min_s),
            strong_pause_ms: u64::from(recognition_cfg.strong_pause_ms),
            long_pause_ms: u64::from(recognition_cfg.long_pause_ms),
        }
        .sanitize(u64::from(vad_cfg.max_uncommitted_s));
        tracing::info!(
            model = %model,
            silence_threshold = vad_cfg.silence_threshold,
            min_silence_ms = vad_cfg.min_silence_ms,
            min_sentence_ms = vad_cfg.min_sentence_ms,
            soft_window_s = vad_cfg.soft_window_s,
            hard_window_s = vad_cfg.hard_window_s,
            max_uncommitted_s = vad_cfg.max_uncommitted_s,
            preview_window_ms = recognition.preview_window_ms,
            preview_refresh_ms = recognition.preview_refresh_ms,
            draft_min_s = recognition.draft_min_s,
            strong_pause_ms = recognition.strong_pause_ms,
            long_pause_ms = recognition.long_pause_ms,
            profile = ?profile,
            "伪流式 STT 引擎: VAD + GGUF worker 通道 (就绪)"
        );
        let mut sentence_state = SentenceState::new();
        sentence_state.set_draft_range_reservation(profile == RecognitionProfile::PreviewDraft);

        Ok(Self {
            inner: Arc::new(Mutex::new(PseudoInner {
                vad: EnergyVad::with_params_and_windows(
                    16000,
                    vad_cfg.silence_threshold,
                    vad_cfg.min_silence_ms,
                    vad_cfg.min_sentence_ms,
                    vad_cfg.soft_window_ms(),
                    vad_cfg.hard_window_ms(),
                ),
                sentences: sentence_state,
                samples: Vec::new(),
                last_preview: Instant::now(),
                last_preview_elapsed: Duration::ZERO,
                last_preview_sample_end: 0,
                preview_in_flight: false,
                preview_owner: 0,
                next_preview_request: 0,
                latest_preview: String::new(),
                preview_tail_range: None,
                latest_preview_request_id: 0,
                preview_revision: 0,
                preview_generation: 0,
                state_revision: 0,
                last_reported_state: None,
                last_reported_confirmed_revision: 0,
                last_reported_span_count: 0,
                last_reported_preview_revision: 0,
                max_uncommitted_audio_ms: vad_cfg.max_uncommitted_ms(),
                coordinator: RecognitionCoordinator::new(16000, profile, recognition),
                boundary_candidate: None,
                uncommitted_voiced_samples: 0,
                uncommitted_strong_samples: 0,
                uncommitted_strong_run_samples: 0,
                uncommitted_strong_run_max: 0,
                decision_wait_reason: None,
                pending_preview: None,
                preview_started: false,
                single_worker: true,
                worker_gate: Arc::new(tokio::sync::Mutex::new(())),
                session_failed: false,
                preview_phrases: Vec::new(),
                pending_phrase: None,
                next_phrase_request: 0,
                phrase_anchor: 0,
                preview_tail: String::new(),
                preview_settled_committed: 0,
                stale_before_worker: 0,
                preview_retreat_chars: 0,
            })),
            connection: Some(conn),
            sample_rate: 16000,
            boundary_observer: None,
            decision_observer: None,
            #[cfg(test)]
            finalize_observer: None,
        })
    }

    /// 给独立的 WAV 诊断回放挂载数值切点记录器。
    pub fn with_boundary_observer(mut self, observer: Arc<Mutex<Vec<SttBoundaryRecord>>>) -> Self {
        self.boundary_observer = Some(observer);
        self
    }

    /// 给独立的 WAV 诊断回放挂载候选切分判断记录器。
    pub fn with_decision_observer(mut self, observer: Arc<Mutex<Vec<SttDecisionRecord>>>) -> Self {
        self.decision_observer = Some(observer);
        self
    }

    /// 给独立的 WAV 诊断回放挂载定稿阶段记录器。
    #[cfg(test)]
    pub fn with_finalize_observer(mut self, observer: Arc<Mutex<Vec<SttFinalizeRecord>>>) -> Self {
        self.finalize_observer = Some(observer);
        self
    }

    /// 记录定稿阶段时间戳；生产路径未挂 observer 时不分配记录。
    #[cfg(test)]
    fn record_finalize(
        observer: &Option<Arc<Mutex<Vec<SttFinalizeRecord>>>>,
        phase: &'static str,
        identity: SegmentIdentity,
    ) {
        let Some(observer) = observer else {
            return;
        };
        observer
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .push(SttFinalizeRecord {
                phase,
                session_generation: identity.session_generation,
                commit_generation: identity.commit_generation,
                segment_id: identity.segment_id,
                observed_at: Instant::now(),
            });
    }

    /// 返回当前应使用的预览间隔（累积过长时降频）。
    fn preview_interval(samples_len: usize, sample_rate: u32, last_elapsed: Duration) -> Duration {
        let duration_ms = (samples_len as f64 / sample_rate as f64 * 1000.0) as u64;
        let base = if duration_ms > PREVIEW_SLOWDOWN_THRESHOLD_MS {
            PREVIEW_SLOW_INTERVAL_MS
        } else {
            PREVIEW_INTERVAL_MS
        };
        // 目标是让预览推理的长期占空比不超过约 1/3：推理 N ms 后至少
        // 冷却 2N ms。短音频仍受 500ms 基础间隔约束。
        let adaptive = last_elapsed.as_millis().saturating_mul(2);
        Duration::from_millis(base.max(adaptive.min(PREVIEW_MAX_INTERVAL_MS as u128) as u64))
    }

    /// 0.23.14 PreviewDraft 刷新间隔：配置下限与推理耗时冷却的较大值。
    ///
    /// PreviewDraft 的预览窗口锚定短语锚点、受 preview_window 有界约束，
    /// 不需要 Legacy 的 8s 慢速降档；但同样遵守"推理 N ms 后至少冷却 2N ms"
    /// 的占空比约束（≤ 1/3），并设 PREVIEW_MAX_INTERVAL_MS 上限——慢机器
    /// 长音频上不再以固定 700ms 间隔持续制造推理积压。
    fn preview_refresh_interval(base_ms: u64, last_elapsed: Duration) -> Duration {
        let adaptive = last_elapsed.as_millis().saturating_mul(2);
        Duration::from_millis(base_ms.max(adaptive.min(PREVIEW_MAX_INTERVAL_MS as u128) as u64))
    }

    fn has_min_preview_growth(total: usize, last_end: usize, sample_rate: u32) -> Option<bool> {
        let min_samples = (PREVIEW_MIN_NEW_AUDIO_MS * sample_rate as u64 / 1000) as usize;
        total.checked_sub(last_end).map(|new| new >= min_samples)
    }

    /// 0.22.15 follow-up: 安全锁——在 Mutex poison 时恢复而非 panic。
    ///
    /// 如果 Mutex 已 poisoned（因后台 task panic），返回 `None`。
    /// 调用方应据此安全终止当前操作或返回错误。
    ///
    /// **不用 `PoisonError::into_inner()`**——poison 意味着状态可能损坏，
    /// 盲目继续会掩盖问题。正确做法是让当前 session 失败，等待 `reset` 后重试。
    fn try_lock(inner: &Mutex<PseudoInner>) -> Option<std::sync::MutexGuard<'_, PseudoInner>> {
        inner.lock().ok()
    }

    /// 组装返回 JSON 字符串。
    ///
    /// 携带状态版本号（`revision`）与 `confirmed_changed` 标记：只有当
    /// confirmed 真正增长时才携带完整累计正文，避免每块音频都搬运全文。
    /// confirmed 与 preview 同时为空时返回空串（消费方不产生任何事件）。
    fn compose_result(
        revision: u64,
        confirmed: &str,
        preview: &str,
        confirmed_changed: bool,
    ) -> String {
        if confirmed.is_empty() && preview.is_empty() {
            return String::new();
        }
        serde_json::json!({
            "v": 2,
            "revision": revision,
            "confirmed_changed": confirmed_changed,
            "confirmed": confirmed,
            "preview": preview,
        })
        .to_string()
    }

    /// 转录（等待结果）——走 worker transport 通道。
    ///
    /// 通道就绪由 start 时的 ready 握手保证，finalize 调用时不再重复握手——
    /// 额外的 hello 请求会与 worker 的推理线程竞争，可能触发访问违例。
    /// 请求在客户端串行化（单请求在途）。
    async fn transcribe_samples(
        &self,
        samples: &[f32],
        off_threshold: f64,
    ) -> Result<String, SttError> {
        if samples.is_empty() {
            return Ok(String::new());
        }

        let conn = self
            .connection
            .as_ref()
            .ok_or_else(|| SttError::Engine("伪流式引擎无连接快照".to_string()))?;
        let transport = conn
            .transport
            .as_ref()
            .ok_or_else(|| SttError::Engine("伪流式引擎连接缺少 worker 通道".to_string()))?;

        // 0.22.15：裁剪尾部静音——使用 VAD off_threshold 统一静音语义
        let trimmed = trim_trailing_silence(samples, self.sample_rate, off_threshold);
        if trimmed.is_empty() {
            // RequestAudioGate 的 NoSpeech：不发送空 WAV，调用方将按 owned
            // range 消费覆盖水位。
            return Ok(String::new());
        }
        let wav_bytes = super::wav::pcm_to_wav(&trimmed, self.sample_rate, 1);

        let (worker_gate, single_worker) = {
            let inner = Self::try_lock(&self.inner).ok_or_else(|| {
                SttError::Engine("STT session 已损坏 (Mutex poisoned)".to_string())
            })?;
            (Arc::clone(&inner.worker_gate), inner.single_worker)
        };
        let _worker_guard = if single_worker {
            Some(worker_gate.lock().await)
        } else {
            None
        };

        let text = transport
            .transcribe(&wav_bytes)
            .await
            .map_err(|e| SttError::Engine(e.to_string()))?;

        // 剥离 SenseVoice 幻觉的英文语气词
        Ok(strip_filler_words(&text))
    }

    /// 0.22.15：后台 spawn 一个定稿识别 task（worker transport 通道）。
    ///
    /// task 携带 session_generation + segment_id，返回时通过
    /// `commit_or_rollback` 校验 identity 后写入状态。
    fn spawn_sentence_finalize(&self, sentence_samples: Vec<f32>, identity: SegmentIdentity) {
        if sentence_samples.is_empty() {
            // 空 segment 直接 rollback
            let result = FinalizeResult {
                identity,
                text: String::new(),
                ok: false,
            };
            let mut inner = match Self::try_lock(&self.inner) {
                Some(g) => g,
                None => {
                    tracing::error!(
                        seg = identity.segment_id,
                        "Mutex poisoned at empty finalize rollback"
                    );
                    return;
                }
            };
            if let Some(deferred) = inner.commit_or_rollback(&result) {
                // deferred.range 是绝对坐标，转换为局部切片
                let samples: Vec<f32> = match inner
                    .sentences
                    .abs_to_local_range(&deferred.range, inner.samples.len())
                {
                    Some(r) => inner.samples[r].to_vec(),
                    None => {
                        inner.mark_session_failed("finalize deferred 坐标非法");
                        Vec::new()
                    }
                };
                drop(inner);
                self.spawn_sentence_finalize(samples, deferred.identity);
            }
            return;
        }

        // 标记 in_flight + 获取 VAD off_threshold
        let (off_threshold, worker_gate, single_worker, recognition_profile) = {
            let mut inner = match Self::try_lock(&self.inner) {
                Some(g) => g,
                None => {
                    tracing::error!(
                        seg = identity.segment_id,
                        "Mutex poisoned at finalize in_flight mark"
                    );
                    return;
                }
            };
            inner.sentences.finalize_in_flight = true;
            (
                inner.vad.current_off_threshold(),
                Arc::clone(&inner.worker_gate),
                inner.single_worker,
                inner.coordinator.profile,
            )
        };

        let inner = Arc::clone(&self.inner);
        let Some(transport) = self.connection.as_ref().and_then(|c| c.transport.clone()) else {
            tracing::warn!("定稿识别缺少 worker 通道，跳过");
            let result = FinalizeResult {
                identity,
                text: String::new(),
                ok: false,
            };
            let mut inner = match Self::try_lock(&self.inner) {
                Some(g) => g,
                None => {
                    tracing::error!(
                        seg = identity.segment_id,
                        "Mutex poisoned at no-transport rollback"
                    );
                    return;
                }
            };
            if let Some(deferred) = inner.commit_or_rollback(&result) {
                let samples: Vec<f32> = match inner
                    .sentences
                    .abs_to_local_range(&deferred.range, inner.samples.len())
                {
                    Some(r) => inner.samples[r].to_vec(),
                    None => {
                        inner.mark_session_failed("no-transport deferred 坐标非法");
                        Vec::new()
                    }
                };
                drop(inner);
                self.spawn_sentence_finalize(samples, deferred.identity);
            }
            return;
        };
        let sample_rate = self.sample_rate;
        #[cfg(test)]
        let finalize_observer = self.finalize_observer.clone();

        tokio::spawn(async move {
            let mut current_samples = sentence_samples;
            let mut current_identity = identity;
            let mut current_threshold = off_threshold;
            loop {
                let _worker_guard = if single_worker {
                    Some(worker_gate.lock().await)
                } else {
                    None
                };
                // 0.22.15：裁剪尾部静音——使用 VAD off_threshold 统一静音语义
                let trimmed =
                    trim_trailing_silence(&current_samples, sample_rate, current_threshold);
                let no_speech = trimmed.is_empty();
                let result = if trimmed.is_empty() {
                    Ok(String::new())
                } else {
                    let wav_bytes = super::wav::pcm_to_wav(&trimmed, sample_rate, 1);
                    #[cfg(test)]
                    if let Some(observer) = &finalize_observer {
                        observer
                            .lock()
                            .unwrap_or_else(|poison| poison.into_inner())
                            .push(SttFinalizeRecord {
                                phase: "transport_start",
                                session_generation: current_identity.session_generation,
                                commit_generation: current_identity.commit_generation,
                                segment_id: current_identity.segment_id,
                                observed_at: Instant::now(),
                            });
                    }
                    transport.transcribe(&wav_bytes).await
                };

                let finalize_result = match result {
                    Ok(text) => {
                        let cleaned = strip_filler_words(&text);
                        tracing::debug!(
                            seg = current_identity.segment_id,
                            text_len = cleaned.chars().count(),
                            samples = current_samples.len(),
                            "定稿识别完成"
                        );
                        FinalizeResult {
                            identity: current_identity,
                            text: cleaned,
                            ok: true,
                        }
                    }
                    Err(e) => {
                        tracing::warn!(seg = current_identity.segment_id, %e, "定稿识别失败");
                        FinalizeResult {
                            identity: current_identity,
                            text: String::new(),
                            ok: false,
                        }
                    }
                };

                // 写入状态——先验 identity
                // 0.22.15 follow-up: 使用 try_lock 避免 Mutex poison 连锁 panic
                let mut inner = match inner.lock() {
                    Ok(g) => g,
                    Err(_) => {
                        tracing::error!(
                            seg = current_identity.segment_id,
                            "Mutex poisoned at finalize result write — session 已损坏，放弃写入"
                        );
                        return;
                    }
                };
                // 识别成功但正文为空（纯静音，或剥离 "/sil" 后为空的噪声段）
                // 与音频级 NoSpeech 同等对待：PreviewDraft 消费 owned range、
                // 不产生 span；Legacy 保持空文本 rollback 语义不变。
                let silence_only = finalize_result.ok && finalize_result.text.is_empty();
                let deferred = if (no_speech || silence_only)
                    && recognition_profile == RecognitionProfile::PreviewDraft
                {
                    let range_end = inner
                        .sentences
                        .pending
                        .as_ref()
                        .filter(|pending| pending.identity == current_identity)
                        .map(|pending| pending.range.end);
                    // 0.23.9：通过 coordinator 同步 NoSpeech 消费。
                    let _ = inner
                        .coordinator
                        .consume_no_speech(current_identity.segment_id);
                    range_end.and_then(|end| inner.consume_no_speech(current_identity, end))
                } else {
                    inner.commit_or_rollback(&finalize_result)
                };
                if let Some(deferred) = deferred {
                    let Some(local_range) = inner
                        .sentences
                        .abs_to_local_range(&deferred.range, inner.samples.len())
                    else {
                        inner.mark_session_failed("deferred finalize 坐标非法");
                        return;
                    };
                    current_samples = inner.samples[local_range].to_vec();
                    current_identity = deferred.identity;
                    current_threshold = inner.vad.current_off_threshold();
                    tracing::debug!(
                        seg = current_identity.segment_id,
                        "继续处理 deferred segment"
                    );
                    drop(inner);
                    drop(_worker_guard);
                    continue;
                }
                drop(_worker_guard);
                return;
            }
        });
    }

    /// 后台 spawn 一个预览识别 task（worker transport 通道）。
    ///
    /// **所有权约定（0.24）**：启动时独占 `preview_in_flight` 并记录 owner token
    /// （request id）。返回时**只有 owner 本人**才可释放该标志，且释放由 RAII
    /// 守卫保证覆盖成功/失败/取消/panic 全部路径。旧任务的迟到结果一律无法
    /// 清除新任务的状态。
    fn spawn_preview_recognition(
        &self,
        samples_snapshot: Vec<f32>,
        snapshot_end: usize,
        snapshot_range: AudioRange,
    ) {
        if samples_snapshot.is_empty() {
            return;
        }

        // 登记所有权 + 捕获当前代际 + 获取 VAD off_threshold
        let (request_id, generation, off_threshold, worker_gate, single_worker) = {
            let mut inner = match Self::try_lock(&self.inner) {
                Some(g) => g,
                None => {
                    tracing::error!("Mutex poisoned at preview in_flight mark");
                    return;
                }
            };
            let request_id = inner.next_preview_request.wrapping_add(1);
            inner.next_preview_request = request_id;
            inner.preview_in_flight = true;
            inner.preview_owner = request_id;
            // 0.23.9：通过 coordinator 同步 running preview 状态。
            let preview_revision = inner.preview_revision + 1;
            inner.coordinator.running_preview = Some(PreviewRequest {
                request_id,
                audio_range: snapshot_range,
                model_input_range: None,
                revision: preview_revision,
            });
            (
                request_id,
                inner.preview_generation,
                inner.vad.current_off_threshold(),
                Arc::clone(&inner.worker_gate),
                inner.single_worker,
            )
        };

        let inner = Arc::clone(&self.inner);
        let Some(transport) = self.connection.as_ref().and_then(|c| c.transport.clone()) else {
            tracing::warn!("预览识别缺少 worker 通道，跳过");
            if let Some(mut g) = Self::try_lock(&self.inner) {
                g.release_preview_owner(request_id);
            }
            return;
        };
        let sample_rate = self.sample_rate;

        tokio::spawn(async move {
            // RAII：任何退出路径都释放 owner（旧任务不会误清新任务的状态）
            let _owner = PreviewOwnerGuard {
                inner: Arc::clone(&inner),
                request_id,
            };

            let started_at = Instant::now();
            let _worker_guard = if single_worker {
                Some(worker_gate.lock().await)
            } else {
                None
            };
            // 0.23.14：gate 后、transport 前复核代际与所有权——排队期间句尾
            // 已发生或新任务已接管的 stale Preview 直接退出，不再占用模型
            // 时间（0.23.13 实测 487ms 过期预览仍完整推理后才被丢弃）。
            {
                let mut inner = match inner.lock() {
                    Ok(g) => g,
                    Err(_) => {
                        tracing::error!(gen = generation, "Mutex poisoned at preview gate check");
                        return;
                    }
                };
                let stale = inner.preview_generation != generation
                    || (inner.preview_in_flight && inner.preview_owner != request_id);
                if stale {
                    inner.stale_before_worker = inner.stale_before_worker.wrapping_add(1);
                    tracing::debug!(
                        request_id,
                        gen_stale = inner.preview_generation != generation,
                        "gate 后淘汰过期预览（未调用模型）"
                    );
                    inner.coordinator.finish(request_id, false, String::new());
                    inner.release_preview_owner(request_id);
                    return;
                }
            }
            // 0.22.15：裁剪尾部静音——使用 VAD off_threshold 统一静音语义
            let trimmed = trim_trailing_silence(&samples_snapshot, sample_rate, off_threshold);
            let wav_bytes = super::wav::pcm_to_wav(&trimmed, sample_rate, 1);

            let result = transport.transcribe(&wav_bytes).await;

            let elapsed = started_at.elapsed();
            let mut inner = match inner.lock() {
                Ok(g) => g,
                Err(_) => {
                    tracing::error!(gen = generation, "Mutex poisoned at preview completion");
                    return;
                }
            };
            // 已被新任务接管：旧结果彻底丢弃，且不得触碰新任务状态
            if inner.preview_in_flight && inner.preview_owner != request_id {
                tracing::debug!(
                    request_id,
                    current_owner = inner.preview_owner,
                    "丢弃已被新任务接管的预览结果"
                );
                // 0.23.9：通过 coordinator 的 finish 清空 running_preview。
                // stale request_id 不会误清新 owner 的 running_preview。
                inner.coordinator.finish(request_id, false, String::new());
                return;
            }

            match result {
                Ok(text) => {
                    let cleaned = strip_filler_words(&text);
                    // 代际校验：句尾后丢弃过期预览（防重复影子）
                    if inner.preview_generation == generation {
                        inner.set_preview_if_changed(cleaned, snapshot_range, request_id);
                    } else {
                        tracing::debug!(
                            gen = generation,
                            cur_gen = inner.preview_generation,
                            "丢弃过期预览（句尾已发生）"
                        );
                    }
                }
                Err(e) => {
                    tracing::trace!(%e, "预览识别失败（非致命）");
                }
            }
            // 0.23.9：通过 coordinator 的 finish 清空 running_preview。
            // finish 对 Preview 只清空 running_preview 槽，不产 span。
            inner.coordinator.finish(request_id, true, String::new());

            // 速率控制只在代际仍有效时推进（句尾已自行重置计时）
            if inner.preview_generation == generation {
                inner.last_preview = Instant::now();
                inner.last_preview_elapsed = elapsed;
                inner.last_preview_sample_end = snapshot_end;
            }
        });
    }

    /// 预览定稿：对被作废候选覆盖的短语做一次识别并冻结进账本。
    ///
    /// 与尾部预览不同，短语结果只追加、不替换；真实 Draft 提交覆盖其范围
    /// 后由 `commit_or_rollback` 清退。
    ///
    /// 0.23.14 事务式：锚点在登记时投机推进（尾部窗口不重复覆盖该音频），
    /// 识别成功非空且代际/登记均有效才提交入账；error/空文本回退锚点，
    /// 该音频并入下一次短语尝试（合并重识别，不丢前缀）；stale（代际推进
    /// 或 settle 已消费范围）只丢弃登记——范围已由 Draft/reset 接管。
    fn spawn_phrase_recognition(&self, samples: Vec<f32>, pending: PendingPhrase) {
        if samples.is_empty() {
            // 无音频可送模：按失败处理，回退锚点恢复覆盖。
            if let Some(mut inner) = Self::try_lock(&self.inner) {
                inner.fail_pending_phrase(pending.request_id, pending.anchor_before);
            }
            return;
        }

        let (off_threshold, worker_gate, single_worker) = {
            let inner = match Self::try_lock(&self.inner) {
                Some(g) => g,
                None => {
                    tracing::error!("Mutex poisoned at phrase spawn");
                    return;
                }
            };
            (
                inner.vad.current_off_threshold(),
                Arc::clone(&inner.worker_gate),
                inner.single_worker,
            )
        };

        let inner_arc = Arc::clone(&self.inner);
        let Some(transport) = self.connection.as_ref().and_then(|c| c.transport.clone()) else {
            tracing::warn!("短语定稿缺少 worker 通道，回退锚点");
            if let Some(mut inner) = Self::try_lock(&self.inner) {
                inner.fail_pending_phrase(pending.request_id, pending.anchor_before);
            }
            return;
        };
        let sample_rate = self.sample_rate;
        let request_id = pending.request_id;

        tokio::spawn(async move {
            let _worker_guard = if single_worker {
                Some(worker_gate.lock().await)
            } else {
                None
            };
            // 0.23.14：gate 后、transport 前复核——排队期间边界已接受（代际
            // 推进，范围由 Draft 接管）或登记已被 settle 消费的 stale 短语
            // 直接退出，不占模型时间。
            {
                let mut inner = match inner_arc.lock() {
                    Ok(g) => g,
                    Err(_) => {
                        tracing::error!("Mutex poisoned at phrase gate check");
                        return;
                    }
                };
                let owned_current = inner
                    .pending_phrase
                    .as_ref()
                    .is_some_and(|p| p.request_id == request_id);
                let gen_stale = inner.preview_generation != pending.generation;
                if gen_stale || !owned_current {
                    inner.stale_before_worker = inner.stale_before_worker.wrapping_add(1);
                    if gen_stale && owned_current {
                        // 范围已由 Draft/reset 接管，作废登记（锚点不回退）。
                        inner.pending_phrase = None;
                    }
                    tracing::debug!(request_id, gen_stale, "gate 后淘汰过期短语（未调用模型）");
                    return;
                }
            }
            let trimmed = trim_trailing_silence(&samples, sample_rate, off_threshold);
            let wav_bytes = super::wav::pcm_to_wav(&trimmed, sample_rate, 1);

            let result = transport.transcribe(&wav_bytes).await;

            let mut inner = match inner_arc.lock() {
                Ok(g) => g,
                Err(_) => {
                    tracing::error!("Mutex poisoned at phrase completion");
                    return;
                }
            };
            // 只处理仍属于自己的登记；settle/替换/新短语会移除旧登记。
            let Some(owned) = inner
                .pending_phrase
                .clone()
                .filter(|p| p.request_id == request_id)
            else {
                tracing::debug!(request_id, "丢弃迟到短语定稿（登记已消费或易主）");
                return;
            };
            // 代际已推进（句尾/终态/reset）：该范围已交给 Draft 或已作废。
            if inner.preview_generation != owned.generation {
                inner.pending_phrase = None;
                tracing::debug!("丢弃迟到短语定稿（代际已推进）");
                return;
            }
            match result {
                Ok(text) => {
                    let cleaned = strip_filler_words(&text);
                    if !cleaned.is_empty() {
                        let chars = cleaned.chars().count();
                        inner.pending_phrase = None;
                        inner
                            .preview_phrases
                            .push(PreviewSpan::new(owned.range, cleaned));
                        // 0.23.14.7 P1-1：冻结短语入账必须同步清退与该音频范围
                        // 重叠的尾部——tail 与短语共享 phrase_anchor 起点，
                        // 不清退会出现同一音频被投影两次。
                        inner.settle_tail_for_phrase(owned.range.end_sample);
                        // 0.23.14.6：对外范围信封按 span 清单推导（见
                        // compose_typed_result），不再单独维护"组合文本配
                        // 最后一个局部范围"的字段。
                        inner.rebuild_preview_text();
                        tracing::debug!(
                            ledger_len = inner.preview_phrases.len(),
                            chars,
                            "短语定稿入账"
                        );
                    } else {
                        // 空文本：锚点回退，该范围并入下一次短语尝试。
                        inner.fail_pending_phrase(owned.request_id, owned.anchor_before);
                        tracing::debug!("短语定稿识别返回空文本，回退锚点等待合并");
                    }
                }
                Err(e) => {
                    // 0.23.13 排查：此前错误被静默吞掉。0.23.14：错误同样
                    // 回退锚点——音频交回可识别覆盖范围，不静默丢失前缀。
                    inner.fail_pending_phrase(owned.request_id, owned.anchor_before);
                    tracing::debug!(%e, "短语定稿识别失败，回退锚点等待合并");
                }
            }
        });
    }

    /// 会话开始即预热 worker（fire-and-forget）。
    ///
    /// 0.23.10.2：GGUF worker 首次推理承担懒加载开销（实测冷启 649ms，
    /// 热态 ~300ms），直接吃掉首个预览/短语结果的响应预算。会话开始时
    /// 送一段 250ms 低幅噪声触发完整推理路径，把该开销移到用户尚未说话
    /// 的窗口内。结果与引擎状态完全解耦——不触碰预览/代际/owner，仅按
    /// single_worker 语义串行占用 worker gate；失败只记 debug。
    pub fn warm_up_worker(&self) {
        let Some(transport) = self.connection.as_ref().and_then(|c| c.transport.clone()) else {
            return;
        };
        let (worker_gate, single_worker) = {
            let inner = match Self::try_lock(&self.inner) {
                Some(g) => g,
                None => return,
            };
            (Arc::clone(&inner.worker_gate), inner.single_worker)
        };
        let sample_rate = self.sample_rate.max(1);
        let warm_samples: Vec<f32> = (0..sample_rate / 4)
            .map(|i| {
                // 确定性低幅噪声：能量高于零但不构成语音，强制模型完整解码。
                0.002 * (((i % 97) as f32 / 97.0) - 0.5)
            })
            .collect();
        let wav_bytes = super::wav::pcm_to_wav(&warm_samples, sample_rate, 1);
        tokio::spawn(async move {
            let _worker_guard = if single_worker {
                Some(worker_gate.lock().await)
            } else {
                None
            };
            if let Err(e) = transport.transcribe(&wav_bytes).await {
                tracing::debug!(%e, "worker 预热失败（忽略，不影响会话）");
            }
        });
    }

    /// PreviewDraft profile 的音频入口。
    ///
    /// 它与旧的 Legacy 路径分开，避免 G1/G3 在迁移期间被新 Draft 门槛
    /// 改变；G2/Editor 通过 `from_connection` 默认进入此路径。VAD 只在这里
    /// 形成候选，候选满足上下文/强停顿条件后才交给 SentenceState 预留。
    async fn transcribe_chunk_preview_draft(&self, samples: &[f32]) -> Result<String, SttError> {
        let (
            pending_segment,
            should_preview,
            samples_snapshot,
            snapshot_end,
            snapshot_range,
            phrase_snapshot,
        ) = {
            let mut inner = Self::try_lock(&self.inner).ok_or_else(|| {
                SttError::Engine("STT session 已损坏 (Mutex poisoned)".to_string())
            })?;
            if inner.session_failed {
                return Err(SttError::Engine(
                    "STT session 已失败，需要 reset 后重试".to_string(),
                ));
            }
            if inner.sentences.finalizing.is_some() {
                return Err(SttError::Engine("STT session 正在 finalize".to_string()));
            }
            inner.samples.extend_from_slice(samples);
            let total = inner
                .sentences
                .buffer_base_sample
                .checked_add(inner.samples.len())
                .ok_or_else(|| SttError::Engine("STT 音频坐标溢出".to_string()))?;
            let mut event = inner.vad.process_chunk(samples);
            let off_threshold = inner.vad.current_off_threshold();

            // 派生 backlog hard limit = 2 × max_uncommitted，达到即终止，
            // 不能继续持有无界 PCM。保留已提交 Draft 与完整绝对水位。
            // 0.23.9：通过 coordinator 的 accept_audio_end 检查 backlog，
            // coordinator 成为过载判定的唯一真源。
            // Draft 提交走 SentenceState（不经 coordinator.finish），因此
            // 判定前必须把生产 committed 水位同步进 coordinator；否则
            // backlog 退化为会话总时长，24s 硬限会在正常听写中误触发。
            let committed_end = inner.sentences.committed_sample_end as u64;
            inner.coordinator.draft_committed_audio_end = inner
                .coordinator
                .draft_committed_audio_end
                .max(committed_end);
            let max_uncommitted_s = inner.max_uncommitted_audio_ms / 1000;
            if !inner
                .coordinator
                .accept_audio_end(total as u64, max_uncommitted_s)
            {
                inner.pending_preview = None;
                inner.preview_generation = inner.preview_generation.wrapping_add(1);
                inner.mark_session_failed("stt_overloaded");
                return Err(SttError::Engine("stt_overloaded".to_string()));
            }

            // reserved 之后的尾段达到单段上限时形成兜底边界，即使 VAD
            // 长期停在滞回区也不会让 Draft 请求无限变长。
            let reserved = inner
                .sentences
                .draft_reserved_sample_end
                .max(inner.sentences.committed_sample_end);
            let max_segment_samples =
                inner.max_uncommitted_audio_ms as u128 * self.sample_rate as u128 / 1000;
            if !event.is_boundary()
                && (total.saturating_sub(reserved) as u128) >= max_segment_samples
            {
                event = VadEvent::HardWindow;
            }

            let observed_new_candidate = event.is_boundary();
            let invalidated_candidate = inner.observe_boundary_candidate(
                event,
                total,
                samples,
                off_threshold,
                self.sample_rate,
            );

            // 0.23.9.10：候选被新语音作废 → 预览定稿。对 [phrase_anchor,
            // quiet_start) 做有声门控：通过则登记在途短语并投机推进锚点
            // （error/空文本时回退，见 spawn_phrase_recognition）；有声不足
            // 则保留锚点等下一短语合并；纯静音段直接跳过锚点。
            // 0.23.14：在途短语未落地时不再发起新短语——下一次触发按
            // 回退/推进后的锚点合并覆盖。
            let mut phrase_snapshot: Option<(Vec<f32>, PendingPhrase)> = None;
            if let Some(quiet_start) = invalidated_candidate
                && quiet_start > inner.phrase_anchor
                && inner.pending_phrase.is_none()
            {
                let anchor_before = inner.phrase_anchor;
                let start = anchor_before.min(total as u64) as usize;
                let end = (quiet_start as usize).min(total);
                let phrase_range = AudioRange::new(start as u64, end as u64);
                match inner
                    .sentences
                    .abs_to_local_range(&(start..end), inner.samples.len())
                {
                    Some(local) => {
                        let phrase_samples = &inner.samples[local];
                        let gate = RequestAudioGate::evaluate(
                            phrase_range,
                            phrase_samples,
                            self.sample_rate,
                            off_threshold,
                            PHRASE_MIN_VOICED_MS,
                            false,
                        );
                        match gate {
                            RequestAudioGate::Valid {
                                model_input_range, ..
                            } => {
                                let offset_start = model_input_range
                                    .start_sample
                                    .saturating_sub(phrase_range.start_sample)
                                    .min(phrase_samples.len() as u64)
                                    as usize;
                                let offset_end = model_input_range
                                    .end_sample
                                    .saturating_sub(phrase_range.start_sample)
                                    .min(phrase_samples.len() as u64)
                                    as usize;
                                let model_samples = if offset_start < offset_end {
                                    Some(phrase_samples[offset_start..offset_end].to_vec())
                                } else {
                                    None
                                };
                                if let Some(samples_snapshot) = model_samples {
                                    inner.next_phrase_request =
                                        inner.next_phrase_request.wrapping_add(1).max(1);
                                    let pending = PendingPhrase {
                                        request_id: inner.next_phrase_request,
                                        anchor_before,
                                        range: phrase_range,
                                        generation: inner.preview_generation,
                                    };
                                    phrase_snapshot = Some((samples_snapshot, pending.clone()));
                                    inner.pending_phrase = Some(pending);
                                }
                                inner.phrase_anchor = quiet_start;
                            }
                            RequestAudioGate::TooShort => {}
                            RequestAudioGate::NoSpeech => {
                                inner.phrase_anchor = quiet_start;
                            }
                        }
                    }
                    None => {
                        // 坐标越界（紧凑边界）：跳过该片段，锚点仍推进，
                        // 避免同一静音段反复触发。
                        inner.phrase_anchor = quiet_start;
                    }
                }
            }

            let mut accepted_boundary = None;
            if let Some(candidate) = inner.boundary_candidate.clone() {
                let readiness = inner.candidate_readiness(&candidate, total, self.sample_rate);
                if let Ok(accepted_via) = readiness {
                    let mut boundary_total = candidate.boundary_sample as usize;
                    let committed = inner
                        .sentences
                        .draft_reserved_sample_end
                        .max(inner.sentences.committed_sample_end);
                    // hard/cap 可回退到近期谷底，但不得落到 reserved 之前。
                    if (candidate.reason == "hard_window" || candidate.reason == "uncommitted_cap")
                        && boundary_total > committed
                    {
                        if let Some(offset) = inner
                            .vad
                            .low_energy_valley_offset(HARD_CUT_VALLEY_WINDOW_MS)
                        {
                            let valley = total.saturating_sub(offset);
                            if valley > committed && valley < total {
                                boundary_total = valley;
                            }
                        }
                    }
                    accepted_boundary = Some((boundary_total, candidate.reason.clone(), accepted_via));
                } else {
                    // 0.23.14.7 case_17：候选被拒的证据必须可追溯——只在候选
                    // 新建时记录会漏掉"候选随静默增长后改由可信度门拒绝"
                    // （噪声尾段正是被这条路拦下的）。这里对同一候选按
                    // **等待原因变化**补记，噪声尾段的拒绝原因因此显式落报告，
                    // 而稳态等待不产生重复行。
                    let wait_reason = readiness.err();
                    if observed_new_candidate || inner.decision_wait_reason != wait_reason {
                        inner.decision_wait_reason = wait_reason;
                        let reserved = inner
                            .sentences
                            .draft_reserved_sample_end
                            .max(inner.sentences.committed_sample_end)
                            as u64;
                        let sample_rate = u64::from(self.sample_rate).max(1);
                        if let Some(observer) = &self.decision_observer {
                            observer
                                .lock()
                                .unwrap_or_else(|error| error.into_inner())
                                .push(SttDecisionRecord {
                                    audio_ms: candidate.boundary_sample * 1000 / sample_rate,
                                    owned_start_ms: reserved * 1000 / sample_rate,
                                    owned_end_ms: candidate.boundary_sample * 1000 / sample_rate,
                                    reason: candidate.reason,
                                    outcome: "waiting",
                                    wait_reason,
                                    accepted_via: None,
                                    strong_ms: candidate.strong_samples * 1000 / sample_rate,
                                    strong_run_ms: candidate.strong_run_max_samples * 1000
                                        / sample_rate,
                                    voiced_ms: candidate.voiced_samples * 1000 / sample_rate,
                                    quiet_ms: candidate.quiet_samples * 1000 / sample_rate,
                                });
                        }
                    }
                }
            }

            let pending = if let Some((boundary_total, reason, accepted_via)) = accepted_boundary {
                let preview_snapshot = inner.latest_preview.clone();
                let reserved = inner
                    .sentences
                    .draft_reserved_sample_end
                    .max(inner.sentences.committed_sample_end)
                    as u64;
                let observer_reason = match reason.as_str() {
                    "uncommitted_cap" => "uncommitted_cap",
                    "hard_window" => "hard_window",
                    "soft_window" => "soft_window",
                    _ => "natural_silence",
                };
                if let Some(observer) = &self.boundary_observer {
                    observer
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .push(SttBoundaryRecord {
                            audio_ms: boundary_total as u64 * 1000 / self.sample_rate as u64,
                            reason: observer_reason,
                            #[cfg(test)]
                            observed_at: Instant::now(),
                        });
                }
                if let Some(observer) = &self.decision_observer {
                    let sample_rate = u64::from(self.sample_rate).max(1);
                    let candidate = inner.boundary_candidate.as_ref();
                    observer
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .push(SttDecisionRecord {
                            audio_ms: boundary_total as u64 * 1000 / sample_rate,
                            owned_start_ms: reserved * 1000 / sample_rate,
                            owned_end_ms: boundary_total as u64 * 1000 / sample_rate,
                            reason: reason.clone(),
                            outcome: "accepted",
                            wait_reason: None,
                            accepted_via: Some(accepted_via.as_str()),
                            strong_ms: candidate
                                .map(|value| value.strong_samples * 1000 / sample_rate)
                                .unwrap_or(0),
                            strong_run_ms: candidate
                                .map(|value| value.strong_run_max_samples * 1000 / sample_rate)
                                .unwrap_or(0),
                            voiced_ms: candidate
                                .map(|value| value.voiced_samples * 1000 / sample_rate)
                                .unwrap_or(0),
                            quiet_ms: candidate
                                .map(|value| value.quiet_samples * 1000 / sample_rate)
                                .unwrap_or(0),
                        });
                }
                #[cfg(test)]
                {
                    let created_identity = SegmentIdentity {
                        session_generation: inner.sentences.session_generation,
                        commit_generation: inner.sentences.commit_generation,
                        segment_id: inner.sentences.next_segment_id,
                    };
                    Self::record_finalize(&self.finalize_observer, "created", created_identity);
                }
                let pending = inner
                    .sentences
                    .on_sentence_end(boundary_total, &preview_snapshot);
                if let Err(reason) = inner.reset_vad_at_boundary(boundary_total, total) {
                    inner.mark_session_failed(reason);
                    return Err(SttError::Engine(reason.to_string()));
                }
                // 回退切点之后的音频属于下一 Draft，重新计算其有效有声量；
                // 被提交范围不再参与下一候选门槛。
                let suffix = inner
                    .sentences
                    .abs_to_local_range(&(boundary_total..total), inner.samples.len())
                    .ok_or_else(|| {
                        inner.mark_session_failed("Draft 边界回退坐标非法");
                        SttError::Engine("STT Draft 坐标非法".to_string())
                    })?;
                inner.uncommitted_voiced_samples = PseudoInner::voiced_samples(
                    &inner.samples[suffix.clone()],
                    off_threshold,
                    self.sample_rate,
                );
                // 0.23.14.7 case_17：可信度证据随回退后归属重算。
                let suffix_profile = PseudoInner::strong_run_profile(
                    &inner.samples[suffix],
                    inner.vad.current_on_threshold(),
                    self.sample_rate,
                );
                inner.uncommitted_strong_samples = suffix_profile.strong_samples;
                inner.uncommitted_strong_run_samples = suffix_profile.trail_run;
                inner.uncommitted_strong_run_max = suffix_profile.max_run;
                // 0.23.9：通过 coordinator 同步 Draft 预留水位，使 coordinator
                // 的 reserved/committed 水位与 SentenceState 保持一致。
                let max_uncommitted_s = inner.max_uncommitted_audio_ms / 1000;
                let reserved = inner
                    .sentences
                    .draft_reserved_sample_end
                    .max(inner.sentences.committed_sample_end);
                let span_id = inner.sentences.next_segment_id as u64;
                let revision = inner.sentences.confirmed_revision + 1;
                let request_id = inner.coordinator.next_request_id();
                inner.coordinator.reserve_draft(
                    DraftRequest {
                        request_id,
                        span_id,
                        owned_range: AudioRange::new(reserved as u64, boundary_total as u64),
                        model_input_range: None,
                        revision,
                    },
                    max_uncommitted_s,
                );
                inner.clear_candidate();
                // 0.23.10.2：接受边界时不再清空尾部——Draft 推理需要数百毫秒，
                // 提前清空会让已显示的虚字先消失、等 Draft 提交后再以实字重现
                // （实测 08:32 会话 21 字符→9→0 的回缩闪烁）。尾部保留到
                // commit_or_rollback 的 settle（真实提交/NoSpeech 消费推进
                // committed 水位）时才清退；回滚路径无需恢复任何预览。
                inner.phrase_anchor = boundary_total as u64;
                inner.pending_preview = None;
                inner.preview_generation = inner.preview_generation.wrapping_add(1);
                inner.last_preview = Instant::now();
                inner.last_preview_elapsed = Duration::ZERO;
                inner.last_preview_sample_end = total;
                pending
            } else {
                None
            };

            // Preview 使用滚动窗口而不是最近新增 chunk。首轮至少 1.2s
            // 有效输入；之后只要求增量达到 500ms，刷新周期与窗口解耦。
            // 0.23.14：固定刷新间隔升级为自适应冷却（配置下限与 2×上轮
            // 推理耗时的较大值，带上限）。
            let preview_interval = Self::preview_refresh_interval(
                inner.coordinator.settings.preview_refresh_ms,
                inner.last_preview_elapsed,
            );
            let has_growth = Self::has_min_preview_growth(
                total,
                inner.last_preview_sample_end,
                self.sample_rate,
            )
            .ok_or_else(|| {
                inner.mark_session_failed("preview snapshot end 倒退");
                SttError::Engine("STT preview 坐标倒退".to_string())
            })?;
            let window_samples = inner.coordinator.settings.preview_window_ms as usize
                * self.sample_rate as usize
                / 1000;
            // 0.23.13 时间触发短语冻结兜底：停顿检测失灵（底噪高于停顿电平、
            // 连续无候选等）时，滚动窗前缀会无声滚出 preview_window 而从未
            // 冻结——灰色预览只剩最近 3s 碎片。前缀积满 1.2s 即主动追认为
            // 短语（与"安静候选被作废"路径同一入账通道），保证预览无条件
            // 按短语增量累积。纯静音前缀直接跳过锚点，不等识别。
            let roll_start = total.saturating_sub(window_samples);
            if phrase_snapshot.is_none()
                && inner.pending_phrase.is_none()
                && pending.is_none()
                && !inner.sentences.finalize_in_flight
                && (inner.phrase_anchor as usize) < roll_start
                && roll_start - inner.phrase_anchor as usize
                    >= TIME_FREEZE_MIN_PREFIX_MS as usize * self.sample_rate as usize / 1000
            {
                let anchor_before = inner.phrase_anchor;
                let prefix_range = inner.phrase_anchor as usize..roll_start;
                match inner
                    .sentences
                    .abs_to_local_range(&prefix_range, inner.samples.len())
                {
                    Some(local) => {
                        let prefix_samples = &inner.samples[local];
                        // 0.23.14.6 切点选择（合格谷枚举 + 谷内稳健位置 +
                        // roll_start 回退）见 choose_time_freeze_cut；词中硬切
                        // 交事务式锚点回退兜底。
                        let cut_abs = PseudoInner::choose_time_freeze_cut(
                            prefix_samples,
                            anchor_before,
                            roll_start as u64,
                            off_threshold,
                            self.sample_rate,
                        );
                        let prefix_audio =
                            AudioRange::new(anchor_before, cut_abs);
                        let gate = RequestAudioGate::evaluate(
                            prefix_audio,
                            prefix_samples,
                            self.sample_rate,
                            off_threshold,
                            PHRASE_MIN_VOICED_MS,
                            false,
                        );
                        match gate {
                            RequestAudioGate::Valid {
                                model_input_range, ..
                            } => {
                                let offset_start = model_input_range
                                    .start_sample
                                    .saturating_sub(prefix_audio.start_sample)
                                    .min(prefix_samples.len() as u64) as usize;
                                let offset_end = model_input_range
                                    .end_sample
                                    .saturating_sub(prefix_audio.start_sample)
                                    .min(prefix_samples.len() as u64) as usize;
                                let model_samples = if offset_start < offset_end {
                                    Some(prefix_samples[offset_start..offset_end].to_vec())
                                } else {
                                    None
                                };
                                if let Some(samples_snapshot) = model_samples {
                                    inner.next_phrase_request =
                                        inner.next_phrase_request.wrapping_add(1).max(1);
                                    let pending_phrase = PendingPhrase {
                                        request_id: inner.next_phrase_request,
                                        anchor_before,
                                        range: prefix_audio,
                                        generation: inner.preview_generation,
                                    };
                                    phrase_snapshot =
                                        Some((samples_snapshot, pending_phrase.clone()));
                                    inner.pending_phrase = Some(pending_phrase);
                                }
                                inner.phrase_anchor = cut_abs;
                            }
                            RequestAudioGate::TooShort => {
                                // 有声不足 500ms：等前缀再长一点，锚点不动
                            }
                            RequestAudioGate::NoSpeech => {
                                inner.phrase_anchor = roll_start as u64;
                            }
                        }
                    }
                    None => {
                        // 坐标越界（紧凑边界）：跳过该前缀，锚点仍推进
                        inner.phrase_anchor = roll_start as u64;
                    }
                }
            }
            // 0.23.9.10：尾部窗口锚定在短语锚点（最近作废候选 quiet_start 与
            // 已提交水位的较大者）——短语内从头增长成整句，仅当单个短语超过
            // preview_window 后才回退为滚动尾部。Legacy 路径不经过此处。
            let anchor = inner
                .phrase_anchor
                .max(inner.sentences.committed_sample_end as u64)
                .min(total as u64) as usize;
            let range_start = anchor.max(total.saturating_sub(window_samples));
            let abs_range = range_start..total;
            let preview_due = pending.is_none()
                && !inner.sentences.finalize_in_flight
                && inner.last_preview.elapsed() >= preview_interval
                && has_growth
                && abs_range.end > abs_range.start
                && (inner.preview_started
                    || abs_range.len() >= 1_200 * self.sample_rate as usize / 1000);
            let mut should_preview = false;
            let mut snapshot = Vec::new();
            let mut snapshot_end = total;
            let mut preview_range = AudioRange::new(range_start as u64, total as u64);
            let queued_preview = if pending.is_none()
                && !inner.sentences.finalize_in_flight
                && !inner.preview_in_flight
            {
                let generation = inner.preview_generation;
                inner
                    .pending_preview
                    .take()
                    .filter(|queued| queued.generation == generation)
            } else {
                None
            };
            if let Some(queued) = queued_preview {
                should_preview = true;
                snapshot = queued.samples;
                snapshot_end = queued.snapshot_end;
                preview_range = queued.audio_range;
            } else if preview_due {
                let local_range = inner
                    .sentences
                    .abs_to_local_range(&abs_range, inner.samples.len())
                    .ok_or_else(|| {
                        inner.mark_session_failed("preview snapshot 坐标非法");
                        SttError::Engine("STT preview 坐标非法".to_string())
                    })?;
                snapshot = inner.samples[local_range].to_vec();
                let gate = RequestAudioGate::evaluate(
                    preview_range,
                    &snapshot,
                    self.sample_rate,
                    off_threshold,
                    if inner.preview_started { 1 } else { 1_200 },
                    inner.preview_started,
                );
                match gate {
                    RequestAudioGate::Valid {
                        model_input_range, ..
                    } => {
                        let start_offset = model_input_range
                            .start_sample
                            .saturating_sub(preview_range.start_sample)
                            .min(snapshot.len() as u64)
                            as usize;
                        let end_offset = model_input_range
                            .end_sample
                            .saturating_sub(preview_range.start_sample)
                            .min(snapshot.len() as u64)
                            as usize;
                        if start_offset < end_offset {
                            // Gate 返回的 model range 也决定送模切片，避免
                            // 2–4s 窗口前导静音重新进入模型。
                            snapshot = snapshot[start_offset..end_offset].to_vec();
                            preview_range = model_input_range;
                            inner.preview_started = true;
                            if inner.preview_in_flight {
                                // 0.23.9.10：入队刷新限频——in-flight 期间 due
                                // 条件在每个音频块都成立（last_preview 只在完成时
                                // 推进），若每个 10ms 块都 replace 排队快照，会以
                                // 块率消耗 coordinator request id（实测 ~100/s，
                                // spawn id 从 1 膨胀到三位数）。仅当排队快照落后
                                // ≥500ms 新音频时才替换。
                                let refresh_due =
                                    inner.pending_preview.as_ref().map_or(true, |queued| {
                                        total.saturating_sub(queued.snapshot_end)
                                            >= (PREVIEW_MIN_NEW_AUDIO_MS * self.sample_rate as u64
                                                / 1000)
                                                as usize
                                    });
                                if refresh_due {
                                    inner.pending_preview = Some(PendingPreview {
                                        samples: snapshot.clone(),
                                        snapshot_end: total,
                                        audio_range: preview_range,
                                        generation: inner.preview_generation,
                                    });
                                    // 0.23.9：通过 coordinator 同步 pending preview，
                                    // 使 coordinator 的 pending_preview 槽与生产一致。
                                    let preview_request_id = inner.coordinator.next_request_id();
                                    let preview_revision = inner.preview_revision + 1;
                                    inner.coordinator.replace_pending_preview(PreviewRequest {
                                        request_id: preview_request_id,
                                        audio_range: preview_range,
                                        model_input_range: Some(model_input_range),
                                        revision: preview_revision,
                                    });
                                    // next_request_id 已经消耗了一个 id；把它
                                    // 记录到 next_preview_request 保持同步。
                                    inner.next_preview_request =
                                        inner.next_preview_request.max(preview_request_id);
                                }
                            } else {
                                should_preview = true;
                            }
                        }
                    }
                    RequestAudioGate::TooShort => {
                        snapshot.clear();
                    }
                    RequestAudioGate::NoSpeech => {
                        snapshot.clear();
                    }
                }
            }

            (
                pending,
                should_preview,
                snapshot,
                snapshot_end,
                preview_range,
                phrase_snapshot,
            )
        };

        // 0.23.9.10 / 0.23.14：预览定稿短语识别（锁外 spawn，事务式入账，
        // 见 spawn_phrase_recognition）。
        if let Some((phrase_samples, pending_phrase)) = phrase_snapshot {
            self.spawn_phrase_recognition(phrase_samples, pending_phrase);
        }

        if let Some(pending) = pending_segment {
            let sentence_samples = {
                let inner = Self::try_lock(&self.inner).ok_or_else(|| {
                    SttError::Engine("STT session 已损坏 (Mutex poisoned)".to_string())
                })?;
                let local_range = inner
                    .sentences
                    .abs_to_local_range(&pending.range, inner.samples.len())
                    .ok_or_else(|| SttError::Engine("STT Draft 坐标非法".to_string()))?;
                inner.samples[local_range].to_vec()
            };
            self.spawn_sentence_finalize(sentence_samples, pending.identity);
        }

        {
            let mut inner = Self::try_lock(&self.inner).ok_or_else(|| {
                SttError::Engine("STT session 已损坏 (Mutex poisoned)".to_string())
            })?;
            let samples_len = inner.samples.len();
            if let Ok(Some(n)) = inner.sentences.try_compact(samples_len) {
                inner.samples.drain(..n);
            }
            // 如果 Preview 正在飞行，新的滚动快照已经被保存在 pending 槽；
            // 下一个音频块会在 owner 释放后取走，旧结果不能越过 request id。
        }
        if should_preview {
            self.spawn_preview_recognition(samples_snapshot, snapshot_end, snapshot_range);
        }
        self.compose_typed_result()
    }

    /// 0.23.14.7 P1-1 Preview 投影不变量：span 序列全部为有效半开区间，且按音频
    /// 时间单调、互不重叠（`previous.end_sample <= next.start_sample`）。
    /// 返回首个违规 span 的下标（语义上属于"应被移除的后到者"：区间退化，
    /// 或与前一个 span 重叠）。组合预览出口（`compose_typed_result`）以此
    /// 终审，事件消费方的 span 清单同源同构。
    fn preview_spans_invariant_violation(spans: &[PreviewSegment]) -> Option<usize> {
        for (index, span) in spans.iter().enumerate() {
            if span.range.start_sample >= span.range.end_sample {
                return Some(index);
            }
            if index > 0 && spans[index - 1].range.end_sample > span.range.start_sample {
                return Some(index);
            }
        }
        None
    }

    /// PreviewDraft profile 的类型化兼容 envelope。
    ///
    /// `streaming_port` 优先解析 `kind=draft/preview`；旧 G1/G3 仍消费上面
    /// 的 v2 累计字段。每次调用最多交付一个 span，避免可靠事件被 latest
    /// preview 槽覆盖；cursor 使无音频块时也能补齐已完成的 Draft。
    fn compose_typed_result(&self) -> Result<String, SttError> {
        let mut inner = Self::try_lock(&self.inner)
            .ok_or_else(|| SttError::Engine("STT session 已损坏 (Mutex poisoned)".to_string()))?;
        if let Some(span) = inner
            .sentences
            .draft_spans()
            .get(inner.last_reported_span_count)
            .cloned()
        {
            inner.last_reported_span_count += 1;
            return Ok(serde_json::json!({
                "v": 2,
                "kind": "draft",
                "span": span,
            })
            .to_string());
        }
        if inner.preview_revision != inner.last_reported_preview_revision {
            inner.last_reported_preview_revision = inner.preview_revision;
            let committed = inner.sentences.committed_sample_end as u64;
            // 0.23.14.6：组合预览的组成段清单 = 短语账本 + 当前尾部，每段
            // 携带自己的音频范围——这是对外范围的单一真源；顶层 audioRange
            // 是全部 span 的包络。非空预览必然至少有一个非退化 span，禁止
            // 退化为 (committed, committed) 零长度区间。
            let mut spans: Vec<PreviewSegment> = inner.preview_phrases.clone();
            if !inner.preview_tail.is_empty()
                && let Some(tail_range) = inner.preview_tail_range
            {
                spans.push(PreviewSegment::new(tail_range, inner.preview_tail.clone()));
            }
            // 0.23.14.7 P1-1：投影不变量终审——span 序列必须单调、互不重叠、
            // 区间有效。上游裁决（settle_tail_for_phrase / tail_range_is_stale）
            // 之后的违规是缺陷信号，不允许带病投影变成用户可见的重复文本；
            // 就地移除违规 span（后到者让位），同步修正组合文本并记 error。
            let mut repaired = false;
            while let Some(index) = Self::preview_spans_invariant_violation(&spans) {
                tracing::error!(
                    index,
                    spans = spans.len(),
                    "Preview span 不变量破坏：移除违规 span 修复投影"
                );
                spans.remove(index);
                repaired = true;
            }
            if repaired {
                // 0.23.14.7 P2：修复必须持久化回 Preview 真源（短语账本 +
                // 尾部），否则下次 compose 从未修复的真源重建同一冲突——再次
                // 记 error、再次发同一 Preview。span 清单 = 短语账本 + 尾部
                // 顺序拼接，repair 只删除不重排：尾部若幸存必在末尾，按
                // AudioRange 身份判定（重叠副本已被修复移除，range 相等唯一）。
                let tail_survived = inner.preview_tail_range.is_some()
                    && spans
                        .last()
                        .is_some_and(|last| Some(last.range) == inner.preview_tail_range);
                if tail_survived {
                    let phrases_len = spans.len() - 1;
                    inner.preview_phrases = spans[..phrases_len].to_vec();
                    // preview_tail / preview_tail_range 保持不变（幸存 span 即
                    // 尾部本体，不按字符串重裁）。
                } else {
                    inner.preview_phrases = spans.clone();
                    inner.preview_tail.clear();
                    inner.preview_tail_range = None;
                }
                inner.latest_preview = spans.iter().map(|span| span.text.as_str()).collect();
                // 0.23.14.7 P2：不再递增 preview_revision——本次调用返回的
                // envelope 已经携带修复后内容，且 reported cursor 在本调用
                // 开头已对齐当前 revision；若在此再递增，下一次 compose 会把
                // 同一份（已修复的）内容当"未上报变化"再次发出。真源已收敛，
                // revision 只随真实预览状态变化推进（rebuild_preview_text）。
            }
            let envelope_start = spans
                .first()
                .map(|span| span.range.start_sample)
                .unwrap_or(committed);
            let envelope_end = spans
                .last()
                .map(|span| span.range.end_sample)
                .unwrap_or(committed);
            let range = AudioRange::new(envelope_start, envelope_end);
            return Ok(serde_json::json!({
                "v": 2,
                "kind": "preview",
                "requestId": inner.latest_preview_request_id.max(inner.preview_revision),
                "audioRange": range,
                "revision": inner.preview_revision,
                "text": inner.latest_preview,
                "spans": spans,
            })
            .to_string());
        }
        Ok(String::new())
    }

    /// 取走已提交但尚未经 `compose_typed_result` 上报的 Draft span。
    ///
    /// 供一次性回放诊断在 `finalize` 后补收尾段与句间在途 Draft——terminal
    /// finalize 通过 `commit_terminal_finalize` 直接把尾段写入 Draft ledger，
    /// 不经过 chunk 响应，逐块上报的游标在此之后不会再推进。生产事件路径
    /// 不使用（Final 携带累计全文）。游标语义与 `compose_typed_result`
    /// 一致：取走后不会再次出现。
    pub fn drain_unreported_draft_spans(&self) -> Vec<DraftSpan> {
        let Some(mut inner) = Self::try_lock(&self.inner) else {
            return Vec::new();
        };
        let spans = inner.sentences.draft_spans();
        let pending = spans
            .get(inner.last_reported_span_count..)
            .unwrap_or_default()
            .to_vec();
        inner.last_reported_span_count = spans.len();
        pending
    }

    /// 0.23.9: 从生产 Coordinator 产出只读诊断快照。
    ///
    /// 此方法只读取当前生产会话的 `RecognitionCoordinator` 状态，不修改
    /// 任何调度状态、不触发推理、不推进水位。app 层在 VAD 调试页打开时
    /// 按需拉取，关闭后停止。
    ///
    /// `max_uncommitted_s` 由调用方从配置传入，用于计算 backlog_limit。
    pub fn snapshot_coordinator_trace(&self, max_uncommitted_s: u64) -> Option<CoordinatorTrace> {
        Self::try_lock(&self.inner).map(|inner| {
            // 同步 PseudoInner 的 SentenceState 水位到 coordinator，确保
            // trace 反映的是真实生产水位而非过期快照。
            let mut coord = inner.coordinator.clone();
            coord.captured_audio_end = inner
                .sentences
                .buffer_base_sample
                .saturating_add(inner.samples.len()) as u64;
            coord.draft_committed_audio_end = inner.sentences.committed_sample_end as u64;
            coord.draft_reserved_audio_end = inner
                .sentences
                .draft_reserved_sample_end
                .max(inner.sentences.committed_sample_end)
                as u64;
            coord.snapshot_trace(max_uncommitted_s)
        })
    }

    async fn finalize_with_wait_timeout(&self, wait_timeout: Duration) -> Result<String, SttError> {
        // 先给 preview/segment task 一个有界完成窗口。超时不是“都完成了”：
        // 后续会原子推进提交权代际并接管剩余区间，迟到 task 只能被丢弃。
        let deadline = Instant::now() + wait_timeout;
        let timed_out = loop {
            let (preview_in_flight, finalize_in_flight) = {
                let inner = Self::try_lock(&self.inner).ok_or_else(|| {
                    SttError::Engine("STT session 已损坏 (Mutex poisoned)".to_string())
                })?;
                if inner.session_failed {
                    return Err(SttError::Engine(
                        "STT session 已失败，需要 reset 后重试".to_string(),
                    ));
                }
                (inner.preview_in_flight, inner.sentences.finalize_in_flight)
            };
            if !preview_in_flight && !finalize_in_flight {
                break false;
            }
            if Instant::now() >= deadline {
                break true;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        };

        // 原子冻结本 session 的尾段并取得唯一提交权。推进 preview 代际也会
        // 使迟到 preview 无法写入 reset 后或 terminal finalize 中的状态。
        let (identity, remaining_samples, abs_end, off_threshold, tail_strong_run, profile) = {
            let mut inner = Self::try_lock(&self.inner).ok_or_else(|| {
                SttError::Engine("STT session 已损坏 (Mutex poisoned)".to_string())
            })?;
            if inner.sentences.finalizing.is_some() {
                return Err(SttError::Engine("STT session 正在 finalize".to_string()));
            }
            let abs_start = inner.sentences.committed_sample_end;
            let abs_end = inner
                .sentences
                .buffer_base_sample
                .checked_add(inner.samples.len())
                .ok_or_else(|| SttError::Engine("STT 音频坐标溢出".to_string()))?;
            let local_range = inner
                .sentences
                .abs_to_local_range(&(abs_start..abs_end), inner.samples.len())
                .ok_or_else(|| SttError::Engine("STT finalize 音频坐标非法".to_string()))?;
            let remaining_samples = inner.samples[local_range].to_vec();
            let off_threshold = inner.vad.current_off_threshold();
            // 0.23.14.7 case_17：终态尾段的可信发声证据**复用流式期间冻结的
            // 累计值**，不在 finalize 时用"当下阈值"重新测量。原因：VAD 的
            // on 阈值随底噪自适应，而在尾段静音中底噪会漂移，同一段音频在
            // 不同时刻用不同阈值测量会得出互相矛盾的结论（实测 case_04 远场
            // 低声：流式累计连续段 290ms、finalize 时刻重测只有 90ms）。
            // `uncommitted_strong_run_max` 自上一次采纳（或会话开始）起累计，
            // 正好覆盖 [上次采纳边界, 当前尾端] ⊇ [committed, 尾端]。
            let tail_strong_run = inner.uncommitted_strong_run_max;
            let profile = inner.coordinator.profile;
            let identity = inner.sentences.begin_terminal_finalize();
            inner.preview_generation = inner.preview_generation.wrapping_add(1);
            inner.preview_in_flight = false;
            // 0.23.9：通过 coordinator 的 begin_closing 标记终态，
            // 清空 pending preview，使 coordinator 状态与生产一致。
            inner.coordinator.begin_closing(abs_end as u64);
            // 原子撤销预览所有权：在途预览任务即使迟到也无法再写状态
            inner.preview_owner = 0;
            (identity, remaining_samples, abs_end, off_threshold, tail_strong_run, profile)
        };

        if timed_out {
            tracing::warn!(
                session = identity.session_generation,
                commit_generation = identity.commit_generation,
                "finalize: 等待 in-flight 超时，已撤销旧任务提交权并接管尾段"
            );
        }

        // 0.23.14.7 case_17 噪声门控：终态尾段必须先有可信发声证据。纯环境声
        // 尾段（只有风扇/键盘脉冲）在此按 NoSpeech 消费——不调用模型，从而不会
        // 把环境声幻觉追加进终态文本。没有这道门，被候选采纳层拒绝的噪声尾段
        // 会在 drain 里"补一刀"重新进入模型。
        //
        // **仅 PreviewDraft**：可信度证据（连续强有声段）只在 PreviewDraft 的
        // 候选观察路径累计；Legacy（G1/G3 默认）没有这条证据链，若一并套用会
        // 因证据恒为空而**拒掉全部终态文本**——终态覆盖未提交音频是 Legacy 的
        // 既有契约，必须原样保留（引擎级不变量测试 `valley_*` / `retreated_*`
        // 覆盖该契约）。
        let tail_credible = profile != RecognitionProfile::PreviewDraft
            || PseudoInner::credible_voicing_run(tail_strong_run, self.sample_rate);
        if !tail_credible && !remaining_samples.is_empty() {
            tracing::debug!(
                session = identity.session_generation,
                samples = remaining_samples.len(),
                strong_run_ms = tail_strong_run * 1000 / u64::from(self.sample_rate).max(1),
                "终态尾段无可信发声证据，按 NoSpeech 消费"
            );
        }
        let (finalize_text, finalize_error) =
            if remaining_samples.is_empty() || !tail_credible {
                (String::new(), None)
            } else {
            match self
                .transcribe_samples(&remaining_samples, off_threshold)
                .await
            {
                Ok(text) => (text, None),
                Err(e) => {
                    tracing::warn!(%e, "finalize 定稿识别失败，保留尾段等待重试");
                    (String::new(), Some(e))
                }
            }
        };

        if let Some(error) = finalize_error {
            let mut inner = Self::try_lock(&self.inner).ok_or_else(|| {
                SttError::Engine("STT session 已损坏 (Mutex poisoned)".to_string())
            })?;
            if !inner.sentences.abort_terminal_finalize(identity) {
                return Err(SttError::Engine(
                    "STT session 在 finalize 期间已重置".to_string(),
                ));
            }
            return Err(error);
        }

        let final_text = {
            let mut inner = Self::try_lock(&self.inner).ok_or_else(|| {
                SttError::Engine("STT session 已损坏 (Mutex poisoned)".to_string())
            })?;
            let before_confirmed = inner.sentences.confirmed_revision;
            if !inner
                .sentences
                .commit_terminal_finalize(identity, abs_end, &finalize_text)
            {
                tracing::debug!(
                    session = identity.session_generation,
                    commit_generation = identity.commit_generation,
                    "丢弃 reset/新接管后的 terminal finalize 结果"
                );
                return Err(SttError::Engine(
                    "STT session 在 finalize 期间已重置".to_string(),
                ));
            }
            if inner.sentences.confirmed_revision != before_confirmed {
                inner.state_revision = inner.state_revision.wrapping_add(1);
                // 0.23.9：通过 coordinator 同步终态提交水位。
                inner.coordinator.commit_terminal(abs_end as u64);
            }
            // 0.23.9.10：终态提交覆盖整个会话尾段（含 NoSpeech 消费），
            // 统一收敛预览账本与锚点。
            inner.settle_preview_after_commit();

            // Preview 永远只是可变反馈，不能在 terminal/final-tail 失败时
            // 伪装成可靠文本提交；G2 由 app 根据 Final/错误状态决定是否注入。
            inner.sentences.confirmed_text()
        };

        tracing::info!(text_len = final_text.chars().count(), "伪流式识别完成");
        Ok(final_text)
    }
}

#[async_trait::async_trait]
impl SttEngine for PseudoStreamingSttEngine {
    async fn transcribe_chunk(&self, samples: &[f32]) -> Result<String, SttError> {
        let preview_draft = Self::try_lock(&self.inner)
            .map(|inner| inner.coordinator.profile == RecognitionProfile::PreviewDraft)
            .unwrap_or(false);
        if preview_draft {
            return self.transcribe_chunk_preview_draft(samples).await;
        }
        // ── 1. 累积音频 + 喂 VAD ──
        let (_vad_event, pending_segment, should_preview, samples_snapshot, snapshot_end) = {
            let mut inner = match Self::try_lock(&self.inner) {
                Some(g) => g,
                None => {
                    tracing::error!("Mutex poisoned at transcribe_chunk start");
                    return Err(SttError::Engine(
                        "STT session 已损坏 (Mutex poisoned)".to_string(),
                    ));
                }
            };
            // 0.22.15 follow-up: session 失败态检查
            if inner.session_failed {
                return Err(SttError::Engine(
                    "STT session 已失败，需要 reset 后重试".to_string(),
                ));
            }
            if inner.sentences.finalizing.is_some() {
                return Err(SttError::Engine("STT session 正在 finalize".to_string()));
            }
            inner.samples.extend_from_slice(samples);
            // 绝对尾端 = buffer_base + samples.len()
            let total = match inner
                .sentences
                .buffer_base_sample
                .checked_add(inner.samples.len())
            {
                Some(total) => total,
                None => {
                    inner.mark_session_failed("计算音频绝对尾端溢出");
                    return Err(SttError::Engine("STT 音频坐标溢出".to_string()));
                }
            };

            // 喂 VAD
            let mut event = inner.vad.process_chunk(samples);
            let mut boundary_reason = event.reason();

            // VAD 的 speaking 状态不是内存/负载边界：真实麦克风输入可能长期
            // 落在 on/off 滞回区，导致 VAD 计时不前进。按绝对未提交音频长度
            // 再做一次硬限制，保证送入模型的窗口不会无限增长。
            if !event.is_boundary()
                && inner.sentences.pending.is_none()
                && !inner.sentences.finalize_in_flight
            {
                match inner.exceeds_uncommitted_hard_limit(
                    total,
                    inner.sentences.committed_sample_end,
                    self.sample_rate,
                ) {
                    Some(true) => {
                        event = VadEvent::HardWindow;
                        boundary_reason = "uncommitted_cap";
                    }
                    Some(false) => {}
                    None => {
                        inner.mark_session_failed("计算未提交音频长度失败");
                        return Err(SttError::Engine("STT 音频坐标倒退".to_string()));
                    }
                }
            }

            // 0.22.15：处理句尾——创建 pending segment（不推进 committed end）
            // 先 clone latest_preview 避免 mutable/immutable 借用冲突
            let pending = if event.is_boundary() {
                let preview_snapshot = inner.latest_preview.clone();
                // 0.23.7.2 D：强制切（VAD 硬窗口/未提交上限）优先回退到近期
                // 有界能量谷底；无合格谷底或回退点不落在未提交区间内时，
                // 保持当前时刻兜底。只移动切点位置，不新增边界。
                let mut boundary_total = total;
                if event == VadEvent::HardWindow {
                    if let Some(offset) = inner
                        .vad
                        .low_energy_valley_offset(HARD_CUT_VALLEY_WINDOW_MS)
                    {
                        let candidate = total.saturating_sub(offset);
                        let committed = inner.sentences.committed_sample_end;
                        if candidate > committed && candidate < total {
                            boundary_total = candidate;
                            boundary_reason = match boundary_reason {
                                "uncommitted_cap" => "uncommitted_cap_valley",
                                _ => "hard_window_valley",
                            };
                        }
                    }
                }
                tracing::debug!(
                    reason = boundary_reason,
                    total,
                    boundary_total,
                    "STT segment boundary"
                );
                if let Some(observer) = &self.boundary_observer {
                    observer
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .push(SttBoundaryRecord {
                            audio_ms: boundary_total as u64 * 1000 / self.sample_rate as u64,
                            reason: boundary_reason,
                            #[cfg(test)]
                            observed_at: Instant::now(),
                        });
                }
                // 记录 segment 在边界处创建的时间。即使上一个 finalize 仍在
                // 飞行中、此处转为 deferred，也必须保留该时间点以测量排队。
                // identity 与 SentenceState::on_sentence_end 使用同一组当前值。
                #[cfg(test)]
                {
                    let created_identity = SegmentIdentity {
                        session_generation: inner.sentences.session_generation,
                        commit_generation: inner.sentences.commit_generation,
                        segment_id: inner.sentences.next_segment_id,
                    };
                    Self::record_finalize(&self.finalize_observer, "created", created_identity);
                }
                let pending = inner
                    .sentences
                    .on_sentence_end(boundary_total, &preview_snapshot);
                if let Err(reason) = inner.reset_vad_at_boundary(boundary_total, total) {
                    inner.mark_session_failed(reason);
                    return Err(SttError::Engine(reason.to_string()));
                }
                pending
            } else {
                None
            };

            // 检查是否该触发预览
            let interval = Self::preview_interval(
                inner.samples.len(),
                self.sample_rate,
                inner.last_preview_elapsed,
            );
            let has_min_growth = match Self::has_min_preview_growth(
                total,
                inner.last_preview_sample_end,
                self.sample_rate,
            ) {
                Some(value) => value,
                None => {
                    inner.mark_session_failed("preview snapshot end 倒退");
                    return Err(SttError::Engine("STT preview 坐标倒退".to_string()));
                }
            };
            let should_preview = inner.last_preview.elapsed() >= interval
                && has_min_growth
                && !inner.preview_in_flight
                && !inner.sentences.finalize_in_flight
                && !event.is_boundary();

            // 句尾时清空预览（本句已定稿，下一段预览从空开始）
            // 同时递增 generation，使 in-flight 的旧预览返回时被丢弃（防重复影子）
            if event.is_boundary() {
                inner.clear_preview();
                inner.preview_generation = inner.preview_generation.wrapping_add(1);
                inner.last_preview = Instant::now();
                inner.last_preview_elapsed = Duration::ZERO;
                inner.last_preview_sample_end = total;
            }

            let snapshot = if should_preview {
                // 只取未 committed 部分的音频（绝对→局部转换）
                let abs_range = inner.sentences.committed_sample_end..total;
                match inner
                    .sentences
                    .abs_to_local_range(&abs_range, inner.samples.len())
                {
                    Some(local_range) => inner.samples[local_range].to_vec(),
                    None => {
                        tracing::error!(
                            committed_end = inner.sentences.committed_sample_end,
                            total,
                            buffer_base = inner.sentences.buffer_base_sample,
                            samples_len = inner.samples.len(),
                            "preview snapshot 坐标非法，跳过本轮预览"
                        );
                        inner.mark_session_failed("preview snapshot 坐标非法");
                        Vec::new()
                    }
                }
            } else {
                Vec::new()
            };

            (event, pending, should_preview, snapshot, total)
        };

        // ── 2. VAD 句尾 → spawn 定稿识别（后台 worker transport） ──
        if let Some(pending) = pending_segment {
            let sentence_samples: Vec<f32> = {
                let mut inner = match Self::try_lock(&self.inner) {
                    Some(g) => g,
                    None => {
                        tracing::error!(
                            seg = pending.identity.segment_id,
                            "Mutex poisoned at sentence sample extraction"
                        );
                        return Err(SttError::Engine(
                            "STT session 已损坏 (Mutex poisoned)".to_string(),
                        ));
                    }
                };
                // pending.range 是绝对坐标，转换为局部切片
                match inner
                    .sentences
                    .abs_to_local_range(&pending.range, inner.samples.len())
                {
                    Some(local_range) => inner.samples[local_range].to_vec(),
                    None => {
                        tracing::error!(
                            range = ?pending.range,
                            buffer_base = inner.sentences.buffer_base_sample,
                            samples_len = inner.samples.len(),
                            seg = pending.identity.segment_id,
                            "定稿音频坐标非法，跳过此 segment"
                        );
                        inner.mark_session_failed("定稿音频坐标非法");
                        Vec::new()
                    }
                }
            };

            self.spawn_sentence_finalize(sentence_samples, pending.identity);
        }

        // 0.22.15 fix: 尝试 compact 已 committed PCM（防止长录音内存无界增长）
        {
            let mut inner = match Self::try_lock(&self.inner) {
                Some(g) => g,
                None => {
                    tracing::error!("Mutex poisoned at compact attempt");
                    return Err(SttError::Engine(
                        "STT session 已损坏 (Mutex poisoned)".to_string(),
                    ));
                }
            };
            let samples_len = inner.samples.len();
            match inner.sentences.try_compact(samples_len) {
                Ok(Some(n)) => {
                    inner.samples.drain(..n);
                }
                Ok(None) => {}
                Err(reason) => {
                    inner.mark_session_failed(reason);
                    return Err(SttError::Engine("STT compact 坐标非法".to_string()));
                }
            }
        }

        // ── 3. 500ms 定时 → spawn 预览识别（后台 worker transport） ──
        if should_preview {
            let snapshot_range = AudioRange::new(
                snapshot_end.saturating_sub(samples_snapshot.len()) as u64,
                snapshot_end as u64,
            );
            self.spawn_preview_recognition(samples_snapshot, snapshot_end, snapshot_range);
        }

        // ── 4. 组装返回（状态边沿触发） ──
        // 状态版本未变化时返回空串——消费方据此不产生任何对外事件。
        // 修复前每块音频都返回一整套「全量 confirmed + preview」，事件量与
        // 正文搬运量随录音时长平方级增长（92s 录音 ≈ 8,800 条状态事件）。
        // strip_confirmed_prefix 兜底：即使预览只取了未确认音频，
        // 模型仍可能因为句子边界切分不完全而产生部分重叠文本
        let (revision, confirmed, preview, confirmed_changed) = {
            let mut inner = match Self::try_lock(&self.inner) {
                Some(g) => g,
                None => {
                    tracing::error!("Mutex poisoned at result compose");
                    return Err(SttError::Engine(
                        "STT session 已损坏 (Mutex poisoned)".to_string(),
                    ));
                }
            };
            let revision = inner.state_revision;
            if inner.last_reported_state == Some(revision) {
                return Ok(String::new());
            }
            inner.last_reported_state = Some(revision);
            // 仅当 confirmed 真正增长时才携带累计正文
            let confirmed_changed =
                inner.sentences.confirmed_revision != inner.last_reported_confirmed_revision;
            inner.last_reported_confirmed_revision = inner.sentences.confirmed_revision;
            let full_confirmed = inner.sentences.confirmed_text();
            let preview = strip_confirmed_prefix(&full_confirmed, &inner.latest_preview);
            let confirmed = if confirmed_changed {
                full_confirmed
            } else {
                String::new()
            };
            (revision, confirmed, preview, confirmed_changed)
        };

        Ok(Self::compose_result(
            revision,
            &confirmed,
            &preview,
            confirmed_changed,
        ))
    }

    async fn finalize(&self) -> Result<String, SttError> {
        self.finalize_with_wait_timeout(Duration::from_millis(FINALIZE_WAIT_TIMEOUT_MS))
            .await
    }

    fn reset(&self) {
        let mut inner = match self.inner.lock() {
            Ok(g) => g,
            Err(poison) => {
                // Mutex poisoned — 使用 into_inner 兜底，
                // 因为 reset 必须能执行（否则整个 session 永久卡死）
                tracing::error!("Mutex poisoned at reset — 强制恢复");
                let mut g = poison.into_inner();
                g.vad.reset();
                g.sentences.reset();
                g.samples.clear();
                g.last_preview = Instant::now();
                g.last_preview_elapsed = Duration::ZERO;
                g.last_preview_sample_end = 0;
                g.preview_in_flight = false;
                g.preview_owner = 0;
                g.latest_preview.clear();
                g.preview_tail_range = None;
                g.latest_preview_request_id = 0;
                g.preview_revision = 0;
                g.preview_generation = g.preview_generation.wrapping_add(1);
                g.coordinator.reset();
                g.boundary_candidate = None;
                g.uncommitted_voiced_samples = 0;
                g.uncommitted_strong_samples = 0;
                g.uncommitted_strong_run_samples = 0;
                g.uncommitted_strong_run_max = 0;
                g.decision_wait_reason = None;
                g.pending_preview = None;
                g.preview_started = false;
                g.preview_phrases.clear();
                g.pending_phrase = None;
                g.next_phrase_request = 0;
                g.phrase_anchor = 0;
                g.preview_tail.clear();
                g.preview_settled_committed = 0;
                g.stale_before_worker = 0;
                g.preview_retreat_chars = 0;
                g.state_revision = 0;
                g.last_reported_state = None;
                g.last_reported_confirmed_revision = 0;
                g.last_reported_span_count = 0;
                g.last_reported_preview_revision = 0;
                g.session_failed = false;
                drop(g);
                self.inner.clear_poison();
                tracing::debug!("伪流式引擎 reset (from poison recovery)");
                return;
            }
        };
        inner.vad.reset();
        inner.sentences.reset();
        inner.samples.clear();
        inner.last_preview = Instant::now();
        inner.last_preview_elapsed = Duration::ZERO;
        inner.last_preview_sample_end = 0;
        inner.preview_in_flight = false;
        inner.preview_owner = 0;
        inner.latest_preview.clear();
        inner.preview_tail_range = None;
        inner.latest_preview_request_id = 0;
        inner.preview_revision = 0;
        inner.preview_generation = inner.preview_generation.wrapping_add(1);
        inner.coordinator.reset();
        inner.boundary_candidate = None;
        inner.uncommitted_voiced_samples = 0;
        inner.uncommitted_strong_samples = 0;
        inner.uncommitted_strong_run_samples = 0;
        inner.uncommitted_strong_run_max = 0;
        inner.decision_wait_reason = None;
        inner.pending_preview = None;
        inner.preview_started = false;
        inner.preview_phrases.clear();
        inner.pending_phrase = None;
        inner.next_phrase_request = 0;
        inner.phrase_anchor = 0;
        inner.preview_tail.clear();
        inner.preview_settled_committed = 0;
        inner.stale_before_worker = 0;
        inner.preview_retreat_chars = 0;
        inner.state_revision = 0;
        inner.last_reported_state = None;
        inner.last_reported_confirmed_revision = 0;
        inner.last_reported_span_count = 0;
        inner.last_reported_preview_revision = 0;
        inner.session_failed = false;
        tracing::debug!("伪流式引擎 reset");
    }

    /// 诊断快照：PCM 大小、在途推理任务数、状态版本（不含正文）。
    fn stream_stats(&self) -> SttStreamStats {
        match Self::try_lock(&self.inner) {
            Some(inner) => SttStreamStats {
                pcm_samples: inner.samples.len(),
                pcm_committed_end: inner.sentences.committed_sample_end,
                preview_in_flight: inner.preview_in_flight,
                finalize_in_flight: inner.sentences.finalize_in_flight,
                in_flight_inferences: inner.in_flight_inferences(),
                confirmed_revision: inner.sentences.confirmed_revision,
                preview_revision: inner.preview_revision,
                stale_before_worker: inner.stale_before_worker,
                preview_retreat_chars: inner.preview_retreat_chars,
                ..SttStreamStats::default()
            },
            None => SttStreamStats::default(),
        }
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    /// 0.23.10.2：trait 入口委托到内部 fire-and-forget 预热。
    fn warm_up(&self) {
        self.warm_up_worker();
    }
}

// ── 测试 ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests;
