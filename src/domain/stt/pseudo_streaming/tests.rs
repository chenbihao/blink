//! 伪流式 STT 引擎测试。
//!
//! 测试覆盖：
//! - [`SentenceState`](super::super::sentence_state::SentenceState)：commit/rollback/deferred/compact
//! - [`PseudoStreamingSttEngine`](super::PseudoStreamingSttEngine)：reset、hard limit、finalize 超时
//! - 后处理函数：[`strip_confirmed_prefix`]、[`strip_filler_words`]、[`trim_trailing_silence`]
//! - [`EnergyVad`](super::super::vad::EnergyVad)：底噪自适应、句尾检测、脉冲抑制

use super::*;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::sync::{Notify, oneshot};

struct ControlledTransport {
    responses: Mutex<VecDeque<oneshot::Receiver<Result<String, String>>>>,
    calls: AtomicUsize,
    called: Notify,
}

impl ControlledTransport {
    fn new(responses: Vec<oneshot::Receiver<Result<String, String>>>) -> Arc<Self> {
        Arc::new(Self {
            responses: Mutex::new(responses.into()),
            calls: AtomicUsize::new(0),
            called: Notify::new(),
        })
    }

    async fn wait_for_calls(&self, expected: usize) {
        while self.calls.load(Ordering::SeqCst) < expected {
            self.called.notified().await;
        }
    }

    /// 带超时的等待——用于断言"某次推理必须/不得被启动"。
    async fn wait_for_calls_or_fail(&self, expected: usize) {
        tokio::time::timeout(Duration::from_secs(2), self.wait_for_calls(expected))
            .await
            .expect("推理调用次数未在超时内达到预期");
    }
}

#[async_trait::async_trait]
impl crate::domain::stt::SttTransport for ControlledTransport {
    async fn check_ready(&self) -> Result<(), crate::domain::stt::SttTransportError> {
        Ok(())
    }

    async fn transcribe(
        &self,
        _wav_bytes: &[u8],
    ) -> Result<String, crate::domain::stt::SttTransportError> {
        let rx = self
            .responses
            .lock()
            .unwrap()
            .pop_front()
            .expect("测试必须提供 transport response");
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.called.notify_waiters();
        rx.await
            .expect("测试 response sender 不应提前 drop")
            .map_err(|detail| crate::domain::stt::SttTransportError::Unavailable { detail })
    }
}

fn controlled_engine(
    transport: Arc<ControlledTransport>,
    samples: Vec<f32>,
) -> PseudoStreamingSttEngine {
    PseudoStreamingSttEngine {
        inner: Arc::new(Mutex::new(PseudoInner::for_test(samples))),
        connection: Some(crate::domain::stt::SttEngineConnection {
            host: "127.0.0.1".into(),
            port: 0,
            engine_id: "funasr".into(),
            instance_id: "test-instance".into(),
            transport: Some(transport),
        }),
        sample_rate: 16_000,
        boundary_observer: None,
        finalize_observer: None,
    }
}

/// 无 transport 的引擎（纯状态语义测试用）。
fn engine_without_transport(samples: Vec<f32>) -> PseudoStreamingSttEngine {
    PseudoStreamingSttEngine {
        inner: Arc::new(Mutex::new(PseudoInner::for_test(samples))),
        connection: None,
        sample_rate: 16_000,
        boundary_observer: None,
        finalize_observer: None,
    }
}

