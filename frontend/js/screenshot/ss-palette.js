//! 0.20.7：来源无关配色核心与可用输出。
//!
//! 功能：
//! - 整图分块扫描固定直方图，提取色始终来自原始像素
//! - 首屏展示 3 个图片提取方案；数学配色由显式基准色独立生成
//! - 显示 WCAG 对比度值和推荐黑/白文字色
//! - 支持 Ctrl+左键跨方案多选、复制所选/整组；输出格式含 HEX/RGB/HSL、纯颜色列表、CSS variables
//! - 色块左键设标注色，右键单色复制
//! - Worker 负责紧凑直方图聚类；主线程分块扫描时主动让出事件循环
//!
//! 算法说明：
//! - 扫描：5-bit RGB 固定直方图，每桶保留原图真实代表像素
//! - 聚类：在 OKLab 空间对直方图桶做带权 k-means，K=1..8
//! - 角色色：background / accent / foreground / muted
//! - harmony：RGB→HSL → 色相偏移 → HSL→RGB，每色相 3 档明暗梯度

import * as annot from './annotation-engine.js';
import { ss } from './ss-state.js';
import { syncFromAnnot } from './ss-color-picker.js';
import { copyToClipboard as copyTextToClipboard } from '../shared/api.js';
import {
  analyzePaletteHistogram,
  accumulateColorHistogram,
  createColorHistogramAccumulator,
  finalizeColorHistogram,
  generateDesignPalettes,
  PALETTE_ALGORITHM_V1,
  rgbToHex,
  formatAsCssVariables,
  formatOutput,
} from '../shared/color/palette-core.js';

// ── DOM 引用 ──────────────────────────────────────────────

let extractBtn = null;
let paletteEl = null;
let harmonyEl = null;
let harmonySwatches = null;
let moreSchemesEl = null;
let outputFormatSelect = null;
let copyAllBtn = null;
let copyStatusEl = null;
let themeSummaryEl = null;
let moreToggleEl = null;
let actionsRowEl = null;

/** 防抖定时器 */
let debounceTimer = 0;
let pendingHistogram = null;
/** Worker 已回传结果的最新 epoch（看门狗据此判断本轮是否仍在等待） */
let workerResultEpoch = -1;
/** 每个 epoch 的 watchdog timer 和完成状态 */
let workerWatchdogTimer = 0;
/** 当前 watchdog 对应的 epoch（只取消/触发属于该 epoch 的 timer） */
let workerWatchdogEpoch = -1;

/** 新截图/图片编辑会话开始时清理上一轮分析、展开与多选状态。 */
export function resetPaletteState() {
  clearTimeout(debounceTimer);
  debounceTimer = 0;
  // 清理 watchdog timer，防止旧 epoch 的超时回调影响新会话
  if (workerWatchdogTimer) {
    clearTimeout(workerWatchdogTimer);
    workerWatchdogTimer = 0;
  }
  workerWatchdogEpoch = -1;
  // 让已经发给 Worker 的旧任务失效，避免结果回写到新截图。
  ss.paletteEpoch++;
  ss.paletteResult = null;
  ss.paletteSelected = new Set();
  ss.paletteColorOrder = [];
  ss.paletteFormat = 'hex';
  ss.paletteMoreExpanded = false;
  ss.paletteAnchorHex = null;
  pendingHistogram = null;

  if (paletteEl) {
    paletteEl.replaceChildren();
    paletteEl.hidden = true;
  }
  if (harmonySwatches) harmonySwatches.replaceChildren();
  if (moreSchemesEl) {
    moreSchemesEl.replaceChildren();
    moreSchemesEl.hidden = true;
  }
  if (harmonyEl) harmonyEl.hidden = true;
  if (themeSummaryEl) {
    themeSummaryEl.textContent = '';
    themeSummaryEl.title = '';
    themeSummaryEl.hidden = true;
  }
  if (actionsRowEl) actionsRowEl.hidden = true;
  if (copyStatusEl) copyStatusEl.textContent = '';
  if (extractBtn) extractBtn.disabled = false;
  if (outputFormatSelect) outputFormatSelect.value = 'hex';
  if (moreToggleEl) moreToggleEl.textContent = '生成当前色配色方案';
  updateCopyButtonLabel();
}

