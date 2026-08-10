//! 截图 overlay 主逻辑（0.14.6 重构：按职责拆分为子模块）。
//!
//! 架构：
//! - 主 canvas #canvas：全屏截图 + 暗色蒙版 + 亮区（选区）
//! - 标注 canvas #annot-canvas：位置由 JS 动态设置为选区区域，画标注
//! - 工具栏 #toolbar：HTML 元素，选区完成后显示
//!
//! 本文件是编排层：
//! - 初始化 DOM / 状态 / 回调注册
//! - 选区生命周期管理（resetState / loadScreenshot / enterAnnotationMode / exitAnnotationMode）
//! - 画布事件绑定（mousedown / mousemove / mouseup / dblclick / contextmenu / keydown / blur）
//!
//! 拆分模块：
//! - ss-state.js    — 共享状态 + 常量 + initDOM
//! - ss-utils.js     — 纯工具函数（norm / pointInRect / applySquareConstraint）
//! - ss-draw.js      — 绘制函数（drawDimmed / drawSelection / drawFinalSelection / redrawAnnot*）
//! - ss-display.js   — 显示器几何（getDisplays / findDisplayCssAt / positionToolbar）
//! - ss-interaction.js — 选区交互（beginSelectionInteraction / updateSelectionInteraction 等）
//! - ss-reading.js   — OCR 阅读模式（hitTestWord / enterReadingMode / bindHitCanvasEvents 等）
//! - ss-ocr.js       — OCR 面板 + 翻译 + UI helpers（showOcrResult / doIdentifySelection 等）
//! - ss-output.js    — 输出动作（doCopySelection / doPinSelection / compositeSelection / doCancel）
//! - ss-toolbar.js   — 工具栏 + 水印表单 + 文本输入（bindToolbar / openWatermarkForm / showTextInput）
//!
//! 坐标约定：
//! - canvas 内部像素 = 物理像素（BitBlt 输出）
//! - canvas CSS 尺寸 = 视口大小（CSS 像素）
//! - DPR = 物理像素 / CSS 像素
//! - 鼠标事件 offsetX/Y = CSS 像素
//! - 选区 selCss 存 CSS 像素；annot-canvas 内部像素 = 物理像素
//! - 标注坐标使用物理像素相对裁剪区

import {
  screenshotSetAnnotationMode, hideScreenshotOverlay,
  ocrImage, invoke, copyToClipboard,
} from "../shared/api.js";
import { normalizeError } from "../shared/tauri.js";
import * as annot from "./annotation-engine.js";
import { ensureSpriteLoaded } from "../shared/icon.js";
import { applyThemeFromConfig } from "../shared/theme.js";

// ── 子模块 ──────────────────────────────────────────────
import { ss, initDOM, PREWARM_MIN_WIDTH, PREWARM_MIN_HEIGHT, TOOL_CAPS } from "./ss-state.js";
import { IMAGE_SOURCE } from './image-editor-session.js';
import { norm, pointInRect, applySquareConstraint } from "./ss-utils.js";
import { shouldStartFreeSelection, monitorDprAtCss, syncRenderScale, cssRectToBitmap, cssPointToScreen } from "./ss-selection-geometry.js";
import { drawDimmed, drawSelection, drawFinalSelection, redrawAnnotPreview, redrawAnnotFull, scheduleDrawSelection, cancelDrawSelectionRaf } from "./ss-draw.js";
import { positionToolbar, findDisplayCssAt, invalidateDisplaysCache } from "./ss-display.js";
import {
  getSelectionHandle, beginSelectionInteraction, updateSelectionInteraction,
  finishSelectionInteraction, updateSelectionCursor, refreshShapePreviewOnShift,
  updateStrokeCursor,
  updatePixelMagnifier, hidePixelMagnifier, cycleMagnifierFormat, getMagnifierColorText,
} from "./ss-interaction.js";
import {
  exitReadingMode, getReadingSelectionText, showReadingContextMenu, copyReadingSelection,
} from "./ss-reading.js";
import {
  showOcrResult, showSelLoading, hideSelLoading, showTransientHint,
  updateOutputButtonsDisabled, updateToolbarButtonActive, syncPanelTranslatedFromOverlay,
  updateOverlayButtonsActive, doIdentifySelection, doOverlayTranslate, doPanelToggle,
} from "./ss-ocr.js";
import {
  doCopySelection, doCopyFullScreen, doPinSelection, doSaveSelection,
  compositeSelection, doCancel, hasActivePanel, outputEditorPng,
} from "./ss-output.js";
import { bindToolbar, showTextInput, updateUndoRedoButtons } from "./ss-toolbar.js";
// 0.15.8：智能窗口吸附 + 像素放大镜
import { loadPickableWindows, clearPickableWindows, updateWindowHover, getHoveredWindowRect, clearHover } from "./ss-hover.js";
// 0.18.2：控件级智能吸附（跨屏预选版）
import {
  setControlTarget, prefetchControlHints, clearControlHints, updateControlHover,
  getHoveredControlRect, clearControlHover,
} from "./ss-control-hints.js";
// 0.15.7：长截图
import {
  enterScrollCapture, exitScrollCapture, onScrollWheel,
  bindScrollToolbar, enterScrollEdit, isScrollCaptureActive,
  outputLongImage, resetScrollCaptureSession,
} from "./scroll/index.js";
import { refreshDiagnosticsVisibility } from "./scroll/diagnostics.js";
import { refreshOcrDiagnosticsVisibility } from "./ss-ocr-diagnostics.js";

// ════════════════════════════════════════════════════════════
//  初始化
// ════════════════════════════════════════════════════════════

// 0.15.7：长图编辑——Space 或中键拖拽平移超长图
let _spaceDown = false;

// 0.18.x：控件预选统一门控——截图加载完成 + renderScale 同步 + 配置加载完成 后只触发一次
let captureHintsStarted = false;
let screenshotReady = false;
let renderScaleReady = false;
let configReady = false;

/** 统一门控：截图 + 窗口列表 + 控件预热只触发一次 */
function maybeStartCaptureHints() {
  if (captureHintsStarted) return;
  if (!screenshotReady) return;
  if (!renderScaleReady) return;
  if (!configReady) return;

  captureHintsStarted = true;
  try {
    loadPickableWindows(ss.windowListGen);
  } catch (e) {
    console.warn('[screenshot] maybeStartCaptureHints: loadPickableWindows threw', e);
  }
  // 初始前台窗口预热：只触发一次
  if (ss.screenshotConfig.controlSnap) {
    const meta = window.__blinkScreenMeta;
    if (meta && meta.fgHwnd) {
      prefetchControlHints(meta.fgHwnd);
    }
  }
}

initDOM();

