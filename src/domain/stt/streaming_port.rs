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
//! ## 并发安全
//!
//! 内部通过 `tokio::sync::Mutex` 串行化所有引擎调用，确保同一时刻只有一个操作。
//! `push_audio` 不阻塞调用方——音频采样在独立 task 中通过 channel 转发。

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::sync::{Mutex as TokioMutex, mpsc};

use super::{StreamingSttPort, SttEngine, SttError, SttEvent};

/// GGUF 引擎的 `StreamingSttPort` 适配器。
///
/// 包装任意 `SttEngine` 实现为 `StreamingSttPort`。
/// 适用于 PseudoStreamingSttEngine 和 LocalSttEngine。
pub struct GgufStreamingAdapter {
    /// 内部引擎
    engine: Arc<dyn SttEngine>,
    /// 事件 sender（短锁，不跨 await）
    event_tx: std::sync::Mutex<mpsc::UnboundedSender<SttEvent>>,
    /// generation 计数器
    generation: AtomicU64,
    /// 当前 active generation（None = 无活跃 session）
    active_gen: TokioMutex<Option<u64>>,
}

impl GgufStreamingAdapter {
    /// 创建适配器，包装一个 `SttEngine`。
    pub fn new(engine: Arc<dyn SttEngine>) -> Self {
        let (event_tx, _) = mpsc::unbounded_channel();
        Self {
            engine,
            event_tx: std::sync::Mutex::new(event_tx),
            generation: AtomicU64::new(0),
            active_gen: TokioMutex::new(None),
        }
    }

    /// 发送事件到当前 channel。
    fn emit(&self, event: SttEvent) {
        let tx = self.event_tx.lock().unwrap();
        let _ = tx.send(event);
    }

    /// 解析伪流式引擎返回的 JSON 字符串，构造 Partial 事件。
    ///
    /// 返回值：`true` = 产出了 Partial 事件，`false` = 无事件（空文本）。
    fn emit_partial_from_result(&self, generation: u64, text: &str) {
        if text.is_empty() {
            return;
        }

        // 尝试解析 JSON（伪流式引擎返回 confirmed + preview）
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(text) {
            let confirmed = v.get("confirmed").and_then(|t| t.as_str()).unwrap_or("");
            let preview = v.get("preview").and_then(|t| t.as_str()).unwrap_or("");
            if !confirmed.is_empty() || !preview.is_empty() {
                self.emit(SttEvent::Partial {
                    generation,
                    confirmed: confirmed.to_string(),
                    preview: preview.to_string(),
                });
            }
        } else {
            // 纯文本（非流式引擎的兼容路径——不应在 push_audio 中出现）
            if !text.is_empty() {
                self.emit(SttEvent::Partial {
                    generation,
                    confirmed: String::new(),
                    preview: text.to_string(),
                });
            }
        }
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

        let session_gen = self.generation.fetch_add(1, Ordering::AcqRel) + 1;
        *active = Some(session_gen);

        tracing::debug!(generation = session_gen, "GGUF session begin");
        Ok(session_gen)
    }

    async fn push_audio(&self, generation: u64, samples: &[f32]) -> Result<(), SttError> {
        // 检查 generation 匹配
        let active = self.active_gen.lock().await;
        if *active != Some(generation) {
            return Err(SttError::Engine(format!(
                "generation 不匹配: 期望 {generation:?}，当前 {active:?}"
            )));
        }
        drop(active);

        // 调用引擎的 transcribe_chunk
        // 伪流式引擎内部会 spawn 后台 HTTP task，这里只是触发预览检查
        match self.engine.transcribe_chunk(samples).await {
            Ok(text) => {
                self.emit_partial_from_result(generation, &text);
                Ok(())
            }
            Err(e) => {
                self.emit(SttEvent::Error {
                    generation,
                    message: e.to_string(),
                });
                Err(e)
            }
        }
    }

    async fn finish_session(&self, generation: u64) -> Result<(), SttError> {
        let mut active = self.active_gen.lock().await;
        if *active != Some(generation) {
            return Err(SttError::Engine(format!(
                "generation 不匹配: 期望 {generation:?}，当前 {active:?}"
            )));
        }

        // 调用 finalize
        match self.engine.finalize().await {
            Ok(text) => {
                self.emit(SttEvent::Final { generation, text });
            }
            Err(e) => {
                self.emit(SttEvent::Error {
                    generation,
                    message: e.to_string(),
                });
            }
        }

        *active = None;
        Ok(())
    }

    async fn cancel_session(&self, generation: u64) -> Result<(), SttError> {
        let mut active = self.active_gen.lock().await;
        // 幂等：不匹配时也返回 Ok
        if *active == Some(generation) {
            self.engine.reset();
            *active = None;
            tracing::debug!(generation, "GGUF session cancelled");
        }
        Ok(())
    }

    async fn reset(&self) -> Result<(), SttError> {
        self.engine.reset();
        *self.active_gen.lock().await = None;
        // 递增 generation 使任何在途事件失效
        self.generation.fetch_add(1, Ordering::AcqRel);
        Ok(())
    }

    fn supports_native_partial(&self) -> bool {
        false
    }

    fn events(&self) -> mpsc::UnboundedReceiver<SttEvent> {
        // 重建 channel 以获取新 receiver
        let (tx, rx) = mpsc::unbounded_channel();
        *self.event_tx.lock().unwrap() = tx;
        rx
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
        fn name(&self) -> &str {
            "mock"
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
            fn name(&self) -> &str {
                "failing"
            }
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
}
