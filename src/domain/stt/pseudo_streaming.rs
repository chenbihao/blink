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
//! 返回 JSON 字符串 `{"confirmed":"...","preview":"..."}`，
//! voice.rs 解析后分别 emit confirmed 和 preview。
//! 如果 confirmed 和 preview 都为空，返回空字符串（兼容现有逻辑）。
//!
//! ## 并发安全
//!
//! 使用 `Arc<std::sync::Mutex>` 保护内部状态。后台 HTTP task 通过 clone 的
//! `Arc` 在完成后短暂加锁写入结果。`transcribe_chunk` 是 async 但不跨 await
//! 持有 `std::sync::Mutex`（先 lock 取数据/写数据，再 drop guard，再 await）。

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::vad::{EnergyVad, VadEvent};
use super::{SttEngine, SttError};

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

/// VAD 状态异常或音量长期落在滞回区时的最终保险：未提交音频达到
/// 12 秒后仍强制切段。这个上限按绝对音频坐标计算，不依赖 speaking 状态。
const MAX_UNCOMMITTED_AUDIO_MS: u64 = 12_000;

/// finalize 等待 in_flight 请求的最大时间。
const FINALIZE_WAIT_TIMEOUT_MS: u64 = 3000;

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
}

/// 0.22.15：pending segment 的 identity——finalize task 返回时必须匹配。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SegmentIdentity {
    session_generation: u64,
    segment_id: u64,
}

/// 0.22.15：pending segment——句尾产生的候选，等待定稿结果决定 commit 或 rollback。
#[derive(Debug, Clone)]
struct PendingSegment {
    /// 此 segment 的 identity
    identity: SegmentIdentity,
    /// 候选音频范围 [start, end)
    range: std::ops::Range<usize>,
    /// 句尾时的 preview 快照（rollback 时恢复）
    preview_snapshot: String,
}

/// 0.22.15：finalize task 的返回结果——携带 identity，使写入方可以校验。
#[derive(Debug)]
struct FinalizeResult {
    identity: SegmentIdentity,
    text: String,
    /// 是否成功（false = 错误或超时）
    ok: bool,
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
    /// 最新预览文本
    latest_preview: String,
    /// 预览代际计数器（0.10.6 防重复影子）
    ///
    /// 每次 VAD 句尾时递增。`spawn_preview_recognition` 启动时捕获当前代际，
    /// 返回时校验：若代际不匹配（句尾已发生），说明此预览的音频跨越了句子边界，
    /// 包含已定稿句子的内容，直接丢弃避免覆盖 `latest_preview` 造成重复影子。
    preview_generation: u64,
    /// 0.22.15 follow-up: session 失败标志。
    ///
    /// 当内部不变量被破坏（如坐标非法）时设为 `true`。
    /// 设为 true 后，`transcribe_chunk` 和 `finalize` 返回 `SttError`，
    /// 不再处理新音频。`reset` 清除此标志。
    session_failed: bool,
}

impl PseudoInner {
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
                preview_in_flight = self.preview_in_flight,
                "STT session 进入失败态"
            );
        }
        self.session_failed = true;
    }
}

/// 0.22.15 事务化句子状态管理。
///
/// ## 样本坐标系统（0.22.15 follow-up: 统一绝对坐标）
///
/// 所有坐标一律使用**绝对样本坐标**（录音会话内从 0 开始的单调位置）。
///
/// - `committed_sample_end`：已 committed 的绝对末尾
/// - `buffer_base_sample`：`samples[0]` 在录音会话中的绝对位置
///   （compact 后推进到 `committed_sample_end`）
/// - 当前绝对尾端 = `buffer_base_sample.checked_add(samples.len())`
///
/// 局部切片通过 [`SentenceState::abs_to_local_range`] 统一转换，禁止手工减法。
struct SentenceState {
    /// 已定稿的句子列表
    confirmed_sentences: Vec<String>,
    /// 已 committed 的音频末尾（绝对坐标，下一句的起始）
    committed_sample_end: usize,
    /// 单调 session generation（每次 reset 递增）
    session_generation: u64,
    /// 单调 segment id（每个句尾递增）
    next_segment_id: u64,
    /// 当前 pending segment（如果有）
    pending: Option<PendingSegment>,
    /// 有 finalize task 在飞行中
    finalize_in_flight: bool,
    /// 0.22.15 fix: pending 期间再次出现句尾时排队的 deferred segment
    /// （finalize_in_flight 为 true 时，新句尾暂存于此，finalize 完成后再处理）
    deferred: Option<PendingSegment>,
    /// `samples[0]` 在录音会话中的绝对位置。
    ///
    /// compact 后推进到 `committed_sample_end`。
    /// 初始为 0，compact 时 `drain_count = committed_sample_end - buffer_base_sample`，
    /// drain 后 `buffer_base_sample = committed_sample_end`。
    buffer_base_sample: usize,
}

impl SentenceState {
    fn new() -> Self {
        Self {
            confirmed_sentences: Vec::new(),
            committed_sample_end: 0,
            session_generation: 1,
            next_segment_id: 1,
            pending: None,
            finalize_in_flight: false,
            deferred: None,
            buffer_base_sample: 0,
        }
    }

    /// 将绝对 range 转换为相对于 `samples` 的局部 range。
    ///
    /// 校验：`start >= buffer_base`、`end >= start`、
    /// `end <= buffer_base + samples_len`。
    /// 非法 range 返回 `None`（调用方应安全终止，不 panic）。
    ///
    /// **所有 `samples` 切片必须通过此 helper，禁止手工减法。**
    fn abs_to_local_range(
        &self,
        abs_range: &std::ops::Range<usize>,
        samples_len: usize,
    ) -> Option<std::ops::Range<usize>> {
        let base = self.buffer_base_sample;
        if abs_range.start < base {
            tracing::error!(
                abs_start = abs_range.start,
                buffer_base = base,
                "坐标错误：abs_start < buffer_base"
            );
            return None;
        }
        if abs_range.end < abs_range.start {
            tracing::error!(
                abs_start = abs_range.start,
                abs_end = abs_range.end,
                "坐标错误：abs_end < abs_start"
            );
            return None;
        }
        let local_start = abs_range.start - base;
        let local_end = abs_range.end - base;
        if local_end > samples_len {
            tracing::error!(
                local_end = local_end,
                samples_len = samples_len,
                "坐标错误：local_end > samples_len"
            );
            return None;
        }
        Some(local_start..local_end)
    }

    /// 句尾：创建 pending segment，不推进 committed end。
    ///
    /// `total_samples` 是**绝对**坐标（`buffer_base_sample + samples.len()`）。
    /// pending range `[committed_sample_end, total_samples)` 也是绝对坐标。
    ///
    /// 调用方随后 spawn finalize task，结果通过 `commit_or_rollback` 写入。
    ///
    /// 0.22.15 fix: 如果 finalize_in_flight 为 true（上一个句尾的定稿还在飞行中），
    /// 新句尾暂存到 deferred，等前一个 finalize 完成后再处理。
    /// 返回 None 表示暂存了，调用方不应 spawn 新 finalize task。
    fn on_sentence_end(
        &mut self,
        total_samples: usize,
        preview_snapshot: &str,
    ) -> Option<PendingSegment> {
        let segment_id = self.next_segment_id;
        self.next_segment_id += 1;

        // 绝对坐标 range：[committed_sample_end, total_samples)
        let start = self.committed_sample_end;
        let end = total_samples;
        let range = start..end;
        let identity = SegmentIdentity {
            session_generation: self.session_generation,
            segment_id,
        };
        let pending = PendingSegment {
            identity,
            range: range.clone(),
            preview_snapshot: preview_snapshot.to_string(),
        };

        if self.finalize_in_flight {
            // 上一个 finalize 还在飞行中，暂存
            tracing::debug!(
                seg = segment_id,
                "句尾时 finalize_in_flight，暂存到 deferred"
            );
            self.deferred = Some(pending);
            return None;
        }

        self.pending = Some(pending.clone());
        Some(pending)
    }

