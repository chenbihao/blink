//! 截图 overlay 主逻辑（0.11.7-a）：选区 + 工具栏 + 标注模式切换 + 合成输出。
//!
//! 架构：
//! - 主 canvas #canvas：全屏截图 + 暗色蒙版 + 亮区（选区）
//! - 标注 canvas #annot-canvas：位置由 JS 动态设置为选区区域，画标注
//! - 工具栏 #toolbar：HTML 元素，选区完成后显示
//!
//! 坐标约定：
//! - canvas 内部像素 = 物理像素（BitBlt 输出）
//! - canvas CSS 尺寸 = 视口大小（CSS 像素）
//! - DPR = 物理像素 / CSS 像素
//! - 鼠标事件 offsetX/Y = CSS 像素
//! - 选区 selCss 存 CSS 像素；annot-canvas 内部像素 = 物理像素
//! - 标注坐标使用物理像素相对裁剪区

import {
  screenshotCopy,
  screenshotCopyRegion,
  screenshotPin,
  screenshotSave,
  screenshotCancel,
  screenshotSetAnnotationMode,
  hideScreenshotOverlay,
  ocrImage,
  frontendLog,
} from "./api.js";
import * as annot from "./annotation-engine.js";
import { ensureSpriteLoaded } from "./icon.js";

// ── **临时**（0.11.7-f 调试用）：console 转发到后端 tracing ────────────
// TODO(0.11.7 收尾)：0.11.7 稳定后移除此块 + api.js 的 frontendLog + Rust 端 frontend_log command
// 保留原生 console 打印 + 同时转发到后端
{
  const wrap = (level) => {
    const orig = console[level].bind(console);
    return (...args) => {
      orig(...args);
      try {
        const msg = args
          .map((a) => {
            if (typeof a === 'string') return a;
            if (a instanceof Error) return `${a.name}: ${a.message}\n${a.stack || ''}`;
            try { return JSON.stringify(a); } catch { return String(a); }
          })
          .join(' ');
        frontendLog(level, msg);
      } catch (_) { /* fail-silent */ }
    };
  };
  console.error = wrap('error');
  console.warn = wrap('warn');
  console.info = wrap('info');
  console.debug = wrap('debug');
  // window.onerror 兜底
  window.addEventListener('error', (e) => {
    frontendLog('error', `window.onerror: ${e.message} @ ${e.filename}:${e.lineno}:${e.colno}`);
  });
  window.addEventListener('unhandledrejection', (e) => {
    frontendLog('error', `unhandledrejection: ${e.reason && e.reason.stack ? e.reason.stack : e.reason}`);
  });
}

// ── DOM 引用 ──────────────────────────────────────────

const canvas = document.getElementById('canvas');
const ctx = canvas.getContext('2d');
const annotCanvas = document.getElementById('annot-canvas');
const annotCtx = annotCanvas.getContext('2d');
const toolbar = document.getElementById('toolbar');
const sizeHint = document.getElementById('size-hint');
const errorHint = document.getElementById('error-hint');
const strokeCursor = document.getElementById('stroke-cursor');

// ── 状态 ──────────────────────────────────────────────

let screenshot = null;          // 全屏截图 Image
let startX = 0, startY = 0;     // 拖拽起点（CSS 像素）
let endX = 0, endY = 0;         // 拖拽终点（CSS 像素）
let isDragging = false;         // 是否正在选区拖拽
let isAnnotDragging = false;    // 是否正在标注绘制拖拽
let selCss = null;              // 选区 CSS 像素 {x, y, w, h}
let isAnnotating = false;       // 是否在标注模式（选区已确定）
let sent = false;               // 防止重复提交
let singleClickTimeout = null;  // 单击→200ms 后隐藏的定时器
let blurGuard = false;          // blur 事件短窗口防抖（避免重复触发）
// 标注绘制状态（物理像素）
let annotStartX = 0, annotStartY = 0;
let annotCurrentX = 0, annotCurrentY = 0;

// ── 初始化 ────────────────────────────────────────────

// 图标 sprite（工具栏按钮走 Lucide 图标；fire-and-forget，加载失败降级为空图标）
ensureSpriteLoaded();

annot.init(annotCanvas);

// 0.11.7-f：预热窗口（URL 带 ?preheat=1）跳过 loadScreenshot——SESSION 还没建立，
// 加载必失败留下 error-hint 状态污染。等用户 Alt+A 触发 __blinkReloadScreenshot 再加载。
const isPreheat = new URLSearchParams(window.location.search).get('preheat') === '1';
if (!isPreheat) {
  loadScreenshot();
}

// 后端复用窗口时调用此函数触发重新加载（每次 Alt+A show_screenshot_overlay 时都调）
window.__blinkReloadScreenshot = function () {
  resetState();
  loadScreenshot();
};

/** 完全重置前端状态——每次 overlay 显示时都要走一遍，防止上次残留 */
function resetState() {
  isDragging = false;
  isAnnotDragging = false;
  isAnnotating = false;
  sent = false;
  selCss = null;
  screenshot = null;
  if (singleClickTimeout) { clearTimeout(singleClickTimeout); singleClickTimeout = null; }
  // UI 状态清空
  sizeHint.style.display = 'none';
  toolbar.style.display = 'none';
  annotCanvas.style.display = 'none';
  errorHint.style.display = 'none';
  errorHint.textContent = '';
  // 清 canvas 内容（防止上次的选区框/画面残留）
  if (canvas.width > 0) {
    ctx.clearRect(0, 0, canvas.width, canvas.height);
  }
  // 清标注 canvas（防止上次标注残留）
  if (annotCanvas.width > 0) {
    annotCtx.clearRect(0, 0, annotCanvas.width, annotCanvas.height);
  }
  annotCanvas.width = 0;
  annotCanvas.height = 0;
  // 通知后端清除标注模式
  screenshotSetAnnotationMode(false).catch((e) => console.error('setAnnotationMode(false) 失败', e));
  // 清 OCR 面板（如果上次遗留）
  const oldOcr = document.getElementById('ocr-panel');
  if (oldOcr) oldOcr.remove();
  // 0.11.8-c：水印表单已内嵌进 text-dropdown（视图切回列表即可）
  const textDropdown = document.getElementById('text-dropdown');
  if (textDropdown) {
    textDropdown.setAttribute('data-view', 'list');
    textDropdown.setAttribute('data-open', 'false');
  }
  // 清工具栏用户拖动位置（新一轮截图重回自动定位）
  toolbar.removeAttribute('data-user-moved');
  toolbar.style.left = '';
  toolbar.style.top = '';
  // 清取消门禁（防止快速 Alt+A → ESC → Alt+A → ESC 被卡）
  cancelInProgress = false;
}

