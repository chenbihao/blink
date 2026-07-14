//! Mock STT 引擎:不调用真实模型,返回假文本。
//!
//! 用于 0.10.0 打通管线(hold → 录音 → STT → 注入)。
//! Mock 行为:
//! - `transcribe_chunk`: 按 elapsed 时间返回渐进式假文本(模拟流式)
//! - `finalize`: 返回完整假文本
//! - `reset`: 重置计时

use std::sync::Mutex;
use std::time::{Duration, Instant};

use super::{SttEngine, SttError};

/// Mock STT 引擎。
pub struct MockSttEngine {
    /// 录音开始时刻(reset 时设为 now)
    started_at: Mutex<Instant>,
}

impl MockSttEngine {
    pub fn new() -> Self {
        Self {
            started_at: Mutex::new(Instant::now()),
        }
    }

    fn elapsed(&self) -> Duration {
        self.started_at.lock().unwrap().elapsed()
    }
}

impl Default for MockSttEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl SttEngine for MockSttEngine {
    fn transcribe_chunk(&self, _samples: &[f32]) -> Result<String, SttError> {
        let elapsed = self.elapsed();
        let text = super::mock_text_for_elapsed(elapsed);
        Ok(text.to_string())
    }

    fn finalize(&self) -> Result<String, SttError> {
        let elapsed = self.elapsed();
        // finalize 时返回完整假文本
        let text = if elapsed < Duration::from_secs(2) {
            "你好"
        } else {
            "你好世界这是一段测试语音识别的文字结果"
        };
        tracing::debug!(?elapsed, text, "MockSttEngine::finalize");
        Ok(text.to_string())
    }

    fn reset(&self) {
        *self.started_at.lock().unwrap() = Instant::now();
        tracing::debug!("MockSttEngine::reset");
    }

    fn name(&self) -> &str {
        "mock"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mock_engine_returns_progressive_text() {
        let engine = MockSttEngine::new();
        engine.reset();

        // 立即调用 (< 2s) → 空文本
        let t0 = engine.transcribe_chunk(&[]).unwrap();
        assert!(t0.is_empty());

        // finalize → 有文本
        let final_text = engine.finalize().unwrap();
        assert!(!final_text.is_empty());
    }

    #[test]
    fn mock_engine_reset_restarts_timer() {
        let engine = MockSttEngine::new();
        let _ = engine.finalize().unwrap();

        engine.reset();
        let t = engine.transcribe_chunk(&[]).unwrap();
        // reset 后立即调用 → elapsed < 2s → 空文本
        assert!(t.is_empty());
    }

    #[test]
    fn mock_text_for_elapsed_progression() {
        assert_eq!(super::super::mock_text_for_elapsed(Duration::from_secs(0)), "");
        assert_eq!(super::super::mock_text_for_elapsed(Duration::from_secs(2)), "你好");
        assert_eq!(super::super::mock_text_for_elapsed(Duration::from_secs(3)), "你好世界");
        assert_eq!(
            super::super::mock_text_for_elapsed(Duration::from_secs(10)),
            "你好世界这是一段测试语音识别的文字结果"
        );
    }
}
