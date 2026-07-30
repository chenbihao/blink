//! 截图 overlay 选区交互（0.14.6 §4 拆分）。
//!
//! 从 chord-screenshot.js 提取的选区交互函数：
//! - selectionCursor / getSelectionHandle — 命中测试与光标样式
//! - beginSelectionInteraction / updateSelectionInteraction / finishSelectionInteraction — 拖拽交互
//! - updateSelectionCursor / refreshShapePreviewOnShift / updateStrokeCursor — 光标与预览更新
//!
//! 注意：invalidateSelectionContent 留在主文件（协调多模块）。

import { ss, SELECTION_HANDLE_SIZE, MIN_SELECTION_SIZE } from './ss-state.js';
import { norm, applySquareConstraint } from './ss-utils.js';
import { drawSelection, drawFinalSelection } from './ss-draw.js';
import { findDisplayCssAt } from './ss-display.js';
import * as annot from './annotation-engine.js';

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
  const dpr = window.devicePixelRatio || 1;
  ss.sizeHint.textContent = `${Math.round(ss.selCss.w * dpr)} × ${Math.round(ss.selCss.h * dpr)}`;
  ss.sizeHint.style.display = 'block';
  ss.sizeHint.style.left = (ss.selCss.x + 4) + 'px';
  ss.sizeHint.style.top = (ss.selCss.y > 24 ? ss.selCss.y - 22 : ss.selCss.y + 4) + 'px';
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
  if (!ss.isAnnotating || !ss.selCss) { strokeCursor.style.display = 'none'; return; }
  if (ss.isAnnotDragging) { strokeCursor.style.display = 'none'; return; }
  if (clientX < ss.selCss.x || clientX > ss.selCss.x + ss.selCss.w ||
      clientY < ss.selCss.y || clientY > ss.selCss.y + ss.selCss.h) {
    strokeCursor.style.display = 'none';
    return;
  }
  const tool = annot.getTool();
  const w = annot.getWidth();
  const dpr = window.devicePixelRatio || 1;
  let cssPxDiameter = 0;
  if (tool === 'pencil') {
    cssPxDiameter = w / dpr;
  } else if (tool === 'highlight-multiply' || tool === 'highlight-translucent') {
    cssPxDiameter = (w * 4) / dpr;
  } else if (tool === 'eraser') {
    cssPxDiameter = (Math.max(6, w * 3) * 2) / dpr;
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
  if (tool === 'eraser') {
    strokeCursor.style.borderColor = 'rgba(255,255,255,0.9)';
  } else {
    strokeCursor.style.borderColor = annot.getColor();
  }
}
