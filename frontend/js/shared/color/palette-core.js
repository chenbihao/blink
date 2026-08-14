//! 0.20.7：来源无关配色核心。
//!
//! 纯函数模块，无 DOM/Tauri/ss-state/标注引擎依赖。
//! 包含：OKLab 色彩空间转换、DeltaE 距离、确定性 k-means++ 聚类、
//! 角色色分配、harmony 方案生成、WCAG 对比度与推荐文字色、输出格式生成。
//!
//! ## 算法契约
//! - 输入：RGBA8 像素数组（flat 或数组）+ 来源元数据
//! - 输出：确定性角色色、占比、对比度、推荐文字色、推荐方案及完整 harmony
//! - 透明度 < 0.5 的像素跳过；空样本返回结构化无结果
//! - 使用 OKLab、确定性带权 k-means seed；K=1..8，最大 32 次迭代
//! - 聚类中心映射到最近真实原图像素
//! - 常量集中在 PALETTE_ALGORITHM_V1

// ── 常量 ─────────────────────────────────────────────────────────────────

/**
 * 算法常量（唯一真源，禁止散落 UI）。
 * 变更阈值必须更新 fixture 说明。
 */
export const PALETTE_ALGORITHM_V1 = Object.freeze({
  MIN_ALPHA: 0.5,                // alpha < 0.5 的像素跳过
  K_MIN: 3,                      // 最小聚类数
  K_MAX: 8,                      // 最大聚类数
  MAX_ITERATIONS: 32,            // k-means 最大迭代
  CONVERGENCE_EPSILON: 0.001,    // 中心位移小于此值时提前停止
  DEBOUNCE_MS: 120,              // 选区变化后防抖
  // rgbToOklab() 返回未乘 100 的 OKLab，欧氏距离通常处于 0..1；
  // 这里必须使用同一量级。5.0 会把所有颜色无条件合并成一个聚类。
  NEAR_COLOR_MERGE_DELTA_E: 0.05, // 近色合并阈值（OKLab 欧氏距离）
  ACCENT_MIN_RATIO: 0.02,         // 点缀最小占比
  BACKGROUND_MIN_RATIO: 0.35,     // 背景最小占比
  WORKER_VERSION: 1,
});

// ── 类型定义 ─────────────────────────────────────────────────────────────

/**
 * @typedef {number[]} RgbaFlat - [r,g,b,a, r,g,b,a, ...] flat 数组
 * @typedef {{r: number, g: number, b: number, a: number}} Rgba8
 * @typedef {[number, number, number]} OkLab - [L, a, b]
 * @typedef {{rgb: number[], oklab: OkLab, count: number, ratio: number}} ClusterResult
 * @typedef {{rgb: number[], role: string, ratio: number, oklab: OkLab, hex: string}} RoleColor
 * @typedef {{label: string, scheme: string, colors: number[][]}} HarmonyScheme
 * @typedef {{ratio: number, textColor: 'dark'|'light'}} ContrastInfo
 * @typedef {{roles: RoleColor[], recommended: HarmonyScheme[], full: HarmonyScheme[], empty: boolean}} PaletteResult
 */

// ── sRGB ↔ 线性 RGB ─────────────────────────────────────────────────────

/**
 * sRGB 通道值 → 线性 RGB（用于 OKLab 转换）。
 * @param {number} c - 0-255
 * @returns {number} 0-1 线性
 */
function srgbToLinear(c) {
  const cs = c / 255;
  return cs <= 0.04045 ? cs / 12.92 : Math.pow((cs + 0.055) / 1.055, 2.4);
}

/**
 * 线性 RGB → sRGB 通道值。
 * @param {number} c - 0-1 线性
 * @returns {number} 0-255
 */
function linearToSrgb(c) {
  // 负线性值和接近 0 的噪声钳制为 0（sRGB 无法表示负光，极小正值是浮点噪声）
  if (c <= 0.0001) return 0;
  if (c >= 1) return 255;
  const cs = c <= 0.0031308 ? c * 12.92 : 1.055 * Math.pow(c, 1 / 2.4) - 0.055;
  return Math.max(0, Math.min(255, Math.round(cs * 255)));
}

// ── RGB → OKLab ──────────────────────────────────────────────────────────

/**
 * sRGB (0-255) → OKLab [L, a, b]。
 *
 * 使用标准 OKLab 算法（Björn Ottosson 2020）：
 * 1. sRGB → 线性 RGB
 * 2. 线性 RGB → LMS（使用 M 矩阵）
 * 3. 立方根
 * 4. LMS' → OKLab（使用 M_inv 矩阵）
 *
 * @param {number} r - 0-255
 * @param {number} g - 0-255
 * @param {number} b - 0-255
 * @returns {OkLab} [L, a, b]
 */
export function rgbToOklab(r, g, b) {
  const lr = srgbToLinear(r);
  const lg = srgbToLinear(g);
  const lb = srgbToLinear(b);

  // 线性 RGB → LMS
  const l = 0.4122214708 * lr + 0.5363325543 * lg + 0.0514459929 * lb;
  const m = 0.2119034982 * lr + 0.6806995451 * lg + 0.1073969566 * lb;
  const s = 0.0883024619 * lr + 0.2812306966 * lg + 0.6299786801 * lb;

  // 立方根
  const l_ = Math.cbrt(l);
  const m_ = Math.cbrt(m);
  const s_ = Math.cbrt(s);

  // LMS' → OKLab
  return [
    0.2104542553 * l_ + 0.7936177850 * m_ + 0.0040767417 * s_,
    1.9779984951 * l_ - 2.4285922050 * m_ + 0.4505937099 * s_,
    0.0259040375 * l_ + 0.7827717663 * m_ - 0.8086757669 * s_,
  ];
}

/**
 * OKLab → sRGB (0-255)。
 * @param {OkLab} lab - [L, a, b]
 * @returns {[number, number, number]} [r, g, b] 0-255
 */
export function oklabToRgb(lab) {
  const [L, a, b] = lab;

  // OKLab → LMS'
  const l_ = L + 0.3963077455 * a + 0.2158043324 * b;
  const m_ = L - 0.1055627838 * a - 0.0638541749 * b;
  const s_ = L - 0.0894829827 * a - 1.2912780446 * b;

  // 立方
  const l = l_ * l_ * l_;
  const m = m_ * m_ * m_;
  const s = s_ * s_ * s_;

// LMS → 线性 RGB（LMS 矩阵的数值逆）
const lr = 4.0766 * l - 3.3074 * m + 0.2309 * s;
const lg = -1.2682 * l + 2.6093 * m - 0.3413 * s;
const lb = -0.0053 * l - 0.7012 * m + 1.7073 * s;

  return [linearToSrgb(lr), linearToSrgb(lg), linearToSrgb(lb)];
}

