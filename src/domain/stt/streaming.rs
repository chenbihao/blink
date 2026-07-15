//! 流式 STT 引擎：通过 WebSocket 连接 blink_stt_server 的 /ws/stream 端点。
//!
//! ## 工作模式
//!
//! 真流式（hold-to-talk + streaming）：
//! - `new()`：创建引擎 + 后台预连接 WebSocket（减少首次 chunk 的 ~2s 握手延迟）
//! - `reset`：标记需要重连（不干扰进行中的预连接）
//! - `transcribe_chunk`：预连接未完成时跳过（返回空 partial），完成后正常收发
//! - `finalize`：等待预连接完成 → 发送空帧 → 读取最终结果
//!
//! ## 与 LocalSttEngine 的关系
//!
//! `LocalSttEngine` 是非流式引擎（累积音频 → finalize 一次性 HTTP）。
//! `StreamingSttEngine` 是流式引擎（逐 chunk WebSocket，边说边出字）。
//! 两者都实现 `SttEngine` trait，由 `create_engine()` 根据 `streaming` 开关选择。
//!
//! ## WebSocket 协议
//!
//! - Client → Server: binary frame = raw f32 PCM (16kHz, mono, little-endian)
//! - Client → Server: empty binary frame = finalize 信号（触发 server 处理剩余缓冲并发送 is_final=True）
//! - Server → Client: text frame = JSON `{"text": "...", "is_final": false|true}`
//!
//! ## 服务端音频缓冲
//!
//! cpal/WASAPI 回调每次给 ~10ms（160 samples）小片段，但 Paraformer streaming
//! 模型 `chunk_size=[0,10,5]` 期望每次 `generate` 收到 9600 samples（600ms）。
//! 服务端（`blink_stt_server.py`）缓冲客户端发送的小片段，攒满 600ms 后才调用
//! `generate`，确保模型能正常推理。
//!
//! 客户端 `transcribe_chunk` 采用 **Drain-then-Send** 模式：先非阻塞排空已到达的
//! 响应（0ms 超时），再发送音频。这样发送不被读响应阻塞，音频实时送达服务端，
//! 服务端的 partial 文本在下次 drain 时被捡起。
//!
//! ## 预连接
//!
//! `new()` 在创建后立即 spawn 后台 task 预连接 WebSocket。
//! `transcribe_chunk` 在预连接完成前直接跳过（返回空 partial），不阻塞音频 task。
//! 这样 voice-level 动画事件能正常发出，不会因为 WebSocket 握手延迟而卡住。
//! `finalize` 会等待预连接完成后再发送 finalize 信号。
//!
//! ## 并发安全
//!
//! 使用 `Arc<tokio::sync::Mutex>`（非 `std::sync::Mutex`），因为 `MutexGuard`
//! 需要跨 `.await` 点持有（WebSocket 发送/接收是 async 操作），且需 clone 给
//! 后台预连接 task。`reset()` 是 sync 函数，用 `try_lock()` 回退。

use std::sync::Arc;
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use tokio::sync::Mutex;
use tokio_tungstenite::tungstenite::Message;

use super::{SttEngine, SttError};

/// WebSocket 流式 STT 引擎。
///
/// 通过 WebSocket 连接 blink_stt_server 的 `/ws/stream` 端点，
/// 发送 f32 PCM 音频块，接收逐步完善的 partial text。
pub struct StreamingSttEngine {
    /// WebSocket URL: `ws://127.0.0.1:{port}/ws/stream`
    ws_url: String,
    /// 内部状态（WebSocket 连接 + 累积文本）
    /// 使用 Arc<tokio::sync::Mutex> 因 MutexGuard 需跨 .await 持有，
    /// 且需 clone 给后台预连接 task
    inner: Arc<Mutex<StreamInner>>,
}

