//! 截图 overlay 选区交互（0.14.6 §4 拆分）。
//!
//! 从 chord-screenshot.js 提取的选区交互函数：
//! - selectionCursor / getSelectionHandle — 命中测试与光标样式
//! - beginSelectionInteraction / updateSelectionInteraction / finishSelectionInteraction — 拖拽交互
//! - updateSelectionCursor / refreshShapePreviewOnShift / updateStrokeCursor — 光标与预览更新
//!
//! 注意：invalidateSelectionContent 留在主文件（协调多模块）。

import { ss, SELECTION_HANDLE_SIZE, MIN_SELECTION_SIZE, TOOL_CAPS } from './ss-state.js';
import { norm, applySquareConstraint } from './ss-utils.js';
import { scheduleDrawSelection, drawFinalSelection, scheduleDrawFinalSelection, cancelDrawFinalSelectionRaf } from './ss-draw.js';
import { findDisplayCssAt, applyFloatingUiScale, applyFloatingUiScaleAt } from './ss-display.js';
import * as annot from './annotation-engine.js';
import {
  formatColor, magnifierSampleRegion, cssPointToBitmap, getRenderScale, screenPointToBitmap,
  moveCrosshair1px, moveRect1px, clampRectToBitmapBounds,
} from './ss-selection-geometry.js';
import { screenshotCursorPosition } from '../shared/api.js';

export function selectionCursor(handle) {
  if (handle === 'n' || handle === 's') return 'ns-resize';
  if (handle === 'e' || handle === 'w') return 'ew-resize';
  if (handle === 'nw' || handle === 'se') return 'nwse-resize';
  if (handle === 'ne' || handle === 'sw') return 'nesw-resize';
  return 'move';
}

/** 命中选区八向缩放热区。坐标和 rect 都是视口 CSS 像素。 */
export function getSelectionHandle(x, y, rect) {
  if (!rect) return null;
  const m = SELECTION_HANDLE_SIZE;
  if (x < rect.x - m || x > rect.x + rect.w + m ||
      y < rect.y - m || y > rect.y + rect.h + m) return null;
  const nearL = Math.abs(x - rect.x) <= m;
  const nearR = Math.abs(x - (rect.x + rect.w)) <= m;
  const nearT = Math.abs(y - rect.y) <= m;
  const nearB = Math.abs(y - (rect.y + rect.h)) <= m;
  if (nearT && nearL) return 'nw';
  if (nearT && nearR) return 'ne';
  if (nearB && nearL) return 'sw';
  if (nearB && nearR) return 'se';
  if (nearT) return 'n';
  if (nearB) return 's';
  if (nearL) return 'w';
  if (nearR) return 'e';
  return null;
}

export function beginSelectionInteraction(kind, e, handle = null) {
  if (!ss.selCss) return;
  const original = { ...ss.selCss };
  ss.selectionInteraction = {
    kind,
    handle,
    activated: false,
    startX: e.offsetX,
    startY: e.offsetY,
    original,
    monitor: findDisplayCssAt(original.x + original.w / 2, original.y + original.h / 2),
  };
  ss.isDragging = false;
  if (kind === 'new') {
    ss.startX = e.offsetX;
    ss.startY = e.offsetY;
    ss.endX = ss.startX;
    ss.endY = ss.startY;
  }
  ss.canvas.style.cursor = kind === 'resize' ? selectionCursor(handle) : (kind === 'move' ? 'move' : 'crosshair');
}

