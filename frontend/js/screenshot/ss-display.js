//! 截图 overlay 显示器几何（0.14.6 §4 拆分）。
//!
//! 后端注入 `physicalDisplays`（原始物理矩形），前端用 canvas 实测的 renderScale
//! 实时转换为 CSS 矩形。`monitorDprAtCss` 通过 CSS→screen→physicalDisplay 命中
//! 查询原生 DPI，仅供显示器识别和跨 DPI clamp 使用，不参与坐标变换。

import {ss} from './ss-state.js';
import {screenRectToCss, uiScaleAtCss} from './ss-selection-geometry.js';
import {placeOcrPanel} from './ss-panel-resize.js';

/** 收纳动作始终锚定「更多」，避免隐藏菜单中的按钮返回零矩形。 */
export function getActionAnchorRect(actionId) {
    const button = document.getElementById(`btn-${actionId}`);
    const anchor = button?.classList.contains('ss-menu-action')
        ? document.getElementById('btn-more') : button;
    const rect = anchor?.getBoundingClientRect();
    return rect && rect.right > rect.left && rect.bottom > rect.top
        ? rect : ss.toolbar.getBoundingClientRect();
}

/** 结果面板避让选区；用户拖动/缩放后保留位置，仅按视觉尺寸钳制。 */
export function positionResultPanel(panel, actionId) {
    const scale = applyFloatingUiScale(panel);
    const anchor = getActionAnchorRect(actionId);
    const moved = panel.dataset.userMoved === 'true';
    const current = moved ? panel.getBoundingClientRect() : anchor;
    const mon = findDisplayCssAt((current.left + current.right) / 2, (current.top + current.bottom) / 2);
    panel.style.maxHeight = `${Math.max(1, (mon.h - 16) / scale)}px`;
    panel.style.maxWidth = `${Math.max(1, (mon.w - 16) / scale)}px`;
    const width = panel.offsetWidth * scale, height = panel.offsetHeight * scale;
    const placed = moved ? {
        left: Math.max(mon.x + 8, Math.min(current.left, mon.x + mon.w - width - 8)),
        top: Math.max(mon.y + 8, Math.min(current.top, mon.y + mon.h - height - 8)),
    } : placeOcrPanel(anchor, width, height, mon, ss.selCss);
    panel.style.left = `${placed.left}px`;
    panel.style.top = `${placed.top}px`;
    return scale;
}

// M7 优化：缓存 getDisplays() 结果——renderScale 不变时 CSS 矩形不变
let _displaysCache = null;
let _displaysCacheKey = null;

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
 *
 * M7 优化：结果按 renderScale 缓存，mousemove 高频调用时不重复转换。
 * 缓存 key = renderScaleX|renderScaleY，resize 时调 invalidateDisplaysCache() 失效。
 */
export function getDisplays() {
    const meta = window.__blinkScreenMeta;
    if (!meta) return [];
    const physical = getPhysicalDisplays();
    if (physical.length === 0) return [];
    // 缓存 key：renderScale 变化时失效
    const rx = meta.renderScaleX || 1;
    const ry = meta.renderScaleY || 1;
    const key = `${rx}|${ry}|${physical.length}`;
    if (_displaysCache && _displaysCacheKey === key) {
        return _displaysCache;
    }
    _displaysCache = physical.map((d) => {
        const css = screenRectToCss({x: d.x, y: d.y, w: d.w, h: d.h}, meta);
        return {x: css.x, y: css.y, w: css.w, h: css.h, dpi: d.dpi, primary: d.primary};
    });
    _displaysCacheKey = key;
    return _displaysCache;
}

/** M7 优化：手动失效 displays 缓存（resize / syncRenderScale 后调） */
export function invalidateDisplaysCache() {
    _displaysCache = null;
    _displaysCacheKey = null;
}

/**
 * 读取当前工具栏 UI scale（由 positionToolbar 设置）。
 * 浮层元素（sub-panel、OCR panel、size-hint、magnifier）调用此函数
 * 获取与工具栏一致的视觉缩放比，保证跨屏物理尺寸一致。
 */
