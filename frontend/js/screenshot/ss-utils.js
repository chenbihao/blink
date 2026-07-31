//! 截图 overlay 工具函数（0.14.6 §4 拆分）。
//!
//! 从 chord-screenshot.js 提取的纯函数，无状态依赖。

/** 标准化矩形坐标：返回 {x, y, w, h}，w/h 非负 */
export function norm(x1, y1, x2, y2) {
  return {
    x: Math.min(x1, x2), y: Math.min(y1, y2),
    w: Math.abs(x2 - x1), h: Math.abs(y2 - y1),
  };
}

/** 点是否在矩形内 */
export function pointInRect(px, py, rect) {
  return px >= rect.x && px <= rect.x + rect.w && py >= rect.y && py <= rect.y + rect.h;
}

/**
 * 矩形/椭圆按住 Shift 约束长宽等比（0.11.8-e）：
 * 从起点 (sx,sy) 到当前 (ex,ey)，取 max(|dx|,|dy|) 作等边，符号保持原方向。
 * 只对 rect/ellipse 生效——箭头/铅笔等自由笔画不约束。
 * 返回修正后的 {x, y}，或 null 表示不需要约束。
 *
 * @param {string} tool — 当前标注工具（由调用方传入 annot.getTool()）
 */
export function applySquareConstraint(sx, sy, ex, ey, tool) {
  if (tool !== 'rect' && tool !== 'ellipse') return null;
  const dx = ex - sx;
  const dy = ey - sy;
  const side = Math.max(Math.abs(dx), Math.abs(dy));
  return {
    x: sx + (dx >= 0 ? side : -side),
    y: sy + (dy >= 0 ? side : -side),
  };
}
