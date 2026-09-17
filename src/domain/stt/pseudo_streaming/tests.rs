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
        decision_observer: None,
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
        decision_observer: None,
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

/// 应答一个推理调用后等待进展：committed 水位推进（句定稿落地）或
/// 下一个推理调用到达（短语/预览应答只进预览账本、不推进 committed）。
async fn wait_committed_or_next_call(
    engine: &PseudoStreamingSttEngine,
    transport: &ControlledTransport,
    committed_before: usize,
    answered: usize,
) {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if engine.stream_stats().pcm_committed_end > committed_before
                || transport.calls.load(Ordering::SeqCst) > answered
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("应答后既未推进 committed 也未等到下一个推理调用");
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

#[test]
fn silence_marker_strip_covers_standalone_suffix_and_repeats() {
    // 稳态噪声段的 "/sil" 标记不得进入 Draft/最终文本
    assert_eq!(strip_filler_words("/sil"), "");
    assert_eq!(strip_filler_words("/sil/sil"), "");
    assert_eq!(strip_filler_words("风扇背景。/sil"), "风扇背景。");
    assert_eq!(strip_filler_words("可能会比较长。/sil。"), "可能会比较长。");
    // 无标记的纯英文识别不受影响
    assert_eq!(strip_filler_words("yes"), "yes");
}

// ── 0.23.9 PreviewDraft 引擎回归 ──

/// 构造默认参数（VAD A + PreviewDraft）的受控引擎。
fn preview_draft_engine(transport: Arc<ControlledTransport>) -> PseudoStreamingSttEngine {
    let conn = crate::domain::stt::SttEngineConnection {
        host: "127.0.0.1".into(),
        port: 0,
        engine_id: "funasr".into(),
        instance_id: "pd-test".into(),
        transport: Some(transport),
    };
    PseudoStreamingSttEngine::from_connection_with_profile(
        &crate::domain::config::stt_config::SttConfig::default(),
        conn,
        RecognitionProfile::PreviewDraft,
    )
    .expect("engine constructs")
}

/// Draft 提交推进 committed 水位后，backlog 必须按未提交音频计算：
/// 超过 2 × max_uncommitted = 24s 硬限的长会话逐段提交不得触发
/// stt_overloaded。修复前协调器 committed 水位不随 Draft 提交推进，
/// backlog 退化为会话总时长，24s 硬限会在正常听写中误触发。
///
/// 0.23.13 时间冻结兜底使每轮调用数浮动：短语锚点停在上轮边界，
/// 滚动窗前缀跨轮积满 1.2s 冻结线后，句定稿之外还会出现短语调用。
/// 应答循环消费所有已到调用，直到本轮 Draft 提交（committed 水位
/// 推进）才进入下一轮；短语/预览应答只进预览账本，最终文本仍只由
/// 各轮 Draft 按提交顺序拼接。
#[tokio::test]
async fn preview_draft_long_session_does_not_overload_after_draft_commits() {
    let channels: Vec<(
        oneshot::Sender<Result<String, String>>,
        oneshot::Receiver<Result<String, String>>,
    )> = (0..24).map(|_| oneshot::channel()).collect();
    let (mut senders, receivers): (Vec<_>, Vec<_>) = channels.into_iter().unzip();
    let transport = ControlledTransport::new(receivers);
    let engine = preview_draft_engine(transport.clone());

    let speech = vec![0.1f32; 16_000 * 32 / 10];
    let pause = vec![0.0f32; 16_000 * 8 / 10];
    let mut committed_before = 0usize;
    let mut answered = 0usize;
    for round in 0..8 {
        for source in [&speech, &pause] {
            for chunk in source.chunks(160) {
                engine
                    .transcribe_chunk(chunk)
                    .await
                    .expect("长会话喂入不得过载");
            }
        }
        while engine.stream_stats().pcm_committed_end <= committed_before {
            answered += 1;
            transport.wait_for_calls_or_fail(answered).await;
            senders
                .remove(0)
                .send(Ok(format!("第{}段。", round + 1)))
                .expect("response sender 不应泄漏");
            // 短语/预览应答不推进 committed；等结果落地或下一个调用
            // 到达后循环继续应答（该轮句定稿最后到达并推进水位）。
            wait_committed_or_next_call(&engine, &transport, committed_before, answered).await;
        }
        committed_before = engine.stream_stats().pcm_committed_end;
    }

    let final_text = engine.finalize().await.expect("finalize ok");
    assert_eq!(
        final_text,
        "第1段。第2段。第3段。第4段。第5段。第6段。第7段。第8段。"
    );
}

/// 模型对噪声段返回 "/sil"：剥离后为空 → 按 NoSpeech 消费 owned range，
/// 不产生 span、不进 confirmed，但 committed 水位推进（避免反复重试）。
#[tokio::test]
async fn preview_draft_silence_marker_result_consumes_without_span() {
    let (sender, receiver) = oneshot::channel();
    let transport = ControlledTransport::new(vec![receiver]);
    let engine = preview_draft_engine(transport.clone());

    let speech = vec![0.1f32; 16_000 * 5 / 2];
    let pause = vec![0.0f32; 16_000 * 8 / 10];
    for source in [&speech, &pause] {
        for chunk in source.chunks(160) {
            engine.transcribe_chunk(chunk).await.expect("chunk ok");
        }
    }
    transport.wait_for_calls_or_fail(1).await;
    sender
        .send(Ok("/sil".to_string()))
        .expect("sender 不应泄漏");
    wait_until(|| engine.stream_stats().pcm_committed_end > 0).await;

    let final_text = engine.finalize().await.expect("finalize ok");
    assert_eq!(final_text, "", "静音标记不得进入最终文本");
    assert!(
        engine.stream_stats().pcm_committed_end >= 16_000 * 5 / 2,
        "NoSpeech 消费必须推进 committed 水位"
    );
}

/// 0.23.9.10 预览定稿：短停顿候选被复语作废 → 短语冻结进账本并拼接；
/// 强停顿真实切割 → Draft 整段重识别提交，覆盖范围内的短语清退。
/// 预期对外预览从"滚动替换"变为"短语增量 + 尾部"的组合视图。
#[tokio::test]
async fn phrase_finalizes_accumulate_then_draft_replaces() {
    let channels: Vec<(
        oneshot::Sender<Result<String, String>>,
        oneshot::Receiver<Result<String, String>>,
    )> = (0..3).map(|_| oneshot::channel()).collect();
    let (mut senders, receivers): (Vec<_>, Vec<_>) = channels.into_iter().unzip();
    let transport = ControlledTransport::new(receivers);
    let engine = preview_draft_engine(transport.clone());

    let speech = vec![0.1f32; 16_000];
    // 300ms 停顿：VAD SentenceEnd 候选成立但低于自然采纳下限
    // （NATURAL_PAUSE_MIN_MS=400ms），复语作废 → 短语冻结路径。
    let short_pause = vec![0.0f32; 16_000 * 3 / 10];
    let strong_pause = vec![0.0f32; 16_000 * 8 / 10];

    let feed = |source: Vec<f32>| {
        let engine = &engine;
        async move {
            for chunk in source.chunks(160) {
                engine.transcribe_chunk(chunk).await.expect("chunk ok");
            }
        }
    };

    // 第 1 短语：1s 语音 + 300ms 停顿（候选）+ 复语 → 候选作废 → 短语定稿
    feed(speech.clone()).await;
    feed(short_pause.clone()).await;
    feed(speech.clone()).await;
    transport.wait_for_calls_or_fail(1).await;
    senders
        .remove(0)
        .send(Ok("明确的多句话。".to_string()))
        .expect("sender 不应泄漏");
    wait_until(|| engine.inner.lock().unwrap().latest_preview == "明确的多句话。").await;

    // 第 2 短语：锚点推进到上一候选 quiet_start，短语范围 [2.0s, 3.4s]
    feed(short_pause.clone()).await;
    feed(speech.clone()).await;
    transport.wait_for_calls_or_fail(2).await;
    senders
        .remove(0)
        .send(Ok("句与句之间。".to_string()))
        .expect("sender 不应泄漏");
    wait_until(|| engine.inner.lock().unwrap().latest_preview == "明确的多句话。句与句之间。")
        .await;
    {
        let inner = engine.inner.lock().unwrap();
        assert_eq!(inner.preview_phrases.len(), 2, "两条短语定稿入账");
        // 时间线：1s 语音 + 0.3s 停顿 ×2 —— 第 2 候选谷起点 = 2.3s；
        // 0.23.14.6 起切点推进到谷内稳健位置（+30~150ms）。
        let anchor = inner.phrase_anchor;
        assert!(
            anchor > (16_000 * 23 / 10) as u64
                && anchor <= (16_000 * 23 / 10 + 16_000 * 150 / 1000) as u64,
            "锚点推进到第 2 候选谷内切点 [2.33s, 2.45s]（实测 {anchor}）"
        );
    }

    // 真实切割：800ms 强停顿 → Draft 整段重识别 → 短语清退、尾部清空
    feed(strong_pause).await;
    transport.wait_for_calls_or_fail(3).await;
    senders
        .remove(0)
        .send(Ok("草稿正文。".to_string()))
        .expect("sender 不应泄漏");
    wait_until(|| {
        let inner = engine.inner.lock().unwrap();
        inner.preview_phrases.is_empty() && inner.latest_preview.is_empty()
    })
    .await;

    let final_text = engine.finalize().await.expect("finalize ok");
    assert_eq!(final_text, "草稿正文。", "最终文本以 Draft 为准");
    {
        let inner = engine.inner.lock().unwrap();
        assert_eq!(
            inner.phrase_anchor, inner.sentences.committed_sample_end as u64,
            "锚点不低于已提交水位"
        );
    }
}

/// 0.23.13 时间触发短语冻结：连续语音无任何停顿候选（停顿检测失灵的
/// 等效场景——底噪高于停顿电平、VAD 永不判安静）时，滚动窗前缀积满
/// 1.2s 主动追认为短语，灰色预览仍按短语增量累积而非只剩最近 3s 碎片。
#[tokio::test]
async fn time_freeze_finalizes_prefix_without_pause_candidates() {
    let channels: Vec<(
        oneshot::Sender<Result<String, String>>,
        oneshot::Receiver<Result<String, String>>,
    )> = (0..2).map(|_| oneshot::channel()).collect();
    let (mut senders, receivers): (Vec<_>, Vec<_>) = channels.into_iter().unzip();
    let transport = ControlledTransport::new(receivers);
    let engine = preview_draft_engine(transport.clone());

    // 4.5s 连续语音（恒定幅度，无停顿）：不形成候选、不作废、自然切句
    // 也不触发——唯一的短语入账路径是时间冻结（前缀 [0, total-3s] ≥ 1.2s
    // 在 total ≥ 4.2s 时成立）。
    let speech = vec![0.1f32; 16_000 * 45 / 10];
    for chunk in speech.chunks(160) {
        engine.transcribe_chunk(chunk).await.expect("chunk ok");
    }
    transport.wait_for_calls_or_fail(1).await;
    senders
        .remove(0)
        .send(Ok("时间冻结短语。".to_string()))
        .expect("sender 不应泄漏");
    wait_until(|| {
        let inner = engine.inner.lock().unwrap();
        inner.preview_phrases.len() == 1
    })
    .await;

    let inner = engine.inner.lock().unwrap();
    assert_eq!(
        inner.preview_phrases.len(),
        1,
        "无停顿场景前缀仍按时间冻结入账"
    );
    assert!(
        inner.phrase_anchor >= (16_000 * 12 / 10) as u64,
        "锚点推进到滚动窗起点（实际 {}）",
        inner.phrase_anchor
    );
    // 组合预览 = 冻结短语 + 尾部，前缀不再随滚动窗丢失
    assert!(
        inner.latest_preview.contains("时间冻结短语。"),
        "组合预览包含冻结短语（实际 {:?}）",
        inner.latest_preview
    );
}

/// 0.23.9.10 入队刷新限频：尾部预览 in-flight 期间，排队快照只在新音频
/// ≥500ms 时替换。修复前每个 10ms 音频块都 replace 并消耗一个 coordinator
/// request id（~100/s），spawn id 从 1 膨胀到三位数。
#[tokio::test]
async fn pending_preview_queue_refresh_is_rate_limited() {
    let channels: Vec<(
        oneshot::Sender<Result<String, String>>,
        oneshot::Receiver<Result<String, String>>,
    )> = (0..3).map(|_| oneshot::channel()).collect();
    let (mut senders, receivers): (Vec<_>, Vec<_>) = channels.into_iter().unzip();
    let transport = ControlledTransport::new(receivers);
    let engine = preview_draft_engine(transport.clone());

    // last_preview 从引擎创建起计时；越过刷新间隔后快速喂入才会触发尾部调度
    tokio::time::sleep(Duration::from_millis(800)).await;

    // 1.3s 语音满足首预览门槛（≥1.2s 有效输入）→ spawn 尾部预览（不响应，
    // 保持 in-flight）
    let speech = vec![0.1f32; 16_000 * 13 / 10];
    for chunk in speech.chunks(160) {
        engine.transcribe_chunk(chunk).await.expect("chunk ok");
    }
    transport.wait_for_calls_or_fail(1).await;

    // in-flight 期间再喂 2s：限频后排队替换只应发生 ~4 次
    let more = vec![0.1f32; 16_000 * 2];
    for chunk in more.chunks(160) {
        engine.transcribe_chunk(chunk).await.expect("chunk ok");
    }
    let burned = engine.inner.lock().unwrap().next_preview_request;
    assert!(
        burned <= 10,
        "入队刷新必须限频：next_preview_request={burned}（修复前以块率膨胀到数百）"
    );

    // 收尾：空结果释放 in-flight，排队快照接管（第二次尾部调用）
    senders
        .remove(0)
        .send(Ok(String::new()))
        .expect("sender 不应泄漏");
    wait_until(|| !engine.inner.lock().unwrap().preview_in_flight).await;
    let tail = vec![0.1f32; 16_000 / 10];
    for chunk in tail.chunks(160) {
        engine.transcribe_chunk(chunk).await.expect("chunk ok");
    }
    transport.wait_for_calls_or_fail(2).await;
    senders
        .remove(0)
        .send(Ok(String::new()))
        .expect("sender 不应泄漏");
    wait_until(|| !engine.inner.lock().unwrap().preview_in_flight).await;

    // finalize：剩余全部为语音 → terminal 识别（第三次调用；提前应答，
    // 否则 transport 的 oneshot 会永久等待）
    senders
        .remove(0)
        .send(Ok("终稿。".to_string()))
        .expect("sender 不应泄漏");
    let final_text = engine.finalize().await.expect("finalize ok");
    assert_eq!(final_text, "终稿。", "terminal 定稿文本应原样返回");
    assert_eq!(transport.calls.load(std::sync::atomic::Ordering::SeqCst), 3);
}

/// 0.23.9.10 清空信封的 requestId 不得倒退：`clear_preview` 保留最近一次
/// 尾部结果的 request id，否则清空信封携带低 id 被消费方的 request-id 墙
/// 当作过期丢弃，句尾旧虚字残留。
#[tokio::test]
async fn clear_envelope_request_id_stays_monotonic() {
    let transport = ControlledTransport::new(vec![]);
    let engine = preview_draft_engine(transport);

    {
        let mut inner = engine.inner.lock().unwrap();
        inner.preview_tail = "旧预览".to_string();
        inner.latest_preview = "旧预览".to_string();
        inner.latest_preview_request_id = 235;
        inner.preview_revision = 7;
        inner.last_reported_preview_revision = 6;
    }

    let first = engine
        .transcribe_chunk(&[0.0; 160])
        .await
        .expect("chunk ok");
    let first: serde_json::Value = serde_json::from_str(&first).expect("预览信封");
    assert_eq!(first["kind"], "preview");
    assert_eq!(first["requestId"].as_u64(), Some(235));

    engine.inner.lock().unwrap().clear_preview();

    let second = engine
        .transcribe_chunk(&[0.0; 160])
        .await
        .expect("chunk ok");
    let second: serde_json::Value = serde_json::from_str(&second).expect("清空信封");
    assert_eq!(second["kind"], "preview");
    assert_eq!(second["text"].as_str(), Some(""));
    assert!(
        second["requestId"].as_u64().unwrap_or(0) >= 235,
        "清空信封 requestId 不得倒退（实测 {}）",
        second["requestId"]
    );
}

// ── 引擎 reset 测试 ──

/// 0.23.10.2 短句停顿即时短语定稿：句长不足 min_sentence（默认 800ms）的
/// 停顿被 VAD 判为 ShortPhraseEnd——不再等复语作废候选，暂停达 min_silence
/// 即对 [phrase_anchor, quiet_start) 定稿并推进锚点。修复前该短语被静默
/// 吞掉（无事件、无候选、无定稿），短首句的首个可见文本被推迟数秒。
#[tokio::test]
async fn short_phrase_pause_freezes_phrase_without_waiting_for_resume() {
    let channels: Vec<(
        oneshot::Sender<Result<String, String>>,
        oneshot::Receiver<Result<String, String>>,
    )> = (0..2).map(|_| oneshot::channel()).collect();
    let (mut senders, receivers): (Vec<_>, Vec<_>) = channels.into_iter().unzip();
    let transport = ControlledTransport::new(receivers);
    let engine = preview_draft_engine(transport.clone());

    // 600ms 语音（< min_sentence 800ms）+ 400ms 停顿（≥ min_silence 300ms）
    let speech = vec![0.1f32; 16_000 * 6 / 10];
    let pause = vec![0.0f32; 16_000 * 4 / 10];
    for source in [&speech, &pause] {
        for chunk in source.chunks(160) {
            engine.transcribe_chunk(chunk).await.expect("chunk ok");
        }
    }

    // 停顿期间（未复语）短语推理即已发起——这是本修复的核心断言
    transport.wait_for_calls_or_fail(1).await;
    senders
        .remove(0)
        .send(Ok("短句定稿。".to_string()))
        .expect("sender 不应泄漏");
    wait_until(|| engine.inner.lock().unwrap().latest_preview == "短句定稿。").await;
    {
        let inner = engine.inner.lock().unwrap();
        assert_eq!(inner.preview_phrases.len(), 1, "短句必须定稿入账");
        // 0.23.14.6：切点推进到谷内稳健位置（谷起点 600ms + 30~150ms），
        // 自然衰减尾音保留给短语识别；对外范围信封由 span 清单推导。
        let phrase_end = inner.preview_phrases[0].range.end_sample;
        assert!(
            phrase_end > (16_000 * 6 / 10) as u64
                && phrase_end <= (16_000 * 6 / 10 + 16_000 * 150 / 1000) as u64,
            "短语范围终点必须在谷内 [630ms, 750ms]（实测 {phrase_end}）"
        );
        assert_eq!(
            inner.phrase_anchor, phrase_end,
            "锚点推进到谷内切点，尾部不再重复识别该短语"
        );
    }

    // 终态：terminal drain 识别剩余全部音频（第二次调用需提前应答）
    senders
        .remove(0)
        .send(Ok("终稿。".to_string()))
        .expect("sender 不应泄漏");
    let final_text = engine.finalize().await.expect("finalize ok");
    assert_eq!(final_text, "终稿。");
}

// ── 0.23.14 P0-1 长静音可靠终结 ──────────────────────────────────

/// 构造 PreviewDraft profile 的内部状态（candidate_readiness 纯逻辑测试用）。
fn preview_draft_inner() -> PseudoInner {
    let mut inner = PseudoInner::for_test(Vec::new());
    inner.coordinator = RecognitionCoordinator::new(
        16_000,
        RecognitionProfile::PreviewDraft,
        RecognitionSettings::default(),
    );
    inner
}

fn candidate_ms(voiced_ms: u64, quiet_ms: u64) -> BoundaryCandidate {
    // strong = voiced（强帧占比 100%）：默认构造代表清语音；可信度门
    // （0.23.14.7 case_17）由专门测试用低占比构造覆盖。
    BoundaryCandidate {
        boundary_sample: voiced_ms * 16,
        quiet_start_sample: voiced_ms * 16,
        reason: "natural_silence".to_string(),
        voiced_samples: voiced_ms * 16,
        strong_samples: voiced_ms * 16,
        strong_run_max_samples: voiced_ms * 16,
        quiet_samples: quiet_ms * 16,
    }
}

/// 长静音终结决策矩阵：voiced × silence 组合下候选是否升级为 Draft。
///
/// 修复前（0.23.13）：短句（< 2s owned / < 1.2s voiced）后无论静音多久
/// 都不满足 readiness——只有 owned ≥ 5s 或强停顿（700ms + owned ≥ 2s +
/// voiced ≥ 1.2s）两条路。0.23.14 增加 long_pause（默认 1100ms）独立
/// 终结：只要可信有声 ≥ 300ms 即接受；句内 300ms 停顿不误切。
/// 0.23.14.7 增加自然句尾独立采纳：`natural_silence`（VAD 已完成
/// min_sentence_ms 校验）+ 停顿 ≥ strong_pause_ms + 有效有声 ≥
/// min_sentence_ms 即接受——与 VAD 自身句长校验同源对齐，消除
/// case_12 的门槛夹缝（1.12s 句 + 900ms 停顿两边都够不着）。
#[test]
fn long_pause_readiness_matrix() {
    let inner = preview_draft_inner();
    // (voiced_ms, quiet_ms, 期望接受)——voiced {300,600,900,1500} ×
    // silence {300,700,1200,2000} 全交叉，另含噪声/全静音边界。
    let matrix: &[(u64, u64, bool)] = &[
        // 句内停顿（300ms）：任何短句都不切——保留短语/预览语义
        (300, 300, false),
        (600, 300, false),
        (900, 300, false),
        (1_500, 300, false),
        // 700ms 停顿：自然句尾路径按 min_sentence（800ms）有声裁决——
        // ≥800ms 的自然句被采纳（VAD SentenceEnd 已校验过同一句长，
        // 采纳下限 NATURAL_PAUSE_MIN_MS=400ms），
        // <800ms 的短句不被采纳（等长静音或复语作废）
        (300, 700, false),
        (600, 700, false),
        (900, 700, true),
        (1_500, 700, true),
        // case_12 首句实测形状：门控口径停顿约 570ms（体感约 900ms，
        // 起音块内静默不参与累计）+ voiced≈1.79s——修复前两边门槛都够不着
        (1_790, 570, true),
        // case_12 第三句实测形状：四档停顿中最短的一档（gate 口径
        // 约 430–470ms，体感约 700ms）——采纳下限必须覆盖它
        (1_480, 450, true),
        (600, 470, false),
        // 长静音（≥1100ms）：可信有声（≥300ms）即终结——短句不再悬停
        (300, 1_200, true),
        (600, 1_200, true),
        (900, 1_200, true),
        (1_500, 1_200, true),
        (300, 2_000, true),
        (600, 2_000, true),
        (900, 2_000, true),
        (1_500, 2_000, true),
        // 噪声脉冲（<300ms 有效有声）即使长静音也不终结
        (60, 1_200, false),
        (60, 2_000, false),
        (120, 2_000, false),
        (0, 2_000, false),
    ];
    for &(voiced_ms, quiet_ms, expect_ok) in matrix {
        let candidate = candidate_ms(voiced_ms, quiet_ms);
        let readiness = inner.candidate_readiness(&candidate, usize::MAX / 2, 16_000);
        assert_eq!(
            readiness.is_ok(),
            expect_ok,
            "voiced={voiced_ms}ms quiet={quiet_ms}ms 期望接受={expect_ok}，实际 {readiness:?}"
        );
    }
}

/// 自然句尾独立采纳路径的边界裁决（0.23.14.7）：有声与 min_sentence_ms
/// 同源对齐，不套 generic strong pause 的 owned ≥ 2s / voiced ≥ 1.2s；
/// 无自然句尾证据的候选（soft_window 等）不得借用该路径。
#[test]
fn natural_sentence_end_adopted_independently_of_strong_pause_guards() {
    let inner = preview_draft_inner();
    // case_12 首句实测形状：门控口径 voiced≈1.79s、停顿≈670ms（能量视角
    // 的"900ms 停顿"含呼吸/起音帧）——修复前 strong 差几十 ms quiet、
    // long 差 400ms，两边都够不着。
    let case_12_like = candidate_ms(1_790, 670);
    assert!(
        inner
            .candidate_readiness(&case_12_like, usize::MAX / 2, 16_000)
            .is_ok(),
        "自然句尾 + 600ms 级停顿必须按 min_sentence 口径采纳（不硬套 1.2s voiced）"
    );
    // owned 短于 generic strong 的 2s 下限同样接受（800–1200ms 自然句 +
    // 700–1000ms 停顿矩阵）。
    let short_owned = candidate_ms(850, 750);
    assert!(
        inner
            .candidate_readiness(&short_owned, usize::MAX / 2, 16_000)
            .is_ok()
    );
    // 有声低于 min_sentence：即使停顿充分也不采纳（咳嗽/呼吸防护）。
    let sub_min_sentence = candidate_ms(700, 900);
    assert_eq!(
        inner.candidate_readiness(&sub_min_sentence, usize::MAX / 2, 16_000),
        Err("natural_sentence_voiced_too_short")
    );
    // 停顿未达自然采纳下限（400ms）：继续等待（等长静音升级或复语作废）。
    let weak_quiet = candidate_ms(1_500, 350);
    assert_eq!(
        inner.candidate_readiness(&weak_quiet, usize::MAX / 2, 16_000),
        Err("below_natural_pause")
    );
    // 无自然句尾证据的候选不得借用自然路径：soft_window + 700ms 停顿 +
    // owned ≥ 2s + 900ms 有声，generic strong 的 voiced ≥ 1.2s 仍适用。
    let no_natural_evidence = BoundaryCandidate {
        reason: "soft_window".to_string(),
        voiced_samples: 900 * 16,
        ..candidate_ms(2_000, 700)
    };
    assert_eq!(
        inner.candidate_readiness(&no_natural_evidence, usize::MAX / 2, 16_000),
        Err("strong_pause_voiced_too_short")
    );
}

/// 0.23.14.7 case_17 证据链：readiness 放行必须报告依据分支——
/// candidate reason 只描述 VAD 事件类型，`natural_silence` 候选既可能被
/// 自然句尾路径放行，也可能被 draft_min / long_pause 放行；诊断记录必须
/// 能区分，环境声误切才能归因。
#[test]
fn candidate_readiness_reports_accepted_via_branch() {
    let inner = preview_draft_inner();
    // 自然句尾：natural_silence + 停顿 ≥400ms + voiced ≥ min_sentence，
    // owned < 5s（否则 draft_min 先放行）。
    let natural = candidate_ms(900, 700);
    assert_eq!(
        inner.candidate_readiness(&natural, 40_000, 16_000),
        Ok(AcceptedVia::NaturalSentence)
    );
    // draft_min：boundary ≥ 5s（owned 由 boundary_sample 决定）优先放行。
    let long_owned = candidate_ms(5_100, 700);
    assert_eq!(
        inner.candidate_readiness(&long_owned, usize::MAX / 2, 16_000),
        Ok(AcceptedVia::DraftMin)
    );
    // 长静音：quiet ≥ 1100ms + voiced ≥ 300ms（soft_window 无自然句证据）。
    let long_pause = BoundaryCandidate {
        reason: "soft_window".to_string(),
        voiced_samples: 300 * 16,
        ..candidate_ms(1_000, 1_200)
    };
    assert_eq!(
        inner.candidate_readiness(&long_pause, 40_000, 16_000),
        Ok(AcceptedVia::LongPause)
    );
    // 强停顿：700ms + owned ≥ 2s + voiced ≥ 1.2s。
    let strong = BoundaryCandidate {
        reason: "soft_window".to_string(),
        voiced_samples: 1_200 * 16,
        ..candidate_ms(2_500, 700)
    };
    assert_eq!(
        inner.candidate_readiness(&strong, 40_000, 16_000),
        Ok(AcceptedVia::StrongPause)
    );
}

/// 0.23.14.7 case_17 可信发声连续性门：只有稀疏环境声脉冲（无连续强有声段）
/// 的候选不得经自然句尾分支放行。全量真实回放标定——case_17 尾段候选
/// 连续强有声仅 50ms 且被模型回以 2 字符伪文本；合法候选最低 120ms
/// （case_15 远场轻声），其余 290ms～4230ms。门槛 100ms。
#[test]
fn natural_pause_rejects_noise_without_sustained_voicing() {
    let inner = preview_draft_inner();

    // case_17 修复前实测形状：quiet 440ms + voiced 1060ms + 强有声 340ms，
    // 但最长连续强有声段只有 50ms（键盘/风扇脉冲）——修复前被 natural_pause
    // 放行并产生 2 字符伪文本 Draft。
    let noise = BoundaryCandidate {
        boundary_sample: 2_160 * 16,
        quiet_start_sample: 2_160 * 16,
        reason: "natural_silence".to_string(),
        voiced_samples: 1_060 * 16,
        strong_samples: 340 * 16,
        strong_run_max_samples: 50 * 16,
        quiet_samples: 440 * 16,
    };
    assert_eq!(
        inner.candidate_readiness(&noise, 40_000, 16_000),
        Err("natural_sentence_voiced_not_credible")
    );

    // case_15 r5 真句形状（远场轻声，全量回放中合法候选的最低值）：
    // 连续强有声 120ms 必须保留。
    let quiet_speech = BoundaryCandidate {
        boundary_sample: 2_400 * 16,
        quiet_start_sample: 2_400 * 16,
        reason: "natural_silence".to_string(),
        voiced_samples: 1_560 * 16,
        strong_samples: 600 * 16,
        strong_run_max_samples: 120 * 16,
        quiet_samples: 400 * 16,
    };
    assert_eq!(
        inner.candidate_readiness(&quiet_speech, 40_000, 16_000),
        Ok(AcceptedVia::NaturalSentence),
        "远场轻声（连续强有声 120ms）不得被可信度门误伤"
    );

    // 无任何强有声（纯噪声尾段）：同样拒绝。
    let pure_noise = BoundaryCandidate {
        boundary_sample: 1_500 * 16,
        quiet_start_sample: 1_500 * 16,
        reason: "natural_silence".to_string(),
        voiced_samples: 1_000 * 16,
        strong_samples: 0,
        strong_run_max_samples: 0,
        quiet_samples: 500 * 16,
    };
    assert_eq!(
        inner.candidate_readiness(&pure_noise, 40_000, 16_000),
        Err("natural_sentence_voiced_not_credible")
    );
}

/// 0.23.14.7 case_17：长静音兜底分支共用可信发声连续性判据——否则被自然
/// 句尾拒绝的噪声候选会在 quiet 涨到 long_pause_ms 后被"换个分支再采纳
/// 一次"，修复等同无效。
#[test]
fn long_pause_also_rejects_noise_without_sustained_voicing() {
    let inner = preview_draft_inner();

    // quiet 1_400ms（≥ long_pause_ms 1100）、voiced 1_060ms（≥ 300ms 门槛），
    // 但连续强有声只有 50ms。
    let noise = BoundaryCandidate {
        boundary_sample: 2_300 * 16,
        quiet_start_sample: 2_300 * 16,
        reason: "natural_silence".to_string(),
        voiced_samples: 1_060 * 16,
        strong_samples: 340 * 16,
        strong_run_max_samples: 50 * 16,
        quiet_samples: 1_400 * 16,
    };
    assert_eq!(
        inner.candidate_readiness(&noise, 40_000, 16_000),
        Err("long_pause_voiced_not_credible")
    );

    // 同样是长静音，但存在可信连续发声（真句）——long_pause 照旧放行。
    let speech = BoundaryCandidate {
        boundary_sample: 2_300 * 16,
        quiet_start_sample: 2_300 * 16,
        reason: "natural_silence".to_string(),
        voiced_samples: 1_060 * 16,
        strong_samples: 700 * 16,
        strong_run_max_samples: 220 * 16,
        quiet_samples: 1_400 * 16,
    };
    assert_eq!(
        inner.candidate_readiness(&speech, 40_000, 16_000),
        Ok(AcceptedVia::LongPause)
    );
}

/// 0.23.14.7 case_17：可信度噪声门**只作用于 PreviewDraft**。Legacy
/// （G1/G3 默认 profile）没有候选观察路径，`uncommitted_strong_run_max`
/// 恒为 0；若把门一并套到 Legacy，终态 finalize 会把**全部**未提交尾段判为
/// NoSpeech 而不再送模——引擎级不变量测试（`valley_*` / `retreated_*`）正是
/// 在这种情形下挂起并暴露该缺陷。同一段"稀疏脉冲"音频在两种 profile 下
/// 必须给出相反结论。
#[tokio::test]
async fn credibility_noise_gate_applies_only_to_preview_draft() {
    // 500ms 静音 + 30ms 脉冲 + 600ms 静音。脉冲幅度取 0.01（RMS≈0.0071）：
    // 位于 off(0.005) 与 on(0.01) 之间的滞回带——`trim_trailing_silence` 不裁它，
    // 但不足以让 VAD 起音，因此既不产生候选（不引入 Draft 调用），又能在 Legacy
    // 路径形成一次真实送模范围。PreviewDraft 侧该范围没有任何强有声帧
    // （RMS ≥ on），连续段为 0 → 噪声门按 NoSpeech 消费。
    let audio = valley_concat(&[valley_silence(500), valley_tone(30, 0.01), valley_silence(600)]);

    // Legacy：终态照旧送模并提交文本（既有覆盖契约）。
    let (tx, rx) = oneshot::channel();
    let transport = WavCaptureTransport::new(vec![rx]);
    let legacy_engine = std::sync::Arc::new(valley_engine(transport.clone()));
    legacy_engine.transcribe_chunk(&audio).await.unwrap();
    let finalize_legacy = std::sync::Arc::clone(&legacy_engine);
    let task = tokio::spawn(async move {
        finalize_legacy
            .finalize_with_wait_timeout(Duration::from_secs(3))
            .await
    });
    transport.wait_for_calls(1).await;
    tx.send(Ok("尾段。".into())).unwrap();
    assert_eq!(
        task.await.unwrap().unwrap(),
        "尾段。",
        "Legacy 终态必须覆盖未提交尾段（可信度门不得介入）"
    );

    // PreviewDraft：同一段音频无可信连续发声 → 终态按 NoSpeech 消费，不送模。
    let (tx2, rx2) = oneshot::channel();
    let transport2 = WavCaptureTransport::new(vec![rx2]);
    let pd_engine = std::sync::Arc::new(valley_engine_preview_draft(transport2.clone()));
    pd_engine.transcribe_chunk(&audio).await.unwrap();
    let text = pd_engine
        .finalize_with_wait_timeout(Duration::from_millis(200))
        .await
        .unwrap();
    assert_eq!(text, "", "无可信发声的尾段不得进入终态文本");
    assert_eq!(
        transport2.calls.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "PreviewDraft 尾段噪声门必须跳过送模"
    );
    drop(tx2);
}

/// 0.23.14.7 case_17 送模范围门与候选采纳证据同源——范围内必须有 ≥100ms
/// 连续强有声段，否则按 NoSpeech 消费（Draft 定稿与终态 drain 共用）。
#[test]
fn range_credible_voicing_requires_sustained_run() {
    // 40ms 强脉冲 + 长静音：噪声形态，无可信发声。
    let mut impulse = vec![0.0f32; 16_000];
    for sample in impulse.iter_mut().take(640) {
        *sample = 0.5;
    }
    assert!(
        !PseudoInner::range_has_credible_voicing(&impulse, 0.05, 16_000),
        "稀疏强脉冲不得视为可信发声"
    );

    // 200ms 连续强发声：可信（真音节量级）。
    let mut voiced = vec![0.0f32; 16_000];
    for sample in voiced.iter_mut().take(3_200) {
        *sample = 0.5;
    }
    assert!(
        PseudoInner::range_has_credible_voicing(&voiced, 0.05, 16_000),
        "200ms 连续发声必须视为可信"
    );

    // 纯静音：无可信发声。
    let silence = vec![0.0f32; 16_000];
    assert!(!PseudoInner::range_has_credible_voicing(
        &silence, 0.05, 16_000
    ));
}

/// 0.23.14.7 case_17 复语裁决一致性：不可信的自然句尾候选在复语块不得
/// 借自然路径保留（照旧作废并触发短语冻结），与 readiness 拒绝同一判据。
#[test]
fn natural_candidate_without_credibility_is_invalidated_on_resume() {
    let mut inner = preview_draft_inner();
    let voiced_chunk = vec![0.1f32; 1600]; // 100ms 起音块

    // 形状同 case_17 噪声尾段：quiet/voiced 达标但连续强有声只有 50ms。
    inner.boundary_candidate = Some(BoundaryCandidate {
        boundary_sample: 2_160 * 16,
        quiet_start_sample: 2_160 * 16,
        reason: "natural_silence".to_string(),
        voiced_samples: 1_060 * 16,
        strong_samples: 340 * 16,
        strong_run_max_samples: 50 * 16,
        quiet_samples: 440 * 16,
    });
    let invalidated = inner.observe_boundary_candidate(
        VadEvent::None,
        2_160 * 16 + 1600,
        &voiced_chunk,
        0.0,
        16_000,
    );
    assert!(
        invalidated.is_some(),
        "不可信候选在复语块必须作废（触发短语冻结），不得滞留等待"
    );
}

/// 0.23.14.7 复语保留裁决：复语起音时已满足采纳条件的自然句尾候选不作废
/// （case_12 实测形态：quiet 570ms 时起音块到达，起音块内静默不参与
/// 累计——若要求"先跨越下限再等复语"，自然句尾永远等不到采纳），保留
/// 给本轮 readiness 采纳；短句与不可采纳候选照旧作废并触发短语冻结。
#[test]
fn natural_sentence_candidate_survives_resume_chunk_when_adoptable() {
    let mut inner = preview_draft_inner();
    let voiced_chunk = vec![0.1f32; 1600]; // 100ms 起音块

    // 自然句尾候选（quiet 570ms ≥ 400ms 下限，voiced 1790ms ≥ 800ms）。
    inner.boundary_candidate = Some(BoundaryCandidate {
        boundary_sample: 2_680 * 16,
        quiet_start_sample: 2_680 * 16,
        reason: "natural_silence".to_string(),
        voiced_samples: 1_790 * 16,
        strong_samples: 1_790 * 16,
        strong_run_max_samples: 1_790 * 16,
        quiet_samples: 570 * 16,
    });
    let invalidated = inner.observe_boundary_candidate(
        VadEvent::None,
        2_680 * 16 + 1600,
        &voiced_chunk,
        0.0,
        16_000,
    );
    assert!(invalidated.is_none(), "可采纳自然句尾在复语块不得作废");
    let candidate = inner.boundary_candidate.clone().expect("候选保留");
    assert!(
        inner
            .candidate_readiness(&candidate, 2_680 * 16 + 1600, 16_000)
            .is_ok(),
        "保留的候选本轮 readiness 立即采纳"
    );

    // 未达采纳条件的自然句尾（voiced < min_sentence）：复语照旧作废。
    inner.boundary_candidate = Some(BoundaryCandidate {
        reason: "natural_silence".to_string(),
        voiced_samples: 600 * 16,
        ..candidate_ms(2_000, 670)
    });
    let invalidated = inner.observe_boundary_candidate(
        VadEvent::None,
        2_000 * 16 + 1600,
        &voiced_chunk,
        0.0,
        16_000,
    );
    assert!(invalidated.is_some(), "不可采纳候选必须作废并触发短语冻结");
    assert!(inner.boundary_candidate.is_none());

    // 短句候选：复语照旧作废（不借用自然路径）。
    inner.boundary_candidate = Some(BoundaryCandidate {
        reason: "short_phrase".to_string(),
        voiced_samples: 660 * 16,
        quiet_samples: 670 * 16,
        ..candidate_ms(660, 670)
    });
    let invalidated = inner.observe_boundary_candidate(
        VadEvent::None,
        660 * 16 + 1600,
        &voiced_chunk,
        0.0,
        16_000,
    );
    assert!(invalidated.is_some(), "短句候选复语作废语义保持");
    assert!(inner.boundary_candidate.is_none());
}

/// 0.23.14.7 核心场景（P1-3 根因）：自然 SentenceEnd（VAD 已完成
/// min_sentence_ms 校验）+ 600ms 级停顿必须在停顿期间采纳为 Draft——不再
/// 等 long_pause（1100ms）或复语合并。case_12 首句（1.1s+ 句 + 门控口径
/// 约 670ms 停顿）修复前被门槛夹缝卡住，最终与下一句合并到 7.66s。
#[tokio::test]
async fn natural_sentence_drafts_during_strong_pause_before_long_pause() {
    let (sender, receiver) = oneshot::channel();
    let transport = ControlledTransport::new(vec![receiver]);
    let engine = preview_draft_engine(transport.clone());

    let feed = |source: Vec<f32>| {
        let engine = &engine;
        async move {
            for chunk in source.chunks(160) {
                engine.transcribe_chunk(chunk).await.expect("chunk ok");
            }
        }
    };

    // 900ms 自然句（≥ min_sentence 800ms）+ 700ms 停顿（≥ 自然采纳下限
    // 400ms，< long 1100ms）：Draft 必须在静音期间起飞，发生在复语之前。
    feed(vec![0.1f32; 16_000 * 9 / 10]).await;
    feed(vec![0.0f32; 16_000 * 7 / 10]).await;
    transport.wait_for_calls_or_fail(1).await;
    sender
        .send(Ok("自然句定稿。".to_string()))
        .expect("sender 不应泄漏");
    wait_until(|| engine.stream_stats().pcm_committed_end > 0).await;
    {
        let inner = engine.inner.lock().unwrap();
        assert!(
            inner.sentences.committed_sample_end >= 16_000 * 9 / 10,
            "Draft 覆盖整个自然句音频"
        );
        assert_eq!(
            inner.sentences.draft_spans().len(),
            1,
            "强停顿期间采纳 1 个自然句 Draft"
        );
        assert!(inner.boundary_candidate.is_none(), "采纳后候选清退");
    }

    // 终态：剩余纯静音无新调用，最终文本即 Draft 正文
    let final_text = engine.finalize().await.expect("finalize ok");
    assert_eq!(final_text, "自然句定稿。");
}

/// 0.23.14.7 P1-1：冻结短语入账必须同步清退覆盖同一音频范围的尾部——
/// tail 已显示 A，同范围短语 A 随后入账，投影只能出现一个 A；迟到旧
/// Preview（范围裁决）不得让 A 再次出现。
#[test]
fn phrase_landing_settles_overlapping_tail_to_single_projection() {
    let mut inner = preview_draft_inner();
    // 尾部预览已显示 A（范围 [0, 2s)）。
    inner.set_preview_if_changed("明确的多句话。".to_string(), AudioRange::new(0, 32_000), 1);
    assert_eq!(inner.latest_preview, "明确的多句话。");

    // 同一音频范围的短语随后冻结入账（[0, 3s)），尾部被范围清退。
    inner.preview_phrases.push(PreviewSpan::new(
        AudioRange::new(0, 48_000),
        "明确的多句话。",
    ));
    inner.settle_tail_for_phrase(48_000);
    inner.rebuild_preview_text();
    assert_eq!(
        inner.latest_preview, "明确的多句话。",
        "同一音频范围只投影一次 A"
    );
    assert!(inner.preview_tail.is_empty(), "重叠尾部已清退");

    // 迟到旧 Preview（同一范围）不得让 A 再次出现。
    inner.set_preview_if_changed("明确的多句话。".to_string(), AudioRange::new(0, 32_000), 2);
    assert_eq!(
        inner.latest_preview, "明确的多句话。",
        "迟到预览被范围裁决丢弃"
    );
    assert!(inner.preview_tail.is_empty());
}

/// 0.23.14.7 P1-1 跨界尾部：短语只覆盖尾部的前缀时，重叠前缀整条清退
/// （不做字符串裁剪），位于短语之后的真实后缀 B 交回尾部窗口重识别——
/// 迟到旧 Preview 不得回场，新尾部窗口投影 B 后组合预览 = A + B 且
/// spans 单调不重叠。
#[test]
fn crossing_tail_withdraws_overlap_and_suffix_recovers_from_tail_window() {
    let mut inner = preview_draft_inner();
    // 尾部已显示 A + 后缀 B（识别范围 [0, 5s)，短语只占 [0, 2s)）。
    inner.set_preview_if_changed("A后缀B".to_string(), AudioRange::new(0, 80_000), 1);
    assert_eq!(inner.latest_preview, "A后缀B");

    // 短语 A 冻结入账（[0, 2s)）：跨界尾部整条清退，后缀交回尾部窗口。
    inner
        .preview_phrases
        .push(PreviewSpan::new(AudioRange::new(0, 32_000), "A"));
    inner.settle_tail_for_phrase(32_000);
    inner.rebuild_preview_text();
    assert_eq!(inner.latest_preview, "A", "重叠前缀清退后只剩短语 A");

    // 迟到旧 Preview（覆盖短语范围）不得让 A 再次出现。
    inner.set_preview_if_changed("A后缀B".to_string(), AudioRange::new(0, 80_000), 2);
    assert_eq!(inner.latest_preview, "A");

    // 尾部窗口（锚点已推进到短语终点）重识别出后缀 B → 后缀恢复。
    inner.set_preview_if_changed("后缀B".to_string(), AudioRange::new(32_000, 112_000), 3);
    assert_eq!(
        inner.latest_preview, "A后缀B",
        "真实后缀经重识别恢复，无重复 A"
    );

    // spans 单调不重叠。
    let mut spans = inner.preview_phrases.clone();
    if !inner.preview_tail.is_empty()
        && let Some(range) = inner.preview_tail_range
    {
        spans.push(PreviewSegment::new(range, inner.preview_tail.clone()));
    }
    assert_eq!(
        PseudoStreamingSttEngine::preview_spans_invariant_violation(&spans),
        None,
        "全部 Preview spans 两两不重叠"
    );
}

/// 0.23.14.7 P1-1：触及已提交水位的迟到预览按范围裁决丢弃——Draft settle
/// 之后旧前缀不得借尾部回场；健康范围（起点 ≥ committed 且不与短语
/// 重叠）照常显示。
#[test]
fn late_preview_overlapping_committed_or_phrase_is_dropped() {
    let mut inner = preview_draft_inner();
    inner.sentences.committed_sample_end = 48_000;

    // 完全落在已提交水位之下。
    inner.set_preview_if_changed("旧前缀".to_string(), AudioRange::new(0, 32_000), 1);
    assert!(inner.preview_tail.is_empty(), "已提交范围内的预览被丢弃");
    // 跨越已提交水位（文本可能含已定稿音频）。
    inner.set_preview_if_changed("跨界预览".to_string(), AudioRange::new(16_000, 80_000), 2);
    assert!(inner.preview_tail.is_empty(), "跨界已提交水位的预览被丢弃");

    // 与短语账本重叠。
    inner
        .preview_phrases
        .push(PreviewSpan::new(AudioRange::new(48_000, 96_000), "短语。"));
    inner.set_preview_if_changed("重叠预览".to_string(), AudioRange::new(64_000, 128_000), 3);
    assert!(inner.preview_tail.is_empty(), "与短语重叠的预览被丢弃");

    // 健康范围（短语之后）照常显示。
    inner.set_preview_if_changed("尾部".to_string(), AudioRange::new(96_000, 144_000), 4);
    assert_eq!(inner.preview_tail, "尾部");
    let mut spans = inner.preview_phrases.clone();
    spans.push(PreviewSegment::new(
        inner.preview_tail_range.unwrap(),
        inner.preview_tail.clone(),
    ));
    assert_eq!(
        PseudoStreamingSttEngine::preview_spans_invariant_violation(&spans),
        None
    );
}

/// 0.23.14.7 P1-1：组合预览出口的 span 不变量终审——存量冲突状态不允许
/// 带病投影，违规 span 被移除、组合文本同步修正（text 与 spans 一致）。
#[test]
fn compose_repairs_overlapping_preview_spans() {
    let engine = engine_without_transport(Vec::new());
    {
        let mut inner = engine.inner.lock().unwrap();
        inner
            .preview_phrases
            .push(PreviewSpan::new(AudioRange::new(0, 16_000), "短语A。"));
        // 尾部与短语重叠（模拟上游裁决之外的存量冲突状态）。
        inner.preview_tail = "短语A。尾部B".to_string();
        inner.preview_tail_range = Some(AudioRange::new(0, 32_000));
        inner.rebuild_preview_text();
    }
    let envelope = engine.compose_typed_result().expect("compose ok");
    let value: serde_json::Value = serde_json::from_str(&envelope).expect("envelope json");
    assert_eq!(value["kind"], "preview");
    let spans = value["spans"].as_array().expect("spans list");
    assert_eq!(spans.len(), 1, "违规尾部 span 被移除，仅剩短语 span");
    assert_eq!(spans[0]["text"], "短语A。");
    assert_eq!(
        value["text"], "短语A。",
        "组合文本与修复后的 spans 一致，不再含重复"
    );
    // 修复后的范围不重叠。
    for pair in spans.windows(2) {
        let prev_end = pair[0]["range"]["endSample"].as_u64().unwrap_or(0);
        let next_start = pair[1]["range"]["startSample"].as_u64().unwrap_or(0);
        assert!(prev_end <= next_start, "修复后 spans 单调不重叠");
    }
}

/// 0.23.14.7 P2 连续 compose 回归：投影修复必须持久化回 Preview 真源
/// （短语账本 + 尾部），且不得在已对齐 reported cursor 后再递增 revision。
/// 旧缺陷：修复只改局部 spans 与 latest_preview，真源仍保留冲突，同时
/// revision 被再次递增——下一次 compose 重建同一冲突、再次记 error 并
/// 再次发出同一 Preview。
#[test]
fn consecutive_compose_persists_repair_and_does_not_reemit() {
    let engine = engine_without_transport(Vec::new());
    let revision_before;
    {
        let mut inner = engine.inner.lock().unwrap();
        inner
            .preview_phrases
            .push(PreviewSpan::new(AudioRange::new(0, 16_000), "短语A。"));
        // 尾部与短语重叠（模拟上游裁决之外的存量冲突状态）。
        inner.preview_tail = "短语A。尾部B".to_string();
        inner.preview_tail_range = Some(AudioRange::new(0, 32_000));
        inner.rebuild_preview_text();
        revision_before = inner.preview_revision;
    }

    // 第一次 compose：返回修复后的合法 envelope。
    let first = engine.compose_typed_result().expect("compose ok");
    let value: serde_json::Value = serde_json::from_str(&first).expect("envelope json");
    assert_eq!(value["kind"], "preview");
    let spans = value["spans"].as_array().expect("spans list");
    assert_eq!(spans.len(), 1, "违规尾部被移除，仅剩短语 span");
    assert_eq!(spans[0]["text"], "短语A。");

    // 底层 phrase/tail 真源同步收敛：区间单调、非退化、互不重叠；被移除
    // 的尾部不得残留。
    {
        let inner = engine.inner.lock().unwrap();
        let mut truth = inner.preview_phrases.clone();
        if !inner.preview_tail.is_empty()
            && let Some(range) = inner.preview_tail_range
        {
            truth.push(PreviewSegment::new(range, inner.preview_tail.clone()));
        }
        assert_eq!(
            PseudoStreamingSttEngine::preview_spans_invariant_violation(&truth),
            None,
            "修复必须持久化：真源 phrase/tail 也必须满足不变量"
        );
        assert!(inner.preview_tail.is_empty(), "被移除的尾部不得残留在真源");
        assert_eq!(inner.latest_preview, "短语A。");
        assert_eq!(
            inner.preview_revision, revision_before,
            "修复不得递增 revision（本次调用已携带修复后内容）"
        );
        assert_eq!(
            inner.last_reported_preview_revision, inner.preview_revision,
            "reported cursor 必须与 revision 对齐"
        );
    }

    // 第二次 compose：不得再次修复、再次递增 revision 或重复发同一 Preview。
    let second = engine.compose_typed_result().expect("compose ok");
    assert!(second.is_empty(), "已上报的修复内容不得再次作为 Preview 发出");
    {
        let inner = engine.inner.lock().unwrap();
        assert_eq!(inner.preview_revision, revision_before, "revision 必须稳定");
    }
}

/// 0.23.14.7 P2：违规者是短语、尾部幸存时，修复只从真源移除违规短语；
/// 尾部按 AudioRange 身份原样保留（不重裁字符串），组合投影 = 幸存短语
/// + 尾部，且第二次 compose 不再发出。
#[test]
fn compose_repair_keeps_surviving_tail_in_truth_source() {
    let engine = engine_without_transport(Vec::new());
    {
        let mut inner = engine.inner.lock().unwrap();
        inner
            .preview_phrases
            .push(PreviewSpan::new(AudioRange::new(0, 16_000), "A。"));
        // 与 A 重叠的违规短语（后到者让位）。
        inner
            .preview_phrases
            .push(PreviewSpan::new(AudioRange::new(8_000, 32_000), "重叠。"));
        inner.preview_tail = "尾部。".to_string();
        inner.preview_tail_range = Some(AudioRange::new(32_000, 48_000));
        inner.rebuild_preview_text();
    }
    let first = engine.compose_typed_result().expect("compose ok");
    let value: serde_json::Value = serde_json::from_str(&first).expect("envelope json");
    let spans = value["spans"].as_array().expect("spans list");
    assert_eq!(spans.len(), 2, "违规短语被移除，幸存短语 + 尾部保留");
    assert_eq!(spans[0]["text"], "A。");
    assert_eq!(spans[1]["text"], "尾部。");
    {
        let inner = engine.inner.lock().unwrap();
        assert_eq!(inner.preview_phrases.len(), 1);
        assert_eq!(inner.preview_tail, "尾部。", "幸存尾部不得被清除或重裁");
        assert_eq!(
            inner.preview_tail_range,
            Some(AudioRange::new(32_000, 48_000))
        );
        assert_eq!(inner.latest_preview, "A。尾部。");
    }
    let second = engine.compose_typed_result().expect("compose ok");
    assert!(second.is_empty(), "重复内容不得再次发出");
}

/// 强停顿既有路径不回归：owned ≥ 2s + voiced ≥ 1.2s 在 700ms 停顿即接受
/// （早于 long_pause 的 1100ms），不受新规则影响。
#[test]
fn strong_pause_path_still_accepts_before_long_pause() {
    let inner = preview_draft_inner();
    assert!(
        inner
            .candidate_readiness(&candidate_ms(2_000, 700), usize::MAX / 2, 16_000)
            .is_ok()
    );
    // 无自然句尾证据的候选（soft_window）voiced 不足 1.2s 时 700ms 停顿
    // 仍不接受，等长静音兜底。
    let low_voiced = BoundaryCandidate {
        reason: "soft_window".to_string(),
        voiced_samples: 1_000 * 16,
        ..candidate_ms(2_000, 700)
    };
    assert!(
        inner
            .candidate_readiness(&low_voiced, usize::MAX / 2, 16_000)
            .is_err()
    );
}

/// 0.23.14 核心场景：短句（< min_sentence 800ms）后保持长静音，静音期间
/// 必须产生可靠 Draft——不再等复语或会话结束。修复前 ShortPhraseEnd 只
/// 冻结预览短语，短句 + 任意长度静音都无法定稿。
#[tokio::test]
async fn short_utterance_long_pause_finalizes_before_resume() {
    let channels: Vec<(
        oneshot::Sender<Result<String, String>>,
        oneshot::Receiver<Result<String, String>>,
    )> = (0..2).map(|_| oneshot::channel()).collect();
    let (mut senders, receivers): (Vec<_>, Vec<_>) = channels.into_iter().unzip();
    let transport = ControlledTransport::new(receivers);
    let engine = preview_draft_engine(transport.clone());

    let feed = |source: Vec<f32>| {
        let engine = &engine;
        async move {
            for chunk in source.chunks(160) {
                engine.transcribe_chunk(chunk).await.expect("chunk ok");
            }
        }
    };

    // 600ms 语音（< min_sentence）+ 400ms 停顿：ShortPhraseEnd 在 300ms
    // 处冻结短语（调用 1），静默尚未达到 long_pause——候选仍在等待。
    feed(vec![0.1f32; 16_000 * 6 / 10]).await;
    feed(vec![0.0f32; 16_000 * 4 / 10]).await;
    transport.wait_for_calls_or_fail(1).await;
    senders
        .remove(0)
        .send(Ok("短语预览。".to_string()))
        .expect("sender 不应泄漏");
    wait_until(|| {
        let inner = engine.inner.lock().unwrap();
        inner.preview_phrases.len() == 1
    })
    .await;

    // 继续静音 1.6s：quiet 达到 1100ms → long_pause 接受候选 → Draft 起飞
    // （调用 2）。全部发生在静音期间，未复语。
    feed(vec![0.0f32; 16_000 * 16 / 10]).await;
    transport.wait_for_calls_or_fail(2).await;
    // 应答 Draft：committed 水位在静音期间推进
    senders
        .remove(0)
        .send(Ok("短句定稿。".to_string()))
        .expect("sender 不应泄漏");
    wait_until(|| engine.stream_stats().pcm_committed_end > 0).await;
    {
        let inner = engine.inner.lock().unwrap();
        assert!(
            inner.sentences.committed_sample_end >= 16_000 * 6 / 10,
            "Draft 覆盖整个短句音频"
        );
        assert_eq!(
            inner.sentences.draft_spans().len(),
            1,
            "静音期间产生 1 个可靠 Draft span"
        );
        // Draft 提交 settle 清退被覆盖的短语 span
        assert!(
            inner.preview_phrases.is_empty(),
            "Draft 覆盖范围内短语账本必须清退"
        );
    }

    // 终态：剩余纯静音裁剪后无新调用；最终文本即 Draft 正文
    let final_text = engine.finalize().await.expect("finalize ok");
    assert_eq!(final_text, "短句定稿。");
}

/// 噪声门兜底：键盘/点击类短脉冲（有效有声 < 300ms）+ 长静音不得产生
/// Draft span；模型对残余脉冲返回静音标记时按 NoSpeech 消费，最终为空。
#[tokio::test]
async fn short_word_long_pause_survives_noise_gate() {
    let (sender, receiver) = oneshot::channel();
    let transport = ControlledTransport::new(vec![receiver]);
    let engine = preview_draft_engine(transport.clone());

    // 3 × 40ms 脉冲（间隔 60ms）≈ 120ms 有效有声 < 300ms 可信下限
    let mut clicks = Vec::new();
    for _ in 0..3 {
        clicks.extend(vec![0.2f32; 16_000 * 40 / 1000]);
        clicks.extend(vec![0.0f32; 16_000 * 60 / 1000]);
    }
    let silence = vec![0.0f32; 16_000 * 2];
    for source in [&clicks, &silence] {
        for chunk in source.chunks(160) {
            engine.transcribe_chunk(chunk).await.expect("chunk ok");
        }
    }

    // 静音期间不得发起任何识别：脉冲有声不足以冻结短语（<500ms），
    // 长静音候选因 voiced < 300ms 不被接受
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        transport.calls.load(Ordering::SeqCst),
        0,
        "噪声脉冲 + 长静音不得触发短语/预览/Draft 推理"
    );
    {
        let inner = engine.inner.lock().unwrap();
        assert!(
            inner.sentences.draft_spans().is_empty(),
            "长静音不得把短脉冲升级为 Draft"
        );
    }

    // 终态：terminal 对残余脉冲识别，模型返回静音标记 → 最终为空
    sender
        .send(Ok("/sil".to_string()))
        .expect("sender 不应泄漏");
    let final_text = engine.finalize().await.expect("finalize ok");
    assert_eq!(final_text, "", "噪声脉冲不得进入最终文本");
}