// ── RGB ↔ HSL（用于 harmony 色相偏移）──────────────────────────────────

/**
 * RGB → HSL。h: 0-360, s/l: 0-100。
 * @param {number} r
 * @param {number} g
 * @param {number} b
 * @returns {[number, number, number]} [h, s, l]
 */
export function rgbToHsl(r, g, b) {
  const rN = r / 255, gN = g / 255, bN = b / 255;
  const max = Math.max(rN, gN, bN);
  const min = Math.min(rN, gN, bN);
  const delta = max - min;
  const l = (max + min) / 2;

  let s = 0, h = 0;
  if (delta !== 0) {
    s = l > 0.5 ? delta / (2 - max - min) : delta / (max + min);
    if (max === rN) h = ((gN - bN) / delta) % 6;
    else if (max === gN) h = (bN - rN) / delta + 2;
    else h = (rN - gN) / delta + 4;
    h *= 60;
    if (h < 0) h += 360;
  }

  return [Math.round(h), Math.round(s * 100), Math.round(l * 100)];
}

/**
 * HSL → RGB。h: 0-360, s/l: 0-100。
 * @param {number} h
 * @param {number} s
 * @param {number} l
 * @returns {[number, number, number]} [r, g, b] 0-255
 */
export function hslToRgb(h, s, l) {
  const sn = s / 100, ln = l / 100;
  const c = (1 - Math.abs(2 * ln - 1)) * sn;
  const x = c * (1 - Math.abs(((h / 60) % 2) - 1));
  const m = ln - c / 2;
  let r, g, b;
  if (h < 60) { r = c; g = x; b = 0; }
  else if (h < 120) { r = x; g = c; b = 0; }
  else if (h < 180) { r = 0; g = c; b = x; }
  else if (h < 240) { r = 0; g = x; b = c; }
  else if (h < 300) { r = x; g = 0; b = c; }
  else { r = c; g = 0; b = x; }
  return [
    Math.round((r + m) * 255),
    Math.round((g + m) * 255),
    Math.round((b + m) * 255),
  ];
}

// ── RGB → HEX ────────────────────────────────────────────────────────────

/**
 * RGB → HEX 字符串（#RRGGBB，大写）。
 * @param {number} r
 * @param {number} g
 * @param {number} b
 * @returns {string}
 */
export function rgbToHex(r, g, b) {
  const h = (v) => Math.max(0, Math.min(255, Math.round(v))).toString(16).padStart(2, '0').toUpperCase();
  return `#${h(r)}${h(g)}${h(b)}`;
}

// ── DeltaE（OKLab 空间欧氏距离）─────────────────────────────────────────

/**
 * 计算两个 OKLab 颜色之间的 DeltaE 距离。
 * 使用 OKLab 空间的欧氏距离，与视觉感知更一致。
 * @param {OkLab} lab1
 * @param {OkLab} lab2
 * @returns {number}
 */
export function deltaE(lab1, lab2) {
  const dL = lab1[0] - lab2[0];
  const da = lab1[1] - lab2[1];
  const db = lab1[2] - lab2[2];
  return Math.sqrt(dL * dL + da * da + db * db);
}

// ── WCAG 对比度 ──────────────────────────────────────────────────────────

/**
 * 计算 sRGB 通道的相对亮度分量。
 * @param {number} c - 0-255
 * @returns {number} 0-1
 */
function relativeLuminanceChannel(c) {
  const cs = c / 255;
  return cs <= 0.03928 ? cs / 12.92 : Math.pow((cs + 0.055) / 1.055, 2.4);
}

/**
 * 计算颜色的 WCAG 相对亮度。
 * @param {number[]} rgb - [r, g, b]
 * @returns {number} 0-1
 */
export function relativeLuminance(rgb) {
  const [r, g, b] = rgb;
  return 0.2126 * relativeLuminanceChannel(r)
    + 0.7152 * relativeLuminanceChannel(g)
    + 0.0722 * relativeLuminanceChannel(b);
}

/**
 * 计算 WCAG 2.1 对比度比值。
 * 返回值范围 1.0 (相同色) 到 21.0 (黑白)。
 *
 * @param {number[]} rgb1 - [r, g, b]
 * @param {number[]} rgb2 - [r, g, b]
 * @returns {number}
 */
export function contrastRatio(rgb1, rgb2) {
  const l1 = relativeLuminance(rgb1);
  const l2 = relativeLuminance(rgb2);
  const lighter = Math.max(l1, l2);
  const darker = Math.min(l1, l2);
  return (lighter + 0.05) / (darker + 0.05);
}

/**
 * 给定背景色，推荐前景文字色（黑或白）。
 * 选择对比度更高的方向。
 *
 * @param {number[]} bgRgb - 背景色 [r, g, b]
 * @returns {ContrastInfo} { ratio, textColor }
 */
export function recommendTextColor(bgRgb) {
  const black = [0, 0, 0];
  const white = [255, 255, 255];
  const blackContrast = contrastRatio(black, bgRgb);
  const whiteContrast = contrastRatio(white, bgRgb);
  if (blackContrast >= whiteContrast) {
    return { ratio: blackContrast, textColor: 'dark' };
  }
  return { ratio: whiteContrast, textColor: 'light' };
}

/**
 * 给定前景和背景色，返回对比度和推荐文字色方向。
 * @param {number[]} fgRgb - 前景色
 * @param {number[]} bgRgb - 背景色
 * @returns {ContrastInfo}
 */
export function contrastWithRecommendation(fgRgb, bgRgb) {
  const ratio = contrastRatio(fgRgb, bgRgb);
  // 如果前景是暗色，推荐 dark；前景是亮色推荐 light
  const fgLum = relativeLuminance(fgRgb);
  return {
    ratio,
    textColor: fgLum < 0.5 ? 'dark' : 'light',
  };
}

// ── 确定性 k-means++ 聚类 ────────────────────────────────────────────────

/**
 * 确定性伪随机数生成器（mulberry32）。
 * 给定相同种子产生相同序列，确保聚类结果可复现。
 *
 * @param {number} seed
 * @returns {() => number} 返回 0-1 之间的函数
 */
