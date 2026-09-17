//! GGUF 伪流式/非流式引擎的 `StreamingSttPort` 适配器（0.22.9 Handoff 05）。
//!
//! 将现有的 `SttEngine` trait（`transcribe_chunk` / `finalize` / `reset`）
//! 包装为新的 `StreamingSttPort`，使 VoiceService 只消费统一事件。
//!
//! ## 行为
//!
//! - `begin_session` → reset 引擎，递增 generation
//! - `push_audio` → 调用 `transcribe_chunk`，解析 JSON 结果产出 `Partial` 事件
//! - `finish_session` → 调用 `finalize`，产出 `Final` 事件
//! - `cancel_session` → reset 引擎，递增 generation（旧 generation 结果被丢弃）
//! - `reset` → reset 引擎
//!
//! `supports_native_partial` 返回 `false`——伪流式的 partial 由 VAD + 定时预览产生，
//! 不是模型原生流式输出。
//!
//! ## 事件通道：有界 + 可合并 + 只保留最新值（0.24）
//!
//! 事件量必须随「状态变化次数」增长，而不是随音频块数增长（10ms/chunk = 100/s）。
//! 三层防护：
//!
//! 1. **边沿触发**：引擎状态版本（`revision`）未变化的结果被抑制，不产出事件；
//! 2. **有界通道**：容量固定，队列深度有硬上界，不存在无界排队；
//! 3. **按重要性分流**：纯预览事件进入独立的 latest slot，满载时只替换待交付
//!    的旧值；**携带 confirmed 变化的事件绝不静默丢失**，降级为阻塞发送形成背压。
//!
//! 终态事件（`Final`/`Error`）永远走阻塞发送，不受合并策略影响。
//!
//! ## 并发安全
//!
//! 内部通过 `tokio::sync::Mutex` 串行化所有引擎调用，确保同一时刻只有一个操作。
//! `push_audio` 不阻塞调用方——音频采样在独立 task 中通过 channel 转发。

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use tokio::sync::{Mutex as TokioMutex, Notify, mpsc, oneshot};

use super::{
    AudioRange, DraftSpan, PreviewSegment, RecognitionProfile, StreamingSttPort, SttEngine,
    SttError, SttEvent, SttStreamStats,
};

/// 事件通道容量（有界）。256 足以吸收正常消费抖动；溢出即按重要性分流。
pub(crate) const EVENT_CHANNEL_CAPACITY: usize = 256;

/// 队列已满时的分流决策。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OverflowPolicy {
    /// 携带 confirmed 变化的事件：降级为阻塞发送形成背压，绝不丢弃。
    Backpressure,
    /// 纯预览事件：替换待交付值（"只保留最新值"，消费恢复后补发）。
    CoalesceLatest,
}

/// 队列已满时的分流规则（单一真源，纯函数）。
fn overflow_policy(confirmed_bearing: bool) -> OverflowPolicy {
    if confirmed_bearing {
        OverflowPolicy::Backpressure
    } else {
        OverflowPolicy::CoalesceLatest
    }
}

/// 上一次观察到的状态（用于边沿触发去重，与成功交付状态分离）。
#[derive(Default)]
struct ObservedState {
    /// 引擎状态版本（None = 引擎未提供版本号，退化为文本比较）。
    revision: Option<u64>,
    /// 最近一次收到的累计 confirmed（引擎在未变化时发空串，这里保留缓存值）。
    confirmed: String,
    /// 最近一次收到的预览文本。
    preview: String,
}

struct EventStats {
    partials_emitted: Arc<AtomicU64>,
    partials_coalesced: Arc<AtomicU64>,
    partials_dropped_closed: Arc<AtomicU64>,
    confirmed_backpressure: Arc<AtomicU64>,
    max_queue_depth: Arc<AtomicUsize>,
}

struct ReliableEvent {
    event: SttEvent,
    delivered: Option<oneshot::Sender<bool>>,
}

struct EventBus {
    reliable_tx: mpsc::Sender<ReliableEvent>,
    preview_slot: std::sync::Mutex<Option<SttEvent>>,
    notify: Notify,
    active_generation: Arc<AtomicU64>,
    stats: Arc<EventStats>,
    queue_depth: AtomicUsize,
}

/// 引擎结果中新增的类型化 envelope。旧引擎仍使用 JSON v2 累计字段，
/// 适配器只在明确识别出 lane + 音频区间时走此协议，避免把旧 preview
/// 字符串误判为 Draft。
enum TypedResult {
    Draft(DraftSpan),
    Preview {
        request_id: u64,
        audio_range: AudioRange,
        revision: u64,
        text: String,
        spans: Vec<PreviewSegment>,
    },
}

fn object_u64(value: Option<&serde_json::Value>, names: &[&str]) -> Option<u64> {
    let object = value?.as_object()?;
    names
        .iter()
        .find_map(|name| object.get(*name).and_then(serde_json::Value::as_u64))
}

fn object_str(value: Option<&serde_json::Value>, names: &[&str]) -> Option<String> {
    let object = value?.as_object()?;
    names
        .iter()
        .find_map(|name| object.get(*name).and_then(serde_json::Value::as_str))
        .map(str::to_string)
}

fn nested_object<'a>(
    value: &'a serde_json::Value,
    names: &[&str],
) -> Option<&'a serde_json::Value> {
    names
        .iter()
        .find_map(|name| value.get(*name).filter(|candidate| candidate.is_object()))
}

fn parse_audio_range(
    root: &serde_json::Value,
    nested: Option<&serde_json::Value>,
) -> Option<AudioRange> {
    let range = nested_object(
        nested.unwrap_or(root),
        &["audio_range", "audioRange", "range"],
    )
    .or_else(|| nested_object(root, &["audio_range", "audioRange", "range"]));
    let start = object_u64(
        range,
        &["start_sample", "startSample", "audio_start", "audioStart"],
    )
    .or_else(|| {
        object_u64(
            nested,
            &["start_sample", "startSample", "audio_start", "audioStart"],
        )
    })
    .or_else(|| {
        object_u64(
            Some(root),
            &["start_sample", "startSample", "audio_start", "audioStart"],
        )
    })?;
    let end = object_u64(range, &["end_sample", "endSample", "audio_end", "audioEnd"])
        .or_else(|| {
            object_u64(
                nested,
                &["end_sample", "endSample", "audio_end", "audioEnd"],
            )
        })
        .or_else(|| {
            object_u64(
                Some(root),
                &["end_sample", "endSample", "audio_end", "audioEnd"],
            )
        })?;
    Some(AudioRange::new(start, end))
}