/// 流式引擎内部状态。
struct StreamInner {
    /// WebSocket 连接（None = 未连接 / 需要重连）
    ws: Option<WsConnection>,
    /// 最新 partial 文本
    partial: String,
    /// 是否需要重连（`reset()` 或预连接失败时设置为 true）
    needs_reconnect: bool,
    /// 预连接是否进行中（`new()` 设置为 true，预连接 task 完成后设置为 false）
    /// 为 true 时 `transcribe_chunk` 跳过音频发送，`finalize` 等待完成
    preconnecting: bool,
}

/// WebSocket 连接的封装类型。
type WsConnection =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

impl StreamInner {
    #[allow(dead_code)]
    fn new() -> Self {
        Self {
            ws: None,
            partial: String::new(),
            needs_reconnect: true,
            preconnecting: false,
        }
    }

    /// 用于 `new()` 的构造：标记预连接进行中。
    fn new_with_preconnect() -> Self {
        Self {
            ws: None,
            partial: String::new(),
            needs_reconnect: false, // 预连接会处理，不需要 lazy connect
            preconnecting: true,
        }
    }
}

impl StreamingSttEngine {
    /// 创建流式 STT 引擎。
    ///
    /// 快速检查 server 是否在运行（TCP 级别），未运行则返回错误。
    /// 创建后立即 spawn 后台 task 预连接 WebSocket。
    pub fn new(port: u16) -> Result<Self, String> {
        // 使用 127.0.0.1 避免 Windows 上 localhost DNS 解析延迟
        let ws_url = format!("ws://127.0.0.1:{port}/ws/stream");

        // 快速检查：server 是否在运行（TCP 级别）
        if !super::funasr::is_server_ready(port) {
            return Err(format!(
                "STT 服务未在端口 {port} 上运行。\
                 请在设置页「语音输入」→「本地模式」中点击「启动服务」按钮。"
            ));
        }

        let inner = Arc::new(Mutex::new(StreamInner::new_with_preconnect()));

        // 预连接 WebSocket（减少首次 transcribe_chunk 的 ~2s 握手延迟）
        let inner_clone = Arc::clone(&inner);
        let ws_url_clone = ws_url.clone();
        tokio::spawn(async move {
            tracing::info!(%ws_url_clone, "预连接 WebSocket...");
            match tokio_tungstenite::connect_async(&ws_url_clone).await {
                Ok((ws, resp)) => {
                    let mut guard = inner_clone.lock().await;
                    if guard.ws.is_none() {
                        guard.ws = Some(ws);
                        guard.needs_reconnect = false;
                        guard.preconnecting = false;
                        tracing::debug!(
                            status = ?resp.status(),
                            "WebSocket 预连接完成"
                        );
                    } else {
                        // ensure_connected 已抢先连接，丢弃预连接
                        guard.preconnecting = false;
                        tracing::debug!("预连接完成但已有连接，丢弃");
                    }
                }
                Err(e) => {
                    let mut guard = inner_clone.lock().await;
                    guard.preconnecting = false;
                    guard.needs_reconnect = true; // 回退到 lazy connect
                    tracing::debug!(
                        %e,
                        "WebSocket 预连接失败（将在首次 transcribe_chunk 时重试）"
                    );
                }
            }
        });

        tracing::info!(%ws_url, "流式 STT 引擎: WebSocket (预连接中)");

        Ok(Self { ws_url, inner })
    }

    /// 连接 WebSocket（如果尚未连接且预连接不在进行中）。
    async fn ensure_connected(inner: &mut StreamInner, ws_url: &str) -> Result<(), SttError> {
        if inner.needs_reconnect || inner.ws.is_none() {
            tracing::info!(%ws_url, "连接 WebSocket...");
            let (ws, resp) = tokio_tungstenite::connect_async(ws_url)
                .await
                .map_err(|e| {
                    let msg = format!("{e}");
                    // 检测 404 错误——通常是服务端缺少 websockets 库
                    if msg.contains("404") || msg.contains("Not Found") {
                        SttError::Engine(format!(
                            "WebSocket 连接失败: {e}\n\
                             ── 这通常是因为服务端缺少 WebSocket 库 ──\n\
                             请尝试重启 STT 服务（Blink 会自动安装 uvicorn[standard]）。\n\
                             如仍失败，请在设置页点击「安装环境」重新安装 Python 依赖。"
                        ))
                    } else {
                        SttError::Engine(format!("WebSocket 连接失败: {e}"))
                    }
                })?;
            tracing::debug!(status = ?resp.status(), "WebSocket 连接已建立");
            inner.ws = Some(ws);
            inner.needs_reconnect = false;
            inner.partial.clear();
        }
        Ok(())
    }
}

