//! 长截图采集框与缩略预览。只负责 DOM / Canvas 呈现，不参与采集和配准决策。

import {ss} from '../ss-state.js';
import {positionedFrameBounds} from './stitch.js';
import {computePreviewPosition} from './preview-layout.js';
import {uiScaleAtCss} from '../ss-selection-geometry.js';
import {findDisplayCssAt, getMonitorForScroll} from '../ss-display.js';
import {computeFloatingPlacement} from '../ss-utils.js';
import {createScrollThumbnail, SCROLL_THUMBNAIL_WIDTH} from './thumbnail.js';

const PREVIEW_W = SCROLL_THUMBNAIL_WIDTH;
export const SCROLL_PREVIEW_GAP = 8;
let segmentCanvasCache = new WeakMap();
let cachedAccent = null;
let lastRendering = null;

function canvasForSegment(segment) {
    let cached = segmentCanvasCache.get(segment.image);
    if (cached) return cached;
    const image = segment.preview || createScrollThumbnail(segment.image);
    cached = document.createElement('canvas');
    cached.width = image.width;
    cached.height = image.height;
    cached.getContext('2d').putImageData(image, 0, 0);
    segmentCanvasCache.set(segment.image, cached);
    return cached;
}

export function resetPreviewRendering() {
    ss.scrollPreviewCanvas?.classList.remove('is-pending');
    cachedAccent = null;
    segmentCanvasCache = new WeakMap();
    lastRendering = null;
}

/**
 * 显示采集框。通过 transform: scale(uiScale) 补偿跨屏 DPR 差异，
 * 使边框物理厚度在目标屏上保持一致；CSS 宽高除以 uiScale 以保持视觉尺寸不变。
 */
export function showCaptureFrame(rect) {
    const frame = document.getElementById('scroll-capture-frame');
    if (!frame) return;
    const meta = window.__blinkScreenMeta || {vx: 0, vy: 0};
    const uiScale = uiScaleAtCss(rect.x + rect.w / 2, rect.y + rect.h / 2, meta);
    frame.style.transformOrigin = 'top left';
    frame.style.transform = `scale(${uiScale})`;
    frame.style.left = rect.x + 'px';
    frame.style.top = rect.y + 'px';
    frame.style.width = (rect.w / uiScale) + 'px';
    frame.style.height = (rect.h / uiScale) + 'px';
    frame.classList.remove('hidden');
}

export function hideCaptureFrame() {
    document.getElementById('scroll-capture-frame')?.classList.add('hidden');
}

/**
 * 定位缩略图：纵向贴选区右边，横向贴选区下边。
 * 通过 transform: scale(uiScale) 补偿跨屏 DPR 差异，
 * 定位计算使用视觉宽高（= CSS 尺寸 × uiScale）确保完整落入来源显示器。
 * 锚点使用选区中心所在显示器，不使用全局 viewport。
 */
export function positionPreview(rect) {
    const preview = ss.scrollPreviewCanvas;
    if (!preview) return;
    preview.classList.remove('hidden');
    const meta = window.__blinkScreenMeta || {vx: 0, vy: 0};
    const anchorX = rect.x + rect.w / 2;
    const anchorY = rect.y + rect.h / 2;
    const uiScale = uiScaleAtCss(anchorX, anchorY, meta);
    preview.style.transformOrigin = 'top left';
    preview.style.transform = `scale(${uiScale})`;
    const visualW = (preview.offsetWidth || preview.width || PREVIEW_W) * uiScale;
    const mon = getMonitorForScroll(ss.scrollSession) || findDisplayCssAt(anchorX, anchorY);
    // 按最终可用高度预留位置，避免每追加几行就把整个预览向上挪一次。
    const visualH = previewHeightLimit(mon, uiScale) * uiScale;
    const position = computePreviewPosition({
        rect,
        direction: ss.scrollDirection,
        monitorRect: mon,
        previewWidth: visualW,
        previewHeight: visualH,
        gap: SCROLL_PREVIEW_GAP,
    });
    preview.style.left = position.left + 'px';
    preview.style.top = position.top + 'px';
    preview.style.right = '';
    preview.style.bottom = '';
}

function previewHeightLimit(monitor, uiScale) {
    return Math.max(1, Math.min(600, Math.floor((monitor.h - SCROLL_PREVIEW_GAP * 2) / uiScale)));
}

