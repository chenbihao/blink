//! 截图 overlay 绘制模块（0.14.6 §4 拆分）。
//!
//! 从 chord-screenshot.js 提取的绘制函数：
//! - drawDimmed：暗色蒙版（初始态 + 无选区时）
//! - drawSelection：选区拖拽中的实时绘制
//! - drawFinalSelection：选区确定后的静态绘制
//! - redrawAnnotPreview：标注实时预览
//! - redrawAnnotFull：全量重绘标注层

import { ss, TOOL_CAPS } from './ss-state.js';
import { norm } from './ss-utils.js';
import * as annot from './annotation-engine.js';
import { cssToScreen, formatSelectionInfo } from './ss-selection-geometry.js';

/** 暗色蒙版（初始态 + 无选区时） */
export function drawDimmed() {
  try {
    const { ctx, canvas, screenshot } = ss;
    if (!screenshot || !ctx || !canvas) {
      console.warn('[screenshot] drawDimmed: missing prerequisites', {
        hasScreenshot: !!screenshot, hasCtx: !!ctx, canvasW: canvas?.width,
      });
      return;
    }
    ctx.clearRect(0, 0, canvas.width, canvas.height);
    ctx.drawImage(screenshot, 0, 0);
    ctx.fillStyle = 'rgba(0, 0, 0, 0.45)';
    ctx.fillRect(0, 0, canvas.width, canvas.height);
  } catch (e) {
    console.error('[screenshot] drawDimmed threw', e);
  }
}

/** 选区绘制：选区外暗 + 选区内亮 */
export function drawSelection() {
  const { ctx, canvas, screenshot, startX, startY, endX, endY, sizeHint } = ss;
  // C 类：主 canvas bitmap 映射，必须用 overlay dpr（bitmap↔CSS 映射全局固定）
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

  // 选区边框（拖拽预览：实线，与智能预选的虚线区分）
  ctx.strokeStyle = '#4a9eff';
  ctx.lineWidth = 2 * dpr;
  ctx.strokeRect(px, py, pw, ph);

  // size-hint 显示物理像素尺寸 + 坐标（0.15.8 R0：统一用 formatSelectionInfo）
  // A 类：屏幕坐标换算，0.18.8 per-monitor（不再传 dpr）
  const meta = window.__blinkScreenMeta || { vx: 0, vy: 0 };
  const screenPos = cssToScreen(r.x, r.y, meta);
  sizeHint.textContent = formatSelectionInfo(screenPos.x, screenPos.y, pw, ph);
  sizeHint.classList.remove('hidden');
  sizeHint.style.left = (r.x + 4) + 'px';
  sizeHint.style.top = (r.y > 24 ? r.y - 22 : r.y + 4) + 'px';
}

