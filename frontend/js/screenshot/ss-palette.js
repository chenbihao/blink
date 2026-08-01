//! 配色提取（0.15.6，0.15.10 改 k-means 聚类）。
//!
//! 功能：
//! - 基础档：从截图选区提取主色调（k-means 聚类算法），展示 N 色板
//! - 进阶档：基于主色生成 harmony 配色方案（互补/三元/类比/分裂互补）
//! - 每个色块可点击设为当前色，右键复制 HEX
//!
//! 算法说明：
//! - 降采样：从 cropSourceCanvas 按 N×N 网格取像素（默认 64×64 ≈ 4096 像素）
//! - k-means 聚类：用 median cut 结果初始化聚类中心，迭代分配+更新中心
//!   比纯 median cut 更贴合实际色彩分布（按像素相似度聚类而非按通道中位数分裂）
//! - harmony：RGB→HSL → 色相偏移 → HSL→RGB，每色相生成 3 档明暗梯度

import * as annot from './annotation-engine.js';
import { ss } from './ss-state.js';
import { rgbToHex, rgbToHsl, hslToRgb, syncFromAnnot } from './ss-color-picker.js';

// ── Median Cut 量化算法 ──────────────────────────────────

/**
 * 从 canvas 降采样获取像素数组。
 * 一次性 getImageData 全图，再按网格步长采样，避免逐像素 getImageData 的开销。
 * @param {HTMLCanvasElement} canvas - 源 canvas
 * @param {number} sampleSize - 采样网格尺寸（如 64 表示每 ~1/64 宽度取一列）
 * @returns {number[][]} 像素数组 [[r,g,b], ...]
 */
function downsampleCanvas(canvas, sampleSize = 64) {
  const ctx = canvas.getContext('2d');
  const w = canvas.width;
  const h = canvas.height;
  if (w === 0 || h === 0) return [];

  const imageData = ctx.getImageData(0, 0, w, h);
  const data = imageData.data;

  const stepX = Math.max(1, Math.floor(w / sampleSize));
  const stepY = Math.max(1, Math.floor(h / sampleSize));
  const pixels = [];
  for (let y = 0; y < h; y += stepY) {
    for (let x = 0; x < w; x += stepX) {
      const idx = (y * w + x) * 4;
      // 跳过完全透明的像素
      if (data[idx + 3] < 128) continue;
      pixels.push([data[idx], data[idx + 1], data[idx + 2]]);
    }
  }
  return pixels;
}

/**
 * 计算像素桶在 R/G/B 三通道上的范围（max - min）。
 * @param {number[][]} pixels - [[r,g,b], ...]
 * @returns {[number, number, number]} [rRange, gRange, bRange]
 */
function channelRanges(pixels) {
  let rMin = 255, rMax = 0, gMin = 255, gMax = 0, bMin = 255, bMax = 0;
  for (const [r, g, b] of pixels) {
    if (r < rMin) rMin = r; if (r > rMax) rMax = r;
    if (g < gMin) gMin = g; if (g > gMax) gMax = g;
    if (b < bMin) bMin = b; if (b > bMax) bMax = b;
  }
  return [rMax - rMin, gMax - gMin, bMax - bMin];
}

/**
 * Median cut 量化算法。
 * 将像素数组量化为 numColors 个代表色，每个代表色是其桶内所有像素的 RGB 平均值。
 *
 * 算法步骤：
 * 1. 所有像素放入一个桶
 * 2. 找到 R/G/B 通道范围最大的桶
 * 3. 按该通道排序，从中位数位置分裂为两个桶
 * 4. 重复步骤 2-3 直到桶数达到 numColors
 * 5. 每个桶取平均色作为代表色
 *
 * @param {number[][]} pixels - [[r,g,b], ...]
 * @param {number} numColors - 目标色数
 * @returns {number[][]} 代表色数组 [[r,g,b], ...]
 */