// 注册跨模块回调（避免循环依赖）
ss._invalidateSelectionContent = invalidateSelectionContent;
ss._enterAnnotationMode = enterAnnotationMode;
ss._exitAnnotationMode = exitAnnotationMode;
ss._redrawAnnotPreview = redrawAnnotPreview;
ss._showOcrResult = showOcrResult;
ss._showTransientHint = showTransientHint;
ss._doCancel = doCancel;
ss._compositeSelection = compositeSelection;
ss._doPinSelection = doPinSelection;
ss._outputEditorPng = outputEditorPng;
// 0.15.7：长截图回调
ss._enterCanvasImageEditor = enterCanvasImageEditor;

// 图标 sprite
ensureSpriteLoaded();
applyThemeFromConfig();

annot.init(ss.annotCanvas);
annot.setTool('select');
{
  const _hc = document.getElementById('ocr-hit-canvas');
  if (_hc) _hc.setAttribute('data-tool', 'select');
}

// 0.15.9：每次模块加载时清除上一轮残留状态
// 防止页面未重载时上一轮的 OCR 面板/文本输入/标注命令残留导致交互异常
{
  const staleOcr = document.getElementById('ocr-panel');
  if (staleOcr) staleOcr.remove();
  const staleText = document.querySelector('.text-annot-input');
  if (staleText) staleText.remove();
  annot.clearOverlay();
  // 清除标注引擎内部状态（commands/cropImageData/watermark 等）
  annot.reset(0, 0, null);
  // 清除主 canvas 上一轮的截图内容
  if (ss.canvas.width > 0) {
    ss.ctx.clearRect(0, 0, ss.canvas.width, ss.canvas.height);
  }
}

// 预热窗口跳过图片加载；冷建窗可通过 query 直接进入剪贴板图片编辑。
const initialParams = new URLSearchParams(window.location.search);
const isPreheat = initialParams.get('preheat') === '1';
const initialSource = initialParams.get('source');
if (!isPreheat) {
  if (initialSource === IMAGE_SOURCE.CLIPBOARD) {
    loadEditorImage(IMAGE_SOURCE.CLIPBOARD);
  } else {
    loadScreenshot();
    // 窗口列表在 img.onload 的 rAF 中加载（syncRenderScale 之后）
  }
}
// 0.15.7：绑定长截图专属工具栏
bindScrollToolbar();
refreshOcrDiagnosticsVisibility();

// resize 时重新同步 renderScale（canvas 布局变化后比例可能改变）
window.addEventListener('resize', () => {
  const { canvas } = ss;
  const meta = window.__blinkScreenMeta;
  if (!canvas || !meta) return;
  if (syncRenderScale(canvas, meta)) {
    // M7 优化：renderScale 变化后失效 displays 缓存
    invalidateDisplaysCache();
    console.debug('[screenshot] resize: renderScale re-synced', { scaleX: meta.renderScaleX, scaleY: meta.renderScaleY });
  }
});

window.__blinkClearScreenshotVisual = function () {
  console.info('[screenshot] __blinkClearScreenshotVisual called');
  try {
    resetState();
    const { canvas, ctx } = ss;
    ctx.clearRect(0, 0, canvas.width, canvas.height);
  } catch (e) {
    console.error('[screenshot] clearScreenshotVisual threw', e);
  }
};

window.__blinkReloadScreenshot = function () {
  console.info('[screenshot] __blinkReloadScreenshot called');
  try {
    resetState();
    console.info('[screenshot] resetState done');
  } catch (e) {
    console.error('[screenshot] resetState threw, attempting to continue', e);
  }
  try {
    loadScreenshot();
  } catch (e) {
    console.error('[screenshot] loadScreenshot threw', e);
    ss.errorHint.textContent = '截图初始化失败，按 ESC 重试';
    ss.errorHint.classList.remove('hidden');
  }
};

window.__blinkOpenImageEditor = function () {
  console.info('[image-editor] __blinkOpenImageEditor called');
  try {
    resetState();
    loadEditorImage(window.__blinkEditorSource?.kind || IMAGE_SOURCE.CLIPBOARD);
  } catch (e) {
    console.error('[image-editor] 初始化失败', e);
    ss.errorHint.textContent = '图片编辑初始化失败，按 ESC 关闭';
    ss.errorHint.classList.remove('hidden');
  }
};

// ════════════════════════════════════════════════════════════
//  选区生命周期
// ════════════════════════════════════════════════════════════