/** 确定选区后的静态绘制（选区不再随鼠标变化，但仍需要蒙版效果） */
export function drawFinalSelection() {
  const { ctx, canvas, screenshot, selCss } = ss;
  if (!selCss) return;
  // C 类：主 canvas bitmap 映射，必须用 overlay dpr（bitmap↔CSS 映射全局固定）
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

  // size-hint：选区确定后也需显示尺寸+坐标（与拖拽阶段一致）。
  // 修复智能选区（snap）后 sizeHint 不显示的问题——snap 路径不经过 drawSelection，
  // 需要在此统一补显。
  const meta = window.__blinkScreenMeta || { vx: 0, vy: 0 };
  // A 类：屏幕坐标换算，0.18.8 per-monitor（不再传 dpr）
  const screenPos = cssToScreen(selCss.x, selCss.y, meta);
  if (ss.sizeHint) {
    ss.sizeHint.textContent = formatSelectionInfo(screenPos.x, screenPos.y, pw, ph);
    ss.sizeHint.classList.remove('hidden');
    ss.sizeHint.style.left = (selCss.x + 4) + 'px';
    ss.sizeHint.style.top = (selCss.y > 24 ? selCss.y - 22 : selCss.y + 4) + 'px';
  }

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

/**
 * 绘制流式画笔的连续圆角预览。
 *
 * 整条轨迹只提交一次 stroke，避免半透明笔刷逐点画圆、逐段描边时在采样点
 * 重复叠色，形成一串可见的圆圈。单点点击仍显示一个圆形笔触。
 */
function drawContinuousBrushPreview(c, points, width, color) {
  if (points.length === 0) return;
  c.fillStyle = color;
  c.strokeStyle = color;
  c.lineWidth = width;
  c.lineCap = 'round';
  c.lineJoin = 'round';

  if (points.length === 1) {
    c.beginPath();
    c.arc(points[0].x, points[0].y, width / 2, 0, Math.PI * 2);
    c.fill();
    return;
  }

  c.beginPath();
  c.moveTo(points[0].x, points[0].y);
  for (let i = 1; i < points.length; i++) {
    c.lineTo(points[i].x, points[i].y);
  }
  c.stroke();
}

/** 标注实时预览：重绘已提交的 + 当前绘制中的预览
 *  0.15.10：用 _committedSnapshot 快速恢复已提交内容，避免每帧全量重放。 */
export function redrawAnnotPreview() {
  const { isAnnotDragging, selCss, annotCtx, annotStartX, annotStartY, annotCurrentX, annotCurrentY, annotCanvas } = ss;
  if (!isAnnotDragging || !selCss) return;

  const tool = annot.getTool();
  // 0.15.11：聚光灯支持多次框选——预览新聚光灯时保留已提交的旧聚光灯
  if (ss._committedSnapshot) {
    // 0.15.10：快照恢复——O(1) drawImage 替代 O(n) 全量重放
    annotCtx.clearRect(0, 0, annotCanvas.width, annotCanvas.height);
    annotCtx.drawImage(ss._committedSnapshot, 0, 0);
  } else {
    redrawAnnotFull();
  }
  annotCtx.save();
  switch (tool) {
    case 'rect': {
      const x = Math.min(annotStartX, annotCurrentX);
      const y = Math.min(annotStartY, annotCurrentY);
      const w = Math.abs(annotCurrentX - annotStartX);
      const h = Math.abs(annotCurrentY - annotStartY);
      annotCtx.strokeStyle = annot.getColor();
      annotCtx.lineWidth = annot.getWidthForTool('rect');
      if (annot.getStrokeStyle() === 'dashed') annotCtx.setLineDash([8, 4]);
      annotCtx.strokeRect(x, y, w, h);
      annotCtx.setLineDash([]);
      break;
    }
    case 'ellipse': {
      const cx = (annotStartX + annotCurrentX) / 2;
      const cy = (annotStartY + annotCurrentY) / 2;
      const rx = Math.abs(annotCurrentX - annotStartX) / 2;
      const ry = Math.abs(annotCurrentY - annotStartY) / 2;
      annotCtx.strokeStyle = annot.getColor();
      annotCtx.lineWidth = annot.getWidthForTool('ellipse');
      if (annot.getStrokeStyle() === 'dashed') annotCtx.setLineDash([8, 4]);
      annotCtx.beginPath();
      annotCtx.ellipse(cx, cy, rx, ry, 0, 0, Math.PI * 2);
      annotCtx.stroke();
      annotCtx.setLineDash([]);
      break;
    }
    case 'arrow': {
      const angle = Math.atan2(annotCurrentY - annotStartY, annotCurrentX - annotStartX);
      const headLen = 12 * annot.getWidthForTool('arrow') / 2;
      annotCtx.strokeStyle = annot.getColor();
      annotCtx.lineWidth = annot.getWidthForTool('arrow');
      if (annot.getStrokeStyle() === 'dashed') annotCtx.setLineDash([8, 4]);
      annotCtx.beginPath();
      annotCtx.moveTo(annotStartX, annotStartY);
      annotCtx.lineTo(annotCurrentX, annotCurrentY);
      annotCtx.stroke();
      annotCtx.setLineDash([]);
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
        annotCtx.lineWidth = annot.getWidthForTool('pencil');
        annotCtx.lineCap = 'round';
        annotCtx.lineJoin = 'round';
        if (annot.getStrokeStyle() === 'dashed') annotCtx.setLineDash([8, 4]);
        annotCtx.beginPath();
        annotCtx.moveTo(pts[0].x, pts[0].y);
        for (let i = 1; i < pts.length; i++) {
          annotCtx.lineTo(pts[i].x, pts[i].y);
        }
        annotCtx.stroke();
        annotCtx.setLineDash([]);
      }
      break;
    }
    case 'highlight-multiply':
    case 'highlight-translucent': {
      // 0.15.8-fix：根据模式切换预览风格
      const hlMode = annot.getToolMode(tool);
      if (hlMode === 'box') {
        const bx = Math.min(annotStartX, annotCurrentX);
        const by = Math.min(annotStartY, annotCurrentY);
        const bw = Math.abs(annotCurrentX - annotStartX);
        const bh = Math.abs(annotCurrentY - annotStartY);
        const alpha = tool === 'highlight-multiply' ? 0.55 : 0.30;
        annotCtx.fillStyle = annot.withAlpha(annot.getColor(), alpha);
        annotCtx.fillRect(bx, by, bw, bh);
        annotCtx.strokeStyle = 'rgba(255, 255, 255, 0.5)';
        annotCtx.lineWidth = 1;
        annotCtx.setLineDash([4, 3]);
        annotCtx.strokeRect(bx, by, bw, bh);
        annotCtx.setLineDash([]);
        break;
      }
      const pts = annot.getCurrentPoints();
      if (pts.length >= 2) {
        const w = annot.getWidthForTool(tool);
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
      // 0.15.8-fix：根据模式切换预览风格
      const erMode = annot.getToolMode('eraser');
      if (erMode === 'box') {
        const bx = Math.min(annotStartX, annotCurrentX);
        const by = Math.min(annotStartY, annotCurrentY);
        const bw = Math.abs(annotCurrentX - annotStartX);
        const bh = Math.abs(annotCurrentY - annotStartY);
        annotCtx.fillStyle = 'rgba(255, 255, 255, 0.15)';
        annotCtx.fillRect(bx, by, bw, bh);
        annotCtx.strokeStyle = 'rgba(255, 255, 255, 0.6)';
        annotCtx.lineWidth = 1;
        annotCtx.setLineDash([4, 3]);
        annotCtx.strokeRect(bx, by, bw, bh);
        annotCtx.setLineDash([]);
        break;
      }
      const pts = annot.getCurrentPoints();
      if (pts.length >= 1) {
        const w = annot.getWidthForTool('eraser');
        const r = Math.max(6, w * 3);
        annotCtx.globalCompositeOperation = 'destination-out';
        drawContinuousBrushPreview(annotCtx, pts, r * 2, '#000');
      }
      break;
    }
    case 'mosaic': {
      // 预览使用半透明连续笔画；最终渲染以同一轨迹裁剪经典像素块马赛克。
      const mosMode = annot.getToolMode('mosaic');
      if (mosMode === 'box') {
        const bx = Math.min(annotStartX, annotCurrentX);
        const by = Math.min(annotStartY, annotCurrentY);
        const bw = Math.abs(annotCurrentX - annotStartX);
        const bh = Math.abs(annotCurrentY - annotStartY);
        annotCtx.fillStyle = 'rgba(150, 150, 150, 0.4)';
        annotCtx.fillRect(bx, by, bw, bh);
        annotCtx.strokeStyle = 'rgba(255, 255, 255, 0.6)';
        annotCtx.lineWidth = 1;
        annotCtx.setLineDash([4, 3]);
        annotCtx.strokeRect(bx, by, bw, bh);
        annotCtx.setLineDash([]);
        break;
      }
      // brush 模式预览：半透明灰色笔画
      const pts = annot.getCurrentPoints();
      if (pts.length >= 1) {
        const r = Math.max(8, annot.getBrushSize());
        drawContinuousBrushPreview(annotCtx, pts, r * 2, 'rgba(150, 150, 150, 0.3)');
      }
      break;
    }
    case 'pixelate': {
      // 0.15.8-fix→fix：根据模式切换预览风格
      const pixMode = annot.getToolMode('pixelate');
      if (pixMode === 'brush') {
        // 画笔模式预览：半透明连续笔画，宽度与最终马赛克遮罩一致。
        const pts = annot.getCurrentPoints();
        if (pts.length >= 1) {
          const r = Math.max(8, annot.getBrushSize());
          drawContinuousBrushPreview(annotCtx, pts, r * 2, 'rgba(150, 150, 150, 0.3)');
        }
        break;
      }
      // box 模式（默认）
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
    case 'blur': {
      // 0.15.3：高斯模糊预览。brush 模式 = 连续圆角笔画；box 模式 = 半透明灰框。
      const blurMode = annot.getToolMode('blur');
      if (blurMode === 'brush') {
        const pts = annot.getCurrentPoints();
        if (pts.length >= 1) {
          const r = Math.max(8, annot.getBrushSize());
          drawContinuousBrushPreview(annotCtx, pts, r * 2, 'rgba(150, 150, 150, 0.3)');
        }
      } else {
        const bx = Math.min(annotStartX, annotCurrentX);
        const by = Math.min(annotStartY, annotCurrentY);
        const bw = Math.abs(annotCurrentX - annotStartX);
        const bh = Math.abs(annotCurrentY - annotStartY);
        annotCtx.fillStyle = 'rgba(150, 150, 150, 0.4)';
        annotCtx.fillRect(bx, by, bw, bh);
        annotCtx.strokeStyle = 'rgba(255, 255, 255, 0.6)';
        annotCtx.lineWidth = 1;
        annotCtx.setLineDash([4, 3]);
        annotCtx.strokeRect(bx, by, bw, bh);
        annotCtx.setLineDash([]);
      }
      break;
    }
    case 'spotlight':
    case 'spotlight-multi': {
      // 0.15.12：聚光灯预览——四条遮罩条（与最终渲染一致）
      const sx = Math.min(annotStartX, annotCurrentX);
      const sy = Math.min(annotStartY, annotCurrentY);
      const sw = Math.abs(annotCurrentX - annotStartX);
      const sh = Math.abs(annotCurrentY - annotStartY);
      annotCtx.save();
      annotCtx.fillStyle = 'rgba(0,0,0,0.6)';
      annotCtx.fillRect(0, 0, ss.annotCanvas.width, sy);
      annotCtx.fillRect(0, sy + sh, ss.annotCanvas.width, ss.annotCanvas.height - sy - sh);
      annotCtx.fillRect(0, sy, sx, sh);
      annotCtx.fillRect(sx + sw, sy, ss.annotCanvas.width - sx - sw, sh);
      annotCtx.restore();
      break;
    }
    case 'magnifier': {
      // 0.15.9：放大镜预览——半透明矩形 + 虚线框 + 倍率提示。
      const mx = Math.min(annotStartX, annotCurrentX);
      const my = Math.min(annotStartY, annotCurrentY);
      const mw = Math.abs(annotCurrentX - annotStartX);
      const mh = Math.abs(annotCurrentY - annotStartY);
      if (mw > 4 && mh > 4) {
        const zoom = annot.getMagnifierZoom();
        annotCtx.fillStyle = 'rgba(74, 158, 255, 0.15)';
        annotCtx.fillRect(mx, my, mw, mh);
        annotCtx.strokeStyle = 'rgba(255, 255, 255, 0.7)';
        annotCtx.lineWidth = 2;
        annotCtx.setLineDash([4, 3]);
        annotCtx.strokeRect(mx, my, mw, mh);
        annotCtx.setLineDash([]);
        annotCtx.fillStyle = 'rgba(255, 255, 255, 0.8)';
        annotCtx.font = '12px sans-serif';
        annotCtx.textAlign = 'center';
        annotCtx.textBaseline = 'middle';
        annotCtx.fillText(`${zoom}×`, mx + mw / 2, my + mh / 2);
      }
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
