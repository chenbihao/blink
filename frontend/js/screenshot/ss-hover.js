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

/** 缓存的可吸附窗口列表（CSS 坐标） */
let pickableWindows = [];

/** 当前悬停的窗口索引（-1 = 无） */
let hoveredIndex = -1;

/** #window-hint DOM 元素 */
let hintEl = null;

/**
 * 加载可吸附窗口列表（overlay 加载时调一次）。
 * 物理坐标 → CSS 坐标转换在此一次完成。
 */
export async function loadPickableWindows() {
  try {
    const list = await screenshotWindowList();
    const meta = window.__blinkScreenMeta || { vx: 0, vy: 0 };
    const dpr = window.devicePixelRatio || 1;
    pickableWindows = (list || []).map((w) => ({
      hwnd: w.hwnd,
      title: w.title,
      processName: w.process_name,
      x: (w.x - meta.vx) / dpr,
      y: (w.y - meta.vy) / dpr,
      w: w.w / dpr,
      h: w.h / dpr,
    }));
    console.debug('[screenshot] pickable windows loaded', pickableWindows.length);
  } catch (e) {
    console.warn('[screenshot] loadPickableWindows 失败', e);
    pickableWindows = [];
  }
}

/** 释放窗口列表 + 隐藏提示框（overlay 关闭时调） */
export function clearPickableWindows() {
  pickableWindows = [];
  hoveredIndex = -1;
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

  // 从后往前遍历（Z-order 上层的窗口先命中）
  let found = -1;
  for (let i = pickableWindows.length - 1; i >= 0; i--) {
    const w = pickableWindows[i];
    if (cssX >= w.x && cssX <= w.x + w.w && cssY >= w.y && cssY <= w.y + w.h) {
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

/**
 * 获取当前悬停的窗口矩形（CSS 坐标）。
 * 单击时调此函数获取吸附目标；返回 null 表示无悬停。
 */
export function getHoveredWindowRect() {
  if (hoveredIndex < 0) return null;
  const w = pickableWindows[hoveredIndex];
  return { x: w.x, y: w.y, w: w.w, h: w.h };
}

/** 显示窗口虚线框 */
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
  // 工具提示显示进程名 + 标题
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