/** 完全重置前端状态——每次 overlay 显示时都要走一遍 */
function resetState() {
  console.info('[screenshot] resetState start');
  resetScrollCaptureSession();
  const { canvas, ctx, annotCanvas, annotCtx, sizeHint, toolbar, errorHint } = ss;
  ss._loadGen++;  // BUG1 fix: 使待处理的旧 img.onload 回调失效
  // 0.15.9：取消待执行的标注预览 rAF
  if (ss._annotRaf) { cancelAnimationFrame(ss._annotRaf); ss._annotRaf = 0; }
  // 0.15.10：清除快照
  ss._committedSnapshot = null;
  ss.isDragging = false;
  ss.isAnnotDragging = false;
  ss.isAnnotating = false;
  ss.sent = false;
  ss.ocrBusy = false;
  ss.translationBusy = false;
  ss._translateAndPinPending = false;
  // 0.15.9：清除防抖标志——快速连续截图时上一轮的 cancelInProgress/blurGuard
  // 可能仍在生效期，导致新一轮的 cancel/blur 被静默忽略（用户被困在 overlay 里）
  ss.cancelInProgress = false;
  ss.blurGuard = false;
  ss.selCss = null;
  ss.selectionInteraction = null;
  // 0.15.8 R2：清除 pending-snap 状态
  ss.pendingSnap = null;
  ss.snappedHwnd = null;
  ss.selectionRevision++;
  ss.translationRevision++;
  canvas.style.cursor = 'crosshair';
  canvas.setAttribute('data-tool', 'select');
  ss.screenshot = null;
  ss.screenshotOffscreen = null;
  ss.editorSession.reset();
  document.body.classList.remove('image-editor-mode');
  const scrollButton = document.getElementById('btn-scroll');
  if (scrollButton) scrollButton.hidden = false;
  if (ss.singleClickTimeout) { clearTimeout(ss.singleClickTimeout); ss.singleClickTimeout = null; }
  sizeHint.classList.add('hidden');
  toolbar.classList.add('hidden');
  annotCanvas.classList.add('hidden');
  errorHint.classList.add('hidden');
  errorHint.textContent = '';
  if (canvas.width > 0) {
    ctx.clearRect(0, 0, canvas.width, canvas.height);
  }
  if (annotCanvas.width > 0) {
    annotCtx.clearRect(0, 0, annotCanvas.width, annotCanvas.height);
  }
  annotCanvas.width = 0;
  annotCanvas.height = 0;

  // 0.15.9：以下操作分组 try-catch——任何一个失败不应阻塞后续重置 + loadScreenshot
  try { exitReadingMode(); } catch (e) { console.warn('[screenshot] resetState: exitReadingMode failed', e); }
  try {
    screenshotSetAnnotationMode(false).catch((e) => console.error('[screenshot] setAnnotationMode(false) 失败', e));
  } catch (e) { console.warn('[screenshot] resetState: screenshotSetAnnotationMode threw', e); }
  try {
    const oldOcr = document.getElementById('ocr-panel');
    if (oldOcr) oldOcr.remove();
  } catch (e) { console.warn('[screenshot] resetState: remove ocr-panel failed', e); }
  try {
    const wmDropdown = document.getElementById('text-dropdown');
    if (wmDropdown) {
      wmDropdown.setAttribute('data-view', 'list');
      wmDropdown.setAttribute('data-open', 'false');
    }
  } catch (e) { console.warn('[screenshot] resetState: text-dropdown reset failed', e); }
  ss.ocrPrewarm = null;
  ss.ocrResultCache = null;
  ss.ocrBusy = false;
  ss.translationBusy = false;
  try { updateOutputButtonsDisabled(); } catch (e) { console.warn('[screenshot] resetState: updateOutputButtonsDisabled failed', e); }
  try { annot.clearOverlay(); } catch (e) { console.warn('[screenshot] resetState: annot.clearOverlay failed', e); }
  try { updateOverlayButtonsActive(); } catch (e) { console.warn('[screenshot] resetState: updateOverlayButtonsActive failed', e); }
  try {
    toolbar.removeAttribute('data-user-moved');
    toolbar.style.left = '';
    toolbar.style.top = '';
  } catch (e) { console.warn('[screenshot] resetState: toolbar reset failed', e); }
  try { clearPickableWindows(); } catch (e) { console.warn('[screenshot] resetState: clearPickableWindows failed', e); }
  // 0.18.2：清除控件提示列表
  try { clearControlHints(); } catch (e) { console.warn('[screenshot] resetState: clearControlHints failed', e); }
  // 0.18.x：重置控件预选门控
  captureHintsStarted = false;
  screenshotReady = false;
  renderScaleReady = false;
  configReady = false;
  if (ss.magnifierRaf) { cancelAnimationFrame(ss.magnifierRaf); ss.magnifierRaf = 0; }
  // 长截图状态、在途任务与 DOM 统一由 resetScrollCaptureSession 清理。
  _spaceDown = false;
  try {
    if (ss.canvas) ss.canvas.style.pointerEvents = '';
    if (ss.canvas) {
      ss.canvas.style.left = '';
      ss.canvas.style.top = '';
      ss.canvas.style.width = '';
      ss.canvas.style.height = '';
    }
    if (ss.hitCanvas) ss.hitCanvas.style.pointerEvents = '';
  } catch (e) { console.warn('[screenshot] resetState: pointer-events restore failed', e); }
  console.info('[screenshot] resetState done');
}

function loadScreenshot() {
  console.info('[screenshot] loadScreenshot start');
  ss.errorHint.classList.add('hidden');

  // 配置读取与图像加载并行；失败时保留默认值。
  loadEditorConfig(true);

  // 加载代际守卫
  const gen = ++ss._loadGen;
  const img = new Image();
  img.crossOrigin = 'anonymous';

  // 0.15.9：加载超时检测——5 秒未完成则提示错误（防止协议请求静默失败）
  const timeoutId = setTimeout(() => {
    if (gen !== ss._loadGen) return;
    if (ss.screenshot) return;
    console.error('[screenshot] 加载超时（5s），协议请求可能失败', { gen });
    ss.errorHint.textContent = '截图加载超时，按 ESC 重试';
    ss.errorHint.classList.remove('hidden');
  }, 5000);

  img.onload = () => {
    clearTimeout(timeoutId);
    if (gen !== ss._loadGen) {
      console.info('[screenshot] 丢弃过期截图加载回调', { gen, cur: ss._loadGen });
      return;
    }
    try {
      const { canvas } = ss;
      ss.screenshot = img;
      canvas.width = img.width;
      canvas.height = img.height;
      ss.screenshotOffscreen = document.createElement('canvas');
      ss.screenshotOffscreen.width = img.width;
      ss.screenshotOffscreen.height = img.height;
      ss.screenshotOffscreen.getContext('2d', { willReadFrequently: true }).drawImage(img, 0, 0);
      // 等布局稳定后同步 renderScale，再绘制和加载窗口/控件
      requestAnimationFrame(() => {
        if (gen !== ss._loadGen) return;
        const meta = window.__blinkScreenMeta || { vx: 0, vy: 0 };
        syncRenderScale(canvas, meta);
        // M7 优化：初始 renderScale 设定后失效 displays 缓存
        invalidateDisplaysCache();
        drawDimmed();
        // 诊断日志：截图加载完成后的坐标空间状态
        const rect = canvas.getBoundingClientRect();
        console.debug('[screenshot] render scale synced', {
          injectedDpi: meta.overlayDpi,
          devicePixelRatio: window.devicePixelRatio,
          canvasWidth: canvas.width,
          canvasHeight: canvas.height,
          canvasRectWidth: rect.width,
          canvasRectHeight: rect.height,
          scaleX: meta.renderScaleX,
          scaleY: meta.renderScaleY,
        });
        // 0.18.x：统一门控触发窗口列表加载 + 控件预热
        screenshotReady = true;
        renderScaleReady = true;
        maybeStartCaptureHints();
      });
      console.info('[screenshot] screenshot loaded', { w: img.width, h: img.height, gen });
    } catch (e) {
      console.error('[screenshot] img.onload 处理异常', e, { gen });
      ss.errorHint.textContent = '截图渲染失败，按 ESC 重试';
      ss.errorHint.classList.remove('hidden');
    }
  };
  img.onerror = (e) => {
    clearTimeout(timeoutId);
    if (gen !== ss._loadGen) return;
    console.error('[screenshot] Image load failed (onerror)', { gen, error: e, src: img.src });
    ss.errorHint.textContent = '截图加载失败，按 ESC 关闭';
    ss.errorHint.classList.remove('hidden');
  };
  console.info('[screenshot] requesting screenshot image', { gen });
  img.src = 'http://blink-screenshot.localhost/capture?t=' + Date.now();
}

function loadEditorConfig(includeCaptureHints) {
  invoke('get_config_section', { key: 'screenshot:config' })
    .then((val) => {
      if (val && typeof val === 'object') {
        ss.screenshotConfig.prewarmOcr = val.prewarmOcr !== false;
        ss.screenshotConfig.scrollDebug = val.scrollDebug === true;
        ss.screenshotConfig.ocrDebug = val.ocrDebug === true;
        ss.screenshotConfig.controlSnap = val.controlSnap === true;
        ss.screenshotConfig.controlSnapDepth = val.controlSnapDepth ?? 15;
        ss.screenshotConfig.controlSnapDeadlineMs = val.controlSnapDeadlineMs ?? 1000;
        ss.screenshotConfig.controlSnapMinSize = val.controlSnapMinSize ?? 50;
        refreshDiagnosticsVisibility();
        refreshOcrDiagnosticsVisibility();
        // 0.18.x：配置加载完成，触发统一门控
        configReady = true;
        maybeStartCaptureHints();
      }
    })
    .catch((e) => {
      console.warn('[image-editor] 读 screenshot:config 失败,用默认值', e);
      // 配置加载失败也需放行门控（使用默认配置）
      configReady = true;
      maybeStartCaptureHints();
    });
}

