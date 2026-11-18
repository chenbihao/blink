//! 云端 STT 实时流式引擎：OpenAI Realtime 转写 WebSocket 协议（0.29.1）。
//!
//! ## 协议
//!
//! 连接 `ws(s)://<base_url>/realtime?intent=transcription`，音频为
//! base64 PCM16（16kHz mono）分片：
//!
//! - 客户端 → 服务端：`session.update`（funasr 系参考实现：`turn_detection:
//!   server_vad`）或 `transcription_session.update`（OpenAI 官方：`pcm16` +
//!   model）→ `input_audio_buffer.append`（逐帧）→ `input_audio_buffer.commit`
//! - 服务端 → 客户端：`session.created` /
//!   `conversation.item.input_audio_transcription.delta`（**funasr 系为累计
//!   全文，OpenAI 官方为增量片段**——按 provider kind 区分）/
//!   `conversation.item.input_audio_transcription.completed {transcript}` /
//!   `input_audio_buffer.committed` / `error`
//!
//! 实测参考（funasr sensevoice-native-server，2026-11）：握手即 101，
//! 上传期间每 ~500ms 推累计 delta（真增量预览），commit 后
//! committed + item.created + completed 一起到达。
//!
//! ## 结构
//!
//! [`CloudStreamingPort`] 原生实现 [`StreamingSttPort`]（不走 GgufStreamingAdapter）：
//! - WS 可用 → 真流式：delta → Preview/Partial，completed → Draft span，
//!   finish → Final。span 音频范围按客户端样本计数近似（funasr 服务不发
//!   `speech_started/stopped` 毫秒偏移，OpenAI 的偏移 v1 也不消费）。
//! - WS 不可用（连接失败/超时）→ **内置回退**：`CloudSttEngine` +
//!   `GgufStreamingAdapter`（整段上传，Legacy 语义），用户无感降级。
//!
//! 事件投递复用 `streaming_port::EventBus`（reliable 队列 + preview
//! latest-slot + generation 墙），与本地引擎同一套背压/去重语义。

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};

use async_trait::async_trait;
use base64::Engine as _;
use tokio::sync::mpsc;
use tokio::sync::{Mutex as TokioMutex, Notify};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Error as WsError;
use tokio_tungstenite::tungstenite::Message;

use crate::cloud;
use crate::streaming_port::{EventBus, EventStats};
use crate::{
    AudioRange, DraftSpan, PreviewSegment, RecognitionProfile, SttEngine, SttError, SttEvent,
    StreamingSttPort, SttStreamStats,
};

/// WS 连接超时（毫秒）。LAN 服务数十 ms；公网 OpenAI ~200-500ms。
const CONNECT_TIMEOUT_MS: u64 = 3_000;
/// `finish_session` 后等待终段（completed / committed+宽限 / 服务端关闭）的预算。
const FINALIZE_WAIT_TIMEOUT_MS: u64 = 3_000;
/// `input_audio_buffer.committed` 到达后，再等 completed 的宽限（毫秒）。
/// 静音会话（无语音）不会有 completed，靠这个及时收尾。
const COMMITTED_GRACE_MS: u64 = 1_000;
/// 攒批下限（样本数 @16kHz = 100ms）：更小的 push 合并成一帧 append，
/// 避免 WS 文本帧信头开销放大。
const FLUSH_MIN_SAMPLES: usize = 1_600;

type BusSlot = StdMutex<Option<Arc<EventBus>>>;

// ── 协议层（纯函数）──────────────────────────────────────────────────────

/// base_url（如 `https://api.openai.com/v1`）→ Realtime 转写 WS URL。
///
/// 规则：http→ws / https→wss（已是 ws(s) 则保留）；无路径时补 `/v1`；
/// 追加 `/realtime?intent=transcription`。
pub(crate) fn derive_ws_url(base_url: &str) -> Result<String, SttError> {
    let trimmed = base_url.trim().trim_end_matches('/');
    let (scheme, rest) = if let Some(r) = trimmed.strip_prefix("https://") {
        ("wss", r)
    } else if let Some(r) = trimmed.strip_prefix("http://") {
        ("ws", r)
    } else if let Some(r) = trimmed.strip_prefix("wss://") {
        ("wss", r)
    } else if let Some(r) = trimmed.strip_prefix("ws://") {
        ("ws", r)
    } else {
        return Err(SttError::Engine(format!(
            "云端 STT base_url 缺少 scheme: {base_url}"
        )));
    };
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, ""),
    };
    if authority.is_empty() {
        return Err(SttError::Engine(format!(
            "云端 STT base_url 缺少 host: {base_url}"
        )));
    }
    let base_path = if path.is_empty() { "/v1" } else { path };
    Ok(format!(
        "{scheme}://{authority}{base_path}/realtime?intent=transcription"
    ))
}

/// f32 样本（钳制到 -1.0..=1.0）→ PCM16 小端字节。
pub(crate) fn samples_to_pcm16(samples: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(samples.len() * 2);
    for &s in samples {
        let clamped = s.clamp(-1.0, 1.0);
        let v = (clamped * 32767.0).round() as i16;
        out.extend_from_slice(&v.to_le_bytes());
    }
    out
}

/// 构造会话配置消息（funasr 系参考实现 stream_client.py 风格）。
///
/// 0.29.1 修正：**不做 OpenAI 官方方言分支**。官方 Realtime 转写要求
/// 24kHz pcm16 与增量 delta 语义，与已验证的 funasr 累计方言不兼容——
/// 半套适配只会产出错误拼接（预览重叠/变速音频）。统一发 funasr 风格
/// `session.update`：funasr 系服务正常工作；OpenAI 官方端点会拒绝该配置
/// 并回 error 事件（会话可见失败，好于静默乱字）。官方对接需完整适配
/// （重采样 24k + 增量 delta + transcription_session 事件），留待后续版本。
pub(crate) fn build_session_update() -> String {
    serde_json::json!({
        "type": "session.update",
        "session": {"turn_detection": {"type": "server_vad"}}
    })
    .to_string()
}

pub(crate) fn build_append_message(pcm16: &[u8]) -> String {
    serde_json::json!({
        "type": "input_audio_buffer.append",
        "audio": base64::engine::general_purpose::STANDARD.encode(pcm16),
    })
    .to_string()
}

pub(crate) fn build_commit_message() -> String {
    r#"{"type":"input_audio_buffer.commit"}"#.to_string()
}

/// 服务端事件（解析后的类型化形式）。
///
/// 命名兼容两套：OpenAI Realtime（`conversation.item.input_audio_transcription.*`）
/// 与 Speaches/funasr 的 SSE 同名风格（`transcript.text.delta/done`）。
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum ServerEvent {
    SessionCreated,
    Delta {
        item: Option<String>,
        text: String,
    },
    Completed {
        item: Option<String>,
        transcript: String,
    },
    Committed,
    Error {
        message: String,
    },
    /// speech_started/stopped、conversation.item.created、未知事件——不参与状态机。
    Ignored,
}

