//! 改进三：混合——掩码相位相关全局召回 + 鲁棒 SAD 局部精修 + 全量分验证。
//!
//! 对应生产“粗召回 + 全分辨率确认”的两段式哲学：
//! 1. 掩码 PC 给出前 4 个正向候选峰，并入鲁棒 SAD 全局最优
//!    （PC 在准周期投影上会混叠，真峰可能不在峰列表里；且 PC 天然
//!    覆盖 > 0.78H 的大步进）；
//! 2. 每个候选 ±4px 内用鲁棒 SAD（变化掩码 + 截尾）精修；
//! 3. 接受判定与多义守门回到全量 SAD 口径：候选选择容忍固定层，
//!    验证保持严格。

use super::{phase, robust, Estimate, PairCtx, Status, UNCHANGED_THRESHOLD};
use crate::frame::Frame;

pub const MATCH_THRESHOLD: f64 = 22.0;
pub const IMPROVEMENT_RATIO: f64 = 0.8;
pub const AMBIGUITY_DISTANCE: i32 = 12;
pub const AMBIGUITY_RATIO: f64 = 1.12;
pub const AMBIGUITY_DELTA: f64 = 1.5;
const PEAK_FLOOR: f64 = 0.02;
const REFINE_RANGE: i32 = 4;
const CHANGE_EPS: f64 = 1.0;

pub fn estimate(prev: &Frame, curr: &Frame, ctx: &PairCtx) -> Estimate {
    if ctx.same_score <= UNCHANGED_THRESHOLD {
        return Estimate { status: Status::Unchanged, shift: 0.0, score: ctx.same_score, ambiguous: false };
    }
    let row_mask: Vec<bool> = ctx.row_change.iter().map(|c| *c > CHANGE_EPS).collect();
    let pc = phase::phase_correlate(&ctx.proj_prev, &ctx.proj_curr, Some(&row_mask));
    if pc.peak < PEAK_FLOOR {
        return Estimate::no_match(false);
    }

    // 候选 = PC 峰 ∪ 鲁棒 SAD 全局最优
    let h = prev.h.min(curr.h) as i32;
    let mut cands: Vec<i32> = pc.peaks.iter().take(4).map(|(s, _)| *s).filter(|s| *s > 0).collect();
    let (sad_best, _) = robust::best_full_search(prev, curr, ctx, (h - 9).max(1));
    if sad_best > 0 && !cands.contains(&sad_best) {
        cands.push(sad_best);
    }

    // 候选峰局部精修（鲁棒分）
    let mut refined: Vec<(i32, f64)> = Vec::new();
    for cand in cands {
        let mut best = (f64::INFINITY, cand);
        for d in -REFINE_RANGE..=REFINE_RANGE {
            let s = cand + d;
            if s <= 0 {
                continue;
            }
            let score = robust::robust_score(prev, curr, s, ctx);
            if score < best.0 {
                best = (score, s);
            }
        }
        refined.push((best.1, best.0));
    }
    refined.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap());
    let Some(&(best_shift, _)) = refined.first() else {
        return Estimate::no_match(false);
    };

    // 全量分验证（与基线同口径）
    let full_best = robust::full_score(prev, curr, best_shift);
    if full_best > MATCH_THRESHOLD || full_best >= ctx.same_score * IMPROVEMENT_RATIO {
        return Estimate::no_match(false);
    }
    // 多义守门：远距精修候选 / 全量分接近 → 拒绝
    for &(shift, _) in refined.iter().skip(1) {
        if (shift - best_shift).abs() >= AMBIGUITY_DISTANCE
            && robust::full_score(prev, curr, shift) <= full_best * AMBIGUITY_RATIO + AMBIGUITY_DELTA
        {
            return Estimate::no_match(true);
        }
    }
    Estimate { status: Status::Matched, shift: best_shift as f64, score: full_best, ambiguous: false }
}