/** 从独立用户编辑载荷初始化完整图片画布，不读取截图捕获 SESSION。 */
function loadEditorImage(source) {
  if (source !== IMAGE_SOURCE.CLIPBOARD) {
    throw new TypeError(`不支持的用户图片来源: ${source}`);
  }
  document.body.classList.add('image-editor-mode');
  loadEditorConfig(false);
  ss.errorHint.classList.add('hidden');
  const gen = ++ss._loadGen;
  const img = new Image();
  img.crossOrigin = 'anonymous';
  const timeoutId = setTimeout(() => {
    if (gen !== ss._loadGen || ss.editorSession.active) return;
    ss.errorHint.textContent = '图片加载超时，按 ESC 关闭';
    ss.errorHint.classList.remove('hidden');
  }, 5000);
  img.onload = () => {
    clearTimeout(timeoutId);
    if (gen !== ss._loadGen) return;
    try {
      const baseCanvas = document.createElement('canvas');
      baseCanvas.width = img.width;
      baseCanvas.height = img.height;
      const baseCtx = baseCanvas.getContext('2d', { willReadFrequently: true });
      baseCtx.drawImage(img, 0, 0);
      const imageData = baseCtx.getImageData(0, 0, img.width, img.height);
      enterCanvasImageEditor(imageData, img.width, img.height, source);
      triggerOcrPrewarm(img.width, img.height);
      console.info('[image-editor] image loaded', { source, w: img.width, h: img.height, gen });
    } catch (e) {
      console.error('[image-editor] image onload 处理失败', e);
      ss.errorHint.textContent = '图片渲染失败，按 ESC 关闭';
      ss.errorHint.classList.remove('hidden');
    }
  };
  img.onerror = (error) => {
    clearTimeout(timeoutId);
    if (gen !== ss._loadGen) return;
    console.error('[image-editor] image load failed', { source, error });
    ss.errorHint.textContent = '图片加载失败，按 ESC 关闭';
    ss.errorHint.classList.remove('hidden');
  };
  img.src = `http://blink-screenshot.localhost/editor?t=${Date.now()}`;
}

/** 进入标注模式：显示工具栏 + 定位标注 canvas + 通知后端 */
function enterAnnotationMode(rect) {
  console.info('[screenshot] enterAnnotationMode', rect);
  const { annotCanvas, screenshot } = ss;

  ss.selCss = rect;
  ss.editorSession.beginScreenshotSelection();
  ss.isAnnotating = true;
  ss.sent = false;
hidePixelMagnifier();
// Bug-fix: 进入标注模式时隐藏吸附虚线框
clearHover();
clearControlHover();

  // C 类：标注 canvas backing store = 物理像素，CSS width/height 铺满选区。
  // bitmap↔CSS 映射比 = overlay dpr，全局固定，不改 per-monitor。
  const dpr = window.devicePixelRatio || 1;
  annotCanvas.classList.remove('hidden');
  annotCanvas.style.left = rect.x + 'px';
  annotCanvas.style.top = rect.y + 'px';
  annotCanvas.style.width = rect.w + 'px';
  annotCanvas.style.height = rect.h + 'px';
  const pw = Math.max(1, Math.round(rect.w * dpr));
  const ph = Math.max(1, Math.round(rect.h * dpr));

  let cropData = null;
  try {
    const tempCanvas = document.createElement('canvas');
    tempCanvas.width = pw;
    tempCanvas.height = ph;
    const tempCtx = tempCanvas.getContext('2d');
    tempCtx.drawImage(
      screenshot,
      Math.round(rect.x * dpr), Math.round(rect.y * dpr), pw, ph,
      0, 0, pw, ph
    );
    cropData = tempCtx.getImageData(0, 0, pw, ph);
  } catch (e) {
    console.warn('[screenshot] 提取裁剪区图像失败（马赛克功能不可用）', e);
  }

  annot.reset(pw, ph, cropData);
  updateUndoRedoButtons();
  screenshotSetAnnotationMode(true).catch((e) => console.error('[screenshot] setAnnotationMode(true) 失败', e));
  drawFinalSelection();
  positionToolbar(rect);
  triggerOcrPrewarm(pw, ph);
}

/**
 * 来源无关的图片编辑入口：跳过截图 SESSION 裁剪，直接以 ImageData 初始化底图、
 * 标注画布与输出会话。长截图与剪贴板图片共用此路径。
 *
 * @param {ImageData} cropData - 来源适配器提供的完整图片
 * @param {number} pw - 物理像素宽
 * @param {number} ph - 物理像素高
 */
function enterCanvasImageEditor(cropData, pw, ph, source = IMAGE_SOURCE.LONG_SCREENSHOT) {
  console.debug('[image-editor] enterCanvasImageEditor', { source, pw, ph });
  const { annotCanvas, toolbar } = ss;
  const dpr = window.devicePixelRatio || 1;
  const cssW = pw / dpr;
  const cssH = ph / dpr;

  // 水平默认展示长图中央；竖向超出视口时从底部开始编辑。
  const initialX = Math.round((window.innerWidth - cssW) / 2);
  const initialY = cssH <= window.innerHeight
    ? Math.round((window.innerHeight - cssH) / 2)
    : Math.round(window.innerHeight - cssH);
  // 顶部至少留 12px 边距
  const clampedY = Math.max(12, initialY);
  ss.selCss = { x: initialX, y: clampedY, w: cssW, h: cssH };
  ss.isAnnotating = true;
  ss.sent = false;
  ss._imagePan = {
    x: initialX, y: clampedY, dragging: false, lastX: 0, lastY: 0,
  };
  hidePixelMagnifier();

  const baseCanvas = document.createElement('canvas');
  baseCanvas.width = pw;
  baseCanvas.height = ph;
  baseCanvas.getContext('2d').putImageData(cropData, 0, 0);
  ss.editorSession.beginCanvasSource(source, baseCanvas);
  const scrollButton = document.getElementById('btn-scroll');
  if (scrollButton) scrollButton.hidden = source === IMAGE_SOURCE.CLIPBOARD;

  // 主 canvas 作为长图可见底图；annotCanvas 只承载透明标注层。
  ss.canvas.width = pw;
  ss.canvas.height = ph;
  ss.canvas.style.left = initialX + 'px';
  ss.canvas.style.top = clampedY + 'px';
  ss.canvas.style.width = cssW + 'px';
  ss.canvas.style.height = cssH + 'px';
  ss.canvas.style.pointerEvents = '';
  ss.canvas.style.cursor = 'grab';
  ss.canvas.classList.add('long-image-editing');
  ss.ctx.clearRect(0, 0, pw, ph);
  ss.ctx.drawImage(baseCanvas, 0, 0);

  annotCanvas.classList.remove('hidden');
  annotCanvas.style.left = initialX + 'px';
  annotCanvas.style.top = clampedY + 'px';
  annotCanvas.style.width = cssW + 'px';
  annotCanvas.style.height = cssH + 'px';
  annotCanvas.width = pw;
  annotCanvas.height = ph;

  annot.reset(pw, ph, cropData);
  updateUndoRedoButtons();
  if (ss.editorSession.ownsScreenshotSession) {
    screenshotSetAnnotationMode(true).catch((e) => console.error('setAnnotationMode(true) 失败', e));
  }

  // 工具栏定位：选区未超出屏幕时贴选区下方，超出时贴屏幕底部。
  toolbar.classList.remove('hidden');
  const selectionBottom = clampedY + cssH;
  const toolbarH = toolbar.offsetHeight || 48;
  const placeBelow = selectionBottom + toolbarH + 8 <= window.innerHeight;
  const toolbarTop = placeBelow
    ? (selectionBottom + 8)
    : Math.max(8, window.innerHeight - toolbarH - 8);
  toolbar.style.top = toolbarTop + 'px';
  requestAnimationFrame(() => {
    toolbar.style.left = Math.max(8, Math.round((window.innerWidth - toolbar.offsetWidth) / 2)) + 'px';
  });
}