pub(crate) fn parse_server_event(payload: &str) -> ServerEvent {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(payload) else {
        return ServerEvent::Ignored;
    };
    let Some(event_type) = value.get("type").and_then(|t| t.as_str()) else {
        return ServerEvent::Ignored;
    };
    let item = || {
        value
            .get("item_id")
            .and_then(|v| v.as_str())
            .map(str::to_string)
    };
    match event_type {
        "session.created" | "transcription_session.created" | "session.updated"
        | "transcription_session.updated" => ServerEvent::SessionCreated,
        "conversation.item.input_audio_transcription.delta" | "transcript.text.delta" => {
            ServerEvent::Delta {
                item: item(),
                text: value
                    .get("delta")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
            }
        }
        "conversation.item.input_audio_transcription.completed" | "transcript.text.done" => {
            ServerEvent::Completed {
                item: item(),
                transcript: value
                    .get("transcript")
                    .or_else(|| value.get("text"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
            }
        }
        "input_audio_buffer.committed" => ServerEvent::Committed,
        "error" => ServerEvent::Error {
            message: value
                .pointer("/error/message")
                .and_then(|m| m.as_str())
                .or_else(|| value.get("error").and_then(|e| e.as_str()))
                .unwrap_or("未知错误")
                .to_string(),
        },
        _ => ServerEvent::Ignored,
    }
}

// ── endpoint 解析（key 宽容版）──────────────────────────────────────────

/// Realtime 会话所需的 endpoint 投影。
pub(crate) struct RealtimeEndpoint {
    pub ws_url: String,
    pub api_key: Option<String>,
    pub model_id: String,
}

/// 从 `SttConfig` 解析 Realtime endpoint。
///
/// 与 [`cloud::resolve_stt_endpoint`] 的差异：**API Key 缺失不报错**——
/// 自部署 funasr 服务（`http://host:port/v1`）通常无鉴权，连接时省略
/// Authorization 头即可；官方 OpenAI 缺 key 会在握手处得到 401 并走回退。
///
/// 0.29.1：不再按 kind 判定 delta 方言——自部署服务用 kind=openai/custom
/// 都很常见，而 funasr 系服务无论 kind 都发**累计** delta。统一按累计处理。
fn resolve_realtime_endpoint(
    config: &blink_domain_config::stt_config::SttConfig,
) -> Result<RealtimeEndpoint, SttError> {
    let provider = config
        .cloud_provider
        .as_ref()
        .ok_or(SttError::NotInitialized)?;
    let base = provider
        .base_url
        .as_deref()
        .unwrap_or_else(|| cloud::default_base_url(&provider.kind))
        .trim_end_matches('/');
    Ok(RealtimeEndpoint {
        ws_url: derive_ws_url(base)?,
        api_key: blink_infra::platform::secret::load_secret(
            &blink_infra::platform::secret::SecretRef::stt_cloud(),
        )
        .ok()
        .map(|s| s.expose().to_owned()),
        model_id: provider.model_id.clone(),
    })
}

// ── WS 连接抽象（测试可注入 fake）───────────────────────────────────────

/// WS 连接的最小收发抽象。生产实现包 `WebSocketStream`，测试注入内存通道。
#[async_trait]
pub(crate) trait WsConn: Send {
    async fn send_text(&mut self, text: &str) -> Result<(), String>;
    /// `None` = 连接关闭；`Some(Err)` = 协议/IO 错误。
    async fn recv(&mut self) -> Option<Result<String, String>>;
}

/// 生产实现：tokio-tungstenite 连接。
pub(crate) struct TungsteniteConn<S>(pub(crate) S);

#[async_trait]
impl<S> WsConn for TungsteniteConn<S>
where
    S: futures_util::Stream<Item = Result<Message, WsError>>
        + futures_util::Sink<Message, Error = WsError>
        + Send
        + Unpin,
{
    async fn send_text(&mut self, text: &str) -> Result<(), String> {
        futures_util::SinkExt::send(&mut self.0, Message::text(text.to_string()))
            .await
            .map_err(|e| e.to_string())
    }

    async fn recv(&mut self) -> Option<Result<String, String>> {
        loop {
            use futures_util::StreamExt;
            match self.0.next().await {
                None => return None,
                Some(Ok(Message::Text(t))) => return Some(Ok(t.as_str().to_string())),
                Some(Ok(Message::Close(_))) => return None,
                // Ping/Pong 由 tungstenite 内部应答；Binary 不属于本协议。
                Some(Ok(_)) => continue,
                Some(Err(e)) => return Some(Err(e.to_string())),
            }
        }
    }
}

/// 连接 Realtime 转写 WS（带超时与可选 Bearer）。
async fn connect_realtime(url: &str, api_key: Option<&str>) -> Result<Box<dyn WsConn>, String> {
    let mut request = url
        .into_client_request()
        .map_err(|e| format!("WS URL 非法: {e}"))?;
    if let Some(key) = api_key {
        let header = http::HeaderValue::from_str(&format!("Bearer {key}"))
            .map_err(|e| format!("API Key 含非法字符: {e}"))?;
        request
            .headers_mut()
            .insert(http::header::AUTHORIZATION, header);
    }
    let (stream, _response) = tokio::time::timeout(
        std::time::Duration::from_millis(CONNECT_TIMEOUT_MS),
        tokio_tungstenite::connect_async(request),
    )
    .await
    .map_err(|_| format!("连接超时（{CONNECT_TIMEOUT_MS}ms）"))?
    .map_err(|e| format!("WebSocket 握手失败: {e}"))?;
    Ok(Box::new(TungsteniteConn(stream)))
}

// ── 段账本（reader task 单写）───────────────────────────────────────────

/// 会话内的 span/累计文本状态。reader task 写，port 在 finish 时读。
#[derive(Debug, Default)]
struct SpanLedger {
    /// 已定稿段的累计全文（Final 用）。
    cumulative: String,
    /// 当前段起始样本（= 上一段 end）。
    span_start_sample: u64,
    /// 当前（未定稿）段所属 item_id。
    current_item: Option<String>,
    /// 最近一次定稿的 item_id——同 item 的迟到 delta 不得复活已关段。
    closed_item: Option<String>,
    /// 当前段预览文本。
    current_text: String,
    /// 已分配 span_id 数。
    next_span_id: u64,
    /// 预览事件 revision（每次 Preview/Partial 递增）。
    revision: u64,
}

impl SpanLedger {
    fn new() -> Self {
        Self::default()
    }

    /// 处理 delta；返回 `(预览文本, revision, 段起始样本)`。
    ///
    /// delta 一律按**累计替换**（funasr 系实测语义：每次 delta 是当前段的
    /// 完整最新版，含尾部修正）——增量拼接仅在 OpenAI 官方方言下成立，
    /// 但官方需 24kHz 等完整适配（见 `build_session_update` 注释），未开。
    /// 同 item 定稿后的迟到 delta 返回 None（不复活已关段）。
    fn on_delta(&mut self, item: Option<&str>, text: &str) -> Option<(String, u64, u64)> {
        if let (Some(closed), Some(cur)) = (self.closed_item.as_deref(), item) {
            if closed == cur {
                return None;
            }
        }
        if item.is_some() && self.current_item.as_deref() != item {
            self.current_item = item.map(str::to_string);
            self.current_text.clear();
        }
        self.current_text = text.to_string();
        self.revision += 1;
        Some((
            self.current_text.clone(),
            self.revision,
            self.span_start_sample,
        ))
    }

    /// 处理 completed；返回 `(Draft span, 累计全文)`。
    fn on_completed(
        &mut self,
        item: Option<&str>,
        transcript: &str,
        samples_end: u64,
    ) -> (DraftSpan, String) {
        let start = self.span_start_sample;
        let end = samples_end.max(start);
        let span = DraftSpan::new(self.next_span_id, AudioRange::new(start, end), transcript, 1);
        self.next_span_id += 1;
        self.span_start_sample = end;
        self.cumulative.push_str(transcript);
        self.current_text.clear();
        self.closed_item = item.map(str::to_string);
        (span, self.cumulative.clone())
    }

    /// Final 文本：已定稿累计 + 未定稿预览尾巴（超时收尾时尽量不丢已见文字；
    /// 正常路径 `current_text` 已被 completed 清空，等价于 `cumulative`）。
    fn final_text(&self) -> String {
        let mut text = self.cumulative.clone();
        text.push_str(&self.current_text);
        text
    }
}

// ── 会话上下文与 reader task ────────────────────────────────────────────

enum WsCmd {
    Append(Vec<u8>),
    Commit,
}

struct SessionCtx {
    generation: u64,
    profile: RecognitionProfile,
    bus_slot: Arc<BusSlot>,
    ledger: Arc<StdMutex<SpanLedger>>,
    samples_total: Arc<AtomicU64>,
    /// finish 等待与 settle 通知共享同一实例。
    settled: Arc<Notify>,
    settled_flag: Arc<AtomicBool>,
    /// reader 遇到错误时置位——port 的 finish 据此跳过 Final（Error 已是终态）。
    failed: Arc<AtomicBool>,
    /// port 已进入终态（finish 发 Final 前置位）——reader 收到迟到事件不再外发。
    /// 残余竞窗（reader 检查后、port 发 Final 前的微秒级窗口内塞入 Draft）
    /// 服务端不会对同一 item 重发 completed，可接受。
    finished: Arc<AtomicBool>,
}

impl SessionCtx {
    fn settle(&self) {
        if !self.settled_flag.swap(true, Ordering::AcqRel) {
            self.settled.notify_waiters();
        }
    }

    fn current_bus(&self) -> Option<Arc<EventBus>> {
        match self.bus_slot.lock() {
            Ok(slot) => slot.clone(),
            Err(poison) => poison.into_inner().clone(),
        }
    }

    /// delta → Preview（PreviewDraft）/ Partial（Legacy）。走 preview
    /// latest-slot：预览可被更新值替换，不占可靠队列。
    fn emit_preview(&self, text: String, revision: u64, span_start: u64, samples_end: u64) {
        let Some(bus) = self.current_bus() else {
            return;
        };
        let end = samples_end.max(span_start);
        let event = match self.profile {
            RecognitionProfile::PreviewDraft => {
                let range = AudioRange::new(span_start, end);
                SttEvent::Preview {
                    generation: self.generation,
                    request_id: self.generation,
                    audio_range: range,
                    revision,
                    text: text.clone(),
                    spans: vec![PreviewSegment::new(range, text)],
                }
            }
            RecognitionProfile::Legacy => SttEvent::Partial {
                generation: self.generation,
                revision,
                confirmed: String::new(),
                confirmed_changed: false,
                preview: text,
            },
        };
        bus.set_preview(event);
    }

    /// completed → Draft（PreviewDraft）/ 带 confirmed 的 Partial（Legacy）。
    async fn emit_completed(&self, span: DraftSpan, cumulative: String) {
        let Some(bus) = self.current_bus() else {
            return;
        };
        let event = match self.profile {
            RecognitionProfile::PreviewDraft => SttEvent::Draft {
                generation: self.generation,
                span,
            },
            RecognitionProfile::Legacy => SttEvent::Partial {
                generation: self.generation,
                revision: span.span_id,
                confirmed: cumulative,
                confirmed_changed: true,
                preview: String::new(),
            },
        };
        bus.send_reliable(event).await;
    }

    async fn emit_error(&self, message: String) {
        self.failed.store(true, Ordering::Release);
        if let Some(bus) = self.current_bus() {
            bus.send_reliable(SttEvent::Error {
                generation: self.generation,
                message,
            })
            .await;
        }
    }
}

/// WS 会话主循环：双工处理命令（append/commit）与服务端事件。
async fn run_ws_session(
    mut conn: Box<dyn WsConn>,
    mut cmd_rx: mpsc::UnboundedReceiver<WsCmd>,
    ctx: Arc<SessionCtx>,
    session_update: String,
) {
    if let Err(e) = conn.send_text(&session_update).await {
        ctx.emit_error(format!("发送会话配置失败: {e}")).await;
        ctx.settle();
        return;
    }
    let mut commit_sent = false;
    loop {
        tokio::select! {
            biased;
            cmd = cmd_rx.recv() => {
                match cmd {
                    Some(WsCmd::Append(bytes)) => {
                        if let Err(e) = conn.send_text(&build_append_message(&bytes)).await {
                            ctx.emit_error(format!("发送音频失败: {e}")).await;
                            ctx.settle();
                            return;
                        }
                    }
                    Some(WsCmd::Commit) => {
                        commit_sent = true;
                        if let Err(e) = conn.send_text(&build_commit_message()).await {
                            ctx.emit_error(format!("发送 commit 失败: {e}")).await;
                            ctx.settle();
                            return;
                        }
                    }
                    None => {
                        // port 端已释放（cancel 或 finish 收尾完成）——直接退出，
                        // conn drop 时 socket 关闭。
                        return;
                    }
                }
            }
            msg = conn.recv() => {
                match msg {
                    None => {
                        // 服务端关闭。commit 后的关闭视为已收尾。
                        if commit_sent {
                            ctx.settle();
                        }
                        return;
                    }
                    Some(Err(e)) => {
                        ctx.emit_error(format!("WebSocket 错误: {e}")).await;
                        ctx.settle();
                        return;
                    }
                    Some(Ok(text)) => {
                        let samples = ctx.samples_total.load(Ordering::Relaxed);
                        match parse_server_event(&text) {
                            ServerEvent::Delta { item, text } => {
                                // TRACE 档记录预览正文（排查错字/断句/重复的唯一
                                // 窗口，用户显式开深挖档才可见）——与 AI 对话日志
                                // 同模式：blink 自身结构化日志承载内容诊断，
                                // tungstenite 帧级 dump（含 base64 音频）恒压。
                                tracing::trace!(
                                    item = item.as_deref().unwrap_or("-"),
                                    samples,
                                    chars = text.chars().count(),
                                    preview = %text,
                                    "云端流式 delta（预览）"
                                );
                                if ctx.finished.load(Ordering::Acquire) {
                                    continue;
                                }
                                let outcome = ctx
                                    .ledger
                                    .lock()
                                    .ok()
                                    .and_then(|mut l| l.on_delta(item.as_deref(), &text));
                                if let Some((preview, revision, span_start)) = outcome {
                                    ctx.emit_preview(preview, revision, span_start, samples);
                                }
                            }
                            ServerEvent::Completed { item, transcript } => {
                                tracing::trace!(
                                    item = item.as_deref().unwrap_or("-"),
                                    samples,
                                    chars = transcript.chars().count(),
                                    transcript = %transcript,
                                    "云端流式 completed（段定稿正文）"
                                );
                                if ctx.finished.load(Ordering::Acquire) {
                                    continue;
                                }
                                // guard 作用域收窄——不得跨 await 持有。
                                let completed = ctx
                                    .ledger
                                    .lock()
                                    .ok()
                                    .map(|mut ledger| {
                                        ledger.on_completed(item.as_deref(), &transcript, samples)
                                    });
                                if let Some((span, cumulative)) = completed {
                                    ctx.emit_completed(span, cumulative).await;
                                }
                                if commit_sent {
                                    ctx.settle();
                                }
                            }
                            ServerEvent::Committed => {
                                // 静音会话没有 completed：committed + 宽限后收尾。
                                if commit_sent {
                                    let grace_ctx = Arc::clone(&ctx);
                                    tokio::spawn(async move {
                                        tokio::time::sleep(std::time::Duration::from_millis(
                                            COMMITTED_GRACE_MS,
                                        ))
                                        .await;
                                        grace_ctx.settle();
                                    });
                                }
                            }
                            ServerEvent::Error { message } => {
                                ctx.emit_error(format!("服务端错误: {message}")).await;
                                ctx.settle();
                                return;
                            }
                            ServerEvent::SessionCreated | ServerEvent::Ignored => {}
                        }
                    }
                }
            }
        }
    }
}

// ── CloudStreamingPort ──────────────────────────────────────────────────

/// 云端流式 port：WS 真流式优先，连接失败内置回退整段上传。
///
/// 生命周期与 [`StreamingSttPort`] 契约一致；`warm_up` 恒 no-op（云端无
/// 本地 worker 懒加载，且避免对付费 API 发无意义请求）。
pub struct CloudStreamingPort {
    profile: RecognitionProfile,
    inner: TokioMutex<Option<ActiveSession>>,
    generation: AtomicU64,
    active_event_generation: Arc<AtomicU64>,
    bus_slot: Arc<BusSlot>,
    chunks_received: AtomicU64,
    /// 已接收样本总数（诊断快照 `pcm_samples` 口径；session 被 take 后仍可读）。
    samples_received: AtomicU64,
    /// finish 等待预算（默认 [`FINALIZE_WAIT_TIMEOUT_MS`]；测试可注入短值）。
    finalize_wait: StdMutex<std::time::Duration>,
}

enum ActiveSession {
    Ws(WsSession),
    /// 整段上传回退：复用 `CloudSttEngine` 做样本累积与 finalize，
    /// 终态（Final/Error）直接发在本 port 的事件总线上——与旧
    /// `GgufStreamingAdapter(CloudSttEngine)` 行为一致（无 Partial、
    /// 仅终态），且 generation 与 port 对外计数一致（不走第二套总线）。
    Fallback {
        engine: Arc<cloud::CloudSttEngine>,
    },
}

struct WsSession {
    generation: u64,
    cmd_tx: mpsc::UnboundedSender<WsCmd>,
    settled: Arc<Notify>,
    settled_flag: Arc<AtomicBool>,
    failed: Arc<AtomicBool>,
    finished: Arc<AtomicBool>,
    ledger: Arc<StdMutex<SpanLedger>>,
    samples_total: Arc<AtomicU64>,
    /// 攒批发送缓冲（< FLUSH_MIN_SAMPLES 的 push 在此合并）。
    pending: Vec<f32>,
}

impl CloudStreamingPort {
    pub fn new(profile: RecognitionProfile) -> Self {
        Self {
            profile,
            inner: TokioMutex::new(None),
            generation: AtomicU64::new(0),
            active_event_generation: Arc::new(AtomicU64::new(0)),
            bus_slot: Arc::new(StdMutex::new(None)),
            chunks_received: AtomicU64::new(0),
            samples_received: AtomicU64::new(0),
            finalize_wait: StdMutex::new(std::time::Duration::from_millis(
                FINALIZE_WAIT_TIMEOUT_MS,
            )),
        }
    }

    /// 测试注入：缩短 finish 等待预算。
    #[cfg(test)]
    pub(crate) fn with_finalize_wait(self, wait: std::time::Duration) -> Self {
        *self.finalize_wait.lock().unwrap() = wait;
        self
    }

    /// 启动 WS 会话（生产连接或测试注入的 conn 共用此路径）。
    fn start_ws_session(&self, generation: u64, conn: Box<dyn WsConn>) -> ActiveSession {
        let ledger = Arc::new(StdMutex::new(SpanLedger::new()));
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let settled = Arc::new(Notify::new());
        let ctx = Arc::new(SessionCtx {
            generation,
            profile: self.profile,
            bus_slot: Arc::clone(&self.bus_slot),
            ledger: Arc::clone(&ledger),
            samples_total: Arc::new(AtomicU64::new(0)),
            settled: Arc::clone(&settled),
            settled_flag: Arc::new(AtomicBool::new(false)),
            failed: Arc::new(AtomicBool::new(false)),
            finished: Arc::new(AtomicBool::new(false)),
        });
        let session_update = build_session_update();
        let runner_ctx = Arc::clone(&ctx);
        tokio::spawn(run_ws_session(conn, cmd_rx, runner_ctx, session_update));
        ActiveSession::Ws(WsSession {
            generation,
            cmd_tx,
            settled,
            settled_flag: Arc::clone(&ctx.settled_flag),
            failed: Arc::clone(&ctx.failed),
            finished: Arc::clone(&ctx.finished),
            ledger,
            samples_total: Arc::clone(&ctx.samples_total),
            pending: Vec::new(),
        })
    }

    /// 测试注入 fake 连接启动会话（跳过网络）。endpoint 仅保持与生产入口同构。
    #[cfg(test)]
    pub(crate) async fn begin_session_with_conn(
        &self,
        conn: Box<dyn WsConn>,
        _endpoint: &RealtimeEndpoint,
    ) -> Result<u64, SttError> {
        let mut active = self.inner.lock().await;
        if active.is_some() {
            return Err(SttError::Engine("session already active".into()));
        }
        let generation = self.generation.fetch_add(1, Ordering::AcqRel) + 1;
        self.active_event_generation
            .store(generation, Ordering::Release);
        *active = Some(self.start_ws_session(generation, conn));
        Ok(generation)
    }

    /// 终态事件（Final/Error）发到本 port 的总线（无订阅则丢弃）。
    async fn emit_event(&self, event: SttEvent) {
        // TRACE 档记录最终正文——覆盖 WS 定稿、超时兜底（累计+未定稿预览尾巴）
        // 与整段上传回退三条路径，便于对照上屏内容排查。
        if let SttEvent::Final { text, .. } = &event {
            tracing::trace!(
                chars = text.chars().count(),
                final_text = %text,
                "云端流式 Final（最终正文）"
            );
        }
        let bus = match self.bus_slot.lock() {
            Ok(slot) => slot.clone(),
            Err(poison) => poison.into_inner().clone(),
        };
        if let Some(bus) = bus {
            bus.send_reliable(event).await;
        }
    }

    fn clear_preview(&self) {
        let bus = match self.bus_slot.lock() {
            Ok(slot) => slot.clone(),
            Err(poison) => poison.into_inner().clone(),
        };
        if let Some(bus) = bus {
            bus.clear_preview();
        }
    }
}

#[async_trait]
impl StreamingSttPort for CloudStreamingPort {
    async fn begin_session(&self) -> Result<u64, SttError> {
        let mut active = self.inner.lock().await;
        if active.is_some() {
            return Err(SttError::Engine("session already active".into()));
        }
        let config = blink_domain_config::stt_config::get_stt_config();
        let endpoint = resolve_realtime_endpoint(&config)?;
        let generation = self.generation.fetch_add(1, Ordering::AcqRel) + 1;
        self.active_event_generation
            .store(generation, Ordering::Release);

        let session = match connect_realtime(&endpoint.ws_url, endpoint.api_key.as_deref()).await {
            Ok(conn) => self.start_ws_session(generation, conn),
            Err(e) => {
                // 回退：整段上传（CloudSttEngine 累积 + finalize）。
                tracing::info!(
                    ws_url = %endpoint.ws_url,
                    error = %e,
                    "云端实时流式 WS 不可用，回退整段上传"
                );
                ActiveSession::Fallback {
                    engine: Arc::new(cloud::CloudSttEngine::new()),
                }
            }
        };
        *active = Some(session);
        Ok(generation)
    }

    async fn push_audio(&self, generation: u64, samples: &[f32]) -> Result<(), SttError> {
        let mut active = self.inner.lock().await;
        match active.as_mut() {
            Some(ActiveSession::Ws(ws)) if ws.generation == generation => {
                self.chunks_received.fetch_add(1, Ordering::Relaxed);
                self.samples_received
                    .fetch_add(samples.len() as u64, Ordering::Relaxed);
                ws.samples_total
                    .fetch_add(samples.len() as u64, Ordering::Relaxed);
                ws.pending.extend_from_slice(samples);
                if ws.pending.len() >= FLUSH_MIN_SAMPLES {
                    let pcm = samples_to_pcm16(&ws.pending);
                    ws.pending.clear();
                    ws.cmd_tx
                        .send(WsCmd::Append(pcm))
                        .map_err(|_| SttError::Engine("WS 会话任务已退出".into()))?;
                }
                Ok(())
            }
            Some(ActiveSession::Fallback { engine }) => {
                // 累积样本；与旧 CloudSttEngine 行为一致（恒返回空串 partial）。
                self.chunks_received.fetch_add(1, Ordering::Relaxed);
                self.samples_received
                    .fetch_add(samples.len() as u64, Ordering::Relaxed);
                engine.transcribe_chunk(samples).await.map(|_| ())
            }
            _ => Err(SttError::Engine(format!(
                "push_audio generation 不匹配或无活跃 session: {generation}"
            ))),
        }
    }

    async fn finish_session(&self, generation: u64) -> Result<(), SttError> {
        let session = {
            let mut active = self.inner.lock().await;
            match active.take() {
                Some(session)
                    if matches!(
                        &session,
                        ActiveSession::Ws(ws) if ws.generation == generation
                    ) =>
                {
                    if let ActiveSession::Ws(ws) = &session {
                        // 冲刷攒批缓冲 + commit。发送失败不视为错误——会话可能
                        // 已由 reader 侧终止（Error 已发）或服务端已关闭。
                        if !ws.pending.is_empty() {
                            let pcm = samples_to_pcm16(&ws.pending);
                            let _ = ws.cmd_tx.send(WsCmd::Append(pcm));
                        }
                        let _ = ws.cmd_tx.send(WsCmd::Commit);
                    }
                    session
                }
                Some(ActiveSession::Fallback { engine }) => {
                    // 整段上传回退：finalize 的 Ok/Err 都映射为终态事件
                    // （与旧适配器一致），走本 port 总线与 port generation。
                    let event = match engine.finalize().await {
                        Ok(text) => SttEvent::Final { generation, text },
                        Err(e) => SttEvent::Error {
                            generation,
                            message: e.to_string(),
                        },
                    };
                    self.emit_event(event).await;
                    return Ok(());
                }
                other => {
                    if other.is_some() {
                        *active = other;
                    }
                    return Err(SttError::Engine(format!(
                        "finish_session generation 不匹配或无活跃 session: {generation}"
                    )));
                }
            }
        };
        let ActiveSession::Ws(ws) = session else {
            unreachable!("上方已按 Ws 分支过滤");
        };

        // 有界等待终段（completed / committed+宽限 / 服务端关闭 / 预算耗尽）。
        let budget = *self.finalize_wait.lock().unwrap();
        let deadline = tokio::time::Instant::now() + budget;
        loop {
            let notified = ws.settled.notified();
            if ws.settled_flag.load(Ordering::Acquire) {
                break;
            }
            if tokio::time::Instant::now() >= deadline {
                tracing::warn!(
                    budget_ms = budget.as_millis() as u64,
                    "云端流式 finish 等待终段超时，按当前累计文本收尾"
                );
                break;
            }
            let _ = tokio::time::timeout_at(deadline, notified).await;
        }

        // reader 侧已报错（Error 事件是终态）则不再补 Final。
        if !ws.failed.load(Ordering::Acquire) {
            // 先立 finished 墙（reader 停止外发迟到事件），再快照终态文本。
            ws.finished.store(true, Ordering::Release);
            let final_text = ws
                .ledger
                .lock()
                .map(|l| l.final_text())
                .unwrap_or_default();
            self.emit_event(SttEvent::Final {
                generation,
                text: final_text,
            })
            .await;
        }
        // ws 在此 drop → cmd_tx 关闭 → reader task 退出并关闭 socket。
        Ok(())
    }

    async fn cancel_session(&self, generation: u64) -> Result<(), SttError> {
        let session = {
            let mut active = self.inner.lock().await;
            match active.take() {
                Some(ActiveSession::Ws(ws)) if ws.generation == generation => Some(ws),
                // 整段上传回退：丢弃累积样本即可（engine drop）。
                Some(ActiveSession::Fallback { .. }) => None,
                _ => None,
            }
        };
        drop(session); // Ws：drop → cmd_tx 关闭 → reader 退出、socket 关闭
        self.active_event_generation.store(0, Ordering::Release);
        self.clear_preview();
        // 递增 generation 使在途事件失效（幂等；对齐 adapter.reset 语义）。
        self.generation.fetch_add(1, Ordering::AcqRel);
        Ok(())
    }

    async fn reset(&self) -> Result<(), SttError> {
        *self.inner.lock().await = None;
        self.active_event_generation.store(0, Ordering::Release);
        self.clear_preview();
        self.generation.fetch_add(1, Ordering::AcqRel);
        Ok(())
    }

    fn supports_native_partial(&self) -> bool {
        true
    }

    fn recognition_profile(&self) -> RecognitionProfile {
        self.profile
    }

    fn events(&self) -> mpsc::Receiver<SttEvent> {
        // 与适配器一致：每次订阅重建总线；generation 墙阻止旧会话事件进入新会话。
        let (bus, rx) = EventBus::new(
            Arc::clone(&self.active_event_generation),
            EventStats::fresh(),
        );
        match self.bus_slot.lock() {
            Ok(mut slot) => *slot = Some(bus),
            Err(poison) => *poison.into_inner() = Some(bus),
        }
        rx
    }

    fn stream_stats(&self) -> SttStreamStats {
        let mut stats = SttStreamStats::default();
        stats.chunks_received = self.chunks_received.load(Ordering::Relaxed);
        stats.pcm_samples = self.samples_received.load(Ordering::Relaxed) as usize;
        let bus = match self.bus_slot.lock() {
            Ok(slot) => slot.clone(),
            Err(poison) => poison.into_inner().clone(),
        };
        if let Some(bus) = bus {
            stats.queue_depth = bus.queue_depth();
            stats.queue_capacity = EventBus::capacity();
            let (emitted, dropped_closed, max_depth) = bus.delivery_stats();
            stats.partials_emitted = emitted;
            stats.partials_dropped_closed = dropped_closed;
            stats.max_queue_depth = max_depth;
        }
        stats
    }
}

// ── 流式能力探测（设置页「流式测试」按钮）──────────────────────────────

/// WS 探测结果。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct WsProbeResult {
    pub supported: bool,
    pub detail: String,
    pub latency_ms: Option<u64>,
}

/// SSE（`stream=true`）探测结果。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SseProbeResult {
    pub supported: bool,
    pub detail: String,
}

