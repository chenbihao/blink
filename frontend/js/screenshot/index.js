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
import { norm, pointInRect, applySquareConstraint, computePanAxisBounds, computeCanvasEditorInitialPosition } from "./ss-utils.js";
import { shouldStartFreeSelection, syncRenderScale, cssRectToBitmap, cssPointToScreen, cssPointToBitmap, getRenderScale } from "./ss-selection-geometry.js";
import { applySquareResize } from "./ss-selection-geometry.js";
import { drawDimmed, drawStaticBase, drawSelection, drawFinalSelection, redrawAnnotPreview, redrawAnnotFull, scheduleDrawSelection, cancelDrawSelectionRaf, scheduleDrawFinalSelection, cancelDrawFinalSelectionRaf, syncInteractionCanvasSize } from "./ss-draw.js";
import { positionToolbar, invalidateDisplaysCache, findDisplayCssAt, getMonitorForScroll } from "./ss-display.js";
import {
  getSelectionHandle, beginSelectionInteraction, updateSelectionInteraction,
  finishSelectionInteraction, updateSelectionCursor, refreshShapePreviewOnShift,
  updateStrokeCursor,
  updatePixelMagnifier, hidePixelMagnifier, cycleMagnifierFormat, getMagnifierColorText,
  moveSelection1px,
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
  cleanupCanvasVisuals,
} from "./ss-output.js";
import { bindToolbar, showTextInput, updateUndoRedoButtons, selectTool, cycleToolInGroup, TOOL_GROUPS } from "./ss-toolbar.js";
// 0.15.8：智能窗口吸附 + 像素放大镜
import { loadPickableWindows, clearPickableWindows, updateWindowHover, getHoveredWindowRect, clearHover, hideWindowHintIfVisible, showWindowHintIfPending } from "./ss-hover.js";
// 0.18.2：控件级智能吸附（跨屏预选版）
import {
  setControlTarget, prefetchControlHints, clearControlHints, updateControlHover,
  getHoveredControlRect, clearControlHover,
} from "./ss-control-hints.js";
// 0.15.7：长截图
import {
  enterScrollCapture, exitScrollCapture, onScrollWheel,
  bindScrollToolbar, enterScrollEdit, isScrollCaptureActive, isScrollCapturing,
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
let _screenshotReadyTs = 0; // ss.screenshot 赋值时刻
let _firstMousedownLogged = false;

/** 统一门控：截图 + 窗口列表 + 控件预热只触发一次 */
function maybeStartCaptureHints() {
  if (captureHintsStarted) return;
  if (!screenshotReady) return;
  if (!renderScaleReady) return;
  if (!configReady) return;

  captureHintsStarted = true;
  console.debug('[screenshot] maybeStartCaptureHints fired', { screenshotReady, renderScaleReady, configReady });
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

// P0 优化：clearVisual 不再 clearRect 暗罩（保留 resetState 画的 P5 暗罩），
// 并立即启动 fetch 预取——与 show+focus+double rAF 并行，省 ~80ms。
window.__blinkClearScreenshotVisual = function () {
  console.debug('[screenshot] __blinkClearScreenshotVisual called');
  try {
    resetState();
    // 不 clearRect——resetState 末尾已画出 P5 暗罩（rgba(0,0,0,0.45)），
    // 保留它让用户在窗口 show 的瞬间就看到暗色背景 + 十字光标。
  } catch (e) {
    console.error('[screenshot] clearScreenshotVisual threw', e);
  }
  // 立即启动 fetch 预取——不等 double rAF，与 show+focus 并行。
  // SESSION 在 begin_session 完成后就准备好了，此时可以安全读取。
  const _tPreload = performance.now();
  const _activeMonitor = window.__blinkActiveDisplay ?? 0;
  window.__blinkScreenshotPreload = fetch(`http://blink-screenshot.localhost/raw?monitor=${_activeMonitor}&t=${Date.now()}`)
    .then(r => {
      if (!r.ok) throw new Error(`preload fetch failed: ${r.status}`);
      // Tauri 自定义协议不暴露自定义 headers 给前端 fetch API，
      // 尺寸/偏移由 loadScreenshot 从 __blinkScreenMeta.physicalDisplays 计算。
      return r.arrayBuffer();
    })
    .then(buf => {
      console.debug('[screenshot] preload fetch done', { ms: Math.round(performance.now() - _tPreload), bytes: buf.byteLength, monitor: _activeMonitor });
      return buf;
    })
    .catch(e => {
      console.error('[screenshot] preload fetch error', e);
      return null;
    });
};

window.__blinkReloadScreenshot = function () {
  console.debug('[screenshot] __blinkReloadScreenshot called');
  // P0 优化：resetState 已在 clearVisual 里调过一次，这里跳过避免重复。
  // 但如果 clearVisual 没被调过（首次创建路径），仍需要 resetState。
  if (!window.__blinkScreenshotPreload) {
    try {
      resetState();
    } catch (e) {
      console.error('[screenshot] resetState threw, attempting to continue', e);
    }
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
  const _t0 = performance.now();
  console.debug('[screenshot] resetState start');
  resetScrollCaptureSession();
  const { canvas, ctx, annotCanvas, annotCtx, sizeHint, toolbar, errorHint } = ss;
  ss._loadGen++;  // BUG1 fix: 使待处理的旧 img.onload 回调失效
  // 0.15.9：取消待执行的标注预览 rAF
  if (ss._annotRaf) { cancelAnimationFrame(ss._annotRaf); ss._annotRaf = 0; }
  // 取消待执行的选区绘制 rAF，防止 resetState 后旧 rAF 用 null source 尝试绘制
  cancelDrawSelectionRaf();
  cancelDrawFinalSelectionRaf();
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
  // 0.20.5：清空动态交互层
  if (ss.interactionCanvas && ss.interactionCanvas.width > 0 && ss.interactionCtx) {
    ss.interactionCtx.clearRect(0, 0, ss.interactionCanvas.width, ss.interactionCanvas.height);
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
  // P0 优化：清理预取 promise，防止上一轮截图的预取残留到新一轮
  window.__blinkScreenshotPreload = null;
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
  ss._magnifierSampleGen = (ss._magnifierSampleGen || 0) + 1;
  ss._pendingMagnifierPos = null;
  // 0.20.6：重置取色器状态
  ss.colorPickerMode = 'idle';
  ss._pickerBitmapPos = null;
  if (ss.precisionHint) ss.precisionHint.classList.add('hidden');
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
  // P5: 立即画暗罩——不等截图加载完成，消除"锁死感"
  // 用户看到暗色背景 + 十字光标，可以立即移动鼠标
  // 截图加载后 drawDimmed() 会叠加截图图像
  if (canvas.width > 0 && ctx) {
    ctx.fillStyle = 'rgba(0, 0, 0, 0.45)';
    ctx.fillRect(0, 0, canvas.width, canvas.height);
  }
  console.debug('[screenshot] resetState done', { ms: Math.round(performance.now() - _t0) });
}

// A+B 优化：BGRA → RGBA 原地 swap（u32 位运算，与后端 swap_rb_u32 等价）
// 在前端做 swap 省掉后端 43MB 分配 + 遍历，且 per-monitor 数据量更小（~15MB）
function swapBgraToRgba(buffer) {
  const u32 = new Uint32Array(buffer);
  for (let i = 0; i < u32.length; i++) {
    const v = u32[i];
    const rb = v & 0x00FF00FF;
    const ga = v & 0xFF00FF00;
    u32[i] = ga | (rb << 16) | (rb >>> 16);
  }
}

// P4: raw BGRA 协议——fetch raw BGRA bytes + 前端 swap + ImageData + putImageData
// A+B 优化：per-monitor 分块加载，先传光标所在屏（~15MB），其他屏懒加载
async function loadScreenshot() {
  const _t0 = performance.now();
  console.debug('[screenshot] loadScreenshot start');
  ss.errorHint.classList.add('hidden');

  // 配置读取与图像加载并行；失败时保留默认值。
  loadEditorConfig(true);

  // 加载代际守卫
  const gen = ++ss._loadGen;

  // 0.15.9：加载超时检测——5 秒未完成则提示错误（防止协议请求静默失败）
  const timeoutId = setTimeout(() => {
    if (gen !== ss._loadGen) return;
    if (ss.screenshot) return;
    console.error('[screenshot] 加载超时（5s），协议请求可能失败', { gen });
    ss.errorHint.textContent = '截图加载超时，按 ESC 重试';
    ss.errorHint.classList.remove('hidden');
  }, 5000);

  try {
    // P0 优化：复用 clearVisual 阶段启动的预取 fetch，避免重复请求。
    // 预取在 show 之前就开始了，这里可能已经完成（0ms 等待）。
    const preloadPromise = window.__blinkScreenshotPreload;
    let rawBuffer;
    if (preloadPromise) {
      console.debug('[screenshot] reusing preload fetch', { gen });
      rawBuffer = await preloadPromise;
      window.__blinkScreenshotPreload = null;
      if (gen !== ss._loadGen) {
        console.debug('[screenshot] 丢弃过期截图加载回调', { gen, cur: ss._loadGen });
        return;
      }
      if (!rawBuffer) throw new Error('preload fetch 返回 null');
    } else {
      console.debug('[screenshot] requesting raw bgra (no preload)', { gen });
      const _tFetchStart = performance.now();
      const _activeMonitor = window.__blinkActiveDisplay ?? 0;
      const response = await fetch(`http://blink-screenshot.localhost/raw?monitor=${_activeMonitor}&t=${Date.now()}`);
      if (!response.ok) throw new Error(`fetch failed: ${response.status}`);
      rawBuffer = await response.arrayBuffer();
      console.debug('[screenshot] raw bgra fetched', { gen, ms: Math.round(performance.now() - _tFetchStart), bytes: rawBuffer.byteLength });
      if (gen !== ss._loadGen) {
        console.debug('[screenshot] 丢弃过期截图加载回调', { gen, cur: ss._loadGen });
        return;
      }
    }

    // 从 __blinkScreenMeta 取完整虚拟桌面尺寸 + 显示器列表
    const meta = window.__blinkScreenMeta || {};
    const w = meta.w || 0;
    const h = meta.h || 0;
    if (w === 0 || h === 0) throw new Error('invalid dimensions from __blinkScreenMeta');

    // A 优化：从 physicalDisplays 计算活动显示器的尺寸和偏移
    // （Tauri 自定义协议不暴露自定义 headers 给前端 fetch API）
    const _activeIdx = window.__blinkActiveDisplay ?? 0;
    const _displays = meta.physicalDisplays || [];
    const _activeDisp = _displays[_activeIdx];
    if (!_activeDisp || !_activeDisp.w || !_activeDisp.h) {
      throw new Error('active display geometry unavailable');
    }
    const activeData = {
      buffer: rawBuffer,
      width: _activeDisp.w,
      height: _activeDisp.h,
      offsetX: _activeDisp.x - (meta.vx || 0),
      offsetY: _activeDisp.y - (meta.vy || 0),
    };
    if (!activeData.buffer || activeData.width === 0 || activeData.height === 0) {
      throw new Error('active monitor data invalid');
    }

    const _tPutStart = performance.now();

    // A+B 优化：offscreen canvas 建为完整虚拟桌面尺寸，
    // 逐显示器 putImageData 到各自偏移位置。
    const { canvas } = ss;
    canvas.width = w;
    canvas.height = h;

    ss.screenshotOffscreen = document.createElement('canvas');
    ss.screenshotOffscreen.width = w;
    ss.screenshotOffscreen.height = h;
    const offCtx = ss.screenshotOffscreen.getContext('2d', { willReadFrequently: true });

    // 写入光标所在屏的 BGRA→RGBA 数据
    swapBgraToRgba(activeData.buffer);
    const activeImageData = new ImageData(
      new Uint8ClampedArray(activeData.buffer),
      activeData.width,
      activeData.height
    );
    offCtx.putImageData(activeImageData, activeData.offsetX, activeData.offsetY);

    // ss.screenshot 设为 offscreen canvas（drawImage 接受 canvas source）
    ss.screenshot = ss.screenshotOffscreen;
    _screenshotReadyTs = performance.now();
    _firstMousedownLogged = false;

    clearTimeout(timeoutId);
    const _tPutEnd = performance.now();
    console.debug('[screenshot] putImageData done', { w: activeData.width, h: activeData.height, ox: activeData.offsetX, oy: activeData.offsetY, ms: Math.round(_tPutEnd - _tPutStart) });

    // 等布局稳定后同步 renderScale，再绘制和加载窗口/控件
    requestAnimationFrame(() => {
      if (gen !== ss._loadGen) return;
      const _tRafStart = performance.now();
      const meta = window.__blinkScreenMeta || { vx: 0, vy: 0 };
      syncRenderScale(canvas, meta);
      // M7 优化：初始 renderScale 设定后失效 displays 缓存
      invalidateDisplaysCache();
      drawDimmed();
      const _tRafEnd = performance.now();
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
        vx: meta.vx, vy: meta.vy, vw: meta.w, vh: meta.h,
        physicalDisplays: meta.physicalDisplays,
      });
      // 0.18.x：统一门控触发窗口列表加载 + 控件预热
      screenshotReady = true;
      renderScaleReady = true;
      maybeStartCaptureHints();
      console.debug('[screenshot] rAF render done', { ms: Math.round(_tRafEnd - _tRafStart), totalMs: Math.round(performance.now() - _t0) });
    });

    // A 优化：懒加载其他显示器——不阻塞用户交互，后台 fetch + swap + putImageData
    for (let i = 0; i < _displays.length; i++) {
      if (i === _activeIdx) continue;
      const _disp = _displays[i];
      if (!_disp || !_disp.w || !_disp.h) continue;
      fetch(`http://blink-screenshot.localhost/raw?monitor=${i}&t=${Date.now()}`)
        .then(r => {
          if (!r.ok) return null;
          return r.arrayBuffer();
        })
        .then(buf => {
          if (gen !== ss._loadGen) return; // 过期丢弃
          if (!buf || buf.byteLength === 0) return;
          const _tLazy = performance.now();
          swapBgraToRgba(buf);
          const imgData = new ImageData(
            new Uint8ClampedArray(buf),
            _disp.w,
            _disp.h
          );
          offCtx.putImageData(imgData, _disp.x - (meta.vx || 0), _disp.y - (meta.vy || 0));
          console.debug('[screenshot] lazy monitor loaded', { monitor: i, ms: Math.round(performance.now() - _tLazy) });
          // 仅在暗色蒙版态（未拖选/未标注）时刷新主 canvas，
          // 拖选中/标注中不调 drawDimmed（下次 mousemove/redraw 会自然读到新数据）
          if (!ss.isDragging && !ss.isAnnotating && !ss.selectionInteraction) {
            drawDimmed();
          }
        })
        .catch(e => console.warn('[screenshot] lazy monitor load failed', e));
    }

    console.debug('[screenshot] screenshot loaded', { w, h, gen, activeMonitor: _activeIdx, totalMonitors: _displays.length, ms: Math.round(performance.now() - _t0) });
  } catch (e) {
    clearTimeout(timeoutId);
    if (gen !== ss._loadGen) return;
    console.error('[screenshot] loadScreenshot failed', e, { gen });
    ss.errorHint.textContent = '截图加载失败，按 ESC 重试';
    ss.errorHint.classList.remove('hidden');
  }
}

function loadEditorConfig(includeCaptureHints) {
  const _t0 = performance.now();
  invoke('get_config_section', { key: 'screenshot:config' })
    .then((val) => {
      console.debug('[screenshot] loadEditorConfig done', { ms: Math.round(performance.now() - _t0), hasVal: !!val });
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
      console.debug('[screenshot] loadEditorConfig failed', { ms: Math.round(performance.now() - _t0) });
      // 配置加载失败也需放行门控（使用默认配置）
      configReady = true;
      maybeStartCaptureHints();
    });
}

/** 从独立用户编辑载荷初始化完整图片画布，不读取截图捕获 SESSION。 */
function loadEditorImage(source) {
  if (source !== IMAGE_SOURCE.CLIPBOARD && source !== IMAGE_SOURCE.HISTORY && source !== IMAGE_SOURCE.PIN) {
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
  const _t0 = performance.now();
  console.debug('[screenshot] enterAnnotationMode', rect);
  const { annotCanvas, screenshot } = ss;

  ss.selCss = rect;
  ss.editorSession.beginScreenshotSelection();
  ss.isAnnotating = true;
  ss.sent = false;
hidePixelMagnifier();
// Bug-fix: 进入标注模式时隐藏吸附虚线框
clearHover();
clearControlHover();

  // 标注 canvas backing store = 物理像素，CSS width/height 铺满选区。
  // bitmap rect 来自 cssRectToBitmap（使用实测 renderScale）。
  const meta = window.__blinkScreenMeta || { vx: 0, vy: 0 };
  const bmpRect = cssRectToBitmap(rect, meta);
  annotCanvas.classList.remove('hidden');
  annotCanvas.style.left = rect.x + 'px';
  annotCanvas.style.top = rect.y + 'px';
  annotCanvas.style.width = rect.w + 'px';
  annotCanvas.style.height = rect.h + 'px';
  const pw = Math.max(1, bmpRect.w);
  const ph = Math.max(1, bmpRect.h);

  let cropData = null;
  try {
    const _tCropStart = performance.now();
    const tempCanvas = document.createElement('canvas');
    tempCanvas.width = pw;
    tempCanvas.height = ph;
    const tempCtx = tempCanvas.getContext('2d');
    tempCtx.drawImage(
      screenshot,
      bmpRect.x, bmpRect.y, pw, ph,
      0, 0, pw, ph
    );
    cropData = tempCtx.getImageData(0, 0, pw, ph);
    console.debug('[screenshot] enterAnnotationMode: crop+getImageData', { pw, ph, ms: Math.round(performance.now() - _tCropStart) });
  } catch (e) {
    console.warn('[screenshot] 提取裁剪区图像失败（马赛克功能不可用）', e);
  }

  const _tResetStart = performance.now();
  annot.reset(pw, ph, cropData);
  updateUndoRedoButtons();
  console.debug('[screenshot] enterAnnotationMode: annot.reset', { ms: Math.round(performance.now() - _tResetStart) });
  screenshotSetAnnotationMode(true).catch((e) => console.error('[screenshot] setAnnotationMode(true) 失败', e));
  const _tDrawStart = performance.now();
  drawFinalSelection();
  console.debug('[screenshot] enterAnnotationMode: drawFinalSelection', { ms: Math.round(performance.now() - _tDrawStart) });
  positionToolbar(rect);
  const _tOcrStart = performance.now();
  triggerOcrPrewarm(pw, ph);
  console.debug('[screenshot] enterAnnotationMode: triggerOcrPrewarm (sync part)', { ms: Math.round(performance.now() - _tOcrStart) });
  console.debug('[screenshot] enterAnnotationMode: total', { ms: Math.round(performance.now() - _t0) });
}

/**
 * 来源无关的图片编辑入口：跳过截图 SESSION 裁剪，直接以 ImageData 初始化底图、
 * 标注画布与输出会话。长截图与剪贴板图片共用此路径。
 *
 * @param {ImageData} cropData - 来源适配器提供的完整图片
 * @param {number} pw - 物理像素宽
 * @param {number} ph - 物理像素高
 * @param {string} source - 图片来源（IMAGE_SOURCE）
 * @param {{x,y,w,h}|null} sourceMonitor - 来源显示器 CSS 矩形
 */
function enterCanvasImageEditor(cropData, pw, ph, source = IMAGE_SOURCE.LONG_SCREENSHOT, sourceMonitor = null) {
  console.debug('[image-editor] enterCanvasImageEditor', { source, pw, ph, sourceMonitor });
  const { annotCanvas, toolbar } = ss;
  // CSS 尺寸 = bitmap 尺寸 / renderScale
  const meta = window.__blinkScreenMeta || { vx: 0, vy: 0 };
  const { scaleX: rsx, scaleY: rsy } = getRenderScale(meta);
  const cssW = pw / rsx;
  const cssH = ph / rsy;

  // 使用来源显示器矩形作为定位容器，默认居中到该屏幕，而不是虚拟桌面。
  const mon = sourceMonitor || { x: 0, y: 0, w: window.innerWidth, h: window.innerHeight };

  const initial = computeCanvasEditorInitialPosition(cssW, cssH, mon);
  const initialX = initial.x;
  const initialY = initial.y;
  ss.selCss = { x: initialX, y: initialY, w: cssW, h: cssH };
  ss.isAnnotating = true;
  ss.sent = false;
  ss._imagePan = {
    x: initialX, y: initialY, dragging: false, lastX: 0, lastY: 0,
    monitor: mon,
  };
  hidePixelMagnifier();

  // 主动隐藏 sizeHint——canvas-backed 编辑器不需要截图坐标提示
  if (ss.sizeHint) ss.sizeHint.classList.add('hidden');

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
  ss.canvas.style.top = initialY + 'px';
  ss.canvas.style.width = cssW + 'px';
  ss.canvas.style.height = cssH + 'px';
  ss.canvas.style.pointerEvents = '';
  ss.canvas.style.cursor = 'grab';
  ss.canvas.classList.add('long-image-editing');
  ss.ctx.clearRect(0, 0, pw, ph);
  ss.ctx.drawImage(baseCanvas, 0, 0);
  // 0.20.5：同步交互层尺寸并清空（长图编辑模式下不使用选区遮罩，但保持一致性）
  syncInteractionCanvasSize();
  if (ss.interactionCtx) {
    ss.interactionCtx.clearRect(0, 0, pw, ph);
  }

  annotCanvas.classList.remove('hidden');
  annotCanvas.style.left = initialX + 'px';
  annotCanvas.style.top = initialY + 'px';
  annotCanvas.style.width = cssW + 'px';
  annotCanvas.style.height = cssH + 'px';
  annotCanvas.width = pw;
  annotCanvas.height = ph;

  annot.reset(pw, ph, cropData);
  updateUndoRedoButtons();
  if (ss.editorSession.ownsScreenshotSession) {
    screenshotSetAnnotationMode(true).catch((e) => console.error('setAnnotationMode(true) 失败', e));
  }

  // 工具栏定位：选区未超出来源显示器时贴选区下方，超出时贴来源显示器底部。
  toolbar.classList.remove('hidden');
  const selectionBottom = initialY + cssH;
  const toolbarH = toolbar.offsetHeight || 48;
  const placeBelow = selectionBottom + toolbarH + 8 <= mon.y + mon.h;
  const toolbarTop = placeBelow
    ? (selectionBottom + 8)
    : Math.max(mon.y + 8, mon.y + mon.h - toolbarH - 8);
  toolbar.style.top = toolbarTop + 'px';
  requestAnimationFrame(() => {
    toolbar.style.left = Math.max(mon.x + 8, Math.round(mon.x + (mon.w - toolbar.offsetWidth) / 2)) + 'px';
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
          console.debug('[screenshot] OCR 预热完成', { ms: elapsed, textLen: result?.text?.length ?? 0 });
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
  console.debug('[screenshot] exitAnnotationMode');
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
  // CSS 局部坐标按实际 annotation canvas backing/CSS 比例（= renderScale）转换
  const meta = window.__blinkScreenMeta || { vx: 0, vy: 0 };
  const { scaleX: rsx, scaleY: rsy } = getRenderScale(meta);
  if (ss._imagePan) return { x: e.offsetX * rsx, y: e.offsetY * rsy };
  return {
    x: (e.offsetX - ss.selCss.x) * rsx,
    y: (e.offsetY - ss.selCss.y) * rsy,
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
  // 使用来源显示器矩形作为平移边界基准，而不是虚拟桌面。
  const mon = ss._imagePan?.monitor || { x: 0, y: 0, w: window.innerWidth, h: window.innerHeight };
  const xBounds = computePanAxisBounds(w, mon.w, mon.x);
  const yBounds = computePanAxisBounds(h, mon.h, mon.y);
  return { minX: xBounds.min, maxX: xBounds.max, minY: yBounds.min, maxY: yBounds.max };
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
  if (!_firstMousedownLogged) {
    _firstMousedownLogged = true;
    const delta = _screenshotReadyTs > 0 ? Math.round(performance.now() - _screenshotReadyTs) : -1;
    console.debug('[screenshot] first mousedown', { hasScreenshot: !!ss.screenshot, deltaSinceReady: delta });
  }
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
    // 0.19.15：点击选区外部 → 退出标注模式，回到自由框选状态（不取消截图）。
    // 此前行为是 beginSelectionInteraction('move')，对全屏选区来说没有"外部"，
    // 用户被困在全屏标注中无法退出。改为仅退出标注模式，overlay 保留，
    // 用户可再次 click+drag 开始新选区。
    console.debug('[screenshot] click outside selection → exit annotation mode');
    exitAnnotationMode();
    clearHover();
    clearControlHover();
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
  // 0.19.14-fix：先 hit-test 窗口获取 hwnd 但不显示 hint，等控件 hit-test 结果再决定显示哪个 hint，
  // 避免控件命中时每帧 show→hide 窗口 hint 导致蓝色虚线框闪烁
  if (!ss.isAnnotating && !ss.selectionInteraction && !ss.isDragging) {
    // 第一步：窗口 hit-test（仅更新内部索引，不显示 hint）
    updateWindowHover(e.offsetX, e.offsetY, { skipShowHint: true });
    const winRect = getHoveredWindowRect();
    setControlTarget(winRect?.hwnd ?? null);
    // 第二步：控件优先 hit-test
    if (updateControlHover(e.offsetX, e.offsetY)) {
      // 控件命中：隐藏窗口 hint（可能上次鼠标在窗口空白处时显示了）
      hideWindowHintIfVisible();
    } else {
      // 控件未命中或尚未加载：显示窗口级蓝色预选框
      showWindowHintIfPending();
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
    // 0.19.15：移除跨 DPR clamp——canvas backing store = 虚拟桌面物理像素（1:1），
    // renderScale 全局一致，cssRectToBitmap 对任意屏的 CSS 坐标都能正确映射到
    // SESSION 物理像素坐标。跨 DPR 选区的裁剪/复制/pin 均由后端 crop_bgra_virtual
    // 按虚拟屏幕坐标直接裁剪，不存在比例错误。
    // 0.20.6：Shift 按下时强制 1:1 正方形约束（自由框选 mousemove 路径同步）
    if (e.shiftKey) {
      const dx = e.offsetX - ss.startX;
      const dy = e.offsetY - ss.startY;
      const side = Math.max(Math.abs(dx), Math.abs(dy));
      ss.endX = ss.startX + (dx >= 0 ? side : -side);
      ss.endY = ss.startY + (dy >= 0 ? side : -side);
    } else {
      ss.endX = e.offsetX;
      ss.endY = e.offsetY;
    }
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
  cancelDrawFinalSelectionRaf();
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
      console.debug('[screenshot] window snap (pending-snap confirmed)', winRect);
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
  // 0.20.6：Shift 按下时强制 1:1 正方形约束（自由框选 mouseup 路径同步）
  if (e.shiftKey) {
    const dx = e.offsetX - ss.startX;
    const dy = e.offsetY - ss.startY;
    const side = Math.max(Math.abs(dx), Math.abs(dy));
    ss.endX = ss.startX + (dx >= 0 ? side : -side);
    ss.endY = ss.startY + (dy >= 0 ? side : -side);
  } else {
    ss.endX = e.offsetX;
    ss.endY = e.offsetY;
  }

  const rect = norm(ss.startX, ss.startY, ss.endX, ss.endY);
  if (rect.w < 5 || rect.h < 5) {
    console.debug('[screenshot] rect too small, wait for dblclick', rect);
    if (ss.singleClickTimeout) clearTimeout(ss.singleClickTimeout);
    ss.singleClickTimeout = setTimeout(() => {
      ss.singleClickTimeout = null;
      if (!ss.isAnnotating && !ss.sent) {
        console.debug('[screenshot] single click → hide overlay');
        hideScreenshotOverlay().catch((err) => console.error('hideScreenshotOverlay 失败', err));
      }
    }, 200);
    return;
  }

  // ⚠️ 临时诊断日志（跨 DPR 排查用），收尾时清理
  const _meta = window.__blinkScreenMeta || {};
  const _bmp = cssRectToBitmap(rect, _meta);
  console.debug('[screenshot] selection confirmed', {
    cssRect: rect,
    bmpRect: _bmp,
    renderScale: _meta.renderScaleX,
    dpr: window.devicePixelRatio,
    physicalDisplays: _meta.physicalDisplays,
  });
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
  // ── Alt 快捷键：工具切换 + undo/reset（仅标注模式生效）──────────
  // Alt+` → 选取工具；Alt+1~5 → 图形/画笔/文字/马赛克/橡皮；重复按循环组内下一个
  // Alt+Z → undo；Alt+R → reset（清除全部标注）
  if (e.altKey && !e.ctrlKey && !e.metaKey && ss.isAnnotating) {
    const tgt = e.target;
    if (tgt && (tgt.tagName === 'INPUT' || tgt.tagName === 'TEXTAREA' || tgt.isContentEditable)) return;
    const key = e.key;
    if (key === '`' || key === '~') {
      e.preventDefault();
      selectTool('select');
      return;
    }
    if (key === '1') {
      e.preventDefault();
      // 当前已在 shape 组内则循环，否则切到默认工具
      if (TOOL_GROUPS[annot.getTool()] === 'shape') cycleToolInGroup('shape');
      else selectTool('rect');
      return;
    }
    if (key === '2') {
      e.preventDefault();
      if (TOOL_GROUPS[annot.getTool()] === 'stroke') cycleToolInGroup('stroke');
      else selectTool('pencil');
      return;
    }
    if (key === '3') {
      e.preventDefault();
      if (TOOL_GROUPS[annot.getTool()] === 'text') cycleToolInGroup('text');
      else selectTool('text');
      return;
    }
    if (key === '4') {
      e.preventDefault();
      if (TOOL_GROUPS[annot.getTool()] === 'blur') cycleToolInGroup('blur');
      else selectTool('pixelate');
      return;
    }
    if (key === '5') {
      e.preventDefault();
      if (TOOL_GROUPS[annot.getTool()] === 'eraser') cycleToolInGroup('eraser');
      else selectTool('eraser');
      return;
    }
    if (key === 'z' || key === 'Z') {
      e.preventDefault();
      annot.undo();
      updateUndoRedoButtons();
      return;
    }
    if (key === 'r' || key === 'R') {
      e.preventDefault();
      annot.clearAll();
      updateUndoRedoButtons();
      redrawAnnotFull();
      return;
    }
  }
  // 0.20.6：方向键移动选区 1 bitmap px（标注模式下、选取工具、有选区、无标注拖拽时）
  if (ss.isAnnotating && ss.selCss && annot.getTool() === 'select' && !ss.isAnnotDragging && !ss.selectionInteraction) {
    // 标注对象键盘操作优先于选区；文本输入/IME 优先于截图快捷键
    const tgt = e.target;
    if (tgt && (tgt.tagName === 'INPUT' || tgt.tagName === 'TEXTAREA' || tgt.isContentEditable)) {
      // 文本输入中不拦截方向键
    } else if (!e.isComposing) {
      let dx = 0, dy = 0;
      if (e.key === 'ArrowLeft') { dx = -1; }
      else if (e.key === 'ArrowRight') { dx = 1; }
      else if (e.key === 'ArrowUp') { dy = -1; }
      else if (e.key === 'ArrowDown') { dy = 1; }
      if (dx !== 0 || dy !== 0) {
        e.preventDefault();
        const meta = window.__blinkScreenMeta || { vx: 0, vy: 0 };
        const newSelCss = moveSelection1px(ss.selCss, dx, dy, meta);
        if (newSelCss) {
          ss.selCss = newSelCss;
          // 重置标注 canvas 位置和裁剪区域
          const bmpRect = cssRectToBitmap(newSelCss, meta);
          const pw = Math.max(1, bmpRect.w);
          const ph = Math.max(1, bmpRect.h);
          ss.annotCanvas.style.left = newSelCss.x + 'px';
          ss.annotCanvas.style.top = newSelCss.y + 'px';
          // 重新裁剪底图（选区移动后裁剪区域改变）
          if (ss.screenshot) {
            try {
              const tempCanvas = document.createElement('canvas');
              tempCanvas.width = pw;
              tempCanvas.height = ph;
              const tempCtx = tempCanvas.getContext('2d');
              tempCtx.drawImage(ss.screenshot, bmpRect.x, bmpRect.y, pw, ph, 0, 0, pw, ph);
              const cropData = tempCtx.getImageData(0, 0, pw, ph);
              annot.updateCropData(cropData, pw, ph);
            } catch (err) {
              console.warn('[screenshot] 方向键移动选区后裁剪失败', err);
            }
          }
          ss.selectionRevision++;
          ss.ocrPrewarm = null;
          ss.ocrResultCache = null;
          if (typeof ss._invalidateSelectionContent === 'function') {
            ss._invalidateSelectionContent();
          }
          drawFinalSelection();
          // 刷新 OCR 预热
          triggerOcrPrewarm(pw, ph);
        }
        return;
      }
    }
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

// ── Alt 按键状态跟踪：显示/隐藏工具栏上的 kbd 快捷键提示 ──
// 按住 Alt 时 body 加 data-alt-down，CSS 据此显示 .kbd-hint
window.addEventListener('keydown', (e) => {
  if (e.key === 'Alt' && !e.repeat) document.body.dataset.altDown = 'true';
});
window.addEventListener('keyup', (e) => {
  if (e.key === 'Alt') delete document.body.dataset.altDown;
});
window.addEventListener('blur', () => { delete document.body.dataset.altDown; });

window.addEventListener('blur', () => {
  // 长截图采集会把滚轮交给底层窗口，overlay 失焦属于正常流程。
  // 必须在 blurGuard 和任何视觉清理之前返回，否则会刚进入 capturing 就被
  // 普通截图的"失焦即退出"策略错杀。
  if (isScrollCapturing()) {
    console.debug('[screenshot] window blur ignored during scroll capture', {
      phase: ss.scrollSession?.scrollCapturePhase,
      frameCount: ss.scrollSession?.scrollFrames?.length || 0,
      documentFocus: document.hasFocus(),
    });
    return;
  }
  // 0.20.4：图片编辑器模式下用更长的 blurGuard 防止主窗口关闭导致的
  // 焦点瞬态切换误关编辑器，但 2s 后仍允许 blur 自动关闭（用户点击其他窗口时）。
  if (document.body.classList.contains('image-editor-mode') && ss.editorSession.active && !ss.blurGuard) {
    ss.blurGuard = true;
    setTimeout(() => { ss.blurGuard = false; }, 2000);
    console.debug('[screenshot] window blur ignored (image editor mode, extended blurGuard)');
    return;
  }
  if (ss.blurGuard) return;
  ss.blurGuard = true;
  setTimeout(() => { ss.blurGuard = false; }, 500);

  if (hasActivePanel()) {
    console.debug('[screenshot] window blur ignored (active panel)');
    return;
  }

  // H1 优化：取消待执行的 drawSelection rAF
  cancelDrawSelectionRaf();
  cancelDrawFinalSelectionRaf();
  console.debug('[screenshot] window blur, hiding overlay', {
    phase: ss.scrollSession?.scrollCapturePhase,
    active: ss.scrollSession?.active,
    frameCount: ss.scrollSession?.scrollFrames?.length || 0,
    documentFocus: document.hasFocus(),
  });
  // 完成时清理画布，防止下次唤起残留旧画面
  cleanupCanvasVisuals();
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