export function updateSelectionInteraction(e) {
  if (!ss.selectionInteraction) return;
  const totalDx = e.offsetX - ss.selectionInteraction.startX;
  const totalDy = e.offsetY - ss.selectionInteraction.startY;
  if (!ss.selectionInteraction.activated) {
    if (Math.hypot(totalDx, totalDy) < 3) return;
    ss.selectionInteraction.activated = true;
    // invalidateSelectionContent 是协调函数，由主文件提供的回调执行
    if (typeof ss._invalidateSelectionContent === 'function') {
      ss._invalidateSelectionContent();
    }
    if (ss.selectionInteraction.kind === 'new') {
      ss.isDragging = true;
      ss.selCss = null;
    }
  }
  if (ss.selectionInteraction.kind === 'new') {
    // 0.20.6：Shift 按下时强制 1:1 正方形约束（新建拖选路径）
    if (e.shiftKey) {
      const sx = ss.selectionInteraction.startX;
      const sy = ss.selectionInteraction.startY;
      const dx = e.offsetX - sx;
      const dy = e.offsetY - sy;
      const side = Math.max(Math.abs(dx), Math.abs(dy));
      ss.endX = sx + (dx >= 0 ? side : -side);
      ss.endY = sy + (dy >= 0 ? side : -side);
    } else {
      ss.endX = e.offsetX;
      ss.endY = e.offsetY;
    }
    // H1 优化：rAF 节流，避免 mousemove 高频全量重绘
    scheduleDrawSelection();
    return;
  }

  const { original, monitor, handle } = ss.selectionInteraction;
  const dx = e.offsetX - ss.selectionInteraction.startX;
  const dy = e.offsetY - ss.selectionInteraction.startY;
  if (ss.selectionInteraction.kind === 'move') {
    const x = Math.max(monitor.x, Math.min(original.x + dx, monitor.x + monitor.w - original.w));
    const y = Math.max(monitor.y, Math.min(original.y + dy, monitor.y + monitor.h - original.h));
    ss.selCss = { x, y, w: original.w, h: original.h };
  } else {
    let left = original.x;
    let top = original.y;
    let right = original.x + original.w;
    let bottom = original.y + original.h;
    if (handle.includes('w')) left = Math.max(monitor.x, Math.min(e.offsetX, right - MIN_SELECTION_SIZE));
    if (handle.includes('e')) right = Math.min(monitor.x + monitor.w, Math.max(e.offsetX, left + MIN_SELECTION_SIZE));
    if (handle.includes('n')) top = Math.max(monitor.y, Math.min(e.offsetY, bottom - MIN_SELECTION_SIZE));
    if (handle.includes('s')) bottom = Math.min(monitor.y + monitor.h, Math.max(e.offsetY, top + MIN_SELECTION_SIZE));
    // 0.20.6：Shift 按下时强制 1:1 等边约束
    if (e.shiftKey) {
      const meta = window.__blinkScreenMeta || { vx: 0, vy: 0 };
      const { scaleX: rsx, scaleY: rsy } = getRenderScale(meta);
      // 把 CSS 坐标转换为 bitmap 坐标来调用 applySquareResize
      const origBmpX = Math.round(original.x * rsx);
      const origBmpY = Math.round(original.y * rsy);
      const origBmpW = Math.round(original.w * rsx);
      const origBmpH = Math.round(original.h * rsy);
      const newBmpW = Math.round((right - left) * rsx);
      const newBmpH = Math.round((bottom - top) * rsy);
      const canvasW = ss.canvas?.width || meta?.w || 0;
      const canvasH = ss.canvas?.height || meta?.h || 0;
      const constrained = applySquareResize(
        { x: origBmpX, y: origBmpY, w: origBmpW, h: origBmpH },
        handle,
        newBmpW,
        newBmpH,
        canvasW,
        canvasH
      );
      left = constrained.x / rsx;
      top = constrained.y / rsy;
      right = (constrained.x + constrained.w) / rsx;
      bottom = (constrained.y + constrained.h) / rsy;
    }
    ss.selCss = { x: left, y: top, w: right - left, h: bottom - top };
  }
  // 性能优化：rAF 节流，避免 move/resize mousemove 高频全量重绘
  scheduleDrawFinalSelection();
  // sizeHint 由 drawFinalSelection 统一显示（0.18 优化：合并到 drawFinalSelection，
  // 修复智能选区路径不显示 sizeHint 的问题）
}

/** @returns {boolean} true 如果事件被消费（调用方应 return） */
export function finishSelectionInteraction(e) {
  if (!ss.selectionInteraction) return false;
  // 取消待执行的 rAF，确保最终绘制是最新的（不被节流帧覆盖）
  cancelDrawFinalSelectionRaf();
  const { kind, activated } = ss.selectionInteraction;
  if (!activated) {
    ss.selectionInteraction = null;
    ss.canvas.style.cursor = annot.getTool() === 'select' ? 'default' : 'crosshair';
    return true;
  }
  if (kind === 'new') {
    ss.endX = e.offsetX;
    ss.endY = e.offsetY;
    // 0.20.6：Shift 按下时强制 1:1 正方形约束（finish 路径同步）
    if (e.shiftKey) {
      const sx = ss.selectionInteraction.startX;
      const sy = ss.selectionInteraction.startY;
      const dx = e.offsetX - sx;
      const dy = e.offsetY - sy;
      const side = Math.max(Math.abs(dx), Math.abs(dy));
      ss.endX = sx + (dx >= 0 ? side : -side);
      ss.endY = sy + (dy >= 0 ? side : -side);
    }
    ss.selCss = norm(ss.startX, ss.startY, ss.endX, ss.endY);
    ss.isDragging = false;
  }
  ss.selectionInteraction = null;
  if (!ss.selCss || ss.selCss.w < MIN_SELECTION_SIZE || ss.selCss.h < MIN_SELECTION_SIZE) {
    // exitAnnotationMode 由主文件提供的回调执行
    if (typeof ss._exitAnnotationMode === 'function') ss._exitAnnotationMode();
    return true;
  }
  // enterAnnotationMode 由主文件提供的回调执行
  if (typeof ss._enterAnnotationMode === 'function') ss._enterAnnotationMode({ ...ss.selCss });
  return true;
}

