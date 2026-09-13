//! 能量 VAD（Voice Activity Detection）——纯 Rust 实现。
//!
//! ## 用途
//!
//! 0.10.4 伪流式引擎用此模块检测用户停顿（句尾），
//! 在停顿时触发"定稿识别"，实现"句尾即出字"的体感。
//!
//! ## 原理（0.22.15 自适应升级）
//!
//! 不需要 fsmn-vad 等神经网络 VAD——我们不需要精确的语音端点检测，
//! 只需要知道"用户停顿了"。基于 RMS 能量 + 滞回阈值 + 自适应底噪估计：
//!
//! 1. 以固定 frame（~10ms）计算 RMS
//! 2. 维护固定容量的近期能量统计，用低分位数估计 noise floor
//! 3. 从 noise floor 推导有上下界的 on/off threshold（滞回）
//! 4. RMS > on_threshold → 进入 speaking；RMS < off_threshold 且持续
//!    ≥ `min_silence_ms` → 触发 `SentenceEnd`
//! 5. 短 attack debounce 抑制单个脉冲；最小句子长度保护不变
//!
//! 旧 `silence_threshold` 保留为灵敏度基准（影响 on/off 的基线偏移），
//! 旧 JSON 配置继续反序列化，`min_silence_ms`、`min_sentence_ms` 语义不变。
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
    /// 超过软窗口后在低能量帧切段。
    SoftWindow,
    /// 持续有声达到硬上限，强制切段。
    HardWindow,
}

impl VadEvent {
    pub fn is_boundary(self) -> bool {
        self != Self::None
    }

    pub fn reason(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::SentenceEnd => "natural_silence",
            Self::SoftWindow => "soft_window",
            Self::HardWindow => "hard_window",
        }
    }
}

/// VAD 内部状态（用于测试和诊断导出）。
#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VadState {
    /// 是否正在说话
    pub speaking: bool,
    /// 当前底噪估计值
    pub noise_floor: f64,
    /// 当前 on 阈值
    pub on_threshold: f64,
    /// 当前 off 阈值
    pub off_threshold: f64,
    /// 当前句子已累积的样本数
    pub sentence_samples: usize,
    /// 当前静默已累积的样本数
    pub silence_samples: usize,
    /// 当前未提交段的总样本数（含段内静音）。
    pub segment_samples: usize,
}

/// 固定容量的环形能量统计 buffer。
///
/// 用于估计 noise floor——只存低能量分位附近的数据，
/// 不随录音长度无界增长。
struct EnergyHistory {
    /// 环形 buffer
    buf: Vec<f64>,
    /// 写入位置
    head: usize,
    /// 已写入数量（≤ buf.len()）
    count: usize,
}

impl EnergyHistory {
    fn new(capacity: usize) -> Self {
        Self {
            buf: vec![0.0; capacity],
            head: 0,
            count: 0,
        }
    }

    fn push(&mut self, val: f64) {
        self.buf[self.head] = val;
        self.head = (self.head + 1) % self.buf.len();
        if self.count < self.buf.len() {
            self.count += 1;
        }
    }

    /// 返回已存储的值（排序的副本），用于分位数计算。
    fn sorted_values(&self) -> Vec<f64> {
        let mut v: Vec<f64> = self.buf[..self.count].to_vec();
        v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        v
    }

    fn clear(&mut self) {
        self.head = 0;
        self.count = 0;
    }
}