function mulberry32(seed) {
  let a = seed >>> 0;
  return function () {
    a |= 0;
    a = (a + 0x6d2b79f5) | 0;
    let t = Math.imul(a ^ (a >>> 15), 1 | a);
    t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

/**
 * k-means++ 初始化：确定性选择初始聚类中心。
 * 第一个中心选第一个像素，后续中心按距离加权概率选择。
 *
 * @param {OkLab[]} pixels - OKLab 像素数组
 * @param {number} k - 聚类数
 * @param {number} seed - 随机种子
 * @returns {OkLab[]} 初始中心数组
 */
function kMeansPlusPlusInit(pixels, k, seed) {
  if (pixels.length === 0) return [];
  if (pixels.length <= k) return pixels.slice();

  const rng = mulberry32(seed);
  const centers = [pixels[0]]; // 第一个中心确定性地选第一个

  for (let c = 1; c < k; c++) {
    // 计算每个像素到最近中心的距离
    const dists = new Float64Array(pixels.length);
    let total = 0;
    for (let i = 0; i < pixels.length; i++) {
      let minDist = Infinity;
      for (let j = 0; j < centers.length; j++) {
        const d = deltaE(pixels[i], centers[j]);
        if (d < minDist) minDist = d;
      }
      dists[i] = minDist;
      total += minDist;
    }

    if (total === 0) {
      // 所有点都相同
      centers.push(pixels[0]);
      continue;
    }

    // 按距离加权概率选择下一个中心
    const r = rng() * total;
    let acc = 0;
    let chosen = 0;
    for (let i = 0; i < pixels.length; i++) {
      acc += dists[i];
      if (acc >= r) {
        chosen = i;
        break;
      }
    }
    centers.push(pixels[chosen]);
  }

  return centers;
}

/**
 * 确定性 k-means 聚类。
 *
 * - 在 OKLab 空间计算距离
 * - 使用 k-means++ 确定性初始化
 * - 聚类中心映射到最近真实输入像素
 * - 近色合并和点缀保留
 *
 * @param {number[][]} pixels - [[r,g,b], ...] RGB 像素
 * @param {number} k - 目标聚类数
 * @param {number} [seed=42] - 随机种子（默认 42，确保可复现）
 * @returns {ClusterResult[]} 聚类结果，按占比降序
 */
export function kMeansCluster(pixels, k, seed = 42) {
  const C = PALETTE_ALGORITHM_V1;
  if (pixels.length === 0) return [];

  // 钳制 K 范围
  k = Math.max(C.K_MIN, Math.min(C.K_MAX, k));

  // 像素数不足时去重返回
  if (pixels.length <= k) {
    const seen = new Map();
    const results = [];
    for (const p of pixels) {
      const key = (p[0] << 16) | (p[1] << 8) | p[2];
      if (seen.has(key)) {
        seen.get(key).count++;
      } else {
        const oklab = rgbToOklab(p[0], p[1], p[2]);
        seen.set(key, { rgb: p, oklab, count: 1 });
      }
    }
    for (const v of seen.values()) {
      results.push({ ...v, ratio: v.count / pixels.length });
    }
    results.sort((a, b) => b.count - a.count);
    return results;
  }

  // 预计算 OKLab
  const oklabPixels = pixels.map((p) => rgbToOklab(p[0], p[1], p[2]));

  // k-means++ 初始化
  let centers = kMeansPlusPlusInit(oklabPixels, k, seed);
  k = centers.length;

  // 迭代
  for (let iter = 0; iter < C.MAX_ITERATIONS; iter++) {
    // 分配
    const clusters = Array.from({ length: k }, () => []);
    for (let i = 0; i < oklabPixels.length; i++) {
      const p = oklabPixels[i];
      let minDist = Infinity;
      let minIdx = 0;
      for (let c = 0; c < k; c++) {
        const d = deltaE(p, centers[c]);
        if (d < minDist) {
          minDist = d;
          minIdx = c;
        }
      }
      clusters[minIdx].push(i);
    }

    // 更新中心
    let moved = false;
    const newCenters = [];
    for (let c = 0; c < k; c++) {
      const cluster = clusters[c];
      if (cluster.length === 0) {
        newCenters.push(centers[c]);
        continue;
      }
      let sumL = 0, sumA = 0, sumB = 0;
      for (const idx of cluster) {
        const lab = oklabPixels[idx];
        sumL += lab[0];
        sumA += lab[1];
        sumB += lab[2];
      }
      const avg = [sumL / cluster.length, sumA / cluster.length, sumB / cluster.length];
      const shift = deltaE(avg, centers[c]);
      if (shift > C.CONVERGENCE_EPSILON) moved = true;
      newCenters.push(avg);
    }
    centers = newCenters;
    if (!moved) break;
  }

  // 最终分配 + 映射到最近真实像素
  const clusterCounts = new Array(k).fill(0);
  const clusterSums = Array.from({ length: k }, () => [0, 0, 0]);
  // 为每个聚类记录最近的原始 RGB 像素
  const nearestPixel = new Array(k).fill(null);
  const nearestDist = new Array(k).fill(Infinity);

  for (let i = 0; i < oklabPixels.length; i++) {
    const p = oklabPixels[i];
    let minDist = Infinity;
    let minIdx = 0;
    for (let c = 0; c < k; c++) {
      const d = deltaE(p, centers[c]);
      if (d < minDist) {
        minDist = d;
        minIdx = c;
      }
    }
    clusterCounts[minIdx]++;
    clusterSums[minIdx][0] += p[0];
    clusterSums[minIdx][1] += p[1];
    clusterSums[minIdx][2] += p[2];
    // 记录最接近聚类中心的真实像素
    const distToCenter = deltaE(p, centers[minIdx]);
    if (distToCenter < nearestDist[minIdx]) {
      nearestDist[minIdx] = distToCenter;
      nearestPixel[minIdx] = pixels[i];
    }
  }

  const total = pixels.length;
  const results = [];
  for (let c = 0; c < k; c++) {
    if (clusterCounts[c] === 0) continue;
    // 使用聚类中心的 OKLab 作为代表色 OKLab
    const centerOklab = [
      clusterSums[c][0] / clusterCounts[c],
      clusterSums[c][1] / clusterCounts[c],
      clusterSums[c][2] / clusterCounts[c],
    ];
    // 但 RGB 使用最近真实输入像素（保证代表色来自原图）
    results.push({
      rgb: nearestPixel[c],
      oklab: centerOklab,
      count: clusterCounts[c],
      ratio: clusterCounts[c] / total,
    });
  }

  results.sort((a, b) => b.count - a.count);

  // 近色合并
  const merged = mergeNearColors(results);
  return merged;
}

/**
 * 对带权真实颜色做 k-means。输入规模是量化桶数量而非原图像素数量，
 * 聚类中心最终映射回桶内真实出现过的代表 RGB。
 */
export function weightedKMeansCluster(entries, k) {
  const C = PALETTE_ALGORITHM_V1;
  if (!entries.length) return [];
  k = Math.max(1, Math.min(C.K_MAX, k, entries.length));

  const labs = entries.map((entry) => entry.oklab || rgbToOklab(...entry.rgb));
  const centers = [];
  let first = 0;
  for (let i = 1; i < entries.length; i++) {
    if (entries[i].count > entries[first].count) first = i;
  }
  centers.push([...labs[first]]);
  while (centers.length < k) {
    let best = -1;
    let bestScore = -1;
    for (let i = 0; i < entries.length; i++) {
      const minDistance = Math.min(...centers.map((center) => deltaE(labs[i], center)));
      const score = minDistance * minDistance * Math.sqrt(entries[i].count);
      if (score > bestScore) {
        bestScore = score;
        best = i;
      }
    }
    if (best < 0 || bestScore <= 0) break;
    centers.push([...labs[best]]);
  }
  k = centers.length;

  const assignments = new Uint8Array(entries.length);
  for (let iteration = 0; iteration < C.MAX_ITERATIONS; iteration++) {
    const sums = Array.from({ length: k }, () => [0, 0, 0]);
    const weights = new Float64Array(k);
    for (let i = 0; i < entries.length; i++) {
      let nearest = 0;
      let nearestDistance = Infinity;
      for (let c = 0; c < k; c++) {
        const distance = deltaE(labs[i], centers[c]);
        if (distance < nearestDistance) {
          nearestDistance = distance;
          nearest = c;
        }
      }
      assignments[i] = nearest;
      const weight = entries[i].count;
      weights[nearest] += weight;
      sums[nearest][0] += labs[i][0] * weight;
      sums[nearest][1] += labs[i][1] * weight;
      sums[nearest][2] += labs[i][2] * weight;
    }

    let moved = false;
    for (let c = 0; c < k; c++) {
      if (!weights[c]) continue;
      const next = sums[c].map((value) => value / weights[c]);
      if (deltaE(next, centers[c]) > C.CONVERGENCE_EPSILON) moved = true;
      centers[c] = next;
    }
    if (!moved) break;
  }

  const clusterCounts = new Float64Array(k);
  const nearestEntry = new Array(k).fill(-1);
  const nearestDistance = new Float64Array(k).fill(Infinity);
  for (let i = 0; i < entries.length; i++) {
    let nearest = 0;
    let distance = Infinity;
    for (let c = 0; c < k; c++) {
      const d = deltaE(labs[i], centers[c]);
      if (d < distance) {
        distance = d;
        nearest = c;
      }
    }
    clusterCounts[nearest] += entries[i].count;
    if (distance < nearestDistance[nearest]) {
      nearestDistance[nearest] = distance;
      nearestEntry[nearest] = i;
    }
  }

  const total = clusterCounts.reduce((sum, count) => sum + count, 0);
  const clusters = [];
  for (let c = 0; c < k; c++) {
    if (!clusterCounts[c] || nearestEntry[c] < 0) continue;
    clusters.push({
      rgb: entries[nearestEntry[c]].rgb,
      oklab: centers[c],
      count: clusterCounts[c],
      ratio: clusterCounts[c] / total,
    });
  }
  clusters.sort((a, b) => b.count - a.count);
  return mergeNearColors(clusters);
}

/**
 * 合并 OKLab 距离过近的聚类结果。
 * @param {ClusterResult[]} clusters
 * @returns {ClusterResult[]}
 */
function mergeNearColors(clusters) {
  const C = PALETTE_ALGORITHM_V1;
  if (clusters.length <= 1) return clusters;

  const result = [clusters[0]];
  for (let i = 1; i < clusters.length; i++) {
    let merged = false;
    for (let j = 0; j < result.length; j++) {
      if (deltaE(clusters[i].oklab, result[j].oklab) < C.NEAR_COLOR_MERGE_DELTA_E) {
        // 合并到 result[j]
        const totalCount = result[j].count + clusters[i].count;
        result[j].count = totalCount;
        result[j].ratio = totalCount; // 暂存，后面归一
        merged = true;
        break;
      }
    }
    if (!merged) result.push(clusters[i]);
  }

  // 重新计算 ratio
  const total = result.reduce((s, c) => s + c.count, 0);
  for (const c of result) {
    c.ratio = c.count / total;
  }
  result.sort((a, b) => b.count - a.count);
  return result;
}

// ── 角色色分配 ────────────────────────────────────────────────────────────

/**
 * 为聚类结果分配角色色。
 *
 * 角色定义：
 * - background: 占比 >= 35%，或亮度最高且占比最大
 * - accent: 占比 >= 2%，非背景中最显著
 * - foreground: 暗色且非背景
 * - muted: 其他低占比颜色
 *
 * @param {ClusterResult[]} clusters
 * @returns {RoleColor[]}
 */
export function assignRoles(clusters) {
  const C = PALETTE_ALGORITHM_V1;
  if (clusters.length === 0) return [];

  const result = clusters.map((c) => ({
    rgb: c.rgb,
    oklab: c.oklab,
    ratio: c.ratio,
    role: '',
    hex: rgbToHex(c.rgb[0], c.rgb[1], c.rgb[2]),
  }));

  // 按 OKLab L 值排序找最亮和最暗
  const byL = [...result].sort((a, b) => b.oklab[0] - a.oklab[0]);

  // 背景色：占比 >= 35%，或亮度最高且占比最大
  let bgAssigned = false;
  for (const c of result) {
    if (c.ratio >= C.BACKGROUND_MIN_RATIO) {
      c.role = 'background';
      bgAssigned = true;
      break;
    }
  }
  if (!bgAssigned && result.length > 0) {
    // 最亮且占比最大的作为 background
    byL[0].role = 'background';
    bgAssigned = true;
  }

  // 剩余颜色分配
  for (const c of result) {
    if (c.role) continue;
    if (c.ratio >= C.ACCENT_MIN_RATIO) {
      c.role = 'accent';
    } else if (c.oklab[0] < 0.3) {
      c.role = 'foreground';
    } else {
      c.role = 'muted';
    }
  }

  // 角色优先排序：background > accent > foreground > muted
  const roleOrder = { background: 0, accent: 1, foreground: 2, muted: 3 };
  result.sort((a, b) => {
    const ro = roleOrder[a.role] - roleOrder[b.role];
    if (ro !== 0) return ro;
    return b.ratio - a.ratio;
  });

  return result;
}

// ── Harmony 方案生成 ──────────────────────────────────────────────────────

/**
 * 基于基准 RGB 生成 harmony 配色方案。
 * 每个色相生成 3 档明暗梯度（亮/中/暗）。
 *
 * @param {number[]} baseRgb - [r, g, b]
 * @param {string} scheme - 'complementary' | 'triadic' | 'analogous' | 'splitComplementary' | 'monochromatic' | 'square'
 * @returns {number[][]} 配色数组 [[r,g,b], ...]
 */
export function generateHarmony(baseRgb, scheme) {
  const [h, s, l] = rgbToHsl(baseRgb[0], baseRgb[1], baseRgb[2]);

  let hues;
  switch (scheme) {
    case 'complementary':
      hues = [h, (h + 180) % 360];
      break;
    case 'triadic':
      hues = [h, (h + 120) % 360, (h + 240) % 360];
      break;
    case 'analogous':
      hues = [((h + 330) % 360), h, ((h + 30) % 360)];
      break;
    case 'splitComplementary':
      hues = [h, (h + 150) % 360, (h + 210) % 360];
      break;
    case 'monochromatic':
      hues = [h];
      break;
    case 'square':
      hues = [h, (h + 90) % 360, (h + 180) % 360, (h + 270) % 360];
      break;
    default:
      hues = [h];
  }

  const colors = [];
  const sClamped = Math.min(100, Math.max(20, s));
  for (const hue of hues) {
    // 3 档明暗：亮、中、暗
    colors.push(hslToRgb(hue, sClamped, Math.min(85, l + 18)));
    colors.push(hslToRgb(hue, sClamped, l));
    colors.push(hslToRgb(hue, sClamped, Math.max(15, l - 18)));
  }
  return colors;
}

/**
 * 生成所有完整 harmony 方案。
 * @param {number[]} baseRgb
 * @returns {HarmonyScheme[]}
 */
export function generateAllHarmonies(baseRgb) {
  const schemes = [
    { label: '类比', scheme: 'analogous', description: '同色系协调' },
    { label: '互补', scheme: 'complementary', description: '强对比' },
    { label: '分裂互补', scheme: 'splitComplementary', description: '柔和对比' },
    { label: '三角色', scheme: 'triadic', description: '三向均衡' },
    { label: '四角色', scheme: 'square', description: '多角色丰富' },
    { label: '明暗', scheme: 'monochromatic', description: '同色明暗' },
  ];

  return schemes.map((s) => ({
    label: s.label,
    scheme: s.scheme,
    description: s.description,
    colors: generateHarmony(baseRgb, s.scheme),
  }));
}

/**
 * 面向实际设计使用的显式基准色生成器。
 * 与图片提取完全分离：anchorRgb 由 UI 明示，sourceColors 只提供原图灰阶上下文。
 */
export function generateDesignPalettes(anchorRgb, sourceColors = []) {
  const [h, rawS, rawL] = rgbToHsl(...anchorRgb);
  const s = Math.max(28, rawS);
  const dedupe = (colors) => {
    const seen = new Set();
    return colors.filter((rgb) => {
      const hex = rgbToHex(...rgb);
      if (seen.has(hex)) return false;
      seen.add(hex);
      return true;
    });
  };
  const neutrals = sourceColors
    .filter((rgb) => rgbToHsl(...rgb)[1] < 18)
    .sort((a, b) => rgbToHsl(...a)[2] - rgbToHsl(...b)[2]);
  const darkNeutral = neutrals[0];
  const lightNeutral = neutrals[neutrals.length - 1];

  const levels = [...new Set([18, 34, Math.max(12, Math.min(88, rawL)), 66, 84])]
    .sort((a, b) => a - b);
  const monochrome = levels.map((lightness) => hslToRgb(h, s, lightness));
  const analogous = [h - 30, h, h + 30]
    .map((hue) => hslToRgb((hue + 360) % 360, s, Math.max(28, Math.min(72, rawL))));
  const complement = [
    ...(darkNeutral ? [darkNeutral] : []),
    anchorRgb,
    hslToRgb((h + 180) % 360, s, Math.max(28, Math.min(72, rawL))),
    ...(lightNeutral ? [lightNeutral] : []),
  ];

  return [
    { label: '同色层级', scheme: 'generated-tones', description: '同一基准色的明暗层级', colors: dedupe(monochrome) },
    { label: '邻近协调', scheme: 'generated-analogous', description: '基准色左右 30° 的协调色', colors: dedupe(analogous) },
    { label: '互补强调', scheme: 'generated-complement', description: '基准色、互补色与原图灰阶', colors: dedupe(complement) },
  ];
}

/**
 * 从聚类结果选择基准色（用于 harmony 生成）。
 * 优先选 accent 角色，否则选占比最大的非背景色。
 *
 * @param {RoleColor[]} roles
 * @returns {number[]} base RGB [r, g, b]
 */
export function selectBaseColor(roles) {
  if (roles.length === 0) return [128, 128, 128];

  // 优先 accent
  const accent = roles.find((r) => r.role === 'accent');
  if (accent) return accent.rgb;

  // 其次非 background 的最大占比
  const nonBg = roles.filter((r) => r.role !== 'background');
  if (nonBg.length > 0) return nonBg[0].rgb;

  // fallback: 第一个
  return roles[0].rgb;
}

// ── 推荐方案（首屏最多 3 个）──────────────────────────────────────────────

/**
 * 从完整 harmony 列表中选择最多 3 个推荐方案。
 * 选择策略：互补（最强对比）→ 类比（最和谐）→ 三角色（最丰富）
 *
 * @param {number[]} baseRgb
 * @returns {HarmonyScheme[]}
 */
export function selectRecommendedSchemes(baseRgb) {
  const recommended = [
    { label: '类比', scheme: 'analogous', description: '延续图片主色系' },
    { label: '互补', scheme: 'complementary', description: '增强前景对比' },
    { label: '三角色', scheme: 'triadic', description: '扩展多角色用色' },
  ];
  return recommended.map((item) => ({
    ...item,
    colors: generateHarmony(baseRgb, item.scheme),
  }));
}

/**
 * 根据聚类主题色生成可读的整体倾向，不改变聚类结果。
 * 色相使用按饱和度加权的圆周均值，避免红色 359°/1° 被平均成青色。
 *
 * @param {RoleColor[]} roles
 * @returns {{family:string, temperature:string, saturation:string, lightness:string, hueConcentration:number, summary:string}}
 */
export function analyzeTheme(roles) {
  if (!roles.length) {
    return { family: '无主题色', temperature: '中性', saturation: '低饱和', lightness: '中间调', hueConcentration: 0, summary: '无可分析主题色' };
  }

  let x = 0;
  let y = 0;
  let chromaWeight = 0;
  let saturationSum = 0;
  let lightnessSum = 0;
  let ratioSum = 0;
  for (const role of roles) {
    const [h, s, l] = rgbToHsl(role.rgb[0], role.rgb[1], role.rgb[2]);
    const ratio = Math.max(0, role.ratio || 0);
    const hueWeight = ratio * (s / 100);
    const radians = h * Math.PI / 180;
    x += Math.cos(radians) * hueWeight;
    y += Math.sin(radians) * hueWeight;
    chromaWeight += hueWeight;
    saturationSum += s * ratio;
    lightnessSum += l * ratio;
    ratioSum += ratio;
  }

  const avgS = ratioSum > 0 ? saturationSum / ratioSum : 0;
  const avgL = ratioSum > 0 ? lightnessSum / ratioSum : 50;
  // 圆周均值的向量长度表示色相集中度。彩虹/多主题图的各方向会相互抵消，
  // 此时不应把浮点残差误报为某个单一主色。
  const hueConcentration = chromaWeight > 0.01 ? Math.hypot(x, y) / chromaWeight : 0;
  const isMultiHue = roles.length >= 3 && avgS >= 25 && hueConcentration < 0.45;
  const hue = chromaWeight > 0.01 && !isMultiHue
    ? (Math.atan2(y, x) * 180 / Math.PI + 360) % 360
    : null;
  let family = '中性色系';
  if (isMultiHue) {
    family = '多色系';
  } else if (hue !== null && avgS >= 12) {
    if (hue < 15 || hue >= 345) family = '红色系';
    else if (hue < 45) family = '橙色系';
    else if (hue < 75) family = '黄色系';
    else if (hue < 165) family = '绿色系';
    else if (hue < 200) family = '青色系';
    else if (hue < 255) family = '蓝色系';
    else if (hue < 300) family = '紫色系';
    else family = '粉色系';
  }

  const warm = hue !== null && (hue < 90 || hue >= 330);
  const cool = hue !== null && hue >= 150 && hue < 285;
  const temperature = isMultiHue ? '冷暖均衡' : warm ? '暖色倾向' : cool ? '冷色倾向' : '综合色倾向';
  const saturation = avgS < 25 ? '低饱和' : avgS < 60 ? '中等饱和' : '高饱和';
  const lightness = avgL < 35 ? '深色调' : avgL < 70 ? '中间调' : '浅色调';
  return {
    family,
    temperature,
    saturation,
    lightness,
    hueConcentration,
    summary: `${family} · ${temperature} · ${saturation} · ${lightness} · ${roles.length} 个主题色`,
  };
}

// ── 设计取色：显著色 / 均衡色 ─────────────────────────────────────────────

/**
 * 单遍扫描 RGBA，构建固定 5-bit RGB 直方图。每桶用多数候选保留常见的真实像素，
 * 避免少量抗锯齿近色替换纯色；后续提取色不会由缩放插值或桶平均制造。
 */
export function createColorHistogramAccumulator() {
  const counts = new Uint32Array(32 * 32 * 32);
  return {
    counts,
    representatives: new Uint32Array(counts.length),
    representativeVotes: new Int32Array(counts.length),
    validPixels: 0,
  };
}

/** 将指定像素区间累积进固定直方图，供同步分析和主线程分块扫描共用。 */
export function accumulateColorHistogram(accumulator, rgbaFlat, startPixel = 0, endPixel = Math.floor(rgbaFlat.length / 4)) {
  const { counts, representatives, representativeVotes } = accumulator;
  const safeEnd = Math.min(endPixel, Math.floor(rgbaFlat.length / 4));
  for (let pixel = Math.max(0, startPixel); pixel < safeEnd; pixel++) {
    const i = pixel * 4;
    if (rgbaFlat[i + 3] / 255 < PALETTE_ALGORITHM_V1.MIN_ALPHA) continue;
    const r = rgbaFlat[i];
    const g = rgbaFlat[i + 1];
    const b = rgbaFlat[i + 2];
    const qr = r >> 3;
    const qg = g >> 3;
    const qb = b >> 3;
    const key = (qr << 10) | (qg << 5) | qb;
    const packed = (r << 16) | (g << 8) | b;
    // Boyer-Moore 多数候选：纯色填充中的少量抗锯齿不会抢走代表色；
    // 即使桶内无绝对多数，候选仍然保证是原图真实像素。
    if (representativeVotes[key] === 0) {
      representatives[key] = packed;
      representativeVotes[key] = 1;
    } else if (representatives[key] === packed) {
      representativeVotes[key]++;
    } else {
      representativeVotes[key]--;
    }
    counts[key]++;
    accumulator.validPixels++;
  }
  return accumulator;
}

/** 压紧固定直方图；输出的每个颜色都必定是某个原始输入像素。 */
export function finalizeColorHistogram(accumulator, scannedPixels) {
  const { counts: binCounts, representatives, validPixels } = accumulator;
  let size = 0;
  for (const count of binCounts) if (count) size++;
  const colors = new Uint8Array(size * 3);
  const counts = new Uint32Array(size);
  let output = 0;
  for (let key = 0; key < binCounts.length; key++) {
    if (!binCounts[key]) continue;
    const packed = representatives[key];
    colors[output * 3] = (packed >>> 16) & 0xff;
    colors[output * 3 + 1] = (packed >>> 8) & 0xff;
    colors[output * 3 + 2] = packed & 0xff;
    counts[output] = binCounts[key];
    output++;
  }
  return { colors, counts, validPixels, scannedPixels, mode: 'full' };
}

export function buildColorHistogram(rgbaFlat) {
  const scannedPixels = Math.floor(rgbaFlat.length / 4);
  const accumulator = createColorHistogramAccumulator();
  accumulateColorHistogram(accumulator, rgbaFlat, 0, scannedPixels);
  return finalizeColorHistogram(accumulator, scannedPixels);
}

function histogramEntries(histogram) {
  const entries = [];
  const total = histogram.validPixels || histogram.counts.reduce((sum, count) => sum + count, 0);
  for (let i = 0; i < histogram.counts.length; i++) {
    const rgb = [histogram.colors[i * 3], histogram.colors[i * 3 + 1], histogram.colors[i * 3 + 2]];
    const oklab = rgbToOklab(...rgb);
    entries.push({
      rgb,
      oklab,
      chroma: Math.hypot(oklab[1], oklab[2]),
      count: histogram.counts[i],
      ratio: total ? histogram.counts[i] / total : 0,
    });
  }
  return entries;
}

function buildColorBins(pixels) {
  const bins = new Map();
  for (const [r, g, b] of pixels) {
    // 每通道 4 bit 的轻量直方图：合并抗锯齿近色，同时保留 UI 小面积纯色。
    const key = `${r >> 4},${g >> 4},${b >> 4}`;
    const bin = bins.get(key);
    if (bin) {
      bin.count++;
      bin.sumR += r;
      bin.sumG += g;
      bin.sumB += b;
    } else {
      bins.set(key, { count: 1, sumR: r, sumG: g, sumB: b });
    }
  }
  return [...bins.values()].map((bin) => {
    const rgb = [
      Math.round(bin.sumR / bin.count),
      Math.round(bin.sumG / bin.count),
      Math.round(bin.sumB / bin.count),
    ];
    const oklab = rgbToOklab(rgb[0], rgb[1], rgb[2]);
    return {
      rgb,
      oklab,
      chroma: Math.hypot(oklab[1], oklab[2]),
      count: bin.count,
      ratio: bin.count / pixels.length,
    };
  });
}

function pickDiverseColors(candidates, maxColors) {
  if (!candidates.length) return [];
  const remaining = [...candidates];
  const selected = [];
  while (remaining.length && selected.length < maxColors) {
    let bestIndex = 0;
    let bestValue = -Infinity;
    for (let i = 0; i < remaining.length; i++) {
      const candidate = remaining[i];
      const minDistance = selected.length
        ? Math.min(...selected.map((chosen) => deltaE(candidate.oklab, chosen.oklab)))
        : 1;
      if (selected.length && minDistance < 0.04) continue;
      const value = candidate.score * (0.25 + minDistance * 3);
      if (value > bestValue) {
        bestValue = value;
        bestIndex = i;
      }
    }
    if (bestValue === -Infinity) break;
    selected.push(remaining.splice(bestIndex, 1)[0]);
  }
  return selected;
}

/**
 * 在色相环上聚合高色度颜色并找局部峰值。
 *
 * 单个 RGB 桶的面积阈值容易漏掉小图标/状态点；色相环会把同一视觉颜色的主体、
 * 抗锯齿和明暗变体聚到相邻扇区，再以色度和背景反差增强峰值。每个峰最终仍返回
 * 一个原图真实颜色，而不是色相扇区的平均色。
 */
function findHuePeakCandidates(bins, bgLab) {
  const HUE_SECTORS = 24;
  const total = bins.reduce((sum, bin) => sum + bin.count, 0);
  if (!total) return [];

  const sectors = Array.from({ length: HUE_SECTORS }, () => ({
    count: 0,
    chromaSum: 0,
    contrastSum: 0,
    entries: [],
    energy: 0,
    smoothedEnergy: 0,
  }));
  const candidateMinCount = Math.max(2, Math.floor(total * 0.000002));

  for (const bin of bins) {
    const backgroundDistance = deltaE(bin.oklab, bgLab);
    if (bin.chroma < 0.035 || backgroundDistance < 0.055) continue;
    // 用 OKLab a/b 的极角作为感知色相，比直接使用 HSL 色相更适合视觉距离判断。
    const hue = (Math.atan2(bin.oklab[2], bin.oklab[1]) * 180 / Math.PI + 360) % 360;
    const sector = sectors[Math.floor(hue / (360 / HUE_SECTORS)) % HUE_SECTORS];
    sector.count += bin.count;
    sector.chromaSum += bin.chroma * bin.count;
    sector.contrastSum += backgroundDistance * bin.count;
    sector.entries.push({ ...bin, backgroundDistance });
  }

  for (const sector of sectors) {
    if (!sector.count) continue;
    const averageChroma = sector.chromaSum / sector.count;
    const averageContrast = sector.contrastSum / sector.count;
    // 面积只取 0.22 次方：大色块仍更可靠，但不会压死小面积高色度峰。
    sector.energy = Math.pow(sector.count / total, 0.22)
      * (0.25 + averageChroma * 4)
      * (0.35 + averageContrast * 2.5);
  }
  for (let i = 0; i < HUE_SECTORS; i++) {
    const previous = sectors[(i - 1 + HUE_SECTORS) % HUE_SECTORS].energy;
    const next = sectors[(i + 1) % HUE_SECTORS].energy;
    sectors[i].smoothedEnergy = sectors[i].energy + (previous + next) * 0.3;
  }

  // 按峰强排序并抑制相邻扇区；这是色相环上的非极大值抑制。
  const peaks = sectors
    .map((sector, index) => ({ sector, index }))
    .filter(({ sector }) => sector.count >= candidateMinCount)
    .sort((a, b) => b.sector.smoothedEnergy - a.sector.smoothedEnergy);
  const selectedPeaks = [];
  for (const peak of peaks) {
    const overlaps = selectedPeaks.some((chosen) => {
      const distance = Math.abs(chosen.index - peak.index);
      return Math.min(distance, HUE_SECTORS - distance) <= 1;
    });
    if (!overlaps) selectedPeaks.push(peak);
  }

  return selectedPeaks.map(({ sector, index }) => {
    const neighboringEntries = [
      ...sectors[(index - 1 + HUE_SECTORS) % HUE_SECTORS].entries,
      ...sector.entries,
      ...sectors[(index + 1) % HUE_SECTORS].entries,
    ].filter((entry) => entry.count >= candidateMinCount);
    const candidates = neighboringEntries.length ? neighboringEntries : sector.entries;
    const representative = candidates.reduce((best, entry) => {
      const score = Math.pow(entry.ratio, 0.12)
        * (0.25 + entry.chroma * 4)
        * (0.35 + entry.backgroundDistance * 3);
      return !best || score > best.score ? { ...entry, score } : best;
    }, null);
    return representative
      ? { ...representative, score: representative.score * sector.smoothedEnergy }
      : null;
  }).filter(Boolean).sort((a, b) => b.score - a.score);
}

/**
 * 同时生成面积主题之外的两种设计视角：
 * - 视觉焦点色：保护面积很小、但高色度且与背景反差明显的颜色；
 * - 均衡/界面关键色：背景 + 层级灰 + 多样化点缀。
 */
export function extractDesignSchemes(pixelsOrEntries, roles, entriesAreWeighted = false) {
  if (!pixelsOrEntries.length || !roles.length) return [];
  const bins = entriesAreWeighted ? pixelsOrEntries : buildColorBins(pixelsOrEntries);
  const background = roles.find((role) => role.role === 'background') || roles[0];
  const bgLab = background.oklab || rgbToOklab(...background.rgb);

  const salientCandidates = findHuePeakCandidates(bins, bgLab);
  const salient = pickDiverseColors(salientCandidates, 6);

  const neutralCandidates = bins
    .map((bin) => {
      const backgroundDistance = deltaE(bin.oklab, bgLab);
      return {
        ...bin,
        backgroundDistance,
        score: Math.pow(bin.ratio, 0.3) * backgroundDistance,
      };
    })
    .filter((bin) => bin.ratio >= 0.001 && bin.chroma < 0.055 && bin.backgroundDistance >= 0.06)
    .sort((a, b) => b.score - a.score);
  const neutrals = pickDiverseColors(neutralCandidates, 2);

  const balanced = [];
  const balancedLabs = [];
  const appendDistinct = (rgb) => {
    const lab = rgbToOklab(...rgb);
    if (balancedLabs.some((existing) => deltaE(existing, lab) < 0.035)) return;
    balanced.push(rgb);
    balancedLabs.push(lab);
  };
  appendDistinct(background.rgb);
  neutrals.forEach((candidate) => appendDistinct(candidate.rgb));
  salient.slice(0, 4).forEach((candidate) => appendDistinct(candidate.rgb));
  if (balanced.length < 3) roles.forEach((role) => appendDistinct(role.rgb));

  const neutralPixelRatio = bins
    .filter((bin) => bin.chroma < 0.055)
    .reduce((sum, bin) => sum + bin.ratio, 0);
  const interfaceLike = neutralPixelRatio >= 0.6 && salient.length >= 2;

  return [
    {
      label: '视觉焦点色',
      scheme: 'salient',
      description: '色相峰值、高色度与背景反差',
      colors: salient.length ? salient.map((candidate) => candidate.rgb) : roles.map((role) => role.rgb),
    },
    {
      label: interfaceLike ? '界面关键色' : '均衡配色',
      scheme: 'balanced',
      description: interfaceLike ? '背景、文字层级与状态点缀' : '主色、层级色与点缀色',
      colors: balanced,
    },
  ];
}

// ── 完整配色分析入口 ──────────────────────────────────────────────────────

/**
 * 从 RGBA 像素数据执行完整配色分析。
 *
 * 输入为 flat RGBA 数组 [r,g,b,a, r,g,b,a, ...]。
 * 透明度 < MIN_ALPHA 的像素跳过。
 * 空样本返回 { empty: true }。
 *
 * @param {number[]|Uint8ClampedArray} rgbaFlat - flat RGBA 数据
 * @param {number} width
 * @param {number} height
 * @returns {PaletteResult}
 */
export function analyzePalette(rgbaFlat, width, height) {
  return analyzePaletteHistogram(buildColorHistogram(rgbaFlat), width, height);
}

/** 从主线程流式扫描得到的紧凑直方图执行聚类与设计分析。 */
export function analyzePaletteHistogram(histogram, width, height) {
  if (!histogram || !histogram.validPixels || !histogram.counts?.length) {
    return { roles: [], recommended: [], full: [], empty: true };
  }

  const entries = histogramEntries(histogram);
  const distinctCount = entries.length;
  const k = distinctCount <= PALETTE_ALGORITHM_V1.K_MAX
    ? distinctCount
    : Math.min(
      PALETTE_ALGORITHM_V1.K_MAX,
      Math.max(PALETTE_ALGORITHM_V1.K_MIN, Math.round(Math.sqrt(distinctCount))),
    );
  const clusters = weightedKMeansCluster(entries, k);
  const roles = assignRoles(clusters);

  // 默认三组分别回答：整体是什么色、哪里最醒目、怎样组成可用设计色板。
  const sourceScheme = {
    label: '图片主题色',
    scheme: 'source',
    description: '来自整块选区的聚类原色',
    colors: roles.map((role) => role.rgb),
  };
  const recommended = [sourceScheme, ...extractDesignSchemes(entries, roles, true)];

  return {
    roles,
    theme: analyzeTheme(roles),
    sample: {
      width,
      height,
      validPixels: histogram.validPixels,
      scannedPixels: histogram.scannedPixels ?? histogram.validPixels,
      mode: histogram.mode || 'full',
    },
    recommended,
    full: [],
    empty: false,
  };
}

// ── 输出格式生成 ──────────────────────────────────────────────────────────

/**
 * 将角色色列表输出为纯颜色列表。
 * @param {RoleColor[]} roles
 * @returns {string} 每行一个 HEX
 */
export function formatAsList(roles) {
  return roles.map((r) => r.hex).join('\n');
}

/**
 * 将角色色列表输出为 CSS variables。
 * 变量名按角色稳定生成，如 --blink-color-background。
 *
 * @param {RoleColor[]} roles
 * @returns {string} CSS variable 声明
 */
export function formatAsCssVariables(roles) {
  const roleCounts = new Map();
  return roles.map((r) => {
    const count = (roleCounts.get(r.role) || 0) + 1;
    roleCounts.set(r.role, count);
    const suffix = count === 1 ? '' : `-${count}`;
    return `  --blink-color-${r.role}${suffix}: ${r.hex};`;
  }).join('\n');
}

/**
 * 将角色色列表输出为 HEX/RGB/HSL 多格式。
 * @param {RoleColor[]} roles
 * @returns {string} 多行，每行包含角色、HEX、RGB、HSL
 */
export function formatAsMultiLine(roles) {
  return roles
    .map((r) => {
      const [hr, hg, hb] = r.rgb;
      const [h, s, l] = rgbToHsl(hr, hg, hb);
      return `${r.role}: ${r.hex} | rgb(${hr}, ${hg}, ${hb}) | hsl(${h}, ${s}%, ${l}%)`;
    })
    .join('\n');
}

/**
 * 将选中的颜色列表（HEX 数组）格式化为指定输出格式。
 *
 * @param {string[]} hexColors
 * @param {'list'|'css'|'hex'|'rgb'|'hsl'} format
 * @returns {string}
 */
export function formatOutput(hexColors, format) {
  if (format === 'list') {
    return hexColors.join('\n');
  }
  if (format === 'css') {
    return hexColors.map((h) => `  --blink-color: ${h};`).join('\n');
  }
  if (format === 'hex') {
    return hexColors.join('\n');
  }
  if (format === 'rgb') {
    return hexColors.map((hex) => {
      const r = parseInt(hex.slice(1, 3), 16);
      const g = parseInt(hex.slice(3, 5), 16);
      const b = parseInt(hex.slice(5, 7), 16);
      return `rgb(${r}, ${g}, ${b})`;
    }).join('\n');
  }
  if (format === 'hsl') {
    return hexColors.map((hex) => {
      const r = parseInt(hex.slice(1, 3), 16);
      const g = parseInt(hex.slice(3, 5), 16);
      const b = parseInt(hex.slice(5, 7), 16);
      const [h, s, l] = rgbToHsl(r, g, b);
      return `hsl(${h}, ${s}%, ${l}%)`;
    }).join('\n');
  }
  return hexColors.join('\n');
}