export function updateSelectionCursor(x, y) {
  // 0.20.6：取色器激活时始终使用十字光标
  if (ss.eyedropperActive) {
    ss.canvas.style.cursor = 'crosshair';
    return;
  }
  if (annot.getTool() !== 'select') {
    ss.canvas.style.cursor = 'crosshair';
    return;
  }
  if (!ss.isAnnotating || !ss.selCss) {
    ss.canvas.style.cursor = 'crosshair';
    return;
  }
  const handle = getSelectionHandle(x, y, ss.selCss);
  if (handle) {
    ss.canvas.style.cursor = selectionCursor(handle);
  } else {
    ss.canvas.style.cursor = 'move';
  }
}

// ── 0.15.8：像素放大镜 ──────────────────────────────────

/** 像素放大镜网格参数：9行×16列，每格 9px（画布 144×81）
 *  对应物理像素 9×16 区域，以鼠标为中心。 */
const PM_ROWS = 9;
const PM_COLS = 16;
const PM_CELL = 9;

// 0.20.5：坐标 IPC 频率控制常量
const CURSOR_IPC_MIN_INTERVAL_MS = 1000 / 30; // 30Hz 上限
let _cursorIpcInFlight = false; // single-flight：同时只允许一个在途 invoke
let _cursorIpcLastTime = 0; // 上次 IPC 发起时间
let _cursorIpcPendingPos = null; // 待发送的位置（被 throttle 跳过的最新位置）

/**
 * 0.20.5：判断当前是否处于高速框选状态。
 * 高速框选 = 正在拖拽新选区（isDragging）且鼠标移动速度较快。
 * 此时暂停放大镜采样以避免 IPC 和 getImageData 压力。
 */
function isHighSpeedSelecting() {
  // isDragging = 正在拖拽新选区；isAnnotDragging = 标注工具拖拽
  // 取色器模式（eyedropperActive）不在此限
  return ss.isDragging && !ss.eyedropperActive;
}

/**
 * 更新像素放大镜（在 mousemove 中调，rAF 节流）。
 * 0.15.8 R3：mousemove 只记录 pending 坐标，getImageData / 网格绘制 / DOM 定位
 * 全部放进 rAF 回调，一帧最多执行一次。
 * 只在选区拖拽阶段（!isAnnotating）或取色器模式生效。
 * 0.20.5：高速框选时暂停放大镜采样；取色工具/低速/停留或显式精调时恢复。
 */
export function updatePixelMagnifier(cssX, cssY) {
  // 0.15.10：取色器模式下也显示放大镜（复用选区阶段的取色预览逻辑）
  if (!ss.magnifierEl || (ss.isAnnotating && !ss.eyedropperActive)) {
    if (ss.magnifierEl) ss.magnifierEl.classList.add('hidden');
    return;
  }
  // 0.20.5：高速框选时暂停放大镜采样
  if (isHighSpeedSelecting()) {
    if (ss.magnifierEl) ss.magnifierEl.classList.add('hidden');
    return;
  }
  // R3：只记录 pending 坐标，由 rAF 回调执行实际工作
  // 每次指针移动立即失效上一个异步 GetCursorPos 结果；否则 IPC 回流顺序变化时
  // 放大镜可能短暂跳回旧像素。
  const generation = (ss._magnifierSampleGen || 0) + 1;
  ss._magnifierSampleGen = generation;
  ss._pendingMagnifierPos = { x: cssX, y: cssY, generation };
  if (!ss.magnifierRaf) {
    ss.magnifierRaf = requestAnimationFrame(renderPixelMagnifier);
  }
}

