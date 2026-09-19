//! 0.23.9 伪流式识别协调器的纯领域类型与调度状态。
//!
//! 这里不持有 transport、Tauri 或目标窗口状态，只处理音频时间轴上的
//! Preview/Draft 请求。请求的 `owned_range` 是调度覆盖范围，送入模型前可由
//! [`RequestAudioGate`] 得出一个更窄的 `model_input_range`；两者不能混用。
//!
//! 0.23.9：协调器已成为生产 PreviewDraft 路径的唯一调度真源。
//! `PseudoInner` 持有并调用本协调器管理水位、请求槽和过载状态。

use std::collections::VecDeque;

pub(crate) use crate::domain::stt::{AudioRange, DraftSpan, RecognitionProfile};

fn range_is_valid(range: AudioRange) -> bool {
    range.end_sample >= range.start_sample
}

fn range_len_samples(range: AudioRange) -> u64 {
    range.end_sample.saturating_sub(range.start_sample)
}

/// 双层识别的用户可配置部分。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecognitionSettings {
    pub preview_window_ms: u64,
    pub preview_refresh_ms: u64,
    pub draft_min_s: u64,
    pub strong_pause_ms: u64,
    /// 0.23.14 长静音独立终结门槛（毫秒）：停顿达到该值且存在可信有声
    /// 即接受候选为 Draft，不再要求 owned ≥ draft_min / 2s——短句后的
    /// 长静音也必须产生可靠 Draft。噪声由 voiced 下限与模型 NoSpeech 兜底。
    pub long_pause_ms: u64,
    /// 0.23.16.5 预览短语固定冻结节奏（毫秒，0=关闭，沿用窗口滚动兜底）。
    pub phrase_freeze_interval_ms: u64,
    /// 0.23.16.5 Draft 目标窗口中心（秒，0=关闭，现状"首个合格候选即切"）。
    pub draft_target_s: u64,
    /// 0.23.16.5 Draft 目标窗口宽容（秒，优选窗口半宽；target=0 时未用）。
    pub draft_target_tolerance_s: u64,
}

impl Default for RecognitionSettings {
    fn default() -> Self {
        Self {
            preview_window_ms: 3_000,
            preview_refresh_ms: 700,
            draft_min_s: 5,
            strong_pause_ms: 700,
            long_pause_ms: 1_100,
            phrase_freeze_interval_ms: 0,
            draft_target_s: 0,
            draft_target_tolerance_s: 2,
        }
    }
}

impl RecognitionSettings {
    pub const PREVIEW_WINDOW_MIN_MS: u64 = 2_000;
    pub const PREVIEW_WINDOW_MAX_MS: u64 = 4_000;
    pub const PREVIEW_REFRESH_MIN_MS: u64 = 500;
    pub const PREVIEW_REFRESH_MAX_MS: u64 = 1_000;
    pub const DRAFT_MIN_MIN_S: u64 = 3;
    pub const DRAFT_MIN_MAX_S: u64 = 10;
    pub const STRONG_PAUSE_MIN_MS: u64 = 500;
    pub const STRONG_PAUSE_MAX_MS: u64 = 1_500;
    pub const LONG_PAUSE_MIN_MS: u64 = 800;
    pub const LONG_PAUSE_MAX_MS: u64 = 2_000;
    /// 0.23.16.5：短语固定冻结节奏边界（0=关闭；设值 800～3000ms）。
    pub const PHRASE_FREEZE_MIN_MS: u64 = 800;
    pub const PHRASE_FREEZE_MAX_MS: u64 = 3_000;
    /// 0.23.16.5：Draft 目标窗口中心边界（0=关闭；设值 4～30s）。
    pub const DRAFT_TARGET_MIN_S: u64 = 4;
    pub const DRAFT_TARGET_MAX_S: u64 = 30;
    /// 0.23.16.5：目标窗口宽容边界（秒）。
    pub const DRAFT_TARGET_TOLERANCE_MIN_S: u64 = 1;
    pub const DRAFT_TARGET_TOLERANCE_MAX_S: u64 = 10;
    /// `long_pause_ms` 严格大于 `strong_pause_ms` 的最小间隔（毫秒）。
    /// 与配置层 `RECOGNITION_PAUSE_MIN_GAP_MS`（滑块步长 50ms）保持一致。
    pub const PAUSE_MIN_GAP_MS: u64 = 50;