/// 流式能力报告。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct StreamingSupportReport {
    pub ws: WsProbeResult,
    pub sse: SseProbeResult,
}

/// 探测当前云端端点的流式能力。
///
/// - **WS**：Realtime 转写握手 + session.update，等待首个服务端事件。
/// - **SSE**：POST 250ms 静音 WAV + `stream=true`，检查 `text/event-stream`
///   与 `transcript.text` 事件（Speaches 系社区标准的整段 SSE 流式）。
///
/// 两者互不阻塞：任一失败不影响另一个的结果。未配置云端供应商时两项
/// 均为 `supported=false, detail="云端 STT 未配置供应商"`。
pub async fn probe_streaming_support() -> StreamingSupportReport {
    let config = blink_domain_config::stt_config::get_stt_config();
    let endpoint = match resolve_realtime_endpoint(&config) {
        Ok(endpoint) => endpoint,
        Err(e) => {
            let detail = e.to_string();
            return StreamingSupportReport {
                ws: WsProbeResult {
                    supported: false,
                    detail: detail.clone(),
                    latency_ms: None,
                },
                sse: SseProbeResult {
                    supported: false,
                    detail,
                },
            };
        }
    };

    let ws = probe_ws(&endpoint).await;
    let sse = probe_sse(&endpoint).await;
    StreamingSupportReport { ws, sse }
}