export function getToolbarUiScale() {
    const {toolbar} = ss;
    return parseFloat(toolbar?.dataset?.uiScale) || 1;
}

/**
 * 给浮层元素应用 UI scale（transform: scale + transform-origin: top left）。
 * 返回 uiScale 供调用方用于视觉宽高计算。
 */
export function applyFloatingUiScale(el) {
    const uiScale = getToolbarUiScale();
    el.style.transformOrigin = 'top left';
    el.style.transform = `scale(${uiScale})`;
    return uiScale;
}

/**
 * 按锚点 CSS 坐标计算 UI scale 并应用到浮层元素。
 *
 * 与 `applyFloatingUiScale` 不同，此函数不依赖工具栏的 dataset.uiScale，
 * 而是根据指定 CSS 坐标所在显示器的原生 DPR 独立计算缩放比。
 * 用于 sizeHint、pixel magnifier 等需要按自身位置独立缩放的浮层。
 *
 * @param {HTMLElement} el - 要缩放的元素
 * @param {number} cssX - 锚点 CSS X
 * @param {number} cssY - 锚点 CSS Y
 * @returns {number} 应用的 uiScale
 */
export function applyFloatingUiScaleAt(el, cssX, cssY) {
    const meta = window.__blinkScreenMeta || {vx: 0, vy: 0};
    const uiScale = uiScaleAtCss(cssX, cssY, meta);
    el.style.transformOrigin = 'top left';
    el.style.transform = `scale(${uiScale})`;
    return uiScale;
}

/**
 * 获取长截图会话保存的来源显示器矩形。
 * 如果 session 中没有保存，返回 null（调用方应 fallback 到 findDisplayCssAt）。
 */
