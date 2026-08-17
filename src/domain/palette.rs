//! 0.20.7：来源无关配色核心（Rust 单一真源）。
//!
//! 纯函数模块，无 Tauri/DOM/IO 依赖。
//! 包含：OKLab/OKLCH 色彩空间转换、DeltaE 距离、确定性 k-means++ 聚类、
//! Boyer-Moore 直方图代表色、角色色分配、harmony 方案生成（OKLCH + sRGB gamut mapping）、
//! WCAG 对比度与推荐文字色、输出格式生成。
//!
//! ## 算法契约
//! - 输入：RGBA8 像素切片 + 来源元数据
//! - 输出：确定性角色色、占比、对比度、推荐文字色、推荐方案及完整 harmony
//! - 透明度 < 0.5 的像素跳过；空样本返回结构化无结果
//! - 使用 OKLab 距离、确定性 k-means++ seed；K=1..8，最大 32 次迭代
//! - 聚类中心映射到最近真实原图像素
//! - 常量集中在 `PALETTE_ALGORITHM_V1`
//!
//! **架构约束**：本模块是纯函数，不依赖 tauri/async/IO，可独立单测。
//! 与 `src/domain/color.rs`（颜色字面量解析）保持独立，不拆分。

use serde::{Deserialize, Serialize};

// ── 常量 ─────────────────────────────────────────────────────────────────

/// 算法常量（唯一真源，禁止散落 UI）。
/// 变更阈值必须更新 fixture 说明。
pub const PALETTE_ALGORITHM_V1: PaletteConstants = PaletteConstants {
    min_alpha: 0.5,
    k_min: 3,
    k_max: 8,
    max_iterations: 32,
    convergence_epsilon: 0.001,
    near_color_merge_delta_e: 0.05,
    accent_min_ratio: 0.02,
    background_min_ratio: 0.35,
    // 智能搭配硬约束
    smart_text_min_contrast: 4.5,
    smart_accent_min_contrast: 3.0,
    smart_min_candidate_ratio: 0.001,
    smart_max_candidates: 30,
    smart_pair_distance_bonus: 3.0,
    smart_chroma_bonus: 4.0,
    smart_degraded_confidence: 0.5,
    // 显著色（视觉焦点色）提取启发式
    salient_area_bias: 0.45,
    salient_area_exponent: 0.15,
    salient_chroma_min: 0.035,
    salient_bg_distance_min: 0.055,
    salient_min_count_factor: 0.000002,
    salient_smooth_weight: 0.3,
    salient_chroma_gain: 4.0,
    salient_chroma_bias: 0.25,
    salient_contrast_gain: 2.5,
    salient_contrast_bias: 0.35,
    salient_rep_area_exponent: 0.12,
    salient_peak_min_distance: 1,
    salient_peak_suppress_ratio: 0.6,
    salient_max_colors: 9,
    salient_diversity_min_distance: 0.04,
    balanced_salient_take: 6,
    // Mean-shift 聚类参数（OKLab 带权模式查找）
    mean_shift: MeanShiftParams {
        bandwidth: 0.06,
        min_ratio: 0.00001,
        max_iters: 12,
        convergence: 0.0005,
        seed_cap: 96,
        window_cap: 1024,
    },
    // MMCQ 聚类参数（中值切分量化）
    mmcq: MmcqParams { target_colors: 8 },
};

#[derive(Debug, Clone, Copy)]
pub struct PaletteConstants {
    /// alpha < 0.5 的像素跳过
    pub min_alpha: f64,
    /// 最小聚类数
    pub k_min: usize,
    /// 最大聚类数
    pub k_max: usize,
    /// k-means 最大迭代
    pub max_iterations: usize,
    /// 中心位移小于此值时提前停止
    pub convergence_epsilon: f64,
    /// 近色合并阈值（OKLab 欧氏距离）
    pub near_color_merge_delta_e: f64,
    /// 点缀最小占比
    pub accent_min_ratio: f64,
    /// 背景最小占比
    pub background_min_ratio: f64,
    /// 智能搭配：正文对背景的 WCAG 最小对比度
    pub smart_text_min_contrast: f64,
    /// 智能搭配：强调色对背景的 WCAG 最小对比度
    pub smart_accent_min_contrast: f64,
    /// 智能搭配：候选池最小像素占比
    pub smart_min_candidate_ratio: f64,
    /// 智能搭配：候选池最大条目数
    pub smart_max_candidates: usize,
    /// 智能搭配：两两距离评分系数
    pub smart_pair_distance_bonus: f64,
    /// 智能搭配：色度评分系数
    pub smart_chroma_bonus: f64,
    /// 智能搭配：降级时的置信度
    pub smart_degraded_confidence: f64,
    /// 显著色：扇区能量面积项偏置（面积占比低时能量下限，压低面积主导）
    pub salient_area_bias: f64,
    /// 显著色：扇区能量面积项指数（越小对面积越不敏感）
    pub salient_area_exponent: f64,
    /// 显著色：候选色度阈值
    pub salient_chroma_min: f64,
    /// 显著色：背景邻近过滤阈值（仅面积主导背景时生效）
    pub salient_bg_distance_min: f64,
    /// 显著色：扇区最小像素数系数（total × 此值，至少 2）
    pub salient_min_count_factor: f64,
    /// 显著色：扇区能量平滑权重
    pub salient_smooth_weight: f64,
    /// 显著色：能量色度增益
    pub salient_chroma_gain: f64,
    /// 显著色：能量色度偏置
    pub salient_chroma_bias: f64,
    /// 显著色：能量反差增益
    pub salient_contrast_gain: f64,
    /// 显著色：能量反差偏置
    pub salient_contrast_bias: f64,
    /// 显著色：代表色评分面积项指数
    pub salient_rep_area_exponent: f64,
    /// 显著色：相邻扇区峰值最小距离（扇区数）
    pub salient_peak_min_distance: usize,
    /// 显著色：相邻扇区峰值保留的最小能量比（能量相对化 NMS，低于此比值才抑制）
    pub salient_peak_suppress_ratio: f64,
    /// 显著色：最大颜色数
    pub salient_max_colors: usize,
    /// 显著色：多样选色最小 OKLab 距离
    pub salient_diversity_min_distance: f64,
    /// 均衡配色：从显著色取用数量
    pub balanced_salient_take: usize,
    /// Mean-shift 聚类参数
    pub mean_shift: MeanShiftParams,
    /// MMCQ 聚类参数
    pub mmcq: MmcqParams,
}

/// Mean-shift 聚类参数
#[derive(Debug, Clone, Copy)]
pub struct MeanShiftParams {
    /// OKLab 距离带宽（窗口半径）
    pub bandwidth: f64,
    /// 模式最小像素占比，低于则丢弃
    pub min_ratio: f64,
    /// 单种子最大迭代次数
    pub max_iters: usize,
    /// 质心位移小于此值时视为收敛
    pub convergence: f64,
    /// 种子数上限（性能：最远点采样覆盖不同色组）
    pub seed_cap: usize,
    /// 质心窗口大小上限（count 降序取前 N 作为带权质心参考，控每次扫描成本）
    pub window_cap: usize,
}

/// MMCQ（Modified Median Cut）聚类参数
#[derive(Debug, Clone, Copy)]
pub struct MmcqParams {
    /// 目标色数（中值切分的盒数）
    pub target_colors: usize,
}

// ── 类型定义 ─────────────────────────────────────────────────────────────

/// OKLab 色彩空间 [L, a, b]
pub type OkLab = [f64; 3];

/// OKLCH 色彩空间 [L, C, H]（H 为角度 0..360）
pub type OkLch = [f64; 3];

/// RGB 三元组 [r, g, b]（0-255）
pub type Rgb = [u8; 3];

/// 聚类结果
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClusterResult {
    pub rgb: Rgb,
    pub oklab: OkLab,
    pub count: usize,
    pub ratio: f64,
}

/// 可选的聚类算法（前端截图面板切换；默认 kmeans 保持既有行为）。
/// serde snake_case：`kmeans` / `mean_shift` / `mmcq`，与前端取值对齐。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ClusteringAlgo {
    /// OKLab 加权 k-means（既有默认）
    #[default]
    Kmeans,
    /// Mean-shift 密度聚类（自动定簇数）
    MeanShift,
    /// MMCQ 中值切分量化（确定性直方图分盒）
    Mmcq,
}

/// 角色色
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RoleColor {
    pub rgb: Rgb,
    pub role: String,
    pub ratio: f64,
    pub oklab: OkLab,
    pub hex: String,
}

/// Harmony 配色方案
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HarmonyScheme {
    pub label: String,
    pub scheme: String,
    pub description: String,
    pub colors: Vec<Rgb>,
    /// 来源标记："extraction" = 原图真实像素；"generated" = OKLCH 数学变换生成
    pub source_kind: String,
    /// 置信度 0..=1；extraction 固定 1.0，generated 固定 0.8，降级方案 0.5
    pub confidence: f64,
}

/// 对比度信息
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContrastInfo {
    pub ratio: f64,
    pub text_color: String, // "dark" | "light"
}

/// 配色分析结果
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaletteResult {
    pub roles: Vec<RoleColor>,
    pub theme: ThemeAnalysis,
    pub sample: SampleInfo,
    pub recommended: Vec<HarmonyScheme>,
    #[serde(default)]
    pub full: Vec<HarmonyScheme>,
    pub empty: bool,
}

/// 主题分析
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThemeAnalysis {
    pub family: String,
    pub temperature: String,
    pub saturation: String,
    pub lightness: String,
    pub hue_concentration: f64,
    pub summary: String,
}

/// 采样信息
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SampleInfo {
    pub width: usize,
    pub height: usize,
    pub valid_pixels: usize,
    pub scanned_pixels: usize,
    pub mode: String,
}

// ── sRGB ↔ 线性 RGB ─────────────────────────────────────────────────────

/// sRGB 通道值 → 线性 RGB（用于 OKLab 转换）
#[inline]
fn srgb_to_linear(c: f64) -> f64 {
    let cs = c / 255.0;
    if cs <= 0.04045 {
        cs / 12.92
    } else {
        ((cs + 0.055) / 1.055).powf(2.4)
    }
}

/// 线性 RGB → sRGB 通道值 (0-255)
#[inline]
fn linear_to_srgb(c: f64) -> u8 {
    // 负线性值和接近 0 的噪声钳制为 0
    if c <= 0.0001 {
        return 0;
    }
    if c >= 1.0 {
        return 255;
    }
    let cs = if c <= 0.0031308 {
        c * 12.92
    } else {
        1.055 * c.powf(1.0 / 2.4) - 0.055
    };
    round_half_away(cs * 255.0).clamp(0.0, 255.0) as u8
}

// ── RGB → OKLab ──────────────────────────────────────────────────────────

/// sRGB (0-255) → OKLab [L, a, b]
pub fn rgb_to_oklab(r: u8, g: u8, b: u8) -> OkLab {
    let lr = srgb_to_linear(r as f64);
    let lg = srgb_to_linear(g as f64);
    let lb = srgb_to_linear(b as f64);

    // 线性 sRGB → LMS（Ottosson canonical linear-sRGB→LMS matrix）
    let l = 0.4122214708 * lr + 0.5363325363 * lg + 0.0514459929 * lb;
    let m = 0.2119034982 * lr + 0.6806995451 * lg + 0.1073969566 * lb;
    let s = 0.0883024619 * lr + 0.2817188376 * lg + 0.6299787005 * lb;

    // 立方根
    let l_ = l.cbrt();
    let m_ = m.cbrt();
    let s_ = s.cbrt();

    // LMS' → OKLab（M2 canonical）
    [
        0.2104542553 * l_ + 0.7936177850 * m_ - 0.0040720468 * s_,
        1.9779984951 * l_ - 2.4285922050 * m_ + 0.4505937099 * s_,
        0.0259040371 * l_ + 0.7827717662 * m_ - 0.8086757660 * s_,
    ]
}

/// OKLab → 线性 sRGB [0..1]（未钳制，供色域判定与 RGB 转换共用）
fn oklab_to_linear_rgb(lab: OkLab) -> [f64; 3] {
    let [l, a, b] = lab;

    // OKLab → LMS'（M2 逆矩阵 canonical）
    let l_ = l + 0.3963377774 * a + 0.2158037573 * b;
    let m_ = l - 0.1055613458 * a - 0.0638541728 * b;
    let s_ = l - 0.0894841775 * a - 1.2914855480 * b;

    // 立方
    let l = l_ * l_ * l_;
    let m = m_ * m_ * m_;
    let s = s_ * s_ * s_;

    // LMS → 线性 sRGB（Ottosson canonical LMS→linear-sRGB 逆矩阵）
    [
        4.0767416621 * l - 3.3077115913 * m + 0.2309699292 * s,
        -1.2684380046 * l + 2.6097574011 * m - 0.3413193965 * s,
        -0.0041960863 * l - 0.7034186147 * m + 1.7076147010 * s,
    ]
}