/// 复语作废长静音候选：短句 + 静音不足 long_pause 即复语 → 候选作废，
/// 不产生 Draft（句内 300～800ms 停顿语义保持）。回归矩阵覆盖 400ms 与
/// 500ms 两档词间停顿——短句候选只有 short_phrase/strong/long 三条路，
/// 自然句尾路径不适用于 < min_sentence 的短句。
#[tokio::test]
async fn short_utterance_short_pause_does_not_draft() {
    for pause_ms in [4u64, 5] {
        let channels: Vec<(
            oneshot::Sender<Result<String, String>>,
            oneshot::Receiver<Result<String, String>>,
        )> = (0..2).map(|_| oneshot::channel()).collect();
        let (mut senders, receivers): (Vec<_>, Vec<_>) = channels.into_iter().unzip();
        let transport = ControlledTransport::new(receivers);
        let engine = preview_draft_engine(transport.clone());

        let feed = |source: Vec<f32>| {
            let engine = &engine;
            async move {
                for chunk in source.chunks(160) {
                    engine.transcribe_chunk(chunk).await.expect("chunk ok");
                }
            }
        };

        // 600ms 语音 + 400/500ms 停顿（< 700ms strong / < 1100ms long）+ 复语 1s
        feed(vec![0.1f32; 16_000 * 6 / 10]).await;
        feed(vec![0.0f32; (16_000 * pause_ms / 10) as usize]).await;
        feed(vec![0.1f32; 16_000]).await;

        // 只有短语定稿识别（ShortPhraseEnd 冻结），无 Draft
        transport.wait_for_calls_or_fail(1).await;
        senders
            .remove(0)
            .send(Ok("短语。".to_string()))
            .expect("sender 不应泄漏");
        wait_until(|| {
            let inner = engine.inner.lock().unwrap();
            !inner.preview_phrases.is_empty()
        })
        .await;
        {
            let inner = engine.inner.lock().unwrap();
            assert!(
                inner.sentences.draft_spans().is_empty(),
                "{pause_ms}00ms 停顿 + 复语不得产生 Draft"
            );
            assert!(inner.boundary_candidate.is_none(), "复语作废短句候选");
        }

        senders
            .remove(0)
            .send(Ok("终稿。".to_string()))
            .expect("sender 不应泄漏");
        let final_text = engine.finalize().await.expect("finalize ok");
        assert_eq!(final_text, "终稿。");
        engine.reset();
    }
}