// ── Worker 管理 ────────────────────────────────────────────

/**
 * 确保 Worker 已创建（幂等）。
 * Worker 创建失败时返回 null，主线程降级处理。
 *
 * @returns {Worker | null}
 */
function ensureWorker() {
  if (ss.paletteWorker) return ss.paletteWorker;
  try {
    // 纯 ES module worker，无构建步骤
    ss.paletteWorker = new Worker(
      new URL('../shared/color/palette-worker.js', import.meta.url),
      { type: 'module' }
    );
    ss.paletteWorker.onmessage = onWorkerMessage;
    ss.paletteWorker.onerror = onWorkerError;
  } catch (err) {
    console.warn('[palette] Worker 创建失败，降级到主线程', err);
    ss.paletteWorker = null;
  }
  return ss.paletteWorker;
}

/**
 * 销毁当前 Worker（异常/超时后调用），让下次 ensureWorker 重建。
 * 异常后的 Worker 已不可信，静默死亡时 postMessage 不报错也不回结果。
 */
function destroyWorker() {
  if (!ss.paletteWorker) return;
  try {
    ss.paletteWorker.terminate();
  } catch {
    // terminate 失败也直接丢弃引用
  }
  ss.paletteWorker = null;
}

/**
 * Worker 消息处理。校验 version/epoch，旧 epoch 丢弃。
 * @param {MessageEvent} e
 */
function onWorkerMessage(e) {
  const msg = e.data;
  if (!msg) return;

  // 旧 epoch 丢弃
  if (msg.epoch !== ss.paletteEpoch) return;

  if (msg.type === 'result') {
    if (extractBtn) extractBtn.disabled = false;
    workerResultEpoch = msg.epoch;
    // 0.20.7：result 后结束该 epoch 的 watchdog timer
    clearWatchdogTimer(msg.epoch);
    ss.paletteResult = msg.result;
    renderPalette(msg.result);
  } else if (msg.type === 'error') {
    console.warn('[palette] Worker 错误:', msg.message);
    // 0.20.7：error 后也结束该 epoch 的 watchdog timer，防止 fallback 成功后 watchdog 仍触发
    clearWatchdogTimer(msg.epoch);
    // 降级到主线程
    fallbackMainThreadAnalysis(msg.epoch);
  }
}

/**
 * 清理指定 epoch 的 watchdog timer。
 * 超时回调只销毁它启动时对应的 Worker 实例，不能误杀后来重建的 Worker。
 */
function clearWatchdogTimer(epoch) {
  if (workerWatchdogTimer && workerWatchdogEpoch === epoch) {
    clearTimeout(workerWatchdogTimer);
    workerWatchdogTimer = 0;
    workerWatchdogEpoch = -1;
  }
}

/**
 * Worker 异常处理。
 * 异常后的 Worker 已不可信：先销毁（下次 ensureWorker 重建），再降级主线程。
 * @param {ErrorEvent} e
 */
function onWorkerError(e) {
  console.warn('[palette] Worker 异常:', e.message || e);
  clearWatchdogTimer(ss.paletteEpoch);
  destroyWorker();
  // 降级到主线程
  if (ss.paletteEpoch > 0) {
    fallbackMainThreadAnalysis(ss.paletteEpoch);
  }
}

/**
 * 主线程降级分析（Worker 不可用时）。
 */
function fallbackMainThreadAnalysis(epoch) {
  if (!pendingHistogram || epoch !== ss.paletteEpoch) return;
  try {
    const result = analyzePaletteHistogram(
      pendingHistogram,
      pendingHistogram.width,
      pendingHistogram.height,
    );
    if (epoch !== ss.paletteEpoch) return;
    if (extractBtn) extractBtn.disabled = false;
    ss.paletteResult = result;
    // 0.20.7：fallback 成功后结束该 epoch 的 watchdog timer
    clearWatchdogTimer(epoch);
    renderPalette(result);
  } catch (err) {
    console.warn('[palette] 主线程降级分析失败:', err);
    if (extractBtn) extractBtn.disabled = false;
    clearWatchdogTimer(epoch);
  }
}