    /// 与配置层保持同一组边界；`max_uncommitted_s` 是现有 VAD 配置，
    /// 因而作为 sanitize 的输入而不是重复存一份设置。
    pub fn sanitize(self, max_uncommitted_s: u64) -> Self {
        let preview_window_ms = self
            .preview_window_ms
            .clamp(Self::PREVIEW_WINDOW_MIN_MS, Self::PREVIEW_WINDOW_MAX_MS);
        let mut preview_refresh_ms = self
            .preview_refresh_ms
            .clamp(Self::PREVIEW_REFRESH_MIN_MS, Self::PREVIEW_REFRESH_MAX_MS);
        if preview_refresh_ms >= preview_window_ms {
            preview_refresh_ms = preview_window_ms.saturating_sub(1).max(1);
        }
        let max_draft_s = max_uncommitted_s.max(Self::DRAFT_MIN_MIN_S);
        let draft_min_s = self
            .draft_min_s
            .clamp(Self::DRAFT_MIN_MIN_S, Self::DRAFT_MIN_MAX_S)
            .min(max_draft_s);
        let strong_pause_ms = self
            .strong_pause_ms
            .clamp(Self::STRONG_PAUSE_MIN_MS, Self::STRONG_PAUSE_MAX_MS);
        // 0.23.14.6：长静音必须严格晚于强停顿（至少一个滑块步长）——相等时
        // long 分支的约 300ms 有声门槛会遮蔽强停顿的 2s/1.2s 保护。
        let long_pause_floor = strong_pause_ms
            .saturating_add(Self::PAUSE_MIN_GAP_MS)
            .clamp(Self::LONG_PAUSE_MIN_MS, Self::LONG_PAUSE_MAX_MS);
        let long_pause_ms = self
            .long_pause_ms
            .clamp(Self::LONG_PAUSE_MIN_MS, Self::LONG_PAUSE_MAX_MS)
            .max(long_pause_floor);
        // 0.23.16.5：固定冻结节奏 / 目标窗口——边界与配置层同一组常量
        //（sanitize 语义见 RecognitionConfig::sanitize）。
        let phrase_freeze_interval_ms = if self.phrase_freeze_interval_ms == 0 {
            0
        } else {
            self.phrase_freeze_interval_ms
                .clamp(Self::PHRASE_FREEZE_MIN_MS, Self::PHRASE_FREEZE_MAX_MS)
        };
        let draft_target_tolerance_s = self
            .draft_target_tolerance_s
            .clamp(Self::DRAFT_TARGET_TOLERANCE_MIN_S, Self::DRAFT_TARGET_TOLERANCE_MAX_S);
        let mut draft_target_s = if self.draft_target_s == 0 {
            0
        } else {
            self.draft_target_s
                .clamp(Self::DRAFT_TARGET_MIN_S, Self::DRAFT_TARGET_MAX_S)
        };
        if draft_target_s != 0 {
            let ceiling_cap = max_uncommitted_s.saturating_sub(draft_target_tolerance_s);
            if ceiling_cap < draft_target_s {
                draft_target_s = if ceiling_cap >= Self::DRAFT_TARGET_MIN_S {
                    ceiling_cap
                } else {
                    0
                };
            }
        }
        Self {
            preview_window_ms,
            preview_refresh_ms,
            draft_min_s,
            strong_pause_ms,
            long_pause_ms,
            phrase_freeze_interval_ms,
            draft_target_s,
            draft_target_tolerance_s,
        }
    }

    /// 0.23.16.5：目标窗口模式下普通停顿候选的采纳下限（样本）。
    ///
    /// floor = max(draft_min_s, target − tolerance)——目标模式下
    /// draft_min 不再单独放行（被 floor 取代取大者）；返回 None 表示
    /// 目标模式关闭（现状语义）。长静音与强制切不走此门。
    pub fn draft_target_floor_samples(&self, sample_rate: u64) -> Option<u64> {
        if self.draft_target_s == 0 || sample_rate == 0 {
            return None;
        }
        let floor_s = self
            .draft_target_s
            .saturating_sub(self.draft_target_tolerance_s)
            .max(self.draft_min_s);
        Some(floor_s.saturating_mul(sample_rate))
    }

    #[cfg(test)]
    pub const fn legacy() -> Self {
        Self {
            preview_window_ms: 8_000,
            preview_refresh_ms: 500,
            draft_min_s: 0,
            strong_pause_ms: 0,
            long_pause_ms: 0,
            phrase_freeze_interval_ms: 0,
            draft_target_s: 0,
            draft_target_tolerance_s: 2,
        }
    }
}

/// VAD/调度层观察到的候选边界。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoundaryCandidate {
    /// 绝对切点，通常为 `quiet_start_sample`，不包含尾部静音。
    pub boundary_sample: u64,
    /// 原始能量第一次进入连续低能量窗口的位置。
    pub quiet_start_sample: u64,
    /// `natural_silence` / `soft_window` / `hard_window` 等稳定原因。
    pub reason: String,
    /// 候选所属未提交区间内的有效有声样本数（gate 口径，RMS ≥ off）。
    pub voiced_samples: u64,
    /// 0.23.14.7 case_17：其中强有声样本数（RMS ≥ on）。与
    /// `voiced_samples` 同窗口累计。**注意：两者之比实测不具区分度**
    /// （伪文本候选 32.1%、合法噪声候选 21.9%、真句最弱档 38.5%，区间重叠），
    /// 只作证据链记录，不构成采纳判据；采纳判据见 `strong_run_max_samples`。
    pub strong_samples: u64,
    /// 0.23.14.7 case_17：其中**最长连续强有声段**（RMS ≥ on 的连续帧）。
    ///
    /// 与 `strong_samples` 互补：环境声可以在总量上凑够强有声，但形态上是
    /// 稀疏短脉冲（键盘敲击、风扇换挡），连续段只有几十毫秒；真语音的音节
    /// 是连续发声段，连续强帧可达数百毫秒。该量是"有没有真正发过声"的
    /// 形态证据，与能量占比无关，因此不随底噪量级漂移。
    pub strong_run_max_samples: u64,
    /// 已观察到的连续低能量时长。
    pub quiet_samples: u64,
}