/// 引擎内候选裁决诊断回放（BLINK_STT_CORPUS_DIR 门控）：case_12 逐 100ms
/// 块喂入 mock transport 驱动的引擎，轮询候选 quiet/voiced/readiness 的
/// 演化——呈现"VAD SentenceEnd → 采纳/等待/作废"的完整链路。确定性
/// （无真实 worker），与 g2_projection 真实回放的 decision observer 互补：
/// 后者给出生产端到端时间线，本测试解释每个候选为何等待。
#[tokio::test(flavor = "multi_thread")]
async fn case12_engine_candidate_timeline_diagnostic() {
    let Some(dir) = std::env::var("BLINK_STT_CORPUS_DIR")
        .ok()
        .map(std::path::PathBuf::from)
    else {
        return;
    };
    let wav_path = std::fs::read_dir(&dir)
        .expect("corpus dir")
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .find(|path| {
            path.extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("wav"))
                && path
                    .file_name()
                    .is_some_and(|name| name.to_string_lossy().contains("多句话"))
        })
        .expect("case_12 wav");
    let wav = std::fs::read(&wav_path).expect("read wav");
    let decoded = crate::domain::stt::wav::decode_wav(&wav).expect("decode");
    let mut normalizer =
        crate::infra::platform::audio::normalize::AudioNormalizer::from_source_format(
            decoded.format,
        );
    let mut samples = normalizer.process(&decoded.samples);
    samples.extend(normalizer.finish());

    let (sender, receiver) = oneshot::channel();
    let transport = ControlledTransport::new(vec![receiver]);
    let engine = preview_draft_engine(transport.clone());

    let mut fed = 0usize;
    for chunk in samples.chunks(1600) {
        engine.transcribe_chunk(chunk).await.expect("chunk ok");
        fed += chunk.len();
        let inner = engine.inner.lock().unwrap();
        let total = inner.sentences.buffer_base_sample + inner.samples.len();
        if let Some(candidate) = &inner.boundary_candidate {
            let readiness = inner.candidate_readiness(candidate, total, 16_000);
            println!(
                "t={:>5}ms cand reason={} quiet={:>4}ms voiced={:>4}ms ready={:?} anchor={}ms",
                fed * 1000 / 16_000,
                candidate.reason,
                candidate.quiet_samples * 1000 / 16_000,
                candidate.voiced_samples * 1000 / 16_000,
                readiness.err().unwrap_or("ACCEPT"),
                inner.phrase_anchor * 1000 / 16_000,
            );
        } else {
            println!(
                "t={:>5}ms (no candidate) anchor={}ms tail_chars={}",
                fed * 1000 / 16_000,
                inner.phrase_anchor * 1000 / 16_000,
                inner.preview_tail.chars().count(),
            );
        }
    }
    drop(sender);
}