function medianCut(pixels, numColors) {
  if (pixels.length === 0) return [];

  // 像素数少于目标色数，去重后直接返回
  if (pixels.length <= numColors) {
    const seen = new Set();
    return pixels.filter(([r, g, b]) => {
      const key = (r << 16) | (g << 8) | b;
      if (seen.has(key)) return false;
      seen.add(key);
      return true;
    }).slice(0, numColors);
  }

  let buckets = [pixels.slice()];

  while (buckets.length < numColors) {
    // 找范围最大的桶
    let maxRange = -1;
    let maxIdx = -1;
    let maxCh = 0;
    for (let i = 0; i < buckets.length; i++) {
      if (buckets[i].length < 2) continue;
      const [rR, gR, bR] = channelRanges(buckets[i]);
      const range = Math.max(rR, gR, bR);
      if (range > maxRange) {
        maxRange = range;
        maxIdx = i;
        maxCh = rR >= gR && rR >= bR ? 0 : gR >= bR ? 1 : 2;
      }
    }
    if (maxIdx === -1) break; // 所有桶都只有 ≤1 像素

    const bucket = buckets[maxIdx];
    bucket.sort((a, b) => a[maxCh] - b[maxCh]);
    const mid = Math.floor(bucket.length / 2);
    buckets[maxIdx] = bucket.slice(0, mid);
    buckets.push(bucket.slice(mid));
  }

  // 每个桶取平均色 + 记录桶大小
  const result = buckets.map((bucket) => {
    let sR = 0, sG = 0, sB = 0;
    for (const [r, g, b] of bucket) { sR += r; sG += g; sB += b; }
    return {
      rgb: [
        Math.round(sR / bucket.length),
        Math.round(sG / bucket.length),
        Math.round(sB / bucket.length),
      ],
      count: bucket.length,
    };
  });
  // 按桶大小降序排列——频率最高的色在前，供 harmony 取 base hue
  result.sort((a, b) => b.count - a.count);
  return result.map((r) => r.rgb);
}

// ── K-means 聚类算法（0.15.10）──────────────────────────

/**
 * K-means 色彩聚类。
 * 用 median cut 结果初始化聚类中心，迭代分配+更新中心，
 * 比纯 median cut 更贴合实际色彩分布。
 *
 * 算法步骤：
 * 1. 用 medianCut 初始化 k 个聚类中心
 * 2. 每个像素分配到最近的聚类中心（欧氏距离）
 * 3. 重新计算每个聚类的中心（RGB 均值）
 * 4. 重复 2-3 直到收敛或达到最大迭代次数
 * 5. 按聚类大小降序返回代表色
 *
 * @param {number[][]} pixels - [[r,g,b], ...]
 * @param {number} k - 目标色数
 * @param {number} maxIter - 最大迭代次数（默认 10）
 * @returns {number[][]} 代表色数组 [[r,g,b], ...]，按频率降序
 */
function kMeans(pixels, k, maxIter = 10) {
  if (pixels.length === 0) return [];
  if (pixels.length <= k) {
    // 像素数太少，直接去重返回
    const seen = new Set();
    return pixels.filter(([r, g, b]) => {
      const key = (r << 16) | (g << 8) | b;
      if (seen.has(key)) return false;
      seen.add(key);
      return true;
    }).slice(0, k);
  }

  // 1. 用 median cut 初始化聚类中心
  let centers = medianCut(pixels, k);
  if (centers.length === 0) return [];

  for (let iter = 0; iter < maxIter; iter++) {
    // 2. 分配每个像素到最近的中心
    const clusters = centers.map(() => []);
    for (const p of pixels) {
      let minDist = Infinity;
      let minIdx = 0;
      for (let c = 0; c < centers.length; c++) {
        const dr = p[0] - centers[c][0];
        const dg = p[1] - centers[c][1];
        const db = p[2] - centers[c][2];
        const dist = dr * dr + dg * dg + db * db;
        if (dist < minDist) {
          minDist = dist;
          minIdx = c;
        }
      }
      clusters[minIdx].push(p);
    }

    // 3. 重新计算中心
    let moved = false;
    const newCenters = [];
    const clusterSizes = [];
    for (let c = 0; c < centers.length; c++) {
      const cluster = clusters[c];
      clusterSizes.push(cluster.length);
      if (cluster.length === 0) {
        // 空聚类保留原中心
        newCenters.push(centers[c]);
        continue;
      }
      let sR = 0, sG = 0, sB = 0;
      for (const [r, g, b] of cluster) { sR += r; sG += g; sB += b; }
      const nc = [
        Math.round(sR / cluster.length),
        Math.round(sG / cluster.length),
        Math.round(sB / cluster.length),
      ];
      // 检查中心是否移动
      if (Math.abs(nc[0] - centers[c][0]) > 1 ||
          Math.abs(nc[1] - centers[c][1]) > 1 ||
          Math.abs(nc[2] - centers[c][2]) > 1) {
        moved = true;
      }
      newCenters.push(nc);
    }
    centers = newCenters;

    // 4. 收敛检测
    if (!moved) break;
  }

  // 5. 按聚类大小降序排序
  // 重新分配一次以获取最终聚类大小
  const clusterCounts = centers.map(() => 0);
  for (const p of pixels) {
    let minDist = Infinity;
    let minIdx = 0;
    for (let c = 0; c < centers.length; c++) {
      const dr = p[0] - centers[c][0];
      const dg = p[1] - centers[c][1];
      const db = p[2] - centers[c][2];
      const dist = dr * dr + dg * dg + db * db;
      if (dist < minDist) {
        minDist = dist;
        minIdx = c;
      }
    }
    clusterCounts[minIdx]++;
  }
  const indexed = centers.map((rgb, i) => ({ rgb, count: clusterCounts[i] }));
  indexed.sort((a, b) => b.count - a.count);
  return indexed.map((r) => r.rgb);
}

