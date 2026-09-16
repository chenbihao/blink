//! pin 图文字图层（0.23.12）：图上可划词的透明 canvas 覆盖层。
//!
//! 交互契约（与截图 overlay 阅读模式同一套"空白放行"语义，纯函数共用
//! shared/text-selection.js）：
//! - 文字框上按下/拖动 = 划词（高亮 + 可 Ctrl+C / 右键"复制选中文字"）
//! - 真空白（到最近文字框超过 BLANK_RELEASE_THRESHOLD_CSS 视觉距离）按下
//!   = 完全不吞事件，放行给 pin 现有的 document pointerdown → startDragging
//!   原生窗口拖动；阈值内（行间空隙、行尾）仍按最近行兜底进入划词
//! - 文字上双击 = 选整行；空白双击放行（保留 mini 缩小切换）
//! - 右键不吞（保留 pin 右键菜单，菜单内动态提供"复制选中文字"）
//!
//! 粒度：原文层字符级（OCR char_boxes 优先，word 降级）；译文层行级
//! （toOverlayLines 行数组——嵌图渲染只产出行的最终排版框）。
//!
//! 坐标模型（多屏混合 DPI 安全，spec-frontend §5.6）：
//! - items.rect 与 sourcePixel 同域（图片资源像素）
//! - 层 CSS 尺寸 = baseCss × zoom，imgScale = cssW / srcW 由 getGeometry
//!   动态提供；zoom/DPI 变化由宿主调 relayoutPinTextLayer()
//! - backing store 额外乘 window.devicePixelRatio 保证高分屏描边清晰

import {
    BLANK_RELEASE_THRESHOLD_CSS,
    hitTestItems,
    mergeRunsByLine,
    nearestItemByLine,
    nearestTextDistance,
    selectionTextOf,
} from '../shared/text-selection.js';

/** OCR 结果 → 原文划词 items（char_boxes 字符级优先，word 降级）。 */
export function sourceItemsFromOcrResult(result) {
    const fullText = typeof result?.text === 'string' ? result.text : '';
    const raw = (result && Array.isArray(result.char_boxes)) ? result.char_boxes : [];
    if (raw.length > 0) {
        return raw.map((cb) => ({
            text: cb.text,
            rect: cb.rect,
            lineIndex: cb.line_index,
            start: rustCharIndexToUtf16(fullText, cb.char_start),
            end: rustCharIndexToUtf16(fullText, cb.char_end),
        }));
    }
    const words = (result && Array.isArray(result.words)) ? result.words : [];
    const ranges = Array.isArray(result?.char_ranges) ? result.char_ranges : [];
    let searchFrom = 0;
    return words.map((w, i) => {
        let start = null;
        let end = null;
        if (Array.isArray(ranges[i]) && ranges[i].length >= 2) {
            start = rustCharIndexToUtf16(fullText, ranges[i][0]);
            end = rustCharIndexToUtf16(fullText, ranges[i][1]);
        } else if (fullText && w.text) {
            const found = fullText.indexOf(w.text, searchFrom);
            if (found >= 0) {
                start = found;
                end = found + w.text.length;
                searchFrom = end;
            }
        }
        return {
            text: w.text,
            rect: w.rect,
            lineIndex: w.line_index,
            start,
            end,
        };
    });
}

/** 翻译覆盖行 → 译文划词 items（行级：一行一个 item，文本取译文）。 */
export function translatedItemsFromLines(lines) {
    let cursor = 0;
    return lines.map((ln, i) => {
        const text = ln.dstText || ln.srcText || '';
        const start = cursor;
        const end = start + text.length;
        cursor = end + 1; // 调用方用换行拼接各行
        return {text, rect: ln.rect, lineIndex: i, start, end};
    });
}

function rustCharIndexToUtf16(fullText, charIndex) {
    if (!Number.isInteger(charIndex) || charIndex < 0) return null;
    let utf16Offset = 0;
    let charCount = 0;
    for (const ch of fullText) {
        if (charCount >= charIndex) break;
        charCount++;
        utf16Offset += ch.length;
    }
    return charCount === charIndex ? utf16Offset : null;
}

// ── 层状态（模块级单例：一个 pin 窗口一个层） ─────────────────────
const state = {
    active: false,
    kind: 'source',
    items: [],
    fullText: '',
    selStart: null,
    selEnd: null,
    dragStart: null,
    hover: null,
    eventsBound: false,
    canvas: null,
    ctx: null,
    getGeometry: null,
};

/** 当前选中文字；无选择返回 null。 */
export function getPinTextLayerSelection() {
    if (!state.active || state.selStart === null || state.selEnd === null) return null;
    const lo = Math.min(state.selStart, state.selEnd);
    const hi = Math.max(state.selStart, state.selEnd);
    const text = selectionTextOf(state.items, lo, hi, state.fullText);
    return text || null;
}