// ── 0.23.14 P0-2 PreviewSpan 账本 + 事务式锚点 ─────────────────────

/// 短语识别失败不得永久推进锚点：error 回退到冻结前位置，下一次短语
/// 事件重新覆盖该音频（合并重识别）。修复前 error 被静默吞掉且锚点已
/// 推进——该段音频从预览与后续识别中双重丢失。
#[tokio::test]
async fn phrase_error_does_not_advance_committed_anchor() {
    let channels: Vec<(
        oneshot::Sender<Result<String, String>>,
        oneshot::Receiver<Result<String, String>>,
    )> = (0..2).map(|_| oneshot::channel()).collect();
    let (mut senders, receivers): (Vec<_>, Vec<_>) = channels.into_iter().unzip();
    let transport = ControlledTransport::new(receivers);
    let engine = preview_draft_engine(transport.clone());

    let feed = |source: Vec<f32>| {
        let engine = &engine;
        async move {
            for chunk in source.chunks(160) {
                engine.transcribe_chunk(chunk).await.expect("chunk ok");
            }
        }
    };

    // 600ms 语音 + 400ms 停顿 → ShortPhraseEnd 冻结短语（锚点投机推进到 600ms）
    feed(vec![0.1f32; 16_000 * 6 / 10]).await;
    feed(vec![0.0f32; 16_000 * 4 / 10]).await;
    transport.wait_for_calls_or_fail(1).await;
    // 识别失败（transport error）
    senders
        .remove(0)
        .send(Err("worker down".to_string()))
        .expect("sender 不应泄漏");
    wait_until(|| engine.inner.lock().unwrap().pending_phrase.is_none()).await;
    {
        let inner = engine.inner.lock().unwrap();
        assert_eq!(
            inner.phrase_anchor, 0,
            "短语识别失败必须回退锚点（修复前停在 600ms，前缀丢失）"
        );
        assert!(inner.preview_phrases.is_empty());
    }

    // 复语 + 再停顿：下一次短语从回退后的锚点（0）起合并覆盖
    feed(vec![0.1f32; 16_000 * 6 / 10]).await;
    feed(vec![0.0f32; 16_000 * 4 / 10]).await;
    transport.wait_for_calls_or_fail(2).await;
    senders
        .remove(0)
        .send(Ok("合并后短语。".to_string()))
        .expect("sender 不应泄漏");
    wait_until(|| {
        let inner = engine.inner.lock().unwrap();
        !inner.preview_phrases.is_empty()
    })
    .await;
    {
        let inner = engine.inner.lock().unwrap();
        assert_eq!(inner.preview_phrases.len(), 1);
        assert_eq!(
            inner.preview_phrases[0].range.start_sample, 0,
            "重试必须从回退锚点起合并覆盖原音频"
        );
        assert!(inner.latest_preview.contains("合并后短语。"));
    }
}

