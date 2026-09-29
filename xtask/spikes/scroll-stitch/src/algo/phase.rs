//! 改进二：1D 行投影相位相关（FFT）。
//!
//! 每帧投影为逐行平均亮度信号（H 长度），频域归一化互功率谱找全局平移峰：
//! - O(H log H) 用全部行而非采样；
//! - 主峰/次峰比 = 重复纹理的原则化多义度量（替代启发式 ambiguityRatio）；
//! - 抛物线插值给出亚像素位移；
//! - 变化掩码版（mask=true）把固定行（row_change≈0）置零后再相关，
//!   消除固定层在 shift=0 处的强假峰。

use super::{Estimate, PairCtx, Status, UNCHANGED_THRESHOLD};
use crate::frame::Frame;

/// 主峰相关峰度过低 → 信号无纹理，拒绝
const PEAK_FLOOR: f64 = 0.02;
/// 次峰/主峰 超过该值 → 多义，拒绝
const RATIO_CEILING: f64 = 0.75;
/// 判定为同一峰的最小索引距离
const PEAK_SEPARATION: usize = 8;
/// 行变化低于该值的行视为固定层（与 robust.rs 一致）
const CHANGE_EPS: f64 = 1.0;

pub struct PhaseOut {
    /// 峰值位移（带符号，亚像素）
    pub shift: f64,
    /// 归一化相关峰强度（∈ [0,1]）
    pub peak: f64,
    /// 正向候选峰中 次峰/主峰
    pub ratio: f64,
    /// 正向（shift>0）候选峰列表：(整数位移, 强度)，按强度降序
    pub peaks: Vec<(i32, f64)>,
}

// ── 迭代 radix-2 FFT（实信号当复信号算，长度为 2 的幂）──

fn fft(n: usize, re: &mut [f64], im: &mut [f64], inverse: bool) {
    debug_assert!(n.is_power_of_two() && re.len() == n && im.len() == n);
    // 位反转置换
    let mut j = 0usize;
    for i in 0..n {
        if i < j {
            re.swap(i, j);
            im.swap(i, j);
        }
        let mut m = n >> 1;
        while m >= 1 && j & m != 0 {
            j &= !m;
            m >>= 1;
        }
        j |= m;
    }
    let mut len = 2usize;
    while len <= n {
        let ang = if inverse {
            2.0 * std::f64::consts::PI / len as f64
        } else {
            -2.0 * std::f64::consts::PI / len as f64
        };
        let (wr0, wi0) = (ang.cos(), ang.sin());
        for base in (0..n).step_by(len) {
            let (mut wr, mut wi) = (1.0f64, 0.0f64);
            for k in 0..len / 2 {
                let i0 = base + k;
                let i1 = i0 + len / 2;
                let tr = re[i1] * wr - im[i1] * wi;
                let ti = re[i1] * wi + im[i1] * wr;
                re[i1] = re[i0] - tr;
                im[i1] = im[i0] - ti;
                re[i0] += tr;
                im[i0] += ti;
                let nwr = wr * wr0 - wi * wi0;
                wi = wr * wi0 + wi * wr0;
                wr = nwr;
            }
        }
        len <<= 1;
    }
    if inverse {
        for k in 0..n {
            re[k] /= n as f64;
            im[k] /= n as f64;
        }
    }
}

fn next_pow2(n: usize) -> usize {
    let mut m = 1usize;
    while m < n {
        m <<= 1;
    }
    m
}