/** 清除当前划选（不清层）。Escape 优先级链用。 */
export function clearPinTextSelection() {
    if (state.selStart === null && state.selEnd === null && state.dragStart === null) return false;
    state.selStart = null;
    state.selEnd = null;
    state.dragStart = null;
    redraw();
    return true;
}

export function isPinTextLayerActive() {
    return state.active;
}

export function getPinTextLayerKind() {
    return state.active ? state.kind : null;
}

/** mini 模式等场景隐藏层（层不跟随 mini 缩放，避免错位）。 */
export function setPinTextLayerHidden(hidden) {
    if (!state.canvas) return;
    state.canvas.classList.toggle('hidden', !!hidden);
}

/**
 * 激活文字图层。
 *
 * @param {object} opts
 * @param {{text:string, rect:{x,y,w,h}, lineIndex:number}[]} opts.items
 * @param {'source'|'translated'} opts.kind
 * @param {() => {padX:number, padY:number, cssW:number, cssH:number, srcW:number, srcH:number}} opts.getGeometry
 *        宿主动态提供层几何（pad + 图片 CSS 尺寸 + 资源像素尺寸）
 */
export function enterPinTextLayer({items, fullText = '', kind = 'source', getGeometry}) {
    if (!items || items.length === 0 || typeof getGeometry !== 'function') return;
    ensureLayer();
    state.active = true;
    state.kind = kind;
    state.items = items;
    state.fullText = typeof fullText === 'string' ? fullText : '';
    state.selStart = null;
    state.selEnd = null;
    state.dragStart = null;
    state.hover = null;
    state.getGeometry = getGeometry;
    state.canvas.classList.remove('hidden');
    relayoutPinTextLayer();
}

export function exitPinTextLayer() {
    if (!state.active) return;
    state.active = false;
    state.items = [];
    state.fullText = '';
    state.selStart = null;
    state.selEnd = null;
    state.dragStart = null;
    state.hover = null;
    if (state.canvas) {
        state.canvas.classList.add('hidden');
        if (state.ctx && state.canvas.width > 0) {
            state.ctx.clearRect(0, 0, state.canvas.width, state.canvas.height);
        }
    }
}

/** zoom / DPI / 图片尺寸变化后由宿主调用：重定位 + 重绘（选择状态保留）。 */
export function relayoutPinTextLayer() {
    if (!state.active || !state.canvas || !state.getGeometry) return;
    const g = state.getGeometry();
    if (!g || g.cssW <= 0 || g.cssH <= 0 || g.srcW <= 0 || g.srcH <= 0) return;
    state.canvas.style.left = g.padX + 'px';
    state.canvas.style.top = g.padY + 'px';
    state.canvas.style.width = g.cssW + 'px';
    state.canvas.style.height = g.cssH + 'px';
    const dpr = window.devicePixelRatio || 1;
    state.canvas.width = Math.max(1, Math.round(g.cssW * dpr));
    state.canvas.height = Math.max(1, Math.round(g.cssH * dpr));
    redraw();
}

// ── 内部实现 ──────────────────────────────────────────────────

/** 层 CSS 坐标（相对图片区域）→ 图片像素坐标。 */
function cssToImage(cssX, cssY) {
    const g = state.getGeometry();
    const imgScale = g.cssW / g.srcW;
    return {x: cssX / imgScale, y: cssY / imgScale, imgScale};
}

/** 图片像素矩形 → 层 backing store 像素矩形。 */
function imageRectToBacking(rect, imgScale) {
    const dpr = window.devicePixelRatio || 1;
    const s = imgScale * dpr;
    return {
        x: rect.x * s,
        y: rect.y * s,
        w: rect.w * s,
        h: rect.h * s,
    };
}

/** 真空白判定：到最近文字框的视觉距离超过阈值（CSS px，图片像素域折算）。 */
function isBlankForRelease(cssX, cssY) {
    const p = cssToImage(cssX, cssY);
    // 阈值是视觉 CSS 距离，换算到图片像素域（除以 imgScale 等价乘 srcW/cssW）
    const thresholdImage = BLANK_RELEASE_THRESHOLD_CSS / p.imgScale;
    return nearestTextDistance(state.items, p.x, p.y) > thresholdImage;
}

