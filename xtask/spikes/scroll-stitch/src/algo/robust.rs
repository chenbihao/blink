//! 改进一：鲁棒 SAD——行级变化掩码 + 截尾均值。
//!
//! - 变化掩码（ekibun/Stitch isdiff 思想的采样版）：同一屏幕行两帧差异 ≈ 0
//!   的行是固定层（吸顶/置底/悬浮）或无信息死行，直接不参与打分。
//! - 截尾均值：对幸存行的行级 SAD 排序后丢掉最差 25%，残余局部污染
//!   （光标闪烁、部分吸顶泄漏、懒加载闪现）不再污染均值。
//!
//! 搜索保持与基线同一套判定阈值（22 / 0.8），只换打分函数，
//! 以此单独度量“打分鲁棒性”的贡献。

use super::{Estimate, PairCtx, Status, IMPROVEMENT_RATIO, MATCH_THRESHOLD, MAX_SHIFT_RATIO,
            REVERSE_PENALTY, UNCHANGED_THRESHOLD};
use crate::frame::Frame;

const ROWS: usize = 48;
const TRIM_RATIO: f64 = 0.25;
/// 行变化低于该值视为固定层/死行。注意采集噪声（±2）的行变化期望 ≈1.33，
/// 阈值必须高于噪声地板，否则纯噪声行不会被掩掉。
const CHANGE_EPS: f64 = 2.0;
const MIN_VALID_ROWS: usize = 8;
/// 与生产 DEFAULT_MIN_POSITIONED_OVERLAP_RATIO 对齐的最小重叠占比
const MIN_OVERLAP_RATIO: f64 = 0.2;
// 多义守门：与生产 relocalization 同参数
const AMBIGUITY_DISTANCE: i32 = 12;
const AMBIGUITY_RATIO: f64 = 1.12;
const AMBIGUITY_DELTA: f64 = 1.5;

/// 指定位移下，幸存行的行级 SAD 截尾均值；有效行不足返回 INF。
/// 重叠低于 20% 视口高直接 INF：极小重叠窗口比较的行太少，
/// 截尾+掩码可能把全部不匹配行删光，剩余纯噪声行刷出假低分。
pub fn robust_score(prev: &Frame, curr: &Frame, shift: i32, ctx: &PairCtx) -> f64 {
    let w = prev.w.min(curr.w);
    let h = prev.h.min(curr.h) as i32;
    let overlap = h - shift;
    if w == 0 || overlap <= 8 || (overlap as f64) < h as f64 * MIN_OVERLAP_RATIO {
        return f64::INFINITY;
    }
    // 行掩码与采样范围对基线兼容：避开 marginY，掩掉固定行
    let margin_y = (overlap / 4).min(((h as f64 * 0.18).floor() as i32).max(16)) as usize;
    let usable = ((overlap as usize).saturating_sub(2 * margin_y)).max(1);
    let xs = super::col_xs(w);
    // 采样行去重：小重叠时 48 个采样位会重复命中同一行
    let mut ys: Vec<usize> = (0..ROWS)
        .map(|sy| {
            let off = ((sy as f64 + 0.5) * usable as f64 / ROWS as f64).floor();
            margin_y + (off.min(usable as f64 - 1.0) as usize)
        })
        .collect();
    ys.sort_unstable();
    ys.dedup();
    let mut row_sads = Vec::with_capacity(ys.len());
    for y in ys {
        // 双侧固定层掩码：比较对是 (curr 行 y, prev 行 y+shift)。
        // 只查 row_change[y] 会漏掉 prev 侧的悬浮按钮——它经 y+shift 进入
        // 比较对，而该屏幕行在两帧同样位置都存在按钮，行变化同样 ≈ 0。
        let prev_y = y as i32 + shift;
        if ctx.row_change[y] <= CHANGE_EPS || ctx.row_change[prev_y as usize] <= CHANGE_EPS {
            continue;
        }
        let mut sad = 0.0f64;
        for &x in &xs {
            let (pr, pg, pb) = prev.px(x, prev_y as usize);
            let (cr, cg, cb) = curr.px(x, y);
            sad += (pr as f64 - cr as f64).abs()
                + (pg as f64 - cg as f64).abs()
                + (pb as f64 - cb as f64).abs();
        }
        row_sads.push(sad / (xs.len() as f64 * 3.0));
    }
    row_sads.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let keep = ((row_sads.len() as f64) * (1.0 - TRIM_RATIO)).ceil() as usize;
    let taken = &row_sads[..keep.min(row_sads.len())];
    // trim 之后仍需足量有效行，否则分数不具统计意义（行数不足时宁缺毋滥，
    // 交给全量验证拒绝——实测放松该下限会让混叠窗口重新污染候选选择）
    if taken.len() < MIN_VALID_ROWS {
        return f64::INFINITY;
    }
    taken.iter().sum::<f64>() / taken.len() as f64
}

