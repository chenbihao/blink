//! 共享音频规范化链路：downmix mono + stateful resample 16kHz。
//!
//! 0.22.16 Handoff 02：麦克风与文件 WAV 共用的唯一规范化链路。
//!
//! ## 数据流
//!
//! ```text
//! interleaved f32 + source format
//!   → downmix mono (完整 frame)
//!   → stateful resample 16kHz
//!   → NormalizedAudio + NormalizationSummary
//! ```
//!
//! ## 设计约束
//!
//! - **downmix 只接受完整 frame**：调用方负责切齐，不完整帧被丢弃（记日志）。
//! - **StreamingResampler 保存相位和跨 chunk 历史**：只在 new capture stream / reset 时清理。
//! - **相位累计使用整数有理表达**：避免长音频浮点漂移。
//! - **finish() 明确定义尾部 frame 数和舍入语义**。
//! - **callback 不 panic、不阻塞、不做文件 I/O**。
//! - **不破坏 0.22.15 VAD 输入时序和 16k mono 契约**。

#![allow(dead_code)]

use super::format::SourceFormat;

// ── 常量 ──────────────────────────────────────────────────────────────────

/// STT 模型期望的统一输出采样率。
pub const TARGET_SAMPLE_RATE: u32 = 16000;

/// STT 模型期望的统一输出声道数。
pub const TARGET_CHANNELS: u16 = 1;

// ── WAVEFORMATEXTENSIBLE channel mask 位定义 ──────────────────────────────
//
// 参照 Microsoft WAVEFORMATEXTENSIBLE 文档：
// https://learn.microsoft.com/en-us/windows-hardware/drivers/audio/channel-mask
const SPEAKER_FRONT_LEFT: u32 = 0x1;
const SPEAKER_FRONT_RIGHT: u32 = 0x2;
const SPEAKER_FRONT_CENTER: u32 = 0x4;
const SPEAKER_LOW_FREQUENCY: u32 = 0x8;
const SPEAKER_BACK_LEFT: u32 = 0x10;
const SPEAKER_BACK_RIGHT: u32 = 0x20;
const SPEAKER_FRONT_LEFT_OF_CENTER: u32 = 0x40;
const SPEAKER_FRONT_RIGHT_OF_CENTER: u32 = 0x80;
const SPEAKER_BACK_CENTER: u32 = 0x100;
const SPEAKER_SIDE_LEFT: u32 = 0x200;
const SPEAKER_SIDE_RIGHT: u32 = 0x400;
const SPEAKER_TOP_CENTER: u32 = 0x800;

/// 所有已知的非 LFE 声道位掩码。
/// LFE (0x8) 被有意排除——降混时不与主声道等权平均。
const KNOWN_NON_LFE_MASKS: u32 = SPEAKER_FRONT_LEFT
    | SPEAKER_FRONT_RIGHT
    | SPEAKER_FRONT_CENTER
    | SPEAKER_BACK_LEFT
    | SPEAKER_BACK_RIGHT
    | SPEAKER_FRONT_LEFT_OF_CENTER
    | SPEAKER_FRONT_RIGHT_OF_CENTER
    | SPEAKER_BACK_CENTER
    | SPEAKER_SIDE_LEFT
    | SPEAKER_SIDE_RIGHT
    | SPEAKER_TOP_CENTER;

// ── 输出类型 ─────────────────────────────────────────────────────────────

/// 规范化后的音频数据：固定 16kHz、mono、f32。
#[derive(Debug, Clone)]
pub struct NormalizedAudio {
    /// PCM 样本（f32, [-1.0, 1.0], mono）。
    pub samples: Vec<f32>,
    /// 来源格式摘要。
    pub source: NormalizationSummary,
}

/// 规范化过程的来源与变换摘要。
#[derive(Debug, Clone, Copy)]
pub struct NormalizationSummary {
    /// 来源声道数。
    pub source_channels: u16,
    /// 来源采样率（Hz）。
    pub source_sample_rate: u32,
    /// 来源容器位深。
    pub source_bits_per_sample: u16,
    /// 来源样本类型。
    pub source_kind: super::format::SampleKind,
    /// 来源 channel mask（如有）。
    pub source_channel_mask: Option<u32>,
    /// 输出采样率（固定 16000）。
    pub target_sample_rate: u32,
    /// 降混策略。
    pub downmix_strategy: DownmixStrategy,
}

/// 降混到 mono 时使用的策略。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DownmixStrategy {
    /// 输入已是 mono，无变换。
    Identity,
    /// 简单等权平均（L+R+...）/N，用于无 channel mask 的多声道或 mask 全为前左/右。
    SimpleAverage,
    /// 消费 channel mask，排除 LFE 后按权降混。
    ChannelMask,
    /// 无法解释 layout，用简单平均回退。
    FallbackAverage,
}

impl std::fmt::Display for NormalizationSummary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}ch {}Hz → {}Hz mono",
            self.source_channels, self.source_sample_rate, self.target_sample_rate
        )?;
        if let Some(mask) = self.source_channel_mask {
            write!(f, " mask=0x{:X}", mask)?;
        }
        write!(f, " strategy={:?}", self.downmix_strategy)?;
        Ok(())
    }
}

// ── Downmix ──────────────────────────────────────────────────────────────