/** 0.15.8 R3：rAF 回调——一帧最多执行一次 getImageData + 绘制 + DOM 更新
 *  0.20.5：坐标 IPC 实现 single-flight（同时只允许一个在途 invoke）、
 *  30Hz 上限和 generation 门禁。旧 generation 不回流。 */
function renderPixelMagnifier() {
  ss.magnifierRaf = 0;
  if (!ss._pendingMagnifierPos || !ss.magnifierEl) return;
  if (ss.isAnnotating && !ss.eyedropperActive) {
    ss.magnifierEl.classList.add('hidden');
    return;
  }
  // 0.20.5：高速框选时再次检查（可能在 rAF 等待期间进入高速状态）
  if (isHighSpeedSelecting()) {
    ss.magnifierEl.classList.add('hidden');
    return;
  }

  const { x: cssX, y: cssY, generation } = ss._pendingMagnifierPos;
  const meta = window.__blinkScreenMeta || { vx: 0, vy: 0 };

  // 0.20.5：single-flight + 30Hz throttle
  const now = performance.now();
  const elapsed = now - _cursorIpcLastTime;
  const canSend = !_cursorIpcInFlight && elapsed >= CURSOR_IPC_MIN_INTERVAL_MS;

  if (canSend) {
    _cursorIpcInFlight = true;
    _cursorIpcLastTime = now;
    // 选区预览和正式吸管统一使用 Win32 GetCursorPos 的虚拟屏幕物理坐标。
    // CSS 指针坐标在 200% 缩放下通常每次只变化 1 DIP，乘 renderScale 后会
    // 形成 0、2、4… 的 bitmap 步进；物理坐标则能覆盖每一个截图像素。
    screenshotCursorPosition().then((pos) => {
      _cursorIpcInFlight = false;
      if (!canRenderMagnifierSample(generation)) return;
      const bmpPt = screenPointToBitmap(pos.x, pos.y, meta);
      renderMagnifierFromBitmap(bmpPt.x, bmpPt.y, cssX, cssY, meta);
    }).catch(() => {
      _cursorIpcInFlight = false;
      // 平台命令异常时保留旧路径作为降级，至少不让放大镜完全不可用。
      if (!canRenderMagnifierSample(generation)) return;
      const bmpPt = cssPointToBitmap(cssX, cssY, meta);
      renderMagnifierFromBitmap(bmpPt.x, bmpPt.y, cssX, cssY, meta);
    });
  } else if (_cursorIpcInFlight) {
    // IPC 在途：用 CSS 坐标降级采样（不走 IPC，不阻塞放大镜显示）
    if (!canRenderMagnifierSample(generation)) return;
    const bmpPt = cssPointToBitmap(cssX, cssY, meta);
    renderMagnifierFromBitmap(bmpPt.x, bmpPt.y, cssX, cssY, meta);
  } else {
    // 被 throttle 跳过：安排下一轮发送
    _cursorIpcPendingPos = { cssX, cssY, generation, meta };
    setTimeout(() => {
      if (_cursorIpcInFlight) return;
      if (!canRenderMagnifierSample(generation)) return;
      const p = _cursorIpcPendingPos;
      _cursorIpcPendingPos = null;
      if (!p) return;
      _cursorIpcInFlight = true;
      _cursorIpcLastTime = performance.now();
      screenshotCursorPosition().then((pos) => {
        _cursorIpcInFlight = false;
        if (!canRenderMagnifierSample(p.generation)) return;
        const bmpPt = screenPointToBitmap(pos.x, pos.y, p.meta);
        renderMagnifierFromBitmap(bmpPt.x, bmpPt.y, p.cssX, p.cssY, p.meta);
      }).catch(() => {
        _cursorIpcInFlight = false;
        if (!canRenderMagnifierSample(p.generation)) return;
        const bmpPt = cssPointToBitmap(p.cssX, p.cssY, p.meta);
        renderMagnifierFromBitmap(bmpPt.x, bmpPt.y, p.cssX, p.cssY, p.meta);
      });
    }, CURSOR_IPC_MIN_INTERVAL_MS - elapsed);
  }
}

function canRenderMagnifierSample(generation) {
  return generation === ss._magnifierSampleGen
    && !!ss.magnifierEl
    && (!ss.isAnnotating || ss.eyedropperActive);
}

/**
 * 从 bitmap 坐标采样像素并渲染放大镜网格 + 定位元素。
 */
