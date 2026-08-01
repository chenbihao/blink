//! 0.15.7 长截图：三阶段状态机 + 滚动检测 + 帧拼接 + 预览 + 编辑入口。
//!
//! **三阶段**：
//! - `idle` → 用户点工具栏【长截图】按钮 → `capturing`
//! - `capturing` → 用户滚动（手动/自动），逐帧截取拼接 → 点【编辑】 → `editing`
//! - `editing` → 合成长图进标注模式 → pin/保存/复制 → 结束
//!
//! **帧拼接算法**（SAD 模板匹配）：
//! 1. 取上一帧底部 N 行（如 20 行）作为模板
//! 2. 在新帧顶部区域垂直滑动，计算 SAD（绝对差之和）
//! 3. 最小 SAD 位置 = 重叠量，裁掉重叠部分后拼接
//! 4. 退化：纯色/重复纹理匹配失败 → 降级直接拼接
//!
//! **性能策略**：
//! - getImageData 从主 canvas 取帧（`willReadFrequently` 已设）
//! - 拼接在 OffscreenCanvas（或临时 canvas）上完成
//! - 预览缩略图用小尺寸 canvas，每帧追加渲染

import { ss } from './ss-state.js';
import { screenshotPin, screenshotSave, screenshotCopy, screenshotCancel, screenshotAutoScroll, screenshotStopScroll } from '../shared/api.js';

/** 模板匹配的行数（取上一帧底部 N 行作模板） */
const OVERLAP_TEMPLATE_ROWS = 20;
/** SAD 搜索范围（新帧顶部 0~SEARCH_RANGE 行内找最佳匹配） */
const SAD_SEARCH_RANGE = 100;
/** 预览缩略图宽度（CSS 像素） */
const PREVIEW_W = 120;

/**
 * 进入长截图采集阶段（capturing）。
 * 由 index.js 的【长截图】按钮调用。
 *
 * @param {object} rect - 框选矩形 CSS 坐标 {x, y, w, h}
 */
export function enterScrollCapture(rect) {
  const dpr = window.devicePixelRatio || 1;
  const meta = window.__blinkScreenMeta || { vx: 0, vy: 0 };

  // 记录采集带几何（物理像素，虚拟屏幕坐标系）
  ss.scrollBandX = meta.vx + Math.round(rect.x * dpr);
  ss.scrollBandY = meta.vy + Math.round(rect.y * dpr);
  ss.scrollBandW = Math.round(rect.w * dpr);
  ss.scrollDirection = 'vertical';
  ss.autoScroll = false;
  ss.scrollFrames = [];
  ss.scrollCapturePhase = 'capturing';

  // 记录前台窗口 HWND（供 Rust 端 PostMessage 自动滚动用）
  // overlay 显示时前台已被遮挡，但截图 session begin 时已记录
  ss.scrollHwnd = meta.fgHwnd || null;

  // 切换工具栏：隐藏默认，显示专属
  if (ss.toolbar) ss.toolbar.classList.add('hidden');
  const scrollTb = document.getElementById('scroll-toolbar');
  if (scrollTb) {
    scrollTb.classList.remove('hidden');
    // 定位到选区下方
    scrollTb.style.left = (rect.x + rect.w / 2 - 100) + 'px';
    scrollTb.style.top = (rect.y + rect.h + 8) + 'px';
  }

  // 显示预览缩略图
  const preview = ss.scrollPreviewCanvas;
  if (preview) {
    preview.classList.remove('hidden');
    preview.style.right = '16px';
    preview.style.top = '50%';
    preview.style.transform = 'translateY(-50%)';
  }

  // 截取第一帧
  captureFrame();
  console.info('[scroll] enter capturing', { bandW: ss.scrollBandW, bandY: ss.scrollBandY });
}

/**
 * 截取当前帧并拼接到 scrollFrames。
 * 每次滚动后调用（手动 wheel 或自动滚动回调）。
 */