// ── Harmony 配色方案 ────────────────────────────────────

/**
 * 基于 base RGB 生成 harmony 配色方案。
 * 每个色相生成 3 档明暗梯度（亮/中/暗），使色板更实用。
 *
 * @param {number[]} baseRgb - [r, g, b]
 * @param {string} scheme - 'complementary' | 'triadic' | 'analogous' | 'splitComplementary'
 * @returns {number[][]} 配色数组 [[r,g,b], ...]
 */
function generateHarmony(baseRgb, scheme) {
  const [r, g, b] = baseRgb;
  const hsl = rgbToHsl(r, g, b);
  const h = hsl.h;

  let hues;
  switch (scheme) {
    case 'complementary':
      hues = [h, (h + 180) % 360];
      break;
    case 'triadic':
      hues = [h, (h + 120) % 360, (h + 240) % 360];
      break;
    case 'analogous':
      hues = [(h + 330) % 360, h, (h + 30) % 360];
      break;
    case 'splitComplementary':
      hues = [h, (h + 150) % 360, (h + 210) % 360];
      break;
    default:
      hues = [h];
  }

  // 每个色相生成 3 档明暗梯度
  // 0.15.9-fix：返回数组 [r,g,b] 格式，与 medianCut 输出一致
  const colors = [];
  for (const hue of hues) {
    const s = Math.min(100, Math.max(20, hsl.s));
    const c1 = hslToRgb(hue, s, Math.min(75, hsl.l + 18));
    const c2 = hslToRgb(hue, s, hsl.l);
    const c3 = hslToRgb(hue, s, Math.max(20, hsl.l - 18));
    colors.push([c1.r, c1.g, c1.b]);
    colors.push([c2.r, c2.g, c2.b]);
    colors.push([c3.r, c3.g, c3.b]);
  }
  return colors;
}

// ── UI 渲染与交互 ────────────────────────────────────────

/** 提取按钮 */
let extractBtn = null;
/** 主色板容器 */
let paletteEl = null;
/** harmony 区域 */
let harmonyEl = null;
/** harmony 方案按钮组 */
let harmonySchemes = null;
/** harmony 色板容器 */
let harmonySwatches = null;
/** 最近提取的 base color [r,g,b] */
let lastBaseColor = null;
/** 0.15.10：提取颜色数量选择器 */
let paletteCountSelect = null;

/**
 * 渲染色板到容器。
 * 每个色块：
 * - 左键点击 → 设为当前色 + 同步色盘
 * - 右键点击 → 复制 HEX 到剪贴板 + 视觉反馈
 *
 * @param {HTMLElement} container - 色板容器
 * @param {number[][]} colors - [[r,g,b], ...]
 */