function renderMagnifierFromBitmap(px, py, cssX, cssY, meta) {
  // 0.20.6：同步取色器精调基线位置（方向键从此点开始 1px 移动）
  if (ss.eyedropperActive) {
    ss._pickerBitmapPos = { x: px, y: py };
  }
  const halfR = Math.floor(PM_ROWS / 2);
  const halfC = Math.floor(PM_COLS / 2);

  // 从无蒙版的离屏 canvas 取原始像素
  if (!ss.screenshotOffscreen) return;
  const offCtx = ss.screenshotOffscreen.getContext('2d');
  const canvasW = ss.screenshotOffscreen.width;
  const canvasH = ss.screenshotOffscreen.height;

  // R3：边缘采样——计算有界读取区域和网格偏移，
  // 确保中心格始终对应鼠标下的物理像素。
  const sample = magnifierSampleRegion(px, py, canvasW, canvasH, PM_COLS, PM_ROWS);
  const readX = sample.readX;
  const readY = sample.readY;
  const gridOffX = sample.gridOffsetX;
  const gridOffY = sample.gridOffsetY;
  const colsToRead = sample.width;
  const rowsToRead = sample.height;

  let imgData = null;
  try {
    imgData = offCtx.getImageData(readX, readY, colsToRead, rowsToRead);
  } catch (e) {
    return;
  }
  if (!imgData) return;

  drawMagnifierGrid(imgData, gridOffX, gridOffY, colsToRead, rowsToRead);

  // 定位放大镜：鼠标右下方，偏移 16px；空间不足时向左/上翻转
  const el = ss.magnifierEl;
  // 按鼠标所在位置独立计算 UI scale，不复用工具栏的 dataset.uiScale
  const uiScale = applyFloatingUiScaleAt(el, cssX, cssY);
  const elW = (el.offsetWidth || 160) * uiScale;
  const elH = (el.offsetHeight || 120) * uiScale;
  // 使用鼠标所在显示器矩形作为 clamp 基准，不使用全局 viewport
  const mon = findDisplayCssAt(cssX, cssY);
  let left = cssX + 16;
  let top = cssY + 16;
  if (left + elW > mon.x + mon.w) left = cssX - elW - 16;
  if (top + elH > mon.y + mon.h) top = cssY - elH - 16;
  // 最终 clamp 到来源显示器视口，不遮住目标像素也不越界
  left = Math.max(mon.x, Math.min(left, mon.x + mon.w - elW));
  top = Math.max(mon.y, Math.min(top, mon.y + mon.h - elH));
  el.style.left = left + 'px';
  el.style.top = top + 'px';
  el.classList.remove('hidden');

  // 坐标显示（虚拟屏幕物理坐标）
  if (ss.magnifierCoord) {
    ss.magnifierCoord.textContent = `${meta.vx + px}, ${meta.vy + py}`;
  }

  // 中心像素色值——中心格在 imgData 中的局部坐标
  if (ss.magnifierColor) {
    const localCX = halfC - gridOffX;
    const localCY = halfR - gridOffY;
    if (localCX >= 0 && localCX < colsToRead && localCY >= 0 && localCY < rowsToRead) {
      const midIdx = (localCY * colsToRead + localCX) * 4;
      const d = imgData.data;
      const r = d[midIdx], g = d[midIdx + 1], b = d[midIdx + 2];
      ss.magnifierColor.textContent = formatColor(r, g, b, ss.magnifierFormat);
      if (ss.magnifierColorSwatch) {
        ss.magnifierColorSwatch.style.background = `rgb(${r},${g},${b})`;
      }
    }
  }
}

/** 隐藏像素放大镜 */
export function hidePixelMagnifier() {
  if (ss.magnifierRaf) { cancelAnimationFrame(ss.magnifierRaf); ss.magnifierRaf = 0; }
  // 递增代际，使正在途中的 GetCursorPos 异步结果失效
  ss._magnifierSampleGen = (ss._magnifierSampleGen || 0) + 1;
  ss._pendingMagnifierPos = null;
  if (ss.magnifierEl) ss.magnifierEl.classList.add('hidden');
}

/** 切换色值格式（Shift 键） */
export function cycleMagnifierFormat() {
  ss.magnifierFormat = (ss.magnifierFormat + 1) % 3;
}

/** 获取当前色值文本（C 键复制用） */
export function getMagnifierColorText() {
  if (!ss.magnifierColor) return null;
  const text = ss.magnifierColor.textContent;
  return text || null;
}