/// 将 interleaved f32 多声道降混为 mono。
///
/// ## 规则
///
/// - **mono identity**：`channels == 1` 时直接返回（zero-copy）。
/// - **无 channel mask 的 stereo**：使用 `(L + R) * 0.5`（等权平均）。
/// - **有 channel mask 的多声道**：消费 mask，排除 LFE，只对已知主声道平均；
///   若 mask 无法解释任何声道，回退到简单平均。
/// - **不完整 frame 被丢弃**：输入长度不是 `channels` 的整数倍时，
///   尾部多余样本被截断（不 panic）。
///
/// 返回 `(mono_samples, strategy)`。
pub fn downmix_to_mono(
    input: &[f32],
    channels: u16,
    channel_mask: Option<u32>,
) -> (Vec<f32>, DownmixStrategy) {
    if channels == 0 {
        return (Vec::new(), DownmixStrategy::Identity);
    }
    if channels == 1 {
        return (input.to_vec(), DownmixStrategy::Identity);
    }

    let ch = channels as usize;
    let num_complete_frames = input.len() / ch;
    // 不完整 frame 的尾部样本被丢弃
    let leftover = input.len() - num_complete_frames * ch;
    if leftover > 0 {
        tracing::trace!(
            leftover_samples = leftover,
            channels = ch,
            "downmix: 丢弃不完整 frame 尾部样本"
        );
    }

    let frame_data = &input[..num_complete_frames * ch];

    // 尝试消费 channel mask
    let mask_fell_back = channel_mask.is_some();
    if let Some(mask) = channel_mask
        && let Some(result) = downmix_with_mask(frame_data, ch, mask)
    {
        return (result, DownmixStrategy::ChannelMask);
    }
    // 如果 mask 存在但无法解释 → 标记为 FallbackAverage

    // 简单等权平均
    let mut output = Vec::with_capacity(num_complete_frames);
    for frame in frame_data.chunks_exact(ch) {
        let sum: f32 = frame.iter().sum();
        output.push(sum / ch as f32);
    }
    let strategy = if mask_fell_back {
        DownmixStrategy::FallbackAverage
    } else {
        DownmixStrategy::SimpleAverage
    };
    (output, strategy)
}

/// 消费 channel mask 进行降混：排除 LFE，对已知主声道等权平均。
///
/// 如果 mask 为 0 或完全不匹配已知声道位，返回 `None`，调用方回退到简单平均。
fn downmix_with_mask(input: &[f32], channels: usize, mask: u32) -> Option<Vec<f32>> {
    // WAVEFORMATEXTENSIBLE 的 channel mask 只有 32 位，无法描述第 33 个及
    // 之后的声道。此时不能把“无对应 bit”猜成“该声道不参与”，否则会静默
    // 丢失音频；统一 fail-safe 回退到所有声道简单平均。
    if mask == 0 || channels == 0 || channels > u32::BITS as usize {
        return None;
    }

    // 确定哪些声道参与降混（排除 LFE）
    let mut active_channels = Vec::with_capacity(channels);
    for i in 0..channels {
        let bit = 1u32 << i;
        if mask & bit != 0 {
            // 此声道在 mask 中有对应位
            if bit == SPEAKER_LOW_FREQUENCY {
                // LFE 不参与等权降混
                active_channels.push(false);
            } else if KNOWN_NON_LFE_MASKS & bit != 0 {
                active_channels.push(true);
            } else {
                // 未知位 → 不猜测，回退
                tracing::trace!(
                    channel_index = i,
                    bit = format!("0x{:X}", bit),
                    "downmix: channel mask 中有未知位，回退到简单平均"
                );
                return None;
            }
        } else {
            // mask 中没有此声道的位 → 不参与降混
            active_channels.push(false);
        }
    }

    let active_count = active_channels.iter().filter(|&&a| a).count();
    if active_count == 0 {
        // mask 完全不匹配 → 回退
        return None;
    }

    let num_frames = input.len() / channels;
    let mut output = Vec::with_capacity(num_frames);
    for frame in input.chunks_exact(channels) {
        let sum: f32 = frame
            .iter()
            .zip(active_channels.iter())
            .filter(|&(_, &active)| active)
            .map(|(s, _)| *s)
            .sum();
        output.push(sum / active_count as f32);
    }
    Some(output)
}

// ── StreamingResampler ───────────────────────────────────────────────────

/// 有状态重采样器：跨 chunk 保持相位连续。
///
/// ## 相位表达
///
/// 使用 **整数有理表达** 避免浮点漂移：
/// - `write_phase`: 已产出的目标样本数（u64）
/// - 对每个输出样本 j，源位置 = j * from_rate / to_rate（整数除法）
/// - 小数部分 frac = (j * from_rate % to_rate) / to_rate
///
/// ## 历史（pending buffer）
///
/// 跨 chunk 线性插值需要保留尚未被任何输出样本完全消耗的源样本。
/// `pending` 缓冲区存储已喂入但尚未被输出样本"越过"的源样本，
/// 确保任意 chunk 切割方式产出逐样本等价的结果。
///
/// ## finish()
///
/// `finish()` 刷出残余目标样本：基于已消费的源样本总数，
/// 计算还能产出多少目标样本（向下取整）。
pub struct StreamingResampler {
    from_rate: u32,
    to_rate: u32,
    /// 已产出的目标样本数（整数计数）。
    write_phase: u64,
    /// 已喂入的源样本总数（整数计数）。
    read_phase: u64,
    /// 待处理缓冲区：存储尚未被输出样本"越过"的源样本。
    /// 第一个元素始终是最后一个被"越过"的样本（即 idx0 可引用的最近历史）。
    pending: Vec<f32>,
    /// 是否已 finish（finish 后不再接受输入）。
    finished: bool,
}

