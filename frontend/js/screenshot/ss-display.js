//! 截图 overlay 显示器几何（0.14.6 §4 拆分）。
//!
//! 后端注入 `physicalDisplays`（原始物理矩形），前端用 canvas 实测的 renderScale
//! 实时转换为 CSS 矩形。`monitorDprAtCss` 通过 CSS→screen→physicalDisplay 命中
//! 查询原生 DPI，仅供显示器识别和跨 DPI clamp 使用，不参与坐标变换。

import { ss } from './ss-state.js';
import { screenRectToCss } from './ss-selection-geometry.js';

/**
 * 取注入的 physicalDisplays 列表（原始物理矩形）。
 * 缺失返回空数组。
 */
export function getPhysicalDisplays() {
  const meta = window.__blinkScreenMeta;
  return (meta && Array.isArray(meta.physicalDisplays) && meta.physicalDisplays) || [];
}

/**
 * 取 CSS 坐标的 displays 列表。
 * 从 physicalDisplays 通过 screenRectToCss 实时转换（使用当前 renderScale）。
 * 缺失返回空数组。
 */
export function getDisplays() {
  const meta = window.__blinkScreenMeta;
  if (!meta) return [];
  const physical = getPhysicalDisplays();
  if (physical.length === 0) return [];
  return physical.map((d) => {
    const css = screenRectToCss({ x: d.x, y: d.y, w: d.w, h: d.h }, meta);
    return { x: css.x, y: css.y, w: css.w, h: css.h, dpi: d.dpi, primary: d.primary };
  });
}

/**
 * 给一个 CSS 坐标点，返回它所在屏的 CSS 矩形（含 fallback）。
 * 找不到匹配屏时回退到整个 overlay 视口。
 */
export function findDisplayCssAt(cssX, cssY) {
  const displays = getDisplays();
  for (const r of displays) {
    if (cssX >= r.x && cssX < r.x + r.w && cssY >= r.y && cssY < r.y + r.h) {
      return r;
    }
  }
  return { x: 0, y: 0, w: window.innerWidth, h: window.innerHeight };
}

/** 找一块完全包含 rect 的可见屏 CSS 矩形；找不到返回 null。 */
export function findDisplayContainingRect(rect) {
  const displays = getDisplays();
  for (const r of displays) {
    if (rect.left >= r.x && rect.right <= r.x + r.w
        && rect.top >= r.y && rect.bottom <= r.y + r.h) {
      return r;
    }
  }
  return null;
}

/** 定位工具栏（PixPin 风格）。 */
export function positionToolbar(rect) {
  const { toolbar } = ss;
  toolbar.classList.remove('hidden');
  if (toolbar.dataset.userMoved === 'true' && toolbar.style.left && toolbar.style.top) {
    return;
  }
  toolbar.style.left = '-9999px';
  toolbar.style.top = '-9999px';

  requestAnimationFrame(() => {
    const tw = toolbar.offsetWidth;
    const th = toolbar.offsetHeight;
    const MARGIN = 8;

    let left = rect.x + rect.w - tw;
    if (left < rect.x) left = rect.x;

    let top = rect.y + rect.h + MARGIN;
    if (findDisplayContainingRect({ left, top, right: left + tw, bottom: top + th })) {
      applyToolbarPos(left, top, false);
      return;
    }
    top = rect.y + rect.h - th - MARGIN;
    if (top < rect.y + MARGIN) top = rect.y + MARGIN;
    applyToolbarPos(left, top, true);
  });
}

function applyToolbarPos(left, top, floating) {
  const { toolbar } = ss;
  toolbar.style.left = left + 'px';
  toolbar.style.top = top + 'px';
  toolbar.classList.toggle('toolbar-floating', floating);
  const subP = document.getElementById('sub-panel');
  if (subP && !subP.classList.contains('hidden')) {
    const textMain = document.getElementById('text-main');
    if (textMain) {
      const tmRect = textMain.getBoundingClientRect();
      subP.style.left = tmRect.left + 'px';
      subP.style.top = (tmRect.bottom + 4) + 'px';
    } else {
      subP.style.left = left + 'px';
      subP.style.top = (top + (floating ? 0 : toolbar.offsetHeight) + 4) + 'px';
    }
  }
  console.debug('[screenshot] toolbar 定位', { left, top, tw: toolbar.offsetWidth, th: toolbar.offsetHeight, floating });
}
