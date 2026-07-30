//! 截图 overlay 绘制模块（0.14.6 §4 拆分）。
//!
//! 从 chord-screenshot.js 提取的绘制函数：
//! - drawDimmed：暗色蒙版（初始态 + 无选区时）
//! - drawSelection：选区拖拽中的实时绘制
//! - drawFinalSelection：选区确定后的静态绘制
//! - redrawAnnotPreview：标注实时预览
//! - redrawAnnotFull：全量重绘标注层

import { ss } from './ss-state.js';
import { norm } from './ss-utils.js';
import * as annot from './annotation-engine.js';

/** 暗色蒙版（初始态 + 无选区时） */
export function drawDimmed() {
  const { ctx, canvas, screenshot } = ss;
  ctx.clearRect(0, 0, canvas.width, canvas.height);
  ctx.drawImage(screenshot, 0, 0);
  ctx.fillStyle = 'rgba(0, 0, 0, 0.45)';
  ctx.fillRect(0, 0, canvas.width, canvas.height);
}

/** 选区绘制：选区外暗 + 选区内亮 */
export function drawSelection() {
  const { ctx, canvas, screenshot, startX, startY, endX, endY, sizeHint } = ss;
  const dpr = window.devicePixelRatio || 1;
  const r = norm(startX, startY, endX, endY);
  const px = r.x * dpr;
  const py = r.y * dpr;
  const pw = r.w * dpr;
  const ph = r.h * dpr;

  ctx.clearRect(0, 0, canvas.width, canvas.height);
  ctx.drawImage(screenshot, 0, 0);

  // 暗色蒙版（选区外）
  ctx.fillStyle = 'rgba(0, 0, 0, 0.45)';
  ctx.fillRect(0, 0, canvas.width, py);
  ctx.fillRect(0, py + ph, canvas.width, canvas.height - py - ph);
  ctx.fillRect(0, py, px, ph);
  ctx.fillRect(px + pw, py, canvas.width - px - pw, ph);

  // 选区边框
  ctx.strokeStyle = '#4a9eff';
  ctx.lineWidth = 2 * dpr;
  ctx.setLineDash([6 * dpr, 3 * dpr]);
  ctx.strokeRect(px, py, pw, ph);
  ctx.setLineDash([]);

  // size-hint 显示物理像素尺寸
  sizeHint.textContent = `${Math.round(pw)} × ${Math.round(ph)}`;
  sizeHint.classList.remove('hidden');
  sizeHint.style.left = (r.x + 4) + 'px';
  sizeHint.style.top = (r.y > 24 ? r.y - 22 : r.y + 4) + 'px';
}

/** 确定选区后的静态绘制（选区不再随鼠标变化，但仍需要蒙版效果） */
export function drawFinalSelection() {
  const { ctx, canvas, screenshot, selCss } = ss;
  if (!selCss) return;
  const dpr = window.devicePixelRatio || 1;
  const px = selCss.x * dpr;
  const py = selCss.y * dpr;
  const pw = selCss.w * dpr;
  const ph = selCss.h * dpr;

  ctx.clearRect(0, 0, canvas.width, canvas.height);
  ctx.drawImage(screenshot, 0, 0);

  // 蒙版
  ctx.fillStyle = 'rgba(0, 0, 0, 0.45)';
  ctx.fillRect(0, 0, canvas.width, py);
  ctx.fillRect(0, py + ph, canvas.width, canvas.height - py - ph);
  ctx.fillRect(0, py, px, ph);
  ctx.fillRect(px + pw, py, canvas.width - px - pw, ph);

  // 选区边框（标注模式：实线，与拖拽虚线区分）
  ctx.strokeStyle = '#4a9eff';
  ctx.lineWidth = 2 * dpr;
  ctx.strokeRect(px, py, pw, ph);

  // 选取工具显示八个调整手柄，明确提示选区可移动/缩放。
  if (annot.getTool() === 'select') {
    const hs = 6 * dpr;
    const points = [
      [px, py], [px + pw / 2, py], [px + pw, py],
      [px + pw, py + ph / 2], [px + pw, py + ph],
      [px + pw / 2, py + ph], [px, py + ph], [px, py + ph / 2],
    ];
    ctx.fillStyle = '#ffffff';
    ctx.strokeStyle = '#4a9eff';
    ctx.lineWidth = Math.max(1, dpr);
    for (const [hx, hy] of points) {
      ctx.fillRect(hx - hs / 2, hy - hs / 2, hs, hs);
      ctx.strokeRect(hx - hs / 2, hy - hs / 2, hs, hs);
    }
  }
}

