//! 0.18.2 截图控件级智能吸附。
//!
//! 与 `ss-hover.js`（窗口级吸附）同构，但 hit-test 控件矩形优先于窗口矩形。
//! 鼠标同时落在控件和窗口内时，吸附到更小的控件矩形。
//!
//! **坐标转换**：与窗口吸附相同——后端返回物理像素（虚拟屏幕坐标系），
//! 前端 `rectScreenToCss` 转 CSS。复用 `ss-selection-geometry.js`。
//!
//! **时序**：overlay 显示后异步收集 UIA 控件（可配超时/深度/最小尺寸），
//! hints 到达前维持窗口吸附；hints 到达后（且未在拖拽中）启用控件 hit-test。
//!
//! **降级**：UIA 失败/超时/返回空 → pickableControls 为空，hit-test 退化为纯窗口吸附。

import { ss } from './ss-state.js';
import { screenshotControlHints } from '../shared/api.js';
import { listen } from '../shared/tauri.js';
import { EVENTS } from '../shared/event-names.js';
import { clampRectToCss, rectScreenToCss, pointInRect } from './ss-selection-geometry.js';

/** 缓存的控件矩形列表（CSS 坐标） */
let pickableControls = [];

/** 当前悬停的控件索引（-1 = 无） */
let hoveredIndex = -1;

/** #control-hint DOM 元素（控件吸附虚线框，与窗口吸附视觉区分） */
let hintEl = null;

/** hintEl 是否当前可见 */
let hintVisible = false;

/** hideControlHint 的延迟计时器（等 opacity 过渡结束再 visibility:hidden） */
let hintHideTimer = 0;

/** 当前流式订阅的 unlisten 函数（done 或 clear 时调用） */
let unlistenStream = null;

/**
 * 流式加载控件提示。注册 listener 后 invoke 后端开始收集。
 * 后端每完成一层 BFS 就 emit 一批 hints，前端增量追加进 pickableControls。
 * 让浅层控件（窗口直接子元素）几乎立即可吸附，深层控件随后陆续可用。
 *
 * @param {number} requestGen - 调用时的 controlHintsGen，用于防过期
 */
export async function loadControlHints(requestGen) {
  // 先清理上一次可能残留的监听
  if (unlistenStream) {
    unlistenStream();
    unlistenStream = null;
  }
  pickableControls = [];

  const meta = window.__blinkScreenMeta || { vx: 0, vy: 0 };
  const vw = window.innerWidth;
  const vh = window.innerHeight;

  // 注册监听（返回 unlisten promise）
  unlistenStream = await listen(EVENTS.SCREENSHOT_CONTROL_HINTS, (event) => {
    const payload = event.payload;
    if (!payload || payload.generation !== ss.controlHintsGen) return; // 防过期

    if (payload.kind === 'batch' && payload.hints?.length) {
      const batch = normalizeControlHints(payload.hints, meta, vw, vh);
      pickableControls = pickableControls.concat(batch);
      // console.debug('[screenshot] control hints batch', payload.depth, batch.length, pickableControls.length);
    } else if (payload.kind === 'done') {
      console.debug('[screenshot] control hints done', { total: payload.total, truncated: payload.truncated, count: pickableControls.length });
      if (unlistenStream) { unlistenStream(); unlistenStream = null; }
    }
  });

  // 触发后端流式收集
  try {
    await screenshotControlHints(requestGen);
  } catch (e) {
    // invoke 失败：清理监听
    if (unlistenStream) { unlistenStream(); unlistenStream = null; }
    if (requestGen !== ss.controlHintsGen) return;
    console.warn('[screenshot] screenshotControlHints invoke 失败', e);
    pickableControls = [];
  }
}

/** 将物理控件矩形转换并裁剪到当前 overlay；完全不可见的控件不进入 hit-test。 */
export function normalizeControlHints(list, meta, viewportWidth, viewportHeight) {
  return (list || []).map((c) => {
      const screenRect = { x: c.x, y: c.y, w: c.w, h: c.h };
      const cssRect = clampRectToCss(
        rectScreenToCss(screenRect, meta),
        viewportWidth,
        viewportHeight,
      );
      return {
        controlType: c.controlType,
        ...cssRect,
      };
    }).filter((c) => c.w > 0 && c.h > 0);
}

