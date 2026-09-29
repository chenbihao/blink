//! 基线：frontend/js/screenshot/scroll/stitch.js estimateVerticalShift 的忠实移植。
//!
//! 采样 SAD + 双向粗搜（step 4）+ ±3 精搜 + 方向意图 1.08 惩罚
//! + unchanged / improvement-ratio 判定。用于对照改进算法。

use super::{Estimate, Status, IMPROVEMENT_RATIO, MATCH_THRESHOLD, MAX_SHIFT_RATIO,
            REVERSE_PENALTY, SAMPLE_COLS, UNCHANGED_THRESHOLD};
use crate::frame::Frame;

pub fn sampled_sad(prev: &Frame, curr: &Frame, shift: i32, sample_rows: usize,
                   sample_cols: usize) -> f64 {
    let w = prev.w.min(curr.w);
    let h = prev.h.min(curr.h) as i32;
    let overlap = h - shift;
    if w == 0 || overlap <= 8 {
        return f64::INFINITY;
    }
    // 避开顶部吸顶栏：min(overlap/4, max(16, 18% 视口高))
    let margin_y = (overlap / 4).min(((h as f64 * 0.18).floor() as i32).max(16)) as usize;
    let usable = ((overlap as usize).saturating_sub(2 * margin_y)).max(1);
    let mut sad = 0.0f64;
    let mut samples = 0usize;
    for sy in 0..sample_rows {
        let off = ((sy as f64 + 0.5) * usable as f64 / sample_rows as f64).floor();
        let y = margin_y + (off.min(usable as f64 - 1.0) as usize);
        let prev_y = y as i32 + shift;
        for sx in 0..sample_cols {
            let x = (((sx as f64 + 0.5) * w as f64 / sample_cols as f64).floor() as usize)
                .min(w - 1);
            let (pr, pg, pb) = prev.px(x, prev_y as usize);
            let (cr, cg, cb) = curr.px(x, y);
            sad += (pr as f64 - cr as f64).abs()
                + (pg as f64 - cg as f64).abs()
                + (pb as f64 - cb as f64).abs();
            samples += 3;
        }
    }
    if samples == 0 { f64::INFINITY } else { sad / samples as f64 }
}

/// shift > 0 = 视口下移（文档上移），与 stitch.js 语义一致。
/// 负向位移按生产实现交换两帧再算 SAD。
pub fn estimate(prev: &Frame, curr: &Frame, same_score: f64) -> Estimate {
    estimate_impl(prev, curr, same_score, false)
}

/// skip 规则修正版：精搜只跳过粗搜真正测试过的点（≡1 mod 4），
/// 用来单独量化生产 skip bug 的影响。
pub fn estimate_fixed(prev: &Frame, curr: &Frame, same_score: f64) -> Estimate {
    estimate_impl(prev, curr, same_score, true)
}

fn estimate_impl(prev: &Frame, curr: &Frame, same_score: f64, fix_skip: bool) -> Estimate {
    let h = prev.h.min(curr.h) as i32;
    if same_score <= UNCHANGED_THRESHOLD {
        return Estimate { status: Status::Unchanged, shift: 0.0, score: same_score, ambiguous: false };
    }
    let max_shift = (h - 9).min((h as f64 * MAX_SHIFT_RATIO) as i32).max(1);
    let score_at = |dir: i32, dist: i32| -> f64 {
        if dir > 0 {
            sampled_sad(prev, curr, dist, 24, SAMPLE_COLS)
        } else {
            sampled_sad(curr, prev, dist, 24, SAMPLE_COLS)
        }
    };

    // 粗搜 step=4（模拟生产 rejectAmbiguous=false 的快路径）
    let mut best_rank = f64::INFINITY;
    let mut best_score = f64::INFINITY;
    let mut best_shift = 0i32;
    for dir in [1i32, -1] {
        for dist in (1..=max_shift).step_by(4) {
            let score = score_at(dir, dist);
            let rank = if dir < 0 { score * REVERSE_PENALTY } else { score };
            if rank < best_rank {
                best_rank = rank;
                best_score = score;
                best_shift = dir * dist;
            }
        }
    }
    // 精搜 ±3。生产实现的 skip 条件（dist%4==0 && dist<=coarse）跳过的
    // 恰是粗搜从未测试过的点（粗搜网格为 1 mod 4），true shift ≡ 0 (mod 4)
    // 时精搜永远测不到真峰——spike 发现的生产 bug，faithful 保留。
    if best_shift != 0 {
        let dir = best_shift.signum();
        let coarse = best_shift.abs();
        for dist in (coarse - 3).max(1)..=(coarse + 3).min(max_shift) {
            let tested_in_coarse = if fix_skip {
                dist % 4 == 1 && dist <= coarse
            } else {
                dist % 4 == 0 && dist <= coarse
            };
            if tested_in_coarse {
                continue;
            }
            let score = score_at(dir, dist);
            let rank = if dir < 0 { score * REVERSE_PENALTY } else { score };
            if rank < best_rank {
                best_rank = rank;
                best_score = score;
                best_shift = dir * dist;
            }
        }
    }

    // 生产判定：绝对阈值 + 明显优于“没滚动”
    if best_score > MATCH_THRESHOLD || best_score >= same_score * IMPROVEMENT_RATIO {
        return Estimate::no_match(false);
    }
    Estimate { status: Status::Matched, shift: best_shift as f64, score: best_score, ambiguous: false }
}