async fn probe_ws(endpoint: &RealtimeEndpoint) -> WsProbeResult {
    let started = std::time::Instant::now();
    let mut conn = match connect_realtime(&endpoint.ws_url, endpoint.api_key.as_deref()).await {
        Ok(conn) => conn,
        Err(e) => {
            return WsProbeResult {
                supported: false,
                detail: e,
                latency_ms: None,
            }
        }
    };
    let latency_ms = started.elapsed().as_millis() as u64;
    if let Err(e) = conn.send_text(&build_session_update()).await
    {
        return WsProbeResult {
            supported: false,
            detail: format!("发送会话配置失败: {e}"),
            latency_ms: Some(latency_ms),
        };
    }
    // 等待首个服务端事件（3s）——session.created 即证明协议通道可用。
    match tokio::time::timeout(
        std::time::Duration::from_millis(CONNECT_TIMEOUT_MS),
        conn.recv(),
    )
    .await
    {
        Ok(Some(Ok(text))) => {
            let event_type = serde_json::from_str::<serde_json::Value>(&text)
                .ok()
                .and_then(|v| {
                    v.get("type")
                        .and_then(|t| t.as_str())
                        .map(str::to_string)
                })
                .unwrap_or_else(|| "unknown".into());
            WsProbeResult {
                supported: true,
                detail: event_type,
                latency_ms: Some(latency_ms),
            }
        }
        Ok(Some(Err(e))) => WsProbeResult {
            supported: false,
            detail: format!("读取服务端事件失败: {e}"),
            latency_ms: Some(latency_ms),
        },
        Ok(None) => WsProbeResult {
            supported: false,
            detail: "连接被服务端关闭".into(),
            latency_ms: Some(latency_ms),
        },
        Err(_) => WsProbeResult {
            supported: false,
            detail: "等待首个服务端事件超时".into(),
            latency_ms: Some(latency_ms),
        },
    }
}

