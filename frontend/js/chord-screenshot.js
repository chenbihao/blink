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
  translateText,
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
  // 0.11.9-c：清阅读模式
  exitReadingMode();
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

// ── 显示器几何（多屏混合 DPI 正确 clamp）──────────────────
//
// 后端 show_screenshot_overlay 注入 window.__blinkScreenMeta.displays：
//   [{ x, y, w, h, dpi, primary }, ...]  —— 全是虚拟屏物理像素
// selCss / window.inner* 是 CSS 像素，混合 DPI 下两者的「物理↔CSS」换算系数
// 在不同屏上不同（主屏 100% = 1.0、副屏 150% = 1.5），所以不能用一个 dpr 统一折算。
// 这里的策略：把每块屏的物理矩形用**它自己的 dpr** 折算成「overlay CSS 坐标系里
// 这块屏占的矩形」，之后选区中心点 point-in-rect 找出选区所在屏，clamp 基准就是
// 这块屏的 CSS 矩形。

/**
 * 取注入的 displays 列表（每屏物理几何 + DPI）。
 * 缺失返回空数组，调用方按"无多屏信息"降级到旧的 innerWidth/innerHeight clamp。
 */
function getDisplays() {
  const meta = window.__blinkScreenMeta;
  return (meta && Array.isArray(meta.displays) && meta.displays) || [];
}

/**
 * 把单块屏的物理几何折算成 overlay CSS 坐标系里的矩形。
 * 单屏环境：dpr 与 window.devicePixelRatio 相同，结果就是 (0,0,innerW,innerH)。
 * 混合 DPI：每屏用各自的 dpr（= dpi/96）折算，结果矩形拼接覆盖整个 overlay。
 */
function displayToCss(d) {
  const dpr = (d && d.dpi ? d.dpi : 96) / 96;
  return {
    x: d.x / dpr,
    y: d.y / dpr,
    w: d.w / dpr,
    h: d.h / dpr,
  };
}

/**
 * 给一个 CSS 坐标点，返回它所在屏的 CSS 矩形（含 fallback）。
 *
 * 找不到匹配屏（极端：注入的 displays 还没就绪）时，回退到整个 overlay 视口，
 * 保证函数永不返回 null —— 调用方不必处理空值。
 */
function findDisplayCssAt(cssX, cssY) {
  const displays = getDisplays();
  for (const d of displays) {
    const r = displayToCss(d);
    if (cssX >= r.x && cssX < r.x + r.w && cssY >= r.y && cssY < r.y + r.h) {
      return r;
    }
  }
  // fallback：整个 overlay 视口（单屏或 displays 未就绪时走这里）
  return { x: 0, y: 0, w: window.innerWidth, h: window.innerHeight };
}

/** 定位工具栏到选区右下外侧（PixPin 风格）。
 *  0.11.8-a：若用户已手动拖过工具栏（dataset.userMoved），保留用户位置不重定位。
 *  0.11.9：按"选区所在屏"clamp（不再以整个虚拟屏为基准）—— 副屏左边缘做选区时
 *          工具栏不会被推到另一块屏去。混合 DPI 也正确（每屏 CSS 矩形按各自 dpr 折算）。 */
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
    // 选区中心点定位"当前屏"——混合 DPI 下中心点不会落在屏边界外
    const mon = findDisplayCssAt(rect.x + rect.w / 2, rect.y + rect.h / 2);
    const MARGIN = 8;

    // 右对齐选区右边缘
    let left = rect.x + rect.w - tw;
    if (left + tw > mon.x + mon.w - MARGIN) left = mon.x + mon.w - tw - MARGIN;
    if (left < mon.x + MARGIN) left = mon.x + MARGIN;

    // 位于选区下方 8px
    let top = rect.y + rect.h + MARGIN;
    // 底部空间不足 → 翻转到选区上方
    if (top + th > mon.y + mon.h - MARGIN) {
      top = rect.y - th - MARGIN;
    }
    // 上方也不够 → 贴当前屏底部
    if (top < mon.y + MARGIN) {
      top = Math.max(mon.y + MARGIN, mon.y + mon.h - th - MARGIN);
    }

    toolbar.style.left = left + 'px';
    toolbar.style.top = top + 'px';
    console.debug('[screenshot] toolbar 定位', { left, top, tw, th, rect, mon });
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