function loadScreenshot() {
  errorHint.style.display = 'none';
  const img = new Image();
  // 必须设 crossOrigin 否则 canvas 被跨域截图污染，getImageData/toBlob 全不可用
  img.crossOrigin = 'anonymous';
  img.onload = () => {
    screenshot = img;
    canvas.width = img.width;
    canvas.height = img.height;
    drawDimmed();
  };
  img.onerror = (e) => {
    console.error('截图加载失败', e);
    errorHint.textContent = '截图加载失败，按 ESC 关闭';
    errorHint.style.display = 'block';
  };
  img.src = 'http://blink-screenshot.localhost/capture?t=' + Date.now();
}

// ── 绘制 ──────────────────────────────────────────────

/** 暗色蒙版（初始态 + 无选区时） */
function drawDimmed() {
  ctx.clearRect(0, 0, canvas.width, canvas.height);
  ctx.drawImage(screenshot, 0, 0);
  ctx.fillStyle = 'rgba(0, 0, 0, 0.45)';
  ctx.fillRect(0, 0, canvas.width, canvas.height);
}

/** 选区绘制：选区外暗 + 选区内亮 */
function drawSelection() {
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
  sizeHint.style.display = 'block';
  sizeHint.style.left = (r.x + 4) + 'px';
  sizeHint.style.top = (r.y > 24 ? r.y - 22 : r.y + 4) + 'px';
}

/** 确定选区后的静态绘制（选区不再随鼠标变化，但仍需要蒙版效果） */
function drawFinalSelection() {
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
}

// ── 标注预览/重绘 ─────────────────────────────────────

