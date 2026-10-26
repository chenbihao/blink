//! GIF 录制预算与过载决策（0.28.7，phase §3.6「录制期」工程纯逻辑）。
//!
//! 落点说明：规划时拟在 domain，实施时移入 infra——本模块与采集 worker
//! 强耦合（工程预算参数），且 infra 不得反向依赖 domain（0.28.6 分层判例）；
//! domain `gif` 只留时间轴编辑业务。
//!
//! **有界预算**：时长/帧数/磁盘/队列四上限在录制前一次定死，不假定压缩率
//! （phase 验收「不假定压缩率」）——磁盘上限按**压缩后写入量**计，另设素材
//! 目录总量上限兜底；队列按帧数有界（帧尺寸固定，帧数 × 帧字节即字节上限）。
//!
//! **过载策略**（纯函数决策，采集 worker 执行）：
//! 1. 队列接近满 + 当前帧与上一帧完全相同 → 丢弃重复帧（展示时长由时间戳
//!    差语义自然并入后一帧，总时长不丢失）；
//! 2. 连续处于高水位 → 受控降采样（interval 放宽至多 2×，累计记录）；
//! 3. 队列满且帧非重复 / 预算任一达限 → 停止录制并保留完整素材
//!    （`StopReason::Overload` / `BudgetExhausted`），不无界排队不偷偷丢总时长。
//!
//! **有效时间**：单调时钟时间戳差；暂停段不入成品。finalize 时末帧时长取
//! `1000/fps`（与正常帧节拍一致），全部 clamp ≥ 1ms。

use serde::{Deserialize, Serialize};

/// 录制预算（全部硬上限；达限即停）。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct RecordingBudget {
    /// 最大有效录制时长（ms）。
    pub max_duration_ms: u32,
    /// 最大帧数（含被合并的重复帧）。
    pub max_frames: u32,
    /// 压缩素材累计写入上限（字节）。
    pub max_spool_bytes: u64,
    /// 待压缩队列帧数上限（帧尺寸固定 → 字节上限 = 帧数 × 帧字节）。
    pub queue_frame_cap: usize,
}

impl Default for RecordingBudget {
    fn default() -> Self {
        // 短演示预算：120s / 15fps=1800 帧 / 512MB 压缩素材 / 队列 ≤ 2s 缓冲
        // （1280×720 帧 3.5MB 时 2s ≈ 30 帧，帧更小则按字节换算后放大）。
        Self {
            max_duration_ms: 120_000,
            max_frames: 1_800,
            max_spool_bytes: 512 * 1024 * 1024,
            queue_frame_cap: 32,
        }
    }
}

/// 过载决策。
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum OverloadAction {
    /// 正常入队。
    Continue,
    /// 队列压力高但内容帧不丢：丢弃当前重复帧，间隔放宽（factor ≥ 1.0）。
    DropDuplicate { interval_factor: f32 },
    /// 队列压力高且当前帧非重复：不丢帧，只阶梯放宽采集间隔。
    Backoff { interval_factor: f32 },
    /// 无法有界处理：停止录制，保留已有素材。
    Stop,
}

/// 队列高水位线（占比）。
pub const QUEUE_HIGH_WATER: f32 = 0.75;

/// 采集间隔放宽上限。
pub const MAX_INTERVAL_FACTOR: f32 = 2.0;

/// 每次高水位回退的间隔系数。
const BACKOFF_STEP: f32 = 1.25;

/// 过载决策（纯函数，采集 worker 每帧调用）。
///
/// - `queue_len`：当前待压缩队列长度；`duplicate`：当前帧是否与上一帧完全相同。
/// - `current_interval_factor`：当前已放宽的间隔系数。
/// - 队列满（≥ cap）：重复帧可丢；非重复帧必须停（不能丢唯一内容帧）。
/// - 高水位（≥ QUEUE_HIGH_WATER）：重复帧丢弃 + 阶梯放宽间隔；非重复帧只放宽
///   间隔（内容帧不丢）。
pub fn decide_overload(
    queue_len: usize,
    cap: usize,
    duplicate: bool,
    current_interval_factor: f32,
) -> OverloadAction {
    if cap == 0 {
        return OverloadAction::Stop;
    }
    if queue_len >= cap {
        return if duplicate {
            OverloadAction::DropDuplicate {
                interval_factor: current_interval_factor.max(1.0),
            }
        } else {
            OverloadAction::Stop
        };
    }
    let ratio = queue_len as f32 / cap as f32;
    if ratio >= QUEUE_HIGH_WATER {
        if duplicate {
            return OverloadAction::DropDuplicate {
                interval_factor: current_interval_factor.max(1.0),
            };
        }
        let next = (current_interval_factor.max(1.0) * BACKOFF_STEP).min(MAX_INTERVAL_FACTOR);
        return OverloadAction::Backoff {
            interval_factor: next,
        };
    }
    OverloadAction::Continue
}