// M9 优化：复用的小 canvas + ImageData（drawMagnifierGrid 用 putImageData+drawImage 替代逐格 fillRect）
let _magTempCanvas = null;
let _magImageData = null;

/** 在放大镜画布上绘制像素网格
 *  0.15.8 R3：支持网格偏移，确保边缘采样时中心格对应鼠标像素。
 *  M9 优化：用 putImageData + drawImage(nearest-neighbor) 替代逐格 fillStyle + fillRect，
 *  将 144 次 canvas 绘制调用降为 2 次（putImageData + drawImage）。
 * @param {ImageData} imgData - 实际读取的像素数据（可能小于 PM_COLS×PM_ROWS）
 * @param {number} gridOffX - 网格 X 偏移（网格中跳过的列数）
 * @param {number} gridOffY - 网格 Y 偏移
 * @param {number} dataCols - imgData 的实际列数
 * @param {number} dataRows - imgData 的实际行数
 */
function drawMagnifierGrid(imgData, gridOffX, gridOffY, dataCols, dataRows) {
  const ctx = ss.magnifierCtx;
  if (!ctx) return;
  const { data } = imgData;
  const totalW = PM_COLS;
  const totalH = PM_ROWS;

  // M9 优化：优先用 putImageData + drawImage 缩放
  if (!_magTempCanvas) {
    _magTempCanvas = document.createElement('canvas');
    _magTempCanvas.width = totalW;
    _magTempCanvas.height = totalH;
    _magImageData = _magTempCanvas.getContext('2d')?.createImageData(totalW, totalH) ?? null;
  }
  const tempCtx = _magTempCanvas.getContext('2d');

  if (tempCtx && _magImageData) {
    // 把采样像素填入 gridImageData（直接数组写入，无 canvas 状态开销）
    const gridData = _magImageData.data;
    // 清空（上次调用可能残留）
    gridData.fill(0);
    for (let row = 0; row < dataRows; row++) {
      for (let col = 0; col < dataCols; col++) {
        const srcIdx = (row * dataCols + col) * 4;
        const dstCol = gridOffX + col;
        const dstRow = gridOffY + row;
        if (dstCol < 0 || dstCol >= totalW || dstRow < 0 || dstRow >= totalH) continue;
        const dstIdx = (dstRow * totalW + dstCol) * 4;
        gridData[dstIdx] = data[srcIdx];
        gridData[dstIdx + 1] = data[srcIdx + 1];
        gridData[dstIdx + 2] = data[srcIdx + 2];
        gridData[dstIdx + 3] = 255;
      }
    }
    tempCtx.putImageData(_magImageData, 0, 0);
    ctx.clearRect(0, 0, totalW * PM_CELL, totalH * PM_CELL);
    ctx.imageSmoothingEnabled = false;
    ctx.drawImage(_magTempCanvas, 0, 0, totalW, totalH,
      0, 0, totalW * PM_CELL, totalH * PM_CELL);
    ctx.imageSmoothingEnabled = true;
  } else {
    // fallback：原始逐格绘制
    ctx.clearRect(0, 0, totalW * PM_CELL, totalH * PM_CELL);
    for (let row = 0; row < dataRows; row++) {
      for (let col = 0; col < dataCols; col++) {
        const idx = (row * dataCols + col) * 4;
        const r = data[idx];
        const g = data[idx + 1];
        const b = data[idx + 2];
        ctx.fillStyle = `rgb(${r},${g},${b})`;
        ctx.fillRect((gridOffX + col) * PM_CELL, (gridOffY + row) * PM_CELL, PM_CELL, PM_CELL);
      }
    }
  }

  // 中心格高亮边框（固定位置，不随偏移移动）
  const halfR = Math.floor(PM_ROWS / 2);
  const halfC = Math.floor(PM_COLS / 2);
  ctx.strokeStyle = '#4a9eff';
  ctx.lineWidth = 2;
  ctx.strokeRect(halfC * PM_CELL, halfR * PM_CELL, PM_CELL, PM_CELL);
}

// 0.15.8 R0：formatColor / rgbToHsl 已统一到 ss-selection-geometry.js，此处不再重复定义

// ── 0.20.6：取色器方向键 1px 精调 ──────────────────────────────────
//
// 取色器始终处于 following 模式（鼠标跟随实时采样）。
// 方向键辅助鼠标移动 1 个物理像素，用于像素级精确定位。
// 不再使用滚轮冻结方案——精调时直接移动鼠标采样位置并刷新放大镜。