impl BoundaryCandidate {
    pub fn is_strong(&self, sample_rate: u32, strong_pause_ms: u64) -> bool {
        self.quiet_samples >= strong_pause_ms.saturating_mul(u64::from(sample_rate)) / 1000
    }
}

/// 送模前统一执行的音频卫生结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestAudioGate {
    /// 输入包含足够有效语音，可调用模型。
    Valid {
        model_input_range: AudioRange,
        voiced_samples: u64,
    },
    /// 普通 Preview/Draft 暂时太短，继续积累；结束 drain 可显式放宽。
    TooShort,
    /// 整个 owned range 没有有效语音，调用方应消费覆盖范围但不调用模型。
    NoSpeech,
}

impl RequestAudioGate {
    /// 按 10ms frame 统计能量，裁掉首尾静音并返回绝对 model range。
    ///
    /// `samples.len()` 必须对应 `owned_range.len_samples()`；不一致时只使用
    /// 可安全映射的前缀，禁止 panic。gate 不读取或记录转写正文。
    pub fn evaluate(
        owned_range: AudioRange,
        samples: &[f32],
        sample_rate: u32,
        off_threshold: f64,
        min_voiced_ms: u64,
        allow_short: bool,
    ) -> Self {
        if !range_is_valid(owned_range) || samples.is_empty() || sample_rate == 0 {
            return Self::NoSpeech;
        }

        let expected = range_len_samples(owned_range).min(usize::MAX as u64) as usize;
        let usable = samples.len().min(expected);
        if usable == 0 {
            return Self::NoSpeech;
        }

        let frame_size = (u64::from(sample_rate) / 100).max(1) as usize;
        let threshold = if off_threshold.is_finite() {
            off_threshold.max(0.0)
        } else {
            0.0
        };
        let mut first_voiced = None;
        let mut last_voiced_end = 0usize;
        let mut voiced_samples = 0u64;

        let mut offset = 0usize;
        while offset < usable {
            let end = (offset + frame_size).min(usable);
            let frame = &samples[offset..end];
            let sum_sq = frame
                .iter()
                .map(|sample| {
                    let value = f64::from(*sample);
                    if value.is_finite() {
                        value * value
                    } else {
                        0.0
                    }
                })
                .sum::<f64>();
            let rms = (sum_sq / frame.len() as f64).sqrt();
            if rms >= threshold {
                first_voiced.get_or_insert(offset);
                last_voiced_end = end;
                voiced_samples = voiced_samples.saturating_add((end - offset) as u64);
            }
            offset = end;
        }

        let Some(first_voiced) = first_voiced else {
            return Self::NoSpeech;
        };
        let min_voiced_samples = min_voiced_ms.saturating_mul(u64::from(sample_rate)) / 1000;
        if !allow_short && voiced_samples < min_voiced_samples {
            return Self::TooShort;
        }

        let start = owned_range.start_sample.saturating_add(first_voiced as u64);
        let end = owned_range
            .start_sample
            .saturating_add(last_voiced_end as u64)
            .min(owned_range.end_sample);
        if end <= start {
            Self::NoSpeech
        } else {
            Self::Valid {
                model_input_range: AudioRange::new(start, end),
                voiced_samples,
            }
        }
    }
}

/// 可排队的 Preview 请求。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreviewRequest {
    pub request_id: u64,
    pub audio_range: AudioRange,
    pub model_input_range: Option<AudioRange>,
    pub revision: u64,
}

/// 可可靠交付的 Draft 请求。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DraftRequest {
    pub request_id: u64,
    pub span_id: u64,
    pub owned_range: AudioRange,
    pub model_input_range: Option<AudioRange>,
    pub revision: u64,
}

/// 单 worker 调度状态。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecognitionCoordinator {
    pub profile: RecognitionProfile,
    pub settings: RecognitionSettings,
    pub sample_rate: u32,
    pub captured_audio_end: u64,
    pub draft_committed_audio_end: u64,
    pub draft_reserved_audio_end: u64,
    pub candidate: Option<BoundaryCandidate>,
    pub running_draft: Option<DraftRequest>,
    pub pending_draft: Option<DraftRequest>,
    pub running_preview: Option<PreviewRequest>,
    pub pending_preview: Option<PreviewRequest>,
    pub spans: Vec<DraftSpan>,
    pub next_request_id: u64,
    pub next_span_id: u64,
    pub overloaded: bool,
    pub closing: bool,
    queued_preview_revisions: VecDeque<u64>,
}

impl RecognitionCoordinator {
    pub fn new(
        sample_rate: u32,
        profile: RecognitionProfile,
        settings: RecognitionSettings,
    ) -> Self {
        Self {
            profile,
            settings,
            sample_rate,
            captured_audio_end: 0,
            draft_committed_audio_end: 0,
            draft_reserved_audio_end: 0,
            candidate: None,
            running_draft: None,
            pending_draft: None,
            running_preview: None,
            pending_preview: None,
            spans: Vec::new(),
            next_request_id: 0,
            next_span_id: 1,
            overloaded: false,
            closing: false,
            queued_preview_revisions: VecDeque::new(),
        }
    }

    pub fn backlog_samples(&self) -> u64 {
        self.captured_audio_end
            .saturating_sub(self.draft_committed_audio_end)
    }

    pub fn backlog_limit_samples(&self, max_uncommitted_s: u64) -> u64 {
        max_uncommitted_s
            .saturating_mul(2)
            .saturating_mul(u64::from(self.sample_rate))
    }