/// 空文本短语同样回退锚点，等下一次短语合并重识别——不静默推进、不丢前缀。
#[tokio::test]
async fn empty_phrase_merges_or_retries_without_prefix_loss() {
    let channels: Vec<(
        oneshot::Sender<Result<String, String>>,
        oneshot::Receiver<Result<String, String>>,
    )> = (0..2).map(|_| oneshot::channel()).collect();
    let (mut senders, receivers): (Vec<_>, Vec<_>) = channels.into_iter().unzip();
    let transport = ControlledTransport::new(receivers);
    let engine = preview_draft_engine(transport.clone());

    let feed = |source: Vec<f32>| {
        let engine = &engine;
        async move {
            for chunk in source.chunks(160) {
                engine.transcribe_chunk(chunk).await.expect("chunk ok");
            }
        }
    };

    // 首次短语识别返回空文本
    feed(vec![0.1f32; 16_000 * 6 / 10]).await;
    feed(vec![0.0f32; 16_000 * 4 / 10]).await;
    transport.wait_for_calls_or_fail(1).await;
    senders
        .remove(0)
        .send(Ok(String::new()))
        .expect("sender 不应泄漏");
    wait_until(|| {
        let inner = engine.inner.lock().unwrap();
        inner.pending_phrase.is_none() && inner.phrase_anchor == 0
    })
    .await;

    // 下一次短语事件覆盖完整范围（含空文本段），结果入账
    feed(vec![0.1f32; 16_000 * 6 / 10]).await;
    feed(vec![0.0f32; 16_000 * 4 / 10]).await;
    transport.wait_for_calls_or_fail(2).await;
    senders
        .remove(0)
        .send(Ok("重试短语。".to_string()))
        .expect("sender 不应泄漏");
    wait_until(|| {
        let inner = engine.inner.lock().unwrap();
        inner.preview_phrases.len() == 1 && inner.preview_phrases[0].range.start_sample == 0
    })
    .await;
}

