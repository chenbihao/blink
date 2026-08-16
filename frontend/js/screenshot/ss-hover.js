//! 0.15.8 选区体验增强：智能窗口吸附。
//!
//! 在选区拖拽阶段（`!ss.isAnnotating`），鼠标悬停在桌面窗口上时显示虚线框，
//! 单击自动吸附选区到该窗口矩形。
//!
//! **坐标转换**：
//! 后端返回的窗口矩形是虚拟屏幕物理像素坐标（含 origin offset），
//! 前端 `rectScreenToCss` 统一使用 renderScale 换算为 CSS 坐标。
//!
//! **性能策略**：
//! 后端枚举 ~5-15ms，只在 overlay 加载时调一次。前端 mousemove 做纯 JS
//! point-in-rect hit-test（O(n)，n 通常 <30），<0.1ms。

import {ss} from './ss-state.js';
import {screenshotWindowList} from '../shared/api.js';
import {clampRectToCss, pointInRect, rectScreenToCss} from './ss-selection-geometry.js';
import {findDisplayCssAt} from './ss-display.js';
import {hidePreselectionHint, resetPreselectionHint, showPreselectionHint,} from './ss-preselection-hint.js';

/** 缓存的可吸附窗口列表（CSS 坐标） */
let pickableWindows = [];

/** 当前悬停的窗口索引（-1 = 无） */
let hoveredIndex = -1;

/** 当前桌面（全屏）预选区矩形（CSS 坐标）。鼠标在桌面/无窗口区域时激活。 */
let desktopHintRect = null;

/**
 * 加载可吸附窗口列表（overlay 加载时调一次）。
 * 物理坐标 → CSS 坐标转换在此一次完成。
 * 支持会话 generation 防止过期回流。
 */
export async function loadPickableWindows(requestGen, fetchWindows = screenshotWindowList) {
    const _t0 = performance.now();
    console.info('[screenshot] loadPickableWindows start', {gen: requestGen});
    try {
        const list = await fetchWindows();
        const _tFetchEnd = performance.now();

        // 检查 generation，防止过期回流
        if (requestGen !== ss.windowListGen) {
            console.debug('[screenshot] 窗口列表已过期，丢弃', {requestGen, current: ss.windowListGen});
            return;
        }

        const meta = window.__blinkScreenMeta || {vx: 0, vy: 0};

        pickableWindows = normalizePickableWindows(
            list,
            meta,
            window.innerWidth,
            window.innerHeight,
        );

        console.info('[screenshot] loadPickableWindows done', {
            count: pickableWindows.length,
            fetchMs: Math.round(_tFetchEnd - _t0),
            totalMs: Math.round(performance.now() - _t0)
        });
    } catch (e) {
        // 旧请求的失败与旧请求的成功一样，都不能覆盖新一代列表。
        if (requestGen !== ss.windowListGen) {
            console.debug('[screenshot] 旧窗口列表请求失败，忽略', {requestGen, current: ss.windowListGen});
            return;
        }
        console.warn('[screenshot] loadPickableWindows 失败', e);
        pickableWindows = [];
    }
}