    pub fn accept_audio_end(&mut self, captured_audio_end: u64, max_uncommitted_s: u64) -> bool {
        if captured_audio_end < self.captured_audio_end {
            return false;
        }
        self.captured_audio_end = captured_audio_end;
        if self.backlog_samples() >= self.backlog_limit_samples(max_uncommitted_s) {
            self.overloaded = true;
            self.pending_preview = None;
            return false;
        }
        true
    }

    pub fn next_request_id(&mut self) -> u64 {
        self.next_request_id = self.next_request_id.wrapping_add(1).max(1);
        self.next_request_id
    }

    /// 新的 Preview 只替换尚未开始的 Preview；已运行请求由 request id 校验 stale。
    pub fn replace_pending_preview(&mut self, request: PreviewRequest) {
        if self.closing || self.overloaded {
            return;
        }
        if let Some(previous) = self.pending_preview.replace(request) {
            self.queued_preview_revisions.push_back(previous.revision);
        }
    }

    /// 在 Draft 运行期间把新的候选合并进唯一 pending 槽，并受单段上限保护。
    pub fn reserve_draft(&mut self, mut request: DraftRequest, max_uncommitted_s: u64) {
        if self.closing || self.overloaded || !range_is_valid(request.owned_range) {
            return;
        }
        let cap = max_uncommitted_s.saturating_mul(u64::from(self.sample_rate));
        request.owned_range.end_sample = request
            .owned_range
            .end_sample
            .min(request.owned_range.start_sample.saturating_add(cap));
        if request.owned_range.end_sample <= request.owned_range.start_sample {
            return;
        }
        if let Some(pending) = self.pending_draft.as_mut() {
            pending.owned_range.end_sample = pending
                .owned_range
                .end_sample
                .max(request.owned_range.end_sample)
                .min(pending.owned_range.start_sample.saturating_add(cap));
            pending.revision = request.revision;
        } else {
            self.pending_draft = Some(request);
        }
        self.draft_reserved_audio_end = self
            .pending_draft
            .as_ref()
            .map(|pending| pending.owned_range.end_sample)
            .unwrap_or(self.draft_reserved_audio_end)
            .max(
                self.running_draft
                    .as_ref()
                    .map_or(0, |draft| draft.owned_range.end_sample),
            );
    }

    /// 选择下一请求：可靠 Draft 优先，Preview 只有没有 Draft 时才可开始。
    ///
    /// 仅测试消费——生产引擎在 PseudoInner 侧直接 spawn（preview_in_flight
    /// + spawn 路径），协调器槽位只做状态同步。
    #[cfg(test)]
    pub fn take_next(&mut self) -> Option<ScheduledRequest> {
        if self.running_draft.is_some() || self.running_preview.is_some() {
            return None;
        }
        if let Some(draft) = self.pending_draft.take() {
            self.running_draft = Some(draft.clone());
            return Some(ScheduledRequest::Draft(draft));
        }
        if let Some(preview) = self.pending_preview.take() {
            self.running_preview = Some(preview.clone());
            return Some(ScheduledRequest::Preview(preview));
        }
        None
    }

    /// 完成请求并返回新水位；stale request 不得清除新 owner。
    pub fn finish(&mut self, request_id: u64, ok: bool, text: String) -> Option<DraftSpan> {
        if self
            .running_preview
            .as_ref()
            .is_some_and(|request| request.request_id == request_id)
        {
            self.running_preview = None;
            return None;
        }
        let running = self.running_draft.take()?;
        if running.request_id != request_id {
            self.running_draft = Some(running);
            return None;
        }
        if ok && !text.is_empty() {
            let span = DraftSpan {
                span_id: running.span_id,
                audio_range: running.owned_range,
                text,
                revision: running.revision,
            };
            self.draft_committed_audio_end = self
                .draft_committed_audio_end
                .max(running.owned_range.end_sample);
            self.spans.push(span.clone());
            self.draft_reserved_audio_end = self
                .draft_reserved_audio_end
                .max(self.draft_committed_audio_end);
            Some(span)
        } else {
            // 失败不推进 committed；pending 会由调用方用新的 identity 重基。
            self.draft_reserved_audio_end = self.draft_committed_audio_end;
            None
        }
    }

    pub fn begin_closing(&mut self, recording_audio_end: u64) {
        self.closing = true;
        self.captured_audio_end = self.captured_audio_end.max(recording_audio_end);
        self.pending_preview = None;
    }

    /// 0.23.16.7：起点早于 `boundary_sample` 的排队 Preview 已覆盖被短语
    /// 冻结/Draft 提交接管的音频，其结果注定被范围裁决（`tail_range_is_stale`）
    /// 丢弃——提前清槽，省一次推理并把刷新机会让给从新锚点出发的快照。
    pub fn clear_pending_preview_before(&mut self, boundary_sample: u64) {
        if self
            .pending_preview
            .as_ref()
            .is_some_and(|preview| preview.audio_range.start_sample < boundary_sample)
        {
            self.pending_preview = None;
        }
    }

