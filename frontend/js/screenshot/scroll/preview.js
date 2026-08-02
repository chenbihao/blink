//! 长截图采集框与缩略预览。只负责 DOM / Canvas 呈现，不参与采集和配准决策。

import { ss } from '../ss-state.js';
import { positionedFrameBounds } from './stitch.js';

const PREVIEW_W = 120;
export const SCROLL_PREVIEW_GAP = 8;

export function showCaptureFrame(rect) {
  const frame = document.getElementById('scroll-capture-frame');
  if (!frame) return;
  frame.style.left = rect.x + 'px';
  frame.style.top = rect.y + 'px';
  frame.style.width = rect.w + 'px';
  frame.style.height = rect.h + 'px';
  frame.classList.remove('hidden');
}

export function hideCaptureFrame() {
  document.getElementById('scroll-capture-frame')?.classList.add('hidden');
}

/** 定位缩略图：纵向贴选区右边，横向贴选区下边。 */
export function positionPreview(rect) {
  const preview = ss.scrollPreviewCanvas;
  if (!preview) return;
  preview.classList.remove('hidden');
  preview.style.transform = '';

  if (ss.scrollDirection === 'vertical') {
    preview.style.left = rect.x + rect.w + SCROLL_PREVIEW_GAP + 'px';
    preview.style.top = rect.y + 'px';
  } else {
    preview.style.left = rect.x + 'px';
    preview.style.top = rect.y + rect.h + SCROLL_PREVIEW_GAP + 'px';
  }
  preview.style.right = '';
  preview.style.bottom = '';
}

/** 按当前已定位帧更新缩略图和视口定位框。 */
export function updatePreview() {
  const ctx = ss.scrollPreviewCtx;
  const canvas = ss.scrollPreviewCanvas;
  if (!ctx || !canvas) return;

  const bounds = positionedFrameBounds(ss.scrollFrames);
  const firstFrame = ss.scrollFrames.find((frame) => frame?.image);
  if (!bounds || !firstFrame) return;
  const scale = PREVIEW_W / firstFrame.image.width;
  const previewH = Math.round(bounds.height * scale);

  canvas.width = PREVIEW_W;
  canvas.height = Math.min(previewH, 600);
  canvas.style.height = canvas.height + 'px';

  const locatorTop = (ss.scrollCurrentTop - bounds.top) * scale;
  const locatorH = ss.scrollBandH * scale;
  const sourceOffset = Math.max(
    0,
    Math.min(previewH - canvas.height, locatorTop + locatorH / 2 - canvas.height / 2),
  );
  ctx.clearRect(0, 0, canvas.width, canvas.height);
  const orderedFrames = [...ss.scrollFrames].sort((a, b) => a.top - b.top);
  for (const captured of orderedFrames) {
    const scaledTop = (captured.top - bounds.top) * scale - sourceOffset;
    const scaledH = captured.image.height * scale;
    if (scaledTop + scaledH <= 0 || scaledTop >= canvas.height) continue;
    const tmp = document.createElement('canvas');
    tmp.width = captured.image.width;
    tmp.height = captured.image.height;
    tmp.getContext('2d').putImageData(captured.image, 0, 0);
    ctx.drawImage(tmp, 0, scaledTop, PREVIEW_W, scaledH);
  }

  const locatorY = locatorTop - sourceOffset;
  const visibleTop = Math.max(1, locatorY);
  const visibleBottom = Math.min(canvas.height - 1, locatorY + locatorH);
  if (visibleBottom <= visibleTop) return;

  const accent = getComputedStyle(document.documentElement).getPropertyValue('--accent').trim()
    || 'Highlight';
  const trackingLost = ss.scrollTrackingState === 'lost';
  ctx.save();
  ctx.fillStyle = trackingLost ? 'rgba(245, 158, 11, 0.18)' : accent;
  ctx.strokeStyle = trackingLost ? '#f59e0b' : accent;
  ctx.lineWidth = 2;
  if (trackingLost) ctx.setLineDash([5, 4]);
  ctx.globalAlpha = trackingLost ? 1 : 0.14;
  ctx.fillRect(1, visibleTop, canvas.width - 2, visibleBottom - visibleTop);
  ctx.globalAlpha = 1;
  ctx.strokeRect(1, visibleTop, canvas.width - 2, visibleBottom - visibleTop);
  ctx.restore();
}