function captureFrame() {
  if (ss.scrollCapturePhase !== 'capturing') return;
  if (!ss.ctx || !ss.screenshot) return;

  const meta = window.__blinkScreenMeta || { vx: 0, vy: 0 };
  const dpr = window.devicePixelRatio || 1;

  // 从主 canvas 取采集带区域（物理像素）
  const px = Math.max(0, Math.round(ss.scrollBandX - meta.vx));
  const py = Math.max(0, Math.round(ss.scrollBandY - meta.vy));
  const pw = ss.scrollBandW;
  const ph = Math.round((ss.selCss?.h || 200) * dpr);

  try {
    const frame = ss.ctx.getImageData(px, py, pw, ph);
    ss.scrollFrames.push(frame);
    updatePreview();
    console.debug('[scroll] frame captured', { frameIdx: ss.scrollFrames.length, pw, ph });
  } catch (e) {
    console.warn('[scroll] captureFrame getImageData failed', e);
  }
}

/**
 * 更新预览缩略图。
 * 每帧追加渲染到底部。
 */
function updatePreview() {
  const ctx = ss.scrollPreviewCtx;
  const canvas = ss.scrollPreviewCanvas;
  if (!ctx || !canvas) return;

  const frames = ss.scrollFrames;
  if (frames.length === 0) return;

  const firstFrame = frames[0];
  const totalH = frames.reduce((sum, f) => sum + f.height, 0);
  const scale = PREVIEW_W / firstFrame.width;
  const previewH = Math.round(totalH * scale);

  // 调整 canvas 尺寸
  canvas.width = PREVIEW_W;
  canvas.height = Math.min(previewH, 600); // 限制预览高度
  canvas.style.height = canvas.height + 'px';

  // 逐帧绘制到缩略图
  let yOffset = 0;
  for (const frame of frames) {
    const scaledH = Math.round(frame.height * scale);
    // 用临时 canvas 中转（putImageData 不支持缩放）
    const tmp = document.createElement('canvas');
    tmp.width = frame.width;
    tmp.height = frame.height;
    tmp.getContext('2d').putImageData(frame, 0, 0);
    ctx.drawImage(tmp, 0, yOffset, PREVIEW_W, scaledH);
    yOffset += scaledH;
    if (yOffset >= canvas.height) break;
  }
}

/**
 * 合成所有帧为一张长图 ImageData。
 * 使用 SAD 模板匹配去重叠。
 *
 * @returns {ImageData|null} 合成后的长图
 */
function compositeLongImage() {
  const frames = ss.scrollFrames;
  if (frames.length === 0) return null;
  if (frames.length === 1) return frames[0];

  const w = frames[0].width;
  // 计算去重后的总高度
  let totalH = frames[0].height;
  const overlaps = []; // 每帧与上一帧的重叠量

  for (let i = 1; i < frames.length; i++) {
    const overlap = findOverlap(frames[i - 1], frames[i]);
    overlaps.push(overlap);
    totalH += frames[i].height - overlap;
  }

  // 合成
  const result = new ImageData(w, totalH);
  const tmpCanvas = document.createElement('canvas');
  const tmpCtx = tmpCanvas.getContext('2d');
  tmpCanvas.width = w;
  tmpCanvas.height = totalH;

  let yOffset = 0;
  // 第一帧
  tmpCtx.putImageData(frames[0], 0, yOffset);
  yOffset += frames[0].height;

  for (let i = 1; i < frames.length; i++) {
    const overlap = overlaps[i - 1];
    const startY = overlap;
    const frameH = frames[i].height - overlap;
    // 裁掉重叠部分后绘制
    const subTmp = document.createElement('canvas');
    subTmp.width = w;
    subTmp.height = frameH;
    subTmp.getContext('2d').putImageData(frames[i], 0, 0, 0, startY, w, frameH);
    tmpCtx.drawImage(subTmp, 0, yOffset);
    yOffset += frameH;
  }

  return tmpCtx.getImageData(0, 0, w, totalH);
}

/**
 * SAD 模板匹配：找上一帧底部在新帧顶部中的重叠位置。
 *
 * @param {ImageData} prevFrame - 上一帧
 * @param {ImageData} currFrame - 当前帧
 * @returns {number} 重叠行数（0 = 无重叠，直接拼接）
 */