    /// 重置协调器到初始状态（保留 sample_rate / profile / settings）。
    ///
    /// 调用方在 `reset` 时递增 preview_generation 等，这里只负责调度状态。
    pub fn reset(&mut self) {
        self.captured_audio_end = 0;
        self.draft_committed_audio_end = 0;
        self.draft_reserved_audio_end = 0;
        self.candidate = None;
        self.running_draft = None;
        self.pending_draft = None;
        self.running_preview = None;
        self.pending_preview = None;
        self.spans.clear();
        self.next_request_id = 0;
        self.next_span_id = 1;
        self.overloaded = false;
        self.closing = false;
        self.queued_preview_revisions.clear();
    }

    /// 设置/替换当前候选边界（VAD 层观察到停顿后调用）。
    pub fn set_candidate(&mut self, candidate: BoundaryCandidate) {
        self.candidate = Some(candidate);
    }

    /// 清除当前候选（被接受或因新语音出现而作废）。
    pub fn clear_candidate(&mut self) {
        self.candidate = None;
    }

    /// NoSpeech 消费 owned range：推进 committed 但不产 span、不调用模型。
    ///
    /// 返回值与 `finish` 语义一致（None = 无 span 产出）。
    pub fn consume_no_speech(&mut self, request_id: u64) -> bool {
        let running = match self.running_draft.take() {
            Some(r) if r.request_id == request_id => r,
            Some(r) => {
                self.running_draft = Some(r);
                return false;
            }
            None => return false,
        };
        self.draft_committed_audio_end = self
            .draft_committed_audio_end
            .max(running.owned_range.end_sample);
        self.draft_reserved_audio_end = self
            .draft_reserved_audio_end
            .max(self.draft_committed_audio_end);
        true
    }

    /// terminal finalize：推进 committed 到最终位置。
    pub fn commit_terminal(&mut self, audio_end: u64) {
        self.draft_committed_audio_end = self.draft_committed_audio_end.max(audio_end);
        self.draft_reserved_audio_end = self
            .draft_reserved_audio_end
            .max(self.draft_committed_audio_end);
    }

    pub fn drain_preview_count(&self) -> usize {
        usize::from(self.running_preview.is_some()) + usize::from(self.pending_preview.is_some())
    }

    /// 产出只读诊断快照。不修改任何状态，不影响调度时序。
    ///
    /// `max_uncommitted_s` 由调用方从配置传入，用于计算 backlog_limit。
    pub fn snapshot_trace(&self, max_uncommitted_s: u64) -> CoordinatorTrace {
        let profile_str = match self.profile {
            RecognitionProfile::Legacy => "Legacy",
            RecognitionProfile::PreviewDraft => "PreviewDraft",
        };
        let request_trace = |req: Option<&DraftRequest>, state: &'static str| {
            req.map(|r| RequestTrace {
                request_id: r.request_id,
                audio_range_start: r.owned_range.start_sample,
                audio_range_end: r.owned_range.end_sample,
                revision: r.revision,
                state,
            })
        };
        let preview_trace = |req: Option<&PreviewRequest>, state: &'static str| {
            req.map(|r| RequestTrace {
                request_id: r.request_id,
                audio_range_start: r.audio_range.start_sample,
                audio_range_end: r.audio_range.end_sample,
                revision: r.revision,
                state,
            })
        };
        CoordinatorTrace {
            profile: profile_str,
            preview_window_ms: self.settings.preview_window_ms,
            preview_refresh_ms: self.settings.preview_refresh_ms,
            draft_min_s: self.settings.draft_min_s,
            strong_pause_ms: self.settings.strong_pause_ms,
            long_pause_ms: self.settings.long_pause_ms,
            sample_rate: self.sample_rate,
            captured_audio_end: self.captured_audio_end,
            draft_committed_audio_end: self.draft_committed_audio_end,
            draft_reserved_audio_end: self.draft_reserved_audio_end,
            backlog_samples: self.backlog_samples(),
            backlog_limit_samples: self.backlog_limit_samples(max_uncommitted_s),
            overloaded: self.overloaded,
            closing: self.closing,
            candidate: self.candidate.as_ref().map(|c| CandidateTrace {
                boundary_sample: c.boundary_sample,
                quiet_start_sample: c.quiet_start_sample,
                reason: c.reason.clone(),
                voiced_samples: c.voiced_samples,
                quiet_samples: c.quiet_samples,
                accepted: false,
                reject_reason: None,
            }),
            running_draft: request_trace(self.running_draft.as_ref(), "running"),
            pending_draft: request_trace(self.pending_draft.as_ref(), "pending"),
            running_preview: preview_trace(self.running_preview.as_ref(), "running"),
            pending_preview: preview_trace(self.pending_preview.as_ref(), "pending"),
            committed_spans: self.spans.len() as u64,
            drain_preview_count: self.drain_preview_count(),
        }
    }
}

/// 选择结果，便于 transport/app 层只消费一个动作。
///
/// 仅测试消费（配合 `take_next`）；生产引擎直接 spawn，不走此类型。
#[cfg(test)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScheduledRequest {
    Draft(DraftRequest),
    Preview(PreviewRequest),
}

// ── 0.23.9 调试 observer 只读 DTO ──
//
// 这些类型只由 `RecognitionCoordinator::snapshot_trace` 产出，不参与调度
// 决策，不改变生产时序。app 层在调试页打开时按需拉取，关闭后停止。DTO
// 不携带转写正文、音频文件路径或用户私有数据。