/// 等待条件成立（带超时；用于等待后台推理 task 写入状态）。
async fn wait_until(condition: impl Fn() -> bool) {
    tokio::time::timeout(Duration::from_secs(2), async {
        while !condition() {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("条件未在超时内成立");
}

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
            commit_generation: pending.identity.commit_generation,
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
            commit_generation: pending.identity.commit_generation,
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
    assert_eq!(
        PseudoStreamingSttEngine::compose_result(0, "", "", false),
        ""
    );
}

#[test]
fn compose_result_with_preview_only() {
    let result = PseudoStreamingSttEngine::compose_result(3, "", "你好", false);
    let v: serde_json::Value = serde_json::from_str(&result).unwrap();
    assert_eq!(v["confirmed"], "");
    assert_eq!(v["preview"], "你好");
    assert_eq!(v["revision"], 3, "必须携带状态版本号");
    assert_eq!(v["confirmed_changed"], false);
}

#[test]
fn compose_result_with_both() {
    let result = PseudoStreamingSttEngine::compose_result(5, "你好。", "世界", true);
    let v: serde_json::Value = serde_json::from_str(&result).unwrap();
    assert_eq!(v["confirmed"], "你好。");
    assert_eq!(v["preview"], "世界");
    assert_eq!(v["confirmed_changed"], true);
    assert_eq!(v["v"], 2, "协议版本号必须存在");
}

// ── preview_interval 测试 ──

#[test]
fn preview_interval_normal() {
    let interval = PseudoStreamingSttEngine::preview_interval(16000 * 3, 16000, Duration::ZERO);
    assert_eq!(interval, Duration::from_millis(PREVIEW_INTERVAL_MS));
}

#[test]
fn preview_interval_slowdown() {
    let interval = PseudoStreamingSttEngine::preview_interval(16000 * 10, 16000, Duration::ZERO);
    assert_eq!(interval, Duration::from_millis(PREVIEW_SLOW_INTERVAL_MS));
}

#[test]
fn preview_interval_adapts_to_slow_inference_with_cap() {
    let adaptive =
        PseudoStreamingSttEngine::preview_interval(16000 * 3, 16000, Duration::from_millis(800));
    assert_eq!(adaptive, Duration::from_millis(1600));

    let capped =
        PseudoStreamingSttEngine::preview_interval(16000 * 3, 16000, Duration::from_secs(10));
    assert_eq!(capped, Duration::from_millis(PREVIEW_MAX_INTERVAL_MS));
}

#[test]
fn absolute_uncommitted_hard_limit_does_not_depend_on_vad_state() {
    let engine = engine_without_transport(Vec::new());
    let inner = engine.inner.lock().unwrap();
    assert_eq!(
        inner.exceeds_uncommitted_hard_limit(16_000 * 12, 0, 16_000),
        Some(true)
    );
    assert_eq!(
        inner.exceeds_uncommitted_hard_limit(16_000 * 20, 16_000 * 9, 16_000),
        Some(false)
    );
    assert_eq!(inner.exceeds_uncommitted_hard_limit(10, 11, 16_000), None);
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

#[test]
fn filler_strip_uses_original_unicode_boundaries() {
    // `İ`.to_lowercase() 的 UTF-8 字节长度会增长；旧实现按 lowercased
    // pattern 长度反切原文，可能切进字符内部而 panic。
    assert_eq!(strip_filler_words("你好İYeah."), "你好İ");
    assert_eq!(strip_filler_words("你好İ Yeah."), "你好İ");
}

// ── 引擎 reset 测试 ──

#[test]
fn engine_reset_clears_state() {
    let engine = PseudoStreamingSttEngine {
        inner: Arc::new(Mutex::new({
            let mut inner = PseudoInner::for_test(vec![0.1; 1000]);
            inner.vad.process_chunk(&[0.1; 1600]);
            inner.sentences.append_confirmed("测试");
            inner.last_preview = Instant::now() - Duration::from_secs(10);
            inner.preview_in_flight = true;
            inner.preview_owner = 7;
            inner.next_preview_request = 7;
            inner.latest_preview = "测试预览".to_string();
            inner
        })),
        connection: None,
        sample_rate: 16000,
        boundary_observer: None,
        finalize_observer: None,
    };

    engine.reset();

    let inner = engine.inner.lock().unwrap();
    assert!(!inner.vad.is_speaking());
    assert!(inner.samples.is_empty());
    assert!(inner.latest_preview.is_empty());
    assert!(!inner.preview_in_flight);
    assert_eq!(inner.preview_owner, 0, "reset 必须清空预览 owner");
    assert_eq!(
        inner.preview_generation, 1,
        "reset 应递增 preview_generation"
    );
    assert_eq!(inner.sentences.confirmed_text(), "");
    assert_eq!(inner.sentences.committed_sample_end, 0);
    assert!(inner.sentences.pending.is_none());
    assert!(!inner.sentences.finalize_in_flight);
    assert_eq!(
        inner.last_reported_state, None,
        "reset 后首个 chunk 必须重新上报状态"
    );
}

// 验证带连接快照的引擎能正常构造和 reset
#[test]
fn engine_with_token_constructs_and_resets() {
    let engine = PseudoStreamingSttEngine {
        inner: Arc::new(Mutex::new(PseudoInner::for_test(vec![0.1; 100]))),
        connection: Some(crate::domain::stt::SttEngineConnection {
            host: "127.0.0.1".to_string(),
            port: 8000,
            engine_id: "funasr".to_string(),
            instance_id: "inst-test".to_string(),
            transport: None,
        }),
        sample_rate: 16000,
        boundary_observer: None,
        finalize_observer: None,
    };

    engine.reset();

    let inner = engine.inner.lock().unwrap();
    assert!(inner.samples.is_empty());
    assert_eq!(inner.preview_generation, 1);
}

#[tokio::test]
async fn hard_limit_forces_boundary_for_audio_stuck_outside_vad_speaking() {
    let engine = engine_without_transport(Vec::new());

    // 该幅度低于当前 off threshold，不会进入 VAD speaking；绝对窗口保险
    // 仍必须在 12 秒处制造边界。无 transport 会立即安全 rollback。
    let audio = vec![0.006; 16_000 * 12];
    engine.transcribe_chunk(&audio).await.unwrap();

    let inner = engine.inner.lock().unwrap();
    assert!(!inner.vad.is_speaking());
    assert_eq!(inner.preview_generation, 1, "强制边界必须推进预览代际");
    assert_eq!(inner.last_preview_sample_end, audio.len());
}

// ── 0.23.7: 未提交上限合成场景测试 ──
//
// 这组测试单独证明"未提交音频上限"确实生效且独立于 EnergyVad 的
// 软/硬窗口计时——不能只凭 WAV 识别结果判断它。

/// 合成场景 1：VAD 滞回区徘徊（RMS 长期落在 off/on 阈值之间）。
///
/// VAD 的 speaking 与静默计时都不前进（两种事件都不会发生），
/// 未提交上限必须按绝对未提交音频兜底切分。此处把上限配置为 16s：
/// 13s 滞回音频在旧默认 12s 上限下会切、新上限下必须不切；越过 16s
/// 后必须切——证明上限读取的是配置值而非默认常量。
#[tokio::test]
async fn uncommitted_cap_fires_when_vad_timer_stuck_in_hysteresis() {
    let engine = engine_without_transport(Vec::new());
    {
        let mut inner = engine.inner.lock().unwrap();
        inner.max_uncommitted_audio_ms = 16_000;
    }

    // 正弦幅度 0.0045 → RMS ≈ 0.0032，稳定落在默认阈值 off(≈0.002) 与
    // on(≈0.004) 之间的滞回区：不进 speaking、也不算静默。
    let hysteresis_tone = tone_200ms_chunk(0.0045_f32);
    let chunk_len = hysteresis_tone.len();

    // 喂 13s：默认 12s 上限会在此区间切，配置 16s 上限必须不切。
    for _ in 0..65 {
        engine.transcribe_chunk(&hysteresis_tone).await.unwrap();
    }
    assert_eq!(
        engine.inner.lock().unwrap().preview_generation,
        0,
        "13s 滞回音频在 16s 配置上限下不应产生任何边界（12s 默认会误切）"
    );

    // 继续喂到恰好 16.0s：未提交音频达到 16s，cap 必须兜底触发一次。
    //（不再多喂：无 transport 时 rollback 不推进 committed，后续 chunk
    // 会按设计重复兜底同一区间，见有 transport 场景的单次触发验证。）
    for _ in 0..15 {
        engine.transcribe_chunk(&hysteresis_tone).await.unwrap();
    }
    let inner = engine.inner.lock().unwrap();
    assert_eq!(
        inner.preview_generation, 1,
        "越过 16s 后未提交上限必须产生兜底边界（默认 12s 上限会在 13s 内提前触发）"
    );
    assert_eq!(
        inner.last_preview_sample_end,
        80 * chunk_len,
        "边界必须由未提交上限在 16s 绝对音频处触发"
    );
    assert!(
        !inner.vad.is_speaking(),
        "滞回区音频不应进入 speaking——证明边界与 VAD 状态无关"
    );
}

/// 合成场景 2：持续有声 + 定稿挂起。
///
/// 持续有声时 EnergyVad 的硬窗口（12s）计时正常推进，但未提交上限
/// （此处配置 4s）更早到达——证明 cap 与 VAD 内部计时是两套独立边界。
/// 边界产生后 finalize 请求挂起（定稿滞后），此时 cap 不得重复切分
/// 制造乱序边界，直到定稿完成。
#[tokio::test]
async fn uncommitted_cap_fires_before_hard_window_and_waits_for_pending_finalize() {
    // response 挂起不回复——模拟慢定稿
    let (tx, rx) = oneshot::channel();
    let transport = ControlledTransport::new(vec![rx]);
    let engine = controlled_engine(transport.clone(), Vec::new());
    {
        let mut inner = engine.inner.lock().unwrap();
        inner.max_uncommitted_audio_ms = 4_000;
    }

    // 持续有声（RMS 0.07 远超 on 阈值），每次喂 200ms。
    let speech = tone_200ms_chunk(0.1_f32);

    // 3.9s：cap（4s）与硬窗口（12s）都未到，无边界。
    for _ in 0..19 {
        engine.transcribe_chunk(&speech).await.unwrap();
    }
    assert_eq!(
        engine.inner.lock().unwrap().preview_generation,
        0,
        "3.9s 时任何边界都不应发生"
    );

    // 喂到 4.2s：cap 在 4s 处触发（远早于 12s 硬窗口），建立 pending
    // 并发出定稿请求（挂起）。
    for _ in 0..2 {
        engine.transcribe_chunk(&speech).await.unwrap();
    }
    transport.wait_for_calls_or_fail(1).await;
    assert_eq!(
        engine.inner.lock().unwrap().preview_generation,
        1,
        "4s 处未提交上限应恰好产生一次边界"
    );

    // 继续喂到 5s：定稿挂起期间 cap 不得重复切边界（防乱序），
    // 硬窗口（12s）也未到。
    for _ in 0..4 {
        engine.transcribe_chunk(&speech).await.unwrap();
    }
    let inner = engine.inner.lock().unwrap();
    assert_eq!(inner.preview_generation, 1, "定稿挂起期间不得产生额外边界");
    assert!(
        inner.sentences.pending.is_some() && inner.sentences.finalize_in_flight,
        "挂起的定稿应保持 pending 状态"
    );
    assert_eq!(
        transport.calls.load(Ordering::SeqCst),
        1,
        "只应有一次定稿请求"
    );
    // 显式释放锁再等待后台 task 写状态（std Mutex 不可重入）
    drop(inner);

    // 让挂起的定稿失败返回 → rollback，committed 不推进，链路收敛。
    let _ = tx.send(Err("simulated slow finalize".to_string()));
    wait_until(|| {
        let inner = engine.inner.lock().unwrap();
        !inner.sentences.finalize_in_flight && inner.sentences.pending.is_none()
    })
    .await;
}

/// 合成场景 3：cap 边界与自然句尾交替时坐标不重复、不丢段。
///
/// 正常说话（1s 句 + 静默）触发 SentenceEnd 后，cap 从新的 committed
/// 基点重新计数——证明它按"绝对未提交音频"计算而非自身独立计时器。
#[tokio::test]
async fn uncommitted_cap_resets_with_committed_base() {
    let engine = engine_without_transport(Vec::new());
    {
        let mut inner = engine.inner.lock().unwrap();
        inner.max_uncommitted_audio_ms = 2_000;
    }
    let speech = tone_200ms_chunk(0.1_f32);
    let silence = tone_200ms_chunk(0.0_f32);

    // 1s 语音 + 400ms 静默 → 自然句尾（SentenceEnd）
    for _ in 0..7 {
        engine.transcribe_chunk(&speech).await.unwrap();
    }
    for _ in 0..2 {
        engine.transcribe_chunk(&silence).await.unwrap();
    }
    assert_eq!(
        engine.inner.lock().unwrap().preview_generation,
        1,
        "自然句尾应产生一次边界"
    );

    // 句尾定稿无 transport 立即 rollback（committed 不推进是 0.22.15 语义），
    // 因此未提交音频继续累积，2s 后 cap 兜底再次触发。
    for _ in 0..6 {
        engine.transcribe_chunk(&speech).await.unwrap();
    }
    assert!(
        engine.inner.lock().unwrap().preview_generation >= 2,
        "cap 应在新的未提交音频达到 2s 后再次兜底"
    );
}

/// 0.23.7.1 根因回归（引擎级）：轻声语音帧能量落在 on/off 滞回区时，
/// VAD 句长按非静默帧累计，引擎在 300ms 停顿处产生边界并派发定稿
/// （preview_generation 推进）；旧语义只计高于 on 的帧，句长停在起句
/// 爆发的几十毫秒，整句被最短句长保护重置，只能等未提交上限兜底。
#[tokio::test]
async fn soft_voice_pause_finalizes_through_engine_boundary() {
    let engine = engine_without_transport(Vec::new());

    // 安静起底（校准底噪：on≈0.010 / off≈0.005）
    for _ in 0..5 {
        engine
            .transcribe_chunk(&tone_200ms_chunk(0.0_f32))
            .await
            .unwrap();
    }
    // 起句爆发 200ms（RMS≈0.014 > on）进入 speaking
    engine
        .transcribe_chunk(&tone_200ms_chunk(0.02_f32))
        .await
        .unwrap();
    // 轻声主体 1s（RMS≈0.0071，滞回区）
    for _ in 0..5 {
        engine
            .transcribe_chunk(&tone_200ms_chunk(0.010_f32))
            .await
            .unwrap();
    }
    assert!(
        engine.inner.lock().unwrap().vad.is_speaking(),
        "滞回区帧应保持 speaking"
    );
    // 400ms 停顿 → 自然句尾边界（非静默 1.2s ≥ 800ms）
    for _ in 0..2 {
        engine
            .transcribe_chunk(&tone_200ms_chunk(0.0_f32))
            .await
            .unwrap();
    }
    let inner = engine.inner.lock().unwrap();
    assert_eq!(
        inner.preview_generation, 1,
        "轻声句停顿应经引擎产生自然边界并清空预览代际"
    );
    assert_eq!(
        inner.sentences.next_segment_id, 2,
        "边界应建立过 pending segment（无 transport 时立即回滚，但段 id 已分配）"
    );
}

#[tokio::test]
async fn vad_debug_observer_records_actual_natural_boundary() {
    let observer = Arc::new(Mutex::new(Vec::new()));
    let engine = engine_without_transport(Vec::new()).with_boundary_observer(observer.clone());
    for _ in 0..5 {
        engine
            .transcribe_chunk(&tone_200ms_chunk(0.0))
            .await
            .unwrap();
    }
    for _ in 0..6 {
        engine
            .transcribe_chunk(&tone_200ms_chunk(0.02))
            .await
            .unwrap();
    }
    for _ in 0..2 {
        engine
            .transcribe_chunk(&tone_200ms_chunk(0.0))
            .await
            .unwrap();
    }
    let records = observer.lock().unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].reason, "natural_silence");
    assert_eq!(records[0].audio_ms, 2_600); // 引擎按 200ms 喂入块记录实际边界
}

/// 生成 200ms 的 16kHz 单声道音频块（测试辅助）。
fn tone_200ms_chunk(amplitude: f32) -> Vec<f32> {
    let n = 16_000 * 200 / 1000;
    (0..n)
        .map(|i| {
            let t = i as f32 / 16_000.0;
            (2.0 * std::f32::consts::PI * 440.0 * t).sin() * amplitude
        })
        .collect()
}

#[test]
fn reset_recovers_and_clears_poisoned_mutex() {
    let engine = engine_without_transport(Vec::new());
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

// ── 0.23.7: 窗口配置接线测试 ──

/// from_connection 把 SttConfig 的三窗口配置投影到 VAD 实例与未提交上限，
/// 保证设置页保存的值确实控制切片（非仅持久化展示）。
#[test]
fn from_connection_projects_window_config_into_engine() {
    // H 组合：soft=10s / hard=14s / uncommitted=16s
    let config = crate::domain::config::stt_config::SttConfig {
        local_engine: crate::domain::config::stt_config::LocalEngineConfig {
            vad: crate::domain::config::stt_config::VadConfig {
                soft_window_s: 10,
                hard_window_s: 14,
                max_uncommitted_s: 16,
                ..crate::domain::config::stt_config::VadConfig::default()
            },
            ..crate::domain::config::stt_config::LocalEngineConfig::default()
        },
        ..crate::domain::config::stt_config::SttConfig::default()
    };
    let conn = crate::domain::stt::SttEngineConnection {
        host: "127.0.0.1".into(),
        port: 0,
        engine_id: "funasr".into(),
        instance_id: "test-instance".into(),
        transport: Some(ControlledTransport::new(Vec::new())),
    };
    let engine =
        PseudoStreamingSttEngine::from_connection(&config, conn).expect("engine should construct");

    let inner = engine.inner.lock().unwrap();
    assert_eq!(inner.vad.window_ms(), (10_000, 14_000));
    assert_eq!(inner.max_uncommitted_audio_ms, 16_000);

    // 未提交上限判定使用配置值：13s 未提交在配置 16s 上限下不触发，
    // 在旧默认 12s 上限下触发——证明判定读取的是配置而非默认常量。
    let thirteen_s = 16_000 * 13;
    assert_eq!(
        inner.exceeds_uncommitted_hard_limit(thirteen_s, 0, 16_000),
        Some(false),
        "配置 16s 上限时 13s 未提交不应触发（12s 默认上限会误触发）"
    );
    assert_eq!(
        inner.exceeds_uncommitted_hard_limit(16_000 * 16, 0, 16_000),
        Some(true)
    );
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
            commit_generation: state.commit_generation,
            segment_id: 1,
        },
        text: "第一句".to_string(),
        ok: true,
    };
    let deferred = state.commit_or_rollback(&r1);
    assert!(deferred.is_some(), "应返回 deferred segment");
    assert!(state.pending.is_some(), "deferred 应已转为 pending");
    assert!(state.finalize_in_flight, "finalize_in_flight 应为 true");
    let deferred = deferred.unwrap();
    assert_eq!(deferred.range, 1000..2000, "不得重复识别已提交区间");

    let before = state.committed_sample_end;
    assert!(
        state
            .commit_or_rollback(&FinalizeResult {
                identity: deferred.identity,
                text: "第二句".to_string(),
                ok: true,
            })
            .is_none()
    );
    assert!(state.committed_sample_end >= before);
    assert_eq!(state.committed_sample_end, 2000);
    assert_eq!(state.confirmed_text(), "第一句第二句");
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
            commit_generation: state.commit_generation,
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

#[tokio::test]
async fn finalize_waits_for_in_flight_commit_before_timeout() {
    let (tx, rx) = oneshot::channel();
    let transport = ControlledTransport::new(vec![rx]);
    let samples = vec![0.1; 1600];
    let engine = controlled_engine(transport.clone(), samples.clone());
    let pending = {
        let mut inner = engine.inner.lock().unwrap();
        inner.sentences.on_sentence_end(samples.len(), "").unwrap()
    };
    engine.spawn_sentence_finalize(samples, pending.identity);
    transport.wait_for_calls(1).await;

    let finalize = engine.finalize_with_wait_timeout(Duration::from_secs(3));
    tx.send(Ok("第一句".into())).unwrap();
    let text = finalize.await.unwrap();

    assert_eq!(text, "第一句");
    let inner = engine.inner.lock().unwrap();
    assert_eq!(inner.sentences.committed_sample_end, 1600);
    assert_eq!(inner.sentences.confirmed_text(), "第一句");
}

#[tokio::test]
async fn finalize_timeout_revokes_late_segment_commit_right() {
    let (old_tx, old_rx) = oneshot::channel();
    let (terminal_tx, terminal_rx) = oneshot::channel();
    let transport = ControlledTransport::new(vec![old_rx, terminal_rx]);
    let samples = vec![0.1; 1600];
    let engine = Arc::new(controlled_engine(transport.clone(), samples.clone()));
    let pending = {
        let mut inner = engine.inner.lock().unwrap();
        inner
            .sentences
            .on_sentence_end(samples.len(), "旧预览")
            .unwrap()
    };
    engine.spawn_sentence_finalize(samples, pending.identity);
    transport.wait_for_calls(1).await;

    let task = {
        let engine = Arc::clone(&engine);
        tokio::spawn(async move {
            engine
                .finalize_with_wait_timeout(Duration::from_millis(20))
                .await
        })
    };
    transport.wait_for_calls(2).await;
    terminal_tx.send(Ok("完整尾段".into())).unwrap();
    assert_eq!(task.await.unwrap().unwrap(), "完整尾段");

    old_tx.send(Ok("迟到旧段".into())).unwrap();
    tokio::task::yield_now().await;
    let inner = engine.inner.lock().unwrap();
    assert_eq!(inner.sentences.confirmed_text(), "完整尾段");
    assert_eq!(inner.sentences.committed_sample_end, 1600);
}

#[tokio::test]
async fn terminal_takeover_invalidates_pending_and_deferred_together() {
    let mut state = SentenceState::new();
    let first = state.on_sentence_end(1000, "p1").unwrap();
    state.finalize_in_flight = true;
    assert!(state.on_sentence_end(2000, "p2").is_none());
    assert!(state.pending.is_some() && state.deferred.is_some());

    let terminal = state.begin_terminal_finalize();
    assert!(state.pending.is_none() && state.deferred.is_none());
    assert!(!state.finalize_in_flight);
    assert!(
        state
            .commit_or_rollback(&FinalizeResult {
                identity: first.identity,
                text: "迟到".into(),
                ok: true,
            })
            .is_none()
    );
    assert!(state.commit_terminal_finalize(terminal, 2000, "完整"));
    assert_eq!(state.confirmed_text(), "完整");
    assert_eq!(state.committed_sample_end, 2000);
}

#[tokio::test]
async fn reset_during_terminal_finalize_discards_old_result() {
    let (tx, rx) = oneshot::channel();
    let transport = ControlledTransport::new(vec![rx]);
    let engine = Arc::new(controlled_engine(transport.clone(), vec![0.1; 1600]));
    let task = {
        let engine = Arc::clone(&engine);
        tokio::spawn(async move {
            engine
                .finalize_with_wait_timeout(Duration::from_millis(20))
                .await
        })
    };
    transport.wait_for_calls(1).await;
    engine.reset();
    tx.send(Ok("旧 session".into())).unwrap();

    assert!(task.await.unwrap().is_err());
    let inner = engine.inner.lock().unwrap();
    assert!(inner.sentences.confirmed_text().is_empty());
    assert_eq!(inner.sentences.committed_sample_end, 0);
    assert!(inner.samples.is_empty());
}

// ── 0.24: 预览生命周期（preview_in_flight 泄漏）、状态边沿触发、可观测性 ──

/// 回归：句尾使在途预览过期后，预览任务返回时必须释放 `preview_in_flight`。
///
/// 旧实现只在 `preview_generation` 相等时清除该标志，句尾递增代际后
/// 标志永久为 true，`should_preview` 再也不会成立——表现为"录制一两句后
/// 预览永久停止"。
#[tokio::test]
async fn preview_in_flight_released_after_sentence_boundary() {
    let (tx, rx) = oneshot::channel();
    let transport = ControlledTransport::new(vec![rx]);
    let engine = Arc::new(controlled_engine(transport.clone(), vec![]));

    engine.spawn_preview_recognition(vec![0.1; 1600], 1600, AudioRange::new(0, 1600));
    transport.wait_for_calls_or_fail(1).await;
    assert!(
        engine.inner.lock().unwrap().preview_in_flight,
        "预览应在飞行中"
    );

    // 模拟句尾：仅代际推进（句尾对预览所有权的影响就在这里）
    {
        let mut inner = engine.inner.lock().unwrap();
        inner.preview_generation = inner.preview_generation.wrapping_add(1);
    }

    tx.send(Ok("过期预览".into())).unwrap();
    wait_until(|| !engine.inner.lock().unwrap().preview_in_flight).await;

    let inner = engine.inner.lock().unwrap();
    assert!(
        inner.latest_preview.is_empty(),
        "过期预览不得写入 latest_preview"
    );
    assert_eq!(inner.preview_owner, 0, "释放后 owner token 必须归零");
}

/// 回归：预览过期后必须能重新启动——即 `should_preview` 条件重新成立。
#[tokio::test]
async fn preview_restarts_after_stale_preview_released() {
    let (tx1, rx1) = oneshot::channel();
    let (tx2, rx2) = oneshot::channel();
    let transport = ControlledTransport::new(vec![rx1, rx2]);
    let engine = Arc::new(controlled_engine(transport.clone(), vec![]));

    // 第一轮：低于 VAD off threshold 的音频，累积 8000 样本并让预览间隔到期
    {
        let mut inner = engine.inner.lock().unwrap();
        inner.last_preview = Instant::now() - Duration::from_millis(600);
    }
    engine.transcribe_chunk(&[0.006; 8_000]).await.unwrap();
    transport.wait_for_calls_or_fail(1).await;
    assert!(engine.inner.lock().unwrap().preview_in_flight);

    // 句尾使其过期
    {
        let mut inner = engine.inner.lock().unwrap();
        inner.preview_generation = inner.preview_generation.wrapping_add(1);
    }
    tx1.send(Ok("过期预览".into())).unwrap();
    wait_until(|| !engine.inner.lock().unwrap().preview_in_flight).await;

    // 第二轮：同样的音频 + 间隔再次到期 → 必须能启动新一轮预览
    {
        let mut inner = engine.inner.lock().unwrap();
        inner.last_preview = Instant::now() - Duration::from_millis(600);
    }
    engine.transcribe_chunk(&[0.006; 8_000]).await.unwrap();
    transport.wait_for_calls_or_fail(2).await;

    tx2.send(Ok("新预览".into())).unwrap();
    wait_until(|| engine.inner.lock().unwrap().latest_preview == "新预览").await;
}

/// 旧预览任务不得清除新一轮预览的所有权状态。
#[tokio::test]
async fn stale_preview_task_cannot_clear_new_preview_owner() {
    let (tx1, rx1) = oneshot::channel();
    let (tx2, rx2) = oneshot::channel();
    let transport = ControlledTransport::new(vec![rx1, rx2]);
    let engine = Arc::new(controlled_engine(transport.clone(), vec![]));

    engine.spawn_preview_recognition(vec![0.1; 1600], 1600, AudioRange::new(0, 1600));
    transport.wait_for_calls_or_fail(1).await;
    // 句尾后启动新一轮（模拟 owner 已被新请求接管）
    {
        let mut inner = engine.inner.lock().unwrap();
        inner.preview_generation = inner.preview_generation.wrapping_add(1);
    }
    engine.spawn_preview_recognition(vec![0.1; 1600], 1600, AudioRange::new(0, 1600));
    transport.wait_for_calls_or_fail(2).await;

    tx1.send(Ok("旧预览".into())).unwrap();
    // 给旧任务足够时间跑完（不能靠断言"未被清除"来等待）
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
    {
        let inner = engine.inner.lock().unwrap();
        assert!(inner.preview_in_flight, "旧任务不得清除新任务的 in_flight");
        assert!(
            inner.latest_preview.is_empty(),
            "旧任务不得写入 latest_preview"
        );
    }

    tx2.send(Ok("新预览".into())).unwrap();
    wait_until(|| engine.inner.lock().unwrap().latest_preview == "新预览").await;
}

/// 状态未变化的音频块不得产生对外状态（返回空串 = 无事件）。
#[tokio::test]
async fn transcribe_chunk_suppresses_unchanged_state() {
    let engine = engine_without_transport(Vec::new());

    for _ in 0..5 {
        let out = engine.transcribe_chunk(&[0.0; 160]).await.unwrap();
        assert_eq!(out, "", "状态未变化时必须返回空串（不发送状态）");
    }
}

/// confirmed 变化必须上报且只上报一次；重复的同一状态被抑制。
#[tokio::test]
async fn transcribe_chunk_reports_confirmed_change_once() {
    let (tx, rx) = oneshot::channel();
    let transport = ControlledTransport::new(vec![rx]);
    let engine = Arc::new(controlled_engine(transport.clone(), vec![0.1; 1600]));

    let pending = {
        let mut inner = engine.inner.lock().unwrap();
        let pending = inner.sentences.on_sentence_end(1600, "").unwrap();
        inner.vad.reset_sentence();
        pending
    };
    engine.spawn_sentence_finalize(vec![0.1; 1600], pending.identity);
    transport.wait_for_calls_or_fail(1).await;
    tx.send(Ok("第一句。".into())).unwrap();
    wait_until(|| engine.inner.lock().unwrap().sentences.confirmed_text() == "第一句。").await;

    let first = engine.transcribe_chunk(&[0.0; 160]).await.unwrap();
    let v: serde_json::Value = serde_json::from_str(&first).expect("变化时必须返回 JSON 快照");
    assert_eq!(v["confirmed"], "第一句。");
    assert_eq!(v["confirmed_changed"], true);
    assert!(v["revision"].as_u64().unwrap() > 0, "必须携带状态版本号");

    // 同一状态重复调用 → 抑制
    assert_eq!(engine.transcribe_chunk(&[0.0; 160]).await.unwrap(), "");
}

/// 诊断快照暴露 PCM 大小与在途推理任务数（不含正文）。
#[test]
fn stream_stats_reports_pcm_and_inflight() {
    let engine = engine_without_transport(vec![0.1; 4000]);
    {
        let mut inner = engine.inner.lock().unwrap();
        inner.preview_in_flight = true;
        inner.sentences.finalize_in_flight = true;
    }

    let stats = engine.stream_stats();
    assert_eq!(stats.pcm_samples, 4000);
    assert!(stats.preview_in_flight);
    assert!(stats.finalize_in_flight);
    assert_eq!(stats.in_flight_inferences, 2);
}

// ── 0.23.7.2 D：强制切谷底回退的引擎级不变量测试 ──
//
// 证明 SentenceState 在 on_sentence_end 收到"回退的历史边界"（非当前
// 绝对末端）时，pending/deferred/finalize/terminal/compact 各状态下
// 提交区间单调、无重复提交、无丢段、缓冲有界。

/// 合成指定毫秒数的 440Hz 音（引擎测试粒度）。
fn valley_tone(ms: u32, amp: f32) -> Vec<f32> {
    let n = 16_000u32 * ms / 1000;
    (0..n)
        .map(|i| {
            let t = i as f32 / 16_000.0;
            (2.0 * std::f32::consts::PI * 440.0 * t).sin() * amp
        })
        .collect()
}

fn valley_silence(ms: u32) -> Vec<f32> {
    vec![0.0; 16_000u32 as usize * ms as usize / 1000]
}

fn valley_concat(parts: &[Vec<f32>]) -> Vec<f32> {
    let mut out = Vec::new();
    for p in parts {
        out.extend_from_slice(p);
    }
    out
}

/// 11.5s 有声 + 100ms 谷底 + 尾部有声：硬窗口在 ~12.03s 触发，
/// 谷底回退应把切点移到 11.6s（谷底末帧，11600ms*16=185600 样本）。
fn valley_audio(tail_ms: u32) -> Vec<f32> {
    valley_concat(&[
        valley_tone(11_500, 0.1),
        valley_silence(100),
        valley_tone(tail_ms, 0.1),
    ])
}

/// 解析 pcm_to_wav 产物（16k/mono/16bit）data chunk 的时长（ms）。
fn valley_wav_ms(wav: &[u8]) -> u64 {
    let mut pos = 12usize;
    while pos + 8 <= wav.len() {
        let size =
            u32::from_le_bytes([wav[pos + 4], wav[pos + 5], wav[pos + 6], wav[pos + 7]]) as usize;
        if &wav[pos..pos + 4] == b"data" {
            return size as u64 / 32; // 16000Hz*2B = 32 字节/ms
        }
        pos += 8 + size + (size & 1);
    }
    panic!("wav 缺少 data chunk");
}

/// 记录每次 transcribe WAV 时长的受控 transport（响应仍由 oneshot 提供）。
struct WavCaptureTransport {
    responses: Mutex<VecDeque<oneshot::Receiver<Result<String, String>>>>,
    wav_ms: Mutex<Vec<u64>>,
    calls: AtomicUsize,
    called: Notify,
}

impl WavCaptureTransport {
    fn new(responses: Vec<oneshot::Receiver<Result<String, String>>>) -> Arc<Self> {
        Arc::new(Self {
            responses: Mutex::new(responses.into()),
            wav_ms: Mutex::new(Vec::new()),
            calls: AtomicUsize::new(0),
            called: Notify::new(),
        })
    }

    fn captured_ms(&self) -> Vec<u64> {
        self.wav_ms.lock().unwrap().clone()
    }

    async fn wait_for_calls(&self, expected: usize) {
        while self.calls.load(Ordering::SeqCst) < expected {
            self.called.notified().await;
        }
    }
}

#[async_trait::async_trait]
impl crate::domain::stt::SttTransport for WavCaptureTransport {
    async fn check_ready(&self) -> Result<(), crate::domain::stt::SttTransportError> {
        Ok(())
    }

    async fn transcribe(
        &self,
        wav_bytes: &[u8],
    ) -> Result<String, crate::domain::stt::SttTransportError> {
        self.wav_ms.lock().unwrap().push(valley_wav_ms(wav_bytes));
        let rx = self
            .responses
            .lock()
            .unwrap()
            .pop_front()
            .expect("测试必须提供 transport response");
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.called.notify_waiters();
        rx.await
            .expect("测试 response sender 不应提前 drop")
            .map_err(|detail| crate::domain::stt::SttTransportError::Unavailable { detail })
    }
}

/// 构造带捕获 transport 的引擎，并禁用定时预览（预览会竞争 transport 响应）。
fn valley_engine(transport: Arc<WavCaptureTransport>) -> PseudoStreamingSttEngine {
    let engine = PseudoStreamingSttEngine {
        inner: Arc::new(Mutex::new(PseudoInner::for_test(Vec::new()))),
        connection: Some(crate::domain::stt::SttEngineConnection {
            host: "127.0.0.1".into(),
            port: 0,
            engine_id: "funasr".into(),
            instance_id: "valley-test".into(),
            transport: Some(transport),
        }),
        sample_rate: 16_000,
        boundary_observer: None,
        finalize_observer: None,
    };
    engine.inner.lock().unwrap().last_preview = Instant::now() + Duration::from_secs(600);
    engine
}

/// 回退边界提交后区间单调、无重复、无丢段；terminal 覆盖余下全部音频。
#[tokio::test]
async fn hard_cut_retreats_to_valley_and_covers_all_audio() {
    let (tx1, rx1) = oneshot::channel();
    let (tx2, rx2) = oneshot::channel();
    let transport = WavCaptureTransport::new(vec![rx1, rx2]);
    let engine = std::sync::Arc::new(valley_engine(transport.clone()));

    // 12.2s：未提交上限在 12.0s 兜底触发，谷底回退到 11600ms；
    // 该段 pending = [0, 185600)，尾随 100ms 谷底静音被裁剪。
    let audio = valley_audio(600);
    for chunk in audio.chunks(1_600) {
        engine.transcribe_chunk(chunk).await.unwrap();
    }
    transport.wait_for_calls(1).await;
    tx1.send(Ok("第一段。".into())).unwrap();
    wait_until(|| engine.inner.lock().unwrap().sentences.committed_sample_end == 185_600).await;

    let finalize_engine = std::sync::Arc::clone(&engine);
    let task = tokio::spawn(async move {
        finalize_engine
            .finalize_with_wait_timeout(Duration::from_secs(3))
            .await
    });
    transport.wait_for_calls(2).await;
    tx2.send(Ok("第二段。".into())).unwrap();
    let final_text = task.await.unwrap().unwrap();

    let inner = engine.inner.lock().unwrap();
    assert_eq!(inner.sentences.confirmed_text(), "第一段。第二段。");
    assert_eq!(inner.sentences.committed_sample_end, 195_200);
    assert_eq!(final_text, "第一段。第二段。");
    let ms = transport.captured_ms();
    assert_eq!(ms.len(), 2, "恰好一次回退段定稿 + 一次 terminal");
    // 回退段 [0,11600)——100ms 谷底短于 150ms 尾缓冲不被裁剪；
    // terminal [11600,12200) = 600ms
    assert_eq!(ms[0], 11_600);
    assert_eq!(ms[1], 600);
}

/// 强制切点回退后，保留在 PCM 中的尾部语音必须重新进入 VAD 状态；
/// 随后的语音与 300ms 停顿合计达到最短句长时，应产生 deferred 的自然边界。
#[tokio::test]
async fn retreated_tail_replay_allows_deferred_natural_boundary() {
    let (tx1, rx1) = oneshot::channel();
    let (tx2, rx2) = oneshot::channel();
    let transport = WavCaptureTransport::new(vec![rx1, rx2]);
    let boundaries = Arc::new(Mutex::new(Vec::new()));
    let finalize_records = Arc::new(Mutex::new(Vec::new()));
    let engine = std::sync::Arc::new(
        valley_engine(transport.clone())
            .with_boundary_observer(boundaries.clone())
            .with_finalize_observer(finalize_records.clone()),
    );
    {
        let mut inner = engine.inner.lock().unwrap();
        // 700ms speech + 100ms valley + 500ms speech = 1.3s；
        // 用较小 cap 复现生产的未提交上限回退路径。
        inner.max_uncommitted_audio_ms = 1_300;
    }

    let first = valley_concat(&[
        valley_tone(700, 0.1),
        valley_silence(100),
        valley_tone(500, 0.1),
    ]);
    for chunk in first.chunks(1_600) {
        engine.transcribe_chunk(chunk).await.unwrap();
    }
    transport.wait_for_calls(1).await;
    {
        let inner = engine.inner.lock().unwrap();
        let pending = inner.sentences.pending.as_ref().expect("首段应已 pending");
        assert_eq!(pending.range, 0..12_800, "首段应回退到 800ms 谷底");
        // 回退后重放的 500ms 尾音已经进入下一句状态；若未重放，
        // 此处 sentence_samples 会是 0，后续短语音将无法满足 800ms。
        let state = inner.vad.dump_state();
        assert!(state.speaking, "回退尾音应保持 speaking");
        assert_eq!(state.sentence_samples, 8_000, "应重放 500ms 尾音");
        assert_eq!(state.segment_samples, 8_000, "段长也应从真实切点重建");
    }

    // 首段 finalize 在途时触发第二个边界，必须进入 deferred。
    let continuation = valley_concat(&[valley_tone(400, 0.1), valley_silence(300)]);
    for chunk in continuation.chunks(1_600) {
        engine.transcribe_chunk(chunk).await.unwrap();
    }
    {
        let inner = engine.inner.lock().unwrap();
        let deferred = inner
            .sentences
            .deferred
            .as_ref()
            .expect("第二段应 deferred");
        assert_eq!(deferred.range, 0..32_000, "自然边界应位于 2s");
    }

    tx1.send(Ok("第一段。".into())).unwrap();
    transport.wait_for_calls(2).await;
    tx2.send(Ok("第二段。".into())).unwrap();
    wait_until(|| engine.inner.lock().unwrap().sentences.confirmed_text() == "第一段。第二段。")
        .await;

    let observed_boundaries = boundaries.lock().unwrap().clone();
    assert_eq!(
        observed_boundaries
            .iter()
            .map(|record| (record.audio_ms, record.reason))
            .collect::<Vec<_>>(),
        vec![(800, "uncommitted_cap_valley"), (2_000, "natural_silence")]
    );

    let observed_finalize = finalize_records.lock().unwrap().clone();
    assert_eq!(
        observed_finalize
            .iter()
            .map(|record| (record.phase, record.segment_id))
            .collect::<Vec<_>>(),
        vec![
            ("created", 1),
            ("transport_start", 1),
            ("created", 2),
            ("transport_start", 2),
        ]
    );
    assert!(
        observed_finalize[0].observed_at <= observed_finalize[1].observed_at
            && observed_finalize[1].observed_at <= observed_finalize[2].observed_at
            && observed_finalize[2].observed_at <= observed_finalize[3].observed_at,
        "定稿阶段时间戳必须保持创建→请求顺序"
    );
}

/// 第一个（回退）定稿在途时第二个回退边界进 deferred；前段 commit 后
/// deferred 起点重定位到已提交末端——区间连续单调，不重复提交 [0,V1)。
#[tokio::test]
async fn retreated_deferred_boundary_keeps_monotone_coverage() {
    let (tx1, rx1) = oneshot::channel();
    let (tx2, rx2) = oneshot::channel();
    let (tx3, rx3) = oneshot::channel();
    let transport = WavCaptureTransport::new(vec![rx1, rx2, rx3]);
    let engine = std::sync::Arc::new(valley_engine(transport.clone()));

    // 两段 12.2s 模式：cap 边界1 回退到 11600ms；回退尾音重放后，
    // 下一段硬窗口从真实切点计满 12s，于 23600ms 产生 deferred。
    let audio = valley_concat(&[valley_audio(600), valley_audio(600)]);
    for chunk in audio.chunks(1_600) {
        engine.transcribe_chunk(chunk).await.unwrap();
    }
    transport.wait_for_calls(1).await;
    {
        let inner = engine.inner.lock().unwrap();
        assert!(inner.sentences.deferred.is_some(), "边界2 应暂存 deferred");
        assert_eq!(inner.sentences.deferred.as_ref().unwrap().range, 0..377_600);
    }

    tx1.send(Ok("第一段。".into())).unwrap();
    // commit1 推进到回退点 185600，deferred 提升为 pending 并派发段2
    wait_until(|| engine.inner.lock().unwrap().sentences.committed_sample_end == 185_600).await;
    transport.wait_for_calls(2).await;
    tx2.send(Ok("第二段。".into())).unwrap();
    wait_until(|| {
        let inner = engine.inner.lock().unwrap();
        inner.sentences.committed_sample_end == 377_600
            && inner.sentences.confirmed_text() == "第一段。第二段。"
    })
    .await;

    let finalize_engine = std::sync::Arc::clone(&engine);
    let task = tokio::spawn(async move {
        finalize_engine
            .finalize_with_wait_timeout(Duration::from_secs(3))
            .await
    });
    transport.wait_for_calls(3).await;
    tx3.send(Ok("第三段。".into())).unwrap();
    let _ = task.await.unwrap().unwrap();

    let inner = engine.inner.lock().unwrap();
    assert_eq!(inner.sentences.confirmed_text(), "第一段。第二段。第三段。");
    let ms = transport.captured_ms();
    // 段2 必须从 11600ms 起（12000ms 长），证明 deferred 重定位生效、
    // 没有重复提交 [0,11600)
    assert_eq!(ms, vec![11_600, 12_000, 800]);
}

/// 回退段定稿失败回滚后 committed 不动；terminal 重新覆盖全部未提交音频。
#[tokio::test]
async fn retreated_rollback_lets_terminal_recover_everything() {
    let (tx1, rx1) = oneshot::channel();
    let (tx2, rx2) = oneshot::channel();
    let transport = WavCaptureTransport::new(vec![rx1, rx2]);
    let engine = std::sync::Arc::new(valley_engine(transport.clone()));

    let audio = valley_audio(600);
    for chunk in audio.chunks(1_600) {
        engine.transcribe_chunk(chunk).await.unwrap();
    }
    transport.wait_for_calls(1).await;
    tx1.send(Err("worker 暂不可用".into())).unwrap();
    wait_until(|| {
        let inner = engine.inner.lock().unwrap();
        inner.sentences.pending.is_none() && !inner.sentences.finalize_in_flight
    })
    .await;
    assert_eq!(
        engine.inner.lock().unwrap().sentences.committed_sample_end,
        0,
        "回滚不得推进 committed"
    );

    let finalize_engine = std::sync::Arc::clone(&engine);
    let task = tokio::spawn(async move {
        finalize_engine
            .finalize_with_wait_timeout(Duration::from_secs(3))
            .await
    });
    transport.wait_for_calls(2).await;
    tx2.send(Ok("恢复段。".into())).unwrap();
    let _ = task.await.unwrap().unwrap();

    let ms = transport.captured_ms();
    // terminal 从 0 覆盖全部 12200ms（回退段失败被回滚），无丢段
    assert_eq!(ms, vec![11_600, 12_200]);
    assert_eq!(
        engine.inner.lock().unwrap().sentences.confirmed_text(),
        "恢复段。"
    );
}

/// 回退段定稿在途时 terminal 接管：旧结果被代际丢弃，terminal 从
/// committed 起覆盖全部，无丢段。
#[tokio::test]
async fn terminal_takeover_discards_retreated_pending_without_loss() {
    let (tx1, rx1) = oneshot::channel();
    let (tx2, rx2) = oneshot::channel();
    let transport = WavCaptureTransport::new(vec![rx1, rx2]);
    let engine = std::sync::Arc::new(valley_engine(transport.clone()));

    let audio = valley_audio(600);
    for chunk in audio.chunks(1_600) {
        engine.transcribe_chunk(chunk).await.unwrap();
    }
    transport.wait_for_calls(1).await;

    // 终态接管：20ms 等待内 in-flight 不返回 → terminal 获得提交权
    let finalize_engine = std::sync::Arc::clone(&engine);
    let task = tokio::spawn(async move {
        finalize_engine
            .finalize_with_wait_timeout(Duration::from_millis(20))
            .await
    });
    transport.wait_for_calls(2).await;
    tx2.send(Ok("终稿。".into())).unwrap();
    let text = task.await.unwrap().unwrap();
    assert!(text.contains("终稿。"));

    // 旧（回退）段结果此刻才返回：必须被丢弃
    tx1.send(Ok("迟到段。".into())).unwrap();
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
    let inner = engine.inner.lock().unwrap();
    assert_eq!(inner.sentences.confirmed_text(), "终稿。");
    assert!(inner.sentences.pending.is_none());
    let ms = transport.captured_ms();
    // terminal 覆盖 [0,12200)，旧段请求虽已发出但结果被丢弃（不重复入账）
    assert_eq!(ms, vec![11_600, 12_200]);
}

/// 回退候选不高于 committed 时必须放弃回退，保持当前时刻兜底，
/// 不得产生空/负区间。生产两条强制切路径都要求 12s 未提交/段长，
/// 数学上候选恒大于 committed；此处的钳制是防御性验证——用物理合法
/// 的构造（committed ≤ 当前 total，cap 上限放大以防提前兜底）触发。
#[tokio::test]
async fn retreat_candidate_at_or_below_committed_falls_back() {
    let engine = engine_without_transport(Vec::new());
    let observer = Arc::new(Mutex::new(Vec::new()));
    let engine = engine.with_boundary_observer(observer.clone());
    {
        let mut inner = engine.inner.lock().unwrap();
        inner.last_preview = Instant::now() + Duration::from_secs(600);
        inner.max_uncommitted_audio_ms = 30_000;
    }

    // 12300ms：VAD 硬窗口在 ~12030ms（chunk #121，[12000,12100)）触发；
    // 谷底候选 11600ms。在触发 chunk 之前把 committed 设为 185601
    // （略高于候选、低于当前 total），模拟回滚后长 speaking 的偏置。
    let audio = valley_audio(700);
    let chunks: Vec<&[f32]> = audio.chunks(1_600).collect();
    for chunk in &chunks[..120] {
        engine.transcribe_chunk(chunk).await.unwrap();
    }
    engine.inner.lock().unwrap().sentences.committed_sample_end = 185_601;
    for chunk in &chunks[120..] {
        engine.transcribe_chunk(chunk).await.unwrap();
    }

    let records = observer.lock().unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].reason, "hard_window", "无合格回退时保持原兜底");
    assert_eq!(records[0].audio_ms, 12_100);
    drop(records);
    let inner = engine.inner.lock().unwrap();
    assert!(
        inner.sentences.pending.is_none(),
        "无 transport 回滚后无 pending"
    );
    assert_eq!(inner.sentences.committed_sample_end, 185_601);
}

/// 回退边界 commit 后 compact 只回收已提交前缀；下一段从回退点起步。
#[test]
fn sentence_state_compact_after_retreated_boundary() {
    let mut state = SentenceState::new();
    let pending = state.on_sentence_end(185_600, "").unwrap();
    assert_eq!(pending.range, 0..185_600);
    let result = FinalizeResult {
        identity: pending.identity,
        text: "段一".to_string(),
        ok: true,
    };
    state.commit_or_rollback(&result);

    let drain = state.try_compact(387_200).unwrap().unwrap();
    assert_eq!(drain, 185_600);
    assert_eq!(state.buffer_base_sample, 185_600);

    let next = state.on_sentence_end(379_200, "").unwrap();
    assert_eq!(
        next.range,
        185_600..379_200,
        "回退点之后的下一段必须从已提交末端起步，无交叠无丢失"
    );
}