function findOverlap(prevFrame, currFrame) {
  const w = Math.min(prevFrame.width, currFrame.width);
  const prevH = prevFrame.height;
  const currH = currFrame.height;
  const templateRows = Math.min(OVERLAP_TEMPLATE_ROWS, prevH, currH);
  const searchRange = Math.min(SAD_SEARCH_RANGE, currH - templateRows);
  if (searchRange <= 0) return 0;

  const prevData = prevFrame.data;
  const currData = currFrame.data;
  const rowBytes = w * 4;

  // 预取模板（上一帧底部 templateRows 行）
  const template = new Uint8ClampedArray(templateRows * rowBytes);
  const templateStart = (prevH - templateRows) * rowBytes;
  for (let i = 0; i < template.length; i++) {
    template[i] = prevData[templateStart + i];
  }

  // 在新帧顶部 searchRange 行内搜索最小 SAD
  let minSAD = Infinity;
  let bestOffset = 0;

  for (let offset = 0; offset <= searchRange; offset++) {
    let sad = 0;
    for (let row = 0; row < templateRows; row++) {
      const tmplRow = row * rowBytes;
      const currRow = (offset + row) * rowBytes;
      for (let col = 0; col < rowBytes; col += 4) {
        sad += Math.abs(template[tmplRow + col] - currData[currRow + col]);
        sad += Math.abs(template[tmplRow + col + 1] - currData[currRow + col + 1]);
        sad += Math.abs(template[tmplRow + col + 2] - currData[currRow + col + 2]);
      }
    }
    // 早停：已比当前最小值大，跳过
    if (sad < minSAD) {
      minSAD = sad;
      bestOffset = offset;
    }
  }

  // 自适应阈值：如果最小 SAD / (templateRows * w * 3) > 某阈值，认为匹配失败
  const avgSAD = minSAD / (templateRows * w * 3);
  if (avgSAD > 30) {
    console.debug('[scroll] SAD match failed, fallback to direct stitch', { avgSAD, bestOffset });
    return 0; // 降级：直接拼接
  }

  console.debug('[scroll] overlap found', { bestOffset, avgSAD });
  return bestOffset;
}

/**
 * 进入长图编辑阶段。
 * 合成所有帧 → 进入标注模式（复用 annot.reset）。
 *
 * 需要调用 `enterAnnotationWithCropData`（index.js 提供）。
 */
export function enterScrollEdit() {
  if (ss.scrollCapturePhase !== 'capturing') return;
  if (ss.scrollFrames.length === 0) return;

  ss.scrollCapturePhase = 'editing';
  const longImage = compositeLongImage();
  if (!longImage) {
    console.warn('[scroll] compositeLongImage returned null');
    return;
  }

  // 切回默认工具栏
  const scrollTb = document.getElementById('scroll-toolbar');
  if (scrollTb) scrollTb.classList.add('hidden');
  if (ss.toolbar) ss.toolbar.classList.remove('hidden');

  // 隐藏预览
  if (ss.scrollPreviewCanvas) ss.scrollPreviewCanvas.classList.add('hidden');

  // 调用 index.js 提供的 enterAnnotationWithCropData 回调
  if (ss._enterAnnotationWithCropData) {
    ss._enterAnnotationWithCropData(longImage, longImage.width, longImage.height);
  }
  console.info('[scroll] enter editing', { w: longImage.width, h: longImage.height });
}

/**
 * 退出长截图模式（取消或输出后调）。
 * 清理状态，隐藏专属工具栏和预览。
 */
export function exitScrollCapture() {
  ss.scrollCapturePhase = 'idle';
  ss.scrollFrames = [];
  ss.autoScroll = false;
  ss.scrollHwnd = null;

  const scrollTb = document.getElementById('scroll-toolbar');
  if (scrollTb) scrollTb.classList.add('hidden');
  if (ss.scrollPreviewCanvas) ss.scrollPreviewCanvas.classList.add('hidden');
}

/**
 * 手动滚动检测：wheel 事件 → 截帧。
 * 由 index.js 在 capturing 阶段绑定时调用。
 *
 * @param {WheelEvent} e
 */