function renderSwatches(container, colors) {
  if (!container || !Array.isArray(colors)) return;
  container.innerHTML = '';
  for (const color of colors) {
    // 0.15.9-fix：兼容数组 [r,g,b] 和对象 {r,g,b} 两种格式
    let r, g, b;
    if (Array.isArray(color)) {
      [r, g, b] = color;
    } else {
      r = color.r; g = color.g; b = color.b;
    }
    if (r === undefined || g === undefined || b === undefined) continue;
    const hex = rgbToHex(r, g, b);
    const swatch = document.createElement('button');
    swatch.className = 'palette-swatch';
    swatch.style.background = hex;
    swatch.title = `${hex}  (右键复制)`;

    // 左键：设为当前色
    swatch.addEventListener('click', (e) => {
      e.stopPropagation();
      annot.setColor(hex);
      const dot = document.getElementById('color-trigger-dot');
      if (dot) dot.style.background = hex;
      syncFromAnnot();
    });

    // 右键：复制 HEX
    swatch.addEventListener('contextmenu', (e) => {
      e.preventDefault();
      e.stopPropagation();
      navigator.clipboard.writeText(hex).then(() => {
        swatch.classList.add('copied');
        setTimeout(() => swatch.classList.remove('copied'), 600);
      }).catch(() => {});
    });

    // 阻止 mousedown 穿透导致下拉关闭
    swatch.addEventListener('mousedown', (e) => e.stopPropagation());

    container.appendChild(swatch);
  }
}

/** 提取主色调 */
function extractPalette() {
  let sourceCanvas = annot.getCropSourceCanvas();
  if (!sourceCanvas) {
    // 0.15.8-fix：回退到全屏截图离屏 canvas
    sourceCanvas = ss.screenshotOffscreen;
  }
  if (!sourceCanvas) {
    console.warn('[palette] 无可用截图 canvas，无法提取配色');
    return;
  }

  // 0.15.10：从控件读取提取数量
  let numColors = 5;
  if (paletteCountSelect) {
    numColors = parseInt(paletteCountSelect.value, 10) || 5;
  }

  // 降采样 + k-means 聚类
  const pixels = downsampleCanvas(sourceCanvas, 64);
  if (pixels.length === 0) {
    console.warn('[palette] 降采样后无有效像素');
    return;
  }
  const colors = kMeans(pixels, numColors);

  // 渲染主色板
  renderSwatches(paletteEl, colors);
  paletteEl.hidden = false;

  // 存储 base color 供 harmony 使用（取频率最高的第一个色）
  lastBaseColor = colors[0];

  // 显示 harmony 区域
  if (harmonyEl) {
    harmonyEl.hidden = false;
    // 清空之前的 harmony 色板
    if (harmonySwatches) harmonySwatches.innerHTML = '';
  }
}

/** 生成 harmony 配色 */
function handleHarmony(scheme) {
  if (!lastBaseColor) return;
  const colors = generateHarmony(lastBaseColor, scheme);
  renderSwatches(harmonySwatches, colors);
}

/** 初始化配色提取模块（幂等，在 initColorPicker 中调用） */
export function initPalette() {
  const dropdown = document.getElementById('color-dropdown');
  if (!dropdown) return;

  extractBtn = dropdown.querySelector('.palette-extract-btn');
  paletteEl = dropdown.querySelector('.palette-extracted');
  harmonyEl = dropdown.querySelector('.palette-harmony');
  harmonySchemes = harmonyEl ? harmonyEl.querySelector('.harmony-schemes') : null;
  harmonySwatches = harmonyEl ? harmonyEl.querySelector('.harmony-swatches') : null;
  paletteCountSelect = dropdown.querySelector('.palette-count');

  if (extractBtn) {
    extractBtn.addEventListener('click', (e) => {
      e.stopPropagation();
      extractPalette();
    });
    extractBtn.addEventListener('mousedown', (e) => e.stopPropagation());
  }

  // 0.15.10：提取数量滚轮切换
  if (paletteCountSelect) {
    paletteCountSelect.addEventListener('wheel', (e) => {
      e.preventDefault();
      e.stopPropagation();
      const opts = Array.from(paletteCountSelect.options);
      let idx = paletteCountSelect.selectedIndex;
      idx += e.deltaY > 0 ? 1 : -1;
      idx = Math.max(0, Math.min(opts.length - 1, idx));
      paletteCountSelect.selectedIndex = idx;
    }, { passive: false });
    paletteCountSelect.addEventListener('mousedown', (e) => e.stopPropagation());
  }

  if (harmonySchemes) {
    harmonySchemes.querySelectorAll('.harmony-scheme-btn').forEach((btn) => {
      btn.addEventListener('click', (e) => {
        e.stopPropagation();
        handleHarmony(btn.dataset.scheme);
      });
      btn.addEventListener('mousedown', (e) => e.stopPropagation());
    });
  }
}