    /// 0.22.15：commit 或 rollback 一个 finalize 结果。
    ///
    /// - identity 不匹配 → 丢弃（旧 session 的迟到结果）
    /// - 非空结果 → commit：追加 confirmed、推进 committed end、清空对应 preview
    /// - 空/错误 → rollback：committed end 不变、保留 preview 快照
    ///
    /// 返回值：
    /// - `Some(pending)` = commit/rollback 后有 deferred segment 需要处理
    /// - `None` = 已处理，无 deferred
    /// - （被丢弃的 stale result 也返回 None）
    fn commit_or_rollback(&mut self, result: &FinalizeResult) -> Option<PendingSegment> {
        // identity 校验
        let pending = match &self.pending {
            Some(p) if p.identity == result.identity => p,
            Some(p) => {
                tracing::debug!(
                    pending_seg = p.identity.segment_id,
                    result_seg = result.identity.segment_id,
                    "丢弃 identity 不匹配的 finalize 结果"
                );
                return None;
            }
            None => {
                tracing::debug!("无 pending segment，丢弃 finalize 结果");
                return None;
            }
        };

        let range = pending.range.clone();
        let preview_snapshot = &pending.preview_snapshot;

        if result.ok && !result.text.is_empty() {
            // ── commit ──
            // range.end 是绝对坐标，直接推进 committed_sample_end
            self.confirmed_sentences.push(result.text.clone());
            self.committed_sample_end = range.end;
            tracing::debug!(
                seg = result.identity.segment_id,
                text_len = result.text.chars().count(),
                range_end = range.end,
                "commit pending segment"
            );
        } else {
            // ── rollback ──
            // committed end 不变，preview 快照保留在 pending 中供 finalize() 兜底
            tracing::debug!(
                seg = result.identity.segment_id,
                ok = result.ok,
                text_empty = result.text.is_empty(),
                preview_len = preview_snapshot.chars().count(),
                "rollback pending segment"
            );
        }

        self.pending = None;
        self.finalize_in_flight = false;

        // 0.22.15 fix: 如果有 deferred segment，返回它让调用方 spawn 新 finalize
        let deferred = self.deferred.take();
        if let Some(ref d) = deferred {
            self.pending = Some(d.clone());
            self.finalize_in_flight = true;
        }
        deferred
    }

