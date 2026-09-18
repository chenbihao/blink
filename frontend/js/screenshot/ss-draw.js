//! 截图 overlay 绘制模块（0.14.6 §4 拆分，0.20.5 分层优化，0.23.15 拖动期职责切换）。
//!
//! 分层架构：
//! - #canvas（静态层）：只在 session 初始化/来源切换时 drawImage 绘制截图原图
//! - #interaction-canvas（动态层）：**一次提交**成品——遮罩、边框、手柄
//! - #live-selection（实时层，0.23.15）：拖动期间的选区预览，由 ss-live-selection.js 独家驱动
//! - #annot-canvas（标注层）：保持独立生命周期，不合并回动态层
//!
//! 0.23.15 的关键变化：拖动期间 `#interaction-canvas` **零绘制**。
//! 逐帧的几何变化交给 DOM 实时层（成本与截图总面积无关），canvas 只承担
//! 「拖动开始时清空一次」与「松手时画回成品」两个动作。因此本模块不再提供
//! 按帧调用的选区绘制入口（旧的 drawSelection / scheduleDrawSelection /
//! scheduleDrawFinalSelection 已删除，避免重新形成两个独立实时队列）。
//!
//! 绘制函数：
//! - drawStaticBase：静态原图（初始化/来源切换/drawDimmed 时调）
//! - drawDimmed：无选区时的整屏暗罩
//! - drawFinalSelection：选区确定后的单次提交（遮罩 + 边框 + 手柄）
//! - redrawAnnotPreview / redrawAnnotFull：标注实时预览与全量重绘

import {ss} from './ss-state.js';
import * as annot from './annotation-engine.js';
import {cssPointToScreen, cssRectToBitmap, formatSelectionInfo, monitorDprAtCss} from './ss-selection-geometry.js';
import {applyFloatingUiScaleAt} from './ss-display.js';
import {hideLiveSelection} from './ss-live-selection.js';

/** 取截图底图来源：优先用 screenshotOffscreen（canvas→canvas 无解码开销） */
function getScreenshotSource() {
    return ss.screenshotOffscreen || ss.screenshot;
}

/**
 * 0.20.5：同步 interaction canvas 的 backing store 尺寸与主 canvas 一致。
 * 在主 canvas 尺寸变化后调用（loadScreenshot / resetState / enterCanvasImageEditor）。
 */
export function syncInteractionCanvasSize() {
    const {canvas, interactionCanvas} = ss;
    if (!canvas || !interactionCanvas) return;
    if (interactionCanvas.width !== canvas.width || interactionCanvas.height !== canvas.height) {
        interactionCanvas.width = canvas.width;
        interactionCanvas.height = canvas.height;
    }
}

/**
 * 0.20.5：绘制静态底图层。
 * 只在 session 初始化/来源切换时调用，不在拖拽/缩放循环中调用。
 * 主 canvas 永远只保存截图原图；所有遮罩统一画到 interaction canvas，确保选区可真正透出原色。
 */
export function drawStaticBase() {
    try {
        const {ctx, canvas, screenshot, interactionCtx} = ss;
        if (!screenshot || !ctx || !canvas) {
            console.warn('[screenshot] drawStaticBase: missing prerequisites', {
                hasScreenshot: !!screenshot, hasCtx: !!ctx, canvasW: canvas?.width,
            });
            return;
        }
        syncInteractionCanvasSize();
        ctx.clearRect(0, 0, canvas.width, canvas.height);
        ctx.drawImage(getScreenshotSource(), 0, 0);
        // 清空动态交互层
        if (interactionCtx) {
            interactionCtx.clearRect(0, 0, canvas.width, canvas.height);
        }
    } catch (e) {
        console.error('[screenshot] drawStaticBase threw', e);
    }
}

/** 暗色蒙版（初始态 + 无选区时）：原图在静态层，整屏遮罩只画到交互层。 */
export function drawDimmed() {
    // 0.23.15：canvas 提交与实时层隐藏必须同一帧，避免闪白 / 双蒙版
    hideLiveSelection();
    drawStaticBase();
    const {interactionCtx, interactionCanvas} = ss;
    if (!interactionCtx || !interactionCanvas) return;
    interactionCtx.fillStyle = 'rgba(0, 0, 0, 0.45)';
    interactionCtx.fillRect(0, 0, interactionCanvas.width, interactionCanvas.height);
}