/// settle 按范围清退：完全覆盖（end ≤ committed）的 span 退场，边界之后
/// 的 span 保留可见；跨界 span（start < committed < end）整条删除——
/// 剩余部分交回尾部窗口重新识别，不做字符串硬裁剪。
#[test]
fn preview_span_settle_removes_only_covered_ranges() {
    let mut inner = preview_draft_inner();
    inner.preview_phrases = vec![
        PreviewSpan {
            range: AudioRange::new(0, 10_000),
            text: "第一段。".into(),
        },
        PreviewSpan {
            range: AudioRange::new(10_000, 20_000),
            text: "第二段。".into(),
        },
        PreviewSpan {
            range: AudioRange::new(25_000, 35_000),
            text: "边界后。".into(),
        },
    ];
    inner.latest_preview = "第一段。第二段。边界后。".into();
    inner.phrase_anchor = 35_000;
    inner.sentences.committed_sample_end = 20_000;

    inner.settle_preview_after_commit();

    assert_eq!(inner.preview_phrases.len(), 1, "只保留边界后的 span");
    assert_eq!(inner.preview_phrases[0].range.start_sample, 25_000);
    assert_eq!(inner.preview_phrases[0].text, "边界后。");
    assert_eq!(inner.phrase_anchor, 35_000, "锚点不低于已提交水位");
    assert!(
        inner.latest_preview.contains("边界后。"),
        "未覆盖后缀保持可见（实际 {:?}）",
        inner.latest_preview
    );
}

/// 谷底回退产生跨界 span（start < committed < end）时整条删除，锚点收敛
/// 到已提交水位——尾部窗口从新锚点重新覆盖剩余音频，不保留已消费前缀
/// 的旧文本（修复前按 end > committed 保留整条，与新 Draft 文本重复）。
#[test]
fn retreated_boundary_preserves_uncommitted_preview_suffix() {
    let mut inner = preview_draft_inner();
    inner.preview_phrases = vec![
        PreviewSpan {
            range: AudioRange::new(0, 10_000),
            text: "已覆盖。".into(),
        },
        PreviewSpan {
            range: AudioRange::new(15_000, 30_000),
            text: "跨界 span。".into(),
        },
        PreviewSpan {
            range: AudioRange::new(30_000, 40_000),
            text: "回退后缀。".into(),
        },
    ];
    // 强制切回退谷底：committed 落在第二条 span 中间
    inner.sentences.committed_sample_end = 20_000;
    inner.phrase_anchor = 30_000;

    inner.settle_preview_after_commit();

    assert!(
        !inner.latest_preview.contains("跨界 span。"),
        "跨界 span 不得整条残留（实际 {:?}）",
        inner.latest_preview
    );
    assert!(
        inner.latest_preview.contains("回退后缀。"),
        "回退边界之后的 suffix 必须保留"
    );
    assert_eq!(inner.phrase_anchor, 30_000);
    // 在途短语同样按范围裁决
    inner.pending_phrase = Some(PendingPhrase {
        request_id: 1,
        anchor_before: 10_000,
        range: AudioRange::new(12_000, 22_000),
        generation: 0,
    });
    inner.sentences.committed_sample_end = 21_000;
    inner.settle_preview_after_commit();
    assert!(
        inner.pending_phrase.is_none(),
        "在途范围与 committed 跨界时必须作废登记"
    );
}

