//! 0.18.2 截图控件级智能吸附（跨屏预选版）。
//!
//! 与 `ss-hover.js`（窗口级吸附）同构，但 hit-test 控件矩形优先于窗口矩形。
//! 鼠标同时落在控件和窗口内时，吸附到更小的控件矩形。
//!
//! **跨屏预选**：所有显示器上的顶层窗口都能预选。鼠标悬停到任意屏幕的窗口后，
//! 异步加载该窗口的 UIA 控件。同一窗口本次截图会话内只采集一次，后续使用缓存。
//!
//! **坐标转换**：与窗口吸附相同——后端返回物理像素（虚拟屏幕坐标系），
//! 前端 `rectScreenToCss` 统一使用 renderScale 转 CSS。复用 `ss-selection-geometry.js`。
//!
//! **缓存策略**：缓存中保存后端返回的物理坐标，不把 CSS 坐标作为唯一真值。
//! 显示或 hit-test 时再使用当前 renderScale 转换，避免窗口 DPI/viewport 变化后缓存失效。
//!
//! **防串流**：事件 payload 同时携带 `hwnd` 和 `generation`，前端必须同时校验两者。
//! 旧窗口的异步结果不会污染当前窗口。
//!
//! **降级**：UIA 失败/超时/返回空 → pickableControls 为空，hit-test 退化为纯窗口吸附。

import {ss} from './ss-state.js';
import {screenshotControlHints} from '../shared/api.js';
import {listen} from '../shared/tauri.js';
import {EVENTS} from '../shared/event-names.js';
import {clampRectToCss, pointInRect, rectScreenToCss} from './ss-selection-geometry.js';
import {hidePreselectionHint, resetPreselectionHint, showPreselectionHint,} from './ss-preselection-hint.js';

// ── 会话级状态 ──────────────────────────────────────────────────────────────

/**
 * 会话级控件缓存：hwnd -> { status, physicalHints, generation }
 * - status: 'loading' | 'done' | 'failed'
 * - physicalHints: 后端返回的物理坐标 hints（原始数据，不转 CSS）
 * - generation: 该请求的 generation（用于事件校验）
 */
const controlCache = new Map();

/** 当前活跃（悬停中）的窗口 HWND */
let activeHwnd = null;

/** 当前活跃请求的 generation（用于事件显示校验） */
let activeGeneration = 0;

/** 防抖计时器 */
let hoverDebounceTimer = 0;

/** 会话级监听器 unlisten 函数（所有窗口请求共用此监听器） */
let unlistenStream = null;

// ── 显示状态 ──────────────────────────────────────────────────────────────

/** 当前活跃窗口的 CSS 坐标控件列表（由 recomputePickableControls 设置） */
let pickableControls = [];

/** 当前悬停的控件索引（-1 = 无） */
let hoveredIndex = -1;

// ── 内部工具函数 ──────────────────────────────────────────────────────────

/**
 * 从缓存中的物理坐标 hints 重新计算 CSS 坐标 pickableControls。
 * 每次 setControlTarget 切换窗口、或批次到达时调用。
 */
function recomputePickableControls() {
    if (!activeHwnd) {
        pickableControls = [];
        return;
    }
    const entry = controlCache.get(activeHwnd);
    if (!entry || entry.physicalHints.length === 0) {
        pickableControls = [];
        return;
    }
    const meta = (typeof window !== 'undefined' && window.__blinkScreenMeta) || {vx: 0, vy: 0};
    const vw = (typeof window !== 'undefined' && window.innerWidth) || 0;
    const vh = (typeof window !== 'undefined' && window.innerHeight) || 0;
    pickableControls = normalizeControlHints(entry.physicalHints, meta, vw, vh);
}

/**
 * 会话级事件处理函数——所有窗口的请求共用此监听器。
 * 通过 hwnd + generation 双重校验分发到对应缓存。
 */
function onControlHintsEvent(event) {
    const payload = event.payload;
    if (!payload) return;

    // 校验 1：payload.hwnd 必须有对应缓存条目
    const entry = controlCache.get(payload.hwnd);
    if (!entry) return;

    // 校验 2：generation 必须与缓存条目的 generation 一致（防旧请求串流）
    if (entry.generation !== payload.generation) return;

    if (payload.kind === 'batch' && payload.hints?.length) {
        // 追加物理坐标 hints 到缓存
        for (const h of payload.hints) {
            entry.physicalHints.push(h);
        }
        // 只在当前活跃窗口且 generation 匹配时更新显示
        if (payload.hwnd === activeHwnd && payload.generation === activeGeneration) {
            recomputePickableControls();
        }
    } else if (payload.kind === 'done') {
        entry.status = 'done';
        if (payload.hwnd === activeHwnd && payload.generation === activeGeneration) {
            recomputePickableControls();
        }
    }
}

/**
 * 确保会话级监听器已注册。所有窗口请求共用此监听器。
 * 在第一个请求发出前调用。
 */
async function ensureListener() {
    if (unlistenStream) return;
    unlistenStream = await listen(EVENTS.SCREENSHOT_CONTROL_HINTS, onControlHintsEvent);
}

