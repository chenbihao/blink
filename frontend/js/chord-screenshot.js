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
  translateLines,
  copyToClipboard,
  frontendLog,
  invoke,
} from "./api.js";
import * as annot from "./annotation-engine.js";
import { ensureSpriteLoaded } from "./icon.js";
import { applyThemeFromConfig } from "./theme.js";

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
let sent = false;               // 防止复制/保存/钉图重复提交
let ocrBusy = false;            // 显式 OCR 请求门禁（不与输出提交共用）
let translationBusy = false;    // 图上译文请求中；完成前不允许输出半成品截图
let singleClickTimeout = null;  // 单击→200ms 后隐藏的定时器
let blurGuard = false;          // blur 事件短窗口防抖（避免重复触发）
// 标注绘制状态（物理像素）
let annotStartX = 0, annotStartY = 0;
let annotCurrentX = 0, annotCurrentY = 0;

// ── 预热 OCR 状态（0.11.10-b）──────────────────────────
// 选区拖完后后台异步跑 OCR，让「识别」/「翻译」按钮点击时秒响应。
// `ocrPrewarm` 存的是 Promise 而非 result——已完成 / 进行中都 await 同一个 Promise。
// resetState 里清；新选区（重选或退出标注）会替换。
let ocrPrewarm = null;          // Promise<OcrResult> | null
let screenshotConfig = { prewarmOcr: true };  // 默认开；overlay 显示时按需读一次覆盖

// 选区/OCR 异步结果都绑定 revision。重选、移动、缩放后旧 Promise 即使晚到也不能回填新选区。
let selectionRevision = 0;
let translationRevision = 0;

// 选取工具的拖拽状态：move=移动，resize=八向缩放，new=选区外重新框选。
let selectionInteraction = null;
const SELECTION_HANDLE_SIZE = 8;
const MIN_SELECTION_SIZE = 5;

// 预热触发的最小选区面积（物理像素）——太小的图大概率是纯图无字,浪费一次 OCR
const PREWARM_MIN_WIDTH = 100;
const PREWARM_MIN_HEIGHT = 50;

// ── 初始化 ────────────────────────────────────────────

// 图标 sprite（工具栏按钮走 Lucide 图标；fire-and-forget，加载失败降级为空图标）
ensureSpriteLoaded();

// 应用主题（截图 overlay 的 token 引用如 --accent 等随用户主题切换）
applyThemeFromConfig();

annot.init(annotCanvas);
// 0.11.10-a：默认工具设为 'select'（选取），作为标注/OCR 阅读的中立入口。
// annotation-engine 内部默认是 'rect'，这里覆盖成新契约的默认值。
annot.setTool('select');
// hit-canvas data-tool 与工具同步（CSS 据此决定 pointer-events 是否接收鼠标）
{
  const _hc = document.getElementById('ocr-hit-canvas');
  if (_hc) _hc.setAttribute('data-tool', 'select');
}

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
  ocrBusy = false;
  translationBusy = false;
  selCss = null;
  selectionInteraction = null;
  selectionRevision++;
  translationRevision++;
  canvas.style.cursor = 'crosshair';
  canvas.setAttribute('data-tool', 'select');
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
  // 0.11.10-b：清预热缓存(新一轮 overlay 从零开始)
  ocrPrewarm = null;
  // 0.11.10-e：清 OCR 缓存
  ocrResultCache = null;
  // 清所有浮层与工具高亮；复用窗口时不能沿用上一轮的工具/视图状态。
  document.querySelectorAll('.dropdown').forEach((dropdown) => {
    dropdown.setAttribute('data-open', 'false');
    dropdown.removeAttribute('data-placement');
  });
  document.querySelectorAll('.split-main, .tool-direct').forEach((button) => button.classList.remove('active'));
  document.querySelectorAll('.dropdown-item[data-tool]').forEach((button) => button.classList.remove('active'));
  const selectMain = document.getElementById('select-main');
  if (selectMain) selectMain.classList.add('active');
  annot.setTool('select');
  annot.clearOverlay();
  updateOverlayButtonsActive();
  updateOutputButtonsDisabled();
  const _hc = document.getElementById('ocr-hit-canvas');
  if (_hc) _hc.setAttribute('data-tool', 'select');
}

function loadScreenshot() {
  errorHint.style.display = 'none';
  // 0.11.10-b：并行读一下截图配置(不阻塞图像加载)。overlay 显示到用户拖完选区
  // 至少 500ms+,SQLite KV 读 < 5ms,mouseup 时几乎必然已就绪;失败降级默认值。
  invoke('get_config_section', { key: 'screenshot:config' })
    .then((val) => {
      if (val && typeof val === 'object') {
        screenshotConfig.prewarmOcr = val.prewarmOcr !== false;
      }
    })
    .catch((e) => console.debug('[screenshot] 读 screenshot:config 失败,用默认值', e));

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

  // 0.11.10-b：后台预热 OCR（不阻塞 UI）——让「识别」/「翻译」秒响应。
  // 触发条件:配置开 + 选区面积达标(太小的图大概率是纯图无字,浪费)。
  triggerOcrPrewarm(pw, ph);
}

/**
 * 后台预热 OCR:异步调用 ocr_image,结果存 ocrPrewarm Promise。
 * 已有 Promise 则不重复触发(用户快速重选场景 exitAnnotationMode 会清缓存,不会走到重复分支)。
 * 失败静默 —— 用户显式点[识别]时会走正常路径,不影响主链路。
 */
function triggerOcrPrewarm(pw, ph) {
  if (!screenshotConfig.prewarmOcr) return;
  if (pw < PREWARM_MIN_WIDTH || ph < PREWARM_MIN_HEIGHT) {
    console.debug('[screenshot] 预热 OCR 跳过(选区过小)', { pw, ph });
    return;
  }
  if (ocrPrewarm) return;   // 已在跑,复用
  const revision = selectionRevision;
  const startTs = performance.now();
  ocrPrewarm = new Promise((resolve) => {
    compositeSelection((pngBytes) => {
      ocrImage(pngBytes)
        .then((result) => {
          if (revision !== selectionRevision) {
            console.debug('[screenshot] 丢弃旧选区 OCR 预热结果', { revision, current: selectionRevision });
            resolve(null);
            return;
          }
          const elapsed = Math.round(performance.now() - startTs);
          console.info('[screenshot] OCR 预热完成', { ms: elapsed, textLen: result?.text?.length ?? 0 });
          resolve(result);
        })
        .catch((err) => {
          console.warn('[screenshot] OCR 预热失败(用户点识别时会重试)', err);
          resolve(null);   // 缓存失败标记为 null,doOcrSelection 里识别到 null 会走正常路径
        });
    });
  });
}