/// 解析 0.23.9 类型化结果。字段接受 camelCase 与 snake_case，方便
/// worker/测试逐步迁移；缺少明确 audio range 时返回 None，继续走 v2 兼容路径。
fn parse_typed_result(value: &serde_json::Value) -> Option<TypedResult> {
    let nested = nested_object(value, &["span", "draft_span", "draft"]);
    let preview_nested = nested_object(value, &["preview"]);
    let kind = object_str(Some(value), &["kind", "lane", "type"])
        .or_else(|| object_str(nested, &["kind", "lane", "type"]))
        .or_else(|| object_str(preview_nested, &["kind", "lane", "type"]))
        .map(|kind| kind.to_ascii_lowercase());

    let is_draft = matches!(kind.as_deref(), Some("draft") | Some("stable"))
        || nested.is_some_and(|_| {
            value.get("draft_span").is_some()
                || value.get("draft").is_some()
                || value.get("span").is_some()
        });
    let is_preview =
        matches!(kind.as_deref(), Some("preview")) || (preview_nested.is_some() && !is_draft);

    if is_draft {
        let audio_range = parse_audio_range(value, nested)?;
        let text = object_str(nested, &["text", "content"])
            .or_else(|| object_str(Some(value), &["text", "content"]))?;
        let revision = object_u64(nested, &["revision", "span_revision"])
            .or_else(|| object_u64(Some(value), &["revision", "span_revision"]))
            .unwrap_or(0);
        let span_id = object_u64(nested, &["span_id", "spanId", "id"])
            .or_else(|| object_u64(Some(value), &["span_id", "spanId"]))
            .or_else(|| (revision > 0).then_some(revision))?;
        return Some(TypedResult::Draft(DraftSpan::new(
            span_id,
            audio_range,
            text,
            revision,
        )));
    }

    if is_preview {
        let audio_range = parse_audio_range(value, preview_nested)?;
        let text = object_str(preview_nested, &["text", "content"])
            .or_else(|| object_str(Some(value), &["text", "content"]))?;
        let revision = object_u64(preview_nested, &["revision"])
            .or_else(|| object_u64(Some(value), &["revision"]))
            .unwrap_or(0);
        let request_id = object_u64(preview_nested, &["request_id", "requestId", "id"])
            .or_else(|| object_u64(Some(value), &["request_id", "requestId"]))
            .or_else(|| (revision > 0).then_some(revision))?;
        // 0.23.14.6：组合预览的组成段清单（每段文本 + 音频范围）。缺失时
        // 退化为单一尾部段（旧引擎兼容），顶层 audio_range 即该段范围。
        let spans = parse_preview_spans(value, preview_nested).unwrap_or_else(|| {
            vec![PreviewSegment::new(audio_range, text.clone())]
        });
        return Some(TypedResult::Preview {
            request_id,
            audio_range,
            revision,
            text,
            spans,
        });
    }

    None
}

/// 解析 `spans` 数组：`[{"text": "...", "range": {"startSample": ..}}, ..]`。
/// 任一段缺少 text/区间即视为整体缺失（返回 None，调用方走兼容退化）。
fn parse_preview_spans(
    root: &serde_json::Value,
    nested: Option<&serde_json::Value>,
) -> Option<Vec<PreviewSegment>> {
    let host = nested.unwrap_or(root);
    let spans = host.get("spans").or_else(|| root.get("spans"))?.as_array()?;
    if spans.is_empty() {
        return Some(Vec::new());
    }
    let mut out = Vec::with_capacity(spans.len());
    for span in spans {
        let text = object_str(Some(span), &["text", "content"])?;
        let range = parse_audio_range(span, None)?;
        out.push(PreviewSegment::new(range, text));
    }
    Some(out)
}

impl EventBus {
    fn new(
        active_generation: Arc<AtomicU64>,
        stats: Arc<EventStats>,
    ) -> (Arc<Self>, mpsc::Receiver<SttEvent>) {
        let (reliable_tx, mut reliable_rx) = mpsc::channel(EVENT_CHANNEL_CAPACITY);
        let (output_tx, output_rx) = mpsc::channel(EVENT_CHANNEL_CAPACITY);
        let bus = Arc::new(Self {
            reliable_tx,
            preview_slot: std::sync::Mutex::new(None),
            notify: Notify::new(),
            active_generation,
            stats,
            queue_depth: AtomicUsize::new(0),
        });
        let weak_bus = Arc::downgrade(&bus);
        tokio::spawn(async move {
            loop {
                let Some(bus) = weak_bus.upgrade() else { break };
                tokio::select! {
                    biased;
                    event = reliable_rx.recv() => {
                        let Some(ReliableEvent {event, delivered}) = event else { break };
                        if event_generation(&event) != bus.active_generation.load(Ordering::Acquire) {
                            if let Some(ack) = delivered {
                                let _ = ack.send(false);
                            }
                            continue;
                        }
                        let progress = matches!(
                            event,
                            SttEvent::Partial { .. }
                                | SttEvent::Draft { .. }
                                | SttEvent::Preview { .. }
                        );
                        if output_tx.send(event).await.is_err() {
                            if progress {
                                bus.stats.partials_dropped_closed.fetch_add(1, Ordering::Relaxed);
                            }
                            if let Some(ack) = delivered {
                                let _ = ack.send(false);
                            }
                            break;
                        }
                        if progress {
                            bus.stats.partials_emitted.fetch_add(1, Ordering::Relaxed);
                        }
                        if let Some(ack) = delivered {
                            let _ = ack.send(true);
                        }
                        bus.observe_queue(&output_tx);
                        // A reliable event gets priority.  A pending preview
                        // will be retried by the next notify/tick.
                    }
                    _ = bus.notify.notified() => {
                        deliver_latest_preview(&bus, &output_tx);
                    }
                    _ = tokio::time::sleep(std::time::Duration::from_millis(5)) => {
                        deliver_latest_preview(&bus, &output_tx);
                    }
                }
            }
        });
        (bus, output_rx)
    }

