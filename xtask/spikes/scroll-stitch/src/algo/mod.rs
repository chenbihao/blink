//! 算法公共层：估算结果类型 + 每对帧的预计算上下文。

pub mod baseline;
pub mod hybrid;
pub mod phase;
pub mod robust;

use crate::frame::Frame;

// 与 frontend/js/screenshot/scroll/stitch.js 的生产默认值对齐
pub const MATCH_THRESHOLD: f64 = 22.0;
pub const UNCHANGED_THRESHOLD: f64 = 2.5;
pub const IMPROVEMENT_RATIO: f64 = 0.8;
pub const MAX_SHIFT_RATIO: f64 = 0.78;
pub const SAMPLE_COLS: usize = 28;
pub const REVERSE_PENALTY: f64 = 1.08;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Status {
    Matched,
    NoMatch,
    Unchanged,
}

#[derive(Clone, Debug)]
pub struct Estimate {
    pub status: Status,
    pub shift: f64,
    /// 与生产 Estimate 语义对齐保留；记分用 status + shift，分数仅诊断用
    #[allow(dead_code)]
    pub score: f64,
    /// NoMatch 时是否因多义（重复纹理）被拒
    pub ambiguous: bool,
}

impl Estimate {
    pub fn no_match(ambiguous: bool) -> Self {
        Estimate { status: Status::NoMatch, shift: 0.0, score: f64::INFINITY, ambiguous }
    }
}

/// 采样列坐标（与 stitch.js sampledSad 的 x 分布一致）
pub fn col_xs(w: usize) -> Vec<usize> {
    (0..SAMPLE_COLS)
        .map(|sx| {
            (((sx as f64 + 0.5) * w as f64 / SAMPLE_COLS as f64).floor() as usize).min(w - 1)
        })
        .collect()
}

/// 相邻帧对的共享预计算：
/// - same_score：shift=0 的采样 SAD（unchanged 判定 + improvement 比值）
/// - row_change[y]：同一屏幕行 y 两帧的采样平均差——固定层/死行 ≈ 0
/// - proj_*：行亮度投影（每 4px 采样一列），供相位相关
pub struct PairCtx {
    pub same_score: f64,
    pub row_change: Vec<f64>,
    pub proj_prev: Vec<f64>,
    pub proj_curr: Vec<f64>,
}

fn luma(px: (u8, u8, u8)) -> f64 {
    px.0 as f64 * 0.299 + px.1 as f64 * 0.587 + px.2 as f64 * 0.114
}

fn project(f: &Frame, xs: &[usize]) -> Vec<f64> {
    (0..f.h)
        .map(|y| xs.iter().map(|&x| luma(f.px(x, y))).sum::<f64>() / xs.len() as f64)
        .collect()
}

impl PairCtx {
    pub fn new(prev: &Frame, curr: &Frame) -> Self {
        let xs = col_xs(prev.w);
        let mut row_change = vec![0.0; prev.h];
        for (y, rc) in row_change.iter_mut().enumerate() {
            let mut sum = 0.0;
            for &x in &xs {
                let (pr, pg, pb) = prev.px(x, y);
                let (cr, cg, cb) = curr.px(x, y);
                sum += (pr as f64 - cr as f64).abs()
                    + (pg as f64 - cg as f64).abs()
                    + (pb as f64 - cb as f64).abs();
            }
            *rc = sum / (xs.len() as f64 * 3.0);
        }
        let proj_xs: Vec<usize> = (0..prev.w).step_by(4).collect();
        PairCtx {
            same_score: baseline::sampled_sad(prev, curr, 0, 24, SAMPLE_COLS),
            row_change,
            proj_prev: project(prev, &proj_xs),
            proj_curr: project(curr, &proj_xs),
        }
    }
}