/// 能量 VAD：检测用户停顿，用于切句。
///
/// 基于 RMS 能量 + 滞回阈值（on/off）+ 自适应底噪估计。
/// 每个 ~10ms 音频 chunk 调用 [`process_chunk`](Self::process_chunk)。
pub struct EnergyVad {
    /// 用户配置的灵敏度基准（默认 0.005）。
    /// 影响 on/off threshold 的偏移：配置值越低，越灵敏。
    silence_threshold: f64,
    /// 静默持续多久判定句尾（默认 300ms）
    min_silence_ms: u32,
    /// 最小句子长度：短于此值不切句（默认 800ms）
    /// 避免咳嗽、短暂噪声等触发误切
    min_sentence_ms: u32,
    /// 软窗口：持续有声达到该时长后可在低能量帧切段（默认 8s，0.23.7 可配置）
    soft_window_ms: u64,
    /// 硬窗口：持续有声达到该时长后强制切段（默认 12s，0.23.7 可配置）
    hard_window_ms: u64,
    /// 采样率
    sample_rate: u32,
    // ── 自适应状态 ──
    /// 近期能量历史（固定容量环形 buffer）
    energy_history: EnergyHistory,
    /// 当前底噪估计值（慢速更新）
    noise_floor: f64,
    /// 是否已初始化 noise_floor（首帧后为 true）
    noise_initialized: bool,
    // ── 运行时状态 ──
    /// 当前已累积的静默样本数
    silence_samples: usize,
    /// 是否正在说话（有声阶段）
    speaking: bool,
    /// 当前句子已累积的样本数（用于最小句子长度保护）
    sentence_samples: usize,
    /// attack debounce：短暂有声脉冲后需要连续有声才算入句
    /// （抑制键盘/鼠标单脉冲）
    attack_counter: usize,
    /// 当前未提交段的总样本数（含段内静音）。
    segment_samples: usize,
}

/// 能量历史 buffer 容量（约 1.5 秒 @ 10ms frame ≈ 150 帧）
const ENERGY_HISTORY_CAPACITY: usize = 150;

/// attack debounce 所需连续有声 frame 数（约 30ms @ 10ms）。
/// 单个脉冲不会进入 speaking，需连续 3 帧才算入句。
const ATTACK_DEBOUNCE_FRAMES: usize = 3;
const SOFT_WINDOW_MS: u64 = 8_000;
const HARD_WINDOW_MS: u64 = 12_000;

/// noise floor 更新速率：慢速上升（×0.02），较快下降（×0.1）。
/// 非对称更新避免持续人声快速抬高噪声基线。
const NOISE_FLOOR_RISE_RATE: f64 = 0.02;
const NOISE_FLOOR_FALL_RATE: f64 = 0.1;

/// on/off threshold 相对 noise_floor 的偏移倍数。
/// on = noise_floor + max(silence_threshold, noise_floor × on_factor)
/// off = noise_floor + max(silence_threshold * 0.5, noise_floor × off_factor)
/// on 必须严格高于 off（滞回）。
const ON_FACTOR: f64 = 3.0;
const OFF_FACTOR: f64 = 1.5;

/// on/off threshold 的上下界，防止极端值。
const THRESHOLD_MIN: f64 = 0.0005; // 约 -60dB
const THRESHOLD_MAX: f64 = 0.1; // 约 -20dB

impl EnergyVad {
    /// 创建默认配置的能量 VAD。
    ///
    /// 参数：
    /// - `sample_rate`：音频采样率（通常 16000）
    pub fn new(sample_rate: u32) -> Self {
        Self {
            silence_threshold: 0.005,
            min_silence_ms: 300,
            min_sentence_ms: 800,
            soft_window_ms: SOFT_WINDOW_MS,
            hard_window_ms: HARD_WINDOW_MS,
            sample_rate,
            energy_history: EnergyHistory::new(ENERGY_HISTORY_CAPACITY),
            noise_floor: 0.0,
            noise_initialized: false,
            silence_samples: 0,
            speaking: false,
            sentence_samples: 0,
            attack_counter: 0,
            segment_samples: 0,
        }
    }