    /// 0.22.15 fix: 尝试回收已 committed 的 PCM 样本。
    ///
    /// 条件：无 pending 和 deferred（没有飞行中的 finalize 引用旧音频）。
    /// 返回需要丢弃的前缀样本数。调用方据此 `samples.drain(..n)`，
    /// 本方法同时推进 `buffer_base_sample` 到 `committed_sample_end`。
    fn try_compact(&mut self, samples_len: usize) -> Result<Option<usize>, &'static str> {
        if self.pending.is_some() || self.deferred.is_some() {
            return Ok(None);
        }
        if self.buffer_base_sample >= self.committed_sample_end {
            return Ok(None);
        }
        let buffer_end = self
            .buffer_base_sample
            .checked_add(samples_len)
            .ok_or("compact buffer end 溢出")?;
        if self.committed_sample_end > buffer_end {
            return Err("committed end 超出当前 PCM buffer");
        }
        let to_compact = self
            .committed_sample_end
            .checked_sub(self.buffer_base_sample)
            .ok_or("compact base 超过 committed end")?;
        self.buffer_base_sample = self.committed_sample_end;
        if to_compact > 0 {
            tracing::debug!(
                compacted = to_compact,
                buffer_base = self.buffer_base_sample,
                total_committed = self.committed_sample_end,
                "compact 已 committed PCM"
            );
            Ok(Some(to_compact))
        } else {
            Ok(None)
        }
    }

    /// 追加一句定稿文本（测试辅助）。
    #[cfg(test)]
    fn append_confirmed(&mut self, text: &str) {
        if !text.is_empty() {
            self.confirmed_sentences.push(text.to_string());
        }
    }

    /// 获取已确认部分的文本。
    fn confirmed_text(&self) -> String {
        self.confirmed_sentences.join("")
    }

    /// reset：递增 session_generation，清空所有状态。
    fn reset(&mut self) {
        self.confirmed_sentences.clear();
        self.committed_sample_end = 0;
        self.session_generation = self.session_generation.wrapping_add(1);
        self.next_segment_id = 1;
        self.pending = None;
        self.deferred = None;
        self.finalize_in_flight = false;
        self.buffer_base_sample = 0;
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
            "伪流式 STT 引擎: VAD + GGUF worker 通道 (就绪)"
        );

        Ok(Self {
            inner: Arc::new(Mutex::new(PseudoInner {
                vad: EnergyVad::with_params(
                    16000,
                    vad_cfg.silence_threshold,
                    vad_cfg.min_silence_ms,
                    vad_cfg.min_sentence_ms,
                ),
                sentences: SentenceState::new(),
                samples: Vec::new(),
                last_preview: Instant::now(),
                last_preview_elapsed: Duration::ZERO,
                last_preview_sample_end: 0,
                preview_in_flight: false,
                latest_preview: String::new(),
                preview_generation: 0,
                session_failed: false,
            })),
            connection: Some(conn),
            sample_rate: 16000,
        })
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

    fn exceeds_uncommitted_hard_limit(
        total: usize,
        committed_end: usize,
        sample_rate: u32,
    ) -> Option<bool> {
        let max_samples = (MAX_UNCOMMITTED_AUDIO_MS * sample_rate as u64 / 1000) as usize;
        total
            .checked_sub(committed_end)
            .map(|uncommitted| uncommitted >= max_samples)
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
    fn compose_result(confirmed: &str, preview: &str) -> String {
        if confirmed.is_empty() && preview.is_empty() {
            return String::new();
        }
        serde_json::json!({
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
            .map_err(SttError::Engine)?;

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
            if let Some(deferred) = inner.sentences.commit_or_rollback(&result) {
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
            if let Some(deferred) = inner.sentences.commit_or_rollback(&result) {
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
                if let Some(deferred) = inner.sentences.commit_or_rollback(&finalize_result) {
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
    fn spawn_preview_recognition(&self, samples_snapshot: Vec<f32>, snapshot_end: usize) {
        if samples_snapshot.is_empty() {
            return;
        }

        // 标记 in_flight + 捕获当前代际 + 获取 VAD off_threshold
        let (generation, off_threshold) = {
            let mut inner = match Self::try_lock(&self.inner) {
                Some(g) => g,
                None => {
                    tracing::error!("Mutex poisoned at preview in_flight mark");
                    return;
                }
            };
            inner.preview_in_flight = true;
            (inner.preview_generation, inner.vad.current_off_threshold())
        };

        let inner = Arc::clone(&self.inner);
        let Some(transport) = self.connection.as_ref().and_then(|c| c.transport.clone()) else {
            tracing::warn!("预览识别缺少 worker 通道，跳过");
            if let Some(mut g) = Self::try_lock(&self.inner) {
                g.preview_in_flight = false;
            }
            return;
        };
        let sample_rate = self.sample_rate;

        tokio::spawn(async move {
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
            match result {
                Ok(text) => {
                    let cleaned = strip_filler_words(&text);
                    if !cleaned.is_empty() {
                        tracing::trace!(
                            text_len = cleaned.chars().count(),
                            modified = cleaned != text,
                            "预览识别"
                        );
                        // 写入 latest_preview（代际校验：句尾后丢弃过期预览）
                        // 0.22.15 follow-up: poison 时只清除 in_flight 不 panic
                        if inner.preview_generation == generation {
                            inner.latest_preview = cleaned;
                        } else {
                            tracing::debug!(
                                gen = generation,
                                cur_gen = inner.preview_generation,
                                "丢弃过期预览（句尾已发生）"
                            );
                        }
                    }
                }
                Err(e) => {
                    tracing::trace!(%e, "预览识别失败（非致命）");
                }
            }

            inner.preview_in_flight = false;
            if inner.preview_generation == generation {
                inner.last_preview = Instant::now();
                inner.last_preview_elapsed = elapsed;
                inner.last_preview_sample_end = snapshot_end;
            }
        });
    }
}

/// 从预览文本中剥离已确认的前缀部分。
///
/// 预览识别只取未确认音频，但模型仍可能因句子边界切分不完全
/// 而在 preview 开头重复部分 confirmed 文本。此函数做兜底清理：
///
/// 1. 精确前缀匹配 → 直接剥离
/// 2. 逐字符匹配 → 剥离匹配部分（应对标点差异等）
/// 3. 无匹配 → 原样返回
///
/// # 算法
///
/// 逐字符从开头比较 confirmed 和 preview，遇到第一个不匹配的字符停止。
/// 匹配长度 ≥ confirmed 长度的 50% 时才剥离（避免误剥离短公共前缀如"我"）。
fn strip_confirmed_prefix(confirmed: &str, preview: &str) -> String {
    if confirmed.is_empty() || preview.is_empty() {
        return preview.to_string();
    }

    // 1. 精确前缀匹配
    if let Some(stripped) = preview.strip_prefix(confirmed) {
        return stripped.to_string();
    }

    // 2. 逐字符匹配（应对标点差异）
    let confirmed_chars: Vec<char> = confirmed.chars().collect();
    let preview_chars: Vec<char> = preview.chars().collect();

    let mut match_len = 0;
    for (c, p) in confirmed_chars.iter().zip(preview_chars.iter()) {
        if c == p {
            match_len += 1;
        } else {
            break;
        }
    }

    // 匹配长度需达到 confirmed 的 50% 才剥离
    // 避免短公共前缀（如 "我"）导致误剥离
    if match_len > 0 && match_len * 2 >= confirmed_chars.len() {
        preview_chars[match_len..].iter().collect()
    } else {
        preview.to_string()
    }
}

/// 0.22.15：裁剪后保留的尾部缓冲（毫秒），避免切掉软辅音尾音。
const TRIM_TAIL_BUFFER_MS: u32 = 150;

/// 0.22.15：裁剪的最低阈值下界——即使 VAD off_threshold 很低，
/// 裁剪也不会低于此值，防止把极低振幅的环境噪声当成有声。
const TRIM_THRESHOLD_FLOOR: f64 = 0.0005;

/// 0.22.15：裁剪音频尾部的静音/低能量段——统一使用 VAD 的 off_threshold。
///
/// SenseVoice 等多语言模型在尾部静音上容易幻觉出英文语气词
///（如 "Yeah." "Okay."）。裁剪尾部静音可大幅减少此问题。
///
/// # 统一静音语义
///
/// 裁剪阈值取 `vad_off_threshold`（从 `EnergyVad::current_off_threshold()` 获取），
/// 不再用固定常量——确保"什么算静音"在 VAD 和裁剪之间一致。
/// `vad_off_threshold` 随 `noise_floor` 自适应变化。
///
/// # 全静音处理
///
/// 如果整段音频无任何样本超过阈值（全静音/no-speech），
/// 返回空 Vec——不把全静音送入 SenseVoice，避免诱发幻觉。
///
/// 算法：从末尾向前扫描，找到最后一个超过阈值的样本，
/// 保留该位置 + `TRIM_TAIL_BUFFER_MS` 缓冲后的部分。
fn trim_trailing_silence(samples: &[f32], sample_rate: u32, vad_off_threshold: f64) -> Vec<f32> {
    if samples.is_empty() {
        return Vec::new();
    }

    // 取 VAD off_threshold 与下界的较大值
    let threshold = (vad_off_threshold.max(TRIM_THRESHOLD_FLOOR)) as f32;

    // 从末尾向前找最后一个有声样本
    let mut last_audible = None;
    for (i, &s) in samples.iter().enumerate().rev() {
        if s.abs() > threshold {
            last_audible = Some(i);
            break;
        }
    }

    match last_audible {
        None => {
            // 0.22.15：全静音 → 返回空 Vec，不送入 SenseVoice
            Vec::new()
        }
        Some(idx) => {
            let buffer_samples = (TRIM_TAIL_BUFFER_MS as u64 * sample_rate as u64 / 1000) as usize;
            let end = (idx + 1 + buffer_samples).min(samples.len());
            samples[..end].to_vec()
        }
    }
}

/// SenseVoice 常见英文语气词幻觉。
///
/// 这些词在中文语音识别中不应出现，是多语言模型在静音段上的已知幻觉。
const FILLER_WORDS: &[&str] = &[
    "Yeah", "yeah", "Okay", "okay", "OK", "ok", "Mm", "mm", "Hmm", "hmm", "Uh", "uh", "Oh", "oh",
    "Ah", "ah", "Um", "um", "No", "no", "Yes", "yes", "Well", "well", "So", "so", "Right", "right",
    "Like", "like", "But", "but", "And", "and",
];

/// 判断字符是否为中文。
fn is_chinese_char(c: char) -> bool {
    matches!(c, '\u{4e00}'..='\u{9fff}' | '\u{3400}'..='\u{4dbf}')
}

/// 剥离 SenseVoice 幻觉产生的尾部英文语气词。
///
/// 当识别文本以中文为主时，模型可能在尾部静音段幻觉出
/// 英文填充词（如 "Yeah." "Okay."）。此函数做后处理清理。
///
/// emoji 和 CJK 间空格已由 Python server `_postprocess_text` 处理，
/// 此处不再重复。
///
/// 仅当文本包含中文字符时才执行剥离，避免误伤纯英文识别。
fn strip_filler_words(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return trimmed.to_string();
    }

    // 检查是否包含中文字符
    let has_chinese = trimmed.chars().any(is_chinese_char);
    if !has_chinese {
        return trimmed.to_string();
    }

    let mut result = trimmed.to_string();

    // 循环剥离尾部语气词（可能多个连续出现）
    loop {
        let stripped = strip_one_filler_suffix(&result);
        if stripped.len() == result.len() {
            break;
        }
        result = stripped;
    }

    // 清理尾部残留的空格和标点
    result.trim_end().to_string()
}

/// 尝试从文本末尾剥离一个英文语气词后缀。
/// 返回剥离后的文本；如果没有匹配则原样返回。
fn strip_one_filler_suffix(text: &str) -> String {
    let lower = text.to_lowercase();

    for &filler in FILLER_WORDS {
        let filler_lower = filler.to_lowercase();

        // 模式 1: "...中文 Yeah." → 匹配 " Yeah." / " Yeah," 等
        // 前面是空格或中文标点
        for &suffix in &[".", ",", "!", "?", ""] {
            let pattern = format!(" {}{}", filler_lower, suffix);
            if lower.ends_with(&pattern) {
                let cut = text.len() - pattern.len();
                return text[..cut].to_string();
            }
        }

        // 模式 2: "...中文Yeah." → 无空格直接拼接（较少见但存在）
        // 仅当 filler 前面是中文字符或中文标点时才匹配
        for &suffix in &[".", ",", "!", "?"] {
            let pattern = format!("{}{}", filler_lower, suffix);
            if lower.ends_with(&pattern) {
                let cut = text.len() - pattern.len();
                if cut > 0 {
                    let prev_char = text[..cut].chars().next_back();
                    if let Some(pc) = prev_char {
                        // 非 ASCII 字符 = 中文（汉字或标点）
                        if !pc.is_ascii() {
                            return text[..cut].to_string();
                        }
                    }
                }
            }
        }
    }

    text.to_string()
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

            // VAD 的 speaking 状态不是内存/负载边界：真实麦克风输入可能长期
            // 落在 on/off 滞回区，导致 VAD 计时不前进。按绝对未提交音频长度
            // 再做一次硬限制，保证送入模型的窗口不会无限增长。
            if !event.is_boundary()
                && inner.sentences.pending.is_none()
                && !inner.sentences.finalize_in_flight
            {
                match Self::exceeds_uncommitted_hard_limit(
                    total,
                    inner.sentences.committed_sample_end,
                    self.sample_rate,
                ) {
                    Some(true) => event = VadEvent::HardWindow,
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
                tracing::debug!(reason = event.reason(), total, "STT segment boundary");
                inner.sentences.on_sentence_end(total, &preview_snapshot)
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
                inner.latest_preview.clear();
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

            // VAD 句尾后重置句子计数
            if let Some(mut g) = Self::try_lock(&self.inner) {
                g.vad.reset_sentence();
            }
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

        // ── 4. 组装返回 ──
        // strip_confirmed_prefix 兜底：即使预览只取了未确认音频，
        // 模型仍可能因为句子边界切分不完全而产生部分重叠文本
        let (confirmed, preview) = {
            let inner = match Self::try_lock(&self.inner) {
                Some(g) => g,
                None => {
                    tracing::error!("Mutex poisoned at result compose");
                    return Err(SttError::Engine(
                        "STT session 已损坏 (Mutex poisoned)".to_string(),
                    ));
                }
            };
            let confirmed = inner.sentences.confirmed_text();
            let preview = strip_confirmed_prefix(&confirmed, &inner.latest_preview);
            (confirmed, preview)
        };

        Ok(Self::compose_result(&confirmed, &preview))
    }

    async fn finalize(&self) -> Result<String, SttError> {
        // 1. 先等待 in_flight 预览/定稿请求完成（最多 3s）
        //
        // **必须在发送新的 transcribe 请求之前等待**——否则 worker 在处理
        // in-flight 请求时又收到新请求，可能导致内存访问竞争（0xC0000005）。
        // NdjsonWorkerClient 虽有请求锁串行化，但 worker 进程侧的推理线程
        // 可能在处理上一个请求的清理路径时被新请求打断，触发访问违例。
        let deadline = Instant::now() + Duration::from_millis(FINALIZE_WAIT_TIMEOUT_MS);
        loop {
            let (preview_in_flight, finalize_in_flight) = {
                let inner = match Self::try_lock(&self.inner) {
                    Some(g) => g,
                    None => {
                        tracing::error!("Mutex poisoned at finalize wait loop");
                        return Err(SttError::Engine(
                            "STT session 已损坏 (Mutex poisoned)".to_string(),
                        ));
                    }
                };
                (inner.preview_in_flight, inner.sentences.finalize_in_flight)
            };

            if !preview_in_flight && !finalize_in_flight {
                break;
            }
            if Instant::now() >= deadline {
                tracing::warn!("finalize: 等待 in_flight 请求超时，使用已有结果");
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        // 2. 0.22.15：pending segment 的 commit/rollback 已在后台 task 中由
        // commit_or_rollback 处理完毕。rollback 后 committed_sample_end 不变，
        // remaining samples 会包含回退的音频，在第 3 步中被最终识别。

        // 3. 定稿剩余音频（此时 in-flight 请求已全部完成，安全发新请求）
        let (remaining_samples, off_threshold) = {
            let inner = match Self::try_lock(&self.inner) {
                Some(g) => g,
                None => {
                    tracing::error!("Mutex poisoned at finalize remaining samples");
                    return Err(SttError::Engine(
                        "STT session 已损坏 (Mutex poisoned)".to_string(),
                    ));
                }
            };
            // 绝对 range → 局部切片
            let abs_start = inner.sentences.committed_sample_end;
            let abs_end = match inner
                .sentences
                .buffer_base_sample
                .checked_add(inner.samples.len())
            {
                Some(end) => end,
                None => return Err(SttError::Engine("STT 音频坐标溢出".to_string())),
            };
            let abs_range = abs_start..abs_end;
            let samples = match inner
                .sentences
                .abs_to_local_range(&abs_range, inner.samples.len())
            {
                Some(local_range) => inner.samples[local_range].to_vec(),
                None => {
                    tracing::warn!(
                        committed_end = abs_start,
                        total = abs_end,
                        buffer_base = inner.sentences.buffer_base_sample,
                        samples_len = inner.samples.len(),
                        "finalize 坐标非法，使用空剩余"
                    );
                    Vec::new()
                }
            };
            (samples, inner.vad.current_off_threshold())
        };

        let finalize_text = if !remaining_samples.is_empty() {
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
        } else {
            String::new()
        };

        // 4. 拼接 confirmed + finalize_text + 最后一段 preview
        // 0.22.15 fix: preview 兜底——即使已有 confirmed，如果 finalize_text 为空，
        // 仍用 latest_preview 作为尾段兜底（做好 confirmed prefix 去重）
        let final_text = {
            let inner = match Self::try_lock(&self.inner) {
                Some(g) => g,
                None => {
                    tracing::error!("Mutex poisoned at finalize text compose");
                    return Err(SttError::Engine(
                        "STT session 已损坏 (Mutex poisoned)".to_string(),
                    ));
                }
            };
            let mut result = inner.sentences.confirmed_text();
            if !finalize_text.is_empty() {
                result.push_str(&finalize_text);
            }
            // 如果 finalize 没有识别到文本，用最后一段 preview 兜底
            // 0.22.15 fix: 即使 result 非空（已有 confirmed），
            // 如果 finalize_text 为空且 preview 存在，仍用 preview 补尾段
            if finalize_text.is_empty() && !inner.latest_preview.is_empty() {
                let preview = strip_confirmed_prefix(&result, &inner.latest_preview);
                if !preview.is_empty() {
                    result.push_str(&preview);
                }
            }
            // 全空的兜底：只有 result 和 preview 都空时才用 preview
            if result.is_empty() && !inner.latest_preview.is_empty() {
                result = inner.latest_preview.clone();
            }
            result
        };

        tracing::info!(text_len = final_text.chars().count(), "伪流式识别完成",);

        Ok(final_text)
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
                g.latest_preview.clear();
                g.preview_generation = g.preview_generation.wrapping_add(1);
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
        inner.latest_preview.clear();
        inner.preview_generation = inner.preview_generation.wrapping_add(1);
        inner.session_failed = false;
        tracing::debug!("伪流式引擎 reset");
    }

    fn name(&self) -> &str {
        "pseudo-streaming"
    }
}

// ── 测试 ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── SentenceState 基础测试 ──

    #[test]
    fn sentence_state_compose() {
        let mut state = SentenceState::new();
        state.append_confirmed("你好世界。");
        state.append_confirmed("今天天气不错。");
        assert_eq!(state.confirmed_text(), "你好世界。今天天气不错。");
    }

    #[test]
    fn sentence_state_empty() {
        let state = SentenceState::new();
        assert_eq!(state.confirmed_text(), "");
    }

    #[test]
    fn sentence_state_on_sentence_end_creates_pending() {
        let mut state = SentenceState::new();
        let pending = state
            .on_sentence_end(1000, "预览快照")
            .expect("应有 pending");
        assert_eq!(pending.range, 0..1000);
        assert_eq!(pending.preview_snapshot, "预览快照");
        // committed end 不应推进
        assert_eq!(state.committed_sample_end, 0);
        assert!(state.pending.is_some());
    }

    #[test]
    fn sentence_state_commit_advances_committed_end() {
        let mut state = SentenceState::new();
        let pending = state.on_sentence_end(1000, "").expect("应有 pending");
        let result = FinalizeResult {
            identity: pending.identity,
            text: "你好".to_string(),
            ok: true,
        };
        let deferred = state.commit_or_rollback(&result);
        assert!(deferred.is_none(), "无 deferred");
        assert_eq!(state.committed_sample_end, 1000);
        assert_eq!(state.confirmed_text(), "你好");
        assert!(state.pending.is_none());
    }

    #[test]
    fn sentence_state_rollback_keeps_committed_end() {
        let mut state = SentenceState::new();
        let pending = state
            .on_sentence_end(1000, "预览快照")
            .expect("应有 pending");
        let result = FinalizeResult {
            identity: pending.identity,
            text: String::new(),
            ok: false,
        };
        let deferred = state.commit_or_rollback(&result);
        assert!(deferred.is_none());
        // committed end 不变
        assert_eq!(state.committed_sample_end, 0);
        assert_eq!(state.confirmed_text(), "");
        assert!(state.pending.is_none());
        // finalize_in_flight 应清除
        assert!(!state.finalize_in_flight);
    }

    #[test]
    fn sentence_state_stale_identity_discarded() {
        let mut state = SentenceState::new();
        // 创建一个 pending
        let pending = state.on_sentence_end(1000, "").expect("应有 pending");
        // 模拟旧 session 的结果（segment_id 不匹配）
        let stale_result = FinalizeResult {
            identity: SegmentIdentity {
                session_generation: pending.identity.session_generation,
                segment_id: pending.identity.segment_id + 999, // 不匹配
            },
            text: "过期结果".to_string(),
            ok: true,
        };
        let deferred = state.commit_or_rollback(&stale_result);
        assert!(deferred.is_none(), "stale identity 应被丢弃");
        // pending 应仍然存在
        assert!(state.pending.is_some());
        assert_eq!(state.committed_sample_end, 0);
    }

    #[test]
    fn sentence_state_wrong_session_discarded() {
        let mut state = SentenceState::new();
        let pending = state.on_sentence_end(1000, "").expect("应有 pending");
        // 模拟旧 session 的结果
        let stale_result = FinalizeResult {
            identity: SegmentIdentity {
                session_generation: pending.identity.session_generation + 1,
                segment_id: pending.identity.segment_id,
            },
            text: "旧session结果".to_string(),
            ok: true,
        };
        let deferred = state.commit_or_rollback(&stale_result);
        assert!(deferred.is_none(), "旧 session 结果应被丢弃");
        assert!(state.pending.is_some());
    }

    #[test]
    fn sentence_state_reset_clears_everything() {
        let mut state = SentenceState::new();
        state.append_confirmed("测试");
        state.committed_sample_end = 500;
        state.on_sentence_end(1000, "");
        state.finalize_in_flight = true;
        let old_session = state.session_generation;

        state.reset();

        assert_eq!(state.confirmed_text(), "");
        assert_eq!(state.committed_sample_end, 0);
        assert!(state.pending.is_none());
        assert!(state.deferred.is_none());
        assert!(!state.finalize_in_flight);
        assert_eq!(state.buffer_base_sample, 0);
        assert_ne!(state.session_generation, old_session);
        assert_eq!(state.next_segment_id, 1);
    }

    #[test]
    fn sentence_state_multiple_sentences_commit_in_order() {
        let mut state = SentenceState::new();
        // 第一句
        let p1 = state.on_sentence_end(1000, "").expect("应有 pending");
        // commit 第一句
        let r1 = FinalizeResult {
            identity: p1.identity,
            text: "第一句。".to_string(),
            ok: true,
        };
        assert!(state.commit_or_rollback(&r1).is_none());
        assert_eq!(state.committed_sample_end, 1000);
        assert_eq!(state.confirmed_text(), "第一句。");

        // 第二句
        let p2 = state.on_sentence_end(2500, "").expect("应有 pending");
        assert_eq!(p2.range, 1000..2500);
        let r2 = FinalizeResult {
            identity: p2.identity,
            text: "第二句。".to_string(),
            ok: true,
        };
        assert!(state.commit_or_rollback(&r2).is_none());
        assert_eq!(state.committed_sample_end, 2500);
        assert_eq!(state.confirmed_text(), "第一句。第二句。");
    }

    #[test]
    fn sentence_state_commit_empty_text_rollback() {
        let mut state = SentenceState::new();
        let pending = state.on_sentence_end(1000, "").expect("应有 pending");
        // ok=true 但 text 为空 → rollback
        let result = FinalizeResult {
            identity: pending.identity,
            text: String::new(),
            ok: true,
        };
        assert!(state.commit_or_rollback(&result).is_none());
        assert_eq!(state.committed_sample_end, 0);
        assert_eq!(state.confirmed_text(), "");
    }

    // ── compose_result 测试 ──

    #[test]
    fn compose_result_empty_returns_empty_string() {
        assert_eq!(PseudoStreamingSttEngine::compose_result("", ""), "");
    }

    #[test]
    fn compose_result_with_preview_only() {
        let result = PseudoStreamingSttEngine::compose_result("", "你好");
        let v: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(v["confirmed"], "");
        assert_eq!(v["preview"], "你好");
    }

    #[test]
    fn compose_result_with_both() {
        let result = PseudoStreamingSttEngine::compose_result("你好。", "世界");
        let v: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(v["confirmed"], "你好。");
        assert_eq!(v["preview"], "世界");
    }

    // ── preview_interval 测试 ──

    #[test]
    fn preview_interval_normal() {
        let interval = PseudoStreamingSttEngine::preview_interval(16000 * 3, 16000, Duration::ZERO);
        assert_eq!(interval, Duration::from_millis(PREVIEW_INTERVAL_MS));
    }

    #[test]
    fn preview_interval_slowdown() {
        let interval =
            PseudoStreamingSttEngine::preview_interval(16000 * 10, 16000, Duration::ZERO);
        assert_eq!(interval, Duration::from_millis(PREVIEW_SLOW_INTERVAL_MS));
    }

    #[test]
    fn preview_interval_adapts_to_slow_inference_with_cap() {
        let adaptive = PseudoStreamingSttEngine::preview_interval(
            16000 * 3,
            16000,
            Duration::from_millis(800),
        );
        assert_eq!(adaptive, Duration::from_millis(1600));

        let capped =
            PseudoStreamingSttEngine::preview_interval(16000 * 3, 16000, Duration::from_secs(10));
        assert_eq!(capped, Duration::from_millis(PREVIEW_MAX_INTERVAL_MS));
    }

    #[test]
    fn absolute_uncommitted_hard_limit_does_not_depend_on_vad_state() {
        assert_eq!(
            PseudoStreamingSttEngine::exceeds_uncommitted_hard_limit(16_000 * 12, 0, 16_000),
            Some(true)
        );
        assert_eq!(
            PseudoStreamingSttEngine::exceeds_uncommitted_hard_limit(
                16_000 * 20,
                16_000 * 9,
                16_000
            ),
            Some(false)
        );
        assert_eq!(
            PseudoStreamingSttEngine::exceeds_uncommitted_hard_limit(10, 11, 16_000),
            None
        );
    }

    #[test]
    fn preview_growth_requires_500ms_and_rejects_backward_end() {
        assert_eq!(
            PseudoStreamingSttEngine::has_min_preview_growth(8_000, 0, 16_000),
            Some(true)
        );
        assert_eq!(
            PseudoStreamingSttEngine::has_min_preview_growth(7_999, 0, 16_000),
            Some(false)
        );
        assert_eq!(
            PseudoStreamingSttEngine::has_min_preview_growth(100, 101, 16_000),
            None
        );
    }

    // ── strip_confirmed_prefix 测试 ──

    #[test]
    fn strip_prefix_exact_match() {
        assert_eq!(
            strip_confirmed_prefix("你好世界。", "你好世界。今天天气"),
            "今天天气"
        );
    }

    #[test]
    fn strip_prefix_no_confirmed() {
        assert_eq!(strip_confirmed_prefix("", "你好"), "你好");
    }

    #[test]
    fn strip_prefix_no_preview() {
        assert_eq!(strip_confirmed_prefix("你好", ""), "");
    }

    #[test]
    fn strip_prefix_no_overlap() {
        assert_eq!(
            strip_confirmed_prefix("你好世界。", "今天天气不错"),
            "今天天气不错"
        );
    }

    #[test]
    fn strip_prefix_partial_match() {
        assert_eq!(
            strip_confirmed_prefix("你好世", "你好时间今天天气"),
            "时间今天天气"
        );
    }

    #[test]
    fn strip_prefix_short_common_prefix_not_stripped() {
        assert_eq!(
            strip_confirmed_prefix("你好世界今天", "你好朋友"),
            "你好朋友"
        );
    }

    #[test]
    fn strip_prefix_preview_equals_confirmed() {
        assert_eq!(strip_confirmed_prefix("你好世界。", "你好世界。"), "");
    }

    // ── trim_trailing_silence 测试 ──

    #[test]
    fn trim_silence_all_silence() {
        // 0.22.15：全静音 → 返回空 Vec（不送入 SenseVoice 避免幻觉）
        let samples = vec![0.0f32; 1600];
        let trimmed = trim_trailing_silence(&samples, 16000, 0.003);
        assert!(trimmed.is_empty(), "全静音应返回空 Vec");
    }

    #[test]
    fn trim_silence_empty() {
        let trimmed = trim_trailing_silence(&[], 16000, 0.003);
        assert!(trimmed.is_empty());
    }

    #[test]
    fn trim_silence_trims_trailing_zeros() {
        // 有声 50ms + 静音 1s → 裁剪后保留有声 + 150ms 缓冲
        let mut samples = vec![0.1f32; 800]; // 有声 50ms
        samples.extend(vec![0.0f32; 16000]); // 静音 1s
        let trimmed = trim_trailing_silence(&samples, 16000, 0.003);
        // 最后有声样本在 index 799，缓冲 = 150ms * 16000 / 1000 = 2400
        // end = min(800 + 2400, 16800) = 3200
        assert_eq!(trimmed.len(), 3200);
    }

    #[test]
    fn trim_silence_no_trailing_silence() {
        let samples = vec![0.1f32; 1600];
        let trimmed = trim_trailing_silence(&samples, 16000, 0.003);
        assert_eq!(trimmed.len(), 1600);
    }

    // ── strip_filler_words 测试 ──

    #[test]
    fn filler_strip_yeah_period() {
        assert_eq!(
            strip_filler_words("我现在在做一个语音输入的。Yeah."),
            "我现在在做一个语音输入的。"
        );
    }

    #[test]
    fn filler_strip_okay_period() {
        assert_eq!(
            strip_filler_words("然后有一个假的流逝输入。Okay."),
            "然后有一个假的流逝输入。"
        );
    }

    #[test]
    fn filler_strip_multiple_fillers() {
        assert_eq!(strip_filler_words("你好世界。Yeah. Okay."), "你好世界。");
    }

    #[test]
    fn filler_strip_no_chinese_not_stripped() {
        assert_eq!(strip_filler_words("Hello world Yeah."), "Hello world Yeah.");
    }

    #[test]
    fn filler_strip_no_filler() {
        assert_eq!(
            strip_filler_words("你好世界。今天天气不错。"),
            "你好世界。今天天气不错。"
        );
    }

    #[test]
    fn filler_strip_empty() {
        assert_eq!(strip_filler_words(""), "");
    }

    #[test]
    fn filler_strip_only_filler_with_chinese() {
        assert_eq!(strip_filler_words("你好世界 Yeah"), "你好世界");
    }

    #[test]
    fn filler_strip_no_space_variant() {
        assert_eq!(strip_filler_words("你好世界。Yeah."), "你好世界。");
    }

    #[test]
    fn filler_strip_chinese_period_then_yeah() {
        assert_eq!(
            strip_filler_words("我现在呢在做一个语音输入的。然后有一个假的流逝输入。Yeah."),
            "我现在呢在做一个语音输入的。然后有一个假的流逝输入。"
        );
    }

    #[test]
    fn filler_strip_preserves_chinese_text() {
        assert_eq!(strip_filler_words("好的，我知道了。"), "好的，我知道了。");
    }

    // ── 引擎 reset 测试 ──

    #[test]
    fn engine_reset_clears_state() {
        let engine = PseudoStreamingSttEngine {
            inner: Arc::new(Mutex::new(PseudoInner {
                vad: {
                    let mut v = EnergyVad::new(16000);
                    v.process_chunk(&[0.1; 1600]);
                    v
                },
                sentences: {
                    let mut s = SentenceState::new();
                    s.append_confirmed("测试");
                    s
                },
                samples: vec![0.1; 1000],
                last_preview: Instant::now() - Duration::from_secs(10),
                last_preview_elapsed: Duration::ZERO,
                last_preview_sample_end: 0,
                preview_in_flight: true,
                latest_preview: "测试预览".to_string(),
                preview_generation: 0,
                session_failed: false,
            })),
            connection: None,
            sample_rate: 16000,
        };

        engine.reset();

        let inner = engine.inner.lock().unwrap();
        assert!(!inner.vad.is_speaking());
        assert!(inner.samples.is_empty());
        assert!(inner.latest_preview.is_empty());
        assert!(!inner.preview_in_flight);
        assert_eq!(
            inner.preview_generation, 1,
            "reset 应递增 preview_generation"
        );
        assert_eq!(inner.sentences.confirmed_text(), "");
        assert_eq!(inner.sentences.committed_sample_end, 0);
        assert!(inner.sentences.pending.is_none());
        assert!(!inner.sentences.finalize_in_flight);
    }

    // 验证带连接快照的引擎能正常构造和 reset
    #[test]
    fn engine_with_token_constructs_and_resets() {
        let engine = PseudoStreamingSttEngine {
            inner: Arc::new(Mutex::new(PseudoInner {
                vad: EnergyVad::new(16000),
                sentences: SentenceState::new(),
                samples: vec![0.1; 100],
                last_preview: Instant::now(),
                last_preview_elapsed: Duration::ZERO,
                last_preview_sample_end: 0,
                preview_in_flight: false,
                latest_preview: String::new(),
                preview_generation: 0,
                session_failed: false,
            })),
            connection: Some(crate::domain::stt::SttEngineConnection {
                host: "127.0.0.1".to_string(),
                port: 8000,
                engine_id: "funasr".to_string(),
                instance_id: "inst-test".to_string(),
                transport: None,
            }),
            sample_rate: 16000,
        };

        engine.reset();

        let inner = engine.inner.lock().unwrap();
        assert!(inner.samples.is_empty());
        assert_eq!(inner.preview_generation, 1);
    }

    #[tokio::test]
    async fn hard_limit_forces_boundary_for_audio_stuck_outside_vad_speaking() {
        let engine = PseudoStreamingSttEngine {
            inner: Arc::new(Mutex::new(PseudoInner {
                vad: EnergyVad::new(16_000),
                sentences: SentenceState::new(),
                samples: Vec::new(),
                last_preview: Instant::now(),
                last_preview_elapsed: Duration::ZERO,
                last_preview_sample_end: 0,
                preview_in_flight: false,
                latest_preview: String::new(),
                preview_generation: 0,
                session_failed: false,
            })),
            connection: None,
            sample_rate: 16_000,
        };

        // 该幅度低于当前 off threshold，不会进入 VAD speaking；绝对窗口保险
        // 仍必须在 12 秒处制造边界。无 transport 会立即安全 rollback。
        let audio = vec![0.006; 16_000 * 12];
        engine.transcribe_chunk(&audio).await.unwrap();

        let inner = engine.inner.lock().unwrap();
        assert!(!inner.vad.is_speaking());
        assert_eq!(inner.preview_generation, 1, "强制边界必须推进预览代际");
        assert_eq!(inner.last_preview_sample_end, audio.len());
    }

    #[test]
    fn reset_recovers_and_clears_poisoned_mutex() {
        let engine = PseudoStreamingSttEngine {
            inner: Arc::new(Mutex::new(PseudoInner {
                vad: EnergyVad::new(16_000),
                sentences: SentenceState::new(),
                samples: Vec::new(),
                last_preview: Instant::now(),
                last_preview_elapsed: Duration::ZERO,
                last_preview_sample_end: 0,
                preview_in_flight: false,
                latest_preview: String::new(),
                preview_generation: 0,
                session_failed: false,
            })),
            connection: None,
            sample_rate: 16_000,
        };
        let inner = Arc::clone(&engine.inner);
        let _ = std::panic::catch_unwind(move || {
            let _guard = inner.lock().unwrap();
            panic!("poison for reset test");
        });
        assert!(engine.inner.is_poisoned());
        engine.reset();
        assert!(!engine.inner.is_poisoned());
        assert!(engine.inner.lock().is_ok());
    }

    // ── 0.22.15 fix 新增测试 ──

    #[test]
    fn sentence_state_deferred_when_finalize_in_flight() {
        // finalize_in_flight 时句尾暂存到 deferred
        let mut state = SentenceState::new();
        let _p1 = state.on_sentence_end(1000, "").expect("应有 pending");
        state.finalize_in_flight = true;

        // 第二句尾应被暂存
        let p2 = state.on_sentence_end(2000, "preview2");
        assert!(p2.is_none(), "finalize_in_flight 时应返回 None");
        assert!(state.deferred.is_some(), "应暂存到 deferred");

        // commit 第一句后，deferred 转为 pending
        let r1 = FinalizeResult {
            identity: SegmentIdentity {
                session_generation: state.session_generation,
                segment_id: 1,
            },
            text: "第一句".to_string(),
            ok: true,
        };
        let deferred = state.commit_or_rollback(&r1);
        assert!(deferred.is_some(), "应返回 deferred segment");
        assert!(state.pending.is_some(), "deferred 应已转为 pending");
        assert!(state.finalize_in_flight, "finalize_in_flight 应为 true");
    }

    #[test]
    fn sentence_state_try_compact_after_commit() {
        // commit 后可以 compact 已 committed 的 PCM
        let mut state = SentenceState::new();
        let p1 = state.on_sentence_end(1000, "").expect("应有 pending");
        let r1 = FinalizeResult {
            identity: p1.identity,
            text: "你好".to_string(),
            ok: true,
        };
        state.commit_or_rollback(&r1);
        assert_eq!(state.committed_sample_end, 1000);

        // try_compact 应返回 1000（可回收 1000 个样本）
        let n = state.try_compact(1000);
        assert_eq!(n, Ok(Some(1000)));
        assert_eq!(state.buffer_base_sample, 1000);

        // 再次 try_compact 应返回 None
        assert_eq!(state.try_compact(0), Ok(None));
    }

    #[test]
    fn sentence_state_try_compact_blocked_by_pending() {
        // 有 pending 时不 compact
        let mut state = SentenceState::new();
        state.on_sentence_end(1000, "");
        assert_eq!(state.try_compact(1000), Ok(None), "有 pending 不应 compact");
    }

    #[test]
    fn compact_rejects_committed_end_past_buffer() {
        let mut state = SentenceState::new();
        state.committed_sample_end = 1_001;
        assert!(state.try_compact(1_000).is_err());
        assert_eq!(state.buffer_base_sample, 0, "失败时不得推进 buffer base");
    }

    #[test]
    fn sentence_state_compact_adjusts_range() {
        // compact 后 on_sentence_end 的 range 应为绝对坐标
        let mut state = SentenceState::new();
        let p1 = state.on_sentence_end(1000, "").expect("应有 pending");
        let r1 = FinalizeResult {
            identity: p1.identity,
            text: "你好".to_string(),
            ok: true,
        };
        state.commit_or_rollback(&r1);
        state.try_compact(1000).unwrap(); // buffer_base_sample = 1000

        // 第二句绝对范围 [1000, 2000)
        let p2 = state.on_sentence_end(2000, "").expect("应有 pending");
        assert_eq!(p2.range, 1000..2000, "range 应为绝对坐标");
    }

    // ── 0.22.15 follow-up: 统一绝对坐标后的新增测试 ──

    /// 复现生产崩溃：第一段 commit + compact 后，第二段 SentenceEnd 不 panic。
    ///
    /// 生产调用方式：维护真实 `Vec<f32>`，实际执行 drain/切片。
    #[test]
    fn production_semantics_compact_then_second_sentence_no_panic() {
        let mut state = SentenceState::new();
        let mut samples: Vec<f32> = vec![0.1; 1000]; // 第一段 1000 samples

        // 第一段句尾 → pending [0..1000)（绝对）
        let p1 = state
            .on_sentence_end(1000, "preview1")
            .expect("应有 pending");
        // commit 第一段
        let r1 = FinalizeResult {
            identity: p1.identity,
            text: "第一句".to_string(),
            ok: true,
        };
        state.commit_or_rollback(&r1);
        assert_eq!(state.committed_sample_end, 1000);

        // compact：drain 前 1000 个样本
        let n = state
            .try_compact(samples.len())
            .expect("坐标应合法")
            .expect("应可 compact");
        assert_eq!(n, 1000);
        samples.drain(..n);
        assert_eq!(samples.len(), 0);
        assert_eq!(state.buffer_base_sample, 1000);

        // 追加第二段 600 samples
        samples.extend(vec![0.1; 600]);
        let total = state.buffer_base_sample + samples.len(); // 1600

        // 第二段句尾 → pending [1000..1600)（绝对）→ 不 panic
        let p2 = state
            .on_sentence_end(total, "preview2")
            .expect("应有 pending");
        assert_eq!(p2.range, 1000..1600);

        // 用 abs_to_local_range 取局部切片 → 0..600
        let local = state.abs_to_local_range(&p2.range, samples.len());
        assert_eq!(local, Some(0..600));

        // commit 第二段
        let r2 = FinalizeResult {
            identity: p2.identity,
            text: "第二句".to_string(),
            ok: true,
        };
        state.commit_or_rollback(&r2);
        assert_eq!(state.committed_sample_end, 1600);
        assert_eq!(state.confirmed_text(), "第一句第二句");
    }

    /// compact 后第三段继续工作。
    #[test]
    fn production_semantics_three_segments_with_compact() {
        let mut state = SentenceState::new();
        let mut samples: Vec<f32> = vec![0.1; 1000];

        // Seg1: 0..1000
        let p1 = state.on_sentence_end(1000, "").unwrap();
        state.commit_or_rollback(&FinalizeResult {
            identity: p1.identity,
            text: "A".to_string(),
            ok: true,
        });
        // compact
        let n = state.try_compact(samples.len()).unwrap().unwrap();
        samples.drain(..n);
        // buffer_base = 1000

        // Seg2: append 600 → total 1600
        samples.extend(vec![0.1; 600]);
        let total = state.buffer_base_sample + samples.len();
        let p2 = state.on_sentence_end(total, "").unwrap();
        assert_eq!(p2.range, 1000..1600);
        state.commit_or_rollback(&FinalizeResult {
            identity: p2.identity,
            text: "B".to_string(),
            ok: true,
        });
        // compact again
        let n = state.try_compact(samples.len()).unwrap().unwrap();
        samples.drain(..n);
        // buffer_base = 1600

        // Seg3: append 400 → total 2000
        samples.extend(vec![0.1; 400]);
        let total = state.buffer_base_sample + samples.len();
        let p3 = state.on_sentence_end(total, "").unwrap();
        assert_eq!(p3.range, 1600..2000);
        state.commit_or_rollback(&FinalizeResult {
            identity: p3.identity,
            text: "C".to_string(),
            ok: true,
        });
        assert_eq!(state.committed_sample_end, 2000);
        assert_eq!(state.confirmed_text(), "ABC");
    }

    /// compact 后 preview snapshot 只包含未 committed PCM。
    #[test]
    fn preview_snapshot_after_compact_only_uncommitted() {
        let mut state = SentenceState::new();
        let samples: Vec<f32> = vec![0.1; 600]; // 600 samples after compact

        // committed_sample_end = 1000, buffer_base = 1000
        state.committed_sample_end = 1000;
        state.buffer_base_sample = 1000;

        // preview range = [1000, 1600) → local [0, 600)
        let total = state.buffer_base_sample + samples.len();
        let abs_range = state.committed_sample_end..total;
        let local = state.abs_to_local_range(&abs_range, samples.len());
        assert_eq!(local, Some(0..600));
        // 切片取到的就是全部 600 samples
        let snapshot = &samples[local.unwrap()];
        assert_eq!(snapshot.len(), 600);
    }

    /// compact 后 finalize 只转录剩余 PCM。
    #[test]
    fn finalize_after_compact_only_remaining() {
        let mut state = SentenceState::new();
        let samples: Vec<f32> = vec![0.1; 400]; // 400 remaining after compact

        state.committed_sample_end = 1000;
        state.buffer_base_sample = 1000;

        // finalize 取 [1000, 1400) → local [0, 400)
        let abs_start = state.committed_sample_end;
        let abs_end = state.buffer_base_sample + samples.len();
        let abs_range = abs_start..abs_end;
        let local = state.abs_to_local_range(&abs_range, samples.len());
        assert_eq!(local, Some(0..400));
        let remaining = &samples[local.unwrap()];
        assert_eq!(remaining.len(), 400);
    }

    /// 非法 absolute range 不 panic，返回 None。
    #[test]
    fn abs_to_local_range_invalid_returns_none() {
        let mut state = SentenceState::new();
        state.buffer_base_sample = 1000;

        // abs_start < buffer_base
        assert_eq!(
            state.abs_to_local_range(&(500..1500), 1000),
            None,
            "abs_start < buffer_base 应返回 None"
        );

        // abs_end < abs_start
        let reversed_start = 1200;
        let reversed_end = 1100;
        assert_eq!(
            state.abs_to_local_range(&(reversed_start..reversed_end), 1000),
            None,
            "abs_end < abs_start 应返回 None"
        );

        // local_end > samples_len
        assert_eq!(
            state.abs_to_local_range(&(1000..3000), 1000),
            None,
            "local_end > samples_len 应返回 None"
        );

        // 合法 range
        assert_eq!(state.abs_to_local_range(&(1000..2000), 1000), Some(0..1000));
    }

    /// pending/deferred 存在时不能 drain 它们仍引用的音频。
    #[test]
    fn compact_blocked_when_pending_or_deferred() {
        let mut state = SentenceState::new();

        // 有 pending
        state.on_sentence_end(1000, "");
        assert_eq!(state.try_compact(1000), Ok(None), "有 pending 不应 compact");

        // commit pending
        state.commit_or_rollback(&FinalizeResult {
            identity: SegmentIdentity {
                session_generation: state.session_generation,
                segment_id: 1,
            },
            text: "x".to_string(),
            ok: true,
        });

        // 无 pending/deferred → 可以 compact
        assert!(state.try_compact(1000).unwrap().is_some());

        // 有 deferred
        state.buffer_base_sample = state.committed_sample_end; // reset compact state
        state.on_sentence_end(state.committed_sample_end + 1000, "");
        state.finalize_in_flight = true;
        state.on_sentence_end(state.committed_sample_end + 2000, ""); // → deferred
        assert!(state.deferred.is_some());
        assert_eq!(
            state.try_compact(2000),
            Ok(None),
            "有 deferred 不应 compact"
        );
    }

    /// reset 后 base、committed、pending、deferred 和 generation 全部回到一致状态。
    #[test]
    fn reset_full_consistency() {
        let mut state = SentenceState::new();
        state.append_confirmed("test");
        state.committed_sample_end = 5000;
        state.buffer_base_sample = 3000;
        state.on_sentence_end(6000, "");
        state.finalize_in_flight = true;
        let old_gen = state.session_generation;
        let old_seg = state.next_segment_id;

        state.reset();

        assert_eq!(state.confirmed_text(), "");
        assert_eq!(state.committed_sample_end, 0);
        assert_eq!(state.buffer_base_sample, 0);
        assert!(state.pending.is_none());
        assert!(state.deferred.is_none());
        assert!(!state.finalize_in_flight);
        assert_ne!(state.session_generation, old_gen);
        assert_eq!(state.next_segment_id, 1);
        assert_ne!(state.next_segment_id, old_seg);

        // reset 后 abs_to_local_range 在空 samples 上工作正常
        assert_eq!(state.abs_to_local_range(&(0..0), 0), Some(0..0));
    }
}