// ── 原图整图扫描 ──────────────────────────────────────────

function getSelectionImageData() {
  const crop = annot.getCropImageData();
  if (crop?.data?.length && crop.width > 0 && crop.height > 0) return crop;
  const source = annot.getCropSourceCanvas() || ss.editorSession?.baseCanvas;
  if (!source?.width || !source?.height) return null;
  try {
    return source.getContext('2d', { willReadFrequently: true })
      .getImageData(0, 0, source.width, source.height);
  } catch (error) {
    console.warn('[palette] 读取原始选区像素失败', error);
    return null;
  }
}

/** 分块扫描整张原图；固定桶内存约 320KB，不创建逐像素对象。 */
async function scanFullImageHistogram(imageData, epoch) {
  const data = imageData.data;
  const accumulator = createColorHistogramAccumulator();
  const pixelsPerChunk = 500_000;
  const totalPixels = Math.floor(data.length / 4);

  for (let startPixel = 0; startPixel < totalPixels; startPixel += pixelsPerChunk) {
    if (epoch !== ss.paletteEpoch) return null;
    const endPixel = Math.min(totalPixels, startPixel + pixelsPerChunk);
    accumulateColorHistogram(accumulator, data, startPixel, endPixel);
    if (endPixel < totalPixels) await new Promise((resolve) => setTimeout(resolve, 0));
  }

  return {
    ...finalizeColorHistogram(accumulator, totalPixels),
    width: imageData.width,
    height: imageData.height,
  };
}

// ── 配色分析触发 ───────────────────────────────────────────

/**
 * 触发配色分析（防抖 120ms）。
 * 选区变化后 120ms 启动最新分析，旧 epoch 不覆盖新结果。
 */
export function triggerPaletteAnalysis() {
  clearTimeout(debounceTimer);
  const C = PALETTE_ALGORITHM_V1;
  debounceTimer = setTimeout(async () => {
    ss.paletteEpoch++;
    const epoch = ss.paletteEpoch;
    const imageData = getSelectionImageData();
    if (!imageData) return;
    if (extractBtn) extractBtn.disabled = true;
    if (copyStatusEl) copyStatusEl.textContent = `正在扫描整块选区 ${imageData.width}×${imageData.height}…`;
    const histogram = await scanFullImageHistogram(imageData, epoch);
    if (!histogram || epoch !== ss.paletteEpoch) return;
    pendingHistogram = histogram;

    const worker = ensureWorker();
    if (worker) {
      // 捕获 Worker 实例引用——超时回调只销毁它启动时对应的 Worker，不能误杀后来重建的
      const workerRef = worker;
      const watchdogEpoch = epoch;
      worker.postMessage({
        type: 'analyze-histogram',
        version: C.WORKER_VERSION,
        epoch,
        histogram,
        width: histogram.width,
        height: histogram.height,
      });
      // 静默死亡看门狗：Worker 无 error 事件但也迟迟不回结果时，
      // 销毁重建 + 降级主线程，避免按钮永久 disabled / 状态卡在"正在扫描"
      // 0.20.7：保存 timer 和 epoch；result、error、fallback 完成、reset 都 clear
      clearWatchdogTimer(watchdogEpoch);
      workerWatchdogTimer = setTimeout(() => {
        // 只处理属于该 epoch 的超时，且只销毁启动时的 Worker 实例
        if (watchdogEpoch !== ss.paletteEpoch) return;
        if (workerResultEpoch < watchdogEpoch) {
          // 再次检查：result/error 可能在 timer 排队期间到达
          if (workerRef === ss.paletteWorker) {
            console.warn('[palette] Worker 分析超时，销毁并降级到主线程');
            destroyWorker();
          }
          fallbackMainThreadAnalysis(watchdogEpoch);
        }
      }, C.WORKER_TIMEOUT_MS);
      workerWatchdogEpoch = watchdogEpoch;
    } else {
      fallbackMainThreadAnalysis(epoch);
    }
  }, C.DEBOUNCE_MS);
}

// ── UI 渲染 ──────────────────────────────────────────────────