/**
 * 0.11.9-e：一条龙"翻译"——OCR + 立即翻译,面板停在译文 tab。
 * 与 doOcrSelection 唯一差别是 showOcrResult 传 `{ autoTranslate: true }`。
 */
function doTranslateSelection() {
  if (!selCss || sent) return;
  sent = true;
  compositeSelection((pngBytes) => {
    ocrImage(pngBytes)
      .then((result) => showOcrResult(result, { autoTranslate: true }))
      .catch((err) => console.error('[screenshot] ocr(translate) 失败', err))
      .finally(() => { sent = false; });
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

// ── OCR 阅读模式（0.11.9-c）─────────────────────────────
//
// OCR 完成后 overlay 不关,进入"阅读模式":
// - 原图 word 可鼠标拖选(跨行按 word 数组顺序取连续段,不是矩形框选)
// - 图上拖选 → 面板 textarea 里对应字符高亮 + selection
// - 面板 textarea 里选中文字 → 反查覆盖的 words → 图上高亮
// - 用户在 textarea 里手动编辑后,反向映射失效(内容偏移),此后只保留正向
//   (图 → panel),textarea → 图 停止联动;tab 上加提示
//
// 坐标系:word.bounding_rect 是物理像素相对**裁剪区左上角**(与 annot-canvas 一致)
//        hit-canvas 内部像素 = 物理像素,CSS 尺寸 = 选区 CSS 尺寸

const hitCanvas = document.getElementById('ocr-hit-canvas');
const hitCtx = hitCanvas.getContext('2d');

/** 阅读模式状态。null 表示未激活 */
let reading = null;
/**
 * reading = {
 *   words: [{ text, rect: {x,y,w,h}, lineIndex }],  // 物理像素相对裁剪区
 *   charRanges: [{ start, end }],                    // 对应 joined text 的字符范围
 *   fullText: string,                                // 拼接文本(与 result.text 一致)
 *   selectionStart: number, selectionEnd: number,    // 当前选中范围(word 索引,不是字符)
 *   panelDirty: boolean,                              // textarea 内容已被手动编辑
 *   dragStart: number | null,                        // 拖选起始 word 索引
 *   hoverWord: number | null,                        // 当前 hover 的 word 索引
 * }
 */

/** 从 result 构造 charRanges —— 用与后端 join_words_smart 相同的规则复算字符偏移。
 *  依赖:相邻 word 何时加空格/换行必须与后端一致。参考 ocr_engine.rs::join_words_smart。 */
function computeCharRanges(words) {
  const ranges = new Array(words.length);
  let cursor = 0;
  let prevLine = null;
  let prevTailKind = null;
  for (let i = 0; i < words.length; i++) {
    const w = words[i];
    if (!w.text) {
      ranges[i] = { start: cursor, end: cursor };
      continue;
    }
    // 换行
    if (prevLine !== null && prevLine !== w.lineIndex) {
      cursor += 1; // '\n'
      prevTailKind = null;
    }
    // 词间空格
    if (prevTailKind !== null) {
      const hk = charKind(w.text.charAt(0));
      const needSpace = !(prevTailKind === 'cjk' || hk === 'cjk');
      if (needSpace) cursor += 1;
    }
    const start = cursor;
    // 用 [...text].length 计 code point 数(与 String.length 在 BMP 内一致,
    // 但 emoji 等 surrogate pair 时才有差别;OCR 输出几乎不出现,兜底稳妥)
    const len = [...w.text].reduce((n) => n + 1, 0);
    cursor += w.text.length; // 用 UTF-16 长度以对齐 textarea selection API
    ranges[i] = { start, end: cursor };
    // tail kind 用最后一个字符
    prevTailKind = charKind(w.text.charAt(w.text.length - 1));
    prevLine = w.lineIndex;
    void len; // 保留计算但 selection API 用 UTF-16
  }
  return ranges;
}

/** 判定字符分类（'cjk' / 'latin' / 'other'）——与后端 is_cjk_ish/is_latin_word_char 对齐 */
function charKind(ch) {
  if (!ch) return 'other';
  const cp = ch.codePointAt(0);
  // CJK 段
  if (
    (cp >= 0x3400 && cp <= 0x4dbf) ||
    (cp >= 0x4e00 && cp <= 0x9fff) ||
    (cp >= 0x20000 && cp <= 0x2a6df) ||
    (cp >= 0xf900 && cp <= 0xfaff) ||
    (cp >= 0x3040 && cp <= 0x309f) ||
    (cp >= 0x30a0 && cp <= 0x30ff) ||
    (cp >= 0xac00 && cp <= 0xd7af) ||
    (cp >= 0x3000 && cp <= 0x303f) ||
    (cp >= 0xff00 && cp <= 0xffef)
  ) return 'cjk';
  // Latin
  if (/[a-zA-Z0-9]/.test(ch)) return 'latin';
  if ((cp >= 0x00c0 && cp <= 0x024f) || (cp >= 0x1e00 && cp <= 0x1eff)) return 'latin';
  return 'other';
}

/** 命中测试：把点击的 CSS 坐标(相对 hitCanvas)映射到物理像素 + 二分/线性找命中 word */
function hitTestWord(cssX, cssY) {
  if (!reading) return -1;
  const dpr = window.devicePixelRatio || 1;
  const px = cssX * dpr;
  const py = cssY * dpr;
  // 命中优先按 y 区间,再按 x 区间;无重叠假设 O(n) 遍历(word 数 100+ 也够快)
  for (let i = 0; i < reading.words.length; i++) {
    const r = reading.words[i].rect;
    if (px >= r.x && px <= r.x + r.w && py >= r.y && py <= r.y + r.h) return i;
  }
  return -1;
}

/** 找出接近点击点(垂直方向)的最近 word——空白处点击时靠近哪 word 就选哪 */
function nearestWordByLine(cssX, cssY) {
  if (!reading || reading.words.length === 0) return -1;
  const dpr = window.devicePixelRatio || 1;
  const px = cssX * dpr;
  const py = cssY * dpr;
  // 先按 y 找到"最靠近的行"(word.line_index 一致的一批)
  let bestLine = reading.words[0].lineIndex;
  let bestDy = Infinity;
  for (const w of reading.words) {
    const cy = w.rect.y + w.rect.h / 2;
    const dy = Math.abs(cy - py);
    if (dy < bestDy) { bestDy = dy; bestLine = w.lineIndex; }
  }
  // 在该行内按 x 距离找最近
  let bestIdx = -1;
  let bestDx = Infinity;
  for (let i = 0; i < reading.words.length; i++) {
    const w = reading.words[i];
    if (w.lineIndex !== bestLine) continue;
    const cx = w.rect.x + w.rect.w / 2;
    const dx = Math.abs(cx - px);
    if (dx < bestDx) { bestDx = dx; bestIdx = i; }
  }
  return bestIdx;
}

/** 重绘 hit-canvas：高亮当前选中 words + hover word */
function redrawHitLayer() {
  if (!reading) return;
  hitCtx.clearRect(0, 0, hitCanvas.width, hitCanvas.height);
  // 选中范围高亮
  if (reading.selectionStart !== null && reading.selectionEnd !== null) {
    const lo = Math.min(reading.selectionStart, reading.selectionEnd);
    const hi = Math.max(reading.selectionStart, reading.selectionEnd);
    hitCtx.fillStyle = 'rgba(74, 158, 255, 0.35)';
    for (let i = lo; i <= hi; i++) {
      const r = reading.words[i].rect;
      hitCtx.fillRect(r.x, r.y, r.w, r.h);
    }
    // 描边让边界清晰
    hitCtx.strokeStyle = 'rgba(74, 158, 255, 0.85)';
    hitCtx.lineWidth = Math.max(1, Math.round(window.devicePixelRatio || 1));
    for (let i = lo; i <= hi; i++) {
      const r = reading.words[i].rect;
      hitCtx.strokeRect(r.x + 0.5, r.y + 0.5, r.w, r.h);
    }
  }
  // Hover 提示(浅一点)
  if (reading.hoverWord !== null && reading.hoverWord >= 0) {
    const r = reading.words[reading.hoverWord].rect;
    hitCtx.strokeStyle = 'rgba(255, 255, 255, 0.5)';
    hitCtx.lineWidth = 1;
    hitCtx.strokeRect(r.x + 0.5, r.y + 0.5, r.w, r.h);
  }
}

/** 进入阅读模式:定位 hit-canvas + 装事件 + 首次全选 */
function enterReadingMode(result) {
  if (!selCss) return;
  const words = (result && Array.isArray(result.words)) ? result.words : [];
  if (words.length === 0) return; // 没 word 数据直接不进阅读模式

  const dpr = window.devicePixelRatio || 1;
  hitCanvas.style.left = selCss.x + 'px';
  hitCanvas.style.top = selCss.y + 'px';
  hitCanvas.style.width = selCss.w + 'px';
  hitCanvas.style.height = selCss.h + 'px';
  hitCanvas.width = Math.max(1, Math.round(selCss.w * dpr));
  hitCanvas.height = Math.max(1, Math.round(selCss.h * dpr));
  hitCanvas.setAttribute('data-reading', 'true');

  reading = {
    words: words.map((w) => ({
      text: w.text,
      rect: w.rect, // 后端 serde 已 rename bounding_rect -> rect
      lineIndex: w.line_index,
    })),
    charRanges: computeCharRanges(words.map((w) => ({
      text: w.text,
      lineIndex: w.line_index,
    }))),
    fullText: result.text || '',
    selectionStart: null,
    selectionEnd: null,
    panelDirty: false,
    dragStart: null,
    hoverWord: null,
  };

  bindHitCanvasEvents();
  redrawHitLayer();
}

/** 退出阅读模式 */
function exitReadingMode() {
  hitCanvas.removeAttribute('data-reading');
  hitCtx.clearRect(0, 0, hitCanvas.width, hitCanvas.height);
  reading = null;
}

// ── hit-canvas 事件（幂等绑定，模块生命周期只装一次） ─
let hitEventsBound = false;
function bindHitCanvasEvents() {
  if (hitEventsBound) return;
  hitEventsBound = true;

  hitCanvas.addEventListener('mousedown', (e) => {
    if (!reading || e.button !== 0) return;
    e.stopPropagation();
    e.preventDefault();
    let idx = hitTestWord(e.offsetX, e.offsetY);
    if (idx < 0) idx = nearestWordByLine(e.offsetX, e.offsetY);
    if (idx < 0) return;
    reading.dragStart = idx;
    reading.selectionStart = idx;
    reading.selectionEnd = idx;
    redrawHitLayer();
    syncSelectionToPanel();
  });

  hitCanvas.addEventListener('mousemove', (e) => {
    if (!reading) return;
    const idx = hitTestWord(e.offsetX, e.offsetY);
    if (reading.dragStart !== null) {
      // 拖选中
      const endIdx = idx >= 0 ? idx : nearestWordByLine(e.offsetX, e.offsetY);
      if (endIdx >= 0) {
        reading.selectionEnd = endIdx;
        redrawHitLayer();
        syncSelectionToPanel();
      }
    } else {
      // hover 反馈
      if (idx !== reading.hoverWord) {
        reading.hoverWord = idx >= 0 ? idx : null;
        redrawHitLayer();
      }
    }
  });

  hitCanvas.addEventListener('mouseup', () => {
    if (!reading) return;
    reading.dragStart = null;
  });

  hitCanvas.addEventListener('mouseleave', () => {
    if (!reading) return;
    reading.hoverWord = null;
    reading.dragStart = null;
    redrawHitLayer();
  });

  // 双击选一整行
  hitCanvas.addEventListener('dblclick', (e) => {
    if (!reading) return;
    let idx = hitTestWord(e.offsetX, e.offsetY);
    if (idx < 0) idx = nearestWordByLine(e.offsetX, e.offsetY);
    if (idx < 0) return;
    const line = reading.words[idx].lineIndex;
    let lo = idx, hi = idx;
    while (lo > 0 && reading.words[lo - 1].lineIndex === line) lo--;
    while (hi < reading.words.length - 1 && reading.words[hi + 1].lineIndex === line) hi++;
    reading.selectionStart = lo;
    reading.selectionEnd = hi;
    redrawHitLayer();
    syncSelectionToPanel();
  });
}

/** 图上选中变化 → 同步到面板 textarea */
function syncSelectionToPanel() {
  const panel = document.getElementById('ocr-panel');
  if (!panel || !reading) return;
  if (reading.selectionStart === null) return;
  const ta = panel.querySelector('#ocr-textarea-source');
  if (!ta || ta.hidden) return;
  if (reading.panelDirty) return; // 面板已编辑,不再反向覆盖
  const lo = Math.min(reading.selectionStart, reading.selectionEnd);
  const hi = Math.max(reading.selectionStart, reading.selectionEnd);
  const cs = reading.charRanges[lo].start;
  const ce = reading.charRanges[hi].end;
  ta.focus();
  ta.setSelectionRange(cs, ce);
}

/** 面板 textarea 选中变化 → 反查 words 并高亮图 */
function syncSelectionFromPanel(ta) {
  if (!reading) return;
  if (reading.panelDirty) return;
  const cs = ta.selectionStart;
  const ce = ta.selectionEnd;
  if (cs === ce) {
    // 光标零宽 → 清图上选中
    reading.selectionStart = null;
    reading.selectionEnd = null;
    redrawHitLayer();
    return;
  }
  // 二分/线性找命中的 word 范围
  let lo = -1, hi = -1;
  for (let i = 0; i < reading.charRanges.length; i++) {
    const r = reading.charRanges[i];
    if (r.end > cs && lo === -1) lo = i;
    if (r.start < ce) hi = i;
    if (r.start >= ce) break;
  }
  if (lo === -1 || hi === -1 || lo > hi) return;
  reading.selectionStart = lo;
  reading.selectionEnd = hi;
  redrawHitLayer();
}


//
// 0.11.9-d：双 tab（原文 / 译文）。工具栏"OCR"按钮打开 → 停原文 tab;
// "翻译"按钮打开 → 自动 OCR + translate → 切到译文 tab。面板内"翻译"
// 按钮独立触发翻译。修改原文 → 译文标"过期"(斜纹底 + 橙点)。
//
// 参数:
//   result:       { text, lines, words?, text_angle? } — OCR 结果
//   options.autoTranslate: 打开面板后立刻触发翻译 + 切到译文 tab

function showOcrResult(result, options = {}) {
  const old = document.getElementById('ocr-panel');
  if (old) old.remove();
  // 关闭上次可能残留的阅读模式(比如同一 overlay 内多次 OCR)
  exitReadingMode();

  const text = (result && result.text) || '';
  const initialText = text || '（未识别到文字）';

  const panel = document.createElement('div');
  panel.id = 'ocr-panel';
  panel.className = 'ocr-panel';
  panel.innerHTML = `
    <div class="ocr-panel-header">
      <div class="ocr-tabs">
        <button class="ocr-tab active" data-tab="source">原文</button>
        <button class="ocr-tab" data-tab="translated">译文 <span class="stale-dot" aria-hidden="true"></span></button>
      </div>
      <button id="ocr-close" class="tool-btn">✕</button>
    </div>
    <textarea id="ocr-textarea-source" class="ocr-panel-textarea" spellcheck="false"></textarea>
    <textarea id="ocr-textarea-translated" class="ocr-panel-textarea" spellcheck="false" hidden placeholder="点击"翻译"按钮生成译文"></textarea>
    <div class="ocr-panel-footer">
      <span class="ocr-panel-hint">可自由选词复制或编辑</span>
      <button id="ocr-trim-spaces" class="tool-btn" ${text ? '' : 'disabled'} title="合并连续空白 + 去除首尾空格">移除空格</button>
      <button id="ocr-translate" class="tool-btn" ${text ? '' : 'disabled'} title="翻译当前文本">翻译</button>
      <button id="ocr-copy" class="tool-btn tool-btn-primary" ${text ? '' : 'disabled'}>复制</button>
    </div>
  `;
  document.body.appendChild(panel);

  const sourceTa = panel.querySelector('#ocr-textarea-source');
  const translatedTa = panel.querySelector('#ocr-textarea-translated');
  const tabSource = panel.querySelector('.ocr-tab[data-tab="source"]');
  const tabTranslated = panel.querySelector('.ocr-tab[data-tab="translated"]');
  const translateBtn = panel.querySelector('#ocr-translate');
  const trimBtn = panel.querySelector('#ocr-trim-spaces');
  const copyBtn = panel.querySelector('#ocr-copy');

  sourceTa.value = initialText;

  // 面板尺寸自适应（0.11.9：按内容估算行数,钳到 max-height）
  // 原逻辑保留,估算基于原文长度即可(译文长度通常同量级)
  const PANEL_CHROME = 90;
  const TA_MIN = 200, TA_MAX = 390;
  const lineH = 22.4;
  const CHARS_PER_LINE = 40;
  let textLines = 1;
  for (const line of String(text || '').split('\n')) {
    textLines += Math.max(1, Math.ceil((line.length + 1) / CHARS_PER_LINE) || 1);
    if (textLines > 40) break;
  }
  const taH = Math.max(TA_MIN, Math.min(TA_MAX, textLines * lineH));
  panel.style.height = (taH + PANEL_CHROME) + 'px';

  // 屏定位（多屏 clamp，选区所在屏矩形约束四边）
  const MARGIN = 8;
  const mon = findDisplayCssAt(selCss.x + selCss.w / 2, selCss.y + selCss.h / 2);
  const pw = panel.offsetWidth;
  const ph = panel.offsetHeight;

  let left = Math.max(mon.x + MARGIN, selCss.x);
  if (left + pw > mon.x + mon.w - MARGIN) {
    left = Math.max(mon.x + MARGIN, mon.x + mon.w - MARGIN - pw);
  }
  let top;
  const topAbove = selCss.y - ph - MARGIN;
  const topBelow = selCss.y + selCss.h + MARGIN;
  if (topAbove >= mon.y + MARGIN) top = topAbove;
  else if (topBelow + ph <= mon.y + mon.h - MARGIN) top = topBelow;
  else top = Math.max(mon.y + MARGIN, mon.y + mon.h - MARGIN - ph);
  panel.style.left = left + 'px';
  panel.style.top = top + 'px';

  // 面板内交互不应触发 overlay 的 blur 隐藏
  panel.addEventListener('mousedown', (e) => e.stopPropagation());
  document.getElementById('ocr-close').addEventListener('click', () => {
    exitReadingMode();
    panel.remove();
  });

  // ── Tab 切换 ─────────────────────────────────────
  const showTab = (name) => {
    const isSource = name === 'source';
    tabSource.classList.toggle('active', isSource);
    tabTranslated.classList.toggle('active', !isSource);
    sourceTa.hidden = !isSource;
    translatedTa.hidden = isSource;
    // 焦点跟随
    (isSource ? sourceTa : translatedTa).focus();
  };
  tabSource.addEventListener('click', () => showTab('source'));
  tabTranslated.addEventListener('click', () => showTab('translated'));

  // ── 译文过期标记 ─────────────────────────────────
  // 原文改动 → 译文标 stale;新翻译时清 stale
  const markTranslatedStale = (stale) => {
    tabTranslated.setAttribute('data-stale', stale ? 'true' : 'false');
    translatedTa.setAttribute('data-stale', stale ? 'true' : 'false');
  };

  // ── 移除空格（保留兜底） ──────────────────────────
  // 0.11.9-b 后端已智能拼接,该按钮退化为清理残余不间断空格等;
  // toggle 语义不变(按一次清, 再按一次原文)。
  let originalText = sourceTa.value;
  trimBtn.dataset.trimmed = 'false';
  sourceTa.addEventListener('input', () => {
    originalText = sourceTa.value;
    if (trimBtn.dataset.trimmed === 'true') {
      trimBtn.dataset.trimmed = 'false';
      trimBtn.textContent = '移除空格';
    }
    // 用户改原文 → 译文过期
    if (translatedTa.value) markTranslatedStale(true);
    // 0.11.9-c：文本被手动改动,反向映射失效——停止 textarea→图 联动
    if (reading && !reading.panelDirty) {
      reading.panelDirty = true;
      // 提示条(可选):这里省略,仅通过 tab 视觉不主动做提示,避免面板拥挤
    }
  });

  // 0.11.9-c：textarea 选中变化 → 图上高亮反查
  sourceTa.addEventListener('select', () => syncSelectionFromPanel(sourceTa));
  sourceTa.addEventListener('keyup', () => syncSelectionFromPanel(sourceTa));
  sourceTa.addEventListener('click', () => syncSelectionFromPanel(sourceTa));
  trimBtn.addEventListener('click', () => {
    if (trimBtn.dataset.trimmed === 'true') {
      trimBtn.dataset.trimmed = 'false';
      trimBtn.textContent = '移除空格';
      sourceTa.value = originalText;
    } else {
      const trimmed = originalText
        .split(/\r?\n/)
        .map((line) => line.replace(/[^\S\r\n]+/g, '').trim())
        .join('\n')
        .replace(/\n{3,}/g, '\n\n');
      trimBtn.dataset.trimmed = 'true';
      trimBtn.textContent = '显示原文';
      sourceTa.value = trimmed;
    }
    sourceTa.focus();
  });

  // ── 复制（复制当前 tab 内容） ─────────────────────
  copyBtn.addEventListener('click', () => {
    const currentTa = sourceTa.hidden ? translatedTa : sourceTa;
    const value = currentTa.value;
    if (value) {
      navigator.clipboard.writeText(value).catch((e) => console.error('复制失败', e));
    }
    exitReadingMode();
    panel.remove();
  });

  // ── 翻译 ─────────────────────────────────────────
  let translating = false;
  const doTranslate = async () => {
    if (translating) return;
    const src = sourceTa.value.trim();
    if (!src || src === '（未识别到文字）') return;
    translating = true;
    translateBtn.disabled = true;
    translateBtn.textContent = '翻译中…';
    translatedTa.setAttribute('data-loading', 'true');
    translatedTa.value = '翻译中,请稍候…';
    showTab('translated');
    try {
      const dst = await translateText(src);
      translatedTa.value = dst;
      markTranslatedStale(false);
    } catch (e) {
      console.error('[screenshot] 翻译失败', e);
      translatedTa.value = `翻译失败：${e}`;
    } finally {
      translating = false;
      translateBtn.disabled = false;
      translateBtn.textContent = '翻译';
      translatedTa.removeAttribute('data-loading');
    }
  };
  translateBtn.addEventListener('click', doTranslate);

  // ── 一条龙：工具栏"翻译"按钮直接进面板并翻译 ──
  if (options.autoTranslate && text) {
    // 面板首次渲染后触发,不阻塞 UI
    setTimeout(doTranslate, 0);
  }

  // ── 0.11.9-c：启动阅读模式（有 words 数据才启用） ──
  if (result && Array.isArray(result.words) && result.words.length > 0) {
    enterReadingMode(result);
  }
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
  const clearBtn = dropdown.querySelector('.wm-clear');

  // 0.11.9-a：回填已有水印配置（水印现为单例配置,重新打开表单看到当前值）
  const existing = annot.getWatermark();
  if (existing) {
    if (textInput) textInput.value = existing.text;
    if (layoutSelect) layoutSelect.value = existing.layout;
    if (opacityRange) {
      opacityRange.value = Math.round(existing.opacity * 100);
      if (opacityVal) opacityVal.textContent = `${opacityRange.value}%`;
    }
  }
  // "清除水印"按钮只在已有水印时可点
  if (clearBtn) clearBtn.disabled = !existing;

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
    // 0.11.9-a：空文本 + 已有水印 → 视为清除(等同点"清除水印"按钮);空文本无水印才 focus 报"没填"
    if (!text) {
      if (annot.hasWatermark()) {
        annot.clearWatermark();
        if (clearBtn) clearBtn.disabled = true;
        redrawAnnotFull();
        updateUndoRedoButtons();
        backToList();
        dropdown.setAttribute('data-open', 'false');
      } else {
        textInput.focus();
      }
      return;
    }
    annot.commitWatermark({
      text,
      layout: layoutSelect.value,
      color: annot.getColor(),
      width: annot.getWidth(),
      opacity: parseInt(opacityRange.value, 10) / 100,
    });
    redrawAnnotFull();
    updateUndoRedoButtons();
    // 0.11.9-a：应用后回启用"清除"按钮
    if (clearBtn) clearBtn.disabled = false;
    backToList();
    dropdown.setAttribute('data-open', 'false');
  };
  if (applyBtn) applyBtn.addEventListener('click', apply);
  if (backBtn) backBtn.addEventListener('click', (e) => {
    e.stopPropagation();
    backToList();
  });
  // 0.11.9-a：清除水印按钮
  if (clearBtn) clearBtn.addEventListener('click', (e) => {
    e.stopPropagation();
    annot.clearWatermark();
    if (textInput) textInput.value = '';
    clearBtn.disabled = true;
    redrawAnnotFull();
    updateUndoRedoButtons();
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
  bind('btn-translate', doTranslateSelection);
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
      const tw = toolbar.offsetWidth;
      const th = toolbar.offsetHeight;
      // 0.11.9：按"鼠标当前所在屏"clamp（用 e.clientX/Y 找当前屏 CSS 矩形）。
      // 多屏混合 DPI 下，工具栏不会跨屏越界到另一块屏去。
      const mon = findDisplayCssAt(e.clientX, e.clientY);
      const MARGIN = 8;
      let left = e.clientX - offsetX;
      let top = e.clientY - offsetY;
      left = Math.max(mon.x + MARGIN, Math.min(left, mon.x + mon.w - tw - MARGIN));
      top = Math.max(mon.y + MARGIN, Math.min(top, mon.y + mon.h - th - MARGIN));
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
