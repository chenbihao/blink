//! 长截图采集框与缩略预览。只负责 DOM / Canvas 呈现，不参与采集和配准决策。

import { ss } from '../ss-state.js';
import { positionedFrameBounds } from './stitch.js';
import { computePredictedLocatorTop, computePreviewPosition } from './preview-layout.js';
import { uiScaleAtCss } from '../ss-selection-geometry.js';
import { findDisplayCssAt, getMonitorForScroll } from '../ss-display.js';
import { computeFloatingPlacement } from '../ss-utils.js';

const PREVIEW_W = 120;
export const SCROLL_PREVIEW_GAP = 8;
let segmentCanvasCache = new WeakMap();
let predictedTop = null;
let predictedDirection = 0;
let predictionFrame = 0;
let cachedAccent = null;

function canvasForImage(image) {
  let cached = segmentCanvasCache.get(image);
  if (cached) return cached;
  cached = document.createElement('canvas');
  cached.width = image.width;
  cached.height = image.height;
  cached.getContext('2d').putImageData(image, 0, 0);
  segmentCanvasCache.set(image, cached);
  return cached;
}

export function resetPreviewRendering() {
  if (predictionFrame) cancelAnimationFrame(predictionFrame);
  predictionFrame = 0;
  predictedTop = null;
  predictedDirection = 0;
  cachedAccent = null;
  segmentCanvasCache = new WeakMap();
}

/**
 * 显示采集框。通过 transform: scale(uiScale) 补偿跨屏 DPR 差异，
 * 使边框物理厚度在目标屏上保持一致；CSS 宽高除以 uiScale 以保持视觉尺寸不变。
 */