// ── 0.23.14 P0-3 真实优先调度 ─────────────────────────────────────

/// 排队后已过期的 Preview/Phrase 在拿到 worker gate 后、调用模型前被淘汰：
/// 边界接受（代际推进）后，排在 gate 上的短语任务让位 Draft，不产生
/// transport 调用。修复前过期短语完整推理（实测 487ms 浪费）后才被丢弃。
#[tokio::test]
async fn stale_preview_is_dropped_before_transport_call() {
    let channels: Vec<(
        oneshot::Sender<Result<String, String>>,
        oneshot::Receiver<Result<String, String>>,
    )> = (0..3).map(|_| oneshot::channel()).collect();
    let (mut senders, receivers): (Vec<_>, Vec<_>) = channels.into_iter().unzip();
    let transport = ControlledTransport::new(receivers);
    let engine = preview_draft_engine(transport.clone());

    // last_preview 从引擎创建起计时；越过刷新间隔后尾部调度才会触发
    tokio::time::sleep(Duration::from_millis(800)).await;

    let feed = |source: Vec<f32>| {
        let engine = &engine;
        async move {
            for chunk in source.chunks(160) {
                engine.transcribe_chunk(chunk).await.expect("chunk ok");
            }
        }
    };

    // 1.3s 语音 → 首个尾部预览起飞（调用 1，持有 gate，不响应保持 in-flight）
    feed(vec![0.1f32; 16_000 * 13 / 10]).await;
    transport.wait_for_calls_or_fail(1).await;

    // 300ms 停顿（候选，低于自然采纳下限）+ 复语 1s → 候选作废 →
    // 短语任务排队（等 gate）
    feed(vec![0.0f32; 16_000 * 3 / 10]).await;
    feed(vec![0.1f32; 16_000]).await;
    // 2s 长静音 → long_pause 接受边界（调用 2 = Draft，排在短语之后）。
    // 代际推进使短语任务 stale。
    feed(vec![0.0f32; 16_000 * 2]).await;

    // 应答首预览（合法在途任务，已过 gate 检查）——其结果因代际推进被丢弃
    senders
        .remove(0)
        .send(Ok("旧预览。".to_string()))
        .expect("sender 不应泄漏");
    // 短语任务拿 gate → 复核代际 → stale → 不调模型直接退出；
    // Draft 随后拿 gate → 调用 2
    transport.wait_for_calls_or_fail(2).await;
    senders
        .remove(0)
        .send(Ok("草稿定稿。".to_string()))
        .expect("sender 不应泄漏");
    wait_until(|| {
        let inner = engine.inner.lock().unwrap();
        inner.sentences.committed_sample_end > 0 && inner.pending_phrase.is_none()
    })
    .await;

    {
        let inner = engine.inner.lock().unwrap();
        assert!(
            inner.stale_before_worker >= 1,
            "排队任务必须在 gate 后、模型前被淘汰（实测 {}）",
            inner.stale_before_worker
        );
        assert!(inner.sentences.draft_spans().len() == 1, "Draft 正常落地");
    }
    // 全程只有 2 次模型调用：预览 + Draft；stale 短语未占用模型
    assert_eq!(
        transport.calls.load(Ordering::SeqCst),
        2,
        "stale 短语不得产生 transport 调用"
    );

    senders
        .remove(0)
        .send(Ok(String::new()))
        .expect("sender 不应泄漏");
    let final_text = engine.finalize().await.expect("finalize ok");
    assert_eq!(final_text, "草稿定稿。");
}

/// PreviewDraft 自适应刷新间隔：配置下限与 2×上轮推理耗时的较大值，
/// 且有 PREVIEW_MAX_INTERVAL_MS 上限——慢推理后不再以固定 700ms 积压。
#[test]
fn preview_refresh_interval_adapts_to_inference_elapsed() {
    assert_eq!(
        PseudoStreamingSttEngine::preview_refresh_interval(700, Duration::ZERO),
        Duration::from_millis(700),
        "快推理时使用配置下限"
    );
    assert_eq!(
        PseudoStreamingSttEngine::preview_refresh_interval(700, Duration::from_millis(600)),
        Duration::from_millis(1_200),
        "慢推理后冷却 2×耗时"
    );
    assert_eq!(
        PseudoStreamingSttEngine::preview_refresh_interval(700, Duration::from_secs(10)),
        Duration::from_millis(PREVIEW_MAX_INTERVAL_MS),
        "冷却受上限约束"
    );
}

// ── 0.23.14 P1 时间冻结切点优先谷底 ───────────────────────────────

/// 连续语音中的句内微停顿（120ms+，低于 min_silence 不产生候选）处
/// 优先冻结，而不是在滚动窗起点任意硬切——降低词中切断概率。
#[tokio::test]
async fn time_freeze_prefers_valley_cut_point() {
    let channels: Vec<(
        oneshot::Sender<Result<String, String>>,
        oneshot::Receiver<Result<String, String>>,
    )> = (0..2).map(|_| oneshot::channel()).collect();
    let (mut senders, receivers): (Vec<_>, Vec<_>) = channels.into_iter().unzip();
    let transport = ControlledTransport::new(receivers);
    let engine = preview_draft_engine(transport.clone());

    // 1.0s 语音 + 150ms 谷（< min_silence 300ms，不产生候选）+ 3.35s 语音
    // = 4.5s。total ≥ 4.2s 时时间冻结触发（roll_start = 1.5s ≥ 1.2s 前缀），
    // 谷底在 1.0s——切点应为 1.0s 而非 1.5s。
    let mut audio = vec![0.1f32; 16_000];
    audio.extend(vec![0.0f32; 16_000 * 15 / 100]);
    audio.extend(vec![0.1f32; 16_000 * 335 / 100]);
    for chunk in audio.chunks(160) {
        engine.transcribe_chunk(chunk).await.expect("chunk ok");
    }

    transport.wait_for_calls_or_fail(1).await;
    senders
        .remove(0)
        .send(Ok("谷底短语。".to_string()))
        .expect("sender 不应泄漏");
    wait_until(|| {
        let inner = engine.inner.lock().unwrap();
        !inner.preview_phrases.is_empty()
    })
    .await;
    {
        let inner = engine.inner.lock().unwrap();
        assert_eq!(inner.preview_phrases.len(), 1);
        // 0.23.14.6：切点推进到谷内稳健位置——不再贴着谷起点（1.0s）的
        // 下降沿，也不滚动窗硬切（1.5s）；偏移约束 [30ms, 150ms]。
        let phrase_end = inner.preview_phrases[0].range.end_sample;
        assert!(
            phrase_end > 16_000 + (16_000 * 30 / 1000) as u64
                && phrase_end <= 16_000 + (16_000 * 150 / 1000) as u64,
            "切点必须在谷内 (1.03s, 1.15s]（实测 {phrase_end}）"
        );
        assert_eq!(
            inner.phrase_anchor, phrase_end,
            "锚点推进到谷内切点，而非滚动窗起点"
        );
    }

    senders
        .remove(0)
        .send(Ok("终稿。".to_string()))
        .expect("sender 不应泄漏");
    let final_text = engine.finalize().await.expect("finalize ok");
    assert_eq!(final_text, "终稿。");
}

/// 0.23.10.2 尾部清空从"边界接受"迁移到"真实提交"：Draft 推理在途的
/// 数百毫秒内已显示的尾部虚字必须保留，提交（confirmed 增长/NoSpeech
/// 消费推进 committed）时才清退。修复前接受即清空，用户看到文字先消失
/// 再以实字重现（08:32 实测 21 字符 → 9 → 0 的回缩闪烁）。
#[tokio::test]
async fn tail_survives_boundary_accept_until_commit() {
    let channels: Vec<(
        oneshot::Sender<Result<String, String>>,
        oneshot::Receiver<Result<String, String>>,
    )> = (0..3).map(|_| oneshot::channel()).collect();
    let (mut senders, receivers): (Vec<_>, Vec<_>) = channels.into_iter().unzip();
    let transport = ControlledTransport::new(receivers);
    let engine = preview_draft_engine(transport.clone());

    // last_preview 从引擎创建起计时，越过刷新间隔后尾部调度才会触发
    tokio::time::sleep(Duration::from_millis(800)).await;

    // 2.2s 连续语音：首预览在 1.2s 门槛处起飞（调用 1），后续音频按
    // 500ms 限频排队替换快照——与 08:32 实测会话的调度形态一致。
    let speech = vec![0.1f32; 16_000 * 22 / 10];
    for chunk in speech.chunks(160) {
        engine.transcribe_chunk(chunk).await.expect("chunk ok");
    }
    transport.wait_for_calls_or_fail(1).await;
    senders
        .remove(0)
        .send(Ok("尾部预览。".to_string()))
        .expect("sender 不应泄漏");
    wait_until(|| engine.inner.lock().unwrap().latest_preview == "尾部预览。").await;
    // 让完成 task 走完收尾（释放 owner/gate）再喂停顿——真实录音里音频
    // 块每 10ms 一个，任务天然有调度窗口；测试同步喂入必须显式让出。
    tokio::time::sleep(Duration::from_millis(20)).await;

    // 800ms 强停顿：排队快照任务在首块接管；静默 700ms 处候选升级
    // strong（owned 2.2s ≥ 2s）→ 边界接受（代际推进）→ Draft 定稿起飞。
    // 0.23.14：current-thread runtime 下 spawn 的任务只在挂起点被调度，
    // 排队快照拿到 gate 时代际已推进 → gate 后、模型前被淘汰（不产生
    // 调用），Draft 直接成为调用 2。
    let pause = vec![0.0f32; 16_000 * 8 / 10];
    for chunk in pause.chunks(160) {
        engine.transcribe_chunk(chunk).await.expect("chunk ok");
    }
    transport.wait_for_calls_or_fail(2).await;

    // Draft 在途（尚未应答）：尾部必须原样保留——修复前接受即清空，
    // 已显示的虚字会先消失、等 Draft 提交后再以实字重现
    {
        let inner = engine.inner.lock().unwrap();
        assert_eq!(
            inner.preview_tail, "尾部预览。",
            "边界接受不得清空尾部（Draft 在途期间保持显示连续）"
        );
        assert!(
            inner.sentences.finalize_in_flight,
            "前置条件：Draft 定稿确已起飞"
        );
    }

    // 提交：confirmed 增长 → settle 清退尾部
    senders
        .remove(0)
        .send(Ok("草稿正文。".to_string()))
        .expect("sender 不应泄漏");
    wait_until(|| {
        let inner = engine.inner.lock().unwrap();
        inner.sentences.committed_sample_end > 0
            && inner.preview_tail.is_empty()
            && inner.latest_preview.is_empty()
    })
    .await;

    let final_text = engine.finalize().await.expect("finalize ok");
    assert_eq!(final_text, "草稿正文。");
    assert_eq!(
        transport.calls.load(std::sync::atomic::Ordering::SeqCst),
        2,
        "首预览 + Draft 两次推理；排队快照在 gate 后被淘汰不占模型"
    );
    assert!(
        engine.inner.lock().unwrap().stale_before_worker >= 1,
        "过期排队预览必须在模型前被淘汰"
    );
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
        decision_observer: None,
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
        decision_observer: None,
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

/// finalize 后补收未上报 Draft：drain 只取游标之后的 span，取后不重复出现。
/// 对应 VAD 调试回放在 finalize 阶段补收尾段草稿的语义。
#[test]
fn drain_unreported_draft_spans_returns_only_after_cursor() {
    let engine = engine_without_transport(vec![]);
    {
        let mut inner = engine.inner.lock().unwrap();
        for (end, text) in [(1000usize, "第一句"), (2000, "尾段")] {
            let pending = inner
                .sentences
                .on_sentence_end(end, "")
                .expect("句尾应创建 pending");
            inner.sentences.commit_or_rollback(&FinalizeResult {
                identity: pending.identity,
                text: text.to_string(),
                ok: true,
            });
        }
    }
    // 模拟 chunk 循环已经逐块上报了第一个 span
    engine.inner.lock().unwrap().last_reported_span_count = 1;

    let drained = engine.drain_unreported_draft_spans();
    assert_eq!(drained.len(), 1, "只应取走游标之后的尾段 Draft");
    assert_eq!(drained[0].text, "尾段");
    assert_eq!(drained[0].audio_range.end_sample, 2000);
    assert!(
        engine.drain_unreported_draft_spans().is_empty(),
        "取走后不得重复出现"
    );
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
        decision_observer: None,
        finalize_observer: None,
    };
    engine.inner.lock().unwrap().last_preview = Instant::now() + Duration::from_secs(600);
    engine
}