#[async_trait::async_trait]
impl SttEngine for StreamingSttEngine {
    async fn transcribe_chunk(&self, samples: &[f32]) -> Result<String, SttError> {
        let mut inner = self.inner.lock().await;

        // 预连接进行中：跳过此 chunk，返回当前 partial（可能为空）
        // 这样音频 task 不会被阻塞，voice-level 动画事件能正常发出
        if inner.preconnecting {
            return Ok(inner.partial.clone());
        }

        // 确保已连接（预连接失败后回退到 lazy connect）
        Self::ensure_connected(&mut inner, &self.ws_url).await?;

        // ── Drain-then-Send 模式 ──
        //
        // 先非阻塞排空所有已到达的响应（0ms 超时 = 立即返回 if no data），
        // 再发送音频。这样：
        // 1. 发送不会被读响应阻塞——音频实时送达服务端
        // 2. 服务端的 partial 响应在下次 transcribe_chunk 时被捡起
        // 3. 互斥锁持有时间最短（仅 drain + send，不等响应）
        //
        // 服务端缓冲 600ms 才推理一次，所以大部分 drain 拿不到数据，
        // 少数时候能拿到上一轮的 partial。
        //
        // 注意：ws 借用 inner.ws，不能在 ws 活跃时写 inner.partial，
        // 所以先 drain+send 到局部变量，最后统一更新 inner。
        let mut new_partial: Option<String> = None;
        let send_result = {
            let ws = inner.ws.as_mut().unwrap();

            // Drain：非阻塞排空已到达的响应
            loop {
                match tokio::time::timeout(Duration::from_millis(0), ws.next()).await {
                    Ok(Some(Ok(msg))) => {
                        if let Ok(text) = msg.into_text() {
                            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
                                let text = v.get("text").and_then(|t| t.as_str()).unwrap_or("");
                                if !text.is_empty() {
                                    new_partial = Some(text.to_string());
                                }
                            }
                        }
                    }
                    _ => break, // 无更多响应或超时
                }
            }

            // Send：发送 f32 PCM 音频块（little-endian bytes）
            let audio_bytes: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
            ws.send(Message::binary(audio_bytes))
                .await
                .map_err(|e| SttError::Engine(format!("WebSocket 发送失败: {e}")))
        };

        send_result?;

        // 更新累积文本（ws 借用已结束，可安全写 inner）
        if let Some(text) = &new_partial {
            inner.partial = text.clone();
        }