/// a/b 为行投影信号。返回全部峰（含负向），峰值按循环位移解释：
/// b(y) = a(y+s) ⟺ 峰出现在 k=s（见 main.rs 自检）。
pub fn phase_correlate(a: &[f64], b: &[f64], mask: Option<&[bool]>) -> PhaseOut {
    let n = a.len();
    let n2 = next_pow2(n * 2); // 补零到 ≥ 2n，把循环相关近似为线性相关
    let mut ar = vec![0.0f64; n2];
    let mut ai = vec![0.0f64; n2];
    let mut br = vec![0.0f64; n2];
    let mut bi = vec![0.0f64; n2];

    let prep = |sig: &[f64], out: &mut Vec<f64>| {
        let mean = sig.iter().sum::<f64>() / sig.len() as f64;
        for (i, v) in sig.iter().enumerate() {
            let keep = mask.map_or(true, |m| m[i]);
            let window =
                0.5 * (1.0 - (2.0 * std::f64::consts::PI * i as f64 / (n - 1) as f64).cos());
            out[i] = if keep { (v - mean) * window } else { 0.0 };
        }
    };
    prep(a, &mut ar);
    prep(b, &mut br);

    fft(n2, &mut ar, &mut ai, false);
    fft(n2, &mut br, &mut bi, false);

    // R = A·conj(B) / |A·conj(B)|
    let mut rr = vec![0.0f64; n2];
    let mut ri = vec![0.0f64; n2];
    for k in 0..n2 {
        let cr = ar[k] * br[k] + ai[k] * bi[k];
        let ci = ai[k] * br[k] - ar[k] * bi[k];
        let mag = (cr * cr + ci * ci).sqrt();
        if mag > 1e-12 {
            rr[k] = cr / mag;
            ri[k] = ci / mag;
        }
    }
    fft(n2, &mut rr, &mut ri, true);

    // 峰列表：|r[k]| 循环 → 带符号位移，限 |shift| < n
    let mut all: Vec<(i32, f64)> = (0..n2)
        .map(|k| {
            let mag = (rr[k] * rr[k] + ri[k] * ri[k]).sqrt();
            let signed = if k > n2 / 2 { k as i32 - n2 as i32 } else { k as i32 };
            (signed, mag)
        })
        .filter(|(s, _)| s.abs() < n as i32)
        .collect();
    all.sort_by(|x, y| y.1.partial_cmp(&x.1).unwrap());

    // 去重（同峰邻域只留最高）后取正向候选
    let mut distinct: Vec<(i32, f64)> = Vec::new();
    for (s, m) in &all {
        if distinct
            .iter()
            .all(|(ds, _)| (*ds - *s).abs() >= PEAK_SEPARATION as i32)
        {
            distinct.push((*s, *m));
        }
    }
    let positives: Vec<(i32, f64)> = distinct.iter().filter(|(s, _)| *s > 0).cloned().collect();

    let main = all.first().copied().unwrap_or((0, 0.0));
    let main_shift = positives.first().copied().unwrap_or((0, 0.0));
    let ratio = match positives.first().and_then(|_| positives.get(1)) {
        Some((_, second)) => second / main_shift.1.max(1e-12),
        None => 0.0,
    };

    // 抛物线亚像素插值（在 IFFT 幅度上）
    let k = ((main_shift.0 + n2 as i32) % n2 as i32) as usize;
    let mag_at = |idx: usize| {
        let idx = idx % n2;
        (rr[idx] * rr[idx] + ri[idx] * ri[idx]).sqrt()
    };
    let (y1, y2, y3) = (mag_at(k + n2 - 1), mag_at(k), mag_at(k + 1));
    let denom = y1 - 2.0 * y2 + y3;
    let delta = if denom.abs() > 1e-12 {
        (0.5 * (y1 - y3) / denom).clamp(-1.0, 1.0)
    } else {
        0.0
    };

    PhaseOut { shift: main_shift.0 as f64 + delta, peak: main.1, ratio, peaks: positives }
}

pub fn estimate(_prev: &Frame, _curr: &Frame, ctx: &PairCtx, mask: bool) -> Estimate {
    if ctx.same_score <= UNCHANGED_THRESHOLD {
        return Estimate { status: Status::Unchanged, shift: 0.0, score: ctx.same_score, ambiguous: false };
    }
    let row_mask: Option<Vec<bool>> = if mask {
        Some(ctx.row_change.iter().map(|c| *c > CHANGE_EPS).collect())
    } else {
        None
    };
    let out = phase_correlate(&ctx.proj_prev, &ctx.proj_curr, row_mask.as_deref());
    if out.peak < PEAK_FLOOR {
        return Estimate::no_match(false);
    }
    if out.ratio > RATIO_CEILING || out.peaks.is_empty() {
        return Estimate::no_match(true);
    }
    Estimate {
        status: Status::Matched,
        shift: out.shift,
        // 用主峰强度换算一个 SAD 语义外的“置信分”：峰强度越低分越差
        score: (1.0 - out.peak) * 100.0,
        ambiguous: false,
    }
}
