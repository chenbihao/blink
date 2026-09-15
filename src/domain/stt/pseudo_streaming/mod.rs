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

use super::postprocess::{strip_confirmed_prefix, strip_filler_words, trim_trailing_silence};
use super::sentence_state::{FinalizeResult, PendingSegment, SegmentIdentity, SentenceState};
use super::vad::{EnergyVad, VadEvent};
use super::{SttEngine, SttError, SttStreamStats};

/// 预览识别间隔（毫秒）。
const PREVIEW_INTERVAL_MS: u64 = 500;

/// 累积音频超过此时长时，预览间隔自动拉长（毫秒）。
const PREVIEW_SLOWDOWN_THRESHOLD_MS: u64 = 8000;

/// 预览间隔在慢速模式下的值（毫秒）。
const PREVIEW_SLOW_INTERVAL_MS: u64 = 1000;

/// 两次预览之间至少新增的音频。
const PREVIEW_MIN_NEW_AUDIO_MS: u64 = 500;
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
    /// 未提交音频上限（毫秒）：未提交音频达到该值后强制切段（0.23.7 可配置）。
    ///
    /// 按**绝对未提交音频**计算，与 VAD speaking 状态无关——VAD 的
    /// 软/硬窗口只在有声阶段计时，此上限额外覆盖 VAD 计时停滞的场景。
    max_uncommitted_audio_ms: u64,
    /// 0.22.15 follow-up: session 失败标志。
    ///
    /// 当内部不变量被破坏（如坐标非法）时设为 `true`。
    /// 设为 true 后，`transcribe_chunk` 和 `finalize` 返回 `SttError`，
    /// 不再处理新音频。`reset` 清除此标志。
    session_failed: bool,
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
            preview_revision: 0,
            preview_generation: 0,
            state_revision: 0,
            last_reported_state: None,
            last_reported_confirmed_revision: 0,
            max_uncommitted_audio_ms: MAX_UNCOMMITTED_AUDIO_MS,
            session_failed: false,
        }
    }

    /// 提交/回滚一个 finalize 结果，并在 confirmed 实际增长时推进状态版本。
    fn commit_or_rollback(&mut self, result: &FinalizeResult) -> Option<PendingSegment> {
        let before = self.sentences.confirmed_revision;
        let deferred = self.sentences.commit_or_rollback(result);
        if self.sentences.confirmed_revision != before {
            self.state_revision = self.state_revision.wrapping_add(1);
        }
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
    fn set_preview_if_changed(&mut self, preview: String) {
        if preview.is_empty() || preview == self.latest_preview {
            return;
        }
        self.latest_preview = preview;
        self.preview_revision = self.preview_revision.wrapping_add(1);
        self.state_revision = self.state_revision.wrapping_add(1);
        tracing::trace!(
            preview_revision = self.preview_revision,
            chars = self.latest_preview.chars().count(),
            "预览版本变化"
        );
    }

    /// 清空预览（句尾/终态）；非空时才推进版本。
    fn clear_preview(&mut self) {
        if self.latest_preview.is_empty() {
            return;
        }
        self.latest_preview.clear();
        self.preview_revision = self.preview_revision.wrapping_add(1);
        self.state_revision = self.state_revision.wrapping_add(1);
        tracing::trace!(
            preview_revision = self.preview_revision,
            "预览已清空（句尾）"
        );
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

    /// 在途推理任务数（预览 + 定稿 + 排队句段；诊断用）。
    fn in_flight_inferences(&self) -> usize {
        usize::from(self.preview_in_flight)
            + usize::from(self.sentences.finalize_in_flight)
            + usize::from(self.sentences.pending.is_some())
            + usize::from(self.sentences.deferred.is_some())
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
        let model = config.local_engine.funasr_model.clone();

        if conn.transport.is_none() {
            return Err(
                "本地 STT 连接缺少 worker 通道（GGUF worker 是唯一本地实现）。\
                 请确认语音服务已在设置页启动。"
                    .to_string(),
            );
        }

        let vad_cfg = &config.local_engine.vad;
        tracing::info!(
            model = %model,
            silence_threshold = vad_cfg.silence_threshold,
            min_silence_ms = vad_cfg.min_silence_ms,
            min_sentence_ms = vad_cfg.min_sentence_ms,
            soft_window_s = vad_cfg.soft_window_s,
            hard_window_s = vad_cfg.hard_window_s,
            max_uncommitted_s = vad_cfg.max_uncommitted_s,
            "伪流式 STT 引擎: VAD + GGUF worker 通道 (就绪)"
        );

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
                sentences: SentenceState::new(),
                samples: Vec::new(),
                last_preview: Instant::now(),
                last_preview_elapsed: Duration::ZERO,
                last_preview_sample_end: 0,
                preview_in_flight: false,
                preview_owner: 0,
                next_preview_request: 0,
                latest_preview: String::new(),
                preview_revision: 0,
                preview_generation: 0,
                state_revision: 0,
                last_reported_state: None,
                last_reported_confirmed_revision: 0,
                max_uncommitted_audio_ms: vad_cfg.max_uncommitted_ms(),
                session_failed: false,
            })),
            connection: Some(conn),
            sample_rate: 16000,
            boundary_observer: None,
            #[cfg(test)]
            finalize_observer: None,
        })
    }

    /// 给独立的 WAV 诊断回放挂载数值切点记录器。
    pub fn with_boundary_observer(mut self, observer: Arc<Mutex<Vec<SttBoundaryRecord>>>) -> Self {
        self.boundary_observer = Some(observer);
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
        let wav_bytes = super::wav::pcm_to_wav(&trimmed, self.sample_rate, 1);

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
        let off_threshold = {
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
            inner.vad.current_off_threshold()
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
                // 0.22.15：裁剪尾部静音——使用 VAD off_threshold 统一静音语义
                let trimmed =
                    trim_trailing_silence(&current_samples, sample_rate, current_threshold);
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
                if let Some(deferred) = inner.commit_or_rollback(&finalize_result) {
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
                    continue;
                }
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
    fn spawn_preview_recognition(&self, samples_snapshot: Vec<f32>, snapshot_end: usize) {
        if samples_snapshot.is_empty() {
            return;
        }

        // 登记所有权 + 捕获当前代际 + 获取 VAD off_threshold
        let (request_id, generation, off_threshold) = {
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
            (
                request_id,
                inner.preview_generation,
                inner.vad.current_off_threshold(),
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
                return;
            }

            match result {
                Ok(text) => {
                    let cleaned = strip_filler_words(&text);
                    // 代际校验：句尾后丢弃过期预览（防重复影子）
                    if inner.preview_generation == generation {
                        inner.set_preview_if_changed(cleaned);
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

            // 速率控制只在代际仍有效时推进（句尾已自行重置计时）
            if inner.preview_generation == generation {
                inner.last_preview = Instant::now();
                inner.last_preview_elapsed = elapsed;
                inner.last_preview_sample_end = snapshot_end;
            }
        });
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
        let (identity, remaining_samples, abs_end, off_threshold) = {
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
            let identity = inner.sentences.begin_terminal_finalize();
            inner.preview_generation = inner.preview_generation.wrapping_add(1);
            inner.preview_in_flight = false;
            // 原子撤销预览所有权：在途预览任务即使迟到也无法再写状态
            inner.preview_owner = 0;
            (identity, remaining_samples, abs_end, off_threshold)
        };

        if timed_out {
            tracing::warn!(
                session = identity.session_generation,
                commit_generation = identity.commit_generation,
                "finalize: 等待 in-flight 超时，已撤销旧任务提交权并接管尾段"
            );
        }

        let finalize_text = if remaining_samples.is_empty() {
            String::new()
        } else {
            match self
                .transcribe_samples(&remaining_samples, off_threshold)
                .await
            {
                Ok(text) => text,
                Err(e) => {
                    tracing::warn!(%e, "finalize 定稿识别失败，使用已有结果");
                    String::new()
                }
            }
        };

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
            }

            let mut result = inner.sentences.confirmed_text();
            if finalize_text.is_empty() && !inner.latest_preview.is_empty() {
                let preview = strip_confirmed_prefix(&result, &inner.latest_preview);
                if !preview.is_empty() {
                    result.push_str(&preview);
                }
            }
            result
        };

        tracing::info!(text_len = final_text.chars().count(), "伪流式识别完成");
        Ok(final_text)
    }
}

#[async_trait::async_trait]
impl SttEngine for PseudoStreamingSttEngine {
    async fn transcribe_chunk(&self, samples: &[f32]) -> Result<String, SttError> {
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
            self.spawn_preview_recognition(samples_snapshot, snapshot_end);
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
                g.preview_revision = 0;
                g.preview_generation = g.preview_generation.wrapping_add(1);
                g.state_revision = 0;
                g.last_reported_state = None;
                g.last_reported_confirmed_revision = 0;
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
        inner.preview_revision = 0;
        inner.preview_generation = inner.preview_generation.wrapping_add(1);
        inner.state_revision = 0;
        inner.last_reported_state = None;
        inner.last_reported_confirmed_revision = 0;
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
                ..SttStreamStats::default()
            },
            None => SttStreamStats::default(),
        }
    }
}

// ── 测试 ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests;