/**
 * 确定选区后的**单次提交**（选区不再随鼠标变化，但仍需要蒙版效果）。
 *
 * 0.23.15：这是 canvas 侧唯一的选区提交入口。函数开头先隐藏实时 DOM 层，
 * 于是「DOM 隐藏 + canvas 提交」必然落在同一个 JS task、同一帧内——
 * 松手时不会出现闪白、双边框或短暂无蒙版，且自动覆盖全部退出路径
 * （正常提交 / 选区过小 / ESC / reset / blur / 会话清场）。
 *
 * 0.20.5：只绘制动态交互层，不重绘底图。拖动/缩放期间 drawImage(底图) 次数为 0。
 * 0.23.15：本函数只在拖动结束/选区确定时调用一次，不在拖动热路径中。
 */
export function drawFinalSelection() {
    hideLiveSelection();
    const {interactionCtx, interactionCanvas, selCss} = ss;
    if (!selCss || !interactionCtx || !interactionCanvas) return;
    // 选区位置和宽高来自 cssRectToBitmap（使用实测 renderScale）
    const meta = window.__blinkScreenMeta || {vx: 0, vy: 0};
    const bmp = cssRectToBitmap(selCss, meta);
    const px = bmp.x;
    const py = bmp.y;
    const pw = bmp.w;
    const ph = bmp.h;
    // 边框粗细按选区所在屏的 monitorDpr 做视觉补偿
    const targetDpr = monitorDprAtCss(selCss.x, selCss.y, meta);

    // 0.20.5：只清理动态交互层
    interactionCtx.clearRect(0, 0, interactionCanvas.width, interactionCanvas.height);

    // 蒙版
    interactionCtx.fillStyle = 'rgba(0, 0, 0, 0.45)';
    interactionCtx.fillRect(0, 0, interactionCanvas.width, py);
    interactionCtx.fillRect(0, py + ph, interactionCanvas.width, interactionCanvas.height - py - ph);
    interactionCtx.fillRect(0, py, px, ph);
    interactionCtx.fillRect(px + pw, py, interactionCanvas.width - px - pw, ph);

    // 选区边框（标注模式：实线，与拖拽虚线区分）
    interactionCtx.strokeStyle = '#4a9eff';
    interactionCtx.lineWidth = 2 * targetDpr;
    interactionCtx.strokeRect(px, py, pw, ph);

    // size-hint：选区确定后也需显示尺寸+坐标（与拖动阶段一致）。
    // 修复智能选区（snap）后 sizeHint 不显示的问题——snap 路径不经过拖动预览，
    // 需要在此统一补显。
    // 但 canvas-backed 来源（长截图/剪贴板）不显示截图坐标提示。
    if (ss.sizeHint && !ss.editorSession.canvasBacked) {
        const screenPos = cssPointToScreen(selCss.x, selCss.y, meta);
        ss.sizeHint.textContent = formatSelectionInfo(screenPos.x, screenPos.y, pw, ph);
        ss.sizeHint.classList.remove('hidden');
        applyFloatingUiScaleAt(ss.sizeHint, selCss.x, selCss.y);
        ss.sizeHint.style.left = (selCss.x + 4) + 'px';
        ss.sizeHint.style.top = (selCss.y > 24 ? selCss.y - 22 : selCss.y + 4) + 'px';
    }

    // 选取工具显示八个调整手柄，明确提示选区可移动/缩放。
    if (annot.getTool() === 'select') {
        // 手柄物理大小按目标屏 monitorDpr 决定
        const hs = 6 * targetDpr;
        const points = [
            [px, py], [px + pw / 2, py], [px + pw, py],
            [px + pw, py + ph / 2], [px + pw, py + ph],
            [px + pw / 2, py + ph], [px, py + ph], [px, py + ph / 2],
        ];
        interactionCtx.fillStyle = '#ffffff';
        interactionCtx.strokeStyle = '#4a9eff';
        interactionCtx.lineWidth = Math.max(1, targetDpr);
        for (const [hx, hy] of points) {
            interactionCtx.fillRect(hx - hs / 2, hy - hs / 2, hs, hs);
            interactionCtx.strokeRect(hx - hs / 2, hy - hs / 2, hs, hs);
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
    const {isAnnotDragging, selCss, annotCtx, annotStartX, annotStartY, annotCurrentX, annotCurrentY, annotCanvas} = ss;
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
    const {selCss, annotCanvas, annotCtx} = ss;
    if (!selCss || annotCanvas.width === 0) return;
    annotCtx.clearRect(0, 0, annotCanvas.width, annotCanvas.height);
    annot.renderCommandsTo(annot.getCommands(), annotCtx, annotCanvas.width, annotCanvas.height);
}