/** 标注实时预览：重绘已提交的 + 当前绘制中的预览 */
export function redrawAnnotPreview() {
  const { isAnnotDragging, selCss, annotCtx, annotStartX, annotStartY, annotCurrentX, annotCurrentY } = ss;
  if (!isAnnotDragging || !selCss) return;
  redrawAnnotFull();

  const tool = annot.getTool();
  annotCtx.save();
  switch (tool) {
    case 'rect': {
      const x = Math.min(annotStartX, annotCurrentX);
      const y = Math.min(annotStartY, annotCurrentY);
      const w = Math.abs(annotCurrentX - annotStartX);
      const h = Math.abs(annotCurrentY - annotStartY);
      annotCtx.strokeStyle = annot.getColor();
      annotCtx.lineWidth = annot.getWidth();
      annotCtx.strokeRect(x, y, w, h);
      break;
    }
    case 'ellipse': {
      const cx = (annotStartX + annotCurrentX) / 2;
      const cy = (annotStartY + annotCurrentY) / 2;
      const rx = Math.abs(annotCurrentX - annotStartX) / 2;
      const ry = Math.abs(annotCurrentY - annotStartY) / 2;
      annotCtx.strokeStyle = annot.getColor();
      annotCtx.lineWidth = annot.getWidth();
      annotCtx.beginPath();
      annotCtx.ellipse(cx, cy, rx, ry, 0, 0, Math.PI * 2);
      annotCtx.stroke();
      break;
    }
    case 'arrow': {
      const angle = Math.atan2(annotCurrentY - annotStartY, annotCurrentX - annotStartX);
      const headLen = 12 * annot.getWidth() / 2;
      annotCtx.strokeStyle = annot.getColor();
      annotCtx.lineWidth = annot.getWidth();
      annotCtx.beginPath();
      annotCtx.moveTo(annotStartX, annotStartY);
      annotCtx.lineTo(annotCurrentX, annotCurrentY);
      annotCtx.stroke();
      annotCtx.beginPath();
      annotCtx.moveTo(annotCurrentX, annotCurrentY);
      annotCtx.lineTo(annotCurrentX - headLen * Math.cos(angle - 0.4), annotCurrentY - headLen * Math.sin(angle - 0.4));
      annotCtx.moveTo(annotCurrentX, annotCurrentY);
      annotCtx.lineTo(annotCurrentX - headLen * Math.cos(angle + 0.4), annotCurrentY - headLen * Math.sin(angle + 0.4));
      annotCtx.stroke();
      break;
    }
    case 'pencil': {
      const pts = annot.getCurrentPoints();
      if (pts.length >= 2) {
        annotCtx.strokeStyle = annot.getColor();
        annotCtx.lineWidth = annot.getWidth();
        annotCtx.lineCap = 'round';
        annotCtx.lineJoin = 'round';
        annotCtx.beginPath();
        annotCtx.moveTo(pts[0].x, pts[0].y);
        for (let i = 1; i < pts.length; i++) {
          annotCtx.lineTo(pts[i].x, pts[i].y);
        }
        annotCtx.stroke();
      }
      break;
    }
    case 'highlight-multiply':
    case 'highlight-translucent': {
      const pts = annot.getCurrentPoints();
      if (pts.length >= 2) {
        const w = annot.getWidth();
        const alpha = tool === 'highlight-multiply' ? 0.55 : 0.30;
        annotCtx.strokeStyle = annot.withAlpha(annot.getColor(), alpha);
        annotCtx.lineWidth = w * 4;
        annotCtx.lineCap = 'round';
        annotCtx.lineJoin = 'round';
        annotCtx.beginPath();
        annotCtx.moveTo(pts[0].x, pts[0].y);
        for (let i = 1; i < pts.length; i++) {
          annotCtx.lineTo(pts[i].x, pts[i].y);
        }
        annotCtx.stroke();
      }
      break;
    }
    case 'eraser': {
      const pts = annot.getCurrentPoints();
      if (pts.length >= 1) {
        const w = annot.getWidth();
        const r = Math.max(6, w * 3);
        annotCtx.globalCompositeOperation = 'destination-out';
        for (let i = 0; i < pts.length; i++) {
          const p = pts[i];
          annotCtx.beginPath();
          annotCtx.arc(p.x, p.y, r, 0, Math.PI * 2);
          annotCtx.fill();
          if (i > 0) {
            const prev = pts[i - 1];
            annotCtx.strokeStyle = '#000';
            annotCtx.lineWidth = r * 2;
            annotCtx.lineCap = 'round';
            annotCtx.beginPath();
            annotCtx.moveTo(prev.x, prev.y);
            annotCtx.lineTo(p.x, p.y);
            annotCtx.stroke();
          }
        }
      }
      break;
    }
    case 'mosaic': {
      const pts = annot.getCurrentPoints();
      if (pts.length >= 1) {
        const r = 16;
        annotCtx.imageSmoothingEnabled = true;
        for (let i = 0; i < pts.length; i++) {
          const p = pts[i];
          const avg = annot.sampleMosaicColor(p.x, p.y, r);
          annotCtx.fillStyle = avg;
          annotCtx.beginPath();
          annotCtx.arc(p.x, p.y, r, 0, Math.PI * 2);
          annotCtx.fill();
          if (i > 0) {
            const prev = pts[i - 1];
            annotCtx.strokeStyle = avg;
            annotCtx.lineWidth = r * 2;
            annotCtx.lineCap = 'round';
            annotCtx.beginPath();
            annotCtx.moveTo(prev.x, prev.y);
            annotCtx.lineTo(p.x, p.y);
            annotCtx.stroke();
          }
        }
      }
      break;
    }
    case 'pixelate': {
      const x = Math.min(annotStartX, annotCurrentX);
      const y = Math.min(annotStartY, annotCurrentY);
      const w = Math.abs(annotCurrentX - annotStartX);
      const h = Math.abs(annotCurrentY - annotStartY);
      annotCtx.fillStyle = 'rgba(150, 150, 150, 0.4)';
      annotCtx.fillRect(x, y, w, h);
      annotCtx.strokeStyle = 'rgba(255, 255, 255, 0.6)';
      annotCtx.lineWidth = 1;
      annotCtx.setLineDash([4, 3]);
      annotCtx.strokeRect(x, y, w, h);
      annotCtx.setLineDash([]);
      break;
    }
  }
  annotCtx.restore();
}

/** 全量重绘标注层（已提交的命令）
 *  0.11.8-e：走引擎的 renderCommandsTo 保证 highlight-multiply 同色不加深。 */
export function redrawAnnotFull() {
  const { selCss, annotCanvas, annotCtx } = ss;
  if (!selCss || annotCanvas.width === 0) return;
  annotCtx.clearRect(0, 0, annotCanvas.width, annotCanvas.height);
  annot.renderCommandsTo(annot.getCommands(), annotCtx, annotCanvas.width, annotCanvas.height);
}
