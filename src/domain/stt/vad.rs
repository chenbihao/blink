//! 能量 VAD（Voice Activity Detection）——纯 Rust 实现。
//!
//! ## 用途
//!
//! 0.10.4 伪流式引擎用此模块检测用户停顿（句尾），
//! 在停顿时触发"定稿识别"，实现"句尾即出字"的体感。
//!
//! ## 原理
//!
//! 不需要 fsmn-vad 等神经网络 VAD——我们不需要精确的语音端点检测，
//! 只需要知道"用户停顿了"。基于 RMS 能量 + 静默时长即可：
//!
//! 1. 每个 audio chunk 计算 RMS 能量
//! 2. RMS < `silence_threshold` → 累加静默样本
//! 3. 静默持续 ≥ `min_silence_ms` 且之前在说话 → 触发 `SentenceEnd`
//! 4. 最小句子长度保护：< `min_sentence_ms` 的声音不切句
//!
//! ## 单测友好
//!
//! `process_chunk` 是纯函数（接收 `&[f32]`，返回 `VadEvent`），
//! 无 IO 依赖，可完全单元测试。

/// VAD 事件。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VadEvent {
    /// 无事件。
    None,
    /// 检测到句尾（静默超过阈值，且之前在说话）。
    SentenceEnd,
}

/// 能量 VAD：检测用户停顿，用于切句。
///
/// 基于 RMS 能量 + 静默时长判定。
/// 每个 ~10ms 音频 chunk 调用 [`process_chunk`](Self::process_chunk)。
pub struct EnergyVad {
    /// RMS 低于此值视为静默（默认 0.005，约 -46dB）
    silence_threshold: f64,
    /// 静默持续多久判定句尾（默认 300ms）
    min_silence_ms: u32,
    /// 最小句子长度：短于此值不切句（默认 800ms）
    /// 避免咳嗽、短暂噪声等触发误切
    min_sentence_ms: u32,
    /// 采样率
    sample_rate: u32,
    // ── 运行时状态 ──
    /// 当前已累积的静默样本数
    silence_samples: usize,
    /// 是否正在说话（有声阶段）
    speaking: bool,
    /// 当前句子已累积的样本数（用于最小句子长度保护）
    sentence_samples: usize,
}

impl EnergyVad {
    /// 创建默认配置的能量 VAD。
    ///
    /// 参数：
    /// - `sample_rate`：音频采样率（通常 16000）
    #[allow(dead_code)]
    pub fn new(sample_rate: u32) -> Self {
        Self {
            silence_threshold: 0.005,
            min_silence_ms: 300,
            min_sentence_ms: 800,
            sample_rate,
            silence_samples: 0,
            speaking: false,
            sentence_samples: 0,
        }
    }

    /// 创建带自定义参数的能量 VAD。
    pub fn with_params(
        sample_rate: u32,
        silence_threshold: f64,
        min_silence_ms: u32,
        min_sentence_ms: u32,
    ) -> Self {
        Self {
            silence_threshold,
            min_silence_ms,
            min_sentence_ms,
            sample_rate,
            silence_samples: 0,
            speaking: false,
            sentence_samples: 0,
        }
    }

    /// 处理一个音频 chunk，返回是否检测到句尾。
    ///
    /// 调用频率：cpal 回调约每 10ms 一次（160 samples @ 16kHz）。
    pub fn process_chunk(&mut self, samples: &[f32]) -> VadEvent {
        let rms = compute_rms(samples);
        let chunk_samples = samples.len();
        let min_silence_samples =
            (self.min_silence_ms as u64 * self.sample_rate as u64 / 1000) as usize;
        let min_sentence_samples =
            (self.min_sentence_ms as u64 * self.sample_rate as u64 / 1000) as usize;

        if rms < self.silence_threshold {
            // ── 静默 ──
            self.silence_samples += chunk_samples;

            if self.speaking {
                // 正在说话中遇到静默——检查是否够长
                if self.silence_samples >= min_silence_samples {
                    // 静默够长——检查句子是否够长
                    if self.sentence_samples >= min_sentence_samples {
                        self.speaking = false;
                        // 保留 sentence_samples 以便上层取出本句音频范围
                        return VadEvent::SentenceEnd;
                    } else {
                        // 句子太短，不切——重置为静默等待状态
                        self.speaking = false;
                        self.sentence_samples = 0;
                    }
                }
            }
            // 非说话状态继续累积静默，不触发事件
        } else {
            // ── 有声 ──
            if !self.speaking {
                // 从静默→有声，开始新句子
                self.speaking = true;
                self.sentence_samples = 0;
            }
            self.silence_samples = 0;
            self.sentence_samples += chunk_samples;
        }

        VadEvent::None
    }