/**
 * 渲染配色结果到 UI。
 * @param {{roles: Array, recommended: Array, full: Array, empty: boolean}} result
 */
function renderPalette(result) {
  if (!result || result.empty) {
    if (paletteEl) {
      paletteEl.innerHTML = '';
      paletteEl.hidden = true;
    }
    if (harmonyEl) harmonyEl.hidden = true;
    if (themeSummaryEl) {
      themeSummaryEl.textContent = '无有效像素';
      themeSummaryEl.hidden = false;
    }
    if (actionsRowEl) actionsRowEl.hidden = true;
    if (moreSchemesEl) moreSchemesEl.hidden = true;
    if (copyStatusEl) copyStatusEl.textContent = '';
    notifyPaletteLayoutChanged();
    return;
  }

  // 新一轮分析默认不多选；普通左键仍是“设为标注色”，Ctrl+左键才进入批量选择。
  ss.paletteSelected = new Set();
  ss.paletteColorOrder = collectPaletteColorOrder(result);
  ss.paletteMoreExpanded = false;
  const focusColor = result.recommended.find((scheme) => scheme.scheme === 'salient')?.colors?.[0];
  const firstColor = focusColor || result.roles[0]?.rgb;
  ss.paletteAnchorHex = firstColor ? rgbToHex(...firstColor) : null;
  if (copyStatusEl) copyStatusEl.textContent = '';
  updateGenerateButtonLabel();
  if (moreSchemesEl) moreSchemesEl.hidden = true;

  // 图片主题色作为推荐区第一张卡片，不暴露内部角色名。
  renderRoleSwatches(result.roles);
  paletteEl.hidden = false;
  if (themeSummaryEl) {
    const sampleSize = result.sample?.width && result.sample?.height
      ? ` · 整图扫描 ${result.sample.width}×${result.sample.height}`
      : '';
    themeSummaryEl.textContent = `${result.theme?.summary || `${result.roles.length} 个主题色`}${sampleSize}`;
    themeSummaryEl.title = result.sample
      ? `逐像素扫描整块选区，共分析 ${result.sample.validPixels} 个有效像素；提取色均来自原图真实像素`
      : '';
    themeSummaryEl.hidden = false;
  }

  if (actionsRowEl) actionsRowEl.hidden = false;
  updateCopyButtonLabel();

  // 渲染推荐方案
  if (harmonyEl) {
    harmonyEl.hidden = false;
    renderRecommendedSchemes(result.recommended.filter((scheme) => scheme.scheme !== 'source'));

  }
  notifyPaletteLayoutChanged();
}

function updateCopyButtonLabel() {
  if (!copyAllBtn) return;
  const count = ss.paletteSelected.size;
  const label = copyAllBtn.querySelector('span');
  if (label) label.textContent = count > 0 ? `复制 ${count} 色` : '复制多选';
  copyAllBtn.disabled = count === 0;
}

function notifyPaletteLayoutChanged() {
  const dropdown = document.getElementById('color-dropdown');
  requestAnimationFrame(() => dropdown?.dispatchEvent(new CustomEvent('palette-layout-changed')));
}

function collectPaletteColorOrder(result) {
  const seen = new Set();
  const ordered = [];
  const addRgb = (rgb) => {
    const hex = rgbToHex(rgb[0], rgb[1], rgb[2]);
    if (seen.has(hex)) return;
    seen.add(hex);
    ordered.push(hex);
  };
  result.roles.forEach((role) => addRgb(role.rgb));
  result.recommended.forEach((scheme) => scheme.colors.forEach(addRgb));
  result.full.forEach((scheme) => scheme.colors.forEach(addRgb));
  return ordered;
}

function replacePaletteColorOrder(generatedSchemes = []) {
  if (!ss.paletteResult) return;
  const ordered = collectPaletteColorOrder(ss.paletteResult);
  const seen = new Set(ordered);
  for (const scheme of generatedSchemes) {
    for (const rgb of scheme.colors) {
      const hex = rgbToHex(...rgb);
      if (seen.has(hex)) continue;
      seen.add(hex);
      ordered.push(hex);
    }
  }
  ss.paletteColorOrder = ordered;
  ss.paletteSelected = new Set([...ss.paletteSelected].filter((hex) => seen.has(hex)));
  updateCopyButtonLabel();
}