export function showCaptureFrame(rect) {
  const frame = document.getElementById('scroll-capture-frame');
  if (!frame) return;
  const meta = window.__blinkScreenMeta || { vx: 0, vy: 0 };
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
  const meta = window.__blinkScreenMeta || { vx: 0, vy: 0 };
  const anchorX = rect.x + rect.w / 2;
  const anchorY = rect.y + rect.h / 2;
  const uiScale = uiScaleAtCss(anchorX, anchorY, meta);
  preview.style.transformOrigin = 'top left';
  preview.style.transform = `scale(${uiScale})`;
  const visualW = (preview.width || PREVIEW_W) * uiScale;
  const visualH = (preview.height || 200) * uiScale;
  const mon = getMonitorForScroll(ss.scrollSession) || findDisplayCssAt(anchorX, anchorY);
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

/** 滚轮后立即显示预测框；不修改任何真实定位或拼接状态。 */
export function showPredictedPreview(direction) {
  const normalizedDirection = Math.sign(direction || 0);
  const canExtrapolate = ss.scrollTrackingState === 'tracking';
  const pendingTop = canExtrapolate && predictedDirection === normalizedDirection
    ? predictedTop
    : null;
  predictedTop = canExtrapolate
    ? computePredictedLocatorTop({
      currentTop: ss.scrollCurrentTop,
      pendingTop,
      direction: normalizedDirection,
      lastAcceptedShift: ss.scrollLastAcceptedShift,
      viewportHeight: ss.scrollBandH,
    })
    : ss.scrollCurrentTop;
  predictedDirection = normalizedDirection;
  if (predictionFrame) return;
  predictionFrame = requestAnimationFrame(() => {
    predictionFrame = 0;
    renderPreview(predictedTop, { predicted: true });
  });
}

/** 按已确认坐标更新缩略图；新一轮滚轮已发生时保留其预测定位框。 */
export function updatePreview(options = {}) {
  if (predictionFrame) cancelAnimationFrame(predictionFrame);
  predictionFrame = 0;
  if (Number.isFinite(options.candidateTop)) {
    predictedTop = null;
    predictedDirection = 0;
    return renderPreview(options.candidateTop, {
      predicted: true,
      recoveryCandidate: true,
    });
  }
  if (options.preservePrediction && Number.isFinite(predictedTop)) {
    return renderPreview(predictedTop, { predicted: true });
  }
  predictedTop = null;
  predictedDirection = 0;
  return renderPreview(ss.scrollCurrentTop);
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
  const meta = window.__blinkScreenMeta || { vx: 0, vy: 0 };
  const anchorX = rect.x + rect.w / 2;
  const anchorY = rect.y + rect.h - 1;
  const uiScale = uiScaleAtCss(anchorX, anchorY, meta);
  scrollTb.dataset.uiScale = String(uiScale);
  scrollTb.style.transformOrigin = 'top left';
  scrollTb.style.transform = `scale(${uiScale})`;
  scrollTb.style.left = '-9999px';
  scrollTb.style.top = '-9999px';
  requestAnimationFrame(() => {
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
  });
}

/**
 * 一次性输出长截图浮层的最终布局/可见性，定位“采集正常但 UI 看不见”。
 * 双 rAF 后调用，确保工具栏自己的异步定位和 canvas 首帧布局均已提交。
 */
export function logScrollUiDiagnostics(rect) {
  requestAnimationFrame(() => requestAnimationFrame(() => {
    const describe = (id) => {
      const el = document.getElementById(id);
      if (!el) return { id, missing: true };
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
        offset: { width: el.offsetWidth, height: el.offsetHeight },
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
      viewport: { width: window.innerWidth, height: window.innerHeight },
      windowScreen: { x: window.screenX, y: window.screenY },
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

function renderPreview(locatorDocumentTop, options = {}) {
  const predicted = options.predicted === true;
  const startedAt = performance.now();
  const ctx = ss.scrollPreviewCtx;
  const canvas = ss.scrollPreviewCanvas;
  if (!ctx || !canvas) return 0;

  const bounds = positionedFrameBounds(ss.scrollFrames);
  const firstFrame = ss.scrollFrames.find((frame) => frame?.image);
  if (!bounds || !firstFrame) return 0;
  const scale = PREVIEW_W / firstFrame.image.width;
  const displayTop = Math.min(bounds.top, locatorDocumentTop);
  const displayBottom = Math.max(bounds.bottom, locatorDocumentTop + ss.scrollBandH);
  const previewH = Math.round((displayBottom - displayTop) * scale);

  const nextHeight = Math.min(previewH, 600);
  const sizeChanged = canvas.width !== PREVIEW_W || canvas.height !== nextHeight;
  if (canvas.width !== PREVIEW_W) canvas.width = PREVIEW_W;
  if (canvas.height !== nextHeight) canvas.height = nextHeight;
  if (canvas.style.height !== `${canvas.height}px`) canvas.style.height = `${canvas.height}px`;
  // canvas 会随长图增长；每次按新高度重新避让屏幕边缘，不能沿用首帧 200px 的位置。
  if (sizeChanged && ss.scrollSourceRect) positionPreview(ss.scrollSourceRect);

  const locatorTop = (locatorDocumentTop - displayTop) * scale;
  const locatorH = ss.scrollBandH * scale;
  const sourceOffset = Math.max(
    0,
    Math.min(previewH - canvas.height, locatorTop + locatorH / 2 - canvas.height / 2),
  );
  ctx.clearRect(0, 0, canvas.width, canvas.height);
  const orderedFrames = [...ss.scrollFrames].sort((a, b) => a.top - b.top);
  for (const captured of orderedFrames) {
    const scaledTop = (captured.top - displayTop) * scale - sourceOffset;
    const scaledH = captured.image.height * scale;
    if (scaledTop + scaledH <= 0 || scaledTop >= canvas.height) continue;
    const tmp = canvasForImage(captured.image);
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
  const trackingLost = ss.scrollTrackingState === 'lost' && !options.recoveryCandidate;
  ctx.save();
  ctx.fillStyle = trackingLost ? 'rgba(245, 158, 11, 0.18)' : cachedAccent;
  ctx.strokeStyle = trackingLost ? '#f59e0b' : cachedAccent;
  ctx.lineWidth = 2;
  if (trackingLost || predicted) ctx.setLineDash(predicted ? [4, 3] : [5, 4]);
  ctx.globalAlpha = trackingLost ? 1 : (predicted ? 0.09 : 0.14);
  ctx.fillRect(1, visibleTop, canvas.width - 2, visibleBottom - visibleTop);
  ctx.globalAlpha = 1;
  ctx.strokeRect(1, visibleTop, canvas.width - 2, visibleBottom - visibleTop);
  ctx.restore();
  return performance.now() - startedAt;
}
