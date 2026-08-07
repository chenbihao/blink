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
import { drawSelection, drawFinalSelection } from './ss-draw.js';
import { findDisplayCssAt } from './ss-display.js';
import * as annot from './annotation-engine.js';
import {
  formatColor, magnifierSampleRegion,
} from './ss-selection-geometry.js';

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
    ss.endX = e.offsetX;
    ss.endY = e.offsetY;
    drawSelection();
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
    ss.selCss = { x: left, y: top, w: right - left, h: bottom - top };
  }
  drawFinalSelection();
  // sizeHint 由 drawFinalSelection 统一显示（0.18 优化：合并到 drawFinalSelection，
  // 修复智能选区路径不显示 sizeHint 的问题）
}

/** @returns {boolean} true 如果事件被消费（调用方应 return） */
export function finishSelectionInteraction(e) {
  if (!ss.selectionInteraction) return false;
  const { kind, activated } = ss.selectionInteraction;
  if (!activated) {
    ss.selectionInteraction = null;
    ss.canvas.style.cursor = annot.getTool() === 'select' ? 'default' : 'crosshair';
    return true;
  }
  if (kind === 'new') {
    ss.endX = e.offsetX;
    ss.endY = e.offsetY;
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

/**
 * 更新像素放大镜（在 mousemove 中调，rAF 节流）。
 * 0.15.8 R3：mousemove 只记录 pending 坐标，getImageData / 网格绘制 / DOM 定位
 * 全部放进 rAF 回调，一帧最多执行一次。
 * 只在选区拖拽阶段（!isAnnotating）或取色器模式生效。
 */
export function updatePixelMagnifier(cssX, cssY) {
  // 0.15.10：取色器模式下也显示放大镜（复用选区阶段的取色预览逻辑）
  if (!ss.magnifierEl || (ss.isAnnotating && !ss.eyedropperActive)) {
    if (ss.magnifierEl) ss.magnifierEl.classList.add('hidden');
    return;
  }
  // R3：只记录 pending 坐标，由 rAF 回调执行实际工作
  ss._pendingMagnifierPos = { x: cssX, y: cssY };
  if (!ss.magnifierRaf) {
    ss.magnifierRaf = requestAnimationFrame(renderPixelMagnifier);
  }
}

/** 0.15.8 R3：rAF 回调——一帧最多执行一次 getImageData + 绘制 + DOM 更新 */
function renderPixelMagnifier() {
  ss.magnifierRaf = 0;
  if (!ss._pendingMagnifierPos || !ss.magnifierEl) return;
  if (ss.isAnnotating && !ss.eyedropperActive) {
    ss.magnifierEl.classList.add('hidden');
    return;
  }

  const { x: cssX, y: cssY } = ss._pendingMagnifierPos;
  const dpr = window.devicePixelRatio || 1;
  const px = Math.round(cssX * dpr);
  const py = Math.round(cssY * dpr);
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
  const elW = el.offsetWidth || 160;
  const elH = el.offsetHeight || 120;
  let left = cssX + 16;
  let top = cssY + 16;
  if (left + elW > window.innerWidth) left = cssX - elW - 16;
  if (top + elH > window.innerHeight) top = cssY - elH - 16;
  // R3：最终 clamp 到 overlay 视口，不遮住目标像素也不越界
  left = Math.max(0, Math.min(left, window.innerWidth - elW));
  top = Math.max(0, Math.min(top, window.innerHeight - elH));
  el.style.left = left + 'px';
  el.style.top = top + 'px';
  el.classList.remove('hidden');

  // 坐标显示（虚拟屏幕物理坐标）
  const meta = window.__blinkScreenMeta || { vx: 0, vy: 0 };
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

/** 在放大镜画布上绘制像素网格
 *  0.15.8 R3：支持网格偏移，确保边缘采样时中心格对应鼠标像素。
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
  ctx.clearRect(0, 0, PM_COLS * PM_CELL, PM_ROWS * PM_CELL);
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
  // 中心格高亮边框（固定位置，不随偏移移动）
  const halfR = Math.floor(PM_ROWS / 2);
  const halfC = Math.floor(PM_COLS / 2);
  ctx.strokeStyle = '#4a9eff';
  ctx.lineWidth = 2;
  ctx.strokeRect(halfC * PM_CELL, halfR * PM_CELL, PM_CELL, PM_CELL);
}

// 0.15.8 R0：formatColor / rgbToHsl 已统一到 ss-selection-geometry.js，此处不再重复定义

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
  const dpr = window.devicePixelRatio || 1;
  let cssPxDiameter = 0;
  if (tool === 'pencil') {
    cssPxDiameter = w / dpr;
  } else if (tool === 'highlight-multiply' || tool === 'highlight-translucent') {
    cssPxDiameter = (w * 4) / dpr;
  } else if (tool === 'eraser') {
    cssPxDiameter = (Math.max(6, w * 3) * 2) / dpr;
  } else if (tool === 'mosaic' || tool === 'pixelate' || tool === 'blur') {
    // 0.15.8-fix→fix：统一画笔模式工具的光标大小计算。
    // 0.15.11：pixelate/blur 的 widthCat='effect'，getWidthForTool 返回 blurIntensity（统一强度）
    // 而非 brush.size。画笔光标应基于 brush.size。
    const brushSize = annot.getBrushSize();
    if (tool === 'blur') {
      // blur brush 模式的笔画宽度 = brush.size * 2
      cssPxDiameter = (brushSize * 2) / dpr;
    } else {
      // mosaic/pixelate brush 模式的半径 = max(8, brush.size / 2 + 8)，直径 = 半径 * 2
      cssPxDiameter = (Math.max(8, brushSize / 2 + 8) * 2) / dpr;
    }
  } else if (tool === 'number') {
    // 0.15.11：数字标号光标——圆形虚线圈，大小跟随 brushSize
    const brushSize = annot.getBrushSize();
    cssPxDiameter = (Math.max(10, brushSize * 1.2) * 2) / dpr;
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