impl StreamingResampler {
    /// 创建重采样器。
    ///
    /// `from_rate == to_rate` 时为 identity（不做变换，只拷贝）。
    pub fn new(from_rate: u32, to_rate: u32) -> Self {
        debug_assert!(from_rate > 0 && to_rate > 0, "sample rates must be > 0");
        Self {
            from_rate,
            to_rate,
            write_phase: 0,
            read_phase: 0,
            pending: Vec::new(),
            finished: false,
        }
    }

    /// 是否为 identity（不改采样率）。
    pub fn is_identity(&self) -> bool {
        self.from_rate == self.to_rate
    }

    /// 重置相位和历史（用于新 capture stream）。
    pub fn reset(&mut self) {
        self.write_phase = 0;
        self.read_phase = 0;
        self.pending.clear();
        self.finished = false;
    }

    /// 喂入一段 mono 源样本，产出目标采样率的 mono 样本。
    ///
    /// **不 panic**：即使输入为空也安全返回空 Vec。
    /// **不阻塞、不做 I/O**。
    pub fn process(&mut self, input: &[f32]) -> Vec<f32> {
        if self.finished {
            tracing::trace!("resampler: process called after finish, ignoring");
            return Vec::new();
        }
        if input.is_empty() {
            return Vec::new();
        }
        if self.is_identity() {
            // identity：不改幅、只传递
            self.read_phase += input.len() as u64;
            return input.to_vec();
        }

        // 将输入追加到 pending 缓冲区
        self.pending.extend_from_slice(input);
        self.read_phase += input.len() as u64;

        // 计算能产出多少目标样本
        //
        // 对于输出样本 j，源位置 idx0 = j * from_rate / to_rate，
        // 小数部分 frac = (j * from_rate % to_rate) / to_rate。
        //
        // - 当 frac == 0 时，只需 idx0 < read_phase（s0 即可，不需 idx0+1）。
        // - 当 frac > 0 时，需要 idx0+1 < read_phase（线性插值需要两个样本）。
        //
        // 从后往前检查候选样本：如果最后一个不可用，往前退。
        // 由于 frac 和 idx0 是 j 的函数，最多退 step 个（step = to_rate / gcd(from_rate, to_rate)）。
        let max_output_phase = self
            .read_phase
            .saturating_mul(self.to_rate as u64)
            .checked_div(self.from_rate as u64)
            .unwrap_or(0);

        // 从后往前找到最大的可安全产出的 j+1
        let mut max_safe_output = max_output_phase;
        while max_safe_output > self.write_phase {
            let j = max_safe_output - 1;
            let src_pos = j * self.from_rate as u64;
            let idx0 = src_pos / self.to_rate as u64;
            let frac = src_pos % self.to_rate as u64;
            if frac == 0 {
                // 只需 idx0 < read_phase
                if idx0 < self.read_phase {
                    break;
                }
            } else {
                // 需要 idx0+1 < read_phase
                if idx0 + 1 < self.read_phase {
                    break;
                }
            }
            max_safe_output -= 1;
        }

        let num_output = max_safe_output.saturating_sub(self.write_phase);

        if num_output == 0 {
            return Vec::new();
        }

        let mut output = Vec::with_capacity(num_output as usize);
        let pending_base = self.read_phase - self.pending.len() as u64; // pending[0] 的源索引

        for _ in 0..num_output {
            let j = self.write_phase;
            let src_pos_scaled = j * self.from_rate as u64;
            let idx0 = src_pos_scaled / self.to_rate as u64;
            let frac_scaled = src_pos_scaled % self.to_rate as u64;
            let frac = frac_scaled as f32 / self.to_rate as f32;

            // 在 pending 中查找 idx0 和 idx0+1
            let local_idx0 = (idx0.saturating_sub(pending_base)) as usize;
            let local_idx1 = local_idx0 + 1;

            let s0 = self.pending.get(local_idx0).copied().unwrap_or(0.0);
            let s1 = self.pending.get(local_idx1).copied().unwrap_or(s0);

            output.push(s0 * (1.0 - frac) + s1 * frac);
            self.write_phase += 1;
        }

        // 清理已"越过"的样本：保留最后一个被引用的样本作为历史
        if self.write_phase > 0 {
            let last_j = self.write_phase - 1;
            let last_src_pos = last_j * self.from_rate as u64;
            let last_idx0 = last_src_pos / self.to_rate as u64;
            // 保留 last_idx0 及之后的样本
            let keep_from = last_idx0.saturating_sub(pending_base) as usize;
            if keep_from > 0 && keep_from < self.pending.len() {
                self.pending.drain(..keep_from);
            }
        }

        output
    }