/// 同 valley_engine，但切到 PreviewDraft profile（开启候选/可信度证据链）。
fn valley_engine_preview_draft(
    transport: Arc<WavCaptureTransport>,
) -> PseudoStreamingSttEngine {
    let engine = valley_engine(transport);
    {
        let mut inner = engine.inner.lock().unwrap();
        inner.coordinator.profile = RecognitionProfile::PreviewDraft;
        inner.sentences.set_draft_range_reservation(true);
    }
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

// ── 0.23.14.6 复合 Preview 范围契约 ──────────────────────────────

/// 组合预览（A + B + tail）的对外信封必须覆盖全部可见文本对应的音频范围，
/// 并携带逐段 span 清单——非空预览禁止零长度退化区间；Draft settle 只清退
/// 被覆盖范围，剩余 span 的信封仍非退化（0.23.14.5 缺陷：settle 后
/// latest_preview_range 被清 None，对外退化为 (committed, committed)）。
#[test]
fn composed_preview_envelope_covers_all_spans_and_settles_nonzero() {
    let engine = engine_without_transport(Vec::new());
    {
        let mut inner = engine.inner.lock().unwrap();
        inner.preview_phrases = vec![
            PreviewSpan::new(AudioRange::new(0, 160_000), "第一短语。"),
            PreviewSpan::new(AudioRange::new(160_000, 320_000), "第二短语。"),
        ];
        inner.preview_tail = "尾部预览。".into();
        inner.preview_tail_range = Some(AudioRange::new(320_000, 368_000));
        inner.rebuild_preview_text();
    }
    let payload = engine.compose_typed_result().expect("compose ok");
    let value: serde_json::Value = serde_json::from_str(&payload).expect("json");
    assert_eq!(value["kind"], "preview");
    let envelope = &value["audioRange"];
    assert_eq!(envelope["startSample"], 0, "信封必须覆盖首个 span 起点");
    assert_eq!(envelope["endSample"], 368_000, "信封必须覆盖尾部 span 终点");
    let spans = value["spans"].as_array().expect("spans 清单");
    assert_eq!(spans.len(), 3, "短语 A/B + 尾部逐段携带");
    assert_eq!(spans[1]["text"], "第二短语。");

    // Draft 只覆盖 A（committed 推进到 160_000）：A 退场、B 保留，尾部被
    // 消费清空——剩余组合预览的信封仍必须覆盖 B 的完整范围。
    {
        let mut inner = engine.inner.lock().unwrap();
        inner.sentences.committed_sample_end = 160_000;
        inner.settle_preview_after_commit();
    }
    let payload = engine.compose_typed_result().expect("compose after settle");
    let value: serde_json::Value = serde_json::from_str(&payload).expect("json");
    let envelope = &value["audioRange"];
    assert_eq!(
        envelope["startSample"], 160_000,
        "settle 后剩余 span 的信封起点 = B 起点"
    );
    assert_eq!(envelope["endSample"], 320_000);
    assert_eq!(value["spans"].as_array().expect("spans").len(), 1);
    assert_eq!(value["text"], "第二短语。");
}

/// 消费方按 span 范围 settle（共享函数语义）：Draft 覆盖前缀时 A 退场、
/// B 与尾部保留；跨界段整条删除；不做字符串裁剪，也不需要引擎下一个
/// Preview 事件即可完成一致投影（杜绝 `confirmed=A, preview=A+B+C`）。
#[test]
fn settle_preview_segments_keeps_uncovered_suffix() {
    use crate::domain::stt::{PreviewSegment, settle_preview_segments};
    let mut segments = vec![
        PreviewSegment::new(AudioRange::new(0, 160_000), "A"),
        PreviewSegment::new(AudioRange::new(160_000, 320_000), "B"),
        PreviewSegment::new(AudioRange::new(320_000, 368_000), "尾部"),
    ];
    // Draft 提交边界 = A 终点。
    settle_preview_segments(&mut segments, 160_000);
    let projection: String = segments.iter().map(|s| s.text.as_str()).collect();
    assert_eq!(projection, "B尾部", "未覆盖后缀必须保留");
    // 跨界段（start < boundary < end）整条删除。
    let mut crossing = vec![
        PreviewSegment::new(AudioRange::new(150_000, 300_000), "跨界"),
        PreviewSegment::new(AudioRange::new(300_000, 360_000), "后缀"),
    ];
    settle_preview_segments(&mut crossing, 200_000);
    assert_eq!(crossing.len(), 1);
    assert_eq!(crossing[0].text, "后缀");
}

// ── 0.23.14.6 时间冻结：合格谷枚举 + 谷内切点 ────────────────────

fn speech_samples(ms: u64) -> Vec<f32> {
    vec![0.1f32; 16_000 * ms as usize / 1000]
}

fn silence_samples(ms: u64) -> Vec<f32> {
    vec![0.0f32; 16_000 * ms as usize / 1000]
}

/// 0.23.14.5 缺陷回归：100ms 语音 + 120ms 微停顿 + 4s 连续语音——唯一谷
/// 前有声不足 500ms（不合格），不得永久卡住时间冻结；必须回退 roll_start
/// 安全切点让锚点推进。
#[test]
fn early_unqualified_valley_falls_back_to_roll_start() {
    let mut samples = speech_samples(100);
    samples.extend(silence_samples(120));
    samples.extend(speech_samples(4_000));
    let roll_start = (samples.len() - 16_000 * 3) as u64; // total 4.22s - 3s 窗口
    let prefix = &samples[..roll_start as usize];
    let cut = PseudoInner::choose_time_freeze_cut(prefix, 0, roll_start, 0.01, 16_000);
    assert_eq!(cut, roll_start, "无合格谷必须回退 roll_start，锚点推进");
}

/// 多谷场景：早期谷（前有声 100ms）不合格必须跳过，选择后续合格谷
/// （前有声 700ms ≥ 500ms）；切点落在谷内稳健位置（30–150ms），不贴
/// 刚跌破阈值的下降沿。
#[test]
fn multiple_valleys_skip_unqualified_early_valley() {
    let mut samples = speech_samples(100);
    samples.extend(silence_samples(120));
    samples.extend(speech_samples(600));
    samples.extend(silence_samples(150));
    samples.extend(speech_samples(3_000));
    // total = 3.97s，滚动窗 3s → roll_start = 970ms，合格谷（820ms）完整
    // 落在前缀内；选择器只应看到 [anchor, roll_start) 前缀（生产口径）。
    let roll_start = (samples.len() - 16_000 * 3) as u64;
    let prefix = &samples[..roll_start as usize];
    let cut = PseudoInner::choose_time_freeze_cut(prefix, 0, roll_start, 0.01, 16_000);
    let valley_start = 16_000 * 820 / 1000; // 100 + 120 + 600ms
    let min_offset = 16_000 * 30 / 1000;
    let max_offset = 16_000 * 150 / 1000;
    assert!(
        cut > valley_start + min_offset as u64 && cut <= valley_start + max_offset as u64,
        "必须选择后续合格谷的谷内切点（谷起点 {valley_start} + 30~150ms，实测 {cut}）"
    );
}

/// 稳健谷内偏移的边界约束：[30ms, 150ms]；谷本身不足 30ms 时取一半。
#[test]
fn robust_in_valley_offset_bounds() {
    let valley = silence_samples(300);
    let offset = PseudoInner::robust_in_valley_offset(&valley, 16_000);
    assert!(
        offset >= 16_000 * 30 / 1000,
        "至少谷起点后 30ms（实测 {offset}）"
    );
    assert!(
        offset <= 16_000 * 150 / 1000,
        "低能量尾音受 150ms 上限保护（实测 {offset}）"
    );

    let short = silence_samples(20);
    assert_eq!(
        PseudoInner::robust_in_valley_offset(&short, 16_000),
        short.len() / 2
    );
}

/// E2E：早期微停顿 + 连续长语音不得永久卡住时间冻结——前缀经 roll_start
/// 回退冻结成短语，预览按短语增量累积而不是滚出窗口。
#[tokio::test]
async fn early_valley_does_not_stall_time_freeze() {
    let channels: Vec<(
        oneshot::Sender<Result<String, String>>,
        oneshot::Receiver<Result<String, String>>,
    )> = (0..2).map(|_| oneshot::channel()).collect();
    let (mut senders, receivers): (Vec<_>, Vec<_>) = channels.into_iter().unzip();
    let transport = ControlledTransport::new(receivers);
    let engine = preview_draft_engine(transport.clone());

    let mut audio = speech_samples(100);
    audio.extend(silence_samples(120));
    audio.extend(speech_samples(4_000));
    for chunk in audio.chunks(160) {
        engine.transcribe_chunk(chunk).await.expect("chunk ok");
    }

    transport.wait_for_calls_or_fail(1).await;
    senders
        .remove(0)
        .send(Ok("前缀短语。".to_string()))
        .expect("sender 不应泄漏");
    wait_until(|| {
        let inner = engine.inner.lock().unwrap();
        !inner.preview_phrases.is_empty()
    })
    .await;
    {
        let inner = engine.inner.lock().unwrap();
        let anchor = inner.phrase_anchor;
        assert!(
            anchor >= (16_000 * 12 / 10) as u64,
            "锚点必须推进（roll_start 回退），不得卡在 0（实测 {anchor}）"
        );
    }

    senders
        .remove(0)
        .send(Ok("终稿。".to_string()))
        .expect("sender 不应泄漏");
    let final_text = engine.finalize().await.expect("finalize ok");
    assert_eq!(final_text, "终稿。");
}

// ── 0.23.14.6 相等停顿门槛不遮蔽强停顿保护 ──────────────────────

/// `long == strong` 输入被 sanitize 拉开后（long = strong + 50ms），
/// 停顿落在 [strong, long) 窗口必须走强停顿路径——300ms 有声不得绕过
/// `owned ≥ 2s / voiced ≥ 1.2s` 保护直接定稿。
#[test]
fn equalized_pause_thresholds_keep_strong_guard_reachable() {
    let mut inner = preview_draft_inner();
    let mut settings = RecognitionSettings::default();
    settings.strong_pause_ms = 1_200;
    settings.long_pause_ms = 1_200;
    let sanitized = settings.sanitize(12);
    assert!(sanitized.long_pause_ms > sanitized.strong_pause_ms);
    inner.coordinator.settings = sanitized;

    // quiet 1.22s：strong 已达、long（1250ms）未达 → 强停顿路径。
    // （owned 2.2s、voiced 仅 300ms——短句后停顿落入强停顿窗口。）
    let in_strong_window = BoundaryCandidate {
        boundary_sample: 2_200 * 16,
        quiet_start_sample: (2_200 - 1_220) * 16,
        reason: "natural_silence".to_string(),
        voiced_samples: 300 * 16,
        strong_samples: 300 * 16,
        strong_run_max_samples: 300 * 16,
        quiet_samples: 1_220 * 16,
    };
    assert!(
        inner
            .candidate_readiness(&in_strong_window, usize::MAX / 2, 16_000)
            .is_err(),
        "300ms 有声不得在 [strong, long) 窗口绕过强停顿保护"
    );
    // quiet 达到 sanitize 后的 long：长静音独立终结照常接受。
    let at_long = BoundaryCandidate {
        boundary_sample: 2_200 * 16,
        quiet_start_sample: (2_200 - 1_250) * 16,
        reason: "natural_silence".to_string(),
        voiced_samples: 300 * 16,
        strong_samples: 300 * 16,
        strong_run_max_samples: 300 * 16,
        quiet_samples: 1_250 * 16,
    };
    assert!(
        inner
            .candidate_readiness(&at_long, usize::MAX / 2, 16_000)
            .is_ok()
    );
}
