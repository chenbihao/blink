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

use super::postprocess::{strip_confirmed_prefix, strip_filler_words, trim_trailing_silence};
use super::sentence_state::{FinalizeResult, SegmentIdentity, SentenceState};
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
                terminal_finalizing = self.sentences.finalizing.is_some(),
                preview_in_flight = self.preview_in_flight,
                "STT session 进入失败态"
            );
        }
        self.session_failed = true;
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

            if inner.preview_generation == generation {
                inner.preview_in_flight = false;
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
}

// ── 测试 ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests;