    /// 刷出残余目标样本。
    ///
    /// 基于已消费的源样本总数，计算还能产出多少目标样本（向下取整）。
    /// 这确保尾部 frame 数有明确定义。
    ///
    /// `finish()` 后再调用 `process()` 不会产出新样本。
    pub fn finish(&mut self) -> Vec<f32> {
        if self.finished {
            return Vec::new();
        }
        self.finished = true;

        if self.is_identity() {
            return Vec::new();
        }

        // 计算还能产出多少目标样本
        // 此时不再有新 input，只用 pending 中的残余
        //
        // 与 process() 相同逻辑：从后往前检查候选样本
        let max_output_phase = self
            .read_phase
            .saturating_mul(self.to_rate as u64)
            .checked_div(self.from_rate as u64)
            .unwrap_or(0);

        let mut max_safe_output = max_output_phase;
        while max_safe_output > self.write_phase {
            let j = max_safe_output - 1;
            let src_pos = j * self.from_rate as u64;
            let idx0 = src_pos / self.to_rate as u64;
            let frac = src_pos % self.to_rate as u64;
            if frac == 0 {
                if idx0 < self.read_phase {
                    break;
                }
            } else {
                if idx0 + 1 < self.read_phase {
                    break;
                }
            }
            max_safe_output -= 1;
        }
        let remaining = max_safe_output.saturating_sub(self.write_phase);

        if remaining == 0 {
            return Vec::new();
        }

        let mut output = Vec::with_capacity(remaining as usize);
        let pending_base = self.read_phase - self.pending.len() as u64;

        for _ in 0..remaining {
            let j = self.write_phase;
            let src_pos_scaled = j * self.from_rate as u64;
            let idx0 = src_pos_scaled / self.to_rate as u64;
            let frac_scaled = src_pos_scaled % self.to_rate as u64;
            let frac = frac_scaled as f32 / self.to_rate as f32;

            let local_idx0 = (idx0.saturating_sub(pending_base)) as usize;
            let local_idx1 = local_idx0 + 1;

            let s0 = self.pending.get(local_idx0).copied().unwrap_or(0.0);
            let s1 = self.pending.get(local_idx1).copied().unwrap_or(s0);

            output.push(s0 * (1.0 - frac) + s1 * frac);
            self.write_phase += 1;
        }

        output
    }
}

// ── AudioNormalizer ──────────────────────────────────────────────────────

/// 每条 capture stream 持有的独立 normalizer 状态。
///
/// 把 downmix + streaming resample 组合为单一入口。
/// 每条 capture stream 创建一个新实例；reset 时清理相位。
pub struct AudioNormalizer {
    source_channels: u16,
    source_sample_rate: u32,
    channel_mask: Option<u32>,
    resampler: StreamingResampler,
    /// 缓冲不完整 frame 的尾部样本（跨 callback 拼接）。
    frame_buffer: Vec<f32>,
    summary: NormalizationSummary,
}

impl AudioNormalizer {
    /// 从来源格式创建 normalizer。
    pub fn from_source_format(fmt: SourceFormat) -> Self {
        let resampler = StreamingResampler::new(fmt.sample_rate, TARGET_SAMPLE_RATE);
        let summary = NormalizationSummary {
            source_channels: fmt.channels,
            source_sample_rate: fmt.sample_rate,
            source_bits_per_sample: fmt.bits_per_sample,
            source_kind: fmt.kind,
            source_channel_mask: fmt.channel_mask,
            target_sample_rate: TARGET_SAMPLE_RATE,
            downmix_strategy: DownmixStrategy::Identity, // 会在 process 时更新
        };
        Self {
            source_channels: fmt.channels,
            source_sample_rate: fmt.sample_rate,
            channel_mask: fmt.channel_mask,
            resampler,
            frame_buffer: Vec::new(),
            summary,
        }
    }

    /// 从简单参数创建 normalizer（用于 cpal 回调，无完整 SourceFormat）。
    pub fn new(source_channels: u16, source_sample_rate: u32) -> Self {
        Self::from_source_format(SourceFormat {
            channels: source_channels,
            sample_rate: source_sample_rate,
            bits_per_sample: 32,
            valid_bits: 32,
            kind: super::format::SampleKind::IeeeFloat,
            channel_mask: None,
        })
    }

    /// 处理一段 interleaved f32 样本，返回 16kHz mono f32 样本。
    ///
    /// - 不 panic、不阻塞、不做文件 I/O
    /// - 不完整 frame 尾部缓存在内部，下次 process 拼接
    pub fn process(&mut self, input: &[f32]) -> Vec<f32> {
        if input.is_empty() {
            return self.resampler.process(&[]);
        }

        let ch = self.source_channels as usize;
        if ch == 0 {
            return Vec::new();
        }

        // 拼接上次的不完整 frame 尾部
        let mut combined = Vec::with_capacity(self.frame_buffer.len() + input.len());
        combined.extend_from_slice(&self.frame_buffer);
        combined.extend_from_slice(input);
        self.frame_buffer.clear();

        // 切齐到完整 frame
        let num_complete = combined.len() / ch * ch;
        let frame_data = &combined[..num_complete];
        let leftover = &combined[num_complete..];

        if !leftover.is_empty() {
            self.frame_buffer.extend_from_slice(leftover);
        }

        // downmix
        let (mono, strategy) = downmix_to_mono(frame_data, self.source_channels, self.channel_mask);
        self.summary.downmix_strategy = strategy;

        // resample
        self.resampler.process(&mono)
    }

    /// 刷出残余样本。
    pub fn finish(&mut self) -> Vec<f32> {
        // 处理 frame_buffer 中的残余（不完整 frame → 丢弃，记日志）
        if !self.frame_buffer.is_empty() {
            tracing::trace!(
                leftover = self.frame_buffer.len(),
                channels = self.source_channels,
                "normalizer.finish: 丢弃不完整 frame 残余"
            );
            self.frame_buffer.clear();
        }
        self.resampler.finish()
    }

    /// 重置（用于新 capture stream）。
    pub fn reset(&mut self) {
        self.resampler.reset();
        self.frame_buffer.clear();
    }

    /// 获取来源摘要。
    pub fn summary(&self) -> NormalizationSummary {
        self.summary
    }
}