/// 停止原因（写入会话元数据，前端如实展示）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    /// 用户主动停止。
    User,
    /// 过载：队列满且帧非重复。
    Overload,
    /// 预算达限（时长/帧数/磁盘）。
    BudgetExhausted,
    /// 采集失败（屏幕变化/锁屏等）。
    CaptureFailed,
}

/// 由时间戳序列产出素材帧时长表（录制停止后排空调用时调用一次）。
///
/// - `ts_ms`：单调递增的有效时间戳（暂停段已剔除）；重复帧被丢弃时不产生
///   时间戳，时长差自然并入后一帧——总时长语义保持。
/// - 末帧时长 = `1000/fps`（无下一帧可差分）。
/// - 全部 clamp ≥ 1ms。
pub fn finalize_durations(ts_ms: &[u64], fps: u32) -> Vec<u32> {
    let fallback_last = (1000 / fps.max(1)).max(1);
    let mut out = Vec::with_capacity(ts_ms.len());
    for i in 0..ts_ms.len() {
        let dur = if i + 1 < ts_ms.len() {
            ts_ms[i + 1].saturating_sub(ts_ms[i]) as u32
        } else {
            fallback_last
        };
        out.push(dur.max(1));
    }
    out
}

// ── 测试 ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decide_overload_empty_cap_stops() {
        assert_eq!(decide_overload(0, 0, true, 1.0), OverloadAction::Stop);
    }

    #[test]
    fn decide_overload_normal_below_high_water() {
        assert_eq!(
            decide_overload(10, 32, false, 1.0),
            OverloadAction::Continue
        );
        assert_eq!(decide_overload(23, 32, true, 1.0), OverloadAction::Continue);
    }

    #[test]
    fn decide_overload_duplicate_at_high_water_dropped() {
        // 24/32 = 0.75 → 高水位
        assert_eq!(
            decide_overload(24, 32, true, 1.0),
            OverloadAction::DropDuplicate { interval_factor: 1.0 }
        );
    }

    #[test]
    fn decide_overload_unique_at_high_water_backs_off_interval() {
        assert_eq!(
            decide_overload(24, 32, false, 1.0),
            OverloadAction::Backoff { interval_factor: 1.25 }
        );
        // 阶梯累计，上限 2.0
        assert_eq!(
            decide_overload(24, 32, false, 1.5),
            OverloadAction::Backoff { interval_factor: 1.875 }
        );
        assert_eq!(
            decide_overload(24, 32, false, 1.9),
            OverloadAction::Backoff { interval_factor: 2.0 }
        );
    }

    #[test]
    fn decide_overload_unique_frame_at_full_queue_stops() {
        assert_eq!(decide_overload(32, 32, false, 1.0), OverloadAction::Stop);
        assert_eq!(decide_overload(40, 32, false, 1.0), OverloadAction::Stop);
    }

    #[test]
    fn decide_overload_duplicate_at_full_queue_dropped_not_stopped() {
        assert_eq!(
            decide_overload(32, 32, true, 1.4),
            OverloadAction::DropDuplicate { interval_factor: 1.4 }
        );
    }

    #[test]
    fn finalize_durations_diffs_timestamps_and_fills_last() {
        let ts = [0, 66, 133, 266];
        let d = finalize_durations(&ts, 15);
        assert_eq!(d, vec![66, 67, 133, 66]); // 末帧 = 1000/15
    }

    #[test]
    fn finalize_durations_dropped_duplicate_merges_into_next() {
        // 帧 A(ts=0) B(ts=66) [B 重复被丢] C(ts=266)：B 的时长并入 C
        let ts = [0, 66, 266];
        let d = finalize_durations(&ts, 15);
        assert_eq!(d, vec![66, 200, 66]);
        assert_eq!(d.iter().map(|x| *x as u64).sum::<u64>(), 332, "总时长=最后时间戳+末帧");
    }

    #[test]
    fn finalize_durations_clamps_and_handles_single() {
        assert_eq!(finalize_durations(&[50], 10), vec![100]);
        // 非法时间戳（不单调）不会 panic，clamp ≥1
        assert_eq!(finalize_durations(&[100, 100], 10), vec![1, 100]);
    }
}