/** 输入仅改变边框状态，不能移动定位框、扩充画布或重绘尚未确认的内容。 */
export function showScrollInputFeedback() {
    ss.scrollPreviewCanvas?.classList.add('is-pending');
}

/** 内容与视口只跟随已确认坐标；候选框是额外标记，不参与布局。 */
export function updatePreview(options = {}) {
    ss.scrollPreviewCanvas?.classList.toggle('is-pending', options.pendingInput === true);
    return renderPreview(options);
}

/**
 * 定位长截图专属工具栏（0.19.16）。
 *
 * 使用 computeFloatingPlacement 计算位置，通过 transform: scale(uiScale)
 * 补偿跨屏 DPR 差异，保证工具栏物理尺寸在目标屏上一致。
 * 定位基于来源显示器矩形，不使用全局 viewport。
 */
export function positionScrollToolbar(rect) {
    const scrollTb = document.getElementById('scroll-toolbar');
    if (!scrollTb) return;
    scrollTb.classList.remove('hidden');
    const meta = window.__blinkScreenMeta || {vx: 0, vy: 0};
    const anchorX = rect.x + rect.w / 2;
    const anchorY = rect.y + rect.h - 1;
    const uiScale = uiScaleAtCss(anchorX, anchorY, meta);
    scrollTb.dataset.uiScale = String(uiScale);
    scrollTb.style.transformOrigin = 'top left';
    scrollTb.style.transform = `scale(${uiScale})`;
    scrollTb.style.left = '-9999px';
    scrollTb.style.top = '-9999px';
    placeScrollToolbar(scrollTb, rect, anchorX, anchorY, uiScale);
    positionScrollDiagnostics();
}

/** 诊断条与工具栏共用来源屏和 DPI；不能固定在虚拟桌面的左下角。 */
export function positionScrollDiagnostics() {
    const panel = document.getElementById('scroll-diagnostics');
    const rect = ss.scrollSession?.scrollSourceRect;
    if (!panel || panel.classList.contains('hidden') || !rect) return;
    const meta = window.__blinkScreenMeta || {vx: 0, vy: 0};
    const anchorX = rect.x + rect.w / 2, anchorY = rect.y + Math.max(0, rect.h - 1);
    const mon = getMonitorForScroll(ss.scrollSession) || findDisplayCssAt(anchorX, anchorY);
    const uiScale = uiScaleAtCss(anchorX, anchorY, meta);
    panel.style.transformOrigin = 'top left';
    panel.style.transform = `scale(${uiScale})`;
    panel.style.width = Math.min(420, Math.max(1, (mon.w - SCROLL_PREVIEW_GAP * 2) / uiScale)) + 'px';
    const toolbar = document.getElementById('scroll-toolbar');
    const box = toolbar && !toolbar.classList.contains('hidden') ? toolbar.getBoundingClientRect() : null;
    const anchorRect = box?.width > 0
        ? {x: box.left, y: box.top, w: box.width, h: box.height} : rect;
    const placement = computeFloatingPlacement({anchorRect,
        visualWidth: panel.offsetWidth * uiScale, visualHeight: panel.offsetHeight * uiScale,
        monitorRect: mon, margin: SCROLL_PREVIEW_GAP});
    panel.style.left = placement.left + 'px';
    panel.style.top = placement.top + 'px';
    panel.style.bottom = '';
}

/**
 * 同步摆位（0.23.11）。原实现把 computeFloatingPlacement 放进 rAF——实测设置
 * WDA_EXCLUDEFROMCAPTURE 后 WebView2 的呈现可能停摆（DOM/命中正常、画面不更新），
 * 摆位帧被吞，工具栏停留在 -9999px 隐身，用户视角即"点长截图没反应"。
 * offsetWidth 读取本身会强制同步布局，无需 rAF；同步摆位保证工具栏进入
 * 蒙版清除后的第一帧，呈现停摆时也已至少呈现过一次完整 UI。
 */