    /// 创建带自定义参数的能量 VAD（使用默认 8s/12s 切片窗口）。
    ///
    /// 0.23.7 后生产路径经 [`Self::with_params_and_windows`] 从配置注入窗口；
    /// 本构造函数仅保留给测试。
    #[cfg(test)]
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
            soft_window_ms: SOFT_WINDOW_MS,
            hard_window_ms: HARD_WINDOW_MS,
            sample_rate,
            energy_history: EnergyHistory::new(ENERGY_HISTORY_CAPACITY),
            noise_floor: 0.0,
            noise_initialized: false,
            silence_samples: 0,
            speaking: false,
            sentence_samples: 0,
            attack_counter: 0,
            segment_samples: 0,
        }
    }

    /// 创建带自定义参数与切片窗口的能量 VAD（0.23.7 高级配置）。
    ///
    /// `soft_window_ms` / `hard_window_ms` 分别控制"低能量可切"与"强制切"
    /// 窗口；调用方（`VadConfig::sanitize`）负责保证 `soft < hard`。
    pub fn with_params_and_windows(
        sample_rate: u32,
        silence_threshold: f64,
        min_silence_ms: u32,
        min_sentence_ms: u32,
        soft_window_ms: u64,
        hard_window_ms: u64,
    ) -> Self {
        Self {
            silence_threshold,
            min_silence_ms,
            min_sentence_ms,
            soft_window_ms,
            hard_window_ms,
            sample_rate,
            energy_history: EnergyHistory::new(ENERGY_HISTORY_CAPACITY),
            noise_floor: 0.0,
            noise_initialized: false,
            silence_samples: 0,
            speaking: false,
            sentence_samples: 0,
            attack_counter: 0,
            segment_samples: 0,
        }
    }

    /// 处理一个音频 chunk，返回是否检测到句尾。
    ///
    /// 调用频率：cpal 回调约每 10ms 一次（160 samples @ 16kHz）。
    ///
    /// 内部以固定 ~10ms frame 计算 RMS，确保不同 chunk size 得到等价时长行为。
    pub fn process_chunk(&mut self, samples: &[f32]) -> VadEvent {
        if samples.is_empty() {
            return VadEvent::None;
        }

        let frame_size = (self.sample_rate as u64 / 100) as usize; // 10ms @ sample_rate
        let frame_size = frame_size.max(1);
        let min_silence_samples =
            (self.min_silence_ms as u64 * self.sample_rate as u64 / 1000) as usize;
        let min_sentence_samples =
            (self.min_sentence_ms as u64 * self.sample_rate as u64 / 1000) as usize;
        let soft_window_samples = (self.soft_window_ms * self.sample_rate as u64 / 1000) as usize;
        let hard_window_samples = (self.hard_window_ms * self.sample_rate as u64 / 1000) as usize;

        let mut event = VadEvent::None;

        // 按 frame 逐段处理
        let mut i = 0;
        while i < samples.len() {
            let end = (i + frame_size).min(samples.len());
            let frame = &samples[i..end];
            i = end;

            let rms = compute_rms(frame);
            let frame_samples = frame.len();

            // 更新能量历史和底噪估计
            self.update_noise_floor(rms);

            // 计算当前 on/off 阈值
            let (on_thresh, off_thresh) = self.compute_thresholds();

            if rms > on_thresh {
                // ── 有声 ──
                self.silence_samples = 0;

                if !self.speaking {
                    // attack debounce：需连续 ATTACK_DEBOUNCE_FRAMES 帧有声才算入句
                    self.attack_counter += 1;
                    if self.attack_counter >= ATTACK_DEBOUNCE_FRAMES {
                        self.speaking = true;
                        self.sentence_samples = 0;
                        self.segment_samples = 0;
                    }
                }

                if self.speaking {
                    self.sentence_samples += frame_samples;
                }
            } else if rms < off_thresh {
                // ── 静默 ──
                self.attack_counter = 0;
                self.silence_samples += frame_samples;

                if self.speaking && self.silence_samples >= min_silence_samples {
                    if self.sentence_samples >= min_sentence_samples {
                        self.speaking = false;
                        // 保留 sentence_samples 以便上层取出本句音频范围
                        event = VadEvent::SentenceEnd;
                    } else {
                        // 句子太短，不切——重置为静默等待状态
                        self.speaking = false;
                        self.sentence_samples = 0;
                        self.segment_samples = 0;
                    }
                } else if self.speaking && self.segment_samples >= soft_window_samples {
                    self.speaking = false;
                    event = VadEvent::SoftWindow;
                }
            }
            // off_thresh ≤ rms ≤ on_thresh：滞回区间，保持当前状态

            if self.speaking {
                self.segment_samples = self.segment_samples.saturating_add(frame_samples);
                if event == VadEvent::None && self.segment_samples >= hard_window_samples {
                    self.speaking = false;
                    event = VadEvent::HardWindow;
                }
            }
        }

        event
    }

    /// 更新底噪估计：用能量历史低分位数 + 非对称更新。
    fn update_noise_floor(&mut self, rms: f64) {
        // 处理非有限值
        let rms = if rms.is_nan() || rms.is_infinite() {
            0.0
        } else {
            rms
        };

        self.energy_history.push(rms);

        // 用低分位数（P25）作为底噪候选
        let sorted = self.energy_history.sorted_values();
        if sorted.is_empty() {
            return;
        }
        let p25_idx = sorted.len() / 4;
        let p25 = sorted[p25_idx];

        if !self.noise_initialized {
            // 首帧可能就是人声，不能把整帧能量直接吸收到 noise floor；否则中低音量
            // 语音会把 on threshold 抬到自身之上，此后整段都无法进入 speaking。
            // 先以阈值下界作保守种子，后续只在安全的低能量帧上自适应收敛。
            self.noise_floor = p25.min(THRESHOLD_MIN);
            self.noise_initialized = true;
            return;
        }

        // 非对称更新：慢升快降
        // 只在安全条件下更新：p25 不远高于当前 noise_floor 时才升（避免人声抬高底噪）
        if p25 > self.noise_floor {
            // 上升：只在 p25 < off_threshold 时才升（人在说话时 p25 会很高，不应更新底噪）
            let (_, off_thresh) = self.compute_thresholds();
            if p25 < off_thresh {
                self.noise_floor += (p25 - self.noise_floor) * NOISE_FLOOR_RISE_RATE;
            }
        } else {
            // 下降：较快
            self.noise_floor += (p25 - self.noise_floor) * NOISE_FLOOR_FALL_RATE;
        }
    }

    /// 从 noise_floor 和用户 silence_threshold 推导有上下界的 on/off 阈值。
    fn compute_thresholds(&self) -> (f64, f64) {
        let base = self.noise_floor.max(self.silence_threshold * 0.5);
        let on =
            (base * ON_FACTOR + self.silence_threshold * 0.5).clamp(THRESHOLD_MIN, THRESHOLD_MAX);
        let off =
            (base * OFF_FACTOR + self.silence_threshold * 0.25).clamp(THRESHOLD_MIN, THRESHOLD_MAX);
        // 保证 on > off（滞回）
        let on = on.max(off + (off * 0.1).min(0.001));
        (on, off)
    }

    /// 句尾事件后，重置句子计数器（准备下一句）。
    ///
    /// 上层在收到 `SentenceEnd` 并取出本句音频范围后调用。
    pub fn reset_sentence(&mut self) {
        self.sentence_samples = 0;
        self.silence_samples = 0;
        self.segment_samples = 0;
    }

    /// 是否正在说话（有声阶段）。
    pub fn is_speaking(&self) -> bool {
        self.speaking
    }

    /// 当前句子的样本数（用于最小句子长度判断）。
    #[cfg(test)]
    pub fn sentence_samples(&self) -> usize {
        self.sentence_samples
    }

    /// 0.23.7：当前软/硬窗口（ms）——供配置接线测试消费。
    #[cfg(test)]
    pub fn window_ms(&self) -> (u64, u64) {
        (self.soft_window_ms, self.hard_window_ms)
    }

    /// 0.22.15：当前 off 阈值——供裁剪等逻辑消费，避免两套静音定义。
    ///
    /// 此值随 `noise_floor` 自适应变化，反映当前环境底噪。
    /// `trim_trailing_silence` 消费此值而非固定阈值，确保裁剪与 VAD 一致。
    pub fn current_off_threshold(&self) -> f64 {
        let (_, off) = self.compute_thresholds();
        off
    }

    /// 完全重置状态（新录音会话）。
    pub fn reset(&mut self) {
        self.silence_samples = 0;
        self.speaking = false;
        self.sentence_samples = 0;
        self.attack_counter = 0;
        self.segment_samples = 0;
        self.energy_history.clear();
        self.noise_floor = 0.0;
        self.noise_initialized = false;
    }

    /// 导出当前内部状态（用于测试和诊断）。
    #[cfg(test)]
    pub fn dump_state(&self) -> VadState {
        let (on, off) = self.compute_thresholds();
        VadState {
            speaking: self.speaking,
            noise_floor: self.noise_floor,
            on_threshold: on,
            off_threshold: off,
            sentence_samples: self.sentence_samples,
            silence_samples: self.silence_samples,
            segment_samples: self.segment_samples,
        }
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
    fn speech_at_stream_start_is_not_absorbed_into_noise_floor() {
        let mut vad = EnergyVad::new(SAMPLE_RATE);
        // RMS ≈ 0.014：高于默认启动阈值；旧逻辑会用首帧把阈值抬到 ≈0.02 而漏掉。
        let speech = generate_tone(100, 0.02);
        for chunk in speech.chunks(160) {
            let _ = vad.process_chunk(chunk);
        }
        assert!(vad.is_speaking(), "流首中低音量语音应通过 attack debounce");
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
    fn vad_soft_window_uses_low_energy_boundary() {
        let mut vad = EnergyVad::new(SAMPLE_RATE);
        let speech = generate_tone(SOFT_WINDOW_MS as u32 + 100, 0.1);
        let mut event = VadEvent::None;
        for chunk in speech.chunks(160) {
            event = vad.process_chunk(chunk);
            if event.is_boundary() {
                break;
            }
        }
        assert_eq!(event, VadEvent::None, "软上限不应在持续高能量中硬切");

        for chunk in generate_silence(100).chunks(160) {
            event = vad.process_chunk(chunk);
            if event.is_boundary() {
                break;
            }
        }
        assert_eq!(event, VadEvent::SoftWindow);
    }

    #[test]
    fn vad_hard_window_bounds_continuous_speech() {
        let mut vad = EnergyVad::new(SAMPLE_RATE);
        let speech = generate_tone(HARD_WINDOW_MS as u32 + 500, 0.1);
        let mut event = VadEvent::None;
        for chunk in speech.chunks(160) {
            event = vad.process_chunk(chunk);
            if event.is_boundary() {
                break;
            }
        }
        assert_eq!(event, VadEvent::HardWindow);
    }

    /// 0.23.7：自定义软/硬窗口生效——窗口推迟后，同一段音频不再在默认
    /// 8s 软窗口处切，而在配置的更长窗口后低能量帧切段。
    #[test]
    fn vad_custom_windows_replace_defaults() {
        // 持续有声 8.5s + 200ms 短静默（不足 min_silence）：默认软窗口（8s）
        // 已过线，在静默帧按 SoftWindow 切；配置 10s 软窗口未过线，不切。
        let mut speech = generate_tone(8_500, 0.1);
        speech.extend(generate_silence(200));

        let mut default_vad = EnergyVad::new(SAMPLE_RATE);
        let mut configured = EnergyVad::with_params_and_windows(
            SAMPLE_RATE,
            0.005,
            300,
            800,
            10_000, // 软窗口推迟到 10s
            14_000, // 硬窗口推迟到 14s
        );

        let mut default_cut = false;
        let mut configured_cut = false;
        for chunk in speech.chunks(160) {
            if !default_cut && default_vad.process_chunk(chunk).is_boundary() {
                default_cut = true;
            }
            if !configured_cut && configured.process_chunk(chunk).is_boundary() {
                configured_cut = true;
            }
        }

        assert!(default_cut, "默认窗口应在 8.5s 有声后的静默帧按软窗口切句");
        assert!(!configured_cut, "推迟软窗口后 8.5s 持续有声不应切段");

        // 配置版补一段有声 + 静默越过 10s 后，应在软窗口后低能量处切（SoftWindow）
        let mut event = VadEvent::None;
        let mut more = generate_tone(2_000, 0.1);
        more.extend(generate_silence(200));
        for chunk in more.chunks(160) {
            event = configured.process_chunk(chunk);
            if event.is_boundary() {
                break;
            }
        }
        assert_eq!(event, VadEvent::SoftWindow, "推迟后的软窗口仍走低能量切点");
    }

    /// 0.23.7：硬窗口可配置——14s 硬窗口下持续有声到 12.5s 不切，
    /// 超过 14s 才强制切。
    #[test]
    fn vad_custom_hard_window_defers_forced_cut() {
        let mut vad =
            EnergyVad::with_params_and_windows(SAMPLE_RATE, 0.005, 300, 800, 10_000, 14_000);

        // 12.5s 持续有声：默认硬窗口（12s）已切，14s 硬窗口不切
        let speech = generate_tone(12_500, 0.1);
        let mut cut = false;
        for chunk in speech.chunks(160) {
            if vad.process_chunk(chunk).is_boundary() {
                cut = true;
                break;
            }
        }
        assert!(!cut, "14s 硬窗口下 12.5s 持续有声不应强制切");

        // 补 0.6s（越过 14s 但不到 14s+0.5s 的余量则仍不切；这里直接推过线）
        let speech = generate_tone(2_000, 0.1);
        let mut event = VadEvent::None;
        for chunk in speech.chunks(160) {
            event = vad.process_chunk(chunk);
            if event.is_boundary() {
                break;
            }
        }
        assert_eq!(event, VadEvent::HardWindow, "越过 14s 后应强制切");
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
        // 0.22.15：升级为自适应 VAD 后，低振幅（0.001）的信号在底噪估计后会
        // 被判定为静默——底噪约 0.001，on_threshold 远高于此值。
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

    // ── 0.22.15 自适应 VAD 新增测试 ──

    #[test]
    fn vad_all_silence_no_event() {
        let mut vad = EnergyVad::new(SAMPLE_RATE);
        let silence = generate_silence(2000); // 2s 纯静默
        let mut events = 0;
        for chunk in silence.chunks(160) {
            if vad.process_chunk(chunk) == VadEvent::SentenceEnd {
                events += 1;
            }
        }
        assert_eq!(events, 0, "全静默不应触发任何事件");
        assert!(!vad.is_speaking());
    }

    #[test]
    fn vad_quiet_background_low_speech_enters_speaking() {
        // 安静背景下低振幅语音应进入 speaking 并在静音后切句
        let mut vad = EnergyVad::with_params(SAMPLE_RATE, 0.002, 300, 500);

        // 先喂 500ms 静默让底噪校准
        let init_silence = generate_silence(500);
        for chunk in init_silence.chunks(160) {
            vad.process_chunk(chunk);
        }

        // 低振幅语音 800ms（amplitude 0.01，低但非零）
        let speech = generate_tone(800, 0.01);
        for chunk in speech.chunks(160) {
            vad.process_chunk(chunk);
        }
        assert!(vad.is_speaking(), "低振幅语音应进入 speaking");

        // 静默 400ms 应触发句尾
        let silence = generate_silence(400);
        let mut got_end = false;
        for chunk in silence.chunks(160) {
            if vad.process_chunk(chunk) == VadEvent::SentenceEnd {
                got_end = true;
            }
        }
        assert!(got_end, "低振幅语音后静默应触发句尾");
    }

    #[test]
    fn vad_stable_fan_noise_no_false_speaking() {
        // 稳定风扇噪声本身不进入 speaking
        let mut vad = EnergyVad::with_params(SAMPLE_RATE, 0.005, 300, 800);

        // 模拟风扇噪声：低振幅随机噪声
        let mut noise = Vec::new();
        for i in 0..16000 {
            // 伪随机噪声，振幅 ~0.003
            let val = ((i as f32 * 0.7).sin() * 0.002 + (i as f32 * 1.3).cos() * 0.001) * 1.5;
            noise.push(val);
        }
        for chunk in noise.chunks(160) {
            let _ = vad.process_chunk(chunk);
        }
        // 风扇噪声不应进入 speaking
        assert!(!vad.is_speaking(), "稳定风扇噪声不应进入 speaking");
    }

    #[test]
    fn vad_stable_fan_noise_plus_speech_triggers_sentence_end() {
        let mut vad = EnergyVad::with_params(SAMPLE_RATE, 0.005, 300, 500);

        // 先喂 1s 风扇噪声
        let mut noise = Vec::new();
        for i in 0..16000 {
            let val = ((i as f32 * 0.7).sin() * 0.002 + (i as f32 * 1.3).cos() * 0.001) * 1.5;
            noise.push(val);
        }
        for chunk in noise.chunks(160) {
            vad.process_chunk(chunk);
        }

        // 叠加语音（1s 高振幅正弦波）
        let speech = generate_tone(1000, 0.08);
        for chunk in speech.chunks(160) {
            vad.process_chunk(chunk);
        }
        assert!(vad.is_speaking(), "风扇噪声 + 语音应进入 speaking");

        // 静默 400ms
        let silence = generate_silence(400);
        let mut got_end = false;
        for chunk in silence.chunks(160) {
            if vad.process_chunk(chunk) == VadEvent::SentenceEnd {
                got_end = true;
            }
        }
        assert!(got_end, "风扇噪声 + 语音后静默应触发句尾");
    }

    #[test]
    fn vad_threshold_jitter_no_repeated_state_changes() {
        // 阈值附近抖动不应反复切状态
        let mut vad = EnergyVad::with_params(SAMPLE_RATE, 0.005, 300, 500);

        // 先校准底噪
        let silence = generate_silence(500);
        for chunk in silence.chunks(160) {
            vad.process_chunk(chunk);
        }

        // 在阈值附近抖动：交替高低振幅
        let mut jitter = Vec::new();
        for _ in 0..50 {
            // 10ms 有声
            jitter.extend(generate_tone(10, 0.02));
            // 10ms 低声
            jitter.extend(generate_tone(10, 0.003));
        }
        // 总共 1s 抖动
        let mut state_changes = 0;
        let mut prev_speaking = false;
        for chunk in jitter.chunks(160) {
            vad.process_chunk(chunk);
            if vad.is_speaking() != prev_speaking {
                state_changes += 1;
                prev_speaking = vad.is_speaking();
            }
        }
        // 滞回应防止频繁切换——允许进入 speaking 但不应反复进出
        assert!(
            state_changes <= 2,
            "阈值抖动不应反复切状态，实际切换 {state_changes} 次"
        );
    }

    #[test]
    fn vad_single_keyboard_pulse_no_sentence() {
        // 单个键盘/鼠标脉冲不独立提交句子
        let mut vad = EnergyVad::with_params(SAMPLE_RATE, 0.005, 300, 800);

        // 先校准底噪
        let silence = generate_silence(500);
        for chunk in silence.chunks(160) {
            vad.process_chunk(chunk);
        }

        // 单个短脉冲（20ms 高振幅）
        let pulse = generate_tone(20, 0.1);
        vad.process_chunk(&pulse);

        // attack debounce 应阻止进入 speaking
        assert!(!vad.is_speaking(), "单个短脉冲不应进入 speaking");
    }

    #[test]
    fn vad_cough_pulse_no_sentence() {
        // 咳嗽样脉冲（~100ms）不独立提交句子
        let mut vad = EnergyVad::with_params(SAMPLE_RATE, 0.005, 200, 800);

        // 先校准底噪
        let silence = generate_silence(500);
        for chunk in silence.chunks(160) {
            vad.process_chunk(chunk);
        }

        // 咳嗽样脉冲：100ms
        let pulse = generate_tone(100, 0.05);
        for chunk in pulse.chunks(160) {
            vad.process_chunk(chunk);
        }

        // 随后静默
        let silence2 = generate_silence(400);
        let mut events = 0;
        for chunk in silence2.chunks(160) {
            if vad.process_chunk(chunk) == VadEvent::SentenceEnd {
                events += 1;
            }
        }
        // min_sentence_ms=800ms，100ms 脉冲不够长，不应触发句尾
        assert_eq!(events, 0, "咳嗽脉冲不应触发句尾");
    }

    #[test]
    fn vad_multiple_sentences_correct_count() {
        let mut vad = EnergyVad::with_params(SAMPLE_RATE, 0.005, 300, 500);

        for sentence_idx in 0..3 {
            let speech = generate_tone(1000, 0.1); // 1s 语音
            for chunk in speech.chunks(160) {
                vad.process_chunk(chunk);
            }
            let silence = generate_silence(400); // 400ms 静默
            let mut got_end = false;
            for chunk in silence.chunks(160) {
                if vad.process_chunk(chunk) == VadEvent::SentenceEnd {
                    got_end = true;
                }
            }
            assert!(got_end, "第 {} 句应触发句尾", sentence_idx + 1);
            vad.reset_sentence();
        }
    }

    #[test]
    fn vad_different_chunk_sizes_same_events() {
        // 不同 chunk 切分方式对同一波形应得到相同事件数
        let mut vad1 = EnergyVad::with_params(SAMPLE_RATE, 0.005, 300, 500);
        let mut vad2 = EnergyVad::with_params(SAMPLE_RATE, 0.005, 300, 500);

        // 1s 语音 + 400ms 静默
        let mut audio = generate_tone(1000, 0.1);
        audio.extend(generate_silence(400));

        // vad1: 160 samples/chunk (10ms)
        let mut events1 = 0;
        for chunk in audio.chunks(160) {
            if vad1.process_chunk(chunk) == VadEvent::SentenceEnd {
                events1 += 1;
            }
        }

        // vad2: 320 samples/chunk (20ms)
        let mut events2 = 0;
        for chunk in audio.chunks(320) {
            if vad2.process_chunk(chunk) == VadEvent::SentenceEnd {
                events2 += 1;
            }
        }

        assert_eq!(events1, events2, "不同 chunk size 应产生相同事件数");
    }

    #[test]
    fn vad_reset_clears_adaptive_state() {
        let mut vad = EnergyVad::with_params(SAMPLE_RATE, 0.005, 300, 800);

        // 积累状态
        let speech = generate_tone(1000, 0.1);
        for chunk in speech.chunks(160) {
            vad.process_chunk(chunk);
        }
        assert!(vad.is_speaking());

        vad.reset();
        assert!(!vad.is_speaking());
        assert_eq!(vad.sentence_samples(), 0);

        let state = vad.dump_state();
        assert_eq!(state.noise_floor, 0.0, "reset 后 noise_floor 应为 0");
    }

    #[test]
    fn vad_nan_and_infinity_safe() {
        let mut vad = EnergyVad::new(SAMPLE_RATE);

        // NaN 和 Infinity 不应 panic
        vad.process_chunk(&[f32::NAN; 160]);
        vad.process_chunk(&[f32::INFINITY; 160]);
        vad.process_chunk(&[f32::NEG_INFINITY; 160]);
        // 正常数据仍可工作
        let speech = generate_tone(1000, 0.1);
        for chunk in speech.chunks(160) {
            vad.process_chunk(chunk);
        }
    }
}
