//! 录屏每路独立的连续采样游标。设备时间只校准时钟，不逐块 seek 波形。
use super::audio::{RATE, levels};
use std::collections::VecDeque;

pub struct AudioBlock {
    pub start: u64,
    pub samples: Vec<i16>,
    pub level: (f32, f32),
}

pub struct CaptureSignal {
    channels: usize,
    rate: u32,
    next: i64,
    phase: f64,
    step: f64,
    previous: Option<[f32; 2]>,
    expected_stamp: Option<f64>,
    pending: VecDeque<(i64, [f32; 2])>,
    written_end: u64,
}

impl CaptureSignal {
    pub fn new(channels: u16, rate: u32) -> Self {
        Self {
            channels: channels as usize,
            rate,
            next: 0,
            phase: 0.0,
            step: rate as f64 / RATE as f64,
            previous: None,
            expected_stamp: None,
            pending: VecDeque::new(),
            written_end: 0,
        }
    }

    /// observed 是固定设备时钟映射下的块首（48kHz 帧单位）。
    /// 小幅时间噪声只缓慢调节重采样比例；真实断流保留时间间隔。
    pub fn push(&mut self, input: &[f32], observed: f64) -> Result<Option<i64>, &'static str> {
        let count = input.len() / self.channels;
        if count == 0 || count > self.rate as usize || !observed.is_finite() {
            return Err("声音回调超过缓冲预算");
        }
        let discontinuity = self.expected_stamp.map(|at| observed - at);
        // 2ms 容忍设备时间戳噪声；丢失整块及 loopback 无包期仍有独立时间位置。
        let reset = self.expected_stamp.is_none() || discontinuity.is_some_and(|d| d.abs() > 96.0);
        if reset {
            let at = observed.round() as i64;
            self.next = if self.expected_stamp.is_none() {
                at
            } else {
                self.next.max(at)
            };
            self.phase = (self.next as f64 - observed.round()) * self.rate as f64 / RATE as f64;
            self.previous = None;
            self.step = self.rate as f64 / RATE as f64;
        } else {
            // 两秒量级的缓慢校准，最大 1000ppm，避免长录制设备晶振漂移累积，
            // 也避免把亚毫秒时间戳抖动变成缺样/重写或逐块跳相位。
            let predicted = self.next as f64 - self.phase * RATE as f64 / self.rate as f64;
            let correction = ((observed - predicted) / (RATE as f64 * 2.0)).clamp(-0.001, 0.001);
            self.step = self.rate as f64 / RATE as f64 * (1.0 - correction);
        }
        self.expected_stamp = Some(observed + count as f64 * RATE as f64 / self.rate as f64);
        let frame = |n: usize| -> [f32; 2] {
            let values = &input[n * self.channels..(n + 1) * self.channels];
            if self.channels > 2 {
                let mono = values.iter().sum::<f32>() / self.channels as f32;
                [mono, mono]
            } else {
                [values[0], values[self.channels - 1]]
            }
        };
        loop {
            let a = self.phase.floor() as i64;
            let fraction = self.phase - a as f64;
            if a >= count as i64 || (fraction > 1e-9 && a + 1 >= count as i64) {
                break;
            }
            let left = if a < 0 {
                self.previous.unwrap_or_else(|| frame(0))
            } else {
                frame(a as usize)
            };
            let right = if fraction <= 1e-9 {
                left
            } else {
                frame((a + 1).max(0) as usize)
            };
            let value = std::array::from_fn(|c| left[c] + (right[c] - left[c]) * fraction as f32);
            self.pending.push_back((self.next, value));
            self.next += 1;
            self.phase += self.step;
            if self.pending.len() > RATE as usize + RATE as usize / 1000 + 8 {
                return Err("声音时钟超出缓冲预算，已暂停全部来源");
            }
        }
        self.phase -= count as f64;
        self.previous = Some(frame(count - 1));
        Ok(discontinuity
            .filter(|d| d.abs() > 96.0)
            .map(|d| d.round() as i64))
    }

    /// 输出上界以外的少量样本留待下轮，不能逐回调截断后永久丢失。
    /// 静音在排队写盘前按样本时间执行，迟到块和隐私边界保持相同规则。
    pub fn drain(
        &mut self,
        lower: u64,
        upper: u64,
        muted: impl Fn(u64) -> bool,
    ) -> Vec<AudioBlock> {
        let mut blocks: Vec<AudioBlock> = Vec::new();
        let mut meter = Vec::new();
        while let Some(&(at, value)) = self.pending.front() {
            if at >= 0 && at as u64 >= upper {
                break;
            }
            self.pending.pop_front();
            if at < 0 || (at as u64) < lower.max(self.written_end) {
                continue;
            }
            let at = at as u64;
            if blocks.last().is_none_or(|b| {
                b.start + b.samples.len() as u64 / 2 != at || b.samples.len() >= RATE as usize * 2
            }) {
                if let Some(last) = blocks.last_mut() {
                    last.level = levels(&meter);
                }
                meter.clear();
                blocks.push(AudioBlock {
                    start: at,
                    samples: Vec::new(),
                    level: (0.0, 0.0),
                });
            }
            let silence = muted(at / 48);
            let block = blocks.last_mut().unwrap();
            for v in value {
                let v = if silence || !v.is_finite() {
                    0.0
                } else {
                    v.clamp(-1.0, 1.0)
                };
                meter.push(v);
                block.samples.push((v * 32767.0).round() as i16);
            }
            self.written_end = at + 1;
        }
        if let Some(last) = blocks.last_mut() {
            last.level = levels(&meter);
        }
        blocks
    }

    pub fn finish(&mut self) {
        if let Some(value) = self.previous {
            // 上采样最后一个原生样本不足一个目标采样周期时，保持端点至块尾。
            while self.phase < -1e-9 {
                self.pending.push_back((self.next, value));
                self.next += 1;
                self.phase += self.step;
            }
        }
    }
}

#[cfg(test)]
#[path = "audio_capture_tests.rs"]
mod tests;