function setAnnotationColor(hex) {
  annot.setColor(hex);
  ss.paletteAnchorHex = hex;
  const dot = document.getElementById('color-trigger-dot');
  if (dot) dot.style.background = hex;
  syncFromAnnot();
  updateGenerateButtonLabel();
  if (ss.paletteMoreExpanded) renderGeneratedSchemes();
}

function updateGenerateButtonLabel() {
  if (!moreToggleEl) return;
  if (ss.paletteMoreExpanded) {
    moreToggleEl.textContent = '收起配色方案';
    return;
  }
  moreToggleEl.textContent = ss.paletteAnchorHex
    ? `生成 ${ss.paletteAnchorHex} 配色方案`
    : '生成当前色配色方案';
}

function togglePaletteSelection(hex) {
  const selected = !ss.paletteSelected.has(hex);
  if (selected) ss.paletteSelected.add(hex);
  else ss.paletteSelected.delete(hex);
  document.querySelectorAll(`[data-palette-hex="${hex}"]`)
    .forEach((el) => el.classList.toggle('is-selected', selected));
  updateCopyButtonLabel();
}

function bindPaletteColor(el, hex) {
  el.dataset.paletteHex = hex;
  el.classList.toggle('is-selected', ss.paletteSelected.has(hex));
  el.title = `${hex} · 左键设色 · Ctrl+左键多选 · 右键复制`;
  el.addEventListener('click', (e) => {
    e.stopPropagation();
    if (e.ctrlKey || e.metaKey) togglePaletteSelection(hex);
    else setAnnotationColor(hex);
  });
  el.addEventListener('contextmenu', (e) => {
    e.preventDefault();
    e.stopPropagation();
    void copyToClipboard(hex, el);
  });
  el.addEventListener('mousedown', (e) => e.stopPropagation());
}

/**
 * 渲染角色色 swatch。
 * 每个色块：
 * - 左键 → 设为当前色 + 同步色盘
 * - 右键 → 复制 HEX
 * - Ctrl+左键 → 加入/移出跨方案多选
 *
 * @param {Array} roles - 角色色数组
 */
function renderRoleSwatches(roles) {
  if (!paletteEl) return;
  paletteEl.innerHTML = '';

  const header = document.createElement('div');
  header.className = 'harmony-scheme-header';
  const heading = document.createElement('span');
  heading.className = 'harmony-scheme-label';
  heading.textContent = '图片主题色';
  const description = document.createElement('span');
  description.className = 'harmony-scheme-description';
  description.textContent = `原图聚类 · ${roles.length} 色`;
  const copyGroupBtn = document.createElement('button');
  copyGroupBtn.className = 'harmony-copy-group';
  copyGroupBtn.textContent = '复制整组';
  copyGroupBtn.addEventListener('click', (e) => {
    e.stopPropagation();
    void copyScheme({ label: '图片主题色', colors: roles.map((role) => role.rgb) }, copyGroupBtn);
  });
  copyGroupBtn.addEventListener('mousedown', (e) => e.stopPropagation());
  header.append(heading, description, copyGroupBtn);

  const row = document.createElement('div');
  row.className = 'palette-theme-row';

  for (const role of roles) {
    const hex = role.hex;
    const item = document.createElement('button');
    item.className = 'palette-theme-color';
    item.dataset.role = role.role;

    const colorBlock = document.createElement('span');
    colorBlock.className = 'palette-theme-swatch';
    colorBlock.style.background = hex;

    const text = document.createElement('span');
    text.className = 'palette-theme-label';
    text.textContent = `${(role.ratio * 100).toFixed(0)}%`;

    item.append(colorBlock, text);
    bindPaletteColor(item, hex);
    row.appendChild(item);
  }
  paletteEl.append(header, row);
}

/**
 * 渲染推荐方案（首屏最多 3 个）。
 * @param {Array} schemes
 */
function renderRecommendedSchemes(schemes) {
  if (!harmonySwatches) return;
  harmonySwatches.innerHTML = '';

  // 推荐方案不再使用 tab 二次点击，最多 3 组直接完整展开。
  for (const scheme of schemes.slice(0, 3)) {
    renderSingleHarmony(scheme);
  }
}