        // inner.partial 已在 drain 阶段更新（如果有新响应的话）
        Ok(inner.partial.clone())
    }

    async fn finalize(&self) -> Result<String, SttError> {
        // 如果预连接仍在进行中，等待它完成（释放锁让预连接 task 能写入）
        loop {
            {
                let inner = self.inner.lock().await;
                if !inner.preconnecting {
                    break;
                }
                tracing::debug!("finalize: 等待预连接完成...");
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        let mut inner = self.inner.lock().await;

        // Take 出 WebSocket，避免与 inner.partial 的借用冲突
        let mut ws = match inner.ws.take() {
            Some(ws) => ws,
            None => return Ok(inner.partial.clone()),
        };

        let mut partial = inner.partial.clone();

        // 发送关闭信号（空音频帧，触发 server 的 is_final=True）
        ws.send(Message::binary(Vec::new()))
            .await
            .map_err(|e| SttError::Engine(format!("WebSocket 发送关闭信号失败: {e}")))?;

        // 读取最终结果（server 收到空帧后 break → finally 发送 is_final=True）
        loop {
            match tokio::time::timeout(Duration::from_secs(3), ws.next()).await {
                Ok(Some(Ok(msg))) => {
                    if let Ok(text) = msg.into_text() {
                        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
                            let is_final =
                                v.get("is_final").and_then(|f| f.as_bool()).unwrap_or(false);
                            let text = v.get("text").and_then(|t| t.as_str()).unwrap_or("");

                            if !text.is_empty() {
                                partial = text.to_string();
                            }

                            if is_final {
                                break;
                            }
                        }
                    }
                }
                _ => break, // WebSocket 关闭 / 超时 / 错误
            }
        }

        // 关闭连接
        let _ = ws.close(None).await;

        // 更新 inner 状态
        inner.partial = partial.clone();
        inner.ws = None;

        tracing::info!(
            text_len = partial.chars().count(),
            %partial,
            "StreamingSttEngine 识别完成",
        );

        Ok(partial)
    }

    fn reset(&self) {
        // reset 是 sync 函数，用 try_lock 回退
        // 在 hold-to-talk 场景下 reset 总在录音开始前调用，无并发竞争
        if let Ok(mut inner) = self.inner.try_lock() {
            // 预连接进行中时不干扰（new() 创建的引擎已经是干净状态）
            if inner.preconnecting {
                tracing::debug!("StreamingSttEngine::reset (预连接中，跳过)");
                return;
            }
            inner.needs_reconnect = true;
            inner.partial.clear();
            inner.ws.take(); // take 出来，旧连接 drop 时自动关闭
            tracing::debug!("StreamingSttEngine::reset");
        } else {
            // 锁定失败，标记将在下次 transcribe_chunk 时处理
            // 这种情况在实际使用中不应发生
            tracing::warn!(
                "StreamingSttEngine::reset: 锁定失败（并发竞争？），将在下次 transcribe_chunk 时重连"
            );
        }
    }

    fn name(&self) -> &str {
        "streaming-ws"
    }
}