function redraw() {
    if (!state.canvas || !state.ctx) return;
    const {ctx, canvas} = state;
    ctx.clearRect(0, 0, canvas.width, canvas.height);
    if (!state.active) return;
    const g = state.getGeometry();
    const imgScale = g.cssW / g.srcW;
    const dpr = window.devicePixelRatio || 1;
    const lineWidth = Math.max(1, Math.round(dpr));
    const half = lineWidth % 2 === 1 ? 0.5 : 0;

    if (state.selStart !== null && state.selEnd !== null) {
        const lo = Math.min(state.selStart, state.selEnd);
        const hi = Math.max(state.selStart, state.selEnd);
        const runs = mergeRunsByLine(state.items, lo, hi);
        ctx.fillStyle = 'rgba(74, 158, 255, 0.32)';
        for (const r of runs) {
            const b = imageRectToBacking(r, imgScale);
            ctx.fillRect(b.x, b.y, b.w, b.h);
        }
        ctx.strokeStyle = 'rgba(74, 158, 255, 0.6)';
        ctx.lineWidth = lineWidth;
        for (const r of runs) {
            const b = imageRectToBacking(r, imgScale);
            ctx.strokeRect(b.x + half, b.y + half, b.w, b.h);
        }
    }
    if (state.hover !== null && state.hover >= 0 && state.items[state.hover]) {
        const b = imageRectToBacking(state.items[state.hover].rect, imgScale);
        ctx.strokeStyle = 'rgba(255, 255, 255, 0.5)';
        ctx.lineWidth = 1;
        ctx.strokeRect(b.x + 0.5, b.y + 0.5, b.w, b.h);
    }
}

function beginDrag(e, idx) {
    e.stopPropagation();
    e.preventDefault();
    state.dragStart = idx;
    state.selStart = idx;
    state.selEnd = idx;
    redraw();
    if (typeof state.canvas.setPointerCapture === 'function') {
        try {
            state.canvas.setPointerCapture(e.pointerId);
        } catch (_) {
        }
    }
}

function bindLayerEvents() {
    if (state.eventsBound) return;
    state.eventsBound = true;
    const cv = state.canvas;

    cv.addEventListener('pointerdown', (e) => {
        if (!state.active || e.button !== 0) return;
        const p = cssToImage(e.offsetX, e.offsetY);
        let idx = hitTestItems(state.items, p.x, p.y);
        if (idx < 0) {
            if (isBlankForRelease(e.offsetX, e.offsetY)) {
                // 真空白：不吞事件，冒泡给 document 的拖动处理器 → startDragging
                return;
            }
            idx = nearestItemByLine(state.items, p.x, p.y);
        }
        if (idx < 0) return;
        beginDrag(e, idx);
    });

    cv.addEventListener('pointermove', (e) => {
        if (!state.active) return;
        const p = cssToImage(e.offsetX, e.offsetY);
        const idx = hitTestItems(state.items, p.x, p.y);
        cv.style.cursor = (idx < 0 && state.dragStart === null && isBlankForRelease(e.offsetX, e.offsetY))
            ? 'move'
            : 'text';
        if (state.dragStart !== null) {
            const endIdx = idx >= 0 ? idx : nearestItemByLine(state.items, p.x, p.y);
            if (endIdx >= 0 && endIdx !== state.selEnd) {
                state.selEnd = endIdx;
                redraw();
            }
        } else if (idx !== state.hover) {
            state.hover = idx >= 0 ? idx : null;
            redraw();
        }
    });

    const finishDrag = () => {
        state.dragStart = null;
    };
    cv.addEventListener('pointerup', finishDrag);
    cv.addEventListener('pointercancel', finishDrag);

    cv.addEventListener('mouseleave', () => {
        if (!state.active) return;
        state.hover = null;
        state.dragStart = null;
        cv.style.cursor = 'text';
        redraw();
    });

    // 文字上双击 = 选整行（连续同 lineIndex 段）；空白双击放行（mini 切换保留）
    cv.addEventListener('dblclick', (e) => {
        if (!state.active) return;
        const p = cssToImage(e.offsetX, e.offsetY);
        let idx = hitTestItems(state.items, p.x, p.y);
        if (idx < 0) {
            if (isBlankForRelease(e.offsetX, e.offsetY)) return;
            idx = nearestItemByLine(state.items, p.x, p.y);
        }
        if (idx < 0) return;
        e.stopPropagation();
        const line = state.items[idx].lineIndex;
        let lo = idx, hi = idx;
        while (lo > 0 && state.items[lo - 1].lineIndex === line) lo--;
        while (hi < state.items.length - 1 && state.items[hi + 1].lineIndex === line) hi++;
        state.selStart = lo;
        state.selEnd = hi;
        redraw();
    });
}

function ensureLayer() {
    if (state.canvas) return;
    const cv = document.createElement('canvas');
    cv.id = 'pin-text-layer';
    // 层样式主体在 pin.css（层级/指针），几何（left/top/尺寸）由 relayout 写入
    state.canvas = cv;
    state.ctx = cv.getContext('2d');
    bindLayerEvents();
    document.getElementById('container').appendChild(cv);
}
