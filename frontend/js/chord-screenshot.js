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
    case 'eraser': {
      // 橡皮擦预览：半透明灰色方块
      const x = Math.min(annotStartX, annotCurrentX);
      const y = Math.min(annotStartY, annotCurrentY);
      const w = Math.abs(annotCurrentX - annotStartX);
      const h = Math.abs(annotCurrentY - annotStartY);
      annotCtx.fillStyle = 'rgba(200, 200, 200, 0.3)';
      annotCtx.fillRect(x, y, w, h);
      annotCtx.strokeStyle = 'rgba(255, 255, 255, 0.5)';
      annotCtx.lineWidth = 1;
      annotCtx.strokeRect(x, y, w, h);
      break;
    }
    case 'mosaic': {
      const x = Math.min(annotStartX, annotCurrentX);
      const y = Math.min(annotStartY, annotCurrentY);
      const w = Math.abs(annotCurrentX - annotStartX);
      const h = Math.abs(annotCurrentY - annotStartY);
      annotCtx.fillStyle = 'rgba(200, 200, 200, 0.3)';
      annotCtx.fillRect(x, y, w, h);
      break;
    }
  }
  annotCtx.restore();
}

/** 全量重绘标注层（已提交的命令） */
function redrawAnnotFull() {
  if (!selCss || annotCanvas.width === 0) return;
  annotCtx.clearRect(0, 0, annotCanvas.width, annotCanvas.height);
  for (const cmd of annot.getCommands()) {
    annot.executeCommand(cmd, annotCtx);
  }
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
  screenshotSetAnnotationMode(false).catch((e) => console.error('setAnnotationMode(false) 失败', e));
  drawDimmed();
}

/** 定位工具栏到选区右下外侧（PixPin 风格） */
function positionToolbar(rect) {
  toolbar.style.display = 'flex';
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

  // 标注绘制中
  if (isAnnotDragging && selCss) {
    const dpr = window.devicePixelRatio || 1;
    annotCurrentX = (e.offsetX - selCss.x) * dpr;
    annotCurrentY = (e.offsetY - selCss.y) * dpr;
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

canvas.addEventListener('mouseup', (e) => {
  if (!screenshot) return;

  // 标注绘制结束
  if (isAnnotDragging) {
    isAnnotDragging = false;
    const dpr = window.devicePixelRatio || 1;
    annotCurrentX = (e.offsetX - selCss.x) * dpr;
    annotCurrentY = (e.offsetY - selCss.y) * dpr;

    const tool = annot.getTool();
    // 文本工具：允许零拖拽（点击一次就弹输入框）
    // 铅笔：只要有轨迹点就生成（在 startDraw 已有 1 个点）
    // 橡皮擦：允许小距离（用户擦某个小图标）
    // 矩形/椭圆/箭头/马赛克：需要 >=3px 拖动
    const dx = annotCurrentX - annotStartX;
    const dy = annotCurrentY - annotStartY;
    const minDrag = (tool === 'text' || tool === 'eraser' || tool === 'pencil') ? 0 : 3;
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
  compositeSelection((pngBytes) => {
    screenshotPin(pngBytes).catch((err) => {
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
  const displayText = text || '（未识别到文字）';

  const panel = document.createElement('div');
  panel.id = 'ocr-panel';
  panel.className = 'ocr-panel';
  panel.innerHTML = `
    <div class="ocr-panel-header">
      <span>OCR 识别结果</span>
      <button id="ocr-close" class="tool-btn">✕</button>
    </div>
    <div class="ocr-panel-body">${escapeHtml(displayText)}</div>
    <div class="ocr-panel-footer">
      <button id="ocr-copy" class="tool-btn tool-btn-primary" ${text ? '' : 'disabled'}>复制文本</button>
    </div>
  `;
  document.body.appendChild(panel);

  panel.style.left = Math.max(8, selCss.x) + 'px';
  // 默认放在选区上方；如果放不下则翻到选区下方
  const topAbove = selCss.y - panel.offsetHeight - 8;
  if (topAbove < 8) {
    panel.style.top = (selCss.y + selCss.h + 8) + 'px';
  } else {
    panel.style.top = topAbove + 'px';
  }

  document.getElementById('ocr-close').addEventListener('click', () => panel.remove());
  document.getElementById('ocr-copy').addEventListener('click', () => {
    if (text) {
      navigator.clipboard.writeText(text).catch((e) => console.error('复制失败', e));
    }
    panel.remove();
  });
}

function escapeHtml(text) {
  const div = document.createElement('div');
  div.textContent = text;
  return div.innerHTML;
}

// ── 文本标注输入框 ─────────────────────────────────

function showTextInput(x, y) {
  if (!selCss) return;
  const dpr = window.devicePixelRatio || 1;

  const input = document.createElement('input');
  input.type = 'text';
  input.className = 'text-annot-input';
  input.placeholder = '输入文本…';
  // 定位到标注 canvas 上、用户点击位置（CSS 像素）
  input.style.left = (selCss.x + x / dpr) + 'px';
  input.style.top = (selCss.y + y / dpr) + 'px';
  input.style.fontSize = Math.max(14, annot.getWidth() * 3) + 'px';
  input.style.color = annot.getColor();
  input.spellcheck = false;
  input.autocomplete = 'off';
  document.body.appendChild(input);
  input.focus();
  // 全选已有的提示文字，方便用户直接覆盖
  setTimeout(() => input.select(), 0);

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
    if (e.key === 'Enter') {
      e.preventDefault();
      commit(input.value.trim());
    } else if (e.key === 'Escape') {
      e.stopPropagation();
      annot.cancelText();
      cleanup();
    }
  });

  // blur 自动提交——用户点击工具栏或标注区时文本会被提交
  // 空文本时取消标注
  input.addEventListener('blur', () => {
    const text = input.value.trim();
    commit(text);
  });
}

// ── 工具栏事件绑定（0.11.7 review：模块内直接绑定，替代 HTML 内联脚本 + window.__screenshot* 桥接） ─
//
// 模块作为 <script type="module"> 加载，天然 defer——DOM 已解析完成，可直接查询。
// 每次 Alt+A 复用窗口时不需要重绑（元素身份不变），初始化只跑一次。

function bindToolbar() {
  // 标注工具切换
  const toolBtns = document.querySelectorAll('[data-tool]');
  toolBtns.forEach((btn) => {
    btn.addEventListener('click', () => {
      annot.setTool(btn.dataset.tool);
      toolBtns.forEach((b) => b.classList.remove('active'));
      btn.classList.add('active');
    });
  });

  // 颜色选择（swatch + custom picker）
  const swatches = document.querySelectorAll('.color-swatch');
  const colorPicker = document.getElementById('color-picker');
  swatches.forEach((btn) => {
    btn.addEventListener('click', () => {
      annot.setColor(btn.dataset.color);
      swatches.forEach((b) => b.classList.remove('active'));
      btn.classList.add('active');
      if (colorPicker) colorPicker.value = btn.dataset.color;
    });
  });
  if (colorPicker) {
    colorPicker.addEventListener('input', (e) => {
      annot.setColor(e.target.value);
      swatches.forEach((b) => b.classList.remove('active'));
    });
  }

  // 粗细
  const widthBtns = document.querySelectorAll('.width-btn');
  widthBtns.forEach((btn) => {
    btn.addEventListener('click', () => {
      annot.setWidth(parseInt(btn.dataset.width, 10));
      widthBtns.forEach((b) => b.classList.remove('active'));
      btn.classList.add('active');
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