    /// 句尾事件后，重置句子计数器（准备下一句）。
    ///
    /// 上层在收到 `SentenceEnd` 并取出本句音频范围后调用。
    pub fn reset_sentence(&mut self) {
        self.sentence_samples = 0;
        self.silence_samples = 0;
    }

    /// 是否正在说话（有声阶段）。
    #[allow(dead_code)]
    pub fn is_speaking(&self) -> bool {
        self.speaking
    }

    /// 当前句子的样本数（用于最小句子长度判断）。
    #[allow(dead_code)]
    pub fn sentence_samples(&self) -> usize {
        self.sentence_samples
    }

    /// 完全重置状态（新录音会话）。
    pub fn reset(&mut self) {
        self.silence_samples = 0;
        self.speaking = false;
        self.sentence_samples = 0;
    }
}

/// 计算音频样本的 RMS 能量。
fn compute_rms(samples: &[f32]) -> f64 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum_sq: f64 = samples.iter().map(|s| (*s as f64) * (*s as f64)).sum();
    (sum_sq / samples.len() as f64).sqrt()
}

// ── 测试 ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_RATE: u32 = 16000;

    /// 生成指定时长、指定振幅的正弦波样本（模拟语音）。
    fn generate_tone(duration_ms: u32, amplitude: f32) -> Vec<f32> {
        let n = (duration_ms as u64 * SAMPLE_RATE as u64 / 1000) as usize;
        (0..n)
            .map(|i| {
                let t = i as f32 / SAMPLE_RATE as f32;
                (2.0 * std::f32::consts::PI * 440.0 * t).sin() * amplitude
            })
            .collect()
    }

    /// 生成指定时长的静音样本。
    fn generate_silence(duration_ms: u32) -> Vec<f32> {
        let n = (duration_ms as u64 * SAMPLE_RATE as u64 / 1000) as usize;
        vec![0.0; n]
    }

    #[test]
    fn vad_silence_detection_no_sentence_end() {
        let mut vad = EnergyVad::new(SAMPLE_RATE);
        let silence = generate_silence(1000); // 1s 纯静默

        let mut events = 0;
        for chunk in silence.chunks(160) {
            if vad.process_chunk(chunk) == VadEvent::SentenceEnd {
                events += 1;
            }
        }
        assert_eq!(events, 0, "纯静默不应触发句尾");
    }

    #[test]
    fn vad_sentence_end_after_speech_then_silence() {
        let mut vad = EnergyVad::new(SAMPLE_RATE);

        // 1. 说话 1s（amplitude 0.1，远超阈值 0.005）
        let speech = generate_tone(1000, 0.1);
        for chunk in speech.chunks(160) {
            assert_eq!(vad.process_chunk(chunk), VadEvent::None);
        }

        // 2. 静默 400ms（> min_silence_ms=300ms）
        let silence = generate_silence(400);
        let mut got_end = false;
        for chunk in silence.chunks(160) {
            if vad.process_chunk(chunk) == VadEvent::SentenceEnd {
                got_end = true;
            }
        }
        assert!(got_end, "说话后静默 >300ms 应触发句尾");
    }

    #[test]
    fn vad_min_sentence_length_protection() {
        // min_sentence_ms = 800ms，说话只 500ms → 不切句
        let mut vad = EnergyVad::with_params(SAMPLE_RATE, 0.005, 300, 800);

        // 说话 500ms（< min_sentence_ms=800ms）
        let speech = generate_tone(500, 0.1);
        for chunk in speech.chunks(160) {
            assert_eq!(vad.process_chunk(chunk), VadEvent::None);
        }

        // 静默 400ms
        let silence = generate_silence(400);
        let mut got_end = false;
        for chunk in silence.chunks(160) {
            if vad.process_chunk(chunk) == VadEvent::SentenceEnd {
                got_end = true;
            }
        }
        assert!(!got_end, "句子 <800ms 不应触发句尾");
    }

    #[test]
    fn vad_continuous_speech_no_sentence_end() {
        let mut vad = EnergyVad::new(SAMPLE_RATE);

        // 持续有声 3s
        let speech = generate_tone(3000, 0.1);
        let mut events = 0;
        for chunk in speech.chunks(160) {
            if vad.process_chunk(chunk) == VadEvent::SentenceEnd {
                events += 1;
            }
        }
        assert_eq!(events, 0, "持续说话不应触发句尾");
    }

    #[test]
    fn vad_multiple_sentences() {
        let mut vad = EnergyVad::new(SAMPLE_RATE);

        // 句子1: 1s speech + 400ms silence
        let speech1 = generate_tone(1000, 0.1);
        for chunk in speech1.chunks(160) {
            vad.process_chunk(chunk);
        }
        let silence1 = generate_silence(400);
        let mut end_count = 0;
        for chunk in silence1.chunks(160) {
            if vad.process_chunk(chunk) == VadEvent::SentenceEnd {
                end_count += 1;
                vad.reset_sentence();
            }
        }
        assert_eq!(end_count, 1, "第一句应触发一次句尾");

        // 句子2: 1s speech + 400ms silence
        let speech2 = generate_tone(1000, 0.1);
        for chunk in speech2.chunks(160) {
            vad.process_chunk(chunk);
        }
        let silence2 = generate_silence(400);
        for chunk in silence2.chunks(160) {
            if vad.process_chunk(chunk) == VadEvent::SentenceEnd {
                end_count += 1;
                vad.reset_sentence();
            }
        }
        assert_eq!(end_count, 2, "第二句应触发第二次句尾");
    }

    #[test]
    fn vad_reset_clears_state() {
        let mut vad = EnergyVad::new(SAMPLE_RATE);

        // 积累一些状态
        let speech = generate_tone(1000, 0.1);
        for chunk in speech.chunks(160) {
            vad.process_chunk(chunk);
        }
        assert!(vad.is_speaking());

        vad.reset();
        assert!(!vad.is_speaking());
        assert_eq!(vad.sentence_samples(), 0);
    }

    #[test]
    fn vad_short_silence_no_sentence_end() {
        let mut vad = EnergyVad::new(SAMPLE_RATE);

        // 说话 1s
        let speech = generate_tone(1000, 0.1);
        for chunk in speech.chunks(160) {
            vad.process_chunk(chunk);
        }

        // 短暂停顿 200ms（< min_silence_ms=300ms）
        let short_pause = generate_silence(200);
        for chunk in short_pause.chunks(160) {
            assert_eq!(vad.process_chunk(chunk), VadEvent::None);
        }

        // 继续说话——不应有句尾事件
        let speech2 = generate_tone(500, 0.1);
        for chunk in speech2.chunks(160) {
            assert_eq!(vad.process_chunk(chunk), VadEvent::None);
        }
    }

    #[test]
    fn vad_low_amplitude_speech_treated_as_silence() {
        // amplitude 0.001 < threshold 0.005 → 被视为静默
        let mut vad = EnergyVad::with_params(SAMPLE_RATE, 0.005, 300, 800);

        let low_amplitude = generate_tone(2000, 0.001);
        let mut events = 0;
        for chunk in low_amplitude.chunks(160) {
            if vad.process_chunk(chunk) == VadEvent::SentenceEnd {
                events += 1;
            }
        }
        assert_eq!(events, 0, "低于阈值的信号不应触发句尾");
        assert!(!vad.is_speaking(), "低于阈值的信号不应进入 speaking 状态");
    }
}
