//! 截图 overlay 显示器几何（0.14.6 §4 拆分）。
//!
//! 多屏混合 DPI 正确 clamp：后端 `show_screenshot_overlay` 注入 `window.__blinkScreenMeta.displays`，
//! 每屏已用 overlay 窗口实际 DPI 折算成 **overlay CSS 坐标**（单位 CSS 像素）。前端无需再折算，
//! `displayToCss` 退化为恒等--消除旧实现"每屏各自 dpr"与主流路径"窗口级单一 dpr"的坐标系分裂
//! （混合 DPI 下会导致工具栏 clamp / hover 预选区错位）。selCss / window.inner* 同为 CSS 像素，直接对齐。

import { ss } from './ss-state.js';

/**
 * 取注入的 displays 列表（每屏 overlay CSS 几何，由后端折算）。
 * 缺失返回空数组，调用方按"无多屏信息"降级到旧的 innerWidth/innerHeight clamp。
 */
export function getDisplays() {
  const meta = window.__blinkScreenMeta;
  return (meta && Array.isArray(meta.displays) && meta.displays) || [];
}

/**
 * 后端已注入 overlay CSS 坐标，此处恒等返回。
 * 保留函数名以维持调用方签名（`findDisplayCssAt` / `positionToolbar` 等不感知）。
 */
export function displayToCss(d) {
  return { x: d.x, y: d.y, w: d.w, h: d.h };
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

/** 找一块完全包含 rect 的可见屏 CSS 矩形；找不到返回 null。
 *  供工具栏放置判定"能否完整落在可见屏内"（点命中 findDisplayCssAt 只判中心点，不够）。 */
export function findDisplayContainingRect(rect) {
  const displays = getDisplays();
  for (const d of displays) {
    const r = displayToCss(d);
    if (rect.left >= r.x && rect.right <= r.x + r.w
        && rect.top >= r.y && rect.bottom <= r.y + r.h) {
      return r;
    }
  }
  return null;
}

/** 定位工具栏（PixPin 风格）。
 *  若用户已手动拖过工具栏（dataset.userMoved），保留用户位置不重定位。
 *  候选顺序（保持"底部"语义）：
 *  1. 选区下方外部（落可见屏内）-- 旧默认行为
 *  2. 选区下方内部浮入（外部落不到可见屏，如全屏/跨屏选区、下方是空白区）-- 永不出屏
 *  候选2必然在选区内（选区本身在可见区），故无需第三兜底。 */
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

    // 水平：右对齐选区右边缘，clamp 到选区宽度内（选区比工具栏窄则左对齐）
    let left = rect.x + rect.w - tw;
    if (left < rect.x) left = rect.x;

    // 候选1：选区下方外部（完全落在某块可见屏内）
    let top = rect.y + rect.h + MARGIN;
    if (findDisplayContainingRect({ left, top, right: left + tw, bottom: top + th })) {
      applyToolbarPos(left, top, false);
      return;
    }
    // 候选2：选区下方内部浮入（外部放不下；保持"底部"语义一致）
    // 选区本身在可见区，工具栏在选区内即必在可见区，不会出屏
    top = rect.y + rect.h - th - MARGIN;
    if (top < rect.y + MARGIN) top = rect.y + MARGIN; // 选区过矮则贴选区顶部，不越过
    applyToolbarPos(left, top, true);
  });
}

/** 应用工具栏坐标 + 同步二级面板。floating=true 标记浮入选区内（供 CSS 区分视觉）。 */
function applyToolbarPos(left, top, floating) {
  const { toolbar } = ss;
  toolbar.style.left = left + 'px';
  toolbar.style.top = top + 'px';
  toolbar.classList.toggle('toolbar-floating', floating);
  // 0.15.12：工具栏定位后同步 sub-panel 位置（相对 text-main 按钮）
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