/** 将物理窗口矩形转换并裁剪到当前 overlay；完全不可见的窗口不进入 hit-test。 */
export function normalizePickableWindows(list, meta, viewportWidth, viewportHeight) {
    return (list || []).map((w) => {
        const screenRect = {x: w.x, y: w.y, w: w.w, h: w.h};
        const cssRect = clampRectToCss(
            rectScreenToCss(screenRect, meta),
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

/** 释放窗口列表 + 立即隐藏提示框（overlay 关闭时调） */
export function clearPickableWindows() {
    pickableWindows = [];
    hoveredIndex = -1;
    desktopHintRect = null;
    ss.windowListGen++;
    resetPreselectionHint();
}

/**
 * 在 mousemove 中调用：hit-test 鼠标是否在某窗口上。
 * 只在选区拖拽阶段（!isAnnotating）生效。
 *
 * 鼠标在桌面/无窗口区域时，回退为全屏（当前显示器）预选区提示，
 * 单击可吸附整屏。
 *
 * **0.19.14-fix**：`options.skipShowHint=true` 时只更新内部索引（hoveredIndex /
 * desktopHintRect），不立即调用 showWindowHint。调用方在控件 hit-test 完成后
 * 通过 `showWindowHintIfPending()` / `hideWindowHintIfVisible()` 决定是否显示，
 * 避免控件命中时每帧 show→hide 窗口 hint 导致蓝色虚线框闪烁。
 *
 * @param {number} cssX - 鼠标 CSS X
 * @param {number} cssY - 鼠标 CSS Y
 * @param {{ skipShowHint?: boolean }} [options]
 * @returns {boolean} true = 当前悬停在某窗口上（应显示虚线框）
 */
export function updateWindowHover(cssX, cssY, options) {
    const skipShowHint = options?.skipShowHint === true;

    // 选区已确定时不吸附（标注模式）
    if (ss.isAnnotating) {
        if (hoveredIndex >= 0 || desktopHintRect) {
            hoveredIndex = -1;
            desktopHintRect = null;
            if (!skipShowHint) hideWindowHint();
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

    if (found >= 0) {
        // 命中窗口：更新索引，清除桌面预选区
        if (found !== hoveredIndex || desktopHintRect) {
            hoveredIndex = found;
            desktopHintRect = null;
            if (!skipShowHint) showWindowHint(pickableWindows[found]);
        }
        return true;
    }

    // 未命中任何窗口：回退为全屏（当前显示器）预选区
    const displayRect = findDisplayCssAt(cssX, cssY);
    if (!desktopHintRect ||
        desktopHintRect.x !== displayRect.x ||
        desktopHintRect.y !== displayRect.y ||
        desktopHintRect.w !== displayRect.w ||
        desktopHintRect.h !== displayRect.h) {
        hoveredIndex = -1;
        desktopHintRect = {...displayRect};
        if (!skipShowHint) showWindowHint(desktopHintRect);
    }
    return false;
}

/** 获取当前悬停的窗口矩形（CSS 坐标）。
 * 单击时调此函数获取吸附目标；返回 null 表示无悬停。
 * 桌面预选区激活时返回全屏矩形（无 hwnd），单击桌面可 snap 到全屏。
 * 全屏标注被困问题由 index.js 的"点击选区外部 → 退出标注"解决。 */
export function getHoveredWindowRect() {
    if (hoveredIndex >= 0) {
        const w = pickableWindows[hoveredIndex];
        return {x: w.x, y: w.y, w: w.w, h: w.h, hwnd: w.hwnd};
    }
    if (desktopHintRect) {
        return {...desktopHintRect};
    }
    return null;
}

/** 0.15.8 R2：只清除 hover 状态（隐藏虚线框），不清除窗口列表。
 * 拖动开始或进入标注模式时调用。 */
export function clearHover() {
    if (hoveredIndex >= 0 || desktopHintRect) {
        hoveredIndex = -1;
        desktopHintRect = null;
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
            bestTarget = {x: (left + right) / 2, y: (top + bottom) / 2};
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

/** 把统一预选框切换到窗口层级；几何过渡由 ss-preselection-hint 统一管理。 */
function showWindowHint(w) {
    const label = w.processName ? `${w.processName}` : '';
    const title = w.title ? (label ? `${label} — ${w.title}` : w.title) : label;
    showPreselectionHint(w, 'window', title);
}

/** 控件命中时释放窗口层级；若控件已接管统一预选框则不会误隐藏。 */
export function hideWindowHintIfVisible() {
    hideWindowHint();
}

/** 控件未命中时把统一预选框切回待显示的窗口或桌面。 */
export function showWindowHintIfPending() {
    if (hoveredIndex >= 0) {
        showWindowHint(pickableWindows[hoveredIndex]);
    } else if (desktopHintRect) {
        showWindowHint(desktopHintRect);
    }
}

/** 隐藏窗口虚线框（淡出） */
function hideWindowHint() {
    hidePreselectionHint('window');
}