/// OKLab → sRGB (0-255)
pub fn oklab_to_rgb(lab: OkLab) -> Rgb {
    let [lr, lg, lb] = oklab_to_linear_rgb(lab);
    [linear_to_srgb(lr), linear_to_srgb(lg), linear_to_srgb(lb)]
}

// ── OKLab ↔ OKLCH ────────────────────────────────────────────────────────

/// OKLab → OKLCH [L, C, H]（H 为角度 0..360）
pub fn oklab_to_oklch(lab: OkLab) -> OkLch {
    let [l, a, b] = lab;
    let c = (a * a + b * b).sqrt();
    let mut h = b.atan2(a).to_degrees();
    if h < 0.0 {
        h += 360.0;
    }
    [l, c, h]
}

/// OKLCH → OKLab
pub fn oklch_to_oklab(lch: OkLch) -> OkLab {
    let [l, c, h] = lch;
    let h_rad = h.to_radians();
    [l, c * h_rad.cos(), c * h_rad.sin()]
}

// ── sRGB gamut mapping ────────────────────────────────────────────────────

/// 检查 OKLab 颜色是否在 sRGB 色域内。
/// 直接判定线性 RGB 三通道是否落入 [0,1]（精确、无阈值）。
/// 不能用 roundtrip 距离：8bit 量化会让色域内颜色（近黑区）出现 0.05 量级误差，
/// 而贴边超色域颜色反而只有 0.0003 量级误差，二者无法分离（0.20.7 起生成方案全被压成灰即此因）。
fn in_srgb_gamut(lab: OkLab) -> bool {
    let [lr, lg, lb] = oklab_to_linear_rgb(lab);
    (0.0..=1.0).contains(&lr) && (0.0..=1.0).contains(&lg) && (0.0..=1.0).contains(&lb)
}

/// 将 OKLab 颜色映射到 sRGB 色域内（二分法降色度）
fn gamut_map_oklab(lab: OkLab) -> OkLab {
    if in_srgb_gamut(lab) {
        return lab;
    }

    let [l, _, _] = lab;
    let lch = oklab_to_oklch(lab);
    let c_orig = lch[1];

    // 二分法降色度，直到落入 sRGB 色域
    let mut lo = 0.0_f64;
    let mut hi = c_orig;
    let mut best = lab;

    for _ in 0..20 {
        let mid = (lo + hi) / 2.0;
        let candidate = oklch_to_oklab([l, mid, lch[2]]);
        if in_srgb_gamut(candidate) {
            lo = mid;
            best = candidate;
        } else {
            hi = mid;
        }
    }

    // 最终确保 best 是有效的
    if !in_srgb_gamut(best) {
        // 极端情况：直接用灰线
        let gray = [l, 0.0, 0.0];
        if in_srgb_gamut(gray) {
            best = gray;
        } else {
            // l 也超出，钳制到边界
            best = oklch_to_oklab([l.clamp(0.0, 1.0), 0.0, 0.0]);
        }
    }

    best
}

/// 对 OKLCH 做 gamut mapping 后输出 sRGB
fn oklch_to_srgb_gamut_mapped(lch: OkLch) -> Rgb {
    let lab = oklch_to_oklab(lch);
    let mapped = gamut_map_oklab(lab);
    oklab_to_rgb(mapped)
}

// ── RGB ↔ HSL（用于角色色分析）──────────────────────────────────────────

/// RGB → HSL。h: 0-360, s/l: 0-100
pub fn rgb_to_hsl(r: u8, g: u8, b: u8) -> (f64, f64, f64) {
    let r_n = r as f64 / 255.0;
    let g_n = g as f64 / 255.0;
    let b_n = b as f64 / 255.0;
    let max = r_n.max(g_n).max(b_n);
    let min = r_n.min(g_n).min(b_n);
    let delta = max - min;
    let l = (max + min) / 2.0;

    let (s, h) = if delta == 0.0 {
        (0.0, 0.0)
    } else {
        let s = if l > 0.5 {
            delta / (2.0 - max - min)
        } else {
            delta / (max + min)
        };
        let h = if r_n == max {
            ((g_n - b_n) / delta).rem_euclid(6.0)
        } else if g_n == max {
            (b_n - r_n) / delta + 2.0
        } else {
            (r_n - g_n) / delta + 4.0
        };
        (s, h * 60.0)
    };

    let h = if h < 0.0 { h + 360.0 } else { h };
    (h, s * 100.0, l * 100.0)
}

// ── RGB → HEX ────────────────────────────────────────────────────────────

/// RGB → HEX 字符串（#RRGGBB，大写）
pub fn rgb_to_hex(r: u8, g: u8, b: u8) -> String {
    format!("#{:02X}{:02X}{:02X}", r, g, b)
}

// ── DeltaE（OKLab 空间欧氏距离）─────────────────────────────────────────

/// 计算两个 OKLab 颜色之间的 DeltaE 距离
pub fn delta_e(lab1: OkLab, lab2: OkLab) -> f64 {
    let dl = lab1[0] - lab2[0];
    let da = lab1[1] - lab2[1];
    let db = lab1[2] - lab2[2];
    (dl * dl + da * da + db * db).sqrt()
}

// ── WCAG 对比度 ──────────────────────────────────────────────────────────

/// 计算 sRGB 通道的相对亮度分量
#[inline]
fn relative_luminance_channel(c: u8) -> f64 {
    let cs = c as f64 / 255.0;
    if cs <= 0.03928 {
        cs / 12.92
    } else {
        ((cs + 0.055) / 1.055).powf(2.4)
    }
}

/// 计算颜色的 WCAG 相对亮度 (0-1)
pub fn relative_luminance(rgb: Rgb) -> f64 {
    0.2126 * relative_luminance_channel(rgb[0])
        + 0.7152 * relative_luminance_channel(rgb[1])
        + 0.0722 * relative_luminance_channel(rgb[2])
}

/// 计算 WCAG 2.1 对比度比值 (1.0..=21.0)
pub fn contrast_ratio(rgb1: Rgb, rgb2: Rgb) -> f64 {
    let l1 = relative_luminance(rgb1);
    let l2 = relative_luminance(rgb2);
    let lighter = l1.max(l2);
    let darker = l1.min(l2);
    (lighter + 0.05) / (darker + 0.05)
}

/// 给定背景色，推荐前景文字色（黑或白）
pub fn recommend_text_color(bg_rgb: Rgb) -> ContrastInfo {
    let black = [0, 0, 0];
    let white = [255, 255, 255];
    let black_contrast = contrast_ratio(black, bg_rgb);
    let white_contrast = contrast_ratio(white, bg_rgb);
    if black_contrast >= white_contrast {
        ContrastInfo {
            ratio: black_contrast,
            text_color: "dark".into(),
        }
    } else {
        ContrastInfo {
            ratio: white_contrast,
            text_color: "light".into(),
        }
    }
}

/// 给定前景和背景色，返回对比度和推荐文字色方向
#[allow(dead_code)] // WCAG 对比度工具，待主题系统消费
pub fn contrast_with_recommendation(fg_rgb: Rgb, bg_rgb: Rgb) -> ContrastInfo {
    let ratio = contrast_ratio(fg_rgb, bg_rgb);
    let fg_lum = relative_luminance(fg_rgb);
    ContrastInfo {
        ratio,
        text_color: if fg_lum < 0.5 { "dark" } else { "light" }.into(),
    }
}

// ── half-away-from-zero 舍入 ─────────────────────────────────────────────

#[inline]
fn round_half_away(v: f64) -> f64 {
    if v >= 0.0 {
        (v + 0.5).floor()
    } else {
        (v - 0.5).ceil()
    }
}

// ── 确定性带权 k-means 聚类 ──────────────────────────────────────────────

/// 直方图条目（带权 k-means 输入）
#[derive(Debug, Clone)]
pub struct HistogramEntry {
    pub rgb: Rgb,
    pub oklab: OkLab,
    pub chroma: f64,
    pub count: usize,
    pub ratio: f64,
}

/// 对带权真实颜色做 k-means。
/// 输入规模是量化桶数量而非原图像素数量，
/// 聚类中心最终映射回桶内真实出现过的代表 RGB。
pub fn weighted_kmeans_cluster(entries: &[HistogramEntry], k: usize) -> Vec<ClusterResult> {
    let c = PALETTE_ALGORITHM_V1;
    if entries.is_empty() {
        return vec![];
    }
    let k = k.clamp(1, c.k_max.min(entries.len()));

    let labs: Vec<OkLab> = entries.iter().map(|e| e.oklab).collect();

    // 初始化：第一个中心选 count 最大的
    let mut centers: Vec<OkLab> = Vec::with_capacity(k);
    let mut first = 0;
    for (i, entry) in entries.iter().enumerate() {
        if entry.count > entries[first].count {
            first = i;
        }
    }
    centers.push(labs[first]);

    // 后续中心：最大化 min_distance² * sqrt(count)
    while centers.len() < k {
        let mut best = -1isize;
        let mut best_score = -1.0_f64;
        for (i, &lab) in labs.iter().enumerate() {
            let min_distance = centers
                .iter()
                .map(|c| delta_e(lab, *c))
                .fold(f64::INFINITY, f64::min);
            let score = min_distance * min_distance * (entries[i].count as f64).sqrt();
            if score > best_score {
                best_score = score;
                best = i as isize;
            }
        }
        if best < 0 || best_score <= 0.0 {
            break;
        }
        centers.push(labs[best as usize]);
    }

    let k = centers.len();

    // 迭代
    let mut assignments = vec![0usize; entries.len()];
    for _ in 0..c.max_iterations {
        let mut sums = vec![[0.0f64; 3]; k];
        let mut weights = vec![0.0f64; k];

        for (i, &lab) in labs.iter().enumerate() {
            let mut nearest = 0;
            let mut nearest_dist = f64::INFINITY;
            for (c_idx, center) in centers.iter().enumerate() {
                let dist = delta_e(lab, *center);
                if dist < nearest_dist {
                    nearest_dist = dist;
                    nearest = c_idx;
                }
            }
            assignments[i] = nearest;
            let weight = entries[i].count as f64;
            weights[nearest] += weight;
            sums[nearest][0] += lab[0] * weight;
            sums[nearest][1] += lab[1] * weight;
            sums[nearest][2] += lab[2] * weight;
        }

        let mut moved = false;
        for c_idx in 0..k {
            if weights[c_idx] == 0.0 {
                continue;
            }
            let next = [
                sums[c_idx][0] / weights[c_idx],
                sums[c_idx][1] / weights[c_idx],
                sums[c_idx][2] / weights[c_idx],
            ];
            if delta_e(next, centers[c_idx]) > c.convergence_epsilon {
                moved = true;
            }
            centers[c_idx] = next;
        }
        if !moved {
            break;
        }
    }

    // 最终分配 + 映射到最近真实桶
    let mut cluster_counts = vec![0usize; k];
    let mut nearest_entry = vec![-1isize; k];
    let mut nearest_dist = vec![f64::INFINITY; k];

    for (i, &lab) in labs.iter().enumerate() {
        let mut nearest = 0;
        let mut dist = f64::INFINITY;
        for (c_idx, center) in centers.iter().enumerate() {
            let d = delta_e(lab, *center);
            if d < dist {
                dist = d;
                nearest = c_idx;
            }
        }
        cluster_counts[nearest] += entries[i].count;
        if dist < nearest_dist[nearest] {
            nearest_dist[nearest] = dist;
            nearest_entry[nearest] = i as isize;
        }
    }

    let total: usize = cluster_counts.iter().sum();
    let mut clusters = Vec::new();
    for c_idx in 0..k {
        if cluster_counts[c_idx] == 0 || nearest_entry[c_idx] < 0 {
            continue;
        }
        let entry_idx = nearest_entry[c_idx] as usize;
        clusters.push(ClusterResult {
            rgb: entries[entry_idx].rgb,
            oklab: centers[c_idx],
            count: cluster_counts[c_idx],
            ratio: if total > 0 {
                cluster_counts[c_idx] as f64 / total as f64
            } else {
                0.0
            },
        });
    }

    clusters.sort_by_key(|c| std::cmp::Reverse(c.count));
    merge_near_colors(clusters)
}