/** 释放控件列表 + 立即隐藏提示框（overlay 关闭时调） */
export function clearControlHints() {
  pickableControls = [];
  hoveredIndex = -1;
  ss.controlHintsGen++;
  if (unlistenStream) {
    unlistenStream();
    unlistenStream = null;
  }
  // 立即清除，不走淡出过渡（overlay 正在关闭）
  if (hintHideTimer) {
    clearTimeout(hintHideTimer);
    hintHideTimer = 0;
  }
  hintVisible = false;
  if (hintEl) {
    hintEl.style.transition = 'none';
    hintEl.style.opacity = '0';
    hintEl.style.visibility = 'hidden';
    hintEl.style.transition = '';
  }
}

/**
 * 在 mousemove 中调用：hit-test 鼠标是否在某控件上。
 * 只在选区拖拽阶段（!isAnnotating）生效。
 *
 * **控件优先**：多个控件包含鼠标点时，选面积最小的（最精确的控件）。
 *
 * @param {number} cssX - 鼠标 CSS X
 * @param {number} cssY - 鼠标 CSS Y
 * @returns {boolean} true = 当前悬停在某控件上（应显示控件虚线框）
 */
export function updateControlHover(cssX, cssY) {
  if (pickableControls.length === 0) return false;

  // 选区已确定时不吸附（标注模式）
  if (ss.isAnnotating) {
    if (hoveredIndex >= 0) {
      hoveredIndex = -1;
      hideControlHint();
    }
    return false;
  }

  // 控件优先于窗口：找面积最小的命中控件（最精确）
  let found = -1;
  let minArea = Infinity;
  for (let i = 0; i < pickableControls.length; i++) {
    const c = pickableControls[i];
    if (pointInRect(cssX, cssY, c)) {
      const area = c.w * c.h;
      if (area < minArea) {
        minArea = area;
        found = i;
      }
    }
  }

  if (found !== hoveredIndex) {
    hoveredIndex = found;
    if (found >= 0) {
      showControlHint(pickableControls[found]);
    } else {
      hideControlHint();
    }
  }
  return found >= 0;
}

/** 获取当前悬停的控件矩形（CSS 坐标）。
 * 单击时调此函数获取吸附目标；返回 null 表示无悬停。 */
export function getHoveredControlRect() {
  if (hoveredIndex < 0) return null;
  const c = pickableControls[hoveredIndex];
  return { x: c.x, y: c.y, w: c.w, h: c.h };
}

/** 只清除 hover 状态（隐藏虚线框），不清除控件列表。
 * 拖动开始或进入标注模式时调用。 */
export function clearControlHover() {
  if (hoveredIndex >= 0) {
    hoveredIndex = -1;
    hideControlHint();
  }
}

/** 显示控件虚线框（仅边框，琥珀色以区分窗口吸附的蓝色）
 *  与 showWindowHint 同款过渡逻辑：首次出现禁用位移过渡仅淡入。 */
function showControlHint(c) {
  if (!hintEl) {
    hintEl = document.createElement('div');
    hintEl.id = 'control-hint';
    hintEl.className = 'control-hint';
    document.body.appendChild(hintEl);
  }

  // 取消可能挂起的隐藏计时器
  if (hintHideTimer) {
    clearTimeout(hintHideTimer);
    hintHideTimer = 0;
  }

  const wasHidden = !hintVisible;

  if (wasHidden) {
    // 首次出现：禁用所有过渡，瞬时定位
    hintEl.style.transition = 'none';
  }

  hintEl.style.left = c.x + 'px';
  hintEl.style.top = c.y + 'px';
  hintEl.style.width = c.w + 'px';
  hintEl.style.height = c.h + 'px';
  hintEl.style.visibility = 'visible';
  hintEl.style.opacity = '1';

  if (wasHidden) {
    // 强制 reflow 提交无过渡的位置，然后恢复 CSS 过渡供后续切换使用
    hintEl.offsetHeight; // reflow
    hintEl.style.transition = '';
    hintVisible = true;
  }
}

/** 隐藏控件虚线框（淡出） */
function hideControlHint() {
  if (hintEl && hintVisible) {
    hintEl.style.opacity = '0';
    hintVisible = false;
    // 等淡出过渡结束后再 visibility:hidden，避免残留可交互区域
    if (hintHideTimer) clearTimeout(hintHideTimer);
    hintHideTimer = setTimeout(() => {
      hintHideTimer = 0;
      if (hintEl && !hintVisible) {
        hintEl.style.visibility = 'hidden';
      }
    }, 120);
  }
}