/** 后台预热 OCR */
function triggerOcrPrewarm(pw, ph) {
  if (!ss.screenshotConfig.prewarmOcr) return;
  if (pw < PREWARM_MIN_WIDTH || ph < PREWARM_MIN_HEIGHT) {
    console.debug('[screenshot] 预热 OCR 跳过(选区过小)', { pw, ph });
    return;
  }
  if (ss.ocrPrewarm) return;
  const revision = ss.selectionRevision;
  const startTs = performance.now();
  ss.ocrPrewarm = new Promise((resolve) => {
    compositeSelection((pngBytes) => {
      ocrImage(pngBytes)
        .then((result) => {
          if (revision !== ss.selectionRevision) {
            console.debug('[screenshot] 丢弃旧选区 OCR 预热结果', { revision, current: ss.selectionRevision });
            resolve(null);
            return;
          }
          const elapsed = Math.round(performance.now() - startTs);
          console.info('[screenshot] OCR 预热完成', { ms: elapsed, textLen: result?.text?.length ?? 0 });
          resolve(result);
        })
        .catch((rawErr) => {
          const err = normalizeError(rawErr);
          console.warn(`[screenshot] OCR 预热失败 [${err.code}] (用户点识别时会重试)`);
          resolve(null);
        });
    });
  });
}

/** 退出标注模式（清除选区，回到可拖选状态） */
function exitAnnotationMode() {
  console.info('[screenshot] exitAnnotationMode');
  if (ss._annotRaf) { cancelAnimationFrame(ss._annotRaf); ss._annotRaf = 0; }
  // 0.15.10：清除快照
  ss._committedSnapshot = null;
  const { canvas, annotCanvas, toolbar, sizeHint } = ss;
  ss.isAnnotating = false;
  ss.selCss = null;
  ss.selectionInteraction = null;
  ss.selectionRevision++;
  ss.translationRevision++;
  canvas.style.cursor = 'crosshair';
  annotCanvas.classList.add('hidden');
  annotCanvas.width = 0;
  annotCanvas.height = 0;
  toolbar.classList.add('hidden');
  sizeHint.classList.add('hidden');
  ss.ocrPrewarm = null;
  ss.ocrBusy = false;
  ss.translationBusy = false;
  ss._translateAndPinPending = false;
  updateOutputButtonsDisabled();
  ss.ocrResultCache = null;
  exitReadingMode();
  annot.clearOverlay();
  updateOverlayButtonsActive();
  toolbar.removeAttribute('data-user-moved');
  toolbar.style.left = '';
  toolbar.style.top = '';
  screenshotSetAnnotationMode(false).catch((e) => console.error('setAnnotationMode(false) 失败', e));
  drawDimmed();
}

/** 选区内容失效（移动/缩放/重框后清 OCR/阅读/overlay） */
function invalidateSelectionContent() {
  const { annotCanvas, toolbar, sizeHint } = ss;
  ss.selectionRevision++;
  ss.translationRevision++;
  ss.ocrPrewarm = null;
  ss.ocrResultCache = null;
  ss.ocrBusy = false;
  ss.translationBusy = false;
  ss._translateAndPinPending = false;
  updateOutputButtonsDisabled();
  const panel = document.getElementById('ocr-panel');
  if (panel) panel.remove();
  exitReadingMode();
  annot.clearOverlay();
  updateOverlayButtonsActive();
  annotCanvas.classList.add('hidden');
  toolbar.classList.add('hidden');
  sizeHint.classList.add('hidden');
  toolbar.removeAttribute('data-user-moved');
  toolbar.style.left = '';
  toolbar.style.top = '';
}

// ════════════════════════════════════════════════════════════
//  画布事件绑定
// ════════════════════════════════════════════════════════════

const { canvas } = ss;

/** 长图画布移动后，offsetX/Y 已经是图片局部坐标，不能再减 selCss 偏移。 */
function annotationPoint(e) {
  const dpr = window.devicePixelRatio || 1;
  if (ss._imagePan) return { x: e.offsetX * dpr, y: e.offsetY * dpr };
  return {
    x: (e.offsetX - ss.selCss.x) * dpr,
    y: (e.offsetY - ss.selCss.y) * dpr,
  };
}

function pointInEditableImage(e) {
  if (!ss.selCss) return false;
  if (!ss._imagePan) return pointInRect(e.offsetX, e.offsetY, ss.selCss);
  return e.offsetX >= 0 && e.offsetY >= 0
    && e.offsetX <= ss.selCss.w && e.offsetY <= ss.selCss.h;
}

function beginLongImagePan(e) {
  ss._imagePan.dragging = true;
  ss._imagePan.lastX = e.clientX;
  ss._imagePan.lastY = e.clientY;
  ss.canvas.style.cursor = 'grabbing';
  e.preventDefault();
}

function longImagePanBounds() {
  const w = ss.selCss?.w || 0;
  const h = ss.selCss?.h || 0;
  const margin = 48;
  return {
    minX: w <= window.innerWidth ? 0 : window.innerWidth - w - margin,
    maxX: w <= window.innerWidth ? window.innerWidth - w : margin,
    minY: h <= window.innerHeight ? 0 : window.innerHeight - h - margin,
    maxY: h <= window.innerHeight ? window.innerHeight - h : margin,
  };
}