/// 合并 OKLab 距离过近的聚类结果
fn merge_near_colors(clusters: Vec<ClusterResult>) -> Vec<ClusterResult> {
    let c = PALETTE_ALGORITHM_V1;
    if clusters.len() <= 1 {
        return clusters;
    }

    let mut result = vec![clusters[0].clone()];
    for cluster in &clusters[1..] {
        let mut merged = false;
        for r in result.iter_mut() {
            if delta_e(cluster.oklab, r.oklab) < c.near_color_merge_delta_e {
                let total_count = r.count + cluster.count;
                r.count = total_count;
                r.ratio = total_count as f64; // 暂存，后面归一
                merged = true;
                break;
            }
        }
        if !merged {
            result.push(cluster.clone());
        }
    }

    // 重新计算 ratio
    let total: usize = result.iter().map(|c| c.count).sum();
    for item in result.iter_mut() {
        item.ratio = if total > 0 {
            item.count as f64 / total as f64
        } else {
            0.0
        };
    }
    result.sort_by_key(|c| std::cmp::Reverse(c.count));
    result
}

// ── Mean-shift 聚类 ───────────────────────────────────────────────────────

/// 带宽窗口内带权质心（返回质心与窗口总权重）。
fn window_centroid(
    labs: &[OkLab],
    weights: &[f64],
    center: OkLab,
    bandwidth: f64,
) -> ([f64; 3], f64) {
    let mut s = [0.0f64; 3];
    let mut w = 0.0f64;
    for (lab, &weight) in labs.iter().zip(weights) {
        if delta_e(*lab, center) <= bandwidth {
            s[0] += lab[0] * weight;
            s[1] += lab[1] * weight;
            s[2] += lab[2] * weight;
            w += weight;
        }
    }
    if w == 0.0 {
        (s, 0.0)
    } else {
        ([s[0] / w, s[1] / w, s[2] / w], w)
    }
}

/// Mean-shift 密度聚类（OKLab 空间带权模式查找）。
/// 与 k-means 不同，簇数不预设：每个种子沿密度梯度收敛到局部模式，
/// 近邻模式按带宽一半合并。确定性：种子用 k-means++ 式最远点采样，
/// 保证不同色组都有种子（纯 count 截断只会覆盖主导色家族）；迭代顺序固定、无 RNG。
/// 输出契约与 k-means 一致：count 降序 + rgb 映射回真实桶 + `merge_near_colors` 收尾。
fn mean_shift_cluster(entries: &[HistogramEntry]) -> Vec<ClusterResult> {
    let c = PALETTE_ALGORITHM_V1;
    if entries.is_empty() {
        return vec![];
    }
    let ms = c.mean_shift;
    let total: usize = entries.iter().map(|e| e.count).sum();
    if total == 0 {
        return vec![];
    }
    let floor = (2usize).max((total as f64 * ms.min_ratio) as usize);

    // 全部通过 floor 的真实色（噪声已滤除）：种子覆盖用
    let active: Vec<usize> = (0..entries.len())
        .filter(|&i| entries[i].count >= floor)
        .collect();
    if active.is_empty() {
        return vec![];
    }

    // 质心窗口：count 降序取前 window_cap（覆盖质量主体，把每次质心扫描成本封顶；
    // 低占比点缀仍由种子覆盖，其窗口为空时以种子自身为模式，不丢色组）
    let mut mass: Vec<usize> = active.clone();
    mass.sort_by(|&a, &b| {
        entries[b]
            .count
            .cmp(&entries[a].count)
            .then_with(|| a.cmp(&b))
    });
    mass.truncate(mass.len().min(ms.window_cap));
    let win_labs: Vec<OkLab> = mass.iter().map(|&i| entries[i].oklab).collect();
    let win_counts: Vec<f64> = mass.iter().map(|&i| entries[i].count as f64).collect();

    // 种子：k-means++ 式最远点采样 over mass（主要色族都在 top-count 窗口内，
    // 低占比点缀即便不单独成簇也会在最终分配并入最近模式，不影响 top-8 输出）
    let mut seeds: Vec<usize> = Vec::new();
    {
        let mut first = 0;
        for (i, e) in entries.iter().enumerate() {
            if e.count > entries[first].count {
                first = i;
            }
        }
        seeds.push(first);
        let max_seeds = ms.seed_cap.min(mass.len());
        while seeds.len() < max_seeds {
            let mut best = -1isize;
            let mut best_score = 0.0f64;
            for (wi, &i) in mass.iter().enumerate() {
                let min_d = seeds
                    .iter()
                    .map(|&s| delta_e(entries[i].oklab, entries[s].oklab))
                    .fold(f64::INFINITY, f64::min);
                let score = min_d * min_d * (entries[i].count as f64).sqrt();
                if score > best_score {
                    best_score = score;
                    best = wi as isize;
                }
            }
            if best < 0 || best_score <= 0.0 {
                break;
            }
            seeds.push(mass[best as usize]);
        }
    }
    let seed_labs: Vec<OkLab> = seeds.iter().map(|&i| entries[i].oklab).collect();

    // 每个种子沿密度梯度迭代（窗口=质量集），收敛到已有模式则提前终止。
    // merge_dist = 带宽一半：近邻模式视为同一（按种子顺序先到先得）
    let merge_dist = ms.bandwidth * 0.5;
    let mut modes: Vec<OkLab> = Vec::new();
    for &s_lab in &seed_labs {
        let mut p = s_lab;
        // 起点已落在已有模式附近 → 直接归并，不迭代
        if modes.iter().any(|m| delta_e(*m, p) < merge_dist) {
            continue;
        }
        for _ in 0..ms.max_iters {
            let (centroid, w) = window_centroid(&win_labs, &win_counts, p, ms.bandwidth);
            if w == 0.0 {
                break;
            }
            let moved = delta_e(centroid, p);
            p = centroid;
            if moved < ms.convergence {
                break;
            }
            // 漂移到已有模式附近 → 提前终止，不新增模式（多数种子收敛到少数密度峰）
            if modes.iter().any(|m| delta_e(*m, p) < merge_dist) {
                break;
            }
        }
        if !modes.iter().any(|m| delta_e(*m, p) < merge_dist) {
            modes.push(p);
        }
    }
    if modes.is_empty() {
        return vec![];
    }

    // 分配所有 entries 到最近模式；rgb 取簇内距模式最近的真实桶（第一遍，供排序/过滤）
    let mut mode_counts = vec![0usize; modes.len()];
    let mut mode_nearest_entry = vec![-1isize; modes.len()];
    let mut mode_nearest_dist = vec![f64::INFINITY; modes.len()];
    for (i, entry) in entries.iter().enumerate() {
        let mut nearest = 0;
        let mut dist = f64::INFINITY;
        for (m_idx, m) in modes.iter().enumerate() {
            let d = delta_e(entry.oklab, *m);
            if d < dist {
                dist = d;
                nearest = m_idx;
            }
        }
        mode_counts[nearest] += entry.count;
        if dist < mode_nearest_dist[nearest] {
            mode_nearest_dist[nearest] = dist;
            mode_nearest_entry[nearest] = i as isize;
        }
    }

    // 模式按 count 降序：滤除噪声（低于 floor 的已在窗口外）后截断到 k_max，
    // 其余像素最终统一重分配到最近保留模式。
    let mut mode_order: Vec<usize> = (0..modes.len())
        .filter(|&m| mode_counts[m] >= floor)
        .collect();
    mode_order.sort_by_key(|&m| std::cmp::Reverse(mode_counts[m]));
    mode_order.truncate(mode_order.len().min(c.k_max));
    if mode_order.is_empty() {
        // 全被过滤时退化为最大模式，保证非空
        let largest = (0..modes.len())
            .max_by_key(|&m| mode_counts[m])
            .expect("modes 非空");
        mode_order.push(largest);
    }

    // 最终分配：所有 entries 归到保留模式（丢弃模式的像素并入最近保留模式）
    let mut cluster_counts = vec![0usize; mode_order.len()];
    let mut nearest_entry = vec![-1isize; mode_order.len()];
    let mut nearest_dist = vec![f64::INFINITY; mode_order.len()];
    for (i, entry) in entries.iter().enumerate() {
        let mut nearest = 0;
        let mut dist = f64::INFINITY;
        for (ki, &m) in mode_order.iter().enumerate() {
            let d = delta_e(entry.oklab, modes[m]);
            if d < dist {
                dist = d;
                nearest = ki;
            }
        }
        cluster_counts[nearest] += entry.count;
        if dist < nearest_dist[nearest] {
            nearest_dist[nearest] = dist;
            nearest_entry[nearest] = i as isize;
        }
    }

    let mut clusters: Vec<ClusterResult> = Vec::new();
    for (ki, &m) in mode_order.iter().enumerate() {
        if cluster_counts[ki] == 0 || nearest_entry[ki] < 0 {
            continue;
        }
        let entry_idx = nearest_entry[ki] as usize;
        clusters.push(ClusterResult {
            rgb: entries[entry_idx].rgb,
            oklab: modes[m],
            count: cluster_counts[ki],
            ratio: cluster_counts[ki] as f64 / total as f64,
        });
    }

    clusters.sort_by_key(|c| std::cmp::Reverse(c.count));
    merge_near_colors(clusters)
}

// ── MMCQ 聚类 ─────────────────────────────────────────────────────────────

/// MMCQ（Modified Median Cut）直方图量化：按"占比 × 通道范围"选最大盒，
/// 沿范围最大通道在占比中位数处劈开，直至目标色数。
/// 确定性：排序稳定（值相等按索引）、无 RNG。输出契约与 k-means 一致。
fn mmcq_cluster(entries: &[HistogramEntry]) -> Vec<ClusterResult> {
    let c = PALETTE_ALGORITHM_V1;
    if entries.is_empty() {
        return vec![];
    }
    let target = c.mmcq.target_colors.clamp(1, entries.len());
    let total: usize = entries.iter().map(|e| e.count).sum();
    if total == 0 {
        return vec![];
    }

    // 每个盒 = 一组 entries 索引；初始一个盒含全部
    let mut boxes: Vec<Vec<usize>> = vec![(0..entries.len()).collect()];

    while boxes.len() < target {
        // 选 占比 × 最大通道范围 最大的盒与切分轴
        let mut best: Option<(usize, usize)> = None; // (box_idx, axis)
        let mut best_score = 0.0f64;
        for (bi, b) in boxes.iter().enumerate() {
            if b.len() < 2 {
                continue;
            }
            let pop: usize = b.iter().map(|&i| entries[i].count).sum();
            let mut mins = [255i32; 3];
            let mut maxs = [0i32; 3];
            for &i in b {
                for ch in 0..3 {
                    let v = entries[i].rgb[ch] as i32;
                    mins[ch] = mins[ch].min(v);
                    maxs[ch] = maxs[ch].max(v);
                }
            }
            let (axis, range) = (0..3)
                .map(|ch| (ch, maxs[ch] - mins[ch]))
                .max_by_key(|&(_, r)| r)
                .expect("三通道总有最大值");
            if range <= 0 {
                continue; // 盒内同色，无法再分
            }
            let score = pop as f64 * range as f64;
            if score > best_score {
                best_score = score;
                best = Some((bi, axis));
            }
        }
        let Some((bi, axis)) = best else { break };

        // 取走该盒，按 axis 稳定排序后在占比中位数处劈开
        let mut bucket = boxes.swap_remove(bi);
        bucket.sort_by(|&a, &b| {
            entries[a].rgb[axis]
                .cmp(&entries[b].rgb[axis])
                .then_with(|| a.cmp(&b))
        });
        let half = bucket
            .iter()
            .map(|&i| entries[i].count)
            .sum::<usize>()
            .div_ceil(2);
        let mut acc = 0usize;
        let mut split = bucket.len() - 1;
        for (pos, &i) in bucket.iter().enumerate() {
            acc += entries[i].count;
            if acc >= half {
                split = pos + 1;
                break;
            }
        }
        split = split.clamp(1, bucket.len() - 1);
        let right = bucket.split_off(split);
        boxes.push(bucket);
        boxes.push(right);
    }

    // 从每个盒构建 ClusterResult
    let mut clusters = Vec::new();
    for b in &boxes {
        let mut s = [0.0f64; 3];
        let mut w = 0.0f64;
        for &i in b {
            let weight = entries[i].count as f64;
            s[0] += entries[i].oklab[0] * weight;
            s[1] += entries[i].oklab[1] * weight;
            s[2] += entries[i].oklab[2] * weight;
            w += weight;
        }
        if w == 0.0 {
            continue;
        }
        let centroid = [s[0] / w, s[1] / w, s[2] / w];
        // rgb 取盒内距质心最近的真实桶
        let mut nearest = 0;
        let mut nearest_d = f64::INFINITY;
        for &i in b {
            let d = delta_e(entries[i].oklab, centroid);
            if d < nearest_d {
                nearest_d = d;
                nearest = i;
            }
        }
        let count: usize = b.iter().map(|&i| entries[i].count).sum();
        clusters.push(ClusterResult {
            rgb: entries[nearest].rgb,
            oklab: centroid,
            count,
            ratio: count as f64 / total as f64,
        });
    }

    clusters.sort_by_key(|c| std::cmp::Reverse(c.count));
    merge_near_colors(clusters)
}