/**
 * 进入取色器跟随模式。由 ss-color-picker.js 的 enterPickMode 调用。
 */
export function enterColorPickerFollowing() {
  ss.colorPickerMode = 'following';
  ss._pickerBitmapPos = null;
  updatePrecisionHint();
}

/**
 * 取色器完全退出（idle）。由 cleanup 路径调用。
 */
export function resetColorPickerState() {
  ss.colorPickerMode = 'idle';
  ss._pickerBitmapPos = null;
  updatePrecisionHint();
}

/**
 * 取色器方向键移动 1 个物理像素。
 * 调用后立即从新位置采样并刷新放大镜显示。
 * @param {number} dx -1/0/+1
 * @param {number} dy -1/0/+1
 */
export function movePickerPixel(dx, dy) {
  if (ss.colorPickerMode !== 'following' || !ss.eyedropperActive) return;
  const meta = window.__blinkScreenMeta || { vx: 0, vy: 0 };
  // 初始化或更新采样位置
  if (!ss._pickerBitmapPos) {
    // 首次按键：用当前鼠标位置初始化
    // _pickerBitmapPos 在 mousemove 中由 updatePixelMagnifier 同步更新
    return; // 首次按键没有基线位置时跳过
  }
  const newPos = moveCrosshair1px(
    ss._pickerBitmapPos, dx, dy,
    { x: 0, y: 0, w: ss.screenshotOffscreen?.width || 0, h: ss.screenshotOffscreen?.height || 0 }
  );
  ss._pickerBitmapPos = newPos;
  // 立即从新位置采样并渲染放大镜
  const { scaleX: rsx, scaleY: rsy } = getRenderScale(meta);
  const cssX = newPos.x / rsx;
  const cssY = newPos.y / rsy;
  renderMagnifierFromBitmap(newPos.x, newPos.y, cssX, cssY, meta);
  updatePrecisionHint();
}

/**
 * 判断当前是否处于取色器激活状态。
 */
export function isColorPickerActive() {
  return ss.colorPickerMode === 'following';
}

/**
 * 更新精调状态提示 DOM。
 */
function updatePrecisionHint() {
  if (!ss.precisionHint) return;
  if (ss.colorPickerMode === 'following') {
    ss.precisionHint.textContent = '方向键移动 1px · C 复制颜色 · Esc 取消';
    ss.precisionHint.classList.remove('hidden');
  } else {
    ss.precisionHint.classList.add('hidden');
  }
}

/**
 * 0.20.6：选区方向键移动 1 bitmap px。
 * 在 bitmap 空间操作，返回 CSS 空间的新矩形供调用方更新 selCss。
 * @param {{x,y,w,h}} selCss - 当前 CSS 选区
 * @param {number} dx -1/0/+1
 * @param {number} dy -1/0/+1
 * @param {object} meta - window.__blinkScreenMeta
 * @returns {{x,y,w,h} | null} 新的 CSS 选区矩形，或 null 表示不可移动
 */
export function moveSelection1px(selCss, dx, dy, meta) {
  if (!selCss) return null;
  const bmp = cssPointToBitmap(selCss.x, selCss.y, meta);
  const bmpW = Math.round(selCss.w * (meta?.renderScaleX || 1));
  const bmpH = Math.round(selCss.h * (meta?.renderScaleY || 1));
  const bmpRect = { x: bmp.x, y: bmp.y, w: bmpW, h: bmpH };
  const canvasW = ss.canvas?.width || meta?.w || 0;
  const canvasH = ss.canvas?.height || meta?.h || 0;
  const newBmpRect = moveRect1px(bmpRect, dx, dy, canvasW, canvasH);
  // 转回 CSS
  const rsx = meta?.renderScaleX || 1;
  const rsy = meta?.renderScaleY || 1;
  return {
    x: newBmpRect.x / rsx,
    y: newBmpRect.y / rsy,
    w: newBmpRect.w / rsx,
    h: newBmpRect.h / rsy,
  };
}

// moveRect1px 已在顶部导入