function moveLongImagePan(e) {
  if (!ss._imagePan?.dragging) return false;
  const dx = e.clientX - ss._imagePan.lastX;
  const dy = e.clientY - ss._imagePan.lastY;
  const bounds = longImagePanBounds();
  ss._imagePan.x = Math.max(bounds.minX, Math.min(bounds.maxX, ss._imagePan.x + dx));
  ss._imagePan.y = Math.max(bounds.minY, Math.min(bounds.maxY, ss._imagePan.y + dy));
  ss._imagePan.lastX = e.clientX;
  ss._imagePan.lastY = e.clientY;
  const { annotCanvas, selCss } = ss;
  ss.canvas.style.left = ss._imagePan.x + 'px';
  ss.canvas.style.top = ss._imagePan.y + 'px';
  annotCanvas.style.left = ss._imagePan.x + 'px';
  annotCanvas.style.top = ss._imagePan.y + 'px';
  if (selCss) {
    selCss.x = ss._imagePan.x;
    selCss.y = ss._imagePan.y;
  }
  return true;
}

function endLongImagePan() {
  if (!ss._imagePan?.dragging) return false;
  ss._imagePan.dragging = false;
  ss.canvas.style.cursor = (_spaceDown || annot.getTool() === 'select') ? 'grab' : 'crosshair';
  return true;
}

canvas.addEventListener('mousedown', (e) => {
  if (!ss.screenshot && !ss._imagePan) return;

  const tool = annot.getTool();

  // 默认选取工具左键即可平移；其它工具仍可用 Space/中键临时平移。
  if (ss._imagePan && (_spaceDown || e.button === 1 || (e.button === 0 && tool === 'select'))) {
    beginLongImagePan(e);
    return;
  }

  if (e.button !== 0) return;

  // 0.15.8 R2 + 0.18.2：吸附——pending-snap 状态机（控件优先于窗口）
  // mousedown 只记录候选矩形和起点，不立即吸附；
  // mouseup 时若总位移 < 3px 才采用矩形；mousemove 达到阈值转 free-selecting。
  if (!ss.isAnnotating && !ss.selectionInteraction) {
    // 0.18.2：控件优先于窗口——控件命中时用控件矩形，否则回退窗口矩形
    const snapRect = getHoveredControlRect() || getHoveredWindowRect();
    if (snapRect) {
      ss.pendingSnap = {
        startX: e.offsetX,
        startY: e.offsetY,
        winRect: snapRect,
        pointerId: e.pointerId,
      };
      // pointer capture 保证快速拖出 canvas 后仍能收到 mouseup
      if (e.pointerId !== undefined && canvas.setPointerCapture) {
        try { canvas.setPointerCapture(e.pointerId); } catch (_) {}
      }
      return;
    }
  }

  if (ss.isAnnotating && ss.selCss && tool === 'select') {
    const handle = getSelectionHandle(e.offsetX, e.offsetY, ss.selCss);
    if (handle) {
      beginSelectionInteraction('resize', e, handle);
      return;
    }
    if (pointInRect(e.offsetX, e.offsetY, ss.selCss)) {
      beginSelectionInteraction('move', e);
      return;
    }
    beginSelectionInteraction('move', e);
    return;
  }

  if (ss.isAnnotating && ss.selCss && pointInEditableImage(e)) {
    if (tool === 'watermark') return;
    const point = annotationPoint(e);
    ss.annotStartX = point.x;
    ss.annotStartY = point.y;
    ss.annotCurrentX = ss.annotStartX;
    ss.annotCurrentY = ss.annotStartY;
    annot.startDraw(ss.annotStartX, ss.annotStartY);
    ss.isAnnotDragging = true;
    // 0.15.10：拍快照——预览时用 drawImage 恢复，避免每帧全量重放命令
    try {
      const snap = document.createElement('canvas');
      snap.width = ss.annotCanvas.width;
      snap.height = ss.annotCanvas.height;
      snap.getContext('2d').drawImage(ss.annotCanvas, 0, 0);
      ss._committedSnapshot = snap;
    } catch (e) {
      ss._committedSnapshot = null;
    }
    return;
  }

  if (ss.isAnnotating && ss.selCss) {
    console.debug('[screenshot] annotation tool click outside selection → no-op');
    return;
  }

  // 手动框选开始时立即关闭预选区虚线框，避免实线选区与虚线预选区同时出现
  clearHover();
  clearControlHover();
  ss.isDragging = true;
  ss.sent = false;
  ss.startX = e.offsetX;
  ss.startY = e.offsetY;
  ss.endX = ss.startX;
  ss.endY = ss.startY;
});