// ── 测试 ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── downmix ──────────────────────────────────────────────────────────

    #[test]
    fn downmix_mono_identity() {
        let input = vec![0.5, 0.3, -0.2, 0.8];
        let (output, strategy) = downmix_to_mono(&input, 1, None);
        assert_eq!(output, input);
        assert_eq!(strategy, DownmixStrategy::Identity);
    }

    #[test]
    fn downmix_stereo_no_mask() {
        // 2 frames stereo: [L0, R0, L1, R1]
        let input = vec![0.2, 0.4, 0.6, 0.8];
        let (output, strategy) = downmix_to_mono(&input, 2, None);
        assert_eq!(output.len(), 2);
        assert!((output[0] - 0.3).abs() < 1e-6, "got {}", output[0]);
        assert!((output[1] - 0.7).abs() < 1e-6, "got {}", output[1]);
        assert_eq!(strategy, DownmixStrategy::SimpleAverage);
    }

    #[test]
    fn downmix_stereo_with_mask() {
        // mask = FL | FR = 0x3
        let input = vec![0.2, 0.4, 0.6, 0.8];
        let (output, strategy) = downmix_to_mono(&input, 2, Some(0x3));
        assert_eq!(output.len(), 2);
        assert!((output[0] - 0.3).abs() < 1e-6, "got {}", output[0]);
        assert!((output[1] - 0.7).abs() < 1e-6, "got {}", output[1]);
        assert_eq!(strategy, DownmixStrategy::ChannelMask);
    }

    #[test]
    fn downmix_5_1_with_lfe_excluded() {
        // 5.1: FL, FR, FC, LFE, BL, BR
        // mask = FL|FR|FC|LFE|BL|BR = 0x3F
        // 1 frame: [0.2, 0.4, 0.6, 1.0, 0.3, 0.5]
        // LFE (index 3, value 1.0) 被排除
        // active = [FL, FR, FC, BL, BR] = 5 channels
        // mono = (0.2 + 0.4 + 0.6 + 0.3 + 0.5) / 5 = 0.4
        let input = vec![0.2, 0.4, 0.6, 1.0, 0.3, 0.5];
        let (output, strategy) = downmix_to_mono(&input, 6, Some(0x3F));
        assert_eq!(output.len(), 1);
        assert!(
            (output[0] - 0.4).abs() < 1e-6,
            "expected 0.4, got {}",
            output[0]
        );
        assert_eq!(strategy, DownmixStrategy::ChannelMask);
    }

    #[test]
    fn downmix_empty_input() {
        let (output, strategy) = downmix_to_mono(&[], 2, None);
        assert!(output.is_empty());
        assert_eq!(strategy, DownmixStrategy::SimpleAverage);
    }

    #[test]
    fn downmix_incomplete_frame_dropped() {
        // 3 channels, 4 samples = 1 complete frame + 1 leftover
        let input = vec![0.1, 0.2, 0.3, 0.4];
        let (output, _) = downmix_to_mono(&input, 3, None);
        assert_eq!(output.len(), 1);
        assert!((output[0] - 0.2).abs() < 1e-6); // (0.1+0.2+0.3)/3
    }

    #[test]
    fn downmix_unknown_mask_bit_falls_back() {
        // mask with only unknown bits → fallback
        let input = vec![0.2, 0.4];
        let (output, strategy) = downmix_to_mono(&input, 2, Some(0x8000));
        assert_eq!(output.len(), 1);
        assert!((output[0] - 0.3).abs() < 1e-6);
        assert_eq!(strategy, DownmixStrategy::FallbackAverage);
    }

    #[test]
    fn downmix_zero_channels() {
        let (output, strategy) = downmix_to_mono(&[0.5], 0, None);
        assert!(output.is_empty());
        assert_eq!(strategy, DownmixStrategy::Identity);
    }

    #[test]
    fn downmix_32_channels_can_consume_full_mask() {
        let input = vec![1.0; 32];
        let (output, strategy) = downmix_to_mono(&input, 32, Some(0x7ff));
        assert_eq!(output, vec![1.0]);
        assert_eq!(strategy, DownmixStrategy::ChannelMask);
    }

    #[test]
    fn downmix_mask_falls_back_when_channels_exceed_mask_width() {
        for channels in [33_u16, 64, 256] {
            let input: Vec<f32> = (0..channels).map(|i| i as f32).collect();
            let expected = (channels as f32 - 1.0) / 2.0;
            let (output, strategy) = downmix_to_mono(&input, channels, Some(u32::MAX));
            assert_eq!(output.len(), 1);
            assert!((output[0] - expected).abs() < 1e-5);
            assert_eq!(strategy, DownmixStrategy::FallbackAverage);
        }
    }

    #[test]
    fn downmix_zero_mask_falls_back_for_32_channels() {
        let input = vec![0.25; 32];
        let (output, strategy) = downmix_to_mono(&input, 32, Some(0));
        assert_eq!(output, vec![0.25]);
        assert_eq!(strategy, DownmixStrategy::FallbackAverage);
    }

    // ── StreamingResampler: identity ─────────────────────────────────────

    #[test]
    fn resampler_identity_no_change() {
        let mut r = StreamingResampler::new(16000, 16000);
        let input = vec![0.5, 0.3, -0.2, 0.8];
        let output = r.process(&input);
        assert_eq!(output, input);
    }

    #[test]
    fn resampler_identity_empty() {
        let mut r = StreamingResampler::new(16000, 16000);
        let output = r.process(&[]);
        assert!(output.is_empty());
    }

    #[test]
    fn resampler_identity_single_sample() {
        let mut r = StreamingResampler::new(16000, 16000);
        let output = r.process(&[0.42]);
        assert_eq!(output, vec![0.42]);
    }

    #[test]
    fn resampler_identity_preserves_amplitude() {
        let mut r = StreamingResampler::new(16000, 16000);
        let output = r.process(&[1.0, -1.0, 0.5, -0.5]);
        assert_eq!(output, vec![1.0, -1.0, 0.5, -0.5]);
    }

    // ── StreamingResampler: downsample ──────────────────────────────────

    #[test]
    fn resample_48k_to_16k_whole_input() {
        // 48000 → 16000, ratio = 1/3
        // 48 samples → 16 samples
        let mut r = StreamingResampler::new(48000, 16000);
        let input: Vec<f32> = (0..48).map(|i| i as f32 * 0.01).collect();
        let output = r.process(&input);
        assert_eq!(output.len(), 16);
        // 3:1 下采样，j=0 对应 src 0, j=1 对应 src 3, ...
        for j in 0..16 {
            let expected = input[j * 3];
            assert!(
                (output[j] - expected).abs() < 1e-10,
                "j={}: expected {}, got {}",
                j,
                expected,
                output[j]
            );
        }
    }

    #[test]
    fn resample_48k_to_16k_chunked_eq_whole() {
        // 整段输入与随机合法切块输入 frame 数一致，逐样本在明确 epsilon 内等价
        let mut r_whole = StreamingResampler::new(48000, 16000);
        let mut r_chunked = StreamingResampler::new(48000, 16000);

        let input: Vec<f32> = (0..480).map(|i| (i as f32 * 0.02).sin()).collect();
        let whole_output = r_whole.process(&input);
        let whole_final = r_whole.finish();
        let total_whole = combine(&whole_output, &whole_final);

        // 随机切块
        let chunk_sizes = [7, 13, 1, 100, 3, 48, 200, 50, 58];
        let mut offset = 0;
        let mut chunked_process = Vec::new();
        for &cs in &chunk_sizes {
            let end = (offset + cs).min(input.len());
            let chunk = &input[offset..end];
            chunked_process.extend(r_chunked.process(chunk));
            offset = end;
        }
        // 剩余
        if offset < input.len() {
            chunked_process.extend(r_chunked.process(&input[offset..]));
        }
        let chunked_final = r_chunked.finish();
        let total_chunked = combine(&chunked_process, &chunked_final);

        assert_eq!(
            total_whole.len(),
            total_chunked.len(),
            "frame count mismatch: whole={} chunked={}",
            total_whole.len(),
            total_chunked.len()
        );
        for (i, (a, b)) in total_whole.iter().zip(total_chunked.iter()).enumerate() {
            assert!(
                (a - b).abs() < 1e-6,
                "sample {i} mismatch: whole={a} chunked={b}"
            );
        }
    }

    fn combine(a: &[f32], b: &[f32]) -> Vec<f32> {
        let mut v = a.to_vec();
        v.extend_from_slice(b);
        v
    }

    #[test]
    fn resample_48k_stereo_to_16k_mono_duration_not_amplified() {
        // 48k stereo → 16k mono
        // 480 frames stereo @ 48k = 10ms
        // → 160 frames mono @ 16k = 10ms
        let mut normalizer = AudioNormalizer::new(2, 48000);
        let input: Vec<f32> = (0..960).map(|i| i as f32 * 0.001).collect(); // 480 frames × 2ch
        let output = normalizer.process(&input);

        assert_eq!(output.len(), 160); // 10ms @ 16k = 160 samples
        let duration_ms = output.len() as f64 / 16.0; // 10ms
        assert!((duration_ms - 10.0).abs() < 0.01);
    }

    // ── StreamingResampler: edge cases ───────────────────────────────────

    #[test]
    fn resample_empty_input() {
        let mut r = StreamingResampler::new(48000, 16000);
        let output = r.process(&[]);
        assert!(output.is_empty());
    }

    #[test]
    fn resample_single_sample() {
        let mut r = StreamingResampler::new(48000, 16000);
        let output = r.process(&[0.5]);
        // 1 sample @ 48k → 1/3 sample @ 16k → 0 samples（向下取整）
        assert!(output.is_empty());
        // finish 后可能产出 1 个
        let tail = r.finish();
        assert_eq!(tail.len(), 0); // floor(1 * 16000 / 48000) = 0
    }

    #[test]
    fn resample_tiny_chunk() {
        // 2 samples @ 48k → floor(2 * 16/48) = floor(0.666) = 0 samples
        let mut r = StreamingResampler::new(48000, 16000);
        let output = r.process(&[0.1, 0.2]);
        assert!(output.is_empty());
        // finish: floor(2 * 16/48) = 0
        let tail = r.finish();
        assert!(tail.is_empty());
    }

    #[test]
    fn resample_3_samples_at_48k() {
        // 3 samples @ 48k → floor(3 * 16/48) = 1 sample
        let mut r = StreamingResampler::new(48000, 16000);
        let output = r.process(&[0.1, 0.2, 0.3]);
        assert_eq!(output.len(), 1);
        // j=0 → src_pos = 0 * 48000 / 16000 = 0 → idx0=0, frac=0
        assert!((output[0] - 0.1).abs() < 1e-10);
    }

    #[test]
    fn resample_long_input_no_drift() {
        // 48000 samples @ 48k → 16000 samples @ 16k (1 second)
        let mut r = StreamingResampler::new(48000, 16000);
        let input: Vec<f32> = (0..48000).map(|i| (i as f32 * 0.001).sin()).collect();
        let output = r.process(&input);
        assert_eq!(output.len(), 16000);
        let tail = r.finish();
        // 48000 * 16 / 48 = 16000 exactly, no tail
        assert!(tail.is_empty());
    }

    #[test]
    fn resample_different_chunk_sizes_same_result() {
        // 不同 chunk size 产出相同总样本数
        let input: Vec<f32> = (0..960).map(|i| (i as f32 * 0.01).sin()).collect();

        // chunk size 1
        let mut r1 = StreamingResampler::new(48000, 16000);
        let mut out1 = Vec::new();
        for s in input.chunks(1) {
            out1.extend(r1.process(s));
        }
        out1.extend(r1.finish());

        // chunk size 10
        let mut r10 = StreamingResampler::new(48000, 16000);
        let mut out10 = Vec::new();
        for s in input.chunks(10) {
            out10.extend(r10.process(s));
        }
        out10.extend(r10.finish());

        // chunk size 100
        let mut r100 = StreamingResampler::new(48000, 16000);
        let mut out100 = Vec::new();
        for s in input.chunks(100) {
            out100.extend(r100.process(s));
        }
        out100.extend(r100.finish());

        // whole
        let mut rwhole = StreamingResampler::new(48000, 16000);
        let mut out_whole = rwhole.process(&input);
        out_whole.extend(rwhole.finish());

        assert_eq!(out1.len(), out_whole.len());
        assert_eq!(out10.len(), out_whole.len());
        assert_eq!(out100.len(), out_whole.len());

        for i in 0..out_whole.len() {
            assert!(
                (out1[i] - out_whole[i]).abs() < 1e-6,
                "chunk=1 vs whole: sample {i} mismatch: {} vs {}",
                out1[i],
                out_whole[i]
            );
            assert!(
                (out10[i] - out_whole[i]).abs() < 1e-6,
                "chunk=10 vs whole: sample {i} mismatch: {} vs {}",
                out10[i],
                out_whole[i]
            );
            assert!(
                (out100[i] - out_whole[i]).abs() < 1e-6,
                "chunk=100 vs whole: sample {i} mismatch: {} vs {}",
                out100[i],
                out_whole[i]
            );
        }
    }

    // ── StreamingResampler: reset ────────────────────────────────────────

    #[test]
    fn resampler_reset_clears_phase() {
        let mut r = StreamingResampler::new(48000, 16000);
        let _ = r.process(&[0.1; 48]);
        assert!(r.read_phase > 0);
        assert!(r.write_phase > 0);

        r.reset();
        assert_eq!(r.read_phase, 0);
        assert_eq!(r.write_phase, 0);
        assert!(r.pending.is_empty());
        assert!(!r.finished);

        // After reset, should behave like fresh
        let input = vec![0.5f32; 48];
        let output = r.process(&input);
        assert_eq!(output.len(), 16);
    }

    // ── StreamingResampler: upsample ─────────────────────────────────────

    #[test]
    fn resample_16k_to_48k_upsample() {
        // 16k → 48k, ratio = 3
        // 2 个源样本 @ 16k → 上采样到 48k
        // 线性插值需要 idx0+1：j=4,5 需要 idx0+1=2 不存在
        // 所以 process 产出 4 个（j=0..3），finish 不再产出
        let mut r = StreamingResampler::new(16000, 48000);
        let input = vec![0.0, 0.5];
        let output = r.process(&input);
        assert_eq!(output.len(), 4); // j=0..3 可安全产出
        // j=0 → src 0, frac=0 → 0.0
        assert!((output[0] - 0.0).abs() < 1e-10);
        // j=3 → src 1, frac=0 → 0.5
        assert!((output[3] - 0.5).abs() < 1e-10);

        // finish 不产出（没有新数据来插值 j=4,5）
        let tail = r.finish();
        assert!(tail.is_empty());
    }

    // ── StreamingResampler: tone validation ──────────────────────────────

    #[test]
    fn resample_48k_tone_downsample_quality() {
        // 1kHz tone at 48kHz, downsample to 16kHz
        // At 16kHz, 1kHz is below Nyquist (8kHz), should be preserved
        let freq = 1000.0f32;
        let sr_in = 48000.0f32;
        let num_samples = 4800; // 100ms at 48k
        let input: Vec<f32> = (0..num_samples)
            .map(|i| (2.0 * std::f32::consts::PI * freq * (i as f32 / sr_in)).sin() * 0.5)
            .collect();

        let mut r = StreamingResampler::new(48000, 16000);
        let output = r.process(&input);

        // Expected: 1600 samples (100ms at 16k)
        assert_eq!(output.len(), 1600);

        // Check that the output has the expected frequency by verifying
        // the signal energy is similar to the input
        let input_energy: f64 =
            input.iter().map(|s| (*s as f64) * (*s as f64)).sum::<f64>() / input.len() as f64;
        let output_energy: f64 = output
            .iter()
            .map(|s| (*s as f64) * (*s as f64))
            .sum::<f64>()
            / output.len() as f64;

        // Energy should be in similar ballpark (downsample by linear interpolation
        // should preserve energy within ~30%)
        let ratio = output_energy / input_energy;
        assert!(
            ratio > 0.5 && ratio < 2.0,
            "energy ratio out of range: input={:.6} output={:.6} ratio={:.3}",
            input_energy,
            output_energy,
            ratio
        );
    }

    #[test]
    fn resample_96k_tone_downsample_quality() {
        // 1kHz tone at 96kHz, downsample to 16kHz (6:1)
        let freq = 1000.0f32;
        let sr_in = 96000.0f32;
        let num_samples = 9600; // 100ms at 96k
        let input: Vec<f32> = (0..num_samples)
            .map(|i| (2.0 * std::f32::consts::PI * freq * (i as f32 / sr_in)).sin() * 0.5)
            .collect();

        let mut r = StreamingResampler::new(96000, 16000);
        let output = r.process(&input);

        // Expected: 1600 samples (100ms at 16k)
        assert_eq!(output.len(), 1600);

        let input_energy: f64 =
            input.iter().map(|s| (*s as f64) * (*s as f64)).sum::<f64>() / input.len() as f64;
        let output_energy: f64 = output
            .iter()
            .map(|s| (*s as f64) * (*s as f64))
            .sum::<f64>()
            / output.len() as f64;

        let ratio = output_energy / input_energy;
        assert!(
            ratio > 0.3 && ratio < 3.0,
            "energy ratio out of range: input={:.6} output={:.6} ratio={:.3}",
            input_energy,
            output_energy,
            ratio
        );
    }

    // ── AudioNormalizer integration ──────────────────────────────────────

    #[test]
    fn normalizer_mono_16k_identity() {
        let mut n = AudioNormalizer::new(1, 16000);
        let input = vec![0.5, 0.3, -0.2, 0.8];
        let output = n.process(&input);
        assert_eq!(output, input);
        let tail = n.finish();
        assert!(tail.is_empty());
    }

    #[test]
    fn normalizer_stereo_48k_to_mono_16k() {
        let mut n = AudioNormalizer::new(2, 48000);
        // 480 frames × 2ch = 960 samples
        let input: Vec<f32> = (0..960).map(|i| i as f32 * 0.001).collect();
        let output = n.process(&input);
        // 480 frames @ 48k → 160 frames @ 16k
        assert_eq!(output.len(), 160);
    }

    #[test]
    fn normalizer_cross_callback_frame_buffering() {
        // 2 channels: frame size = 2
        // Feed 3 samples (1.5 frames), then 1 sample → 2 frames total
        let mut n = AudioNormalizer::new(2, 48000);
        let _ = n.process(&[0.1, 0.2, 0.3]); // 1 complete frame + 1 leftover
        let output = n.process(&[0.4]); // leftover (0.3) + new (0.4) = 1 frame
        // 2 frames @ 48k stereo → 2/3 frame @ 16k mono = floor(0.666) = 0
        // Actually: 2 frames = 2 samples mono (after downmix) @ 48k → floor(2*16/48) = 0
        assert!(output.is_empty());

        let tail = n.finish();
        // 2 mono samples @ 48k → floor(2*16/48) = 0 → no tail
        assert!(tail.is_empty());
    }

    #[test]
    fn normalizer_reset_clears_state() {
        let mut n = AudioNormalizer::new(2, 48000);
        let _ = n.process(&[0.1, 0.2, 0.3]); // incomplete frame buffered
        assert!(!n.frame_buffer.is_empty());

        n.reset();
        assert!(n.frame_buffer.is_empty());
        assert_eq!(n.resampler.read_phase, 0);
    }

    #[test]
    fn normalizer_summary_correct() {
        let n = AudioNormalizer::new(2, 48000);
        let s = n.summary();
        assert_eq!(s.source_channels, 2);
        assert_eq!(s.source_sample_rate, 48000);
        assert_eq!(s.target_sample_rate, 16000);
    }

    // ── NormalizedAudio & NormalizationSummary ───────────────────────────

    #[test]
    fn normalized_audio_struct() {
        let summary = NormalizationSummary {
            source_channels: 2,
            source_sample_rate: 48000,
            source_bits_per_sample: 16,
            source_kind: super::super::format::SampleKind::PcmSigned,
            source_channel_mask: Some(0x3),
            target_sample_rate: 16000,
            downmix_strategy: DownmixStrategy::ChannelMask,
        };
        let audio = NormalizedAudio {
            samples: vec![0.0; 1600],
            source: summary,
        };
        assert_eq!(audio.samples.len(), 1600);
        assert_eq!(audio.source.source_channels, 2);
        assert_eq!(audio.source.target_sample_rate, 16000);
    }

    #[test]
    fn normalization_summary_display() {
        let s = NormalizationSummary {
            source_channels: 2,
            source_sample_rate: 48000,
            source_bits_per_sample: 16,
            source_kind: super::super::format::SampleKind::PcmSigned,
            source_channel_mask: Some(0x3),
            target_sample_rate: 16000,
            downmix_strategy: DownmixStrategy::ChannelMask,
        };
        let display = format!("{}", s);
        assert!(display.contains("2ch"));
        assert!(display.contains("48000Hz"));
        assert!(display.contains("16000Hz"));
        assert!(display.contains("mask=0x3"));
    }

    // ── Windows callback integration (no panic, no blocking) ────────────

    #[test]
    fn normalizer_process_empty_after_finish() {
        let mut n = AudioNormalizer::new(2, 48000);
        let _ = n.process(&[0.1, 0.2, 0.3, 0.4]);
        n.finish();
        // After finish, process should be safe and return empty
        let output = n.process(&[0.5, 0.6, 0.7, 0.8]);
        assert!(output.is_empty());
    }
}