/** 0.11.8-e：矩形/椭圆拖动期间按/松 Shift 实时更新预览 */
export function refreshShapePreviewOnShift(e) {
  if (!ss.isAnnotDragging || !ss.selCss) return;
  const tool = annot.getTool();
  if (tool !== 'rect' && tool !== 'ellipse') return;
  const constrained = applySquareConstraint(
    ss.annotStartX, ss.annotStartY, ss.annotCurrentX, ss.annotCurrentY, tool
  );
  if (constrained) {
    ss.annotCurrentX = constrained.x;
    ss.annotCurrentY = constrained.y;
    annot.moveDraw(ss.annotCurrentX, ss.annotCurrentY);
    // redrawAnnotPreview 由主文件提供的回调执行
    if (typeof ss._redrawAnnotPreview === 'function') ss._redrawAnnotPreview();
  }
}

export function updateStrokeCursor(clientX, clientY) {
  const { strokeCursor } = ss;
  if (!strokeCursor) return;
  // W4 例外：strokeCursor 是高频逐帧更新的画笔预览光标，
  // 每次鼠标移动都需重设 display + width/height/left/top/borderColor。
  // 用 class 切换会引入额外 reflow，且此处不存在「读 style.display 推断状态」的需求。
  if (!ss.isAnnotating || !ss.selCss) { strokeCursor.style.display = 'none'; return; }
  // Bug-fix: 拖拽时仍显示笔触预览，让用户看到作用区域；只在离开选区时隐藏
  if (clientX < ss.selCss.x || clientX > ss.selCss.x + ss.selCss.w ||
      clientY < ss.selCss.y || clientY > ss.selCss.y + ss.selCss.h) {
    strokeCursor.style.display = 'none';
    return;
  }
  const tool = annot.getTool();
  const caps = TOOL_CAPS[tool];
  if (!caps) {
    strokeCursor.style.display = 'none';
    return;
  }
  // 0.15.8-fix：支持模式切换的工具，只在画笔模式下显示光标
  // 不支持模式切换的工具，按 hasCursor 判断
  let effectiveHasCursor;
  if (caps.supportMode) {
    effectiveHasCursor = annot.getToolMode(tool) === 'brush';
  } else {
    effectiveHasCursor = caps.hasCursor;
  }
  if (!effectiveHasCursor) {
    strokeCursor.style.display = 'none';
    return;
  }
  const w = annot.getWidthForTool(tool);
  // 画笔光标大小映射到 CSS 显示，bitmap→CSS = renderScale
  const _meta = window.__blinkScreenMeta || { vx: 0, vy: 0 };
  const { scaleX: rsx } = getRenderScale(_meta);
  let cssPxDiameter = 0;
  if (tool === 'pencil') {
    cssPxDiameter = w / rsx;
  } else if (tool === 'highlight-multiply' || tool === 'highlight-translucent') {
    cssPxDiameter = (w * 4) / rsx;
  } else if (tool === 'eraser') {
    cssPxDiameter = (Math.max(6, w * 3) * 2) / rsx;
  } else if (tool === 'mosaic' || tool === 'pixelate' || tool === 'blur') {
    // 0.15.8-fix→fix：统一画笔模式工具的光标大小计算。
    // 0.15.11：pixelate/blur 的 widthCat='effect'，getWidthForTool 返回 blurIntensity（统一强度）
    // 而非 brush.size。画笔光标应基于 brush.size。
    const brushSize = annot.getBrushSize();
    if (tool === 'blur') {
      // blur brush 模式的笔画宽度 = brush.size * 2
      cssPxDiameter = (brushSize * 2) / rsx;
    } else {
      // mosaic/pixelate brush 模式的半径 = max(8, brush.size / 2 + 8)，直径 = 半径 * 2
      cssPxDiameter = (Math.max(8, brushSize / 2 + 8) * 2) / rsx;
    }
  } else if (tool === 'number') {
    // 0.15.11：数字标号光标——圆形虚线圈，大小跟随 brushSize
    const brushSize = annot.getBrushSize();
    cssPxDiameter = (Math.max(10, brushSize * 1.2) * 2) / rsx;
  } else {
    strokeCursor.style.display = 'none';
    return;
  }
  if (cssPxDiameter < 4) { strokeCursor.style.display = 'none'; return; }
  strokeCursor.style.display = 'block';
  strokeCursor.style.width = cssPxDiameter + 'px';
  strokeCursor.style.height = cssPxDiameter + 'px';
  strokeCursor.style.left = (clientX - cssPxDiameter / 2) + 'px';
  strokeCursor.style.top = (clientY - cssPxDiameter / 2) + 'px';
  // 0.15.13：所有笔触预览统一使用当前选择的颜色（包括橡皮擦）
  strokeCursor.style.borderColor = annot.getColor();
}