/**
 * 向后端请求控件 hints。已确保缓存条目不存在或已过期。
 */
async function requestControlHints(hwnd) {
    // 防抖期间用户可能已移到另一个窗口
    if (hwnd !== activeHwnd) return;

    // 再次检查缓存（可能已被 prefetch 填充）
    const existing = controlCache.get(hwnd);
    if (existing && (existing.status === 'done' || existing.status === 'loading')) return;

    const gen = ++activeGeneration;
    controlCache.set(hwnd, {
        status: 'loading',
        physicalHints: [],
        generation: gen,
    });

    await ensureListener();

    try {
        await screenshotControlHints(hwnd, gen);
    } catch (e) {
        const entry = controlCache.get(hwnd);
        if (entry && entry.generation === gen) {
            entry.status = 'failed';
        }
        if (hwnd === activeHwnd) {
            console.warn('[screenshot] screenshotControlHints invoke 失败', e);
        }
    }
}

// ── 导出 API ──────────────────────────────────────────────────────────────

/**
 * 设置当前控件目标窗口。在 mousemove 中调用。
 *
 * 行为：
 * - HWND 与当前相同：不重复请求。
 * - HWND 为空：清除当前控件 hover，但不清空已有缓存。
 * - 缓存状态为 done 或 loading：直接切换到缓存。
 * - 无缓存：防抖 ~100ms 后启动请求。
 * - HWND 变化时同步 activeGeneration 到缓存条目的 generation，使旧结果不污染当前显示。
 *
 * @param {number|null} hwnd - 目标窗口 HWND，null 表示鼠标在桌面空白区域
 */
export function setControlTarget(hwnd) {
    if (hwnd === activeHwnd) return; // 相同窗口，无操作

    activeHwnd = hwnd;
    hoveredIndex = -1;
    hideControlHint();

    if (!hwnd) {
        // 鼠标在桌面空白区域：清除控件列表，但不清空缓存
        pickableControls = [];
        return;
    }

    const entry = controlCache.get(hwnd);
    if (entry && (entry.status === 'done' || entry.status === 'loading')) {
        // 缓存命中：同步 activeGeneration 到条目的 generation
        // 这样后续该窗口的 batch 到达时会更新显示
        activeGeneration = entry.generation;
        recomputePickableControls();
        return;
    }

    // 无缓存：清除控件列表，防抖后请求
    pickableControls = [];
    if (hoverDebounceTimer) clearTimeout(hoverDebounceTimer);
    hoverDebounceTimer = setTimeout(() => {
        hoverDebounceTimer = 0;
        requestControlHints(hwnd);
    }, 100);
}

/**
 * 预热控件 hints（截图加载完成后对前台窗口调用一次）。
 * 不设置 activeHwnd（不改变显示状态），只发起请求填充缓存。
 *
 * @param {number} hwnd - 预热目标窗口 HWND
 */
export async function prefetchControlHints(hwnd) {
    if (!hwnd) return;
    const existing = controlCache.get(hwnd);
    if (existing) return; // 已缓存或正在加载

    const gen = ++activeGeneration;
    controlCache.set(hwnd, {
        status: 'loading',
        physicalHints: [],
        generation: gen,
    });

    await ensureListener();

    try {
        await screenshotControlHints(hwnd, gen);
    } catch (e) {
        const entry = controlCache.get(hwnd);
        if (entry && entry.generation === gen) {
            entry.status = 'failed';
        }
        console.warn('[screenshot] prefetchControlHints 失败', e);
    }
}

/** 将物理控件矩形转换并裁剪到当前 overlay；完全不可见的控件不进入 hit-test。 */
export function normalizeControlHints(list, meta, viewportWidth, viewportHeight) {
    return (list || []).map((c) => {
        const screenRect = {x: c.x, y: c.y, w: c.w, h: c.h};
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

/**
 * 释放控件列表 + 立即隐藏提示框（overlay 关闭时调）。
 * 清空缓存、递增 generation、解除监听器。
 */
export function clearControlHints() {
    // 解除会话级监听器
    if (unlistenStream) {
        unlistenStream();
        unlistenStream = null;
    }
    // 清空缓存
    controlCache.clear();
    // 重置会话状态
    activeHwnd = null;
    activeGeneration++;
    pickableControls = [];
    hoveredIndex = -1;
    // 清除防抖计时器
    if (hoverDebounceTimer) {
        clearTimeout(hoverDebounceTimer);
        hoverDebounceTimer = 0;
    }
    resetPreselectionHint();
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
    return {x: c.x, y: c.y, w: c.w, h: c.h};
}

/** 只清除 hover 状态（隐藏虚线框），不清除控件列表。
 * 拖动开始或进入标注模式时调用。 */
export function clearControlHover() {
    if (hoveredIndex >= 0) {
        hoveredIndex = -1;
        hideControlHint();
    }
}

/** 把统一预选框切换到控件层级，保留与窗口层级的连续几何轨迹。 */
function showControlHint(c) {
    showPreselectionHint(c, 'control');
}

/** 隐藏控件虚线框（淡出） */
function hideControlHint() {
    hidePreselectionHint('control');
}