function placeScrollToolbar(scrollTb, rect, anchorX, anchorY, uiScale) {
    try {
        const visualW = scrollTb.offsetWidth * uiScale;
        const visualH = scrollTb.offsetHeight * uiScale;
        const mon = getMonitorForScroll(ss.scrollSession) || findDisplayCssAt(anchorX, anchorY);
        const placement = computeFloatingPlacement({
            anchorRect: rect,
            visualWidth: visualW,
            visualHeight: visualH,
            monitorRect: mon,
            margin: SCROLL_PREVIEW_GAP,
            preferred: 'below-center',
        });
        scrollTb.style.left = placement.left + 'px';
        scrollTb.style.top = placement.top + 'px';
    } catch (e) {
        console.error('[scroll] 滚动工具栏定位失败，降级为选区下方固定偏移', e);
        scrollTb.style.left = Math.max(8, rect.x) + 'px';
        scrollTb.style.top = (rect.y + rect.h + 8) + 'px';
    }
}

/**
 * 一次性输出长截图浮层的最终布局/可见性，定位“采集正常但 UI 看不见”。
 * 双 rAF 后调用，确保工具栏自己的异步定位和 canvas 首帧布局均已提交。
 */
export function logScrollUiDiagnostics(rect) {
    requestAnimationFrame(() => requestAnimationFrame(() => {
        const describe = (id) => {
            const el = document.getElementById(id);
            if (!el) return {id, missing: true};
            const box = el.getBoundingClientRect();
            const style = getComputedStyle(el);
            const centerX = box.left + box.width / 2;
            const centerY = box.top + box.height / 2;
            const centerInViewport = centerX >= 0 && centerX < window.innerWidth
                && centerY >= 0 && centerY < window.innerHeight;
            return {
                id,
                className: el.className,
                hidden: el.classList.contains('hidden'),
                box: {
                    left: box.left, top: box.top, right: box.right, bottom: box.bottom,
                    width: box.width, height: box.height,
                },
                offset: {width: el.offsetWidth, height: el.offsetHeight},
                style: {
                    display: style.display,
                    visibility: style.visibility,
                    opacity: style.opacity,
                    zIndex: style.zIndex,
                    transform: style.transform,
                    pointerEvents: style.pointerEvents,
                },
                centerInViewport,
                stackAtCenter: centerInViewport
                    ? document.elementsFromPoint(centerX, centerY).slice(0, 6).map((node) => (
                        node.id || node.className || node.tagName
                    ))
                    : [],
            };
        };
        const diagnostics = {
            phase: ss.scrollSession?.scrollCapturePhase,
            documentFocus: document.hasFocus(),
            visibilityState: document.visibilityState,
            viewport: {width: window.innerWidth, height: window.innerHeight},
            windowScreen: {x: window.screenX, y: window.screenY},
            sourceRect: rect,
            sourceMonitor: ss.scrollSession?.scrollSourceMonitor,
            toolbar: describe('scroll-toolbar'),
            preview: describe('scroll-preview'),
            captureFrame: describe('scroll-capture-frame'),
        };
        // 字符串化后 DevTools 的“复制控制台消息”不会把关键子对象折叠成 `{…}`。
        console.info('[scroll-ui] layout diagnostics JSON', JSON.stringify(diagnostics));
    }));
}