async fn probe_sse(endpoint: &RealtimeEndpoint) -> SseProbeResult {
    let fail = |detail: String| SseProbeResult {
        supported: false,
        detail,
    };
    // 250ms 静音（16kHz mono）——足以触发服务端事件流，无语音内容。
    // keyless 自部署服务（funasr）不带 Bearer；官方端点缺 key 会得到 401，
    // 如实呈现在 detail。
    let wav = crate::wav::pcm_to_wav(&[0.0; 4_000], 16_000, 1);
    let url = format!("{}/audio/transcriptions", endpoint.base_url_for_http());
    let client = match blink_infra::net::proxied_builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
    {
        Ok(client) => client,
        Err(e) => return fail(format!("构造 HTTP client 失败: {e}")),
    };
    let part = match reqwest::multipart::Part::bytes(wav)
        .file_name("probe.wav")
        .mime_str("audio/wav")
    {
        Ok(part) => part,
        Err(e) => return fail(format!("构造 multipart 失败: {e}")),
    };
    let form = reqwest::multipart::Form::new()
        .text("model", endpoint.model_id.clone())
        .text("stream", "true")
        .part("file", part);
    let mut request = client.post(&url).multipart(form);
    if let Some(key) = endpoint.api_key.as_deref() {
        request = request.bearer_auth(key);
    }
    let resp = match request.send().await {
        Ok(resp) => resp,
        Err(e) => return fail(format!("请求失败: {e}")),
    };
    let is_event_stream = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("text/event-stream"));
    if !is_event_stream {
        let status = resp.status();
        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("(none)")
            .to_string();
        return fail(format!("HTTP {status}（{content_type}，非 text/event-stream）"));
    }
    // 流式响应——读完整事件流确认格式（transcript.text.*）。
    match tokio::time::timeout(std::time::Duration::from_secs(5), resp.bytes()).await {
        Ok(Ok(body)) => {
            let text = String::from_utf8_lossy(&body);
            let has_event = text.contains("transcript.text");
            SseProbeResult {
                supported: has_event,
                detail: if has_event {
                    "SSE 流式（transcript.text 事件）".into()
                } else {
                    format!(
                        "响应非 transcript.text 事件流: {}",
                        text.chars().take(120).collect::<String>()
                    )
                },
            }
        }
        Ok(Err(e)) => fail(format!("读取响应失败: {e}")),
        Err(_) => fail("读取流式响应超时".into()),
    }
}