export function onScrollWheel(e) {
  if (ss.scrollCapturePhase !== 'capturing') return;
  // rAF 节流：避免滚动事件密集触发 getImageData
  if (ss._scrollRaf) return;
  ss._scrollRaf = requestAnimationFrame(() => {
    ss._scrollRaf = 0;
    captureFrame();
  });
}

/**
 * 切换滚动方向（纵向/横向）。
 */
export function toggleScrollDirection() {
  ss.scrollDirection = ss.scrollDirection === 'vertical' ? 'horizontal' : 'vertical';
  const btn = document.getElementById('scroll-direction');
  if (btn) {
    btn.title = ss.scrollDirection === 'vertical' ? '滚动方向：纵向（点击切横向）' : '滚动方向：横向（点击切纵向）';
  }
  console.debug('[scroll] direction', ss.scrollDirection);
}

/**
 * 切换自动滚动模式。
 * 开启时调 Rust 端 PostMessage 驱动滚动。
 */
export async function toggleAutoScroll() {
  ss.autoScroll = !ss.autoScroll;
  const btn = document.getElementById('scroll-auto');
  if (btn) {
    btn.classList.toggle('active', ss.autoScroll);
    btn.title = ss.autoScroll ? '自动滚动中（点击停止）' : '自动滚动';
  }

  if (ss.autoScroll) {
    // 启动自动滚动：调 Rust 端
    try {
      await screenshotAutoScroll(ss.scrollHwnd, ss.scrollDirection);
    } catch (e) {
      console.warn('[scroll] auto scroll failed', e);
      ss.autoScroll = false;
      if (btn) {
        btn.classList.remove('active');
        btn.title = '自动滚动';
      }
    }
  } else {
      await screenshotStopScroll();
  }
}

/**
 * 输出长图（pin/保存/复制）。
 * 如果在 editing 阶段，先合成最终图。
 *
 * @param {string} action - 'pin' | 'save' | 'copy'
 */
export async function outputLongImage(action) {
  // 从标注引擎获取编辑后的图（如果在 editing 阶段）
  // 否则直接合成 scrollFrames
  let pngData = null;

  if (ss.scrollCapturePhase === 'editing' && ss._compositeSelection) {
    pngData = await ss._compositeSelection();
  } else {
    const longImage = compositeLongImage();
    if (!longImage) return;
    // 转 PNG
    const tmpCanvas = document.createElement('canvas');
    tmpCanvas.width = longImage.width;
    tmpCanvas.height = longImage.height;
    tmpCanvas.getContext('2d').putImageData(longImage, 0, 0);
    pngData = await new Promise((resolve) => {
      tmpCanvas.toBlob((blob) => {
        const reader = new FileReader();
        reader.onload = () => resolve(new Uint8Array(reader.result));
        reader.readAsArrayBuffer(blob);
      }, 'image/png');
    });
  }

  if (!pngData) return;

  const meta = window.__blinkScreenMeta || { vx: 0, vy: 0 };
  const screenX = ss.scrollBandX;
  const screenY = ss.scrollBandY;

  switch (action) {
    case 'pin':
      await screenshotPin(pngData, screenX, screenY);
      break;
    case 'save':
      await screenshotSave(pngData, null);
      break;
    case 'copy':
      await screenshotCopy(pngData);
      break;
  }
  exitScrollCapture();
}

/**
 * 绑定长截图专属工具栏事件。
 * 在 index.js initDOM 后调用。
 */
export function bindScrollToolbar() {
  document.getElementById('scroll-direction')?.addEventListener('click', toggleScrollDirection);
  document.getElementById('scroll-auto')?.addEventListener('click', () => toggleAutoScroll());
  document.getElementById('scroll-edit')?.addEventListener('click', enterScrollEdit);
  document.getElementById('scroll-pin')?.addEventListener('click', () => outputLongImage('pin'));
  document.getElementById('scroll-save')?.addEventListener('click', () => outputLongImage('save'));
  document.getElementById('scroll-copy')?.addEventListener('click', () => outputLongImage('copy'));
  document.getElementById('scroll-cancel')?.addEventListener('click', () => {
    exitScrollCapture();
    screenshotCancel();
  });
}
