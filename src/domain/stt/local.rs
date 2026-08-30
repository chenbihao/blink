//! 本地 STT 引擎：GGUF 常驻 worker 转录（0.22.7.4 起）。
//!
//! ## 设计
//!
//! 传输通道由 app 层注入（`SttEngineConnection.transport`，GGUF worker 的
//! NDJSON client 适配）。本引擎只做音频累积与一次性 finalize——
//! 生命周期由 `EngineManager` 管理，通道就绪由 start 时的 ready 握手保证。
//!
//! ### 工作模式
//!
//! **非流式（hold-to-talk）**：
//! - `transcribe_chunk`: 累积 PCM 样本，返回空字符串
//! - `finalize`: WAV → transport.transcribe → 返回文本
//!
//! ## 历史对照
//!
//! 0.10–0.22.6：FunASR Python/PyTorch server（OpenAI 兼容 HTTP）。
//! 0.22.7.4：旧链路删除，本地 STT 仅保留 GGUF worker 通道。

use std::sync::Mutex;

use super::{SttEngine, SttError};

/// 本地 STT 引擎（GGUF 常驻 worker）。
///
/// 通过连接快照中的 worker transport 做语音转文字。
/// worker 进程的生命周期由 `EngineManager` 统一管理。
pub struct LocalSttEngine {
    /// 累积的 PCM 样本（f32, 16kHz, mono）
    samples: Mutex<Vec<f32>>,
    /// 采样率
    sample_rate: u32,
    /// 连接快照（transport 必须存在——stdio worker 是唯一本地实现）
    connection: Option<crate::domain::stt::SttEngineConnection>,
}

impl LocalSttEngine {
    /// 从 `SttEngineConnection` 创建本地 STT 引擎。
    ///
    /// 连接快照必须携带 worker transport（GGUF 常驻 worker 是唯一本地实现；
    /// 无 transport 的连接是上游接线错误，明确报错）。
    pub fn from_connection(
        config: &crate::domain::config::stt_config::SttConfig,
        conn: crate::domain::stt::SttEngineConnection,
    ) -> Result<Self, String> {
        if conn.transport.is_none() {
            return Err(
                "本地 STT 连接缺少 worker 通道（GGUF worker 是唯一本地实现）。\
                 请确认语音服务已在设置页启动。"
                    .to_string(),
            );
        }
        let model = config.local_engine.funasr_model.clone();
        tracing::info!(model = %model, "本地 STT 引擎: GGUF worker (就绪)");

        Ok(Self {
            samples: Mutex::new(Vec::new()),
            sample_rate: 16000,
            connection: Some(conn),
        })
    }

    /// 通过 worker transport 转录（channel 与模型就绪由实现承载）。
    async fn transcribe_via_worker(&self, wav_bytes: &[u8]) -> Result<String, String> {
        let conn = self
            .connection
            .as_ref()
            .ok_or_else(|| "本地引擎无连接快照".to_string())?;
        let transport = conn
            .transport
            .as_ref()
            .ok_or_else(|| "本地引擎连接缺少 worker 通道".to_string())?;
        transport.transcribe(wav_bytes).await
    }
}

#[async_trait::async_trait]
impl SttEngine for LocalSttEngine {
    async fn transcribe_chunk(&self, samples: &[f32]) -> Result<String, SttError> {
        // 非流式模式：只累积，不返回 partial
        self.samples.lock().unwrap().extend_from_slice(samples);
        Ok(String::new())
    }

    async fn finalize(&self) -> Result<String, SttError> {
        let samples = self.samples.lock().unwrap().clone();

        if samples.is_empty() {
            return Ok(String::new());
        }

        let duration_ms = (samples.len() as f64 / self.sample_rate as f64 * 1000.0) as u64;
        tracing::debug!(
            samples = samples.len(),
            duration_ms,
            "LocalSttEngine::finalize 开始识别",
        );

        // PCM → WAV → worker NDJSON 通道
        let wav_bytes = super::wav::pcm_to_wav(&samples, self.sample_rate, 1);
        let text = self
            .transcribe_via_worker(&wav_bytes)
            .await
            .map_err(SttError::Engine)?;

        tracing::info!(
            text_len = text.chars().count(),
            %text,
            "LocalSttEngine 识别完成",
        );

        Ok(text)
    }

    fn reset(&self) {
        self.samples.lock().unwrap().clear();
        tracing::debug!("LocalSttEngine::reset");
    }

    fn name(&self) -> &str {
        "local-funasr-gguf"
    }
}

// ── 测试 ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// 无 transport 的连接必须被拒绝（明确接线错误，不静默降级）。
    #[tokio::test]
    async fn reject_connection_without_transport() {
        let config = crate::domain::config::stt_config::SttConfig::default();
        let conn = crate::domain::stt::SttEngineConnection {
            host: "127.0.0.1".to_string(),
            port: 0,
            engine_id: "funasr".to_string(),
            instance_id: "i".to_string(),
            transport: None,
        };
        let err = match LocalSttEngine::from_connection(&config, conn) {
            Err(e) => e,
            Ok(_) => panic!("无 transport 的连接应被拒绝"),
        };
        assert!(err.contains("worker 通道"), "错误应说明缺少通道: {err}");
    }

    /// 样本累积 + reset 语义（不依赖通道）。
    #[tokio::test]
    async fn stt_engine_accumulates_samples() {
        // 用 mock transport 构造合法引擎
        struct NoopTransport;
        #[async_trait::async_trait]
        impl crate::domain::stt::SttTransport for NoopTransport {
            async fn check_ready(&self) -> Result<(), String> {
                Ok(())
            }
            async fn transcribe(&self, _wav: &[u8]) -> Result<String, String> {
                Ok("mock".to_string())
            }
        }
        let config = crate::domain::config::stt_config::SttConfig::default();
        let conn = crate::domain::stt::SttEngineConnection {
            host: "127.0.0.1".to_string(),
            port: 0,
            engine_id: "funasr".to_string(),
            instance_id: "i".to_string(),
            transport: Some(std::sync::Arc::new(NoopTransport)),
        };
        let engine = LocalSttEngine::from_connection(&config, conn).unwrap();

        engine.transcribe_chunk(&[0.1, 0.2, 0.3]).await.unwrap();
        engine.transcribe_chunk(&[0.4, 0.5]).await.unwrap();

        let samples = engine.samples.lock().unwrap();
        assert_eq!(samples.len(), 5);
        assert!((samples[0] - 0.1).abs() < 1e-6);
        assert!((samples[4] - 0.5).abs() < 1e-6);

        drop(samples);
        engine.reset();
        assert!(engine.samples.lock().unwrap().is_empty());
    }
}