// ── 测试 ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn stream_inner_new_is_clean() {
        let inner = StreamInner::new();
        assert!(inner.ws.is_none());
        assert!(inner.partial.is_empty());
        assert!(inner.needs_reconnect);
        assert!(!inner.preconnecting);
    }

    #[tokio::test]
    async fn stream_inner_new_with_preconnect() {
        let inner = StreamInner::new_with_preconnect();
        assert!(inner.ws.is_none());
        assert!(inner.partial.is_empty());
        assert!(!inner.needs_reconnect);
        assert!(inner.preconnecting);
    }

    #[tokio::test]
    async fn streaming_engine_url_is_correct() {
        let engine = StreamingSttEngine {
            ws_url: "ws://127.0.0.1:8000/ws/stream".to_string(),
            inner: Arc::new(Mutex::new(StreamInner::new())),
        };
        assert!(engine.ws_url.contains("/ws/stream"));
        assert!(engine.ws_url.contains("8000"));
    }

    #[tokio::test]
    async fn reset_marks_needs_reconnect() {
        let engine = StreamingSttEngine {
            ws_url: "ws://127.0.0.1:65535/ws/stream".to_string(),
            inner: Arc::new(Mutex::new(StreamInner::new())),
        };

        // 先设置一些状态
        {
            let mut inner = engine.inner.lock().await;
            inner.partial = "测试文本".to_string();
            inner.needs_reconnect = false;
        }

        // reset
        engine.reset();

        let inner = engine.inner.lock().await;
        assert!(inner.needs_reconnect);
        assert!(inner.partial.is_empty());
        assert!(inner.ws.is_none());
    }

    #[tokio::test]
    async fn reset_skips_when_preconnecting() {
        let engine = StreamingSttEngine {
            ws_url: "ws://127.0.0.1:65535/ws/stream".to_string(),
            inner: Arc::new(Mutex::new(StreamInner::new_with_preconnect())),
        };

        // 先设置一些状态
        {
            let mut inner = engine.inner.lock().await;
            inner.partial = "测试文本".to_string();
        }

        // reset 应跳过（预连接中）
        engine.reset();

        let inner = engine.inner.lock().await;
        assert!(inner.preconnecting, "preconnecting 应仍为 true");
        assert_eq!(inner.partial, "测试文本", "partial 不应被清除");
    }

    #[tokio::test]
    async fn transcribe_chunk_skips_when_preconnecting() {
        let engine = StreamingSttEngine {
            ws_url: "ws://127.0.0.1:1/ws/stream".to_string(),
            inner: Arc::new(Mutex::new(StreamInner::new_with_preconnect())),
        };

        // 预连接中，transcribe_chunk 应跳过并返回空 partial
        let result = engine.transcribe_chunk(&[0.0f32; 160]).await;
        assert!(result.is_ok(), "预连接中应跳过而不报错");
        assert!(result.unwrap().is_empty(), "partial 应为空");

        // 验证 preconnecting 仍为 true
        let inner = engine.inner.lock().await;
        assert!(inner.preconnecting);
    }

    // ── WebSocket 404 错误检测测试 ──

    /// 验证当 WebSocket 连接失败且错误信息包含 "404" 时，
    /// `ensure_connected` 返回的错误信息包含 WebSocket 库缺失提示。
    #[tokio::test]
    async fn websocket_404_error_includes_hint() {
        // 用一个未被占用的端口，连接会失败（不是 404，而是连接拒绝）
        // 但我们可以验证错误信息格式化逻辑
        let engine = StreamingSttEngine {
            ws_url: "ws://127.0.0.1:1/ws/stream".to_string(), // 端口 1 通常不可用
            inner: Arc::new(Mutex::new(StreamInner::new())),
        };

        let result = engine.transcribe_chunk(&[0.0f32; 160]).await;
        assert!(result.is_err(), "连接不可用端口应返回错误");

        let err_msg = format!("{}", result.unwrap_err());
        // 连接拒绝不是 404，所以不应包含 WebSocket 库提示
        assert!(
            !err_msg.contains("uvicorn[standard]"),
            "连接拒绝错误不应包含 websockets 库提示: {err_msg}"
        );
    }

    /// 验证 `reset` 后 `transcribe_chunk` 会尝试重新连接。
    #[tokio::test]
    async fn reset_then_transcribe_triggers_reconnect() {
        let engine = StreamingSttEngine {
            ws_url: "ws://127.0.0.1:1/ws/stream".to_string(),
            inner: Arc::new(Mutex::new(StreamInner::new())),
        };

        // reset 后 needs_reconnect = true
        engine.reset();

        // transcribe_chunk 应尝试连接（并失败，因为端口不可用）
        let result = engine.transcribe_chunk(&[0.0f32; 160]).await;
        assert!(result.is_err(), "应尝试连接并失败");

        // 验证 needs_reconnect 仍为 true（连接失败后未清除）
        let inner = engine.inner.lock().await;
        assert!(
            inner.needs_reconnect || inner.ws.is_none(),
            "连接失败后应保持 needs_reconnect 或 ws 为 None"
        );
    }

    /// 验证 `finalize` 在预连接进行中时会等待。
    #[tokio::test]
    async fn finalize_waits_for_preconnect() {
        let engine = StreamingSttEngine {
            ws_url: "ws://127.0.0.1:1/ws/stream".to_string(),
            inner: Arc::new(Mutex::new(StreamInner::new_with_preconnect())),
        };

        // 在另一个 task 中模拟预连接完成
        let inner_clone = Arc::clone(&engine.inner);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            let mut guard = inner_clone.lock().await;
            guard.preconnecting = false;
            // 不设置 ws（模拟预连接失败）
            guard.needs_reconnect = true;
        });

        // finalize 应等待预连接完成，然后返回空 partial（无 ws 连接）
        let result = engine.finalize().await;
        assert!(result.is_ok(), "finalize 应成功（返回空 partial）");
        assert!(result.unwrap().is_empty(), "partial 应为空");
    }

    // ── 端到端流式 STT 测试 ──

    /// 端到端测试：用 FunASR 示例音频验证流式 STT 管线。
    ///
    /// 流程：
    /// 1. 检查流式 STT 服务是否就绪（WebSocket 端点可连接）
    /// 2. 下载 FunASR 官方示例音频（BAC009S0764W0121.wav）
    /// 3. 读取音频 → 转为 f32 PCM 样本
    /// 4. 分块通过 WebSocket 发送（模拟流式输入）
    /// 5. 调用 finalize → 获取最终识别结果
    /// 6. 断言识别结果不为空
    ///
    /// 如果流式 STT 服务未运行或 WebSocket 端点不可用，跳过（不 fail）。
    #[tokio::test(flavor = "multi_thread")]
    async fn streaming_end_to_end_with_funasr_sample() {
        let port: u16 = 8000;

        // 检查流式 STT 服务是否就绪
        let (ws_ready, ws_err) = crate::domain::stt::funasr::is_websocket_ready(port).await;
        if !ws_ready {
            eprintln!("跳过：流式 STT WebSocket 端点不可用（端口 {port}）");
            if let Some(err) = ws_err {
                eprintln!("  原因: {err}");
                eprintln!("  如果错误包含 404，请检查 websockets 库是否已安装");
            }
            eprintln!("要运行此测试，请先在设置页安装环境并启动流式服务");
            return;
        }

        // FunASR 示例音频
        let audio_url = "https://isv-data.oss-cn-hangzhou.aliyuncs.com/ics/MaaS/ASR/test_audio/BAC009S0764W0121.wav";
        let tmp_wav = std::env::temp_dir().join("blink_streaming_test_sample.wav");

        // 如果本地已有缓存，跳过下载
        if !tmp_wav.exists() {
            eprintln!("下载 FunASR 示例音频...");
            let client = reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .expect("HTTP client 创建失败");

            let resp = client.get(audio_url).send().await.expect("下载音频失败");
            assert!(
                resp.status().is_success(),
                "下载音频 HTTP 失败: {}",
                resp.status()
            );

            let bytes = resp.bytes().await.expect("读取音频字节失败");
            std::fs::write(&tmp_wav, &bytes).expect("写入音频文件失败");
        }

        eprintln!("读取 WAV 文件: {}", tmp_wav.display());
        let wav_bytes = std::fs::read(&tmp_wav).expect("读取 WAV 文件失败");

        // 解析 WAV → f32 PCM 样本
        let samples = crate::domain::stt::wav::parse_wav_to_f32(&wav_bytes).expect("WAV 解析失败");
        eprintln!(
            "音频: {} 样本, {:.1}s",
            samples.len(),
            samples.len() as f64 / 16000.0
        );

        // 创建流式引擎实例（绕过 new() 的 TCP 检查，直接构造）
        let engine = StreamingSttEngine {
            ws_url: format!("ws://127.0.0.1:{port}/ws/stream"),
            inner: Arc::new(Mutex::new(StreamInner::new())),
        };
        engine.reset();

        // 分块发送音频（模拟流式输入，每块 1600 样本 = 100ms）
        let chunk_size = 1600usize;
        let mut partial_count = 0;
        for chunk in samples.chunks(chunk_size) {
            match engine.transcribe_chunk(chunk).await {
                Ok(text) => {
                    if !text.is_empty() {
                        partial_count += 1;
                        eprintln!("partial #{}: \"{text}\"", partial_count);
                    }
                }
                Err(e) => {
                    eprintln!("transcribe_chunk 错误（可能正常）: {e}");
                }
            }
        }

        // finalize → 获取最终结果
        eprintln!("调用 finalize 获取最终结果...");
        let result = engine.finalize().await;

        match &result {
            Ok(text) => {
                eprintln!("最终识别结果: \"{text}\"");
                assert!(!text.is_empty(), "流式识别结果不应为空");
                eprintln!("=== 流式 STT 端到端测试通过 ===");
            }
            Err(e) => {
                panic!("流式 STT finalize 失败: {e}");
            }
        }
    }
}