/** 标注实时预览：重绘已提交的 + 当前绘制中的预览 */
function redrawAnnotPreview() {
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
      // 0.11.8-d：荧光笔实时预览。粗细 × 4，alpha 与最终一致。
      // multiply 模式的"重叠不加深"效果由 endDraw 时的整段一次性 stroke 实现，
      // 预览阶段（单笔仍在画）不需要特殊处理——预览也是单条 stroke，天然不加深。
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
      // 0.11.8-a：橡皮擦沿轨迹用 destination-out 圆形擦除；预览与最终一致，
      // 用户拖动时实时看到已擦区域露出下方蒙版（视觉上"被擦掉"）。
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
      // 涂抹预览：实时显示笔触（与最终 executeCommand 一致）
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
      // 马赛克预览：半透明灰色矩形 + 虚线边框（松手才真正像素化，省性能）
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
function redrawAnnotFull() {
  if (!selCss || annotCanvas.width === 0) return;
  annotCtx.clearRect(0, 0, annotCanvas.width, annotCanvas.height);
  annot.renderCommandsTo(annot.getCommands(), annotCtx, annotCanvas.width, annotCanvas.height);
}

// ── 选区与标注模式 ────────────────────────────────────

/** 进入标注模式：显示工具栏 + 定位标注 canvas + 通知后端 */
function enterAnnotationMode(rect) {
  console.debug('[screenshot] enterAnnotationMode', rect);

  selCss = rect;
  isAnnotating = true;
  sent = false;

  const dpr = window.devicePixelRatio || 1;

  // 定位标注 canvas（CSS 定位 + 内部像素 = 物理像素）
  annotCanvas.style.display = 'block';
  annotCanvas.style.left = rect.x + 'px';
  annotCanvas.style.top = rect.y + 'px';
  annotCanvas.style.width = rect.w + 'px';
  annotCanvas.style.height = rect.h + 'px';
  const pw = Math.max(1, Math.round(rect.w * dpr));
  const ph = Math.max(1, Math.round(rect.h * dpr));

  // 提取裁剪区**原始像素**（不含蒙版/边框）——直接从原图 screenshot 拷贝到临时 canvas
  // 用 canvas 而非 ctx.getImageData 是因为 ctx 上已经画了蒙版/边框，会污染马赛克源数据
  let cropData = null;
  try {
    const tempCanvas = document.createElement('canvas');
    tempCanvas.width = pw;
    tempCanvas.height = ph;
    const tempCtx = tempCanvas.getContext('2d');
    tempCtx.drawImage(
      screenshot,
      Math.round(rect.x * dpr),
      Math.round(rect.y * dpr),
      pw,
      ph,
      0,
      0,
      pw,
      ph
    );
    cropData = tempCtx.getImageData(0, 0, pw, ph);
  } catch (e) {
    console.warn('[screenshot] 提取裁剪区图像失败（马赛克功能不可用）', e);
  }

  // 重置标注引擎（清空命令栈、设 canvas 尺寸、存 cropData）
  annot.reset(pw, ph, cropData);
  updateUndoRedoButtons();

  // 通知后端进入标注模式
  screenshotSetAnnotationMode(true).catch((e) => console.error('setAnnotationMode(true) 失败', e));

  // 静态绘制选区（不再随鼠标移动）
  drawFinalSelection();

  // 显示 + 定位工具栏（放到 drawFinalSelection 后，确保 canvas 已画好）
  positionToolbar(rect);
}

/** 退出标注模式（清除选区，回到可拖选状态） */
function exitAnnotationMode() {
  console.debug('[screenshot] exitAnnotationMode');
  isAnnotating = false;
  selCss = null;
  annotCanvas.style.display = 'none';
  annotCanvas.width = 0;
  annotCanvas.height = 0;
  toolbar.style.display = 'none';
  sizeHint.style.display = 'none';
  // 0.11.8-a：清工具栏用户拖动位置——新选区回到自动定位（否则工具栏悬在旧位置与新选区脱节）
  toolbar.removeAttribute('data-user-moved');
  toolbar.style.left = '';
  toolbar.style.top = '';
  screenshotSetAnnotationMode(false).catch((e) => console.error('setAnnotationMode(false) 失败', e));
  drawDimmed();
}

/** 定位工具栏到选区右下外侧（PixPin 风格）。
 *  0.11.8-a：若用户已手动拖过工具栏（dataset.userMoved），保留用户位置不重定位。 */
function positionToolbar(rect) {
  toolbar.style.display = 'flex';
  // 用户已拖过 → 保留位置，仅确保 display:flex 即可
  if (toolbar.dataset.userMoved === 'true' && toolbar.style.left && toolbar.style.top) {
    return;
  }
  // 先给临时位置让 layout 生效（避免闪一下屏幕左上角）
  toolbar.style.left = '-9999px';
  toolbar.style.top = '-9999px';

  requestAnimationFrame(() => {
    const tw = toolbar.offsetWidth;
    const th = toolbar.offsetHeight;
    const vw = window.innerWidth;
    const vh = window.innerHeight;

    // 右对齐选区右边缘
    let left = rect.x + rect.w - tw;
    if (left + tw > vw - 8) left = vw - tw - 8;
    if (left < 8) left = 8;

    // 位于选区下方 8px
    let top = rect.y + rect.h + 8;
    // 底部空间不足 → 翻转到选区上方
    if (top + th > vh - 8) {
      top = rect.y - th - 8;
    }
    // 上方也不够 → 强制在选区内右下角（贴内边）
    if (top < 8) {
      top = Math.max(8, vh - th - 8);
    }

    toolbar.style.left = left + 'px';
    toolbar.style.top = top + 'px';
    console.debug('[screenshot] toolbar 定位', { left, top, tw, th, rect });
  });
}

// ── 鼠标事件 ──────────────────────────────────────────

canvas.addEventListener('mousedown', (e) => {
  if (!screenshot || e.button !== 0) return;

  // 有选区状态下点击选区内 → 启动标注绘制
  if (isAnnotating && selCss && pointInRect(e.offsetX, e.offsetY, selCss)) {
    // 0.11.8-b：watermark 是"面板驱动"工具，不响应 canvas 拖动
    if (annot.getTool() === 'watermark') return;
    const dpr = window.devicePixelRatio || 1;
    annotStartX = (e.offsetX - selCss.x) * dpr;
    annotStartY = (e.offsetY - selCss.y) * dpr;
    annotCurrentX = annotStartX;
    annotCurrentY = annotStartY;
    annot.startDraw(annotStartX, annotStartY);
    isAnnotDragging = true;
    return;
  }

  // 有选区但点击选区外 → 清除选区，同时用当前点作为新拖选的起点
  if (isAnnotating && selCss) {
    exitAnnotationMode();
  }

  // 启动选区拖拽
  isDragging = true;
  sent = false;
  startX = e.offsetX;
  startY = e.offsetY;
  endX = startX;
  endY = startY;
});

canvas.addEventListener('mousemove', (e) => {
  if (!screenshot) return;

  // 更新笔画预览虚圈（0.11.8-c）：hover 时显示当前工具的笔尖大小
  updateStrokeCursor(e.clientX, e.clientY);

  // 标注绘制中
  if (isAnnotDragging && selCss) {
    const dpr = window.devicePixelRatio || 1;
    annotCurrentX = (e.offsetX - selCss.x) * dpr;
    annotCurrentY = (e.offsetY - selCss.y) * dpr;
    // 0.11.8-e：矩形/椭圆按住 Shift 约束长宽等比（→ 正方形 / 圆）
    if (e.shiftKey) {
      const constrained = applySquareConstraint(annotStartX, annotStartY, annotCurrentX, annotCurrentY);
      if (constrained) { annotCurrentX = constrained.x; annotCurrentY = constrained.y; }
    }
    annot.moveDraw(annotCurrentX, annotCurrentY);
    redrawAnnotPreview();
    return;
  }

  // 选区拖拽中
  if (isDragging) {
    endX = e.offsetX;
    endY = e.offsetY;
    drawSelection();
  }
});

// 鼠标离开 canvas 时隐藏预览圈
canvas.addEventListener('mouseleave', () => {
  if (strokeCursor) strokeCursor.style.display = 'none';
});

// 0.11.8-e：矩形/椭圆拖动期间按/松 Shift 实时更新预览（否则鼠标不动则不重绘）
// 用最后一次 mousemove 的坐标（保存在 annotCurrentX/Y）重算，若 tool 是 rect/ellipse
// 且正在拖动，Shift 状态变化就重新 clamp + redrawAnnotPreview。
function refreshShapePreviewOnShift(e) {
  if (!isAnnotDragging || !selCss) return;
  const tool = annot.getTool();
  if (tool !== 'rect' && tool !== 'ellipse') return;
  // 用 mousemove 里记录的鼠标原始位置反算——但我们没保存"原始未 clamp 的鼠标位置"。
  // 折衷：仅在 Shift keydown 时 clamp（annotCurrentX/Y 已是上次 mousemove 值），
  // Shift keyup 时无法恢复到"真实鼠标位置"——所以放弃 keyup 实时恢复；用户拖动一下
  // 鼠标即可看到最新几何。这个折衷在实际使用里几乎无感，代价是不引入新的坐标存储。
  if (e.type === 'keydown' && e.key === 'Shift') {
    const constrained = applySquareConstraint(annotStartX, annotStartY, annotCurrentX, annotCurrentY);
    if (constrained) { annotCurrentX = constrained.x; annotCurrentY = constrained.y; }
    redrawAnnotPreview();
  }
}
window.addEventListener('keydown', refreshShapePreviewOnShift);

/** 更新笔画预览虚圈位置和大小。CSS 像素坐标（clientX/Y）。
 *  仅在标注模式 + 笔画类工具 hover 选区内时显示；其它情况隐藏。 */
function updateStrokeCursor(clientX, clientY) {
  if (!strokeCursor) return;
  // 只在有选区且鼠标在选区内时显示
  if (!isAnnotating || !selCss) { strokeCursor.style.display = 'none'; return; }
  // 拖拽绘制中不显示（否则和实时预览重叠）
  if (isAnnotDragging) { strokeCursor.style.display = 'none'; return; }
  // 鼠标是否在选区内
  // clientX/Y 与 canvas offsetX/Y 差异是 canvas 相对视口的偏移——canvas 是 100%×100%，
  // 位于 (0,0)，所以 clientX 直接可用比较 selCss（CSS 像素）。
  if (clientX < selCss.x || clientX > selCss.x + selCss.w ||
      clientY < selCss.y || clientY > selCss.y + selCss.h) {
    strokeCursor.style.display = 'none';
    return;
  }
  // 计算 CSS 直径 = 工具的物理笔尖直径 / dpr
  const tool = annot.getTool();
  const w = annot.getWidth();
  const dpr = window.devicePixelRatio || 1;
  let cssPxDiameter = 0;
  if (tool === 'pencil') {
    cssPxDiameter = w / dpr;
  } else if (tool === 'highlight-multiply' || tool === 'highlight-translucent') {
    cssPxDiameter = (w * 4) / dpr;
  } else if (tool === 'eraser') {
    // 引擎里橡皮擦半径 = max(6, w*3)，直径 = 2r
    cssPxDiameter = (Math.max(6, w * 3) * 2) / dpr;
  } else {
    // 其它工具（矩形/椭圆/箭头/文字/水印/马赛克…）不显示预览圈
    strokeCursor.style.display = 'none';
    return;
  }
  // 直径 < 4px 太小意义不大，直接隐藏
  if (cssPxDiameter < 4) { strokeCursor.style.display = 'none'; return; }
  strokeCursor.style.display = 'block';
  strokeCursor.style.width = cssPxDiameter + 'px';
  strokeCursor.style.height = cssPxDiameter + 'px';
  strokeCursor.style.left = (clientX - cssPxDiameter / 2) + 'px';
  strokeCursor.style.top = (clientY - cssPxDiameter / 2) + 'px';
  // 荧光笔用颜色暗示叠加感；橡皮擦用白色描边
  if (tool === 'eraser') {
    strokeCursor.style.borderColor = 'rgba(255,255,255,0.9)';
  } else {
    strokeCursor.style.borderColor = annot.getColor();
  }
}

canvas.addEventListener('mouseup', (e) => {
  if (!screenshot) return;

  // 标注绘制结束
  if (isAnnotDragging) {
    isAnnotDragging = false;
    const dpr = window.devicePixelRatio || 1;
    annotCurrentX = (e.offsetX - selCss.x) * dpr;
    annotCurrentY = (e.offsetY - selCss.y) * dpr;
    // 0.11.8-e：矩形/椭圆按住 Shift 约束长宽等比
    if (e.shiftKey) {
      const constrained = applySquareConstraint(annotStartX, annotStartY, annotCurrentX, annotCurrentY);
      if (constrained) { annotCurrentX = constrained.x; annotCurrentY = constrained.y; }
    }

    const tool = annot.getTool();
    // 文本工具：允许零拖拽（点击一次就弹输入框）
    // 铅笔/橡皮擦/涂抹：只要有轨迹点就生成（startDraw 已有 1 个点，单击也能产生效果）
    // 矩形/椭圆/箭头：需要 >=3px 拖动（避免误触产生 1px 矩形）
    const dx = annotCurrentX - annotStartX;
    const dy = annotCurrentY - annotStartY;
    const minDrag = (tool === 'text' || tool === 'eraser' || tool === 'pencil' || tool === 'mosaic') ? 0 : 3;
    if (Math.abs(dx) < minDrag && Math.abs(dy) < minDrag) {
      console.debug('[screenshot] annotation drag too small, skip', { tool, dx, dy });
      redrawAnnotFull();
      return;
    }

    const result = annot.endDraw(annotCurrentX, annotCurrentY);
    if (result && result.needsText) {
      showTextInput(result.x, result.y);
    }
    redrawAnnotFull();
    updateUndoRedoButtons();
    return;
  }

  // 选区拖拽结束
  if (!isDragging) return;
  isDragging = false;
  endX = e.offsetX;
  endY = e.offsetY;

  const rect = norm(startX, startY, endX, endY);
  // 拖动幅度 < 5px 视为"点击"——把阈值放宽（3px 用户容易误判为拖动）
  if (rect.w < 5 || rect.h < 5) {
    console.debug('[screenshot] rect too small, wait for dblclick', rect);
    // 延迟 200ms 后关闭；给双击留窗口
    if (singleClickTimeout) clearTimeout(singleClickTimeout);
    singleClickTimeout = setTimeout(() => {
      singleClickTimeout = null;
      if (!isAnnotating && !sent) {
        console.info('[screenshot] single click → hide overlay');
        hideScreenshotOverlay().catch((err) => console.error('hideScreenshotOverlay 失败', err));
      }
    }, 200);
    return;
  }

  console.info('[screenshot] selection confirmed', rect);
  // 进入标注模式
  enterAnnotationMode(rect);
});

// 双击：
// - 无选区（overlay 刚显示，用户未拖选）→ 复制全屏
// - 有选区且双击落在选区内 → 复制选区（快捷键，等价于工具栏"复制"）
// - 有选区但双击落在选区外 → 忽略（避免用户误触发全屏复制丢失当前选区）
canvas.addEventListener('dblclick', (e) => {
  console.debug('[screenshot] dblclick', { isAnnotating, hasSelCss: !!selCss, sent });

  if (!screenshot || sent) return;
  if (singleClickTimeout) { clearTimeout(singleClickTimeout); singleClickTimeout = null; }

  if (isAnnotating && selCss) {
    // 双击选区内 → 复制选区
    if (pointInRect(e.offsetX, e.offsetY, selCss)) {
      doCopySelection();
    }
    // 双击选区外：什么都不做（用户可能不小心点错，别丢选区）
    return;
  }

  // 无选区双击 → 复制全屏
  doCopyFullScreen();
});

// 右键取消
canvas.addEventListener('contextmenu', (e) => {
  e.preventDefault();
  doCancel();
});

document.addEventListener('keydown', (e) => {
  if (e.key === 'Escape') {
    e.preventDefault();
    // 优先关闭 OCR 面板
    const ocrPanel = document.getElementById('ocr-panel');
    if (ocrPanel) {
      ocrPanel.remove();
      return;
    }
    // 其次关闭水印面板（0.11.8-c：内嵌 text-dropdown 视图，回列表并关闭）
    const wmDropdown = document.getElementById('text-dropdown');
    if (wmDropdown && wmDropdown.getAttribute('data-view') === 'watermark' && wmDropdown.getAttribute('data-open') === 'true') {
      wmDropdown.setAttribute('data-view', 'list');
      wmDropdown.setAttribute('data-open', 'false');
      return;
    }
    // 其次关闭展开的下拉菜单
    const openDropdown = document.querySelector('.dropdown[data-open="true"]');
    if (openDropdown) {
      openDropdown.setAttribute('data-open', 'false');
      return;
    }
    doCancel();
  }
});

// 失焦兜底——但避免重复调用
// 注意：blur 不调 doCancel，只调 hideScreenshotOverlay（不设 sent）。
// 如果用户在有选区时失焦，应该保留选区（不取消），而不是默默地结束会话。
// 截图 overlay 是透明窗口，失焦可能只是用户不小心点到别处，不应直接取消。
window.addEventListener('blur', () => {
  if (blurGuard) return;
  blurGuard = true;
  setTimeout(() => { blurGuard = false; }, 500);
  console.debug('[screenshot] window blur, hiding overlay');
  // 直接隐藏，不经过 doCancel（不设 sent，不干扰后续操作）
  hideScreenshotOverlay().catch((e) => console.error('hideScreenshotOverlay 失败', e));
});

// ── 工具栏动作 ────────────────────────────────────────

function doCopySelection() {
  if (!selCss || sent) return;
  sent = true;
  console.info('[screenshot] copy selection', { hasAnnot: annot.hasAnnotations() });

  // 快路径：无标注 → 后端直接从 SESSION 裁剪 BGRA 写剪贴板（跳过 PNG 编解码往返）
  //   全屏 2560x1440 大约省 150-250ms（Canvas.toBlob + PNG decode 两次开销）
  // 慢路径：有标注 → 前端合成 PNG 传后端（唯一能把标注像素喂给剪贴板的路径）
  if (!annot.hasAnnotations()) {
    const dpr = window.devicePixelRatio || 1;
    const px = Math.round(selCss.x * dpr);
    const py = Math.round(selCss.y * dpr);
    const pw = Math.round(selCss.w * dpr);
    const ph = Math.round(selCss.h * dpr);
    screenshotCopyRegion(px, py, pw, ph)
      .then(() => console.info('[screenshot] copy 成功（快路径）'))
      .catch((err) => {
        console.error('[screenshot] copy 失败（快路径）', err);
        errorHint.textContent = '截图保存失败：' + err;
        errorHint.style.display = 'block';
        sent = false;
      });
    return;
  }

  compositeSelection((pngBytes) => {
    screenshotCopy(pngBytes)
      .then(() => console.info('[screenshot] copy 成功'))
      .catch((err) => {
        console.error('[screenshot] copy 失败', err);
        errorHint.textContent = '截图保存失败：' + err;
        errorHint.style.display = 'block';
        sent = false;
      });
  });
}

function doCopyFullScreen() {
  if (sent) return;
  sent = true;
  console.info('[screenshot] copy fullscreen');
  // 全屏无标注 → 快路径：直接把整个虚拟屏幕坐标传给后端（canvas.width/height 已是物理像素）
  screenshotCopyRegion(0, 0, canvas.width, canvas.height)
    .then(() => console.info('[screenshot] fullscreen copy 成功（快路径）'))
    .catch((err) => {
      console.error('[screenshot] fullscreen copy 失败', err);
      errorHint.textContent = '截图保存失败：' + err;
      errorHint.style.display = 'block';
      sent = false;
    });
}

function doPinSelection() {
  if (!selCss || sent) return;
  sent = true;
  // 计算选区左上角的屏幕物理坐标，让钉图窗口"就地贴住"截图原位
  const dpr = window.devicePixelRatio || 1;
  const meta = window.__blinkScreenMeta || { vx: 0, vy: 0 };
  const screenX = Math.round(meta.vx + selCss.x * dpr);
  const screenY = Math.round(meta.vy + selCss.y * dpr);
  compositeSelection((pngBytes) => {
    screenshotPin(pngBytes, screenX, screenY).catch((err) => {
      console.error('[screenshot] pin 失败', err);
      sent = false;
    });
  });
}

function doSaveSelection() {
  if (!selCss || sent) return;
  sent = true;
  compositeSelection((pngBytes) => {
    screenshotSave(pngBytes, null).catch((err) => {
      if (err !== '用户取消了保存') {
        console.error('[screenshot] save 失败', err);
      }
      sent = false;
    });
  });
}

function doOcrSelection() {
  if (!selCss || sent) return;
  sent = true; // 占位：防止 Copy/Ocr 并发
  compositeSelection((pngBytes) => {
    ocrImage(pngBytes)
      .then((result) => showOcrResult(result))
      .catch((err) => console.error('[screenshot] ocr 失败', err))
      .finally(() => { sent = false; }); // OCR 不关 overlay，释放占位
  });
}

let cancelInProgress = false;
function doCancel() {
  if (cancelInProgress) return;
  cancelInProgress = true;
  setTimeout(() => { cancelInProgress = false; }, 2000);
  console.info('[screenshot] cancel');
  if (isAnnotating) {
    screenshotCancel().catch((e) => console.error('screenshotCancel 失败', e));
  } else {
    hideScreenshotOverlay().catch((e) => console.error('hideScreenshotOverlay 失败', e));
  }
}

// ── OCR 结果面板 ─────────────────────────────────────

function showOcrResult(result) {
  const old = document.getElementById('ocr-panel');
  if (old) old.remove();

  const text = (result && result.text) || '';

  const panel = document.createElement('div');
  panel.id = 'ocr-panel';
  panel.className = 'ocr-panel';
  panel.innerHTML = `
    <div class="ocr-panel-header">
      <span>OCR 识别结果（可编辑）</span>
      <button id="ocr-close" class="tool-btn">✕</button>
    </div>
    <textarea class="ocr-panel-textarea" spellcheck="false"></textarea>
    <div class="ocr-panel-footer">
      <span class="ocr-panel-hint">可自由选词复制或编辑</span>
      <button id="ocr-trim-spaces" class="tool-btn" ${text ? '' : 'disabled'} title="合并连续空白 + 去除首尾空格">移除空格</button>
      <button id="ocr-copy" class="tool-btn tool-btn-primary" ${text ? '' : 'disabled'}>复制全部</button>
    </div>
  `;
  document.body.appendChild(panel);

  const textarea = panel.querySelector('.ocr-panel-textarea');
  textarea.value = text || '（未识别到文字）';

  panel.style.left = Math.max(8, selCss.x) + 'px';
  // 默认放在选区上方；如果放不下则翻到选区下方
  const topAbove = selCss.y - panel.offsetHeight - 8;
  if (topAbove < 8) {
    panel.style.top = (selCss.y + selCss.h + 8) + 'px';
  } else {
    panel.style.top = topAbove + 'px';
  }

  // 面板内交互不应触发 overlay 的 blur 隐藏
  panel.addEventListener('mousedown', (e) => e.stopPropagation());

  document.getElementById('ocr-close').addEventListener('click', () => panel.remove());

  // 0.11.8：移除空格 toggle —— OCR 引擎常在**汉字之间**夹空格（"然 正 确"），
  // 用户按一次全清，再按一次回原文，方便对比 / 校正。
  //
  // 设计要点：
  // - 每次都从 `originalText`（当前"原文态"）派生结果，避免累积失真
  // - 用 `[^\S\r\n]+`（所有 Unicode 空白但换行除外）而非 `[ \t　]+`——OCR 输出常混入
  //   U+00A0 不间断空格、U+2000~U+200B 各种半宽/零宽空格，`\s` + 保留换行是最稳的实用集
  // - 保留换行结构，防止段落被压成一行
  // - 状态用按钮 dataset 记，避免闭包状态污染
  // - 中文场景 replace 用 '' 而不是 ' '——汉字之间不需要词间空格；英文段落偶尔失去
  //   词间空格是可接受的取舍（用户可手动 undo 或再按一次显示原文）
  const trimBtn = document.getElementById('ocr-trim-spaces');
  let originalText = textarea.value;
  trimBtn.dataset.trimmed = 'false';
  // 用户手动编辑 textarea → 视作新的"原文态"，重置 toggle
  textarea.addEventListener('input', () => {
    originalText = textarea.value;
    if (trimBtn.dataset.trimmed === 'true') {
      trimBtn.dataset.trimmed = 'false';
      trimBtn.textContent = '移除空格';
    }
  });
  trimBtn.addEventListener('click', () => {
    if (trimBtn.dataset.trimmed === 'true') {
      // 当前是 trim 后 → 还原为原文（不触发 input 监听的重置逻辑：先改 dataset 再改 value）
      trimBtn.dataset.trimmed = 'false';
      trimBtn.textContent = '移除空格';
      // 用 setter 会触发 input 事件——但监听里 `originalText = textarea.value` 会把
      // 原文赋回原文，dataset 已是 false，textContent 已是"移除空格"，无副作用
      textarea.value = originalText;
    } else {
      // 当前是原文 → 移除空格
      const trimmed = originalText
        .split(/\r?\n/)
        .map((line) => line.replace(/[^\S\r\n]+/g, '').trim())
        .join('\n')
        .replace(/\n{3,}/g, '\n\n'); // 连续 3+ 空行压成 2 行（保留段落分隔）
      // 先设 dataset/文本，再赋值——避免 input 监听把 trimmed 当新原文
      trimBtn.dataset.trimmed = 'true';
      trimBtn.textContent = '显示原文';
      textarea.value = trimmed;
    }
    textarea.focus();
  });

  document.getElementById('ocr-copy').addEventListener('click', () => {
    const value = textarea.value;
    if (value) {
      navigator.clipboard.writeText(value).catch((e) => console.error('复制失败', e));
    }
    panel.remove();
  });
}

// ── 水印表单（0.11.8-c：内嵌进 text-dropdown，就地展开）──────────────
// 与颜色下拉一致的交互：点"水印" → dropdown 从列表视图切到表单视图；
// 应用 / 返回 → 切回列表视图 + 关闭 dropdown。
//
// 表单元素在 HTML 里静态存在，此函数只负责视图切换 + 事件绑定（幂等）。

let watermarkFormBound = false;

function openWatermarkForm() {
  const dropdown = document.getElementById('text-dropdown');
  if (!dropdown) return;

  // 视图切表单，且保持 dropdown 打开（selectTool 已 closeAllDropdowns，这里回开）
  dropdown.setAttribute('data-view', 'watermark');
  dropdown.setAttribute('data-open', 'true');

  const textInput = dropdown.querySelector('.wm-text');
  const layoutSelect = dropdown.querySelector('.wm-layout');
  const opacityRange = dropdown.querySelector('.wm-opacity');
  const opacityVal = dropdown.querySelector('.wm-opacity-val');

  // 每次打开时把光标放输入框
  if (textInput) setTimeout(() => textInput.focus(), 0);

  if (watermarkFormBound) return;
  watermarkFormBound = true;

  const backToList = () => {
    dropdown.setAttribute('data-view', 'list');
  };

  if (opacityRange && opacityVal) {
    opacityRange.addEventListener('input', () => {
      opacityVal.textContent = `${opacityRange.value}%`;
    });
  }

  const applyBtn = dropdown.querySelector('.wm-apply');
  const backBtn = dropdown.querySelector('.wm-back');
  const apply = () => {
    const text = textInput.value.trim();
    if (!text) { textInput.focus(); return; }
    annot.commitWatermark({
      text,
      layout: layoutSelect.value,
      color: annot.getColor(),
      width: annot.getWidth(),
      opacity: parseInt(opacityRange.value, 10) / 100,
    });
    redrawAnnotFull();
    updateUndoRedoButtons();
    backToList();
    dropdown.setAttribute('data-open', 'false');
  };
  if (applyBtn) applyBtn.addEventListener('click', apply);
  if (backBtn) backBtn.addEventListener('click', (e) => {
    e.stopPropagation();
    backToList();
  });
  // Enter 应用；ESC 让 keydown handler 关 dropdown
  if (textInput) textInput.addEventListener('keydown', (e) => {
    if (e.key === 'Enter') { e.preventDefault(); apply(); }
  });
  // 表单内所有交互不冒到 canvas mousedown（避免关闭 dropdown）
  dropdown.querySelectorAll('.dropdown-view-watermark input, .dropdown-view-watermark select, .dropdown-view-watermark button')
    .forEach((el) => el.addEventListener('mousedown', (e) => e.stopPropagation()));
}

// ── 文本标注输入框 ─────────────────────────────────
//
// 0.11.8-e：从 `<input>` 换成 `<span contenteditable>`。
//
// 根因：Chromium 的 `<input>` 是 UA 表单控件，即使 padding:0 line-height:1，
// 内部也存在 UA 定义的"控件内 leading"（几像素）——文字在容器内垂直居中而非
// 顶对齐，与 canvas `textBaseline='top'` 差几像素，视觉上文字比 canvas 预览低。
//
// `<span contenteditable>` 是普通 block 元素，line-height:1 时字符 glyph 起点
// 严格贴容器顶，配合 canvas textBaseline='top' 严格对齐。
// 副作用：不再需要"隐藏 span 测量宽度"（span 天然按内容自增）。

function showTextInput(x, y) {
  if (!selCss) return;
  const dpr = window.devicePixelRatio || 1;

  const input = document.createElement('span');
  input.className = 'text-annot-input';
  input.contentEditable = 'true';
  input.setAttribute('role', 'textbox');
  input.setAttribute('data-placeholder', '输入文本…');
  // 定位到标注 canvas 上、用户点击位置（CSS 像素）
  input.style.left = (selCss.x + x / dpr) + 'px';
  input.style.top = (selCss.y + y / dpr) + 'px';
  // 字号与最终 canvas 渲染严格对齐——
  // 引擎 fillText 用 `width * 6` 物理像素 → CSS 像素 = 物理 / dpr = `width * 6 / dpr`
  const cssFontPx = (annot.getWidth() * 6) / dpr;
  input.style.fontSize = cssFontPx + 'px';
  input.style.fontFamily = 'sans-serif';
  input.style.lineHeight = '1';
  input.style.color = annot.getColor();
  input.spellcheck = false;
  document.body.appendChild(input);
  input.focus();

  // 全选已有内容（若有），方便用户直接覆盖
  setTimeout(() => {
    const range = document.createRange();
    range.selectNodeContents(input);
    const sel = window.getSelection();
    sel.removeAllRanges();
    sel.addRange(range);
  }, 0);

  const getText = () => (input.textContent || '').trim();

  let cleanedUp = false;
  const cleanup = () => {
    if (cleanedUp) return;
    cleanedUp = true;
    if (input.parentNode) input.parentNode.removeChild(input);
  };
  const commit = (text) => {
    if (text) { annot.commitText(text); redrawAnnotFull(); updateUndoRedoButtons(); }
    else { annot.cancelText(); }
    cleanup();
  };

  input.addEventListener('keydown', (e) => {
    // IME 组合期按 Enter 是"提交候选"而非"提交文本"——跳过 commit
    if (e.isComposing || e.keyCode === 229) return;
    if (e.key === 'Enter') {
      e.preventDefault();
      commit(getText());
    } else if (e.key === 'Escape') {
      e.stopPropagation();
      annot.cancelText();
      cleanup();
    }
  });

  // blur 自动提交——用户点击工具栏或标注区时文本会被提交
  input.addEventListener('blur', () => {
    commit(getText());
  });
}

// ── 工具栏事件绑定（0.11.7 review：模块内直接绑定，替代 HTML 内联脚本 + window.__screenshot* 桥接） ─
//
// 模块作为 <script type="module"> 加载，天然 defer——DOM 已解析完成，可直接查询。
// 每次 Alt+A 复用窗口时不需要重绑（元素身份不变），初始化只跑一次。

function bindToolbar() {
  // ── 下拉菜单（0.11.8 收起化）：触发器点击切换，点外部/选另一项关闭 ──

  /** 关闭所有展开的下拉 */
  function closeAllDropdowns() {
    document.querySelectorAll('.dropdown').forEach((d) => d.setAttribute('data-open', 'false'));
  }

  /** 切换某个下拉的展开状态（打开时关掉其他） */
  function toggleDropdown(dropdown) {
    const willOpen = dropdown.getAttribute('data-open') !== 'true';
    closeAllDropdowns();
    dropdown.setAttribute('data-open', willOpen ? 'true' : 'false');
  }

  // 触发器（split-caret / 单体 dropdown-trigger 通用）
  const shapeTrigger = document.getElementById('shape-trigger');
  const strokeTrigger = document.getElementById('stroke-trigger');
  const textTrigger = document.getElementById('text-trigger');
  const blurTrigger = document.getElementById('blur-trigger');
  const colorTrigger = document.getElementById('color-trigger');
  const widthTrigger = document.getElementById('width-trigger');
  const shapeDropdown = document.getElementById('shape-dropdown');
  const strokeDropdown = document.getElementById('stroke-dropdown');
  const textDropdown = document.getElementById('text-dropdown');
  const blurDropdown = document.getElementById('blur-dropdown');
  const colorDropdown = document.getElementById('color-dropdown');
  const widthDropdown = document.getElementById('width-dropdown');

  if (shapeTrigger) shapeTrigger.addEventListener('click', (e) => { e.stopPropagation(); toggleDropdown(shapeDropdown); });
  if (strokeTrigger) strokeTrigger.addEventListener('click', (e) => { e.stopPropagation(); toggleDropdown(strokeDropdown); });
  if (textTrigger) textTrigger.addEventListener('click', (e) => { e.stopPropagation(); toggleDropdown(textDropdown); });
  if (blurTrigger) blurTrigger.addEventListener('click', (e) => { e.stopPropagation(); toggleDropdown(blurDropdown); });
  if (colorTrigger) colorTrigger.addEventListener('click', (e) => { e.stopPropagation(); toggleDropdown(colorDropdown); });
  if (widthTrigger) widthTrigger.addEventListener('click', (e) => { e.stopPropagation(); toggleDropdown(widthDropdown); });

  // 点外部关闭所有下拉（拖选 canvas / 点其他按钮时）
  document.addEventListener('click', (e) => {
    if (!e.target.closest('.dropdown-wrap')) closeAllDropdowns();
  });
  document.addEventListener('mousedown', (e) => {
    // canvas 拖选启动时也关闭下拉
    if (e.target.id === 'canvas') closeAllDropdowns();
  });

  // ── 工具切换统一入口 ──
  // split-btn 结构（0.11.8-a）：
  //   .split-main   ── 主按钮，点击直接用当前分组选中的工具（data-tool 存"当前工具"）
  //   .split-caret  ── 三角按钮，仅展开 dropdown 供切换
  // 切换 dropdown item 时更新对应 split-main 的 data-tool + 图标。
  const shapeMain = document.getElementById('shape-main');
  const shapeMainIcon = document.getElementById('shape-main-icon');
  const strokeMain = document.getElementById('stroke-main');
  const strokeMainIcon = document.getElementById('stroke-main-icon');
  const textMain = document.getElementById('text-main');
  const textMainIcon = document.getElementById('text-main-icon');
  const blurMain = document.getElementById('blur-main');
  const blurMainIcon = document.getElementById('blur-main-icon');

  /** 工具 → 所属分组（决定同步哪个 split-main 的图标 + 哪个下拉 item 高亮） */
  const TOOL_GROUPS = {
    rect: 'shape', ellipse: 'shape',
    arrow: 'stroke', pencil: 'stroke',
    'highlight-multiply': 'stroke', 'highlight-translucent': 'stroke',
    text: 'text', watermark: 'text',
    pixelate: 'blur', mosaic: 'blur',
    eraser: 'direct',
  };

  /** 分组 → { main, mainIcon, dropdownSelector } */
  const GROUP_META = {
    shape:  { main: shapeMain,  icon: shapeMainIcon,  dropdown: '#shape-dropdown' },
    stroke: { main: strokeMain, icon: strokeMainIcon, dropdown: '#stroke-dropdown' },
    text:   { main: textMain,   icon: textMainIcon,   dropdown: '#text-dropdown' },
    blur:   { main: blurMain,   icon: blurMainIcon,   dropdown: '#blur-dropdown' },
  };

  function selectTool(tool) {
    annot.setTool(tool);
    // 清除所有工具入口 active（split-main、下拉 item、直接按钮）
    document.querySelectorAll('.split-main, .tool-direct').forEach((b) => b.classList.remove('active'));
    document.querySelectorAll('.dropdown-item[data-tool]').forEach((b) => b.classList.remove('active'));
    // 标记当前工具的入口 active
    const group = TOOL_GROUPS[tool];
    const meta = GROUP_META[group];
    if (meta) {
      const item = document.querySelector(`${meta.dropdown} .dropdown-item[data-tool="${tool}"]`);
      if (item) {
        item.classList.add('active');
        const icon = item.querySelector('.item-icon');
        // 同步 split-main：图标 + data-tool（下次点主按钮直接用它）
        if (meta.icon && icon) meta.icon.innerHTML = icon.innerHTML;
        if (meta.main) meta.main.dataset.tool = tool;
      }
      if (meta.main) meta.main.classList.add('active');
    } else if (group === 'direct') {
      const btn = document.querySelector(`.tool-direct[data-tool="${tool}"]`);
      if (btn) btn.classList.add('active');
    }
    closeAllDropdowns();

    // 0.11.8-c：watermark 特殊——不用 closeAllDropdowns 关掉，
    // 而是把 text-dropdown 切到水印表单视图（在原地展开配置项，跟颜色下拉一致的交互）
    if (tool === 'watermark') {
      openWatermarkForm();
    }
  }

  // split-main：点击直接用当前分组记住的工具
  [shapeMain, strokeMain, textMain, blurMain].forEach((btn) => {
    if (!btn) return;
    btn.addEventListener('click', (e) => {
      e.stopPropagation();
      closeAllDropdowns();
      selectTool(btn.dataset.tool);
    });
  });

  // 各分组 dropdown item
  document.querySelectorAll('#shape-dropdown .dropdown-item, #stroke-dropdown .dropdown-item, #text-dropdown .dropdown-item, #blur-dropdown .dropdown-item').forEach((item) => {
    item.addEventListener('click', () => selectTool(item.dataset.tool));
  });
  // 直接按钮（文本 / 橡皮擦）
  document.querySelectorAll('.tool-direct').forEach((btn) => {
    btn.addEventListener('click', () => selectTool(btn.dataset.tool));
  });

  // ── 颜色选择（swatch + custom picker）──
  // 0.11.8-b：swatch/dot 改用 background 填色（原字符 ● + color 呈现"暗底 + 小彩点"）
  const swatches = document.querySelectorAll('.color-swatch');
  const colorPicker = document.getElementById('color-picker');
  const colorTriggerDot = document.getElementById('color-trigger-dot');
  swatches.forEach((btn) => {
    btn.addEventListener('click', () => {
      const color = btn.dataset.color;
      annot.setColor(color);
      swatches.forEach((b) => b.classList.remove('active'));
      btn.classList.add('active');
      if (colorPicker) colorPicker.value = color;
      if (colorTriggerDot) colorTriggerDot.style.background = color;
      closeAllDropdowns();
    });
  });
  if (colorPicker) {
    colorPicker.addEventListener('input', (e) => {
      const color = e.target.value;
      annot.setColor(color);
      swatches.forEach((b) => b.classList.remove('active'));
      if (colorTriggerDot) colorTriggerDot.style.background = color;
    });
  }

  // ── 粗细选择 ──
  const widthTriggerIcon = document.getElementById('width-trigger-icon');
  const widthItems = widthDropdown ? widthDropdown.querySelectorAll('.dropdown-item') : [];
  widthItems.forEach((item) => {
    item.addEventListener('click', () => {
      const width = parseInt(item.dataset.width, 10);
      annot.setWidth(width);
      widthItems.forEach((b) => b.classList.remove('active'));
      item.classList.add('active');
      const icon = item.querySelector('.item-icon');
      // 0.11.8：item-icon 现在装 .width-bar span 而非 Unicode 文本，用 innerHTML 拷贝
      if (widthTriggerIcon && icon) widthTriggerIcon.innerHTML = icon.innerHTML;
      closeAllDropdowns();
    });
  });

  // 撤销/重做
  const btnUndo = document.getElementById('btn-undo');
  const btnRedo = document.getElementById('btn-redo');
  if (btnUndo) btnUndo.addEventListener('click', () => { annot.undo(); updateUndoRedoButtons(); });
  if (btnRedo) btnRedo.addEventListener('click', () => { annot.redo(); updateUndoRedoButtons(); });

  // 输出/取消
  const bind = (id, fn) => {
    const el = document.getElementById(id);
    if (el) el.addEventListener('click', fn);
  };
  bind('btn-cancel', doCancel);
  bind('btn-pin', doPinSelection);
  bind('btn-ocr', doOcrSelection);
  bind('btn-save', doSaveSelection);
  bind('btn-copy', doCopySelection);

  // ── 拖动 handle（0.11.8-a）：拖动整条工具栏 ─────────────────
  // 用户手动拖过之后，positionToolbar 尊重此位置，不再自动重定位。
  const dragHandle = document.getElementById('toolbar-drag');
  if (dragHandle) {
    let dragging = false;
    let offsetX = 0, offsetY = 0;
    dragHandle.addEventListener('mousedown', (e) => {
      if (e.button !== 0) return;
      e.preventDefault();
      e.stopPropagation();
      dragging = true;
      toolbar.dataset.userMoved = 'true';
      const rect = toolbar.getBoundingClientRect();
      offsetX = e.clientX - rect.left;
      offsetY = e.clientY - rect.top;
      document.body.style.cursor = 'grabbing';
    });
    document.addEventListener('mousemove', (e) => {
      if (!dragging) return;
      const vw = window.innerWidth;
      const vh = window.innerHeight;
      const tw = toolbar.offsetWidth;
      const th = toolbar.offsetHeight;
      let left = e.clientX - offsetX;
      let top = e.clientY - offsetY;
      // 边界约束
      left = Math.max(8, Math.min(left, vw - tw - 8));
      top = Math.max(8, Math.min(top, vh - th - 8));
      toolbar.style.left = left + 'px';
      toolbar.style.top = top + 'px';
    });
    document.addEventListener('mouseup', () => {
      if (dragging) {
        dragging = false;
        document.body.style.cursor = '';
      }
    });
  }
}

bindToolbar();

function updateUndoRedoButtons() {
  const btnUndo = document.getElementById('btn-undo');
  const btnRedo = document.getElementById('btn-redo');
  if (btnUndo) btnUndo.disabled = !annot.canUndo();
  if (btnRedo) btnRedo.disabled = !annot.canRedo();
}

// ── 合成 ──────────────────────────────────────────────

/** 合成选区（裁剪区 + 标注）为 PNG bytes */
function compositeSelection(callback) {
  if (!selCss || !screenshot) { console.error('[screenshot] compositeSelection: no selection'); return; }
  const dpr = window.devicePixelRatio || 1;
  const pw = Math.round(selCss.w * dpr);
  const ph = Math.round(selCss.h * dpr);
  const px = Math.round(selCss.x * dpr);
  const py = Math.round(selCss.y * dpr);

  const off = document.createElement('canvas');
  off.width = pw;
  off.height = ph;
  const offCtx = off.getContext('2d');
  offCtx.drawImage(screenshot, px, py, pw, ph, 0, 0, pw, ph);
  if (annot.hasAnnotations()) {
    offCtx.drawImage(annotCanvas, 0, 0);
  }
  try {
    off.toBlob((blob) => {
      if (!blob) { console.error('PNG 合成失败'); sent = false; return; }
      blob.arrayBuffer().then((buf) => callback(new Uint8Array(buf))).catch(() => { sent = false; });
    }, 'image/png');
  } catch (e) {
    console.error('toBlob 异常', e);
    sent = false;
  }
}

// ── 工具 ──────────────────────────────────────────────

function norm(x1, y1, x2, y2) {
  return {
    x: Math.min(x1, x2), y: Math.min(y1, y2),
    w: Math.abs(x2 - x1), h: Math.abs(y2 - y1),
  };
}

function pointInRect(px, py, rect) {
  return px >= rect.x && px <= rect.x + rect.w && py >= rect.y && py <= rect.y + rect.h;
}

/**
 * 矩形/椭圆按住 Shift 约束长宽等比（0.11.8-e）：
 * 从起点 (sx,sy) 到当前 (ex,ey)，取 max(|dx|,|dy|) 作等边，符号保持原方向。
 * 只对 rect/ellipse 生效——箭头/铅笔等自由笔画不约束。
 * 返回修正后的 {x, y}，或 null 表示不需要约束。
 */
function applySquareConstraint(sx, sy, ex, ey) {
  const tool = annot.getTool();
  if (tool !== 'rect' && tool !== 'ellipse') return null;
  const dx = ex - sx;
  const dy = ey - sy;
  const side = Math.max(Math.abs(dx), Math.abs(dy));
  return {
    x: sx + (dx >= 0 ? side : -side),
    y: sy + (dy >= 0 ? side : -side),
  };
}