/// 逐像素全搜全局最优。hybrid 把它作为 PC 峰之外的兜底候选。
pub fn best_full_search(prev: &Frame, curr: &Frame, ctx: &PairCtx,
                        max_shift: i32) -> (i32, f64) {
    let mut best = (0i32, f64::INFINITY);
    let mut best_rank = f64::INFINITY;
    for dir in [1i32, -1] {
        for dist in 1..=max_shift {
            let score = if dir > 0 {
                robust_score(prev, curr, dist, ctx)
            } else {
                robust_score(curr, prev, dist, ctx)
            };
            let rank = if dir < 0 { score * REVERSE_PENALTY } else { score };
            if rank < best_rank {
                best_rank = rank;
                best = (dir * dist, score);
            }
        }
    }
    best
}

/// 全量口径分数，用于接受判定与多义守门。行数取 48（步长 ≈ usable/48，
/// 覆盖全部行残差）：24 行采样与 16px 行网格锁相时会整帧跳过墨行，
/// 让混叠位移拿到噪声级分数（spike 实测）。
pub fn full_score(prev: &Frame, curr: &Frame, shift: i32) -> f64 {
    if shift >= 0 {
        super::baseline::sampled_sad(prev, curr, shift, 48, super::SAMPLE_COLS)
    } else {
        super::baseline::sampled_sad(curr, prev, -shift, 48, super::SAMPLE_COLS)
    }
}

pub fn estimate(prev: &Frame, curr: &Frame, ctx: &PairCtx) -> Estimate {
    if ctx.same_score <= UNCHANGED_THRESHOLD {
        return Estimate { status: Status::Unchanged, shift: 0.0, score: ctx.same_score, ambiguous: false };
    }
    let h = prev.h.min(curr.h) as i32;
    let max_shift = (h - 9).min((h as f64 * MAX_SHIFT_RATIO) as i32).max(1);
    // 1) 鲁棒分选候选（掩码 + 截尾，抗固定层）
    let (best_shift, _) = best_full_search(prev, curr, ctx, max_shift);
    if best_shift == 0 {
        return Estimate::no_match(false);
    }
    // 2) 全量分验证接受
    let full_best = full_score(prev, curr, best_shift);
    if full_best > MATCH_THRESHOLD || full_best >= ctx.same_score * IMPROVEMENT_RATIO {
        return Estimate::no_match(false);
    }
    // 3) 多义守门：远距候选的全量分接近 → 拒绝
    for dist in 1..=max_shift {
        for dir in [1i32, -1] {
            let shift = dir * dist;
            if (shift - best_shift).abs() < AMBIGUITY_DISTANCE || shift == 0 {
                continue;
            }
            if full_score(prev, curr, shift) <= full_best * AMBIGUITY_RATIO + AMBIGUITY_DELTA {
                return Estimate::no_match(true);
            }
        }
    }
    Estimate { status: Status::Matched, shift: best_shift as f64, score: full_best, ambiguous: false }
}