canvas.addEventListener('mousemove', (e) => {
  // 0.15.7：长图平移拖拽
  if (moveLongImagePan(e)) return;

  if (!ss.screenshot && !ss.editorSession.canvasBacked) return;

  if (!ss._imagePan) updateSelectionCursor(e.offsetX, e.offsetY);

  // 0.18.2：选区拖拽阶段智能吸附（控件优先于窗口）
  // 0.18.x：跨屏预选——先命中全局顶层窗口 → 得到 hovered hwnd → setControlTarget → 控件 hit-test
  // 手动框选拖拽中（isDragging）不更新吸附提示，避免实线选区与虚线预选区同时出现
  if (!ss.isAnnotating && !ss.selectionInteraction && !ss.isDragging) {
    // 先命中全局顶层窗口，得到 hovered hwnd
    updateWindowHover(e.offsetX, e.offsetY);
    const winRect = getHoveredWindowRect();
    setControlTarget(winRect?.hwnd ?? null);
    // 控件优先 hit-test：命中控件时清除窗口虚线框
    if (!updateControlHover(e.offsetX, e.offsetY)) {
      // 控件未命中或尚未加载：保持窗口级蓝色预选框
    } else {
      clearHover();
    }
    updatePixelMagnifier(e.offsetX, e.offsetY);
    // 0.15.12：存储最新位置供 Shift 切格式时强制刷新
    ss._lastMagnifierPos = { x: e.offsetX, y: e.offsetY };
  } else if (ss.eyedropperActive) {
    // 0.15.10：取色器模式下显示像素放大镜预览
    updatePixelMagnifier(e.offsetX, e.offsetY);
    ss._lastMagnifierPos = { x: e.offsetX, y: e.offsetY };
  } else if (ss.magnifierEl) {
    hidePixelMagnifier();
  }

  // 0.15.8 R2：pending-snap 阈值检测——达到 3px 转为自由框选
  if (ss.pendingSnap) {
    if (shouldStartFreeSelection(
      ss.pendingSnap.startX,
      ss.pendingSnap.startY,
      e.offsetX,
      e.offsetY,
    )) {
      // 达到阈值，清除候选并从原始按下点开始自由框选
      clearHover();
      clearControlHover();
      ss.startX = ss.pendingSnap.startX;
      ss.startY = ss.pendingSnap.startY;
      ss.endX = e.offsetX;
      ss.endY = e.offsetY;
      ss.pendingSnap = null;
      ss.isDragging = true;
      ss.sent = false;
      ss.snappedHwnd = null;
      // H1 优化：rAF 节流
      scheduleDrawSelection();
    }
    return;
  }

  if (ss.selectionInteraction) {
    updateSelectionInteraction(e);
    return;
  }

  updateStrokeCursor(e.clientX, e.clientY);

  if (ss.isAnnotDragging && ss.selCss) {
    const point = annotationPoint(e);
    ss.annotCurrentX = point.x;
    ss.annotCurrentY = point.y;
    if (e.shiftKey) {
      const constrained = applySquareConstraint(
        ss.annotStartX, ss.annotStartY, ss.annotCurrentX, ss.annotCurrentY, annot.getTool()
      );
      if (constrained) { ss.annotCurrentX = constrained.x; ss.annotCurrentY = constrained.y; }
    }
    annot.moveDraw(ss.annotCurrentX, ss.annotCurrentY);
    // 0.15.9：rAF 节流——每帧最多重绘一次，避免高频 mousemove 导致掉帧
    if (!ss._annotRaf) {
      ss._annotRaf = requestAnimationFrame(() => {
        ss._annotRaf = 0;
        redrawAnnotPreview();
      });
    }
    return;
  }

  if (ss.isDragging) {
    // 跨 dpr 边界选区 clamp 到起点屏（用 monitorDprAtCss 比较原生 DPI，非 overlayDpr）
    const startMon = findDisplayCssAt(ss.startX, ss.startY);
    const startDpr = monitorDprAtCss(ss.startX, ss.startY, window.__blinkScreenMeta);
    const curDpr = monitorDprAtCss(e.offsetX, e.offsetY, window.__blinkScreenMeta);
    let clampedX = e.offsetX;
    let clampedY = e.offsetY;
    if (startDpr !== curDpr) {
      // 跨 dpr 屏：clamp 到起点屏边界
      clampedX = Math.max(startMon.x, Math.min(clampedX, startMon.x + startMon.w));
      clampedY = Math.max(startMon.y, Math.min(clampedY, startMon.y + startMon.h));
    }
    ss.endX = clampedX;
    ss.endY = clampedY;
    // H1 优化：rAF 节流，避免 mousemove 高频全量重绘
    scheduleDrawSelection();
  }
});

canvas.addEventListener('mouseleave', () => {
  // W4 例外：strokeCursor 是高频逐帧更新的画笔预览光标，直接写 style.display 性能更好
  if (ss.strokeCursor) ss.strokeCursor.style.display = 'none';
  // 0.15.8 R2：离开 canvas 时清除 pending-snap 状态
  if (ss.pendingSnap) {
    ss.pendingSnap = null;
    clearHover();
    clearControlHover();
  }
  if (!ss.selectionInteraction) {
    ss.canvas.style.cursor = ss._imagePan && annot.getTool() === 'select'
      ? 'grab'
      : (annot.getTool() === 'select' ? 'default' : 'crosshair');
  }
});

canvas.addEventListener('mouseup', (e) => {
  // H1 优化：取消待执行的 drawSelection rAF，确保最终绘制是最新的
  cancelDrawSelectionRaf();
  // 0.15.7：长图平移结束
  if (endLongImagePan()) return;

  if (!ss.screenshot && !ss.editorSession.canvasBacked) return;

  // 0.15.8 R2：pending-snap 完成——未达阈值，采用窗口矩形
  if (ss.pendingSnap) {
    const winRect = ss.pendingSnap.winRect;
    ss.pendingSnap = null;
    // 释放 pointer capture
    if (e.pointerId !== undefined && canvas.releasePointerCapture) {
      try { canvas.releasePointerCapture(e.pointerId); } catch (_) {}
    }
    if (winRect.w >= 5 && winRect.h >= 5) {
      ss.snappedHwnd = winRect.hwnd || null;
      ss.startX = winRect.x;
      ss.startY = winRect.y;
      ss.endX = winRect.x + winRect.w;
      ss.endY = winRect.y + winRect.h;
      ss.isDragging = false;
      console.info('[screenshot] window snap (pending-snap confirmed)', winRect);
      try {
        enterAnnotationMode({ x: winRect.x, y: winRect.y, w: winRect.w, h: winRect.h });
      } catch (err) {
        console.error('[screenshot] window snap enterAnnotationMode threw', err);
      }
    }
    return;
  }

  if (finishSelectionInteraction(e)) return;

  if (ss.isAnnotDragging) {
    ss.isAnnotDragging = false;
    // 0.15.9：取消待执行的 rAF，确保最终重绘是最新的
    if (ss._annotRaf) { cancelAnimationFrame(ss._annotRaf); ss._annotRaf = 0; }
    // 0.15.10：清除快照
    ss._committedSnapshot = null;
    const point = annotationPoint(e);
    ss.annotCurrentX = point.x;
    ss.annotCurrentY = point.y;
    if (e.shiftKey) {
      const constrained = applySquareConstraint(
        ss.annotStartX, ss.annotStartY, ss.annotCurrentX, ss.annotCurrentY, annot.getTool()
      );
      if (constrained) { ss.annotCurrentX = constrained.x; ss.annotCurrentY = constrained.y; }
    }

    const tool = annot.getTool();
    const dx = ss.annotCurrentX - ss.annotStartX;
    const dy = ss.annotCurrentY - ss.annotStartY;
    // 0.15.1：用 TOOL_CAPS 替代硬编码 minDrag 列表
    const minDrag = (TOOL_CAPS[tool] || TOOL_CAPS.select).minDrag;
    if (Math.abs(dx) < minDrag && Math.abs(dy) < minDrag) {
      console.debug('[screenshot] annotation drag too small, skip', { tool, dx, dy });
      ss._committedSnapshot = null;
      redrawAnnotFull();
      return;
    }

    const result = annot.endDraw(ss.annotCurrentX, ss.annotCurrentY);
    if (result && result.needsText) {
      showTextInput(result.x, result.y);
    }
    redrawAnnotFull();
    updateUndoRedoButtons();
    return;
  }

  if (!ss.isDragging) return;
  ss.isDragging = false;
  ss.endX = e.offsetX;
  ss.endY = e.offsetY;

  const rect = norm(ss.startX, ss.startY, ss.endX, ss.endY);
  if (rect.w < 5 || rect.h < 5) {
    console.debug('[screenshot] rect too small, wait for dblclick', rect);
    if (ss.singleClickTimeout) clearTimeout(ss.singleClickTimeout);
    ss.singleClickTimeout = setTimeout(() => {
      ss.singleClickTimeout = null;
      if (!ss.isAnnotating && !ss.sent) {
        console.info('[screenshot] single click → hide overlay');
        hideScreenshotOverlay().catch((err) => console.error('hideScreenshotOverlay 失败', err));
      }
    }, 200);
    return;
  }

  console.info('[screenshot] selection confirmed', rect);
  // 0.15.8 R2：自由框选不关联窗口 HWND
  ss.snappedHwnd = null;
  try {
    enterAnnotationMode(rect);
  } catch (e) {
    console.error('[screenshot] enterAnnotationMode threw', e);
  }
});