/** 退出标注模式（清除选区，回到可拖选状态） */
function exitAnnotationMode() {
  console.debug('[screenshot] exitAnnotationMode');
  isAnnotating = false;
  selCss = null;
  selectionInteraction = null;
  selectionRevision++;
  translationRevision++;
  canvas.style.cursor = 'crosshair';
  annotCanvas.style.display = 'none';
  annotCanvas.width = 0;
  annotCanvas.height = 0;
  toolbar.style.display = 'none';
  sizeHint.style.display = 'none';
  // 0.11.10-b：清预热缓存(旧选区结果不能给新选区用)
  ocrPrewarm = null;
  ocrBusy = false;
  translationBusy = false;
  updateOutputButtonsDisabled();
  // 0.11.10-e：清 OCR 缓存 + 关阅读模式(overlay 生命周期与选区一致)
  ocrResultCache = null;
  exitReadingMode();
  annot.clearOverlay();
  updateOverlayButtonsActive();
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

function selectionCursor(handle) {
  if (handle === 'n' || handle === 's') return 'ns-resize';
  if (handle === 'e' || handle === 'w') return 'ew-resize';
  if (handle === 'nw' || handle === 'se') return 'nwse-resize';
  if (handle === 'ne' || handle === 'sw') return 'nesw-resize';
  return 'move';
}

/** 命中选区八向缩放热区。坐标和 rect 都是视口 CSS 像素。 */
function getSelectionHandle(x, y, rect) {
  if (!rect) return null;
  const m = SELECTION_HANDLE_SIZE;
  if (x < rect.x - m || x > rect.x + rect.w + m ||
      y < rect.y - m || y > rect.y + rect.h + m) return null;
  const nearL = Math.abs(x - rect.x) <= m;
  const nearR = Math.abs(x - (rect.x + rect.w)) <= m;
  const nearT = Math.abs(y - rect.y) <= m;
  const nearB = Math.abs(y - (rect.y + rect.h)) <= m;
  if (nearT && nearL) return 'nw';
  if (nearT && nearR) return 'ne';
  if (nearB && nearL) return 'sw';
  if (nearB && nearR) return 'se';
  if (nearT) return 'n';
  if (nearB) return 's';
  if (nearL) return 'w';
  if (nearR) return 'e';
  return null;
}

function invalidateSelectionContent() {
  selectionRevision++;
  translationRevision++;
  ocrPrewarm = null;
  ocrResultCache = null;
  ocrBusy = false;
  translationBusy = false;
  updateOutputButtonsDisabled();
  const panel = document.getElementById('ocr-panel');
  if (panel) panel.remove();
  exitReadingMode();
  annot.clearOverlay();
  updateOverlayButtonsActive();
  annotCanvas.style.display = 'none';
  toolbar.style.display = 'none';
  sizeHint.style.display = 'none';
  toolbar.removeAttribute('data-user-moved');
  toolbar.style.left = '';
  toolbar.style.top = '';
}

function beginSelectionInteraction(kind, e, handle = null) {
  if (!selCss) return;
  const original = { ...selCss };
  selectionInteraction = {
    kind,
    handle,
    activated: false,
    startX: e.offsetX,
    startY: e.offsetY,
    original,
    monitor: findDisplayCssAt(original.x + original.w / 2, original.y + original.h / 2),
  };
  isDragging = false;
  if (kind === 'new') {
    startX = e.offsetX;
    startY = e.offsetY;
    endX = startX;
    endY = startY;
  }
  canvas.style.cursor = kind === 'resize' ? selectionCursor(handle) : (kind === 'move' ? 'move' : 'crosshair');
}

function updateSelectionInteraction(e) {
  if (!selectionInteraction) return;
  const totalDx = e.offsetX - selectionInteraction.startX;
  const totalDy = e.offsetY - selectionInteraction.startY;
  if (!selectionInteraction.activated) {
    if (Math.hypot(totalDx, totalDy) < 3) return;
    selectionInteraction.activated = true;
    invalidateSelectionContent();
    if (selectionInteraction.kind === 'new') {
      isDragging = true;
      selCss = null;
    }
  }
  if (selectionInteraction.kind === 'new') {
    endX = e.offsetX;
    endY = e.offsetY;
    drawSelection();
    return;
  }

  const { original, monitor, handle } = selectionInteraction;
  const dx = e.offsetX - selectionInteraction.startX;
  const dy = e.offsetY - selectionInteraction.startY;
  if (selectionInteraction.kind === 'move') {
    const x = Math.max(monitor.x, Math.min(original.x + dx, monitor.x + monitor.w - original.w));
    const y = Math.max(monitor.y, Math.min(original.y + dy, monitor.y + monitor.h - original.h));
    selCss = { x, y, w: original.w, h: original.h };
  } else {
    let left = original.x;
    let top = original.y;
    let right = original.x + original.w;
    let bottom = original.y + original.h;
    if (handle.includes('w')) left = Math.max(monitor.x, Math.min(e.offsetX, right - MIN_SELECTION_SIZE));
    if (handle.includes('e')) right = Math.min(monitor.x + monitor.w, Math.max(e.offsetX, left + MIN_SELECTION_SIZE));
    if (handle.includes('n')) top = Math.max(monitor.y, Math.min(e.offsetY, bottom - MIN_SELECTION_SIZE));
    if (handle.includes('s')) bottom = Math.min(monitor.y + monitor.h, Math.max(e.offsetY, top + MIN_SELECTION_SIZE));
    selCss = { x: left, y: top, w: right - left, h: bottom - top };
  }
  drawFinalSelection();
  const dpr = window.devicePixelRatio || 1;
  sizeHint.textContent = `${Math.round(selCss.w * dpr)} × ${Math.round(selCss.h * dpr)}`;
  sizeHint.style.display = 'block';
  sizeHint.style.left = (selCss.x + 4) + 'px';
  sizeHint.style.top = (selCss.y > 24 ? selCss.y - 22 : selCss.y + 4) + 'px';
}

function finishSelectionInteraction(e) {
  if (!selectionInteraction) return false;
  const { kind, activated } = selectionInteraction;
  if (!activated) {
    selectionInteraction = null;
    canvas.style.cursor = annot.getTool() === 'select' ? 'default' : 'crosshair';
    return true;
  }
  if (kind === 'new') {
    endX = e.offsetX;
    endY = e.offsetY;
    selCss = norm(startX, startY, endX, endY);
    isDragging = false;
  }
  selectionInteraction = null;
  if (!selCss || selCss.w < MIN_SELECTION_SIZE || selCss.h < MIN_SELECTION_SIZE) {
    exitAnnotationMode();
    return true;
  }
  // 选区移动、重框、resize 都改变了实际裁剪内容，需要重建坐标系并重新预热 OCR。
  enterAnnotationMode({ ...selCss });
  return true;
}

function updateSelectionCursor(x, y) {
  if (annot.getTool() !== 'select') {
    canvas.style.cursor = 'crosshair';
    return;
  }
  if (!isAnnotating || !selCss) {
    canvas.style.cursor = 'crosshair';
    return;
  }
  const handle = getSelectionHandle(x, y, selCss);
  if (handle) {
    canvas.style.cursor = selectionCursor(handle);
  } else {
    // 选取工具下，选区内外都允许移动选区；只有选区内会进入 hit-canvas（已 OCR 时）。
    canvas.style.cursor = 'move';
  }
}

canvas.addEventListener('mousedown', (e) => {
  if (!screenshot || e.button !== 0) return;

  const tool = annot.getTool();
  if (isAnnotating && selCss && tool === 'select') {
    const handle = getSelectionHandle(e.offsetX, e.offsetY, selCss);
    if (handle) {
      beginSelectionInteraction('resize', e, handle);
      return;
    }
    if (pointInRect(e.offsetX, e.offsetY, selCss)) {
      // 未 OCR 时选区内拖动 = 移动选区；已 OCR 时选区内由 #ocr-hit-canvas 接管选词，
      // 主 canvas 收不到 pointerdown，所以这里的 move 只在 B 状态（无 reading）生效。
      beginSelectionInteraction('move', e);
      return;
    }
    // 选区外拖动 = 移动整个选区（不再重新框选）。重选走 ESC 关闭后重新 Alt+A。
    // 单击不拖动（< 3px）仍 no-op，保留 0.11.10-f 的误点保护。
    beginSelectionInteraction('move', e);
    return;
  }

  // 有选区状态下点击选区内 → 启动标注绘制
  if (isAnnotating && selCss && pointInRect(e.offsetX, e.offsetY, selCss)) {
    // 0.11.8-b：watermark 是"面板驱动"工具，不响应 canvas 拖动
    if (tool === 'watermark') return;
    const dpr = window.devicePixelRatio || 1;
    annotStartX = (e.offsetX - selCss.x) * dpr;
    annotStartY = (e.offsetY - selCss.y) * dpr;
    annotCurrentX = annotStartX;
    annotCurrentY = annotStartY;
    annot.startDraw(annotStartX, annotStartY);
    isAnnotDragging = true;
    return;
  }

  // 0.11.10-f/k：其它标注工具下点选区外仍 no-op；选取工具已在上方进入重选分支。
  if (isAnnotating && selCss) {
    console.debug('[screenshot] annotation tool click outside selection → no-op');
    return;
  }

  // 启动选区拖拽（无选区态,或选区已 exit 后回退到拖选态）
  isDragging = true;
  sent = false;
  startX = e.offsetX;
  startY = e.offsetY;
  endX = startX;
  endY = startY;
});

canvas.addEventListener('mousemove', (e) => {
  if (!screenshot) return;

  updateSelectionCursor(e.offsetX, e.offsetY);

  if (selectionInteraction) {
    updateSelectionInteraction(e);
    return;
  }

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
  if (!selectionInteraction) canvas.style.cursor = annot.getTool() === 'select' ? 'default' : 'crosshair';
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

  if (finishSelectionInteraction(e)) return;

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

// 右键：reading 模式激活时弹菜单（选区边缘 clip-path 漏过来的右键），否则取消截图。
canvas.addEventListener('contextmenu', (e) => {
  e.preventDefault();
  if (reading) {
    const selText = getReadingSelectionText();
    showReadingContextMenu(selText || null, e);
  } else {
    doCancel();
  }
});

document.addEventListener('keydown', (e) => {
  // 面板中有真正可编辑的文本选区时交给浏览器默认复制，否则复制图上 word 选区。
  if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === 'c') {
    const tgt = e.target;
    const isTextField = tgt && (tgt.tagName === 'INPUT' || tgt.tagName === 'TEXTAREA' || tgt.isContentEditable);
    const hasNativeSelection = isTextField && (
      tgt.isContentEditable ||
      (typeof tgt.selectionStart === 'number' && tgt.selectionStart !== tgt.selectionEnd)
    );
    if (!hasNativeSelection && copyReadingSelection()) {
      e.preventDefault();
      return;
    }
  }
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
    return;
  }
  // 0.11.10-e：`E` 键召唤面板抽屉(与工具栏[≡]等价)。
  //   忽略在 input/textarea/contentEditable 中的按键——用户在面板 textarea 或
  //   文本标注框里输入 e/E 字母不该触发面板 toggle。
  if ((e.key === 'e' || e.key === 'E') && !e.ctrlKey && !e.metaKey && !e.altKey && isAnnotating) {
    const tgt = e.target;
    if (tgt && (tgt.tagName === 'INPUT' || tgt.tagName === 'TEXTAREA' || tgt.isContentEditable)) return;
    e.preventDefault();
    doPanelToggle();
  }
});

// 失焦兜底——但避免重复调用
// 注意：blur 不调 doCancel，只调 hideScreenshotOverlay（不设 sent）。
// 如果用户在有选区时失焦，应该保留选区（不取消），而不是默默地结束会话。
// 截图 overlay 是透明窗口，失焦可能只是用户不小心点到别处，不应直接取消。
//
// 0.11.10-f 例外:面板打开时 blur 保留(不 hide)——用户拿 OCR 面板去查词/翻译,
// 面板内 focus 输入框 window 就会 blur,不能因此关掉 overlay。判定方式:DOM 里
// 有 #ocr-panel 或 text-dropdown 展开态即视为"有交互面板"。
// 后续 0.11.10-c/d 引入 overlayLayer 嵌图后,同样纳入豁免。
window.addEventListener('blur', () => {
  if (blurGuard) return;
  blurGuard = true;
  setTimeout(() => { blurGuard = false; }, 500);

  // 有交互面板/输入态时保留 overlay(§2.6 表格)
  if (hasActivePanel()) {
    console.debug('[screenshot] window blur ignored (active panel)');
    return;
  }

  console.debug('[screenshot] window blur, hiding overlay');
  // 直接隐藏，不经过 doCancel（不设 sent，不干扰后续操作）
  hideScreenshotOverlay().catch((e) => console.error('hideScreenshotOverlay 失败', e));
});

/** 判断是否有主动召唤出的交互面板/输入态。
 *  0.11.10-f:blur 时用它决定是否保留 overlay。
 *  当前覆盖:OCR 面板 + 文本输入框 + 水印表单 + overlay 嵌图激活态。
 *  0.11.10-e:overlayLayer 处于 mode!=null(有嵌图显示中)也视为"活动内容",
 *    用户看图上译文时点其他窗口不应关掉截图 overlay。 */
function hasActivePanel() {
  if (document.getElementById('ocr-panel')) return true;
  if (document.querySelector('.text-annot-input')) return true;
  const wm = document.getElementById('text-dropdown');
  if (wm && wm.getAttribute('data-view') === 'watermark' && wm.getAttribute('data-open') === 'true') return true;
  if (annot.isOverlayActive()) return true;
  return false;
}

// ── 工具栏动作 ────────────────────────────────────────

function ensureOutputReady() {
  const overlay = annot.getOverlay();
  const activeTranslation = translationBusy && overlay && overlay.mode === 'translated';
  if (!ocrBusy && !activeTranslation) return true;
  showTransientHint(activeTranslation ? '翻译尚未完成' : '识别尚未完成');
  return false;
}

function doCopySelection() {
  if (!selCss || sent || !ensureOutputReady()) return;
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
  if (!selCss || sent || !ensureOutputReady()) return;
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
  if (!selCss || sent || !ensureOutputReady()) return;
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

/**
 * 点[识别]——OCR → 面板（原文 tab），图上不嵌字。
 *
 * 心智：
 *   - 首次点 → OCR → 面板展开（原文 tab）→ overlay mode=null（不嵌图）
 *   - 面板已开 + 原文 tab → 关闭面板 + 清 overlay
 *   - 面板已开 + 译文 tab → 切到原文 tab + 清 overlay + 隐藏 adv
 *
 * 复用预热缓存——已完成/进行中都 await 同一 Promise,0ms 感知。
 */
function doIdentifySelection() {
  if (!selCss) return;
  const existingPanel = document.getElementById('ocr-panel');
  if (existingPanel && ocrResultCache) {
    const tabSource = existingPanel.querySelector('.ocr-tab[data-tab="source"]');
    const isSourceActive = tabSource && tabSource.classList.contains('active');
    if (isSourceActive) {
      existingPanel.remove();
      // 关闭面板时清 overlay（否则图上残留译文）
      annot.setOverlayMode(null);
      redrawAnnotFull();
      updateToolbarButtonActive();
    } else {
      // 从翻译切到识别：清 overlay + 隐藏 adv
      annot.setOverlayMode(null);
      redrawAnnotFull();
      const adv = existingPanel.querySelector('.ocr-panel-adv');
      if (adv) adv.style.display = 'none';
      if (tabSource) tabSource.click();
    }
    return;
  }
  ocrBusy = true;
  updateOutputButtonsDisabled();
  showSelLoading('识别中…');
  const revision = selectionRevision;
  const onResult = (result) => {
    if (revision !== selectionRevision) return;
    activateOverlay(result, {
      showOverlay: false,      // 识别路径默认不嵌图
      panelTab: 'source',
      openPanel: true,
      autoTranslate: false,
    });
    ocrBusy = false;
    updateOutputButtonsDisabled();
    hideSelLoading();
  };
  if (ocrPrewarm) {
    console.debug('[screenshot] doIdentify 走预热缓存');
    ocrPrewarm.then((result) => {
      if (revision !== selectionRevision) return;
      if (result) { onResult(result); return; }
      _runOcrFresh({ kind: 'identify', revision });
    }).catch((err) => {
      if (revision !== selectionRevision) return;
      console.error('[screenshot] OCR 预热 Promise 异常', err);
      ocrBusy = false;
      updateOutputButtonsDisabled();
      hideSelLoading();
      showTransientHint('识别失败');
    });
    return;
  }
  _runOcrFresh({ kind: 'identify', revision });
}

/**
 * 点[翻译]——OCR + 翻译 → 面板（译文 tab）+ overlay 嵌译文。
 *
 * 心智：
 *   - 首次点 → OCR → 自动翻译 → 面板展开（译文 tab）→ overlay mode=translated
 *   - 面板已开 + 译文 tab → 关闭面板
 *   - 面板已开 + 原文 tab → 切到译文 tab + 确保 overlay=translated + 显示 adv
 */
function doOverlayTranslate() {
  if (!selCss) return;
  const existingPanel = document.getElementById('ocr-panel');
  if (existingPanel && ocrResultCache) {
    const tabTranslated = existingPanel.querySelector('.ocr-tab[data-tab="translated"]');
    const isTranslatedActive = tabTranslated && tabTranslated.classList.contains('active');
    if (isTranslatedActive) {
      existingPanel.remove();
      updateToolbarButtonActive();
    } else {
      // 从识别切到翻译：确保 overlay = translated + 显示 adv
      const overlay = annot.getOverlay();
      if (overlay && overlay.mode !== 'translated') {
        annot.setOverlayMode('translated');
        redrawAnnotFull();
      }
      const adv = existingPanel.querySelector('.ocr-panel-adv');
      if (adv) adv.style.display = '';
      if (tabTranslated) tabTranslated.click();
    }
    return;
  }
  ocrBusy = true;
  updateOutputButtonsDisabled();
  showSelLoading('识别中…');
  const revision = selectionRevision;
  const onResult = (result) => {
    if (revision !== selectionRevision) return;
    activateOverlay(result, {
      showOverlay: true,       // 翻译路径默认嵌图（mode='translated'）
      panelTab: 'translated',
      openPanel: true,
      autoTranslate: true,     // 立刻翻译，不再有切 mode 不动作的中间态
    });
    ocrBusy = false;
    updateOutputButtonsDisabled();
  };
  if (ocrPrewarm) {
    console.debug('[screenshot] doTranslate 走预热缓存');
    ocrPrewarm.then((result) => {
      if (revision !== selectionRevision) return;
      if (result) { onResult(result); return; }
      _runOcrFresh({ kind: 'translate', revision });
    }).catch((err) => {
      if (revision !== selectionRevision) return;
      console.error('[screenshot] OCR 预热 Promise 异常', err);
      ocrBusy = false;
      updateOutputButtonsDisabled();
      hideSelLoading();
      showTransientHint('识别失败');
    });
    return;
  }
  _runOcrFresh({ kind: 'translate', revision });
}

// 保留旧名字作为别名（尚未替换的调用点走此路径），行为 = 新入口
const doOcrSelection = doIdentifySelection;
const doTranslateSelection = doOverlayTranslate;

/** 走完整合成 → OCR → 展示的正常路径(预热未开或失败时兜底)。
 *  opts.kind 决定走识别路径还是翻译路径（与 activateOverlay 的 opts 对齐）。 */
function _runOcrFresh(opts = {}) {
  const kind = opts.kind || 'identify';
  const revision = opts.revision ?? selectionRevision;
  showSelLoading('识别中…');
  compositeSelection((pngBytes) => {
    ocrImage(pngBytes)
      .then((result) => {
        if (revision !== selectionRevision) {
          console.debug('[screenshot] 丢弃旧选区 OCR 结果', { revision, current: selectionRevision });
          return;
        }
        if (kind === 'translate') {
          showSelLoading('翻译中…');
          activateOverlay(result, {
            showOverlay: true,
            panelTab: 'translated',
            openPanel: true,
            autoTranslate: true,
          });
        } else {
          hideSelLoading();
          activateOverlay(result, {
            showOverlay: false,
            panelTab: 'source',
            openPanel: true,
            autoTranslate: false,
          });
        }
      })
      .catch((err) => {
        if (revision === selectionRevision) {
          showTransientHint('识别失败');
        }
        hideSelLoading();
        console.error('[screenshot] ocr 失败', err);
      })
      .finally(() => {
        if (revision === selectionRevision) {
          ocrBusy = false;
          updateOutputButtonsDisabled();
        }
      });
  });
}

/**
 * 0.11.10-c（重构）：把 OCR 结果落地为 overlayLayer + reading + 面板。
 *
 * 心智：面板是主交互面，overlay 是面板的镜像。本函数不再隐式决定 mode/panel 状态，
 * 改由调用方通过 opts 显式声明四件事：
 *   - showOverlay: 是否在图上嵌字（识别路径=false / 翻译路径=true）
 *   - panelTab:    面板默认展开哪个 tab（'source' | 'translated'）
 *   - openPanel:   是否自动展开面板（识别/翻译主路径=true；被动重算=false）
 *   - autoTranslate: 是否立刻触发翻译（翻译路径=true）
 *
 * 空 lines / 空 text → 提示"未识别到文字",不建立 overlay。
 */
function activateOverlay(result, opts = {}) {
  const lines = (result && Array.isArray(result.lines)) ? result.lines : [];
  const nonEmpty = lines.filter((ln) => ln && ln.text && ln.rect && ln.rect.w > 0 && ln.rect.h > 0);
  if (nonEmpty.length === 0) {
    showTransientHint('未识别到文字');
    return false;
  }
  // overlay 层数据始终建立（reading/翻译都要用），但 mode 由 showOverlay 决定：
  // showOverlay=false → mode=null（图上不渲染，但 lines 保留供面板「在图上显示」勾选时启用）。
  const mode = opts.showOverlay ? (opts.panelTab === 'translated' ? 'translated' : 'source') : null;
  annot.setOverlay({
    lines: nonEmpty.map((ln) => ({
      rect: { x: ln.rect.x, y: ln.rect.y, w: ln.rect.w, h: ln.rect.h },
      srcText: ln.text,
    })),
    mode,
  });
  // 缓存 OCR 全量 result(供面板召唤时复用,不需要重跑 OCR)
  ocrResultCache = result;
  // overlay 建立后即进入"reading 底座"——hit-canvas 激活,
  // select 工具下承担 word 拖选（点空白用 nearestWordByLine 起选）。
  if (result && Array.isArray(result.words) && result.words.length > 0) {
    enterReadingMode(result);
  }
  redrawAnnotFull();
  updateOverlayButtonsActive();
  if (opts.openPanel) {
    showOcrResult(result, { tab: opts.panelTab || 'source', showOverlay: opts.showOverlay });
  }
  if (opts.autoTranslate) {
    requestOverlayTranslation();
  }
  return true;
}

/**
 * 0.11.10-d：批量翻译 overlayLayer 里所有 line 的 srcText,拿到后一次 setTranslations 回填。
 *
 * 0.11.10-g:改走后端 `translate_lines` 批量入口——一次 IPC 拿 N 行译文,
 * 后端内部并发调 translate_text tool。相比阶段二逐行串行(N 次 IPC)体验大幅改善。
 * 单行失败后端自动降级到原文(与前端语义一致)。
 */
/** 有效文本判断：空字符串和纯空白都视为未翻译。 */
function hasText(value) {
  return typeof value === 'string' && value.trim().length > 0;
}

function requestOverlayTranslation(targetLang) {
  const revision = ++translationRevision;
  translationBusy = true;
  updateOutputButtonsDisabled();
  showSelLoading('翻译中…');
  translateOverlayLines(targetLang, revision)
    .catch((e) => {
      if (revision !== translationRevision) return;
      showTransientHint('翻译失败');
      console.error('[screenshot] overlay translate 失败', e);
    })
    .finally(() => {
      if (revision !== translationRevision) return;
      translationBusy = false;
      updateOutputButtonsDisabled();
      hideSelLoading();
    });
}

async function translateOverlayLines(targetLang, revision = ++translationRevision) {
  const selectionAtStart = selectionRevision;
  const current = annot.getOverlay();
  if (!current || current.lines.length === 0) return;
  // 收集需要翻译的行(已有非空 dstText 的复用不重跑)
  const srcs = current.lines.map((l) => hasText(l.dstText) ? '' : (l.srcText || ''));
  const needCount = srcs.filter((s) => hasText(s)).length;
  if (needCount === 0) return;

  const started = performance.now();
  let dsts;
  try {
    dsts = await translateLines(srcs, targetLang || null);
  } catch (e) {
    console.warn('[screenshot] translateLines 失败,降级到逐行单调', e);
    // 兜底:逐行单调 translate_text 保底,避免"翻译按钮点了没反应"
    dsts = [];
    for (let i = 0; i < srcs.length; i++) {
      if (!hasText(srcs[i])) { dsts.push(''); continue; }
      try {
        dsts.push(await translateText(srcs[i], targetLang || null));
      } catch (_) {
        dsts.push(srcs[i]);   // 单行失败降级到原文
      }
    }
  }
  // 用户已重选/移动/缩放或发起更新一轮翻译时，旧结果不得污染当前 overlay。
  if (selectionAtStart !== selectionRevision || revision !== translationRevision) {
    console.debug('[screenshot] 丢弃过期翻译结果', { revision, current: translationRevision });
    return;
  }
  const latest = annot.getOverlay();
  if (!latest || latest.lines.length !== current.lines.length) return;
  const merged = latest.lines.map((l, i) => hasText(l.dstText) ? l.dstText : (dsts[i] || l.srcText));
  annot.setOverlayTranslations(merged, targetLang || null);
  redrawAnnotFull();
  updateOverlayButtonsActive();
  tracing_debug('translateOverlayLines 完成', { lines: needCount, ms: Math.round(performance.now() - started) });
}

function tracing_debug(msg, extra) {
  console.info('[screenshot] ' + msg, extra || '');
}

/** 缓存最近一次 OCR 完整结果——供面板召唤(阶段二 e)复用而不需要重跑 OCR */
let ocrResultCache = null;

/** 简易临时提示(选区附近,2 秒后自动消失)。
 *  有选区时定位到选区顶部居中(工具栏在右下,顶部不冲突);空间不足翻到底部。
 *  无选区时回退屏幕中心。 */
/**
 * 选区中央加载转圈：在选区正中显示 spinner + 文案。
 * @param {string} text 显示文案（如"识别中…"/"翻译中…"）
 */
function showSelLoading(text) {
  const el = document.getElementById('sel-loading');
  if (!el || !selCss) return;
  const label = el.querySelector('.sel-loading-text');
  if (label) label.textContent = text;
  // 定位到选区中心
  el.style.left = (selCss.x + selCss.w / 2) + 'px';
  el.style.top = (selCss.y + selCss.h / 2) + 'px';
  el.hidden = false;
}

/** 隐藏选区加载转圈。 */
function hideSelLoading() {
  const el = document.getElementById('sel-loading');
  if (el) el.hidden = true;
}

function showTransientHint(msg) {
  errorHint.textContent = msg;
  errorHint.style.display = 'block';
  errorHint.style.background = 'rgba(50,50,50,0.85)';
  // 先隐藏测量自然宽高
  errorHint.style.left = '-9999px';
  errorHint.style.top = '-9999px';
  errorHint.style.transform = 'none';

  requestAnimationFrame(() => {
    if (selCss) {
      const MARGIN = 8;
      const ew = errorHint.offsetWidth;
      const eh = errorHint.offsetHeight;
      const mon = findDisplayCssAt(selCss.x + selCss.w / 2, selCss.y + selCss.h / 2);
      // 水平：选区居中，clamp 到屏幕内
      let left = selCss.x + (selCss.w - ew) / 2;
      left = Math.max(mon.x + MARGIN, Math.min(left, mon.x + mon.w - ew - MARGIN));
      // 垂直：选区上方优先（工具栏在右下，上方空旷）
      let top = selCss.y - eh - MARGIN;
      if (top < mon.y + MARGIN) {
        // 上方不够 → 放选区下方
        top = selCss.y + selCss.h + MARGIN;
      }
      errorHint.style.left = left + 'px';
      errorHint.style.top = top + 'px';
    } else {
      // 无选区：回退屏幕中心
      errorHint.style.left = '50%';
      errorHint.style.top = '50%';
      errorHint.style.transform = 'translate(-50%, -50%)';
    }
  });

  setTimeout(() => {
    errorHint.style.display = 'none';
    errorHint.style.background = '';
    errorHint.style.transform = '';
  }, 2000);
}

function updateOutputButtonsDisabled() {
  const overlay = annot.getOverlay();
  const disabled = ocrBusy || (translationBusy && overlay && overlay.mode === 'translated');
  ['btn-save', 'btn-pin', 'btn-copy'].forEach((id) => {
    const btn = document.getElementById(id);
    if (btn) btn.disabled = disabled;
  });
}

/** 工具栏「识别」/「翻译」按钮高亮态：跟随面板当前 tab。
 *  面板关 → 两个都不亮；面板开 → 当前 tab 对应的按钮亮。 */
function updateToolbarButtonActive() {
  const panel = document.getElementById('ocr-panel');
  const btnOcr = document.getElementById('btn-ocr');
  const btnTr = document.getElementById('btn-translate');
  if (!panel) {
    if (btnOcr) btnOcr.classList.remove('active');
    if (btnTr) btnTr.classList.remove('active');
    return;
  }
  const tabTranslated = panel.querySelector('.ocr-tab[data-tab="translated"]');
  const isTranslatedActive = tabTranslated && tabTranslated.classList.contains('active');
  if (btnOcr) btnOcr.classList.toggle('active', !isTranslatedActive);
  if (btnTr) btnTr.classList.toggle('active', isTranslatedActive);
}

/** 把 overlay 里已经翻译好的 line.dstText 回填到面板译文 textarea。
 *  只在面板存在且不在 loading 态时生效；面板关闭/翻译中都不动它。 */
function syncPanelTranslatedFromOverlay() {
  const overlay = annot.getOverlay();
  const translatedReady = !!(overlay && overlay.lines.length > 0 && overlay.lines.every((line) => hasText(line.dstText)));
  const panel = document.getElementById('ocr-panel');
  if (!translatedReady || !panel) return;
  const translatedTa = panel.querySelector('#ocr-textarea-translated');
  if (!translatedTa || translatedTa.getAttribute('data-loading') === 'true') return;
  translatedTa.value = overlay.lines.map((line) => line.dstText).join('\n');
  translatedTa.removeAttribute('data-stale');
  const tab = panel.querySelector('.ocr-tab[data-tab="translated"]');
  if (tab) tab.removeAttribute('data-stale');
}

/** 兼容包装：同步工具栏按钮 + 面板译文（旧调用点语义不变）。
 *  优先在新代码里分别调用上面两个细粒度函数。 */
function updateOverlayButtonsActive() {
  updateToolbarButtonActive();
  syncPanelTranslatedFromOverlay();
}

let cancelInProgress = false;
function doCancel() {
  if (cancelInProgress) return;
  cancelInProgress = true;
  setTimeout(() => { cancelInProgress = false; }, 2000);
  console.info('[screenshot] cancel');
  hideSelLoading();
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
  hitCanvas.removeAttribute('data-resizing');
  hitCtx.clearRect(0, 0, hitCanvas.width, hitCanvas.height);
  reading = null;
}

// ── hit-canvas 事件（幂等绑定，模块生命周期只装一次） ─
let hitEventsBound = false;
function bindHitCanvasEvents() {
  if (hitEventsBound) return;
  hitEventsBound = true;

  const beginPointerSelection = (kind, e, handle = null) => {
    const viewportEvent = { offsetX: e.clientX, offsetY: e.clientY };
    beginSelectionInteraction(kind, viewportEvent, handle);
    if (selectionInteraction && typeof hitCanvas.setPointerCapture === 'function') {
      hitCanvas.setPointerCapture(e.pointerId);
    }
  };

  hitCanvas.addEventListener('pointerdown', (e) => {
    if (!reading || e.button !== 0) return;
    const handle = getSelectionHandle(e.clientX, e.clientY, selCss);
    if (handle) {
      e.stopPropagation();
      e.preventDefault();
      hitCanvas.setAttribute('data-resizing', 'true');
      beginPointerSelection('resize', e, handle);
      return;
    }
    let idx = hitTestWord(e.offsetX, e.offsetY);
    // 选区内任意位置都进入 word 拖选；空白点命中不到 word 时回落到最近 word 起选。
    // 选区移动交给主 canvas 的"选区外拖动"分支；hit-canvas 不再承载 move 语义，
    // 避免 OCR 激活后误触把整个选区挪走、连带 invalidate OCR 数据。
    if (idx < 0) idx = nearestWordByLine(e.offsetX, e.offsetY);
    if (idx < 0) return;
    e.stopPropagation();
    e.preventDefault();
    reading.dragStart = idx;
    reading.selectionStart = idx;
    reading.selectionEnd = idx;
    redrawHitLayer();
    syncSelectionToPanel();
    if (typeof hitCanvas.setPointerCapture === 'function') hitCanvas.setPointerCapture(e.pointerId);
  });

  hitCanvas.addEventListener('pointermove', (e) => {
    if (selectionInteraction) {
      updateSelectionInteraction({ offsetX: e.clientX, offsetY: e.clientY });
      return;
    }
    if (!reading) return;
    const idx = hitTestWord(e.offsetX, e.offsetY);
    // hit-canvas 现在统一是文字选取语义；cursor 始终保持 text（与拖动行为一致）。
    hitCanvas.style.cursor = 'text';
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

  const finishHitPointer = (e) => {
    if (selectionInteraction) {
      hitCanvas.removeAttribute('data-resizing');
      hitCanvas.style.cursor = 'text';
      finishSelectionInteraction({ offsetX: e.clientX, offsetY: e.clientY });
    } else if (reading) {
      reading.dragStart = null;
    }
    if (typeof hitCanvas.hasPointerCapture === 'function' && hitCanvas.hasPointerCapture(e.pointerId)) {
      hitCanvas.releasePointerCapture(e.pointerId);
    }
  };
  hitCanvas.addEventListener('pointerup', finishHitPointer);
  hitCanvas.addEventListener('pointercancel', finishHitPointer);

  hitCanvas.addEventListener('mouseleave', () => {
    if (!reading || selectionInteraction) return;
    reading.hoverWord = null;
    reading.dragStart = null;
    hitCanvas.style.cursor = 'text';
    redrawHitLayer();
  });

  // 双击选一整行——0.11.10-e：若面板未开则先召唤面板，之后再高亮整行
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
    // 面板未开 → 召唤(用缓存,不重跑 OCR);面板已开 → 只同步选中
    if (!document.getElementById('ocr-panel') && ocrResultCache) {
      showOcrResult(ocrResultCache);
    }
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

/** 按当前 word 选择范围拼出文本，保持后端智能拼接产生的空格/换行。 */
function getReadingSelectionText() {
  if (!reading || reading.selectionStart === null || reading.selectionEnd === null) return '';
  const lo = Math.min(reading.selectionStart, reading.selectionEnd);
  const hi = Math.max(reading.selectionStart, reading.selectionEnd);
  const cs = reading.charRanges[lo]?.start;
  const ce = reading.charRanges[hi]?.end;
  if (!Number.isInteger(cs) || !Number.isInteger(ce)) return '';
  return reading.fullText.slice(cs, ce);
}

function copyReadingSelection() {
  const text = getReadingSelectionText();
  if (!text) return false;
  copyToClipboard(text)
    .then(() => showTransientHint('已复制所选文字'))
    .catch((e) => console.error('[screenshot] 复制识别文字失败', e));
  return true;
}

// OCR 命中 canvas 右键：阅读模式激活时弹菜单（有选区→选区菜单，无选区→基础菜单）。
// 不再因"无选区"直接取消截图——用户右键意图是弹菜单,不是取消。
hitCanvas.addEventListener('contextmenu', (e) => {
  e.preventDefault();
  const selText = getReadingSelectionText();
  showReadingContextMenu(selText || null, e);
});

/** 阅读模式右键菜单：复制（选区或全文）/ 取消截图。跟随鼠标定位。
 *  text 为 null 表示无 word 选区（此时复制 = 全文）。 */
function showReadingContextMenu(text, mouseEvent) {
  // 清理旧菜单
  const old = document.getElementById('reading-ctx-menu');
  if (old) old.remove();

  // 跟随鼠标定位（加边界 clamp 防止超出屏幕）
  const MARGIN = 8;
  let x = mouseEvent.clientX;
  let y = mouseEvent.clientY;
  const menuW = 140, menuH = 80;
  const mon = findDisplayCssAt(x, y);
  if (x + menuW > mon.x + mon.w - MARGIN) x = mon.x + mon.w - menuW - MARGIN;
  if (y + menuH > mon.y + mon.h - MARGIN) y = mon.y + mon.h - menuH - MARGIN;
  x = Math.max(mon.x + MARGIN, x);
  y = Math.max(mon.y + MARGIN, y);

  const menu = document.createElement('div');
  menu.id = 'reading-ctx-menu';
  menu.style.cssText = `
    position: fixed; left: ${x}px; top: ${y}px; z-index: 9999;
    background: rgba(30,30,30,0.96); border: 1px solid rgba(255,255,255,0.15);
    border-radius: 8px; padding: 4px; min-width: 120px;
    box-shadow: 0 4px 16px rgba(0,0,0,0.5);
    font-family: "Segoe UI","Microsoft YaHei",sans-serif; font-size: 13px;
  `;
  const makeItem = (label, fn) => {
    const btn = document.createElement('div');
    btn.textContent = label;
    btn.style.cssText = `
      padding: 6px 12px; color: #ddd; cursor: pointer; border-radius: 6px;
    `;
    btn.addEventListener('mouseenter', () => btn.style.background = 'rgba(255,255,255,0.08)');
    btn.addEventListener('mouseleave', () => btn.style.background = '');
    btn.addEventListener('click', () => { fn(); menu.remove(); });
    return btn;
  };

  // 复制：有选区→选区文本，无选区→全文
  const copyLabel = text ? '复制' : '复制全文';
  const copyText = text || (reading ? reading.fullText : '');
  if (copyText) {
    menu.appendChild(makeItem(copyLabel, () => {
      copyToClipboard(copyText)
        .then(() => showTransientHint(text ? '已复制所选文字' : '已复制全文'))
        .catch((e) => console.error('复制失败', e));
    }));
  }

  // 取消截图
  menu.appendChild(makeItem('取消截图', () => doCancel()));

  document.body.appendChild(menu);

  // 点击其他地方关闭
  const close = (ev) => {
    if (!menu.contains(ev.target)) { menu.remove(); document.removeEventListener('pointerdown', close); }
  };
  setTimeout(() => document.addEventListener('pointerdown', close), 0);
}


// "翻译"按钮打开 → 自动 OCR + translate → 切到译文 tab。面板内"翻译"
// 按钮独立触发翻译。修改原文 → 译文标"过期"(斜纹底 + 橙点)。
//
// 参数:
//   result:           { text, lines, words?, text_angle? } — OCR 结果
//   options.tab:      'source' | 'translated' — 打开时默认激活的 tab
//   options.showOverlay: 是否在图上嵌字（识别路径=false / 翻译路径=true）

function showOcrResult(result, options = {}) {
  const old = document.getElementById('ocr-panel');
  if (old) old.remove();
  // 0.11.10-e：不再自动 exitReadingMode——reading 生命周期由 overlay 管理,
  // 面板召唤/关闭独立于 reading。这样打开面板不会打断图上的 word 拖选态。
  // 只在没有既有 reading 时才 enterReadingMode(下方判断)。

  const text = (result && result.text) || '';
  const initialText = text || '（未识别到文字）';
  const initialTab = options.tab === 'translated' ? 'translated' : 'source';
  // 翻译按钮文案：已有译文 → 「重新翻译」，否则 → 「翻译」
  const overlayForLabel = annot.getOverlay();
  const hasTranslation = overlayForLabel && overlayForLabel.lines.length > 0
    && overlayForLabel.lines.every((l) => hasText(l.dstText));
  const translateLabel = hasTranslation ? '重新翻译' : '翻译';

  const panel = document.createElement('div');
  panel.id = 'ocr-panel';
  panel.className = 'ocr-panel';
  panel.innerHTML = `
    <div class="ocr-panel-header">
      <div class="ocr-tabs">
        <button class="ocr-tab ${initialTab === 'source' ? 'active' : ''}" data-tab="source">原文</button>
        <button class="ocr-tab ${initialTab === 'translated' ? 'active' : ''}" data-tab="translated">译文 <span class="stale-dot" aria-hidden="true"></span></button>
      </div>
      <div class="ocr-panel-actions">
        <button id="ocr-copy" class="tool-btn tool-btn-primary" ${text ? '' : 'disabled'}>复制</button>
        <button id="ocr-translate" class="tool-btn" ${text ? '' : 'disabled'} title="翻译当前文本">${translateLabel}</button>
      </div>
      <button id="ocr-close" class="tool-btn" title="关闭面板">✕</button>
    </div>
    <textarea id="ocr-textarea-source" class="ocr-panel-textarea" spellcheck="false"></textarea>
    <textarea id="ocr-textarea-translated" class="ocr-panel-textarea" spellcheck="false" hidden placeholder="点击翻译按钮生成译文"></textarea>
    <div class="ocr-panel-adv">
      <label class="ocr-adv-item">
        <span>嵌图背景</span>
        <select id="ocr-bg-strategy">
          <option value="average">平均色</option>
          <option value="blur">高斯模糊</option>
          <option value="solid">半透明板</option>
        </select>
      </label>
      <label class="ocr-adv-item">
        <span>字号</span>
        <input id="ocr-font-scale" type="range" min="60" max="140" step="10" value="100" title="嵌图字号缩放(60%-140%)" />
        <span id="ocr-font-scale-val">100%</span>
      </label>
    </div>
  `;
  document.body.appendChild(panel);

  // 识别路径（initialTab=source）：隐藏 adv 区域（嵌图背景/字号 仅翻译路径有意义）
  if (initialTab !== 'translated') {
    const advSection = panel.querySelector('.ocr-panel-adv');
    if (advSection) advSection.style.display = 'none';
  }

  const sourceTa = panel.querySelector('#ocr-textarea-source');
  const translatedTa = panel.querySelector('#ocr-textarea-translated');
  const tabSource = panel.querySelector('.ocr-tab[data-tab="source"]');
  const tabTranslated = panel.querySelector('.ocr-tab[data-tab="translated"]');
  const translateBtn = panel.querySelector('#ocr-translate');
  const copyBtn = panel.querySelector('#ocr-copy');
  const bgStrategySelect = panel.querySelector('#ocr-bg-strategy');
  const fontScaleInput = panel.querySelector('#ocr-font-scale');
  const fontScaleVal = panel.querySelector('#ocr-font-scale-val');

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

  // 抽屉锚定触发按钮：识别路径锚 btn-ocr，翻译路径锚 btn-translate。
  // 锚点正下方优先，空间不足翻上方，仍不够退到屏内 clamp。
  const MARGIN = 8;
  const mon = findDisplayCssAt(selCss.x + selCss.w / 2, selCss.y + selCss.h / 2);
  const pw = panel.offsetWidth;
  const ph = panel.offsetHeight;
  // 统一锚定到扫描按钮（无论识别还是翻译路径）
  const anchorBtn = document.getElementById('btn-ocr');
  const anchorRect = anchorBtn ? anchorBtn.getBoundingClientRect() : toolbar.getBoundingClientRect();

  let left = anchorRect.left;
  if (left + pw > mon.x + mon.w - MARGIN) left = mon.x + mon.w - MARGIN - pw;
  left = Math.max(mon.x + MARGIN, left);

  let top = anchorRect.bottom + 4;
  if (top + ph > mon.y + mon.h - MARGIN) top = anchorRect.top - ph - 4;
  if (top < mon.y + MARGIN) top = Math.max(mon.y + MARGIN, mon.y + mon.h - MARGIN - ph);
  panel.style.left = left + 'px';
  panel.style.top = top + 'px';

  // 面板内交互不应触发 overlay 的 blur 隐藏
  panel.addEventListener('mousedown', (e) => e.stopPropagation());
  document.getElementById('ocr-close').addEventListener('click', () => {
    panel.remove();
    updateToolbarButtonActive();
  });

  // ── 面板拖动（复用工具栏 drag 机制：header 抓手 + 多屏 clamp） ──
  // 与工具栏共用 document mousemove/mouseup，靠各自的 dragging 标志位隔离。
  const header = panel.querySelector('.ocr-panel-header');
  if (header) {
    let dragging = false;
    let offsetX = 0, offsetY = 0;
    header.addEventListener('mousedown', (e) => {
      // 点 close 按钮或 toggle 控件时不启动拖动
      if (e.target.closest('#ocr-close')) return;
      if (e.button !== 0) return;
      e.preventDefault();
      e.stopPropagation();
      dragging = true;
      const rect = panel.getBoundingClientRect();
      offsetX = e.clientX - rect.left;
      offsetY = e.clientY - rect.top;
      document.body.style.cursor = 'grabbing';
    });
    const onMove = (e) => {
      if (!dragging) return;
      // 自检：面板已被移除（showOcrResult 重开/外部 remove）→ 清理自身监听
      if (!document.body.contains(panel)) {
        dragging = false;
        document.removeEventListener('mousemove', onMove);
        document.removeEventListener('mouseup', onUp);
        document.body.style.cursor = '';
        return;
      }
      const pwl = panel.offsetWidth;
      const phl = panel.offsetHeight;
      const monMove = findDisplayCssAt(e.clientX, e.clientY);
      let nl = e.clientX - offsetX;
      let nt = e.clientY - offsetY;
      nl = Math.max(monMove.x + MARGIN, Math.min(nl, monMove.x + monMove.w - pwl - MARGIN));
      nt = Math.max(monMove.y + MARGIN, Math.min(nt, monMove.y + monMove.h - phl - MARGIN));
      panel.style.left = nl + 'px';
      panel.style.top = nt + 'px';
    };
    const onUp = () => {
      if (!dragging) return;
      dragging = false;
      document.body.style.cursor = '';
    };
    document.addEventListener('mousemove', onMove);
    document.addEventListener('mouseup', onUp);
    // close 按钮显式清理一次（其余路径靠 onMove 自检回收）
    const cleanup = () => {
      document.removeEventListener('mousemove', onMove);
      document.removeEventListener('mouseup', onUp);
    };
    document.getElementById('ocr-close').addEventListener('click', cleanup);
  }

  // ── Tab 切换 ─────────────────────────────────────
  const showTab = (name) => {
    const isSource = name === 'source';
    tabSource.classList.toggle('active', isSource);
    tabTranslated.classList.toggle('active', !isSource);
    sourceTa.hidden = !isSource;
    translatedTa.hidden = isSource;
    (isSource ? sourceTa : translatedTa).focus();
    // 切到译文 tab 时：确保 overlay.mode = translated + 自动触发翻译（仅首次）
    if (!isSource) {
      const overlay = annot.getOverlay();
      // 确保 overlay 切到 translated 模式（从识别路径切过来时 mode 可能是 null）
      if (overlay && overlay.mode !== 'translated') {
        annot.setOverlayMode('translated');
        redrawAnnotFull();
      }
      // 只有当所有行都没有翻译结果时才自动触发（首次翻译）
      // 如果已有部分/全部译文，说明之前翻译过，不再重复调用
      const needsTranslation = overlay && overlay.lines.length > 0
        && overlay.lines.every((l) => !hasText(l.dstText));
      if (needsTranslation) {
        doTranslate();
      }
    } else {
      // 切回原文 tab 时：overlay.mode 改回 null（图上不嵌字，保留 lines 供再次切换）
      const overlay = annot.getOverlay();
      if (overlay && overlay.mode !== null) {
        annot.setOverlayMode(null);
        redrawAnnotFull();
      }
    }
    // tab 切换后同步工具栏按钮高亮
    updateToolbarButtonActive();
  };
  tabSource.addEventListener('click', () => showTab('source'));
  tabTranslated.addEventListener('click', () => showTab('translated'));

  // 面板与 overlayLayer 共享同一份当前数据，而不是另起一套翻译状态。
  const overlayAtOpen = annot.getOverlay();
  if (overlayAtOpen) {
    const translatedText = overlayAtOpen.lines.map((line) => line.dstText || '').join('\n');
    if (hasText(translatedText)) translatedTa.value = translatedText;
    // initialTab 已在模板里设了 active class；这里只同步 textarea 显示
    sourceTa.hidden = initialTab !== 'source';
    translatedTa.hidden = initialTab !== 'translated';
  } else {
    sourceTa.hidden = initialTab !== 'source';
    translatedTa.hidden = initialTab !== 'translated';
  }

  // ── 译文过期标记 ─────────────────────────────────
  // 原文改动 → 译文标 stale;新翻译时清 stale
  const markTranslatedStale = (stale) => {
    tabTranslated.setAttribute('data-stale', stale ? 'true' : 'false');
    translatedTa.setAttribute('data-stale', stale ? 'true' : 'false');
  };

  // ── 移除空格（保留兜底） ──────────────────────────
  // ── 原文编辑 → 译文过期 ──────────────────
  sourceTa.addEventListener('input', () => {
    if (translatedTa.value) markTranslatedStale(true);
  });

  // ── 复制（复制当前 tab 的选中内容；无选区时复制全文） ──
  copyBtn.addEventListener('click', () => {
    const currentTa = sourceTa.hidden ? translatedTa : sourceTa;
    const selected = currentTa.value.slice(currentTa.selectionStart, currentTa.selectionEnd);
    const value = selected || currentTa.value;
    if (value) {
      copyToClipboard(value)
        .then(() => showTransientHint(selected ? '已复制所选文字' : '已复制全文'))
        .catch((e) => console.error('复制失败', e));
    }
    panel.remove();
    updateToolbarButtonActive();
  });

  // ── 背景策略切换（0.11.10-i）─────────────────────
  // 回读当前 overlay 的 bgStrategy 到 select;change 后触发 overlay 重建 → 重绘
  if (bgStrategySelect) {
    const overlayCur = annot.getOverlay();
    if (overlayCur && overlayCur.bgStrategy) {
      bgStrategySelect.value = overlayCur.bgStrategy;
    }
    bgStrategySelect.addEventListener('click', (e) => e.stopPropagation());
    bgStrategySelect.addEventListener('change', () => {
      const cur = annot.getOverlay();
      if (!cur) return;
      annot.setOverlay({
        lines: cur.lines,
        mode: cur.mode,
        bgStrategy: bgStrategySelect.value,
        targetLang: cur.translationTargetLang,
      });
      redrawAnnotFull();
    });
  }

  // ── 字号微调 ─────────────────────────────────
  if (fontScaleInput) {
    const overlayCur = annot.getOverlay();
    if (overlayCur) fontScaleInput.value = String(Math.round((overlayCur.fontScale ?? 1.0) * 100));
    fontScaleVal.textContent = fontScaleInput.value + '%';
    fontScaleInput.addEventListener('input', () => {
      const pct = parseInt(fontScaleInput.value, 10);
      fontScaleVal.textContent = pct + '%';
      annot.setOverlayFontScale(pct / 100);
      redrawAnnotFull();
    });
  }

  // ── 翻译 ─────────────────────────────────────────
  let translating = false;
  let loadingAnimTimer = null;  // 0.11.10-k：loading 动画定时器
  const doTranslate = async () => {
    if (translating) return;
    const src = sourceTa.value.trim();
    if (!src || src === '（未识别到文字）') return;
    translating = true;
    translateBtn.disabled = true;
    translateBtn.textContent = '翻译中…';
    translatedTa.setAttribute('data-loading', 'true');
    translatedTa.value = '翻译中,请稍候…';
    // 0.11.10-k：翻译中在嵌图中心显示 loading 动画
    annot.setOverlayLoading(true);
    // 启动定时器持续重绘实现旋转动画（每 50ms 一帧）
    loadingAnimTimer = setInterval(() => {
      if (!annot.isOverlayLoading()) {
        clearInterval(loadingAnimTimer);
        loadingAnimTimer = null;
        return;
      }
      redrawAnnotFull();
    }, 50);
    // 注意：不在这里调 showTab('translated')，调用方已负责切 tab
    const overlayLang = annot.getOverlay()?.translationTargetLang;
    requestOverlayTranslation(overlayLang);
    // 轮询等待翻译完成——每 100ms 检查 overlay 是否已回填译文
    const startTime = Date.now();
    const waitForTranslation = () => {
      const latest = annot.getOverlay();
      const allTranslated = latest && latest.lines.length > 0
        && latest.lines.every((line) => hasText(line.dstText));
      if (allTranslated) {
        // 翻译完成 → 同步 textarea + 结束 loading
        translating = false;
        translateBtn.disabled = false;
        translateBtn.textContent = '重新翻译';
        translatedTa.removeAttribute('data-loading');
        if (loadingAnimTimer) { clearInterval(loadingAnimTimer); loadingAnimTimer = null; }
        annot.setOverlayLoading(false);
        translatedTa.value = latest.lines.map((line) => line.dstText || '').join('\n');
        markTranslatedStale(false);
        return;
      }
      // 翻译进行中 →更新进度提示
      if (translationBusy) {
        const done = latest ? latest.lines.filter((l) => hasText(l.dstText)).length : 0;
        const total = latest ? latest.lines.length : 0;
        if (total > 0 && done > 0) {
          translatedTa.value = `翻译中 ${done}/${total}…`;
        }
        if (Date.now() - startTime < 30000) {
          setTimeout(waitForTranslation, 100);
          return;
        }
      }
      // 超时或翻译结束但未全部完成
      if (loadingAnimTimer) { clearInterval(loadingAnimTimer); loadingAnimTimer = null; }
      annot.setOverlayLoading(false);
      translating = false;
      translateBtn.disabled = false;
      translateBtn.textContent = '重新翻译';
      translatedTa.removeAttribute('data-loading');
      if (!translationBusy && !(latest && latest.lines.some((l) => hasText(l.dstText)))) {
        translatedTa.value = '翻译失败，请重试';
      }
    };
    setTimeout(waitForTranslation, 100);
  };
  translateBtn.addEventListener('click', () => {
    // 点击翻译/重新翻译 → 跳到译文 tab + 触发翻译
    showTab('translated');
    // 识别路径下首次点翻译：显示 adv 区域 + 确保 overlay 切到 translated 模式
    const advSection = panel.querySelector('.ocr-panel-adv');
    if (advSection) advSection.style.display = '';
    const overlay = annot.getOverlay();
    if (overlay && overlay.mode !== 'translated') {
      annot.setOverlayMode('translated');
    }
    doTranslate();
  });

  // 0.11.10-e：reading 生命周期与 overlay 绑定,而非与面板绑定。
  //   - overlay 存在时,activateOverlay 里已 enterReadingMode,此处只补齐 result.words 数据。
  //   - overlay 不存在时(旧路径:showOcrResult 被独立调用),仍需 enterReadingMode。
  if (result && Array.isArray(result.words) && result.words.length > 0 && !reading) {
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

  function positionDropdown(dropdown) {
    if (!dropdown) return;
    dropdown.removeAttribute('data-placement');
    const wrap = dropdown.closest('.dropdown-wrap');
    const anchor = wrap ? wrap.getBoundingClientRect() : toolbar.getBoundingClientRect();
    const mon = findDisplayCssAt(anchor.left, anchor.top);
    dropdown.style.visibility = 'hidden';
    dropdown.setAttribute('data-open', 'true');
    const dh = dropdown.offsetHeight;
    if (anchor.bottom + 4 + dh > mon.y + mon.h - 8 && anchor.top - 4 - dh >= mon.y + 8) {
      dropdown.setAttribute('data-placement', 'top');
    }
    dropdown.style.visibility = '';
  }

  /** 切换某个下拉的展开状态（打开时关掉其他） */
  function toggleDropdown(dropdown) {
    const willOpen = dropdown.getAttribute('data-open') !== 'true';
    closeAllDropdowns();
    if (willOpen) positionDropdown(dropdown);
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
    select: 'direct',
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
    canvas.setAttribute('data-tool', tool);
    updateSelectionCursor(-1, -1);
    if (selCss) drawFinalSelection();
    // 0.11.10-a：把当前工具同步到 hit-canvas 的 data-tool 属性，CSS 据此决定
    // pointer-events（仅 select 工具下 hit-canvas 接收鼠标）
    hitCanvas.setAttribute('data-tool', tool);
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