/// 候选边界的诊断快照。
#[derive(Debug, Clone, serde::Serialize)]
pub struct CandidateTrace {
    pub boundary_sample: u64,
    pub quiet_start_sample: u64,
    pub reason: String,
    pub voiced_samples: u64,
    pub quiet_samples: u64,
    pub accepted: bool,
    pub reject_reason: Option<String>,
}

/// 单个 Draft 或 Preview 请求的诊断状态。
#[derive(Debug, Clone, serde::Serialize)]
pub struct RequestTrace {
    pub request_id: u64,
    pub audio_range_start: u64,
    pub audio_range_end: u64,
    pub revision: u64,
    pub state: &'static str,
}

/// 协调器完整快照，供调试页消费。
#[derive(Debug, Clone, serde::Serialize)]
pub struct CoordinatorTrace {
    pub profile: &'static str,
    pub preview_window_ms: u64,
    pub preview_refresh_ms: u64,
    pub draft_min_s: u64,
    pub strong_pause_ms: u64,
    pub long_pause_ms: u64,
    pub sample_rate: u32,
    pub captured_audio_end: u64,
    pub draft_committed_audio_end: u64,
    pub draft_reserved_audio_end: u64,
    pub backlog_samples: u64,
    pub backlog_limit_samples: u64,
    pub overloaded: bool,
    pub closing: bool,
    pub candidate: Option<CandidateTrace>,
    pub running_draft: Option<RequestTrace>,
    pub pending_draft: Option<RequestTrace>,
    pub running_preview: Option<RequestTrace>,
    pub pending_preview: Option<RequestTrace>,
    pub committed_spans: u64,
    pub drain_preview_count: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_keeps_preview_window_longer_than_refresh() {
        let settings = RecognitionSettings {
            preview_window_ms: 1,
            preview_refresh_ms: 10_000,
            draft_min_s: 99,
            strong_pause_ms: 1,
            long_pause_ms: 99,
            ..RecognitionSettings::default()
        }
        .sanitize(4);
        assert_eq!(settings.preview_window_ms, 2_000);
        assert_eq!(settings.preview_refresh_ms, 1_000);
        assert_eq!(settings.draft_min_s, 4);
        assert_eq!(settings.strong_pause_ms, 500);
        assert_eq!(settings.long_pause_ms, 800);
    }

    /// 0.23.14.6 长静音门槛 sanitize：越界收敛，且必须严格大于强停顿至少
    /// 一个滑块步长（50ms）——相等会让 long 分支遮蔽强停顿保护。
    #[test]
    fn sanitize_clamps_long_pause_and_keeps_it_strictly_above_strong_pause() {
        let too_low = RecognitionSettings {
            long_pause_ms: 100,
            strong_pause_ms: 900,
            ..RecognitionSettings::default()
        }
        .sanitize(12);
        assert_eq!(too_low.long_pause_ms, 950, "长静音必须严格大于强停顿 ≥50ms");

        // 相等输入同样被拉开
        let equal = RecognitionSettings {
            long_pause_ms: 1_200,
            strong_pause_ms: 1_200,
            ..RecognitionSettings::default()
        }
        .sanitize(12);
        assert_eq!(equal.long_pause_ms, 1_250);

        let too_high = RecognitionSettings {
            long_pause_ms: 9_999,
            ..RecognitionSettings::default()
        }
        .sanitize(12);
        assert_eq!(too_high.long_pause_ms, 2_000);
    }

    #[test]
    fn audio_gate_trims_silence_and_keeps_absolute_coordinates() {
        let mut samples = vec![0.0; 160];
        samples.extend([0.2; 320]);
        samples.extend([0.0; 160]);
        let result = RequestAudioGate::evaluate(
            AudioRange::new(10_000, 10_640),
            &samples,
            16_000,
            0.01,
            10,
            false,
        );
        assert_eq!(
            result,
            RequestAudioGate::Valid {
                model_input_range: AudioRange::new(10_160, 10_480),
                voiced_samples: 320,
            }
        );
    }

    #[test]
    fn audio_gate_consumes_no_speech_without_model_range() {
        let result = RequestAudioGate::evaluate(
            AudioRange::new(100, 260),
            &[0.0; 160],
            16_000,
            0.01,
            1,
            false,
        );
        assert_eq!(result, RequestAudioGate::NoSpeech);
    }

    #[test]
    fn scheduler_has_one_pending_slot_and_draft_priority() {
        let mut scheduler = RecognitionCoordinator::new(
            16_000,
            RecognitionProfile::PreviewDraft,
            RecognitionSettings::default(),
        );
        scheduler.replace_pending_preview(PreviewRequest {
            request_id: 1,
            audio_range: AudioRange::new(0, 48_000),
            model_input_range: None,
            revision: 1,
        });
        scheduler.replace_pending_preview(PreviewRequest {
            request_id: 2,
            audio_range: AudioRange::new(8_000, 56_000),
            model_input_range: None,
            revision: 2,
        });
        scheduler.reserve_draft(
            DraftRequest {
                request_id: 3,
                span_id: 1,
                owned_range: AudioRange::new(0, 80_000),
                model_input_range: None,
                revision: 1,
            },
            12,
        );
        let next = scheduler.take_next();
        assert!(matches!(next, Some(ScheduledRequest::Draft(_))));
        assert!(scheduler.running_preview.is_none());
        assert_eq!(scheduler.pending_draft, None);
    }