canvas.addEventListener('dblclick', (e) => {
  console.debug('[screenshot] dblclick', { isAnnotating: ss.isAnnotating, hasSelCss: !!ss.selCss, sent: ss.sent });
  if ((!ss.screenshot && !ss.editorSession.canvasBacked) || ss.sent) return;
  if (ss.singleClickTimeout) { clearTimeout(ss.singleClickTimeout); ss.singleClickTimeout = null; }

  if (ss.isAnnotating && ss.selCss) {
    if (pointInEditableImage(e)) {
      doCopySelection();
    }
    return;
  }

  doCopyFullScreen();
});

canvas.addEventListener('contextmenu', (e) => {
  e.preventDefault();
  if (ss.reading) {
    const selText = getReadingSelectionText();
    showReadingContextMenu(selText || null, e);
  } else if (ss.isAnnotating && ss.selCss) {
    doCopySelection();
  } else {
    doCancel();
  }
});

document.addEventListener('keydown', (e) => {
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
  // 0.15.12：Shift 切换放大镜色值格式（选区拖拽阶段 或 取色器模式）
  // 0.15.8 R3：忽略 keydown.repeat，防止按住 Shift 时连续切换
  if (e.key === 'Shift' && !e.repeat && !e.ctrlKey && !e.metaKey && !e.altKey && (!ss.isAnnotating || ss.eyedropperActive)) {
    const tgt = e.target;
    if (tgt && (tgt.tagName === 'INPUT' || tgt.tagName === 'TEXTAREA' || tgt.isContentEditable)) return;
    cycleMagnifierFormat();
    // 0.15.12：立即强制刷新放大镜显示（不等到下一帧 mousemove）
    if (ss._lastMagnifierPos) {
      updatePixelMagnifier(ss._lastMagnifierPos.x, ss._lastMagnifierPos.y);
    }
    return;
  }
  // 0.15.12：C 键复制放大镜色值（选区拖拽阶段 或 取色器模式，非 Ctrl+C）
  if ((e.key === 'c' || e.key === 'C') && !e.ctrlKey && !e.metaKey && !e.altKey && (!ss.isAnnotating || ss.eyedropperActive)) {
    const tgt = e.target;
    if (tgt && (tgt.tagName === 'INPUT' || tgt.tagName === 'TEXTAREA' || tgt.isContentEditable)) return;
    const colorText = getMagnifierColorText();
    if (colorText) {
      e.preventDefault();
      copyToClipboard(colorText).then(() => {
        if (ss._showTransientHint) ss._showTransientHint(`已复制 ${colorText}`);
      });
    }
    return;
  }
  if (e.key === 'Escape') {
    e.preventDefault();
    // 0.15.7：长截图采集阶段——ESC 先退出长截图模式
    if (isScrollCaptureActive()) {
      exitScrollCapture().catch(() => {});
      return;
    }
    const ocrPanel = document.getElementById('ocr-panel');
    if (ocrPanel) {
      ocrPanel.remove();
      return;
    }
    // 0.15.11：水印表单移至 sub-panel，关闭 sub-panel 即可
    const subPanel = document.getElementById('sub-panel');
    if (subPanel && !subPanel.classList.contains('hidden')) {
      subPanel.classList.add('hidden');
      return;
    }
    const openDropdown = document.querySelector('.dropdown[data-open="true"]');
    if (openDropdown) {
      openDropdown.setAttribute('data-open', 'false');
      return;
    }
    doCancel();
    return;
  }
  if ((e.key === 'e' || e.key === 'E') && !e.ctrlKey && !e.metaKey && !e.altKey && ss.isAnnotating) {
    const tgt = e.target;
    if (tgt && (tgt.tagName === 'INPUT' || tgt.tagName === 'TEXTAREA' || tgt.isContentEditable)) return;
    e.preventDefault();
    doPanelToggle();
  }
});

// 0.11.8-e：矩形/椭圆拖动期间按/松 Shift 实时更新预览
window.addEventListener('keydown', refreshShapePreviewOnShift);

// 0.15.7：长截图手动滚动检测——capturing 阶段 wheel 触发截帧
window.addEventListener('wheel', onScrollWheel, { passive: true });

// 鼠标在窄图边缘外松开时 canvas 收不到 mouseup，需在 window 层兜底结束平移。
window.addEventListener('mouseup', endLongImagePan);
window.addEventListener('mousemove', (e) => {
  if (e.target !== canvas) moveLongImagePan(e);
});

// 0.15.7：长图编辑——Space 或中键拖拽平移超长图（_spaceDown 已在文件顶部声明）
window.addEventListener('keydown', (e) => {
  if (e.code === 'Space' && ss._imagePan && !ss._imagePan.dragging) {
    const tgt = e.target;
    if (tgt && (tgt.tagName === 'INPUT' || tgt.tagName === 'TEXTAREA' || tgt.isContentEditable)) return;
    _spaceDown = true;
    if (ss.canvas) ss.canvas.style.cursor = 'grab';
    e.preventDefault();
  }
});
window.addEventListener('keyup', (e) => {
  if (e.code === 'Space') {
    _spaceDown = false;
    if (ss.canvas && !ss.isAnnotDragging) {
      ss.canvas.style.cursor = ss._imagePan && annot.getTool() === 'select' ? 'grab' : '';
    }
  }
});

window.addEventListener('blur', () => {
  if (ss.blurGuard) return;
  ss.blurGuard = true;
  setTimeout(() => { ss.blurGuard = false; }, 500);

  if (hasActivePanel()) {
    console.debug('[screenshot] window blur ignored (active panel)');
    return;
  }

  // H1 优化：取消待执行的 drawSelection rAF
  cancelDrawSelectionRaf();
  console.debug('[screenshot] window blur, hiding overlay');
  if (isScrollCaptureActive()) {
    exitScrollCapture(false)
      .catch((e) => console.warn('[screenshot] blur: scroll cleanup failed', e))
      .finally(() => hideScreenshotOverlay().catch((e) => console.error('hideScreenshotOverlay 失败', e)));
  } else {
    hideScreenshotOverlay().catch((e) => console.error('hideScreenshotOverlay 失败', e));
  }
});

// ════════════════════════════════════════════════════════════
//  工具栏绑定
// ════════════════════════════════════════════════════════════

bindToolbar();