export function getMonitorForScroll(session) {
    if (session && session.scrollSourceMonitor) {
        return session.scrollSourceMonitor;
    }
    return null;
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
    return {x: 0, y: 0, w: window.innerWidth, h: window.innerHeight};
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

/** 定位工具栏（PixPin 风格）。
 *  先确定目标屏（选区右下角内侧），计算候选位置后无条件 clamp 到屏内。
 *  用户已手动拖动过时，保留位置但重新 clamp 防止越界。
 *  工具栏按选区右下角所在屏的 DPR 计算 UI scale，用 transform: scale 补偿
 *  跨屏 renderScale 差异，保证物理尺寸一致。
 *  `immediate`：跳过「-9999 移出 + rAF」测量流程同步定位——用于工具栏
 *  已可见且已布局的场景（宿主切面通过 keepAnchor 保留左侧抓手，避免宽度变化导致位移）。 */
export function positionToolbar(rect, immediate = false) {
    const {toolbar} = ss;
    toolbar.classList.remove('hidden');
    // 录制时工具显隐及选中工具都会触发重新定位，始终以左侧抓手为锚点。
    const recording = toolbar.dataset.mode && toolbar.dataset.mode !== 'screenshot';
    const keepAnchor = toolbar.dataset.keepAnchor === 'true' || recording;
    delete toolbar.dataset.keepAnchor;

    // 按选区右下角所在屏计算 UI scale
    const meta = window.__blinkScreenMeta || {vx: 0, vy: 0};
    const retained = (keepAnchor || toolbar.dataset.userMoved === 'true') && toolbar.style.left && toolbar.style.top;
    const anchorX = retained ? parseFloat(toolbar.style.left) + 4 : rect.x + Math.max(0, rect.w - 1);
    const anchorY = retained ? parseFloat(toolbar.style.top) + 4 : rect.y + Math.max(0, rect.h - 1);
    let uiScale = uiScaleAtCss(anchorX, anchorY, meta);
    toolbar.dataset.uiScale = String(uiScale);
    toolbar.style.transformOrigin = 'top left';
    toolbar.style.transform = `scale(${uiScale})`;
    const actionMonitor = findDisplayCssAt(anchorX, anchorY);
    uiScale *= ss._refreshActionLayout?.((actionMonitor.w - 16) / uiScale) ?? 1;
    // offsetWidth 会四舍五入；分数 DPI 下按真实视觉矩形反算，避免右侧漏出半个像素。
    const measured = toolbar.getBoundingClientRect?.();
    const measuredScale = parseFloat(toolbar.dataset.uiScale) || 1;
    const intrinsicWidth = measured?.width > 0 ? measured.width / measuredScale : toolbar.offsetWidth;
    const intrinsicHeight = measured?.height > 0 ? measured.height / measuredScale : toolbar.offsetHeight;
    uiScale *= Math.min(1, (actionMonitor.w - 16) / Math.max(1, intrinsicWidth * uiScale));
    toolbar.dataset.uiScale = String(uiScale);
    toolbar.style.transform = `scale(${uiScale})`;

    // 用户已手动拖动过：保留位置但重新 clamp 到当前屏
    if ((toolbar.dataset.userMoved === 'true' || keepAnchor) && toolbar.style.left && toolbar.style.top) {
        const tw = intrinsicWidth * uiScale;
        const th = intrinsicHeight * uiScale;
        if (tw > 0 && th > 0) {
            const left = parseFloat(toolbar.style.left);
            const top = parseFloat(toolbar.style.top);
            const mon = actionMonitor;
            const MARGIN = 8;
            const clampedLeft = Math.max(mon.x + MARGIN, Math.min(left, mon.x + mon.w - tw - MARGIN));
            const clampedTop = Math.max(mon.y + MARGIN, Math.min(top, mon.y + mon.h - th - MARGIN));
            if (clampedLeft !== left || clampedTop !== top) {
                const floating = toolbar.classList.contains('toolbar-floating');
                applyToolbarPos(clampedLeft, clampedTop, floating, uiScale);
            } else {
                ss._reflowRecordingPanels?.();
            }
        }
        return;
    }

    const place = () => {
        const tw = intrinsicWidth * uiScale;
        const th = intrinsicHeight * uiScale;
        const MARGIN = 8;

        // 确定目标屏（选区右下角内侧点，默认语义工具栏靠选区右侧）
        const mon = findDisplayCssAt(anchorX, anchorY);

        // 水平：优先右对齐，clamp 到目标屏
        const minLeft = mon.x + MARGIN;
        const maxLeft = mon.x + mon.w - tw - MARGIN;
        let left = rect.x + rect.w - tw;
        left = Math.max(minLeft, Math.min(left, maxLeft));

        // 垂直：选区下方 → 上方 → 浮动选区内部
        const minTop = mon.y + MARGIN;
        const maxTop = mon.y + mon.h - th - MARGIN;
        const below = rect.y + rect.h + MARGIN;
        const above = rect.y - th - MARGIN;

        let top;
        let floating = false;

        if (below <= maxTop) {
            top = below;
        } else if (above >= minTop) {
            top = above;
        } else {
            top = Math.max(minTop, Math.min(rect.y + rect.h - th - MARGIN, maxTop));
            floating = true;
        }

        // 最终无条件 clamp（保证工具栏完整位于屏内）
        left = Math.max(minLeft, Math.min(left, maxLeft));
        top = Math.max(minTop, Math.min(top, maxTop));

        applyToolbarPos(left, top, floating, uiScale);
    };

    if (immediate) {
        place();
    } else {
        toolbar.style.left = '-9999px';
        toolbar.style.top = '-9999px';
        requestAnimationFrame(place);
    }
}

function applyToolbarPos(left, top, floating, uiScale = 1) {
    const {toolbar} = ss;
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
            // 使用变换后的视觉高度
            const visualH = toolbar.offsetHeight * uiScale;
            subP.style.left = left + 'px';
            subP.style.top = (top + (floating ? 0 : visualH) + 4) + 'px';
        }
    }
    ss._reflowRecordingPanels?.();
    console.debug('[screenshot] toolbar 定位', {
        left,
        top,
        tw: toolbar.offsetWidth,
        th: toolbar.offsetHeight,
        visualW: toolbar.offsetWidth * uiScale,
        visualH: toolbar.offsetHeight * uiScale,
        uiScale,
        floating
    });
}