    #[test]
    fn failed_draft_does_not_advance_committed_watermark() {
        let mut scheduler = RecognitionCoordinator::new(
            16_000,
            RecognitionProfile::PreviewDraft,
            RecognitionSettings::default(),
        );
        scheduler.reserve_draft(
            DraftRequest {
                request_id: 1,
                span_id: 1,
                owned_range: AudioRange::new(0, 80_000),
                model_input_range: None,
                revision: 1,
            },
            12,
        );
        let request = match scheduler.take_next() {
            Some(ScheduledRequest::Draft(request)) => request,
            _ => panic!("expected draft"),
        };
        assert!(
            scheduler
                .finish(request.request_id, false, String::new())
                .is_none()
        );
        assert_eq!(scheduler.draft_committed_audio_end, 0);
        assert_eq!(scheduler.draft_reserved_audio_end, 0);
    }

    // ── 0.23.9 诊断 observer 测试 ──

    /// accepted Draft 推进 reserved，成功后推进 committed。
    #[test]
    fn accepted_draft_advances_reserved_then_committed() {
        let mut scheduler = RecognitionCoordinator::new(
            16_000,
            RecognitionProfile::PreviewDraft,
            RecognitionSettings::default(),
        );
        scheduler.reserve_draft(
            DraftRequest {
                request_id: 1,
                span_id: 1,
                owned_range: AudioRange::new(0, 80_000),
                model_input_range: None,
                revision: 1,
            },
            12,
        );
        assert_eq!(scheduler.draft_reserved_audio_end, 80_000);
        let request = match scheduler.take_next().expect("应有 Draft 请求") {
            ScheduledRequest::Draft(req) => req,
            _ => panic!("expected draft"),
        };
        let span = scheduler
            .finish(request.request_id, true, "你好世界".to_string())
            .expect("应有 DraftSpan");
        assert_eq!(span.span_id, 1);
        assert_eq!(scheduler.draft_committed_audio_end, 80_000);
        assert!(scheduler.draft_reserved_audio_end >= scheduler.draft_committed_audio_end);
    }

    /// Draft 失败后 reserved 回退到 committed，产生 rebase 诊断。
    #[test]
    fn draft_failure_rebases_reserved_to_committed() {
        let mut scheduler = RecognitionCoordinator::new(
            16_000,
            RecognitionProfile::PreviewDraft,
            RecognitionSettings::default(),
        );
        // 第一次 Draft 成功，推进 committed 到 80_000
        scheduler.reserve_draft(
            DraftRequest {
                request_id: 1,
                span_id: 1,
                owned_range: AudioRange::new(0, 80_000),
                model_input_range: None,
                revision: 1,
            },
            12,
        );
        let req1 = match scheduler.take_next().expect("应有 Draft") {
            ScheduledRequest::Draft(req) => req,
            _ => panic!("expected draft"),
        };
        scheduler
            .finish(req1.request_id, true, "第一句".to_string())
            .expect("应有 span");
        assert_eq!(scheduler.draft_committed_audio_end, 80_000);

        // 第二次 Draft 失败，reserved 应回退到 committed
        scheduler.reserve_draft(
            DraftRequest {
                request_id: 2,
                span_id: 2,
                owned_range: AudioRange::new(80_000, 160_000),
                model_input_range: None,
                revision: 2,
            },
            12,
        );
        assert_eq!(scheduler.draft_reserved_audio_end, 160_000);
        let req2 = match scheduler.take_next().expect("应有 Draft") {
            ScheduledRequest::Draft(req) => req,
            _ => panic!("expected draft"),
        };
        assert!(
            scheduler
                .finish(req2.request_id, false, String::new())
                .is_none()
        );
        assert_eq!(scheduler.draft_committed_audio_end, 80_000);
        assert_eq!(scheduler.draft_reserved_audio_end, 80_000);
    }

    /// NoSpeech 不产 span 但推进 committed（通过 commit_terminal_finalize 或
    /// consume_no_speech）。Coordinator 的 finish 对空文本不产 span。
    #[test]
    fn no_speech_does_not_produce_span() {
        let mut scheduler = RecognitionCoordinator::new(
            16_000,
            RecognitionProfile::PreviewDraft,
            RecognitionSettings::default(),
        );
        scheduler.reserve_draft(
            DraftRequest {
                request_id: 1,
                span_id: 1,
                owned_range: AudioRange::new(0, 80_000),
                model_input_range: None,
                revision: 1,
            },
            12,
        );
        let req = match scheduler.take_next().expect("应有 Draft") {
            ScheduledRequest::Draft(req) => req,
            _ => panic!("expected draft"),
        };
        // ok=true 但文本为空 → 不产 span，不推进 committed
        assert!(
            scheduler
                .finish(req.request_id, true, String::new())
                .is_none()
        );
        assert_eq!(scheduler.draft_committed_audio_end, 0);
    }

