//! 截图 overlay 显示器几何（0.14.6 §4 拆分）。
//!
//! 多屏混合 DPI 正确 clamp：后端 show_screenshot_overlay 注入 window.__blinkScreenMeta.displays，
//! 每屏物理几何 + DPI。selCss / window.inner* 是 CSS 像素，混合 DPI 下换算系数不同。
//! 策略：每屏用各自的 dpr 折算成 overlay CSS 坐标系矩形，point-in-rect 找所在屏。

import { ss } from './ss-state.js';

/**
 * 取注入的 displays 列表（每屏物理几何 + DPI）。
 * 缺失返回空数组，调用方按"无多屏信息"降级到旧的 innerWidth/innerHeight clamp。
 */
export function getDisplays() {
  const meta = window.__blinkScreenMeta;
  return (meta && Array.isArray(meta.displays) && meta.displays) || [];
}

/**
 * 把单块屏的物理几何折算成 overlay CSS 坐标系里的矩形。
 * 单屏环境：dpr 与 window.devicePixelRatio 相同，结果就是 (0,0,innerW,innerH)。
 * 混合 DPI：每屏用各自的 dpr（= dpi/96）折算，结果矩形拼接覆盖整个 overlay。
 */
export function displayToCss(d) {
  const dpr = (d && d.dpi ? d.dpi : 96) / 96;
  return {
    x: d.x / dpr,
    y: d.y / dpr,
    w: d.w / dpr,
    h: d.h / dpr,
  };
}

/**
 * 给一个 CSS 坐标点，返回它所在屏的 CSS 矩形（含 fallback）。
 * 找不到匹配屏时回退到整个 overlay 视口，保证函数永不返回 null。
 */
export function findDisplayCssAt(cssX, cssY) {
  const displays = getDisplays();
  for (const d of displays) {
    const r = displayToCss(d);
    if (cssX >= r.x && cssX < r.x + r.w && cssY >= r.y && cssY < r.y + r.h) {
      return r;
    }
  }
  return { x: 0, y: 0, w: window.innerWidth, h: window.innerHeight };
}

/** 定位工具栏到选区右下外侧（PixPin 风格）。
 *  若用户已手动拖过工具栏（dataset.userMoved），保留用户位置不重定位。
 *  按"选区所在屏"clamp——副屏左边缘做选区时工具栏不会被推到另一块屏去。 */
export function positionToolbar(rect) {
  const { toolbar } = ss;
  toolbar.style.display = 'flex';
  if (toolbar.dataset.userMoved === 'true' && toolbar.style.left && toolbar.style.top) {
    return;
  }
  toolbar.style.left = '-9999px';
  toolbar.style.top = '-9999px';

  requestAnimationFrame(() => {
    const tw = toolbar.offsetWidth;
    const th = toolbar.offsetHeight;
    const mon = findDisplayCssAt(rect.x + rect.w / 2, rect.y + rect.h / 2);
    const MARGIN = 8;

    let left = rect.x + rect.w - tw;
    if (left + tw > mon.x + mon.w - MARGIN) left = mon.x + mon.w - tw - MARGIN;
    if (left < mon.x + MARGIN) left = mon.x + MARGIN;

    let top = rect.y + rect.h + MARGIN;
    if (top + th > mon.y + mon.h - MARGIN) {
      top = rect.y - th - MARGIN;
    }
    if (top < mon.y + MARGIN) {
      top = Math.max(mon.y + MARGIN, mon.y + mon.h - th - MARGIN);
    }

    toolbar.style.left = left + 'px';
    toolbar.style.top = top + 'px';
    console.debug('[screenshot] toolbar 定位', { left, top, tw, th, rect, mon });
  });
}