function renderPreview(options = {}) {
    const startedAt = performance.now();
    const ctx = ss.scrollPreviewCtx;
    const canvas = ss.scrollPreviewCanvas;
    if (!ctx || !canvas) return 0;

    const bounds = positionedFrameBounds(ss.scrollFrames);
    const firstFrame = ss.scrollFrames.find((frame) => frame?.image);
    if (!bounds || !firstFrame) return 0;
    const scale = PREVIEW_W / firstFrame.image.width;
    const locatorDocumentTop = ss.scrollCurrentTop;
    const displayTop = bounds.top;
    const displayBottom = bounds.bottom;
    const previewH = Math.round((displayBottom - displayTop) * scale);

    const rect = ss.scrollSourceRect || {x: 0, y: 0, w: 0, h: 0};
    const mon = getMonitorForScroll(ss.scrollSession) || findDisplayCssAt(rect.x + rect.w / 2, rect.y + rect.h / 2);
    const uiScale = uiScaleAtCss(rect.x + rect.w / 2, rect.y + rect.h / 2, window.__blinkScreenMeta || {});
    const nextHeight = Math.max(1, Math.min(previewH, previewHeightLimit(mon, uiScale)));
    const sizeChanged = canvas.width !== PREVIEW_W || canvas.height !== nextHeight;
    if (canvas.width !== PREVIEW_W) canvas.width = PREVIEW_W;
    if (canvas.height !== nextHeight) canvas.height = nextHeight;
    if (canvas.style.height !== `${canvas.height}px`) canvas.style.height = `${canvas.height}px`;
    // 使用固定的预留高度摆位，画布增长不会反复移动外壳。
    if (sizeChanged && ss.scrollSourceRect) positionPreview(ss.scrollSourceRect);

    const locatorTop = (locatorDocumentTop - displayTop) * scale;
    const locatorH = ss.scrollBandH * scale;
    const sourceOffset = Math.round(Math.max(
        0,
        Math.min(previewH - canvas.height, locatorTop + locatorH / 2 - canvas.height / 2),
    ));
    const hasCandidate = Number.isFinite(options.candidateTop);
    const candidateTop = hasCandidate ? options.candidateTop : null;
    const previous = lastRendering;
    if (!sizeChanged && previous?.canvas === canvas && previous.ctx === ctx
        && previous.offset === sourceOffset && previous.top === displayTop
        && previous.locatorTop === locatorTop && previous.locatorH === locatorH
        && previous.trackingState === ss.scrollTrackingState && previous.candidateTop === candidateTop
        && previous.frames.length === ss.scrollFrames.length
        && previous.frames.every((frame, i) => frame.image === ss.scrollFrames[i].image && frame.top === ss.scrollFrames[i].top)) {
        return performance.now() - startedAt;
    }
    lastRendering = {canvas, ctx, offset: sourceOffset, top: displayTop, locatorTop, locatorH,
        trackingState: ss.scrollTrackingState, candidateTop,
        frames: ss.scrollFrames.map(({image, top}) => ({image, top}))};
    ctx.clearRect(0, 0, canvas.width, canvas.height);
    const orderedFrames = [...ss.scrollFrames].sort((a, b) => a.top - b.top);
    for (const captured of orderedFrames) {
        // 相邻片段共用同一条整数像素边界，避免小数边缘的透明度混合露出横线。
        const scaledTop = Math.round((captured.top - displayTop) * scale) - sourceOffset;
        const scaledBottom = Math.round((captured.top + captured.image.height - displayTop) * scale) - sourceOffset;
        const scaledH = scaledBottom - scaledTop;
        if (scaledH <= 0 || scaledBottom <= 0 || scaledTop >= canvas.height) continue;
        const tmp = canvasForSegment(captured);
        ctx.drawImage(tmp, 0, scaledTop, PREVIEW_W, scaledH);
    }

    const locatorY = locatorTop - sourceOffset;
    const visibleTop = Math.max(1, locatorY);
    const visibleBottom = Math.min(canvas.height - 1, locatorY + locatorH);
    if (visibleBottom <= visibleTop) return performance.now() - startedAt;

    cachedAccent ||= getComputedStyle(document.documentElement).getPropertyValue('--accent').trim()
        || 'Highlight';
    // 已通过历史像素复核、只等连续帧确认的候选不是“识别失败”。候选框使用
    // 主题强调色虚线；只有没有可靠候选的真正 lost 状态才显示橙色。
    const trackingLost = ss.scrollTrackingState === 'lost' && !hasCandidate;
    ctx.save();
    ctx.fillStyle = trackingLost ? 'rgba(245, 158, 11, 0.18)' : cachedAccent;
    ctx.strokeStyle = trackingLost ? '#f59e0b' : cachedAccent;
    ctx.lineWidth = 2;
    if (trackingLost) ctx.setLineDash([5, 4]);
    ctx.globalAlpha = trackingLost ? 1 : 0.14;
    ctx.fillRect(1, visibleTop, canvas.width - 2, visibleBottom - visibleTop);
    ctx.globalAlpha = 1;
    ctx.strokeRect(1, visibleTop, canvas.width - 2, visibleBottom - visibleTop);
    ctx.restore();
    if (hasCandidate) {
        const candidateY = (options.candidateTop - displayTop) * scale - sourceOffset;
        const top = Math.max(1, candidateY), bottom = Math.min(canvas.height - 1, candidateY + locatorH);
        if (bottom > top) {
            ctx.save();
            ctx.strokeStyle = cachedAccent;
            ctx.setLineDash([4, 3]);
            ctx.strokeRect(1, top, canvas.width - 2, bottom - top);
            ctx.restore();
        }
    }
    return performance.now() - startedAt;
}