    /// Preview request ID/revision 更新与旧结果丢弃。
    #[test]
    fn preview_replace_pending_discards_old_revision() {
        let mut scheduler = RecognitionCoordinator::new(
            16_000,
            RecognitionProfile::PreviewDraft,
            RecognitionSettings::default(),
        );
        scheduler.replace_pending_preview(PreviewRequest {
            request_id: 1,
            audio_range: AudioRange::new(0, 48_000),
            model_input_range: None,
            revision: 1,
        });
        scheduler.replace_pending_preview(PreviewRequest {
            request_id: 2,
            audio_range: AudioRange::new(8_000, 56_000),
            model_input_range: None,
            revision: 2,
        });
        // pending_preview 应只保留最新
        let pending = scheduler.pending_preview.as_ref().expect("应有 pending");
        assert_eq!(pending.request_id, 2);
        assert_eq!(pending.revision, 2);
        // 旧 revision 被记录在 queued_preview_revisions
        assert_eq!(scheduler.queued_preview_revisions.len(), 1);
        assert_eq!(scheduler.queued_preview_revisions[0], 1);
    }

    /// backlog overload 标记 overloaded 并清空 pending preview。
    #[test]
    fn backlog_overload_marks_overloaded() {
        let mut scheduler = RecognitionCoordinator::new(
            16_000,
            RecognitionProfile::PreviewDraft,
            RecognitionSettings::default(),
        );
        // 模拟已提交 0，传入大量音频触发 overload
        let max_uncommitted_s = 12u64;
        let limit = scheduler.backlog_limit_samples(max_uncommitted_s);
        // captured_audio_end 达到 limit → overload
        assert!(!scheduler.accept_audio_end(limit, max_uncommitted_s));
        assert!(scheduler.overloaded);
        assert!(scheduler.pending_preview.is_none());
    }

    /// terminal takeover (begin_closing) 清理 pending Preview。
    #[test]
    fn begin_closing_clears_pending_preview() {
        let mut scheduler = RecognitionCoordinator::new(
            16_000,
            RecognitionProfile::PreviewDraft,
            RecognitionSettings::default(),
        );
        scheduler.replace_pending_preview(PreviewRequest {
            request_id: 1,
            audio_range: AudioRange::new(0, 48_000),
            model_input_range: None,
            revision: 1,
        });
        assert!(scheduler.pending_preview.is_some());
        scheduler.begin_closing(100_000);
        assert!(scheduler.pending_preview.is_none());
        assert!(scheduler.closing);
    }

    /// snapshot_trace 不改变协调器行为——调用前后的调度状态一致。
    #[test]
    fn snapshot_trace_does_not_change_coordinator_behavior() {
        let mut scheduler = RecognitionCoordinator::new(
            16_000,
            RecognitionProfile::PreviewDraft,
            RecognitionSettings::default(),
        );
        scheduler.reserve_draft(
            DraftRequest {
                request_id: 1,
                span_id: 1,
                owned_range: AudioRange::new(0, 80_000),
                model_input_range: None,
                revision: 1,
            },
            12,
        );
        scheduler.replace_pending_preview(PreviewRequest {
            request_id: 2,
            audio_range: AudioRange::new(0, 48_000),
            model_input_range: None,
            revision: 1,
        });

        // 快照前状态
        let pending_before = scheduler.pending_draft.clone();
        let preview_before = scheduler.pending_preview.clone();
        let reserved_before = scheduler.draft_reserved_audio_end;
        let committed_before = scheduler.draft_committed_audio_end;

        // 产出快照
        let trace = scheduler.snapshot_trace(12);
        assert_eq!(trace.profile, "PreviewDraft");
        assert_eq!(trace.preview_window_ms, 3_000);
        assert_eq!(trace.preview_refresh_ms, 700);
        assert_eq!(trace.draft_min_s, 5);
        assert_eq!(trace.strong_pause_ms, 700);
        assert_eq!(trace.draft_reserved_audio_end, reserved_before);
        assert_eq!(trace.draft_committed_audio_end, committed_before);
        assert!(trace.pending_draft.is_some());
        assert!(trace.pending_preview.is_some());

        // 快照后状态不变
        assert_eq!(scheduler.pending_draft, pending_before);
        assert_eq!(scheduler.pending_preview, preview_before);
        assert_eq!(scheduler.draft_reserved_audio_end, reserved_before);
        assert_eq!(scheduler.draft_committed_audio_end, committed_before);

        // 调度行为仍正常
        let next = scheduler.take_next();
        assert!(matches!(next, Some(ScheduledRequest::Draft(_))));
    }

    /// Legacy profile 在快照中正确标记。
    #[test]
    fn snapshot_trace_legacy_profile_label() {
        let scheduler = RecognitionCoordinator::new(
            16_000,
            RecognitionProfile::Legacy,
            RecognitionSettings::legacy(),
        );
        let trace = scheduler.snapshot_trace(12);
        assert_eq!(trace.profile, "Legacy");
    }

    /// 空快照安全——无 candidate、无请求时不 panic。
    #[test]
    fn snapshot_trace_empty_is_safe() {
        let scheduler = RecognitionCoordinator::new(
            16_000,
            RecognitionProfile::PreviewDraft,
            RecognitionSettings::default(),
        );
        let trace = scheduler.snapshot_trace(12);
        assert!(trace.candidate.is_none());
        assert!(trace.running_draft.is_none());
        assert!(trace.pending_draft.is_none());
        assert!(trace.running_preview.is_none());
        assert!(trace.pending_preview.is_none());
        assert_eq!(trace.committed_spans, 0);
        assert_eq!(trace.drain_preview_count, 0);
        assert!(!trace.overloaded);
        assert!(!trace.closing);
    }
}