impl RealtimeEndpoint {
    /// SSE/HTTP 探测用的 base_url（还原 http(s) scheme）。
    fn base_url_for_http(&self) -> String {
        let ws = &self.ws_url;
        if let Some(rest) = ws.strip_prefix("wss://") {
            format!("https://{rest}")
        } else if let Some(rest) = ws.strip_prefix("ws://") {
            format!("http://{rest}")
        } else {
            ws.clone()
        }
        .trim_end_matches("/realtime?intent=transcription")
        .to_string()
    }
}

// ── 测试 ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio_tungstenite::WebSocketStream;

    /// Fake WS 连接：port 发出的客户端消息进入 `client_rx`（测试侧观察），
    /// 测试经 `server_tx` 注入"服务端事件"。
    struct FakeConn {
        client_tx: mpsc::UnboundedSender<String>,
        server_rx: mpsc::UnboundedReceiver<String>,
    }

    impl FakeConn {
        fn new() -> (Self, mpsc::UnboundedReceiver<String>, mpsc::UnboundedSender<String>) {
            let (server_tx, server_rx) = mpsc::unbounded_channel();
            let (client_tx, client_rx) = mpsc::unbounded_channel();
            (Self { client_tx, server_rx }, client_rx, server_tx)
        }
    }

    #[async_trait]
    impl WsConn for FakeConn {
        async fn send_text(&mut self, text: &str) -> Result<(), String> {
            self.client_tx
                .send(text.to_string())
                .map_err(|_| "test harness closed".into())
        }

        async fn recv(&mut self) -> Option<Result<String, String>> {
            self.server_rx.recv().await.map(Ok)
        }
    }

    fn endpoint() -> RealtimeEndpoint {
        RealtimeEndpoint {
            ws_url: "ws://test/v1/realtime?intent=transcription".into(),
            api_key: None,
            model_id: "sensevoice-small".into(),
        }
    }

    /// 收集当前已到达的事件。
    fn drain(rx: &mut mpsc::Receiver<SttEvent>) -> Vec<SttEvent> {
        let mut out = Vec::new();
        while let Ok(event) = rx.try_recv() {
            out.push(event);
        }
        out
    }

    /// 有界等待至少一个事件到达。
    async fn wait_events(rx: &mut mpsc::Receiver<SttEvent>, ms: u64) -> Vec<SttEvent> {
        let deadline = tokio::time::Instant::now() + Duration::from_millis(ms);
        loop {
            let events = drain(rx);
            if !events.is_empty() {
                return events;
            }
            if tokio::time::Instant::now() >= deadline {
                return Vec::new();
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    fn server_event(json: &str) -> String {
        json.to_string()
    }

    // ── 协议层 ──

    #[test]
    fn derive_ws_url_handles_schemes_and_paths() {
        assert_eq!(
            derive_ws_url("https://api.openai.com/v1").unwrap(),
            "wss://api.openai.com/v1/realtime?intent=transcription"
        );
        assert_eq!(
            derive_ws_url("http://192.168.31.77:38040/v1/").unwrap(),
            "ws://192.168.31.77:38040/v1/realtime?intent=transcription"
        );
        // 无路径补 /v1
        assert_eq!(
            derive_ws_url("http://host:9000").unwrap(),
            "ws://host:9000/v1/realtime?intent=transcription"
        );
        // 已是 ws 协议头则保留
        assert_eq!(
            derive_ws_url("wss://example.com/custom").unwrap(),
            "wss://example.com/custom/realtime?intent=transcription"
        );
        assert!(derive_ws_url("api.openai.com/v1").is_err());
    }

    #[test]
    fn samples_to_pcm16_clamps_and_encodes_le() {
        assert_eq!(samples_to_pcm16(&[0.0]), vec![0x00, 0x00]);
        assert_eq!(samples_to_pcm16(&[1.0]), vec![0xff, 0x7f]);
        // 超界钳制
        assert_eq!(samples_to_pcm16(&[2.0]), vec![0xff, 0x7f]);
        assert_eq!(
            samples_to_pcm16(&[-1.0]),
            (-32767i16).to_le_bytes().to_vec()
        );
    }

    #[test]
    fn parse_server_event_covers_both_dialects() {
        // funasr 实测 fixture
        assert_eq!(
            parse_server_event(
                r#"{"type":"session.created","session":{"id":"sess_1","model":"sensevoice"}}"#
            ),
            ServerEvent::SessionCreated
        );
        assert_eq!(
            parse_server_event(
                r#"{"type":"conversation.item.input_audio_transcription.delta","item_id":"it_0","delta":"今天天气不错。"}"#
            ),
            ServerEvent::Delta {
                item: Some("it_0".into()),
                text: "今天天气不错。".into()
            }
        );
        assert_eq!(
            parse_server_event(
                r#"{"type":"conversation.item.input_audio_transcription.completed","item_id":"it_0","transcript":"全文","language":"zh"}"#
            ),
            ServerEvent::Completed {
                item: Some("it_0".into()),
                transcript: "全文".into()
            }
        );
        assert_eq!(
            parse_server_event(r#"{"type":"input_audio_buffer.committed"}"#),
            ServerEvent::Committed
        );
        assert_eq!(
            parse_server_event(r#"{"type":"conversation.item.created","item":{}}"#),
            ServerEvent::Ignored
        );
        // Speaches/SSE 同名风格
        assert_eq!(
            parse_server_event(r#"{"type":"transcript.text.done","text":"完成"}"#),
            ServerEvent::Completed {
                item: None,
                transcript: "完成".into()
            }
        );
        assert_eq!(
            parse_server_event(r#"{"type":"transcript.text.delta","delta":"增"}"#),
            ServerEvent::Delta {
                item: None,
                text: "增".into()
            }
        );
        // error
        assert_eq!(
            parse_server_event(r#"{"type":"error","error":{"message":"boom"}}"#),
            ServerEvent::Error {
                message: "boom".into()
            }
        );
        assert_eq!(parse_server_event("not json"), ServerEvent::Ignored);
    }

    #[test]
    fn session_update_is_funasr_style() {
        let msg = build_session_update();
        let v: serde_json::Value = serde_json::from_str(&msg).unwrap();
        assert_eq!(v["type"], "session.update");
        assert_eq!(v["session"]["turn_detection"]["type"], "server_vad");
    }

    // ── SpanLedger ──

    #[test]
    fn ledger_replaces_text_on_each_delta() {
        let mut ledger = SpanLedger::new();
        let (text, _, _) = ledger.on_delta(Some("it_0"), "今天。").unwrap();
        assert_eq!(text, "今天。");
        let (text, _, _) = ledger.on_delta(Some("it_0"), "今天天气不错。").unwrap();
        assert_eq!(text, "今天天气不错。");

        let (span, cumulative) = ledger.on_completed(Some("it_0"), "今天天气不错。", 160_000);
        assert_eq!(span.audio_range, AudioRange::new(0, 160_000));
        assert_eq!(span.text, "今天天气不错。");
        assert_eq!(cumulative, "今天天气不错。");

        // 同 item 迟到 delta 不复活已关段
        assert!(ledger.on_delta(Some("it_0"), "迟到").is_none());
        // 新 item 重置预览，段起点衔接上一段
        let (text, _, start) = ledger.on_delta(Some("it_1"), "第二句").unwrap();
        assert_eq!(text, "第二句");
        assert_eq!(start, 160_000);
        let (_, cumulative) = ledger.on_completed(Some("it_1"), "第二句", 320_000);
        assert_eq!(cumulative, "今天天气不错。第二句");
    }

    // ── Port 生命周期（fake conn）──

    #[tokio::test]
    async fn previewdraft_happy_path() {
        let port = CloudStreamingPort::new(RecognitionProfile::PreviewDraft)
            .with_finalize_wait(Duration::from_millis(2_000));
        let (conn, mut client_rx, server_tx) = FakeConn::new();
        let session_gen = port
            .begin_session_with_conn(Box::new(conn), &endpoint())
            .await
            .unwrap();
        let mut rx = port.events();

        // 首个客户端消息必须是会话配置
        let first = tokio::time::timeout(Duration::from_millis(500), client_rx.recv())
            .await
            .expect("session.update 超时")
            .unwrap();
        assert!(first.contains("session.update"));

        // push 1600 样本触发攒批冲刷
        port.push_audio(session_gen, &[0.05; FLUSH_MIN_SAMPLES])
            .await
            .unwrap();
        let append = tokio::time::timeout(Duration::from_millis(500), client_rx.recv())
            .await
            .expect("append 超时")
            .unwrap();
        assert!(append.contains("input_audio_buffer.append"));

        // 服务端累计 delta ×2 → Preview 更新
        server_tx
            .send(server_event(
                r#"{"type":"conversation.item.input_audio_transcription.delta","item_id":"it_0","delta":"今天。"}"#,
            ))
            .unwrap();
        let events = wait_events(&mut rx, 1_000).await;
        assert!(
            events
                .iter()
                .any(|e| matches!(e, SttEvent::Preview { text, .. } if text == "今天。")),
            "应有 Preview(今天。)：{events:?}"
        );
        server_tx
            .send(server_event(
                r#"{"type":"conversation.item.input_audio_transcription.delta","item_id":"it_0","delta":"今天天气不错。"}"#,
            ))
            .unwrap();
        let events = wait_events(&mut rx, 1_000).await;
        assert!(
            events
                .iter()
                .any(|e| matches!(e, SttEvent::Preview { text, .. } if text == "今天天气不错。")),
            "累计 delta 应替换预览：{events:?}"
        );

        // completed → Draft span（范围 = 已推样本）
        server_tx
            .send(server_event(
                r#"{"type":"conversation.item.input_audio_transcription.completed","item_id":"it_0","transcript":"今天天气不错。","language":"zh"}"#,
            ))
            .unwrap();
        let events = wait_events(&mut rx, 1_000).await;
        assert!(
            events.iter().any(|e| matches!(e, SttEvent::Draft { span, .. }
                if span.text == "今天天气不错。"
                    && span.audio_range == AudioRange::new(0, FLUSH_MIN_SAMPLES as u64))),
            "应有 Draft span：{events:?}"
        );

        // finish：commit → committed（宽限收尾）→ Final
        port.finish_session(session_gen).await.unwrap();
        let commit = tokio::time::timeout(Duration::from_millis(500), client_rx.recv())
            .await
            .expect("commit 超时")
            .unwrap();
        assert!(commit.contains("input_audio_buffer.commit"));
        server_tx
            .send(server_event(r#"{"type":"input_audio_buffer.committed"}"#))
            .unwrap();
        let events = wait_events(&mut rx, 3_000).await;
        assert!(
            events
                .iter()
                .any(|e| matches!(e, SttEvent::Final { text, .. } if text == "今天天气不错。")),
            "应有 Final：{events:?}"
        );
    }

    #[tokio::test]
    async fn legacy_partial_flow() {
        let port = CloudStreamingPort::new(RecognitionProfile::Legacy)
            .with_finalize_wait(Duration::from_millis(200));
        let (conn, _client_rx, server_tx) = FakeConn::new();
        let session_gen = port
            .begin_session_with_conn(Box::new(conn), &endpoint())
            .await
            .unwrap();
        let mut rx = port.events();

        server_tx
            .send(server_event(
                r#"{"type":"transcript.text.delta","delta":"预览文本"}"#,
            ))
            .unwrap();
        let events = wait_events(&mut rx, 1_000).await;
        assert!(
            events.iter().any(|e| matches!(e, SttEvent::Partial {
                preview,
                confirmed_changed: false,
                ..
            } if preview == "预览文本"))
        );

        server_tx
            .send(server_event(r#"{"type":"transcript.text.done","text":"全文"}"#))
            .unwrap();
        let events = wait_events(&mut rx, 1_000).await;
        assert!(
            events.iter().any(|e| matches!(e, SttEvent::Partial {
                confirmed,
                confirmed_changed: true,
                ..
            } if confirmed == "全文"))
        );

        port.finish_session(session_gen).await.unwrap();
        let events = wait_events(&mut rx, 2_000).await;
        assert!(
            events
                .iter()
                .any(|e| matches!(e, SttEvent::Final { text, .. } if text == "全文"))
        );
    }

    #[tokio::test]
    async fn cancel_drops_pending_and_allows_next_session() {
        let port = CloudStreamingPort::new(RecognitionProfile::PreviewDraft);
        let (conn, _client_rx, server_tx) = FakeConn::new();
        let session_gen = port
            .begin_session_with_conn(Box::new(conn), &endpoint())
            .await
            .unwrap();
        let mut rx = port.events();

        server_tx
            .send(server_event(
                r#"{"type":"conversation.item.input_audio_transcription.delta","delta":"将取消"}"#,
            ))
            .unwrap();
        let _ = wait_events(&mut rx, 1_000).await;

        port.cancel_session(session_gen).await.unwrap();
        // cancel 后迟到事件被 generation 墙拦截
        server_tx
            .send(server_event(
                r#"{"type":"conversation.item.input_audio_transcription.completed","transcript":"迟到"}"#,
            ))
            .unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        let events = drain(&mut rx);
        assert!(
            !events.iter().any(|e| matches!(e, SttEvent::Draft { .. })),
            "cancel 后不应再有 Draft：{events:?}"
        );

        // 旧 generation push 报错；新 session 正常
        assert!(port.push_audio(session_gen, &[0.1]).await.is_err());
        let (conn2, _rx2, _tx2) = FakeConn::new();
        let gen2 = port
            .begin_session_with_conn(Box::new(conn2), &endpoint())
            .await
            .unwrap();
        assert!(gen2 > session_gen);
        assert!(port.push_audio(gen2, &[0.1; 32]).await.is_ok());
    }

    #[tokio::test]
    async fn finish_deadline_emits_final_on_silence() {
        // 服务端不回应 commit（静音/异常）——预算耗尽后仍按累计文本收尾。
        let port = CloudStreamingPort::new(RecognitionProfile::PreviewDraft)
            .with_finalize_wait(Duration::from_millis(150));
        let (conn, _client_rx, _server_tx) = FakeConn::new();
        let session_gen = port
            .begin_session_with_conn(Box::new(conn), &endpoint())
            .await
            .unwrap();
        let mut rx = port.events();
        port.finish_session(session_gen).await.unwrap();
        let events = wait_events(&mut rx, 1_000).await;
        assert!(
            events
                .iter()
                .any(|e| matches!(e, SttEvent::Final { text, .. } if text.is_empty()))
        );
    }

    #[tokio::test]
    async fn ws_error_emits_error_and_skips_final() {
        let port = CloudStreamingPort::new(RecognitionProfile::PreviewDraft);
        let (conn, _client_rx, server_tx) = FakeConn::new();
        let session_gen = port
            .begin_session_with_conn(Box::new(conn), &endpoint())
            .await
            .unwrap();
        let mut rx = port.events();

        server_tx
            .send(server_event(
                r#"{"type":"error","error":{"message":"model overloaded"}}"#,
            ))
            .unwrap();
        let events = wait_events(&mut rx, 1_000).await;
        assert!(
            events.iter().any(|e| matches!(e, SttEvent::Error { message, .. }
                if message.contains("model overloaded")))
        );

        port.finish_session(session_gen).await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        let events = drain(&mut rx);
        assert!(
            !events.iter().any(|e| matches!(e, SttEvent::Final { .. })),
            "Error 已是终态，不应再补 Final：{events:?}"
        );
    }

    /// 回退路径：连接不可达（127.0.0.1:1 立即拒绝）→ 内置回退整段上传。
    /// 依赖全局配置缓存——与 lib.rs `create_engine_returns_err_for_disabled_and_no_connection`
    /// 同款 init_cache + update_cache 双设置模式（OnceLock 只能设一次），结束时恢复。
    #[tokio::test]
    async fn connect_failure_falls_back_to_oneshot() {
        let saved = blink_domain_config::stt_config::get_stt_config();
        let mut cfg = blink_domain_config::stt_config::SttConfig::default();
        cfg.enabled = true;
        cfg.mode = blink_domain_config::stt_config::SttMode::Cloud;
        cfg.cloud_provider = Some(blink_domain_config::stt_config::SttCloudProvider {
            kind: "custom".into(),
            base_url: Some("http://127.0.0.1:1/v1".into()),
            model_id: "sensevoice-small".into(),
        });
        blink_domain_config::stt_config::init_cache(cfg.clone());
        blink_domain_config::stt_config::update_cache(&cfg);

        let port = CloudStreamingPort::new(RecognitionProfile::PreviewDraft);
        let session_gen = port.begin_session().await.expect("回退也应成功 begin");
        // 无 push（无样本）→ CloudSttEngine::finalize 短路 Ok("")（不触网络、
        // 不依赖凭据状态）→ Final 事件经本 port 总线交付（generation 一致）。
        let mut rx = port.events();
        port.finish_session(session_gen).await.unwrap();
        let events = wait_events(&mut rx, 2_000).await;
        assert!(
            events
                .iter()
                .any(|e| matches!(e, SttEvent::Final { text, .. } if text.is_empty())),
            "回退引擎应产出 Final(\"\")：{events:?}"
        );

        blink_domain_config::stt_config::update_cache(&saved);
    }

    /// 生产连接类型实现 WsConn 的编译期证据（不实际连接）。
    #[test]
    fn tungstenite_conn_type_check() {
        #[allow(dead_code)]
        fn probe(s: TungsteniteConn<WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>>) {
            fn require_ws_conn<S>(_: &TungsteniteConn<S>)
            where
                S: futures_util::Stream<Item = Result<Message, WsError>>
                    + futures_util::Sink<Message, Error = WsError>
                    + Send
                    + Unpin,
            {
            }
            require_ws_conn(&s);
        }
        let _ = probe;
    }

    /// 对真实服务的流式能力探测（手动回归工具）：
    /// `cargo test -p blink-domain-stt probe_real_service -- --ignored --nocapture`
    /// 依赖局域网 funasr 服务可达（192.168.31.77:38040）——默认跳过。
    #[tokio::test]
    #[ignore = "依赖局域网 funasr 服务可达"]
    async fn probe_real_service() {
        let saved = blink_domain_config::stt_config::get_stt_config();
        let mut cfg = blink_domain_config::stt_config::SttConfig::default();
        cfg.enabled = true;
        cfg.mode = blink_domain_config::stt_config::SttMode::Cloud;
        cfg.cloud_provider = Some(blink_domain_config::stt_config::SttCloudProvider {
            kind: "custom".into(),
            base_url: Some("http://192.168.31.77:38040/v1".into()),
            model_id: "sensevoice-small".into(),
        });
        blink_domain_config::stt_config::init_cache(cfg.clone());
        blink_domain_config::stt_config::update_cache(&cfg);

        let report = probe_streaming_support().await;
        println!("ws: {:?}", report.ws);
        println!("sse: {:?}", report.sse);
        assert!(report.ws.supported, "funasr 服务应支持 Realtime WS：{:?}", report.ws);
        assert!(report.sse.supported, "funasr 服务应支持 SSE 流式：{:?}", report.sse);

        blink_domain_config::stt_config::update_cache(&saved);
    }

    /// 对真实服务的完整流式会话 E2E（手动回归工具）：
    /// begin → 分块推送本机 %TEMP%/speech16k.wav → finish → 断言 Final 含预期文本。
    /// 依赖局域网 funasr 服务与本机测试音频（PowerShell TTS 生成）——默认跳过。
    #[tokio::test]
    #[ignore = "依赖局域网 funasr 服务与本机测试音频"]
    async fn full_session_against_real_service() {
        let audio_path = std::env::temp_dir().join("speech16k.wav");
        assert!(
            audio_path.exists(),
            "缺少测试音频 {}（先用 PowerShell TTS 生成 16k mono PCM16 WAV）",
            audio_path.display()
        );

        let saved = blink_domain_config::stt_config::get_stt_config();
        let mut cfg = blink_domain_config::stt_config::SttConfig::default();
        cfg.enabled = true;
        cfg.mode = blink_domain_config::stt_config::SttMode::Cloud;
        // kind=openai：自部署服务最常见的配置形态（走 Whisper HTTP 协议）——
        // 0.29.1 修复回归点：此前该 kind 会误判 OpenAI 增量方言导致预览拼接重叠。
        cfg.cloud_provider = Some(blink_domain_config::stt_config::SttCloudProvider {
            kind: "openai".into(),
            base_url: Some("http://192.168.31.77:38040/v1".into()),
            model_id: "sensevoice-small".into(),
        });
        blink_domain_config::stt_config::init_cache(cfg.clone());
        blink_domain_config::stt_config::update_cache(&cfg);

        // 解析 16k mono PCM16 WAV → f32 样本（找 data 块，不依赖外部库）。
        let bytes = std::fs::read(&audio_path).unwrap();
        let data_pos = bytes
            .windows(4)
            .position(|w| w == b"data")
            .expect("WAV 缺 data 块");
        let data_len =
            u32::from_le_bytes(bytes[data_pos + 4..data_pos + 8].try_into().unwrap()) as usize;
        let pcm = &bytes[data_pos + 8..data_pos + 8 + data_len];
        let samples: Vec<f32> = pcm
            .chunks_exact(2)
            .map(|c| i16::from_le_bytes([c[0], c[1]]) as f32 / 32768.0)
            .collect();

        let port = CloudStreamingPort::new(RecognitionProfile::PreviewDraft);
        let session_gen = port.begin_session().await.expect("真服务 WS 应连接成功");
        let mut rx = port.events();

        let mut previews = 0usize;
        let mut drafts = 0usize;
        let mut previews_overlapped = false;
        for chunk in samples.chunks(3_200) {
            port.push_audio(session_gen, chunk).await.unwrap();
            // 模拟真实说话节奏（200ms/块），期间收集事件。
            tokio::time::sleep(Duration::from_millis(30)).await;
            for event in drain(&mut rx) {
                match event {
                    SttEvent::Preview { text, .. } => {
                        previews += 1;
                        // 累计方言：预览不应是"前缀重复拼接"形态
                        let chars: Vec<char> = text.chars().collect();
                        let half = chars.len() / 2;
                        if half > 0 && chars[..half] == chars[half..2 * half] {
                            previews_overlapped = true;
                        }
                    }
                    SttEvent::Draft { .. } => drafts += 1,
                    _ => {}
                }
            }
        }
        port.finish_session(session_gen).await.unwrap();

        // 逐批等待 Final（事件分批到达，不能只看第一批）
        let mut final_text = None;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while final_text.is_none() && tokio::time::Instant::now() < deadline {
            for event in wait_events(&mut rx, 500).await {
                if let SttEvent::Final { text, .. } = event {
                    final_text = Some(text);
                }
            }
        }
        let final_text = final_text.expect("应有 Final 事件");
        println!(
            "previews={previews} drafts={drafts} final={final_text:?}"
        );
        assert!(previews > 0, "流式会话应产生实时预览：previews={previews}");
        assert!(
            !previews_overlapped,
            "预览文本出现重复前缀（增量误拼接特征）：{final_text:?}"
        );
        assert!(
            final_text.contains("今天天气不错"),
            "Final 应含预期文本：{final_text:?}"
        );

        blink_domain_config::stt_config::update_cache(&saved);
    }
}