// ── 角色色分配 ────────────────────────────────────────────────────────────

/// 为聚类结果分配角色色
pub fn assign_roles(clusters: Vec<ClusterResult>) -> Vec<RoleColor> {
    let c = PALETTE_ALGORITHM_V1;
    if clusters.is_empty() {
        return vec![];
    }

    let mut result: Vec<RoleColor> = clusters
        .iter()
        .map(|cl| RoleColor {
            rgb: cl.rgb,
            oklab: cl.oklab,
            ratio: cl.ratio,
            role: String::new(),
            hex: rgb_to_hex(cl.rgb[0], cl.rgb[1], cl.rgb[2]),
        })
        .collect();

    // 按 OKLab L 值排序找最亮和最暗
    let mut by_l: Vec<usize> = (0..result.len()).collect();
    by_l.sort_by(|&a, &b| {
        result[b].oklab[0]
            .partial_cmp(&result[a].oklab[0])
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    // 背景色：占比 >= 35%，或亮度最高且占比最大
    let mut bg_assigned = false;
    for item in result.iter_mut() {
        if item.ratio >= c.background_min_ratio {
            item.role = "background".into();
            bg_assigned = true;
            break;
        }
    }
    if !bg_assigned && !result.is_empty() {
        result[by_l[0]].role = "background".into();
        bg_assigned = true;
    }
    let _ = bg_assigned;

    // 剩余颜色分配
    for item in result.iter_mut() {
        if !item.role.is_empty() {
            continue;
        }
        if item.ratio >= c.accent_min_ratio {
            item.role = "accent".into();
        } else if item.oklab[0] < 0.3 {
            item.role = "foreground".into();
        } else {
            item.role = "muted".into();
        }
    }

    // 角色优先排序：background > accent > foreground > muted
    let role_order = |role: &str| -> usize {
        match role {
            "background" => 0,
            "accent" => 1,
            "foreground" => 2,
            "muted" => 3,
            _ => 4,
        }
    };
    result.sort_by(|a, b| {
        let ro = role_order(&a.role).cmp(&role_order(&b.role));
        if ro != std::cmp::Ordering::Equal {
            return ro;
        }
        b.ratio
            .partial_cmp(&a.ratio)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    result
}

// ── Harmony 方案生成（OKLCH + sRGB gamut mapping）─────────────────────────

// generate_harmony 已删除（0.20.7 修订三轮）：生产路径不使用，仅测试调用。
// generate_design_palettes 不依赖它。

/// 面向实际设计使用的显式基准色生成器
pub fn generate_design_palettes(anchor_rgb: Rgb, source_colors: &[Rgb]) -> Vec<HarmonyScheme> {
    let anchor_oklab = rgb_to_oklab(anchor_rgb[0], anchor_rgb[1], anchor_rgb[2]);
    let [l, c, h] = oklab_to_oklch(anchor_oklab);
    let c_min = c.max(0.03);

    // 去重 helper
    let dedupe = |colors: &[Rgb]| -> Vec<Rgb> {
        let mut seen = std::collections::HashSet::new();
        let mut result = Vec::new();
        for &rgb in colors {
            let hex = rgb_to_hex(rgb[0], rgb[1], rgb[2]);
            if seen.insert(hex) {
                result.push(rgb);
            }
        }
        result
    };

    // 从 source_colors 找灰阶
    let neutrals: Vec<Rgb> = source_colors
        .iter()
        .filter(|rgb| {
            let (_, s, _) = rgb_to_hsl(rgb[0], rgb[1], rgb[2]);
            s < 18.0
        })
        .copied()
        .collect();

    let dark_neutral = neutrals.first().copied();
    let light_neutral = neutrals.last().copied();

    // 同色层级：5 个明度档
    let levels = {
        let mut s = vec![18.0_f64, 34.0, l.clamp(0.12, 0.88) * 100.0, 66.0, 84.0];
        s.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        s.dedup();
        s
    };
    let monochrome: Vec<Rgb> = levels
        .iter()
        .map(|&lightness| oklch_to_srgb_gamut_mapped([lightness / 100.0, c_min, h]))
        .collect();

    // 邻近协调：基准色左右 30° 的协调色
    let l_mid = l.clamp(0.28, 0.72);
    let analogous: Vec<Rgb> = [h - 30.0, h, h + 30.0]
        .iter()
        .map(|&hue| oklch_to_srgb_gamut_mapped([l_mid, c_min, (hue + 360.0) % 360.0]))
        .collect();

    // 互补强调：基准色、互补色与原图灰阶
    let mut complement: Vec<Rgb> = Vec::new();
    if let Some(dn) = dark_neutral {
        complement.push(dn);
    }
    complement.push(anchor_rgb);
    complement.push(oklch_to_srgb_gamut_mapped([
        l_mid,
        c_min,
        (h + 180.0) % 360.0,
    ]));
    if let Some(ln) = light_neutral {
        complement.push(ln);
    }

    vec![
        HarmonyScheme {
            label: "同色层级".into(),
            scheme: "generated-tones".into(),
            description: "同一基准色的明暗层级".into(),
            colors: dedupe(&monochrome),
            source_kind: "generated".into(),
            confidence: 0.8,
        },
        HarmonyScheme {
            label: "邻近协调".into(),
            scheme: "generated-analogous".into(),
            description: "基准色左右 30° 的协调色".into(),
            colors: dedupe(&analogous),
            source_kind: "generated".into(),
            confidence: 0.8,
        },
        HarmonyScheme {
            label: "互补强调".into(),
            scheme: "generated-complement".into(),
            description: "基准色、互补色与原图灰阶".into(),
            colors: dedupe(&complement),
            source_kind: "generated".into(),
            confidence: 0.8,
        },
    ]
}

// ── 主题分析 ──────────────────────────────────────────────────────────────

/// 根据聚类主题色生成可读的整体倾向，不改变聚类结果。
/// 色相使用按饱和度加权的圆周均值，避免红色 359°/1° 被平均成青色。
pub fn analyze_theme(roles: &[RoleColor]) -> ThemeAnalysis {
    if roles.is_empty() {
        return ThemeAnalysis {
            family: "无主题色".into(),
            temperature: "中性".into(),
            saturation: "低饱和".into(),
            lightness: "中间调".into(),
            hue_concentration: 0.0,
            summary: "无可分析主题色".into(),
        };
    }

    let mut x = 0.0_f64;
    let mut y = 0.0_f64;
    let mut chroma_weight = 0.0_f64;
    let mut saturation_sum = 0.0_f64;
    let mut lightness_sum = 0.0_f64;
    let mut ratio_sum = 0.0_f64;

    for role in roles {
        let (h_deg, s, l) = rgb_to_hsl(role.rgb[0], role.rgb[1], role.rgb[2]);
        let ratio = role.ratio.max(0.0);
        let hue_weight = ratio * (s / 100.0);
        let radians = h_deg * std::f64::consts::PI / 180.0;
        x += radians.cos() * hue_weight;
        y += radians.sin() * hue_weight;
        chroma_weight += hue_weight;
        saturation_sum += s * ratio;
        lightness_sum += l * ratio;
        ratio_sum += ratio;
    }

    let avg_s = if ratio_sum > 0.0 {
        saturation_sum / ratio_sum
    } else {
        0.0
    };
    let avg_l = if ratio_sum > 0.0 {
        lightness_sum / ratio_sum
    } else {
        50.0
    };

    // 圆周均值的向量长度表示色相集中度
    let hue_concentration = if chroma_weight > 0.01 {
        (x * x + y * y).sqrt() / chroma_weight
    } else {
        0.0
    };

    let is_multi_hue = roles.len() >= 3 && avg_s >= 25.0 && hue_concentration < 0.45;

    let hue = if chroma_weight > 0.01 && !is_multi_hue {
        Some((y.atan2(x).to_degrees() + 360.0) % 360.0)
    } else {
        None
    };

    let family = if is_multi_hue {
        "多色系".to_string()
    } else if let Some(h) = hue {
        if avg_s < 12.0 {
            "中性色系".to_string()
        } else if !(15.0..345.0).contains(&h) {
            "红色系".to_string()
        } else if h < 45.0 {
            "橙色系".to_string()
        } else if h < 75.0 {
            "黄色系".to_string()
        } else if h < 165.0 {
            "绿色系".to_string()
        } else if h < 200.0 {
            "青色系".to_string()
        } else if h < 255.0 {
            "蓝色系".to_string()
        } else if h < 300.0 {
            "紫色系".to_string()
        } else {
            "粉色系".to_string()
        }
    } else {
        "中性色系".to_string()
    };

    let warm = hue.is_some_and(|h| !(90.0..330.0).contains(&h));
    let cool = hue.is_some_and(|h| (150.0..285.0).contains(&h));
    let temperature = if is_multi_hue {
        "冷暖均衡".to_string()
    } else if warm {
        "暖色倾向".to_string()
    } else if cool {
        "冷色倾向".to_string()
    } else {
        "综合色倾向".to_string()
    };

    let saturation = if avg_s < 25.0 {
        "低饱和".to_string()
    } else if avg_s < 60.0 {
        "中等饱和".to_string()
    } else {
        "高饱和".to_string()
    };

    let lightness = if avg_l < 35.0 {
        "深色调".to_string()
    } else if avg_l < 70.0 {
        "中间调".to_string()
    } else {
        "浅色调".to_string()
    };

    ThemeAnalysis {
        family: family.clone(),
        temperature: temperature.clone(),
        saturation: saturation.clone(),
        lightness: lightness.clone(),
        hue_concentration,
        summary: format!(
            "{} · {} · {} · {} · {} 个主题色",
            family,
            temperature,
            saturation,
            lightness,
            roles.len()
        ),
    }
}

// ── 设计取色：显著色 / 均衡色 ─────────────────────────────────────────────

/// 5-bit RGB 直方图桶数
const HISTOGRAM_BINS: usize = 32 * 32 * 32;

/// 直方图累积器（固定 5-bit RGB 直方图 + Boyer-Moore 代表色投票）
pub struct ColorHistogramAccumulator {
    counts: Vec<u32>,
    representatives: Vec<u32>,
    representative_votes: Vec<i32>,
    valid_pixels: usize,
}

impl ColorHistogramAccumulator {
    pub fn new() -> Self {
        Self {
            counts: vec![0u32; HISTOGRAM_BINS],
            representatives: vec![0u32; HISTOGRAM_BINS],
            representative_votes: vec![0i32; HISTOGRAM_BINS],
            valid_pixels: 0,
        }
    }

    /// 将指定像素区间累积进固定直方图
    pub fn accumulate(&mut self, rgba_flat: &[u8], start_pixel: usize, end_pixel: usize) {
        let safe_end = end_pixel.min(rgba_flat.len() / 4);
        let c = PALETTE_ALGORITHM_V1;
        for pixel in start_pixel..safe_end {
            let i = pixel * 4;
            if rgba_flat[i + 3] as f64 / 255.0 < c.min_alpha {
                continue;
            }
            let r = rgba_flat[i];
            let g = rgba_flat[i + 1];
            let b = rgba_flat[i + 2];
            let qr = (r >> 3) as usize;
            let qg = (g >> 3) as usize;
            let qb = (b >> 3) as usize;
            let key = (qr << 10) | (qg << 5) | qb;
            let packed = ((r as u32) << 16) | ((g as u32) << 8) | b as u32;

            // Boyer-Moore 多数候选
            if self.representative_votes[key] == 0 {
                self.representatives[key] = packed;
                self.representative_votes[key] = 1;
            } else if self.representatives[key] == packed {
                self.representative_votes[key] += 1;
            } else {
                self.representative_votes[key] -= 1;
            }
            self.counts[key] += 1;
            self.valid_pixels += 1;
        }
    }

    /// 压紧固定直方图
    pub fn finalize(&self, scanned_pixels: usize) -> ColorHistogram {
        let mut colors = Vec::new();
        let mut counts = Vec::new();

        for key in 0..HISTOGRAM_BINS {
            if self.counts[key] == 0 {
                continue;
            }
            let packed = self.representatives[key];
            colors.push([
                ((packed >> 16) & 0xff) as u8,
                ((packed >> 8) & 0xff) as u8,
                (packed & 0xff) as u8,
            ]);
            counts.push(self.counts[key]);
        }

        ColorHistogram {
            colors,
            counts,
            valid_pixels: self.valid_pixels,
            scanned_pixels,
            mode: "full".into(),
        }
    }
}

impl Default for ColorHistogramAccumulator {
    fn default() -> Self {
        Self::new()
    }
}

/// 压紧后的直方图
#[derive(Debug, Clone)]
pub struct ColorHistogram {
    pub colors: Vec<Rgb>,
    pub counts: Vec<u32>,
    pub valid_pixels: usize,
    pub scanned_pixels: usize,
    pub mode: String,
}

impl ColorHistogram {
    /// 从直方图构建带权条目列表
    pub fn to_entries(&self) -> Vec<HistogramEntry> {
        let total = if self.valid_pixels > 0 {
            self.valid_pixels
        } else {
            self.counts.iter().sum::<u32>() as usize
        };

        self.colors
            .iter()
            .zip(self.counts.iter())
            .map(|(rgb, &count)| {
                let oklab = rgb_to_oklab(rgb[0], rgb[1], rgb[2]);
                HistogramEntry {
                    rgb: *rgb,
                    oklab,
                    chroma: (oklab[1] * oklab[1] + oklab[2] * oklab[2]).sqrt(),
                    count: count as usize,
                    ratio: if total > 0 {
                        count as f64 / total as f64
                    } else {
                        0.0
                    },
                }
            })
            .collect()
    }
}

/// 从 RGBA flat 数据构建直方图
pub fn build_color_histogram(rgba_flat: &[u8]) -> ColorHistogram {
    let scanned_pixels = rgba_flat.len() / 4;
    let mut acc = ColorHistogramAccumulator::new();
    acc.accumulate(rgba_flat, 0, scanned_pixels);
    acc.finalize(scanned_pixels)
}

// ── 色相峰值候选 ─────────────────────────────────────────────────────────

const HUE_SECTORS: usize = 24;

struct HueSector {
    count: usize,
    chroma_sum: f64,
    contrast_sum: f64,
    entries: Vec<usize>,
    energy: f64,
    smoothed_energy: f64,
}

impl HueSector {
    fn new() -> Self {
        Self {
            count: 0,
            chroma_sum: 0.0,
            contrast_sum: 0.0,
            entries: Vec::new(),
            energy: 0.0,
            smoothed_energy: 0.0,
        }
    }
}

/// 全局显著性（Histogram Contrast 唯一性）：对每个直方图颜色，计算它与
/// 其它颜色按占比加权的 OKLab 距离之和。值越大 = 颜色越独特、越"引人注目"——
/// 与全图高频色相近的颜色得分低（即便面积大），稀有且与多数色差异大的得分高。
///
/// 参考 Cheng 等《Global Contrast Based Salient Region Detection》（TPAMI 2015）。
/// 实现细节：参考集按占比截断到 TOP_N 并重归一，避免全直方图 O(n²)。
fn compute_global_saliency(entries: &[HistogramEntry]) -> Vec<f64> {
    const TOP_N: usize = 128;
    let n = entries.len();
    if n == 0 {
        return Vec::new();
    }

    // 占比最高的 TOP_N 色作为参考集（低占比色对加权和贡献可忽略）
    let mut order: Vec<usize> = (0..n).collect();
    order.sort_by(|&a, &b| entries[b].count.cmp(&entries[a].count));
    let top_n = order.len().min(TOP_N);
    let top_sum: u64 = order[..top_n]
        .iter()
        .map(|&i| entries[i].count as u64)
        .sum();
    let top: Vec<(OkLab, f64)> = order[..top_n]
        .iter()
        .map(|&i| (entries[i].oklab, entries[i].count as f64 / top_sum as f64))
        .collect();

    entries
        .iter()
        .map(|e| top.iter().map(|(lab, w)| w * delta_e(e.oklab, *lab)).sum())
        .collect()
}

/// 在色相环上聚合高色度颜色并找局部峰值。
/// 每个峰最终仍返回一个原图真实颜色，而不是色相扇区的平均色。
fn find_hue_peak_candidates(
    entries: &[HistogramEntry],
    bg_lab: OkLab,
    bg_is_dominant: bool,
) -> Vec<(usize, f64, f64, f64)> {
    let total: usize = entries.iter().map(|e| e.count).sum();
    if total == 0 {
        return vec![];
    }

    let c = PALETTE_ALGORITHM_V1;
    let mut sectors: Vec<HueSector> = (0..HUE_SECTORS).map(|_| HueSector::new()).collect();
    let candidate_min_count = (2usize).max((total as f64 * c.salient_min_count_factor) as usize);
    // 全局唯一性信号：相对全图颜色分布，而非单一背景参考点——
    // 降级指派背景（无主导色）时，对背景的反差会误压最亮点缀（0.21.15 LYZL 亮黄漏检根因）
    let global_saliency = compute_global_saliency(entries);

    for (i, entry) in entries.iter().enumerate() {
        let background_distance = delta_e(entry.oklab, bg_lab);
        if entry.chroma < c.salient_chroma_min {
            continue;
        }
        // 背景为面积主导时才剔除其邻近色；无主导背景时背景色是降级指派，
        // 不排除背景自身——否则多色均分图里最亮的黄/青会被误当背景而漏检。
        if bg_is_dominant && background_distance < c.salient_bg_distance_min {
            continue;
        }
        let hue_raw = entry.oklab[2].atan2(entry.oklab[1]).to_degrees();
        let hue = if hue_raw < 0.0 {
            hue_raw + 360.0
        } else {
            hue_raw
        };
        let sector_idx = ((hue / (360.0 / HUE_SECTORS as f64)) as usize) % HUE_SECTORS;
        let sector = &mut sectors[sector_idx];
        sector.count += entry.count;
        sector.chroma_sum += entry.chroma * entry.count as f64;
        sector.contrast_sum += global_saliency[i] * entry.count as f64;
        sector.entries.push(i);
    }

    let total_f = total as f64;
    for sector in sectors.iter_mut() {
        if sector.count == 0 {
            continue;
        }
        let average_chroma = sector.chroma_sum / sector.count as f64;
        let average_contrast = sector.contrast_sum / sector.count as f64;
        // 面积项归一化：低占比有 bias 托底，少量高色度高全局唯一性点缀不被面积压制；
        // 是否"亮眼"主要由色度与全局唯一性（相对全图分布）决定。
        let area = c.salient_area_bias
            + (1.0 - c.salient_area_bias)
                * (sector.count as f64 / total_f).powf(c.salient_area_exponent);
        sector.energy = area
            * (c.salient_chroma_bias + average_chroma * c.salient_chroma_gain)
            * (c.salient_contrast_bias + average_contrast * c.salient_contrast_gain);
    }

    // 平滑能量
    for i in 0..HUE_SECTORS {
        let prev = sectors[(i + HUE_SECTORS - 1) % HUE_SECTORS].energy;
        let next = sectors[(i + 1) % HUE_SECTORS].energy;
        sectors[i].smoothed_energy = sectors[i].energy + (prev + next) * c.salient_smooth_weight;
    }

    // 按峰强排序并抑制相邻扇区（非极大值抑制）
    let mut peaks: Vec<(usize, f64)> = sectors
        .iter()
        .enumerate()
        .filter(|(_, s)| s.count >= candidate_min_count)
        .map(|(i, s)| (i, s.smoothed_energy))
        .collect();
    peaks.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

    // 能量相对化 NMS：相邻扇区只在"明显弱于已选峰"时才抑制，
    // 两个都强（如亮黄与其邻近橄榄）可同时保留，避免小点缀被相邻峰挤掉。
    let mut selected_peaks: Vec<usize> = Vec::new();
    for (peak_idx, _) in &peaks {
        let suppressed = selected_peaks.iter().any(|&chosen| {
            let distance = (chosen as i64 - *peak_idx as i64).unsigned_abs() as usize;
            distance.min(HUE_SECTORS - distance) <= c.salient_peak_min_distance
                && sectors[*peak_idx].smoothed_energy
                    < sectors[chosen].smoothed_energy * c.salient_peak_suppress_ratio
        });
        if !suppressed {
            selected_peaks.push(*peak_idx);
        }
    }

    // 为每个峰选代表色
    let mut results: Vec<(usize, f64, f64, f64)> = Vec::new();
    for &peak_idx in &selected_peaks {
        let sector = &sectors[peak_idx];
        let mut neighboring_indices: Vec<usize> = Vec::new();
        for &idx in &sectors[(peak_idx + HUE_SECTORS - 1) % HUE_SECTORS].entries {
            neighboring_indices.push(idx);
        }
        for &idx in &sector.entries {
            neighboring_indices.push(idx);
        }
        for &idx in &sectors[(peak_idx + 1) % HUE_SECTORS].entries {
            neighboring_indices.push(idx);
        }

        let filtered: Vec<usize> = neighboring_indices
            .iter()
            .filter(|&&idx| entries[idx].count >= candidate_min_count)
            .copied()
            .collect();
        let candidates: &[usize] = if filtered.is_empty() {
            &neighboring_indices
        } else {
            &filtered
        };

        let mut best: Option<(usize, f64)> = None;
        for &idx in candidates {
            let entry = &entries[idx];
            let score = entry.ratio.powf(c.salient_rep_area_exponent)
                * (c.salient_chroma_bias + entry.chroma * c.salient_chroma_gain)
                * (c.salient_contrast_bias + global_saliency[idx] * c.salient_contrast_gain);
            if best.is_none_or(|(_, s)| score > s) {
                best = Some((idx, score));
            }
        }

        if let Some((idx, score)) = best {
            results.push((idx, score * sector.smoothed_energy, 0.0, 0.0));
        }
    }

    results.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    results
}

/// 多样化选色：从候选中选最大化的代表色集合
fn pick_diverse_colors(
    candidates: &[(usize, f64)], // (entry_idx, score)
    entries: &[HistogramEntry],
    max_colors: usize,
) -> Vec<usize> {
    if candidates.is_empty() {
        return vec![];
    }
    let c = PALETTE_ALGORITHM_V1;

    let mut remaining: Vec<(usize, f64)> = candidates.to_vec();
    let mut selected: Vec<usize> = Vec::new();

    while !remaining.is_empty() && selected.len() < max_colors {
        let mut best_index = 0;
        let mut best_value = f64::NEG_INFINITY;

        for (i, (entry_idx, score)) in remaining.iter().enumerate() {
            let candidate_lab = entries[*entry_idx].oklab;
            let min_distance = if selected.is_empty() {
                1.0
            } else {
                selected
                    .iter()
                    .map(|&s| delta_e(entries[s].oklab, candidate_lab))
                    .fold(f64::INFINITY, f64::min)
            };
            if !selected.is_empty() && min_distance < c.salient_diversity_min_distance {
                continue;
            }
            let value = score * (0.25 + min_distance * 3.0);
            if value > best_value {
                best_value = value;
                best_index = i;
            }
        }

        if best_value == f64::NEG_INFINITY {
            break;
        }

        selected.push(remaining[best_index].0);
        remaining.remove(best_index);
    }

    selected
}

/// 同时生成面积主题之外的两种设计视角：
/// - 视觉焦点色：保护面积很小、但高色度且与背景反差明显的颜色
/// - 均衡/界面关键色：背景 + 层级灰 + 多样化点缀
pub fn extract_design_schemes(
    entries: &[HistogramEntry],
    roles: &[RoleColor],
) -> Vec<HarmonyScheme> {
    if entries.is_empty() || roles.is_empty() {
        return vec![];
    }

    let background = roles
        .iter()
        .find(|r| r.role == "background")
        .unwrap_or(&roles[0]);
    let bg_lab = background.oklab;

    // 色相峰值候选
    // 背景占比 >= background_min_ratio 才算面积主导；否则背景色是降级指派，显著色不应排除它。
    let c = PALETTE_ALGORITHM_V1;
    let bg_is_dominant = background.ratio >= c.background_min_ratio;
    let salient_candidates = find_hue_peak_candidates(entries, bg_lab, bg_is_dominant);
    let salient_entries: Vec<(usize, f64)> = salient_candidates
        .iter()
        .map(|(idx, score, _, _)| (*idx, *score))
        .collect();
    let salient = pick_diverse_colors(&salient_entries, entries, c.salient_max_colors);

    // 中性色候选
    let mut neutral_candidates: Vec<(usize, f64)> = entries
        .iter()
        .enumerate()
        .filter_map(|(i, e)| {
            let bg_dist = delta_e(e.oklab, bg_lab);
            let score = e.ratio.powf(0.3) * bg_dist;
            if e.ratio >= 0.001 && e.chroma < 0.055 && bg_dist >= 0.06 {
                Some((i, score))
            } else {
                None
            }
        })
        .collect();
    neutral_candidates.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    let neutrals = pick_diverse_colors(&neutral_candidates, entries, 2);

    // 均衡配色：背景 + 中性 + 显著色
    let mut balanced: Vec<Rgb> = Vec::new();
    let mut balanced_labs: Vec<OkLab> = Vec::new();
    let append_distinct = |rgb: Rgb, balanced: &mut Vec<Rgb>, balanced_labs: &mut Vec<OkLab>| {
        let lab = rgb_to_oklab(rgb[0], rgb[1], rgb[2]);
        if balanced_labs
            .iter()
            .any(|existing| delta_e(*existing, lab) < 0.035)
        {
            return;
        }
        balanced.push(rgb);
        balanced_labs.push(lab);
    };

    append_distinct(background.rgb, &mut balanced, &mut balanced_labs);
    for &idx in &neutrals {
        append_distinct(entries[idx].rgb, &mut balanced, &mut balanced_labs);
    }
    for &idx in salient.iter().take(c.balanced_salient_take) {
        append_distinct(entries[idx].rgb, &mut balanced, &mut balanced_labs);
    }
    if balanced.len() < 3 {
        for role in roles {
            append_distinct(role.rgb, &mut balanced, &mut balanced_labs);
        }
    }

    let neutral_pixel_ratio: f64 = entries
        .iter()
        .filter(|e| e.chroma < 0.055)
        .map(|e| e.ratio)
        .sum();
    let interface_like = neutral_pixel_ratio >= 0.6 && salient.len() >= 2;

    let salient_colors: Vec<Rgb> = if !salient.is_empty() {
        salient.iter().map(|&idx| entries[idx].rgb).collect()
    } else {
        roles.iter().map(|r| r.rgb).collect()
    };

    vec![
        HarmonyScheme {
            label: "视觉焦点色".into(),
            scheme: "salient".into(),
            description: "色相峰值、高色度与背景反差".into(),
            colors: salient_colors,
            source_kind: "extraction".into(),
            confidence: 1.0,
        },
        HarmonyScheme {
            label: if interface_like {
                "界面关键色"
            } else {
                "均衡配色"
            }
            .into(),
            scheme: "balanced".into(),
            description: if interface_like {
                "背景、文字层级与状态点缀"
            } else {
                "主色、层级色与点缀色"
            }
            .into(),
            colors: balanced,
            source_kind: "extraction".into(),
            confidence: 1.0,
        },
        // P1-2：第四组——原图智能搭配（只含真实输入像素）
        generate_smart_pairing(roles, &salient, entries, bg_lab),
    ]
}

/// P1-2：原图智能搭配——只从输入直方图里出现过的真实像素 RGB 中选最优五角色组合。
///
/// **契约铁则**：候选池全部必须是输入直方图里出现过的真实像素 RGB。
/// 不从 DOM 取色、不做任何 OKLCH/HSL 变换生成新颜色。
///
/// 五角色：背景、表面、正文、主强调、可选次强调。
/// 硬约束：正文对背景 WCAG 对比度 >= 4.5:1，主强调（及次强调，若存在）对背景 >= 3:1。
/// 没有合格组合时返回结构化低置信结果，不伪造颜色。
fn generate_smart_pairing(
    roles: &[RoleColor],
    salient: &[usize],
    entries: &[HistogramEntry],
    bg_lab: OkLab,
) -> HarmonyScheme {
    let c = PALETTE_ALGORITHM_V1;

    // 1. 构建候选池：从聚类角色色、显著色（find_hue_peak_candidates 结果）、均衡色相关条目构成。
    //    全部必须是输入直方图里出现过的真实像素 RGB。
    let mut candidate_indices: Vec<usize> = Vec::new();
    let mut seen_rgb: std::collections::HashSet<Rgb> = std::collections::HashSet::new();

    // 从 roles 的 RGB 反查 entries 索引
    let find_entry_by_rgb =
        |rgb: Rgb| -> Option<usize> { entries.iter().position(|e| e.rgb == rgb) };

    for role in roles {
        if seen_rgb.insert(role.rgb)
            && let Some(idx) = find_entry_by_rgb(role.rgb)
        {
            candidate_indices.push(idx);
        }
    }

    // 从 salient（显著色 entry indices）加入
    for &idx in salient {
        if idx < entries.len() && seen_rgb.insert(entries[idx].rgb) {
            candidate_indices.push(idx);
        }
    }

    // 从均衡色相关条目（中性色、高占比色）补充候选
    for (i, entry) in entries.iter().enumerate() {
        if candidate_indices.len() >= c.smart_max_candidates {
            break;
        }
        if entry.ratio < c.smart_min_candidate_ratio {
            continue;
        }
        if seen_rgb.insert(entry.rgb) {
            candidate_indices.push(i);
        }
    }

    // 2. 按亮度排序候选（用于背景/正文阶梯选色）
    candidate_indices.sort_by(|&a, &b| {
        entries[a].oklab[0]
            .partial_cmp(&entries[b].oklab[0])
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    if candidate_indices.is_empty() {
        // 降级：无候选
        return HarmonyScheme {
            label: "原图智能搭配".into(),
            scheme: "smart_pairing".into(),
            description: "无足够候选像素，已降级".into(),
            colors: vec![],
            source_kind: "extraction".into(),
            confidence: c.smart_degraded_confidence,
        };
    }

    // 3. 判断浅色/深色主题：原图背景明度 > 0.5 → 浅色主题
    let is_light_theme = bg_lab[0] > 0.5;

    // 4. 背景候选：占比最高的候选 或 亮度极端（最亮/最暗）候选
    let bg_candidates: Vec<usize> = if is_light_theme {
        // 浅色主题：优先亮度高的候选
        candidate_indices
            .iter()
            .rev() // 高亮度在前
            .copied()
            .take(5)
            .collect()
    } else {
        // 深色主题：优先亮度低的候选
        candidate_indices.iter().copied().take(5).collect()
    };

    // 也考虑角色色中标记为 background 的
    let bg_role_rgb = roles.iter().find(|r| r.role == "background").map(|r| r.rgb);
    let mut bg_candidates = bg_candidates;
    if let Some(bg_rgb) = bg_role_rgb
        && let Some(idx) = find_entry_by_rgb(bg_rgb)
        && !bg_candidates.contains(&idx)
    {
        bg_candidates.insert(0, idx);
    }

    // 5. 尝试所有背景候选，为每个背景找最佳正文+强调组合
    let mut best_scheme: Option<SmartPairingResult> = None;

    for &bg_idx in &bg_candidates {
        let bg_rgb = entries[bg_idx].rgb;

        // 正文候选：与背景对比度 >= 4.5:1
        let text_candidates: Vec<usize> = candidate_indices
            .iter()
            .copied()
            .filter(|&idx| {
                idx != bg_idx
                    && contrast_ratio(entries[idx].rgb, bg_rgb) >= c.smart_text_min_contrast
            })
            .collect();

        if text_candidates.is_empty() {
            continue;
        }

        // 强调候选：与背景对比度 >= 3:1，且色度较高
        let accent_candidates: Vec<usize> = candidate_indices
            .iter()
            .copied()
            .filter(|&idx| {
                idx != bg_idx
                    && contrast_ratio(entries[idx].rgb, bg_rgb) >= c.smart_accent_min_contrast
                    && entries[idx].chroma > 0.03
            })
            .collect();

        if accent_candidates.is_empty() {
            continue;
        }

        // 表面候选：与背景有适度明度差但不是正文（中间亮度）
        let surface_candidates: Vec<usize> = candidate_indices
            .iter()
            .copied()
            .filter(|&idx| {
                idx != bg_idx
                    && entries[idx].chroma < 0.055
                    && (entries[idx].oklab[0] - bg_lab[0]).abs() > 0.05
            })
            .collect();

        // 尝试组合：选最优正文 + 主强调（+ 次强调可选）
        for &text_idx in &text_candidates {
            for &accent_idx in &accent_candidates {
                if accent_idx == text_idx {
                    continue;
                }

                // 计算组合评分
                let mut score = 0.0;

                // 正文与背景对比度越高越好
                let text_contrast = contrast_ratio(entries[text_idx].rgb, bg_rgb);
                score += text_contrast * 0.3;

                // 强调色与背景对比度
                let accent_contrast = contrast_ratio(entries[accent_idx].rgb, bg_rgb);
                score += accent_contrast * 0.2;

                // 强调色色度越高越好
                score += entries[accent_idx].chroma * c.smart_chroma_bonus;

                // 两两 OKLab 距离（避免选到太接近的颜色）
                let d_text_accent = delta_e(entries[text_idx].oklab, entries[accent_idx].oklab);
                score += d_text_accent * c.smart_pair_distance_bonus;

                // 候选可靠度（占比越高越可靠）
                score += entries[accent_idx].ratio * 10.0;
                score += entries[text_idx].ratio * 5.0;

                // 构建颜色列表
                let mut colors = vec![bg_rgb];
                let mut seen = std::collections::HashSet::new();
                seen.insert(bg_rgb);

                // 表面色
                let surface_rgb = if let Some(&s_idx) = surface_candidates.first() {
                    if seen.insert(entries[s_idx].rgb) {
                        entries[s_idx].rgb
                    } else {
                        bg_rgb
                    }
                } else {
                    bg_rgb
                };
                if surface_rgb != bg_rgb {
                    colors.push(surface_rgb);
                }

                // 正文
                if seen.insert(entries[text_idx].rgb) {
                    colors.push(entries[text_idx].rgb);
                }

                // 主强调
                if seen.insert(entries[accent_idx].rgb) {
                    colors.push(entries[accent_idx].rgb);
                }

                // 次强调（可选）：从 accent_candidates 中选与主强调距离最大的
                if accent_candidates.len() > 1 {
                    let mut best_secondary: Option<(usize, f64)> = None;
                    for &sec_idx in &accent_candidates {
                        if sec_idx == accent_idx || sec_idx == text_idx {
                            continue;
                        }
                        let d = delta_e(entries[sec_idx].oklab, entries[accent_idx].oklab);
                        if best_secondary.is_none_or(|(_, s)| d > s) {
                            best_secondary = Some((sec_idx, d));
                        }
                    }
                    if let Some((sec_idx, _)) = best_secondary
                        && seen.insert(entries[sec_idx].rgb)
                    {
                        colors.push(entries[sec_idx].rgb);
                    }
                }

                let scheme = SmartPairingResult {
                    colors: colors.clone(),
                    score,
                };

                let is_better = match &best_scheme {
                    Some(prev) => scheme.score > prev.score,
                    None => true,
                };
                if is_better {
                    best_scheme = Some(scheme);
                }
            }
        }
    }

    match best_scheme {
        Some(scheme) => HarmonyScheme {
            label: "原图智能搭配".into(),
            scheme: "smart_pairing".into(),
            description: "从原图真实像素中选取的角色色板".into(),
            colors: scheme.colors,
            source_kind: "extraction".into(),
            confidence: 1.0,
        },
        None => {
            // 降级：无法满足 WCAG 硬约束，返回低置信结果（引用均衡配色已有颜色，不发明新颜色）
            let degraded_colors: Vec<Rgb> = roles.iter().take(3).map(|r| r.rgb).collect();
            HarmonyScheme {
                label: "原图智能搭配".into(),
                scheme: "smart_pairing".into(),
                description: "原图无可读组合，已降级".into(),
                colors: degraded_colors,
                source_kind: "extraction".into(),
                confidence: c.smart_degraded_confidence,
            }
        }
    }
}

/// 智能搭配中间结果
struct SmartPairingResult {
    colors: Vec<Rgb>,
    score: f64,
}

// ── 完整配色分析入口 ──────────────────────────────────────────────────────

/// 从 RGBA 像素数据执行完整配色分析（默认 OKLab k-means）。
///
/// 输入为 flat RGBA 数据。
/// 透明度 < MIN_ALPHA 的像素跳过。
/// 空样本返回 `empty: true`。
pub fn analyze_palette(rgba_flat: &[u8], width: usize, height: usize) -> PaletteResult {
    analyze_palette_with(rgba_flat, width, height, ClusteringAlgo::default())
}

/// 指定聚类算法的完整配色分析入口。
pub fn analyze_palette_with(
    rgba_flat: &[u8],
    width: usize,
    height: usize,
    algo: ClusteringAlgo,
) -> PaletteResult {
    analyze_palette_histogram(&build_color_histogram(rgba_flat), width, height, algo)
}

/// 从直方图执行聚类与设计分析
pub fn analyze_palette_histogram(
    histogram: &ColorHistogram,
    width: usize,
    height: usize,
    algo: ClusteringAlgo,
) -> PaletteResult {
    if histogram.valid_pixels == 0 || histogram.counts.is_empty() {
        return PaletteResult {
            roles: vec![],
            theme: analyze_theme(&[]),
            sample: SampleInfo {
                width,
                height,
                valid_pixels: 0,
                scanned_pixels: 0,
                mode: "full".into(),
            },
            recommended: vec![],
            full: vec![],
            empty: true,
        };
    }

    let entries = histogram.to_entries();
    let distinct_count = entries.len();
    let c = PALETTE_ALGORITHM_V1;
    let clusters = match algo {
        ClusteringAlgo::Kmeans => {
            let k = if distinct_count <= c.k_max {
                distinct_count
            } else {
                c.k_max
                    .min(c.k_min.max((distinct_count as f64).sqrt().round() as usize))
            };
            weighted_kmeans_cluster(&entries, k)
        }
        ClusteringAlgo::MeanShift => mean_shift_cluster(&entries),
        ClusteringAlgo::Mmcq => mmcq_cluster(&entries),
    };
    let roles = assign_roles(clusters);

    // 默认三组分别回答：整体是什么色、哪里最醒目、怎样组成可用设计色板。
    let source_scheme = HarmonyScheme {
        label: "图片主题色".into(),
        scheme: "source".into(),
        description: "来自整块选区的聚类原色".into(),
        colors: roles.iter().map(|r| r.rgb).collect(),
        source_kind: "extraction".into(),
        confidence: 1.0,
    };
    let mut recommended = vec![source_scheme];
    recommended.extend(extract_design_schemes(&entries, &roles));

    PaletteResult {
        roles: roles.clone(),
        theme: analyze_theme(&roles),
        sample: SampleInfo {
            width,
            height,
            valid_pixels: histogram.valid_pixels,
            scanned_pixels: histogram.scanned_pixels,
            mode: histogram.mode.clone(),
        },
        recommended,
        full: vec![],
        empty: false,
    }
}

// ── 输出格式生成 ──────────────────────────────────────────────────────────

/// 将角色色列表输出为纯颜色列表（每行一个 HEX）
#[allow(dead_code)] // 输出格式工具，待 CLI 导出消费
pub fn format_as_list(roles: &[RoleColor]) -> String {
    roles
        .iter()
        .map(|r| r.hex.as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

// ── 测试 ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── OKLab 转换测试（fixture 对拍）──────────────────────────────────────

    #[derive(serde::Deserialize)]
    struct OklabFixture {
        oklab_cases: Vec<OklabCase>,
    }

    #[derive(serde::Deserialize)]
    struct OklabCase {
        name: String,
        rgba: [u8; 3],
        oklab: [f64; 3],
        tolerance: f64,
    }

    #[test]
    fn oklab_fixture_consistency() {
        let path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata/palette-fixtures.json");
        if !path.exists() {
            eprintln!("跳过 fixture 测试：{} 不存在", path.display());
            return;
        }

        let content = std::fs::read_to_string(&path).unwrap();
        let fixture: OklabFixture = serde_json::from_str(&content).unwrap();

        for case in &fixture.oklab_cases {
            let result = rgb_to_oklab(case.rgba[0], case.rgba[1], case.rgba[2]);
            for (i, (got, expected)) in result.iter().zip(case.oklab.iter()).enumerate() {
                assert!(
                    (got - expected).abs() < case.tolerance,
                    "OKLab[{}] mismatch for {}: expected {}, got {}",
                    i,
                    case.name,
                    expected,
                    got
                );
            }
        }
        eprintln!(
            "OKLab fixture 对拍通过: {} cases",
            fixture.oklab_cases.len()
        );
    }

    // ── Roundtrip 测试（fixture 对拍）──────────────────────────────────────

    #[derive(serde::Deserialize)]
    struct RoundtripFixture {
        roundtrip_cases: Vec<RoundtripCase>,
    }

    #[derive(serde::Deserialize)]
    struct RoundtripCase {
        name: String,
        rgba: [u8; 3],
        tolerance: u8,
    }

    #[test]
    fn roundtrip_fixture_consistency() {
        let path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata/palette-fixtures.json");
        if !path.exists() {
            eprintln!("跳过 fixture 测试：{} 不存在", path.display());
            return;
        }

        let content = std::fs::read_to_string(&path).unwrap();
        let fixture: RoundtripFixture = serde_json::from_str(&content).unwrap();

        for case in &fixture.roundtrip_cases {
            let lab = rgb_to_oklab(case.rgba[0], case.rgba[1], case.rgba[2]);
            let back = oklab_to_rgb(lab);
            for (i, (got, expected)) in back.iter().zip(case.rgba.iter()).enumerate() {
                let diff = (*got as i16 - *expected as i16).unsigned_abs() as u8;
                assert!(
                    diff <= case.tolerance,
                    "Roundtrip {} channel {} mismatch: expected ~{}, got {}, tolerance {}",
                    case.name,
                    i,
                    expected,
                    got,
                    case.tolerance
                );
            }
        }
        eprintln!(
            "Roundtrip fixture 对拍通过: {} cases",
            fixture.roundtrip_cases.len()
        );
    }

    // ── Palette 测试用例（fixture 对拍）───────────────────────────────────

    #[derive(serde::Deserialize)]
    struct PaletteFixture {
        palette_test_cases: Vec<PaletteCase>,
    }

    #[derive(serde::Deserialize)]
    struct PaletteCase {
        name: String,
        #[allow(dead_code)]
        description: String,
        rgba_flat: Vec<u8>,
        width: usize,
        height: usize,
        expect_empty: bool,
        expect_min_roles: Option<usize>,
        expect_exact_roles: Option<usize>,
        expect_has_background: Option<bool>,
        expect_theme_family: Option<String>,
        expect_theme_lightness: Option<String>,
        /// P3：智能搭配是否期望降级（confidence < 1.0）
        expect_smart_pairing_degraded: Option<bool>,
    }

    #[test]
    fn palette_fixture_consistency() {
        let path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata/palette-fixtures.json");
        if !path.exists() {
            eprintln!("跳过 fixture 测试：{} 不存在", path.display());
            return;
        }

        let content = std::fs::read_to_string(&path).unwrap();
        let fixture: PaletteFixture = serde_json::from_str(&content).unwrap();

        for case in &fixture.palette_test_cases {
            let result = analyze_palette(&case.rgba_flat, case.width, case.height);

            assert_eq!(
                result.empty, case.expect_empty,
                "case '{}': empty mismatch (expected {}, got {})",
                case.name, case.expect_empty, result.empty
            );

            if case.expect_empty {
                continue;
            }

            if let Some(min_roles) = case.expect_min_roles {
                assert!(
                    result.roles.len() >= min_roles,
                    "case '{}': roles count {} < expected min {}",
                    case.name,
                    result.roles.len(),
                    min_roles
                );
            }

            if let Some(expect_bg) = case.expect_has_background {
                let has_bg = result.roles.iter().any(|r| r.role == "background");
                assert_eq!(
                    has_bg, expect_bg,
                    "case '{}': has_background mismatch",
                    case.name
                );
            }

            // P2：强断言 — 精确角色数
            if let Some(exact) = case.expect_exact_roles {
                assert_eq!(
                    result.roles.len(),
                    exact,
                    "case '{}': exact roles mismatch (expected {}, got {})",
                    case.name,
                    exact,
                    result.roles.len()
                );
            }

            // P2：强断言 — 主题色系
            if let Some(ref family) = case.expect_theme_family {
                assert_eq!(
                    result.theme.family, *family,
                    "case '{}': theme family mismatch (expected '{}', got '{}')",
                    case.name, family, result.theme.family
                );
            }

            // P2：强断言 — 主题明度
            if let Some(ref lightness) = case.expect_theme_lightness {
                assert_eq!(
                    result.theme.lightness, *lightness,
                    "case '{}': theme lightness mismatch (expected '{}', got '{}')",
                    case.name, lightness, result.theme.lightness
                );
            }

            // P3：确定性断言——同一输入两次分析结果必须完全相同
            let result2 = analyze_palette(&case.rgba_flat, case.width, case.height);
            assert_eq!(
                result.roles.len(),
                result2.roles.len(),
                "case '{}': 确定性失败——两次分析角色数不一致",
                case.name
            );
            for (r1, r2) in result.roles.iter().zip(result2.roles.iter()) {
                assert_eq!(
                    r1.rgb, r2.rgb,
                    "case '{}': 确定性失败——角色 RGB 不一致",
                    case.name
                );
                assert_eq!(
                    r1.role, r2.role,
                    "case '{}': 确定性失败——角色分配不一致",
                    case.name
                );
            }

            // P3：像素可寻断言——所有角色色必须是输入直方图中出现过的真实像素
            let histogram = build_color_histogram(&case.rgba_flat);
            let input_rgbs: std::collections::HashSet<Rgb> =
                histogram.colors.iter().copied().collect();
            for role in &result.roles {
                assert!(
                    input_rgbs.contains(&role.rgb),
                    "case '{}': 像素可寻失败——角色色 {:?} 不在输入直方图中",
                    case.name,
                    role.rgb
                );
            }

            // P3：智能搭配方案断言
            let smart_pairing = result
                .recommended
                .iter()
                .find(|s| s.scheme == "smart_pairing");
            if let Some(scheme) = smart_pairing {
                // 像素可寻：智能搭配的所有颜色也必须是真实像素
                for rgb in &scheme.colors {
                    assert!(
                        input_rgbs.contains(rgb),
                        "case '{}': 智能搭配像素可寻失败——颜色 {:?} 不在输入直方图中",
                        case.name,
                        rgb
                    );
                }

                // 降级状态断言
                if let Some(expect_degraded) = case.expect_smart_pairing_degraded {
                    let is_degraded = scheme.confidence < 1.0;
                    assert_eq!(
                        is_degraded, expect_degraded,
                        "case '{}': smart_pairing degraded mismatch (expected {}, got {})",
                        case.name, expect_degraded, is_degraded
                    );
                }
            } else if case.expect_smart_pairing_degraded == Some(false) {
                // 期望非降级但方案不存在
                panic!("case '{}': 期望 smart_pairing 方案存在但未找到", case.name);
            }
        }
        eprintln!(
            "Palette fixture 对拍通过: {} cases",
            fixture.palette_test_cases.len()
        );
    }

    // ── 纯函数单测 ─────────────────────────────────────────────────────────

    #[test]
    fn pure_white_oklab() {
        let lab = rgb_to_oklab(255, 255, 255);
        eprintln!("pure_white_oklab: lab = {lab:?}");
        assert!((lab[0] - 1.0).abs() < 0.001);
        assert!((lab[1]).abs() < 0.001);
        assert!((lab[2]).abs() < 0.001);
    }

    #[test]
    fn pure_black_oklab() {
        let lab = rgb_to_oklab(0, 0, 0);
        assert!((lab[0]).abs() < 0.001);
    }

    #[test]
    fn contrast_black_white_is_21() {
        let ratio = contrast_ratio([0, 0, 0], [255, 255, 255]);
        assert!((ratio - 21.0).abs() < 0.01);
    }

    #[test]
    fn recommend_dark_on_light_background() {
        let info = recommend_text_color([240, 240, 240]);
        assert_eq!(info.text_color, "dark");
        assert!(info.ratio >= 4.5);
    }

    #[test]
    fn recommend_light_on_dark_background() {
        let info = recommend_text_color([18, 18, 18]);
        assert_eq!(info.text_color, "light");
        assert!(info.ratio >= 4.5);
    }

    #[test]
    fn empty_pixels_return_empty_result() {
        let result = analyze_palette(&[], 0, 0);
        assert!(result.empty);
        assert!(result.roles.is_empty());
    }

    #[test]
    fn all_transparent_pixels_return_empty_result() {
        let rgba = [255, 0, 0, 0, 0, 255, 0, 0, 0, 0, 255, 0];
        let result = analyze_palette(&rgba, 2, 2);
        assert!(result.empty);
    }

    #[test]
    fn solid_color_has_background() {
        let rgba = [
            240, 240, 240, 255, 240, 240, 240, 255, 240, 240, 240, 255, 240, 240, 240, 255,
        ];
        let result = analyze_palette(&rgba, 2, 2);
        assert!(!result.empty);
        assert!(result.roles.iter().any(|r| r.role == "background"));
    }

    #[test]
    fn rgb_to_hex_uppercase() {
        assert_eq!(rgb_to_hex(255, 0, 0), "#FF0000");
        assert_eq!(rgb_to_hex(0, 255, 0), "#00FF00");
        assert_eq!(rgb_to_hex(0, 0, 255), "#0000FF");
    }

    #[test]
    fn delta_e_zero_for_same_color() {
        let lab = rgb_to_oklab(100, 150, 200);
        assert!(delta_e(lab, lab).abs() < 0.0001);
    }

    #[test]
    fn oklch_roundtrip_preserves_l() {
        let lab = rgb_to_oklab(100, 150, 200);
        let lch = oklab_to_oklch(lab);
        let back = oklch_to_oklab(lch);
        assert!((lab[0] - back[0]).abs() < 0.001);
    }

    #[test]
    fn gamut_map_produces_valid_srgb() {
        // 极端色度颜色，必须映射回 sRGB
        let lab = [0.5, 0.5, 0.5];
        let mapped = gamut_map_oklab(lab);
        let rgb = oklab_to_rgb(mapped);
        // roundtrip 应该稳定
        let _back = rgb_to_oklab(rgb[0], rgb[1], rgb[2]);
    }

    #[test]
    fn format_as_list_produces_hex_lines() {
        let roles = vec![RoleColor {
            rgb: [255, 0, 0],
            role: "background".into(),
            ratio: 1.0,
            oklab: [0.5, 0.2, 0.1],
            hex: "#FF0000".into(),
        }];
        let result = format_as_list(&roles);
        assert_eq!(result, "#FF0000");
    }

    #[test]
    fn salient_includes_forced_bg_hue_when_no_dominant_color() {
        // 回归：多色均分、无主导背景时，最亮的黄会被降级指派为 background。
        // 视觉焦点色检测不应因此把背景自身的亮黄排除（0.21.15 修复亮黄漏检）。
        let mut rgba = vec![128u8; 4 * 12 * 6];
        let hues: [[u8; 3]; 6] = [
            [220, 50, 50],  // 红
            [240, 140, 40], // 橙
            [255, 224, 32], // 亮黄
            [60, 180, 70],  // 绿
            [60, 80, 220],  // 蓝
            [160, 60, 200], // 紫
        ];
        for row in 0..6 {
            for col in 0..12 {
                let i = (row * 12 + col) * 4;
                let c = hues[col / 2];
                rgba[i] = c[0];
                rgba[i + 1] = c[1];
                rgba[i + 2] = c[2];
                rgba[i + 3] = 255;
            }
        }

        let result = analyze_palette(&rgba, 12, 6);
        assert!(!result.empty);

        // 前提：无 35% 主导色，亮黄（明度最高）被降级指派为背景
        let bg = result.roles.iter().find(|r| r.role == "background");
        assert!(
            bg.is_some_and(|r| r.hex == "#FFE020"),
            "多色均分图应将最亮的黄标为背景（降级指派），实际 roles: {:?}",
            result.roles
        );

        // 修复点：视觉焦点色必须包含该亮黄
        let salient = result
            .recommended
            .iter()
            .find(|s| s.scheme == "salient")
            .expect("应存在 salient 方案");
        let salient_hex: Vec<String> = salient
            .colors
            .iter()
            .map(|rgb| rgb_to_hex(rgb[0], rgb[1], rgb[2]))
            .collect();
        assert!(
            salient_hex.contains(&"#FFE020".to_string()),
            "salient 应包含亮黄 #FFE020，实际: {:?}",
            salient_hex
        );
    }

    #[test]
    fn real_image_lyzl_analysis_is_stable() {
        // 真实图片回归锚：蓝色系插画，含零散亮黄点缀（testdata/LYZL.png）。
        // 验证真实 PNG → 解码 → analyze_palette 全链路无 panic、输出确定。
        // 该图亮黄像素仅约 0.7% 且零散分布；显著对比度信号改为 HC 全局唯一性
        // （相对全图分布而非单点背景反差）后，亮黄应进入视觉焦点色（见下方断言）。
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata/LYZL.png");
        assert!(path.exists(), "fixture 缺失: {}", path.display());
        let bytes = std::fs::read(&path).unwrap();
        let (rgba, w, h) =
            crate::infra::platform::screenshot::decode_png_to_rgba(&bytes).expect("PNG 解码失败");
        assert!(w > 0 && h > 0);

        let result = analyze_palette(&rgba, w as usize, h as usize);
        assert!(!result.empty);
        assert!(!result.roles.is_empty());
        assert!(result.roles.iter().any(|r| r.role == "background"));
        // 该图为蓝色系插画（hue_concentration 高）
        assert_eq!(result.theme.family, "蓝色系");

        // 视觉焦点色非空
        let salient = result
            .recommended
            .iter()
            .find(|s| s.scheme == "salient")
            .expect("应存在 salient 方案");
        assert!(!salient.colors.is_empty());

        // HC 全局唯一性显著：零散亮黄点缀（占比 <0.1%）应进入视觉焦点色。
        // 对比度信号从"对背景单点反差"换为相对全图分布后，不再被降级指派的
        // 背景（该图无主导色，最亮黄被降级标为背景）误压。
        let yellow_present = salient.colors.iter().any(|rgb| {
            let (r, g, b) = (rgb[0], rgb[1], rgb[2]);
            r > 180 && g > 140 && b < 140
        });
        assert!(
            yellow_present,
            "亮黄点缀应进入视觉焦点色，实际 salient: {:?}",
            salient.colors
        );

        // 确定性：同输入两次分析完全一致
        let result2 = analyze_palette(&rgba, w as usize, h as usize);
        assert_eq!(result.roles.len(), result2.roles.len());
        for (r1, r2) in result.roles.iter().zip(result2.roles.iter()) {
            assert_eq!(r1.rgb, r2.rgb);
            assert_eq!(r1.role, r2.role);
        }
        let salient2 = result2
            .recommended
            .iter()
            .find(|s| s.scheme == "salient")
            .unwrap();
        assert_eq!(salient.colors, salient2.colors);

        // 像素可寻：所有角色色来自原图直方图的真实像素
        let hist = build_color_histogram(&rgba);
        let input_rgbs: std::collections::HashSet<Rgb> = hist.colors.iter().copied().collect();
        for role in &result.roles {
            assert!(
                input_rgbs.contains(&role.rgb),
                "角色色 {} 不在原图直方图中",
                role.hex
            );
        }
    }

    // ── 新聚类算法（Mean-shift / MMCQ）单测 ───────────────────────────────

    /// 生成"深底 + 少量高饱和红点缀"测试像素（alpha 255）
    fn accent_sample() -> Vec<u8> {
        let mut rgba = vec![30u8; 4 * 15 * 3];
        for px in 0..15 * 3 {
            let i = px * 4;
            rgba[i + 3] = 255;
            if px % 15 == 8 {
                rgba[i] = 255;
                rgba[i + 1] = 0;
                rgba[i + 2] = 0;
            }
        }
        rgba
    }

    #[test]
    fn mean_shift_small_accent_roles() {
        // 深底 + 红点缀：Mean-shift 自动聚类应产出背景 + 红强调
        let rgba = accent_sample();
        let res = analyze_palette_with(&rgba, 15, 3, ClusteringAlgo::MeanShift);
        assert!(!res.empty);
        assert!(
            res.roles.iter().any(|r| r.role == "background"),
            "roles: {:?}",
            res.roles
        );
        let red = res.roles.iter().find(|r| r.rgb == [255, 0, 0]);
        assert!(red.is_some(), "红色点缀应为角色色之一: {:?}", res.roles);
        assert_eq!(red.unwrap().role, "accent");
    }

    #[test]
    fn mmcq_small_accent_roles() {
        let rgba = accent_sample();
        let res = analyze_palette_with(&rgba, 15, 3, ClusteringAlgo::Mmcq);
        assert!(!res.empty);
        assert!(res.roles.iter().any(|r| r.role == "background"));
        let red = res.roles.iter().find(|r| r.rgb == [255, 0, 0]);
        assert!(red.is_some(), "红色点缀应为角色色之一: {:?}", res.roles);
        assert_eq!(red.unwrap().role, "accent");
    }

    #[test]
    fn mean_shift_cluster_contract_and_determinism() {
        // 契约：count 降序、rgb 像素可寻、ratio 归一、两次一致
        let mut rgba = vec![128u8; 4 * 12 * 6];
        let hues: [[u8; 3]; 6] = [
            [220, 50, 50],  // 红
            [240, 140, 40], // 橙
            [255, 224, 32], // 亮黄
            [60, 180, 70],  // 绿
            [60, 80, 220],  // 蓝
            [160, 60, 200], // 紫
        ];
        for row in 0..6 {
            for col in 0..12 {
                let i = (row * 12 + col) * 4;
                let c = hues[col / 2];
                rgba[i] = c[0];
                rgba[i + 1] = c[1];
                rgba[i + 2] = c[2];
                rgba[i + 3] = 255;
            }
        }
        let hist = build_color_histogram(&rgba);
        let entries = hist.to_entries();
        let input_rgbs: std::collections::HashSet<Rgb> = hist.colors.iter().copied().collect();

        for algo in [ClusteringAlgo::MeanShift, ClusteringAlgo::Mmcq] {
            let clusters = match algo {
                ClusteringAlgo::MeanShift => mean_shift_cluster(&entries),
                ClusteringAlgo::Mmcq => mmcq_cluster(&entries),
                ClusteringAlgo::Kmeans => unreachable!(),
            };
            assert!(!clusters.is_empty(), "{algo:?} 不应为空");
            // count 降序
            for w in clusters.windows(2) {
                assert!(
                    w[0].count >= w[1].count,
                    "{algo:?} clusters 应按 count 降序"
                );
            }
            // rgb 像素可寻 + 确定性
            for c in &clusters {
                assert!(
                    input_rgbs.contains(&c.rgb),
                    "{algo:?} cluster rgb {:?} 不在直方图中",
                    c.rgb
                );
            }
            let clusters2 = match algo {
                ClusteringAlgo::MeanShift => mean_shift_cluster(&entries),
                ClusteringAlgo::Mmcq => mmcq_cluster(&entries),
                ClusteringAlgo::Kmeans => unreachable!(),
            };
            assert_eq!(clusters.len(), clusters2.len(), "{algo:?} 确定性失败");
            for (a, b) in clusters.iter().zip(clusters2.iter()) {
                assert_eq!(a.rgb, b.rgb, "{algo:?} 确定性失败 rgb");
                assert_eq!(a.count, b.count, "{algo:?} 确定性失败 count");
            }
        }
    }

    #[test]
    fn all_algos_lyzl_run_and_deterministic() {
        // LYZL 全链路：三种算法都不 panic、输出确定、主题色系一致（蓝色系）
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata/LYZL.png");
        let bytes = std::fs::read(&path).unwrap();
        let (rgba, w, h) =
            crate::infra::platform::screenshot::decode_png_to_rgba(&bytes).expect("PNG 解码失败");

        for algo in [
            ClusteringAlgo::Kmeans,
            ClusteringAlgo::MeanShift,
            ClusteringAlgo::Mmcq,
        ] {
            let r1 = analyze_palette_with(&rgba, w as usize, h as usize, algo);
            assert!(!r1.empty, "{algo:?} LYZL 不应为空");
            assert!(!r1.roles.is_empty(), "{algo:?} 应产出角色色");
            assert_eq!(r1.theme.family, "蓝色系", "{algo:?} 主题色系应一致");

            let r2 = analyze_palette_with(&rgba, w as usize, h as usize, algo);
            assert_eq!(r1.roles.len(), r2.roles.len(), "{algo:?} 确定性失败");
            for (a, b) in r1.roles.iter().zip(r2.roles.iter()) {
                assert_eq!(a.rgb, b.rgb);
                assert_eq!(a.role, b.role);
            }
        }
    }
}