    fn observe_queue(&self, sender: &mpsc::Sender<SttEvent>) {
        let depth = EVENT_CHANNEL_CAPACITY.saturating_sub(sender.capacity());
        self.queue_depth.store(depth, Ordering::Relaxed);
        self.stats
            .max_queue_depth
            .fetch_max(depth, Ordering::Relaxed);
    }

    fn clear_preview(&self) {
        match self.preview_slot.lock() {
            Ok(mut slot) => *slot = None,
            Err(poison) => *poison.into_inner() = None,
        }
    }

    fn queue_depth(&self) -> usize {
        self.queue_depth.load(Ordering::Relaxed)
    }
}

fn event_generation(event: &SttEvent) -> u64 {
    event.generation()
}

/// Deliver the one pending preview only when one output slot remains reserved
/// for reliable events.  A full output queue leaves the latest value in place.
fn deliver_latest_preview(bus: &EventBus, output_tx: &mpsc::Sender<SttEvent>) {
    if output_tx.capacity() <= 1 {
        return;
    }
    let event = match bus.preview_slot.lock() {
        Ok(mut slot) => slot.take(),
        Err(poison) => poison.into_inner().take(),
    };
    let Some(event) = event else { return };
    if event_generation(&event) != bus.active_generation.load(Ordering::Acquire) {
        return;
    }
    match output_tx.try_send(event) {
        Ok(()) => {
            bus.stats.partials_emitted.fetch_add(1, Ordering::Relaxed);
            bus.observe_queue(output_tx);
        }
        Err(mpsc::error::TrySendError::Full(event)) => match bus.preview_slot.lock() {
            Ok(mut slot) => *slot = Some(event),
            Err(poison) => *poison.into_inner() = Some(event),
        },
        Err(mpsc::error::TrySendError::Closed(_)) => {
            bus.stats
                .partials_dropped_closed
                .fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// GGUF 引擎的 `StreamingSttPort` 适配器。
///
/// 包装任意 `SttEngine` 实现为 `StreamingSttPort`。
/// 适用于 PseudoStreamingSttEngine 和 LocalSttEngine。
pub struct GgufStreamingAdapter {
    /// 内部引擎
    engine: Arc<dyn SttEngine>,
    /// 识别调度 profile；旧构造函数保持 Legacy 兼容。
    profile: RecognitionProfile,
    /// 可靠事件队列 + latest preview slot；events() 每次重建一组总线
    event_bus: std::sync::Mutex<Option<Arc<EventBus>>>,
    /// generation 计数器
    generation: AtomicU64,
    /// 当前 active generation（None = 无活跃 session）
    active_gen: TokioMutex<Option<u64>>,
    /// 事件总线使用的 epoch；0 表示无活跃会话
    active_event_generation: Arc<AtomicU64>,
    /// 去重状态（边沿触发）
    last_observed: std::sync::Mutex<ObservedState>,
    // ── 诊断计数（只含计数，不含正文）──
    chunks_received: AtomicU64,
    partials_emitted: Arc<AtomicU64>,
    partials_suppressed: AtomicU64,
    partials_coalesced: Arc<AtomicU64>,
    partials_dropped_closed: Arc<AtomicU64>,
    confirmed_backpressure: Arc<AtomicU64>,
    max_queue_depth: Arc<AtomicUsize>,
}

impl GgufStreamingAdapter {
    /// 创建适配器，包装一个 `SttEngine`。
    #[allow(dead_code)]
    pub fn new(engine: Arc<dyn SttEngine>) -> Self {
        Self::new_with_profile(engine, RecognitionProfile::Legacy)
    }

    /// 创建带显式识别 profile 的适配器。
    pub fn new_with_profile(engine: Arc<dyn SttEngine>, profile: RecognitionProfile) -> Self {
        let stats = Arc::new(EventStats {
            partials_emitted: Arc::new(AtomicU64::new(0)),
            partials_coalesced: Arc::new(AtomicU64::new(0)),
            partials_dropped_closed: Arc::new(AtomicU64::new(0)),
            confirmed_backpressure: Arc::new(AtomicU64::new(0)),
            max_queue_depth: Arc::new(AtomicUsize::new(0)),
        });
        Self {
            engine,
            profile,
            event_bus: std::sync::Mutex::new(None),
            generation: AtomicU64::new(0),
            active_gen: TokioMutex::new(None),
            active_event_generation: Arc::new(AtomicU64::new(0)),
            last_observed: std::sync::Mutex::new(ObservedState::default()),
            chunks_received: AtomicU64::new(0),
            partials_emitted: Arc::clone(&stats.partials_emitted),
            partials_suppressed: AtomicU64::new(0),
            partials_coalesced: Arc::clone(&stats.partials_coalesced),
            partials_dropped_closed: Arc::clone(&stats.partials_dropped_closed),
            confirmed_backpressure: Arc::clone(&stats.confirmed_backpressure),
            max_queue_depth: Arc::clone(&stats.max_queue_depth),
        }
    }

    fn bus(&self) -> Option<Arc<EventBus>> {
        match self.event_bus.lock() {
            Ok(guard) => guard.clone(),
            Err(poison) => poison.into_inner().clone(),
        }
    }

    fn clear_preview(&self) {
        if let Some(bus) = self.bus() {
            bus.clear_preview();
        }
    }

    /// 重置单会话诊断计数与去重状态。
    fn reset_session_counters(&self) {
        self.chunks_received.store(0, Ordering::Relaxed);
        self.partials_emitted.store(0, Ordering::Relaxed);
        self.partials_suppressed.store(0, Ordering::Relaxed);
        self.partials_coalesced.store(0, Ordering::Relaxed);
        self.partials_dropped_closed.store(0, Ordering::Relaxed);
        self.confirmed_backpressure.store(0, Ordering::Relaxed);
        self.max_queue_depth.store(0, Ordering::Relaxed);
        match self.last_observed.lock() {
            Ok(mut guard) => *guard = ObservedState::default(),
            Err(poison) => *poison.into_inner() = ObservedState::default(),
        }
        self.clear_preview();
    }

    /// 把 confirmed/终态事件送入可靠队列；预览永远不走这条等待路径。
    async fn send_reliable(&self, event: SttEvent, wait_delivery: bool) {
        let Some(bus) = self.bus() else {
            if matches!(
                event,
                SttEvent::Partial { .. } | SttEvent::Draft { .. } | SttEvent::Preview { .. }
            ) {
                self.partials_dropped_closed.fetch_add(1, Ordering::Relaxed);
            }
            return;
        };
        let (ack_tx, ack_rx) = if wait_delivery {
            let (tx, rx) = oneshot::channel();
            (Some(tx), Some(rx))
        } else {
            (None, None)
        };
        if event.is_reliable() && bus.reliable_tx.capacity() == 0 {
            self.confirmed_backpressure.fetch_add(1, Ordering::Relaxed);
        }
        let progress = matches!(
            event,
            SttEvent::Partial { .. } | SttEvent::Draft { .. } | SttEvent::Preview { .. }
        );
        if bus
            .reliable_tx
            .send(ReliableEvent {
                event,
                delivered: ack_tx,
            })
            .await
            .is_err()
        {
            if progress {
                self.partials_dropped_closed.fetch_add(1, Ordering::Relaxed);
            }
            tracing::debug!("STT 可靠事件通道已关闭");
            return;
        }
        if let Some(ack_rx) = ack_rx {
            match ack_rx.await {
                Ok(true) => {}
                Ok(false) | Err(_) => {
                    tracing::warn!("STT 终态事件未能交付到事件队列");
                }
            }
        }
    }

    /// 阻塞发送终态事件（`Final`/`Error` 永不丢弃）。
    async fn emit_terminal(&self, event: SttEvent) {
        if event_generation(&event) != self.active_event_generation.load(Ordering::Acquire) {
            return;
        }
        self.send_reliable(event, true).await;
    }

    /// 把可替换 Preview 放入 latest slot。Draft/终态永远不经过此路径。
    fn enqueue_preview(&self, event: SttEvent) {
        let Some(bus) = self.bus() else {
            self.partials_dropped_closed.fetch_add(1, Ordering::Relaxed);
            return;
        };
        match bus.preview_slot.lock() {
            Ok(mut slot) => {
                if slot.replace(event).is_some() {
                    self.partials_coalesced.fetch_add(1, Ordering::Relaxed);
                }
            }
            Err(poison) => {
                if poison.into_inner().replace(event).is_some() {
                    self.partials_coalesced.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
        bus.notify.notify_one();
    }

    /// 解析引擎返回结果，做边沿触发去重后产出 Partial 或类型化事件。
    ///
    /// 返回值仅用于诊断；调用方不需要区分。
    async fn emit_partial_from_result(&self, generation: u64, text: &str) {
        if text.is_empty() || self.active_event_generation.load(Ordering::Acquire) != generation {
            return;
        }

        let parsed = serde_json::from_str::<serde_json::Value>(text);
        if let Ok(value) = &parsed {
            if let Some(typed) = parse_typed_result(value) {
                match typed {
                    TypedResult::Draft(span) => {
                        self.send_reliable(SttEvent::Draft { generation, span }, false)
                            .await;
                    }
                    TypedResult::Preview {
                        request_id,
                        audio_range,
                        revision,
                        text,
                        spans,
                    } => {
                        self.enqueue_preview(SttEvent::Preview {
                            generation,
                            request_id,
                            audio_range,
                            revision,
                            text,
                            spans,
                        });
                    }
                }
                return;
            }
        }

        let (revision, confirmed, preview, confirmed_changed) = match parsed {
            Ok(value) => {
                let confirmed = value
                    .get("confirmed")
                    .and_then(|t| t.as_str())
                    .unwrap_or("");
                let preview = value.get("preview").and_then(|t| t.as_str()).unwrap_or("");
                let confirmed_changed = value
                    .get("confirmed_changed")
                    .and_then(|b| b.as_bool())
                    .unwrap_or(!confirmed.is_empty());
                (
                    value.get("revision").and_then(|r| r.as_u64()),
                    confirmed.to_string(),
                    preview.to_string(),
                    confirmed_changed,
                )
            }
            // 纯文本（非流式引擎的兼容路径——不应在 push_audio 中出现）
            Err(_) => (None, String::new(), text.to_string(), false),
        };

        if confirmed.is_empty() && preview.is_empty() {
            return;
        }

        // ── 边沿触发：状态未变化不得产出事件 ──
        {
            let mut last = match self.last_observed.lock() {
                Ok(guard) => guard,
                Err(poison) => poison.into_inner(),
            };
            let effective_confirmed = if confirmed.is_empty() {
                last.confirmed.clone()
            } else {
                confirmed.clone()
            };
            if revision.is_some()
                && last.revision == revision
                && last.confirmed == effective_confirmed
                && last.preview == preview
            {
                self.partials_suppressed.fetch_add(1, Ordering::Relaxed);
                return;
            }
            if revision.is_none()
                && last.confirmed == effective_confirmed
                && last.preview == preview
            {
                self.partials_suppressed.fetch_add(1, Ordering::Relaxed);
                return;
            }
            last.revision = revision;
            if !confirmed.is_empty() {
                last.confirmed = confirmed.clone();
            }
            last.preview = preview.clone();
        }

        // 携带 confirmed 变化的事件属于"可确认、有序、不得静默丢失"的交付物
        let confirmed_bearing = !confirmed.is_empty();
        let event = SttEvent::Partial {
            generation,
            revision: revision.unwrap_or(0),
            confirmed,
            confirmed_changed,
            preview,
        };

        if confirmed_bearing {
            if overflow_policy(true) == OverflowPolicy::Backpressure {
                self.send_reliable(event, false).await;
            }
            return;
        }

        self.enqueue_preview(event);
    }
}

#[async_trait::async_trait]
impl StreamingSttPort for GgufStreamingAdapter {
    async fn begin_session(&self) -> Result<u64, SttError> {
        let mut active = self.active_gen.lock().await;
        if active.is_some() {
            return Err(SttError::Engine("已有活跃 session".to_string()));
        }

        self.engine.reset();
        self.reset_session_counters();

        let session_gen = self.generation.fetch_add(1, Ordering::AcqRel) + 1;
        *active = Some(session_gen);
        self.active_event_generation
            .store(session_gen, Ordering::Release);

        tracing::debug!(generation = session_gen, "GGUF session begin");
        Ok(session_gen)
    }

    /// 0.23.10.2：会话开始即预热 worker（内部 fire-and-forget，立即返回）。
    async fn warm_up(&self) -> Result<(), SttError> {
        self.engine.warm_up();
        Ok(())
    }

    async fn push_audio(&self, generation: u64, samples: &[f32]) -> Result<(), SttError> {
        // 检查 generation 匹配（guard 不跨 await）
        {
            let active = self.active_gen.lock().await;
            if *active != Some(generation) {
                return Err(SttError::Engine(format!(
                    "generation 不匹配: 期望 {generation:?}，当前 {active:?}"
                )));
            }
        }

        self.chunks_received.fetch_add(1, Ordering::Relaxed);

        // 调用引擎的 transcribe_chunk
        // 伪流式引擎内部会 spawn 后台 HTTP task，这里只是触发预览检查；
        // 状态未变化时引擎返回空串，不产出任何事件。
        match self.engine.transcribe_chunk(samples).await {
            Ok(text) => {
                self.emit_partial_from_result(generation, &text).await;
                Ok(())
            }
            Err(e) => {
                self.emit_terminal(SttEvent::Error {
                    generation,
                    message: e.to_string(),
                })
                .await;
                Err(e)
            }
        }
    }

    async fn finish_session(&self, generation: u64) -> Result<(), SttError> {
        let active = self.active_gen.lock().await;
        if *active != Some(generation) {
            return Err(SttError::Engine(format!(
                "generation 不匹配: 期望 {generation:?}，当前 {active:?}"
            )));
        }
        drop(active);

        // 调用 finalize（不持有 active_gen 锁——finalize 可能等待数秒）
        let result = match self.engine.finalize().await {
            Ok(text) => SttEvent::Final { generation, text },
            Err(e) => SttEvent::Error {
                generation,
                message: e.to_string(),
            },
        };
        self.emit_terminal(result).await;

        self.clear_preview();
        *self.active_gen.lock().await = None;
        Ok(())
    }

    async fn cancel_session(&self, generation: u64) -> Result<(), SttError> {
        let mut active = self.active_gen.lock().await;
        // 幂等：不匹配时也返回 Ok
        if *active == Some(generation) {
            self.active_event_generation.store(0, Ordering::Release);
            self.clear_preview();
            self.engine.reset();
            self.reset_session_counters();
            *active = None;
            tracing::debug!(generation, "GGUF session cancelled");
        }
        Ok(())
    }

    async fn reset(&self) -> Result<(), SttError> {
        self.active_event_generation.store(0, Ordering::Release);
        self.clear_preview();
        self.engine.reset();
        self.reset_session_counters();
        *self.active_gen.lock().await = None;
        // 递增 generation 使任何在途事件失效
        self.generation.fetch_add(1, Ordering::AcqRel);
        Ok(())
    }

    fn supports_native_partial(&self) -> bool {
        false
    }

    fn recognition_profile(&self) -> RecognitionProfile {
        self.profile
    }

    fn events(&self) -> mpsc::Receiver<SttEvent> {
        // 每次订阅重建总线。旧总线只持有弱引用的桥接任务，generation
        // 过滤会阻止旧会话事件进入新会话。
        let stats = Arc::new(EventStats {
            partials_emitted: Arc::clone(&self.partials_emitted),
            partials_coalesced: Arc::clone(&self.partials_coalesced),
            partials_dropped_closed: Arc::clone(&self.partials_dropped_closed),
            confirmed_backpressure: Arc::clone(&self.confirmed_backpressure),
            max_queue_depth: Arc::clone(&self.max_queue_depth),
        });
        let (bus, rx) = EventBus::new(Arc::clone(&self.active_event_generation), stats);
        *self.event_bus.lock().unwrap() = Some(bus);
        rx
    }

    fn stream_stats(&self) -> SttStreamStats {
        let mut stats = self.engine.stream_stats();
        stats.chunks_received = self.chunks_received.load(Ordering::Relaxed);
        stats.partials_emitted = self.partials_emitted.load(Ordering::Relaxed);
        stats.partials_suppressed = self.partials_suppressed.load(Ordering::Relaxed);
        stats.partials_coalesced = self.partials_coalesced.load(Ordering::Relaxed);
        stats.partials_dropped_closed = self.partials_dropped_closed.load(Ordering::Relaxed);
        stats.confirmed_backpressure = self.confirmed_backpressure.load(Ordering::Relaxed);
        stats.queue_capacity = EVENT_CHANNEL_CAPACITY;
        stats.max_queue_depth = self.max_queue_depth.load(Ordering::Relaxed);
        stats.queue_depth = self.bus().map(|bus| bus.queue_depth()).unwrap_or_default();
        stats
    }
}

// ── 测试 ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// 测试用的 Mock SttEngine。
    struct MockEngine {
        partial_text: String,
        final_text: String,
        reset_count: std::sync::atomic::AtomicU32,
    }

    #[async_trait::async_trait]
    impl SttEngine for MockEngine {
        async fn transcribe_chunk(&self, _samples: &[f32]) -> Result<String, SttError> {
            Ok(self.partial_text.clone())
        }
        async fn finalize(&self) -> Result<String, SttError> {
            Ok(self.final_text.clone())
        }
        fn reset(&self) {
            self.reset_count.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn mock_engine(partial: &str, final_text: &str) -> Arc<MockEngine> {
        Arc::new(MockEngine {
            partial_text: partial.to_string(),
            final_text: final_text.to_string(),
            reset_count: std::sync::atomic::AtomicU32::new(0),
        })
    }

    #[tokio::test]
    async fn begin_push_finish_lifecycle() {
        let engine = mock_engine(r#"{"confirmed":"","preview":"你好"}"#, "你好世界");
        let adapter = GgufStreamingAdapter::new(engine.clone());

        let session_gen = adapter.begin_session().await.unwrap();
        assert_eq!(session_gen, 1);

        let mut rx = adapter.events();

        adapter.push_audio(session_gen, &[0.1; 320]).await.unwrap();

        // 应收到 Partial 事件
        let event = rx.recv().await.unwrap();
        match event {
            SttEvent::Partial {
                confirmed, preview, ..
            } => {
                assert_eq!(confirmed, "");
                assert_eq!(preview, "你好");
            }
            other => panic!("期望 Partial，收到 {other:?}"),
        }

        adapter.finish_session(session_gen).await.unwrap();

        // 应收到 Final 事件
        let event = rx.recv().await.unwrap();
        match event {
            SttEvent::Final { text, .. } => {
                assert_eq!(text, "你好世界");
            }
            other => panic!("期望 Final，收到 {other:?}"),
        }
    }

    #[tokio::test]
    async fn typed_draft_and_preview_envelopes_are_delivered_without_v2_projection() {
        let engine = mock_engine(
            r#"{"kind":"draft","span":{"spanId":7,"audioRange":{"startSample":0,"endSample":80000},"text":"稳定草稿。","revision":2}}"#,
            "",
        );
        let adapter =
            GgufStreamingAdapter::new_with_profile(engine, RecognitionProfile::PreviewDraft);
        assert_eq!(
            adapter.recognition_profile(),
            RecognitionProfile::PreviewDraft
        );
        let generation = adapter.begin_session().await.unwrap();
        let mut rx = adapter.events();

        adapter.push_audio(generation, &[0.1; 320]).await.unwrap();
        let draft = tokio::time::timeout(std::time::Duration::from_millis(100), rx.recv())
            .await
            .expect("typed Draft should be delivered")
            .expect("event channel remains open");
        assert!(matches!(
            draft,
            SttEvent::Draft { generation: g, span }
                if g == generation
                    && span.span_id == 7
                    && span.audio_range == AudioRange::new(0, 80_000)
                    && span.text == "稳定草稿。"
        ));

        let preview_engine = mock_engine(
            r#"{"kind":"preview","requestId":9,"audioRange":{"startSample":80000,"endSample":112000},"revision":3,"text":"预览"}"#,
            "",
        );
        let preview_adapter = GgufStreamingAdapter::new_with_profile(
            preview_engine,
            RecognitionProfile::PreviewDraft,
        );
        let generation = preview_adapter.begin_session().await.unwrap();
        let mut rx = preview_adapter.events();
        preview_adapter
            .push_audio(generation, &[0.1; 320])
            .await
            .unwrap();
        let preview = tokio::time::timeout(std::time::Duration::from_millis(100), rx.recv())
            .await
            .expect("typed Preview should be delivered")
            .expect("event channel remains open");
        assert!(matches!(
            preview,
            SttEvent::Preview {
                generation: g,
                request_id: 9,
                audio_range,
                text,
                ..
            } if g == generation
                && audio_range == AudioRange::new(80_000, 112_000)
                && text == "预览"
        ));
    }

    #[tokio::test]
    async fn cancel_discards_results() {
        let engine = mock_engine(r#"{"confirmed":"","preview":"你好"}"#, "你好世界");
        let adapter = GgufStreamingAdapter::new(engine.clone());

        let session_gen = adapter.begin_session().await.unwrap();
        let mut rx = adapter.events();

        adapter.push_audio(session_gen, &[0.1; 320]).await.unwrap();
        // 消费 partial
        let _ = rx.recv().await.unwrap();

        // cancel
        adapter.cancel_session(session_gen).await.unwrap();

        // cancel 后不应有 Final 事件
        // begin 新 session
        let gen2 = adapter.begin_session().await.unwrap();
        assert_eq!(gen2, 2);

        adapter.finish_session(gen2).await.unwrap();

        // 应只收到新 generation 的 Final
        let event = rx.recv().await.unwrap();
        match event {
            SttEvent::Final { generation, text } => {
                assert_eq!(generation, gen2);
                assert_eq!(text, "你好世界");
            }
            other => panic!("期望 Final，收到 {other:?}"),
        }
    }

    #[tokio::test]
    async fn reset_is_idempotent() {
        let engine = mock_engine("", "");
        let adapter = GgufStreamingAdapter::new(engine.clone());

        // reset 多次调用不 panic
        adapter.reset().await.unwrap();
        adapter.reset().await.unwrap();
        adapter.reset().await.unwrap();

        assert!(engine.reset_count.load(Ordering::Relaxed) >= 3);
    }

    #[tokio::test]
    async fn old_generation_partial_discarded() {
        let engine = mock_engine(r#"{"confirmed":"","preview":"你好"}"#, "你好世界");
        let adapter = GgufStreamingAdapter::new(engine.clone());

        let gen1 = adapter.begin_session().await.unwrap();
        let mut rx = adapter.events();

        adapter.push_audio(gen1, &[0.1; 320]).await.unwrap();
        let _ = rx.recv().await.unwrap(); // 消费 partial

        adapter.cancel_session(gen1).await.unwrap();

        // begin 新 session
        let gen2 = adapter.begin_session().await.unwrap();
        adapter.finish_session(gen2).await.unwrap();

        // 只应收到 gen2 的 Final
        let event = rx.recv().await.unwrap();
        match event {
            SttEvent::Final { generation, .. } => {
                assert_eq!(generation, gen2);
            }
            other => panic!("期望 Final gen={gen2}，收到 {other:?}"),
        }
    }

    #[tokio::test]
    async fn supports_native_partial_is_false() {
        let engine = mock_engine("", "");
        let adapter = GgufStreamingAdapter::new(engine);
        assert!(!adapter.supports_native_partial());
    }

    #[tokio::test]
    async fn error_event_on_engine_failure() {
        struct FailingEngine;
        #[async_trait::async_trait]
        impl SttEngine for FailingEngine {
            async fn transcribe_chunk(&self, _: &[f32]) -> Result<String, SttError> {
                Err(SttError::Engine("推理失败".to_string()))
            }
            async fn finalize(&self) -> Result<String, SttError> {
                Err(SttError::Engine("finalize 失败".to_string()))
            }
            fn reset(&self) {}
        }

        let adapter = GgufStreamingAdapter::new(Arc::new(FailingEngine));
        let session_gen = adapter.begin_session().await.unwrap();
        let mut rx = adapter.events();

        // push_audio 在引擎失败时返回 Err 并 emit Error 事件
        let _ = adapter.push_audio(session_gen, &[0.1; 320]).await;

        // 应收到 Error 事件
        let event = rx.recv().await.unwrap();
        assert!(matches!(event, SttEvent::Error { .. }));
    }

    #[tokio::test]
    async fn double_begin_rejected() {
        let engine = mock_engine("", "");
        let adapter = GgufStreamingAdapter::new(engine);

        let _ = adapter.begin_session().await.unwrap();
        // 第二次 begin 应失败
        let result = adapter.begin_session().await;
        assert!(result.is_err());
    }

    // ── 0.24: 有界事件通道、状态边沿触发、confirmed 不丢 ──────────────────

    /// 按调用序产出带版本号状态快照的引擎。
    ///
    /// - `state_change_every`：每 N 次调用 revision 递增一次（非 0）
    /// - `confirmed_every`：每 N 次调用产生一次 confirmed 增量；0 = 永不产生
    struct VersionedEngine {
        calls: AtomicU64,
        state_change_every: u64,
        confirmed_every: u64,
    }

    impl VersionedEngine {
        fn new(state_change_every: u64, confirmed_every: u64) -> Arc<Self> {
            Arc::new(Self {
                calls: AtomicU64::new(0),
                state_change_every,
                confirmed_every,
            })
        }
    }

    #[async_trait::async_trait]
    impl SttEngine for VersionedEngine {
        async fn transcribe_chunk(&self, _samples: &[f32]) -> Result<String, SttError> {
            let n = self.calls.fetch_add(1, Ordering::Relaxed);
            let revision = n / self.state_change_every;
            let confirmed_changed =
                self.confirmed_every > 0 && n.is_multiple_of(self.confirmed_every);
            let confirmed = if confirmed_changed {
                format!("第{revision}句。")
            } else {
                String::new()
            };
            Ok(serde_json::json!({
                "v": 2,
                "revision": revision,
                "confirmed_changed": confirmed_changed,
                "confirmed": confirmed,
                "preview": format!("预览{revision}"),
            })
            .to_string())
        }

        async fn finalize(&self) -> Result<String, SttError> {
            Ok(String::new())
        }

        fn reset(&self) {}
    }

    fn drain_events(rx: &mut mpsc::Receiver<SttEvent>) -> Vec<SttEvent> {
        let mut out = Vec::new();
        while let Ok(event) = rx.try_recv() {
            out.push(event);
        }
        out
    }

    /// 状态未变化时不得产出事件（边沿触发）。
    #[tokio::test]
    async fn partial_suppressed_when_state_unchanged() {
        let engine = VersionedEngine::new(50, 0);
        let adapter = GgufStreamingAdapter::new(engine);
        let session_gen = adapter.begin_session().await.unwrap();
        let mut rx = adapter.events();

        for _ in 0..50 {
            adapter.push_audio(session_gen, &[0.1; 320]).await.unwrap();
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        // 50 个 chunk 属于同一 revision → 只应产出 1 个 Partial
        let events = drain_events(&mut rx);
        assert_eq!(events.len(), 1, "同一状态只允许一次对外变化");

        let stats = adapter.stream_stats();
        assert_eq!(stats.chunks_received, 50);
        assert_eq!(stats.partials_emitted, 1);
        assert_eq!(stats.partials_suppressed, 49);
    }

    /// 纯预览事件在队列满时只保留 latest value——不得无界排队。
    #[tokio::test]
    async fn preview_only_partials_are_coalesced_when_queue_full() {
        let engine = VersionedEngine::new(1, 0);
        let adapter = GgufStreamingAdapter::new(engine);
        let session_gen = adapter.begin_session().await.unwrap();
        let _rx = adapter.events(); // 消费者不读取

        let pushes = EVENT_CHANNEL_CAPACITY + 64;
        for _ in 0..pushes {
            adapter.push_audio(session_gen, &[0.1; 320]).await.unwrap();
        }
        let stats = adapter.stream_stats();
        assert_eq!(stats.chunks_received, pushes as u64);
        assert!(stats.partials_emitted >= 1, "至少应交付一个预览");
        assert_eq!(
            stats.partials_emitted + stats.partials_coalesced,
            (pushes - 1) as u64,
            "除去仍在 latest slot 中待补发的一个值，其余预览要么交付要么被替换: {stats:?}"
        );
        assert!(
            stats.partials_coalesced > 0,
            "队列未消费时后续预览必须被合并"
        );
        assert!(
            stats.queue_depth <= stats.queue_capacity,
            "队列深度不得超过有界容量"
        );
    }

    #[tokio::test]
    async fn latest_preview_survives_full_queue_and_delivers_c() {
        let adapter = GgufStreamingAdapter::new(mock_engine("", ""));
        let generation = adapter.begin_session().await.unwrap();
        let mut rx = adapter.events();
        let bus = adapter.bus().expect("event bus");

        // Fill the reliable/output side without a consumer.  The preview slot
        // is independent and must still converge to C.
        for i in 0..EVENT_CHANNEL_CAPACITY {
            bus.reliable_tx
                .try_send(ReliableEvent {
                    event: SttEvent::Partial {
                        generation,
                        revision: i as u64,
                        confirmed: format!("确认{i}"),
                        confirmed_changed: true,
                        preview: String::new(),
                    },
                    delivered: None,
                })
                .unwrap();
        }
        for _ in 0..20 {
            if adapter.stream_stats().queue_depth >= EVENT_CHANNEL_CAPACITY {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }

        adapter
            .emit_partial_from_result(
                generation,
                r#"{"revision":100,"confirmed":"","preview":"A"}"#,
            )
            .await;
        adapter
            .emit_partial_from_result(
                generation,
                r#"{"revision":101,"confirmed":"","preview":"B"}"#,
            )
            .await;
        adapter
            .emit_partial_from_result(
                generation,
                r#"{"revision":102,"confirmed":"","preview":"C"}"#,
            )
            .await;

        let mut reliable = 0;
        while rx.try_recv().is_ok() {
            reliable += 1;
        }
        assert_eq!(reliable, EVENT_CHANNEL_CAPACITY);
        let preview = tokio::time::timeout(std::time::Duration::from_millis(100), rx.recv())
            .await
            .expect("latest preview should be retried after consumer resumes")
            .expect("preview channel remains open");
        assert!(matches!(preview, SttEvent::Partial { preview, .. } if preview == "C"));
        assert!(adapter.stream_stats().partials_coalesced >= 2);
    }

    #[tokio::test]
    async fn finish_waits_until_final_is_delivered_before_session_switch() {
        let adapter = GgufStreamingAdapter::new(mock_engine("", "final"));
        let generation = adapter.begin_session().await.unwrap();
        let mut rx = adapter.events();
        let bus = adapter.bus().expect("event bus");

        for i in 0..EVENT_CHANNEL_CAPACITY {
            bus.reliable_tx
                .try_send(ReliableEvent {
                    event: SttEvent::Partial {
                        generation,
                        revision: i as u64,
                        confirmed: format!("确认{i}"),
                        confirmed_changed: true,
                        preview: String::new(),
                    },
                    delivered: None,
                })
                .unwrap();
        }
        for _ in 0..20 {
            if adapter.stream_stats().queue_depth >= EVENT_CHANNEL_CAPACITY {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }

        let mut finish = Box::pin(adapter.finish_session(generation));
        tokio::select! {
            result = &mut finish => panic!("队列满时 finish 不应在终态交付前返回: {result:?}"),
            _ = tokio::time::sleep(std::time::Duration::from_millis(20)) => {}
        }

        let mut saw_final = false;
        for _ in 0..=EVENT_CHANNEL_CAPACITY {
            let event = tokio::time::timeout(std::time::Duration::from_millis(100), rx.recv())
                .await
                .expect("终态应在消费恢复后可见")
                .expect("事件通道应保持打开");
            if matches!(event, SttEvent::Final { text, .. } if text == "final") {
                saw_final = true;
                break;
            }
        }
        assert!(saw_final);
        finish.await.unwrap();
        let next_generation = adapter.begin_session().await.unwrap();
        assert_ne!(next_generation, generation);
    }

    #[tokio::test]
    async fn closed_preview_channel_counts_drop_without_emitted() {
        let adapter = GgufStreamingAdapter::new(mock_engine("", ""));
        let generation = adapter.begin_session().await.unwrap();
        let rx = adapter.events();
        drop(rx);
        adapter
            .emit_partial_from_result(
                generation,
                r#"{"revision":1,"confirmed":"","preview":"preview"}"#,
            )
            .await;
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        let stats = adapter.stream_stats();
        assert_eq!(stats.partials_emitted, 0);
        assert!(stats.partials_dropped_closed >= 1);
    }

    /// 携带 confirmed 变化的事件在队列满时降级为阻塞发送，绝不静默丢失。
    ///
    /// 消费者被刻意推迟 500ms 才排空——两级有界队列必然先填满并进入背压。
    /// 单线程 runtime 的协作式预算（~128 次操作让出一次）只保证"队列不会
    /// 无界增长"，不保证"永不触顶"，所以这里必须让消费端真的慢下来。
    #[tokio::test]
    async fn confirmed_partials_never_dropped_when_queue_full() {
        let engine = VersionedEngine::new(1, 1);
        let adapter = GgufStreamingAdapter::new(engine);
        let session_gen = adapter.begin_session().await.unwrap();
        let mut rx = adapter.events();
        let pushes = EVENT_CHANNEL_CAPACITY * 2 + 64;

        let partials = Arc::new(AtomicU64::new(0));
        let consumer = {
            let partials = partials.clone();
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                while let Some(event) = rx.recv().await {
                    if matches!(event, SttEvent::Partial { .. }) {
                        let received = partials.fetch_add(1, Ordering::Relaxed) + 1;
                        if received == pushes as u64 {
                            break;
                        }
                    }
                }
            })
        };

        for _ in 0..pushes {
            adapter.push_audio(session_gen, &[0.1; 320]).await.unwrap();
        }
        for _ in 0..20 {
            if adapter.stream_stats().partials_emitted >= pushes as u64 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }

        consumer.await.unwrap();
        let stats = adapter.stream_stats();
        assert_eq!(
            stats.partials_coalesced, 0,
            "confirmed 变化不得被预览合并计数"
        );
        assert_eq!(
            stats.partials_emitted, pushes as u64,
            "每个 confirmed 变化都必须产出事件: {stats:?}"
        );
        assert!(
            stats.confirmed_backpressure > 0,
            "队列满时应降级为阻塞发送（背压），而非丢弃: {stats:?}"
        );

        drop(adapter);
        assert_eq!(partials.load(Ordering::Relaxed), pushes as u64);
    }

    /// 队列溢出分流规则（单一真源的纯函数）。
    #[test]
    fn overflow_policy_blocks_confirmed_and_coalesces_preview() {
        assert_eq!(
            overflow_policy(true),
            OverflowPolicy::Backpressure,
            "confirmed 事件必须背压，不得丢弃"
        );
        assert_eq!(
            overflow_policy(false),
            OverflowPolicy::CoalesceLatest,
            "纯预览事件只保留最新值"
        );
    }

    /// 自动化长流测试：事件数随"状态变化次数"增长，而不是随音频块数增长。
    ///
    /// 模拟 3 分钟录音（10ms/chunk → 18000 块），状态每 50 块变化一次。
    /// 修复前每块一个事件（18000 个），修复后应约 361 个。
    #[tokio::test]
    async fn long_stream_events_scale_with_state_changes_not_chunks() {
        const CHUNKS: u64 = 18_000;
        const CHANGE_EVERY: u64 = 50;

        let engine = VersionedEngine::new(CHANGE_EVERY, CHANGE_EVERY);
        let adapter = GgufStreamingAdapter::new(engine);
        let session_gen = adapter.begin_session().await.unwrap();
        let mut rx = adapter.events();

        let consumer = tokio::spawn(async move {
            let mut count = 0u64;
            while rx.recv().await.is_some() {
                count += 1;
            }
            count
        });

        for _ in 0..CHUNKS {
            adapter.push_audio(session_gen, &[0.1; 320]).await.unwrap();
            // 让消费者有机会排空（单线程 runtime），复现"消费跟得上"的真实条件
            tokio::task::yield_now().await;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        let stats = adapter.stream_stats();
        assert_eq!(stats.chunks_received, CHUNKS);
        assert_eq!(
            stats.partials_emitted + stats.partials_suppressed,
            CHUNKS,
            "每个 chunk 要么产出事件要么被抑制，计数必须闭合"
        );
        assert!(
            stats.partials_emitted <= CHUNKS / (CHANGE_EVERY - 1),
            "事件数必须随状态变化次数增长（修复前为 {}，实测 {}）",
            CHUNKS,
            stats.partials_emitted
        );
        assert_eq!(stats.partials_coalesced, 0, "消费者跟得上时不应合并预览");
        assert!(
            stats.max_queue_depth <= stats.queue_capacity,
            "队列深度有上界"
        );

        drop(adapter);
        let received = consumer.await.unwrap();
        assert_eq!(received, stats.partials_emitted);
    }

    /// 队列容量是有界的（修复前为 unbounded channel）。
    #[test]
    fn event_channel_is_bounded() {
        const {
            assert!(EVENT_CHANNEL_CAPACITY > 0);
            assert!(EVENT_CHANNEL_CAPACITY <= 4096);
        }
    }
}