/**
 * 渲染单个 harmony 方案。
 * @param {{label: string, scheme: string, colors: number[][]}} scheme
 */
function renderSingleHarmony(scheme, target = harmonySwatches) {
  if (!target) return;

  const card = document.createElement('section');
  card.className = 'harmony-scheme-card';

  const header = document.createElement('div');
  header.className = 'harmony-scheme-header';
  const label = document.createElement('span');
  label.className = 'harmony-scheme-label';
  label.textContent = scheme.label;
  const description = document.createElement('span');
  description.className = 'harmony-scheme-description';
  description.textContent = scheme.description || '';
  const copyGroupBtn = document.createElement('button');
  copyGroupBtn.className = 'harmony-copy-group';
  copyGroupBtn.textContent = '复制整组';
  copyGroupBtn.addEventListener('click', (e) => {
    e.stopPropagation();
    void copyScheme(scheme, copyGroupBtn);
  });
  copyGroupBtn.addEventListener('mousedown', (e) => e.stopPropagation());
  header.append(label, description, copyGroupBtn);
  card.appendChild(header);

  // 色块行
  const row = document.createElement('div');
  row.className = 'harmony-color-row';
  for (const rgb of scheme.colors) {
    const hex = rgbToHex(rgb[0], rgb[1], rgb[2]);
    const swatch = document.createElement('button');
    swatch.className = 'palette-swatch';
    swatch.style.background = hex;
    bindPaletteColor(swatch, hex);
    row.appendChild(swatch);
  }
  card.appendChild(row);

  target.appendChild(card);
}

/**
 * 渲染完整方案（"更多方案"展开时）。
 * @param {Array} schemes
 */
function renderFullSchemes(schemes, heading = '') {
  if (!moreSchemesEl) return;
  moreSchemesEl.innerHTML = '';
  if (heading) {
    const label = document.createElement('div');
    label.className = 'palette-generated-heading';
    label.textContent = heading;
    moreSchemesEl.appendChild(label);
  }
  schemes.forEach((scheme) => renderSingleHarmony(scheme, moreSchemesEl));
}

function renderGeneratedSchemes() {
  if (!ss.paletteAnchorHex || !ss.paletteResult) return;
  const anchor = [
    parseInt(ss.paletteAnchorHex.slice(1, 3), 16),
    parseInt(ss.paletteAnchorHex.slice(3, 5), 16),
    parseInt(ss.paletteAnchorHex.slice(5, 7), 16),
  ];
  const sourceColors = ss.paletteResult.roles.map((role) => role.rgb);
  const schemes = generateDesignPalettes(anchor, sourceColors);
  renderFullSchemes(schemes, `基于 ${ss.paletteAnchorHex} 生成 · 非原图提取色`);
  replacePaletteColorOrder(schemes);
}

// ── 复制 ──────────────────────────────────────────────────

/**
 * 复制文本到剪贴板，并显示视觉反馈。
 * @param {string} text
 * @param {HTMLElement} [feedbackEl] - 显示 copied class 的元素
 */
async function copyToClipboard(text, feedbackEl) {
  try {
    await copyTextToClipboard(text);
    if (feedbackEl) {
      feedbackEl.classList.add('copied');
      setTimeout(() => feedbackEl.classList.remove('copied'), 600);
    }
    if (copyStatusEl) copyStatusEl.textContent = '已复制到剪贴板';
    return true;
  } catch (error) {
    console.warn('[palette] 写入剪贴板失败', error);
    if (feedbackEl) {
      feedbackEl.classList.add('copy-failed');
      setTimeout(() => feedbackEl.classList.remove('copy-failed'), 600);
    }
    if (copyStatusEl) copyStatusEl.textContent = '复制失败，请重试';
    return false;
  }
}

function formatPaletteColors(hexColors) {
  if (ss.paletteFormat === 'list') return hexColors.join('\n');
  if (ss.paletteFormat !== 'css') return formatOutput(hexColors, ss.paletteFormat);

  const roleByHex = new Map((ss.paletteResult?.roles || []).map((role) => [role.hex, role]));
  const cssRoles = hexColors.map((hex, index) => (
    roleByHex.get(hex) || { hex, role: `selected-${index + 1}` }
  ));
  return formatAsCssVariables(cssRoles);
}

