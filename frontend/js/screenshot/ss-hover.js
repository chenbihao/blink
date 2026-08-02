//! 0.15.8 选区体验增强：智能窗口吸附。
//!
//! 在选区拖拽阶段（`!ss.isAnnotating`），鼠标悬停在桌面窗口上时显示虚线框，
//! 单击自动吸附选区到该窗口矩形。
//!
//! **坐标转换**：
//! 后端返回的窗口矩形是虚拟屏幕物理像素坐标（含 origin offset），
//! 需要转换为 overlay CSS 坐标：
//!   CSS_x = (physical_x - meta.vx) / dpr
//!   CSS_y = (physical_y - meta.vy) / dpr
//! 其中 dpr = window.devicePixelRatio（overlay 窗口自身的 DPR）。
//!
//! **性能策略**：
//! 后端枚举 ~5-15ms，只在 overlay 加载时调一次。前端 mousemove 做纯 JS
//! point-in-rect hit-test（O(n)，n 通常 <30），<0.1ms。

import { ss } from './ss-state.js';
import { screenshotWindowList } from '../shared/api.js';
import { clampRectToCss, rectScreenToCss, pointInRect } from './ss-selection-geometry.js';

/** 缓存的可吸附窗口列表（CSS 坐标） */
let pickableWindows = [];

/** 当前悬停的窗口索引（-1 = 无） */
let hoveredIndex = -1;

/** #window-hint DOM 元素（只显示虚线边框，无蓝色背景填充） */
let hintEl = null;

/**
 * 加载可吸附窗口列表（overlay 加载时调一次）。
 * 物理坐标 → CSS 坐标转换在此一次完成。
 * 支持会话 generation 防止过期回流。
 */
export async function loadPickableWindows(requestGen, fetchWindows = screenshotWindowList) {
  try {
    const list = await fetchWindows();
    
    // 检查 generation，防止过期回流
    if (requestGen !== ss.windowListGen) {
      console.debug('[screenshot] 窗口列表已过期，丢弃', { requestGen, current: ss.windowListGen });
      return;
    }
    
    const meta = window.__blinkScreenMeta || { vx: 0, vy: 0 };
    const dpr = window.devicePixelRatio || 1;
    
    pickableWindows = normalizePickableWindows(
      list,
      meta,
      dpr,
      window.innerWidth,
      window.innerHeight,
    );

    console.debug('[screenshot] pickable windows loaded', pickableWindows.length);
  } catch (e) {
    // 旧请求的失败与旧请求的成功一样，都不能覆盖新一代列表。
    if (requestGen !== ss.windowListGen) {
      console.debug('[screenshot] 旧窗口列表请求失败，忽略', { requestGen, current: ss.windowListGen });
      return;
    }
    console.warn('[screenshot] loadPickableWindows 失败', e);
    pickableWindows = [];
  }
}

/** 将物理窗口矩形转换并裁剪到当前 overlay；完全不可见的窗口不进入 hit-test。 */
export function normalizePickableWindows(list, meta, dpr, viewportWidth, viewportHeight) {
  return (list || []).map((w) => {
      const screenRect = { x: w.x, y: w.y, w: w.w, h: w.h };
      const cssRect = clampRectToCss(
        rectScreenToCss(screenRect, meta, dpr),
        viewportWidth,
        viewportHeight,
      );
      return {
        hwnd: w.hwnd,
        title: w.title,
        processName: w.process_name,
        ...cssRect
      };
    }).filter((w) => w.w > 0 && w.h > 0);
}

/** 释放窗口列表 + 隐藏提示框（overlay 关闭时调） */
export function clearPickableWindows() {
  pickableWindows = [];
  hoveredIndex = -1;
  ss.windowListGen++;
  hideWindowHint();
}

/**
 * 在 mousemove 中调用：hit-test 鼠标是否在某窗口上。
 * 只在选区拖拽阶段（!isAnnotating）生效。
 *
 * @param {number} cssX - 鼠标 CSS X
 * @param {number} cssY - 鼠标 CSS Y
 * @returns {boolean} true = 当前悬停在某窗口上（应显示虚线框）
 */
export function updateWindowHover(cssX, cssY) {
  if (pickableWindows.length === 0) return false;

  // 选区已确定时不吸附（标注模式）
  if (ss.isAnnotating) {
    if (hoveredIndex >= 0) {
      hoveredIndex = -1;
      hideWindowHint();
    }
    return false;
  }

  // 0.15.8 R1：从索引 0 开始正序遍历——EnumWindows 返回前景到背景，
  // 第一个命中即为最前景窗口。
  let found = -1;
  for (let i = 0; i < pickableWindows.length; i++) {
    const w = pickableWindows[i];
    if (pointInRect(cssX, cssY, w)) {
      found = i;
      break;
    }
  }

  if (found !== hoveredIndex) {
    hoveredIndex = found;
    if (found >= 0) {
      showWindowHint(pickableWindows[found]);
    } else {
      hideWindowHint();
    }
  }
  return found >= 0;
}

/** 获取当前悬停的窗口矩形（CSS 坐标）。
 * 单击时调此函数获取吸附目标；返回 null 表示无悬停。 */
export function getHoveredWindowRect() {
  if (hoveredIndex < 0) return null;
  const w = pickableWindows[hoveredIndex];
  return { x: w.x, y: w.y, w: w.w, h: w.h, hwnd: w.hwnd };
}

/** 0.15.8 R2：只清除 hover 状态（隐藏虚线框），不清除窗口列表。
 * 拖动开始或进入标注模式时调用。 */
export function clearHover() {
  if (hoveredIndex >= 0) {
    hoveredIndex = -1;
    hideWindowHint();
  }
}

/**
 * 根据框选区域选择滚动目标窗口。
 * 优先取与选区交叠面积最大的外部顶层窗口；相同面积时保留枚举顺序靠前者。
 */
export function findWindowForRect(rect) {
  if (!rect || pickableWindows.length === 0) return null;
  let best = null;
  let bestArea = 0;
  let bestTarget = null;
  for (const candidate of pickableWindows) {
    const left = Math.max(rect.x, candidate.x);
    const top = Math.max(rect.y, candidate.y);
    const right = Math.min(rect.x + rect.w, candidate.x + candidate.w);
    const bottom = Math.min(rect.y + rect.h, candidate.y + candidate.h);
    const area = Math.max(0, right - left) * Math.max(0, bottom - top);
    if (area > bestArea) {
      best = candidate;
      bestArea = area;
      bestTarget = { x: (left + right) / 2, y: (top + bottom) / 2 };
    }
  }
  return best ? {
    hwnd: best.hwnd,
    title: best.title,
    processName: best.processName,
    targetX: bestTarget.x,
    targetY: bestTarget.y,
  } : null;
}

/** 显示窗口虚线框（仅边框，无蓝色背景填充） */
function showWindowHint(w) {
  if (!hintEl) {
    hintEl = document.createElement('div');
    hintEl.id = 'window-hint';
    hintEl.className = 'window-hint';
    document.body.appendChild(hintEl);
  }
  hintEl.style.left = w.x + 'px';
  hintEl.style.top = w.y + 'px';
  hintEl.style.width = w.w + 'px';
  hintEl.style.height = w.h + 'px';
  hintEl.style.display = 'block';
  const label = w.processName ? `${w.processName}` : '';
  const title = w.title ? (label ? `${label} — ${w.title}` : w.title) : label;
  hintEl.title = title;
}

/** 隐藏窗口虚线框 */
function hideWindowHint() {
  if (hintEl) {
    hintEl.style.display = 'none';
  }
}