async function copyScheme(scheme, feedbackEl) {
  const seen = new Set();
  const hexColors = [];
  for (const rgb of scheme.colors) {
    const hex = rgbToHex(rgb[0], rgb[1], rgb[2]);
    if (seen.has(hex)) continue;
    seen.add(hex);
    hexColors.push(hex);
  }
  const copied = await copyToClipboard(formatPaletteColors(hexColors), feedbackEl);
  if (copied && copyStatusEl) {
    copyStatusEl.textContent = `已复制“${scheme.label}”${hexColors.length} 色`;
  }
}

/** 复制 Ctrl+左键选中的任意主题色/方案色。 */
async function copySelected() {
  const hexColors = ss.paletteColorOrder.filter((hex) => ss.paletteSelected.has(hex));
  if (hexColors.length === 0) return;

  const copied = await copyToClipboard(formatPaletteColors(hexColors), copyAllBtn);
  if (copied && copyStatusEl) {
    copyStatusEl.textContent = `已复制 ${hexColors.length} 色 · ${ss.paletteFormat.toUpperCase()}`;
  }
}

// ── 初始化 ──────────────────────────────────────────────────

/**
 * 初始化配色提取模块（幂等，在 initColorPicker 中调用）。
 */
export function initPalette() {
  const dropdown = document.getElementById('color-dropdown');
  if (!dropdown) return;

  extractBtn = dropdown.querySelector('.palette-extract-btn');
  paletteEl = dropdown.querySelector('.palette-extracted');
  harmonyEl = dropdown.querySelector('.palette-harmony');
  harmonySwatches = harmonyEl ? harmonyEl.querySelector('.harmony-swatches') : null;
  moreSchemesEl = dropdown.querySelector('.palette-more-schemes');
  outputFormatSelect = dropdown.querySelector('.palette-output-format');
  copyAllBtn = dropdown.querySelector('.palette-copy-all');
  copyStatusEl = dropdown.querySelector('.palette-copy-status');
  themeSummaryEl = dropdown.querySelector('.palette-theme-summary');
  moreToggleEl = dropdown.querySelector('.palette-more-toggle');
  actionsRowEl = dropdown.querySelector('.palette-actions-row');

  dropdown.addEventListener('wheel', (e) => e.stopPropagation(), { passive: true });

  if (extractBtn) {
    extractBtn.addEventListener('click', (e) => {
      e.stopPropagation();
      triggerPaletteAnalysis();
    });
    extractBtn.addEventListener('mousedown', (e) => e.stopPropagation());
  }

  // 输出格式选择
  if (outputFormatSelect) {
    outputFormatSelect.addEventListener('change', (e) => {
      e.stopPropagation();
      ss.paletteFormat = e.target.value;
    });
    outputFormatSelect.addEventListener('mousedown', (e) => e.stopPropagation());
  }

  // 复制按钮
  if (copyAllBtn) {
    copyAllBtn.addEventListener('click', (e) => {
      e.stopPropagation();
      void copySelected();
    });
    copyAllBtn.addEventListener('mousedown', (e) => e.stopPropagation());
  }

  // 显式基准色的配色生成器展开/折叠
  if (moreToggleEl) {
    moreToggleEl.addEventListener('click', (e) => {
      e.stopPropagation();
      ss.paletteMoreExpanded = !ss.paletteMoreExpanded;
      updateGenerateButtonLabel();
      if (ss.paletteMoreExpanded && ss.paletteAnchorHex && moreSchemesEl) {
        renderGeneratedSchemes();
        moreSchemesEl.hidden = false;
      } else if (moreSchemesEl) {
        moreSchemesEl.hidden = true;
        replacePaletteColorOrder();
      }
      notifyPaletteLayoutChanged();
    });
    moreToggleEl.addEventListener('mousedown', (e) => e.stopPropagation());
  }

  // 预热 Worker（不阻塞 UI）
  ensureWorker();
}
