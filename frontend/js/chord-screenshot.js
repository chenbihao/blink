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
  ocrImage, frontendLog, invoke,
} from "./api.js";
import * as annot from "./annotation-engine.js";
import { ensureSpriteLoaded } from "./icon.js";
import { applyThemeFromConfig } from "./theme.js";

// ── 子模块 ──────────────────────────────────────────────
import { ss, initDOM, PREWARM_MIN_WIDTH, PREWARM_MIN_HEIGHT } from "./ss-state.js";
import { norm, pointInRect, applySquareConstraint } from "./ss-utils.js";
import { drawDimmed, drawSelection, drawFinalSelection, redrawAnnotPreview, redrawAnnotFull } from "./ss-draw.js";
import { positionToolbar } from "./ss-display.js";
import {
  getSelectionHandle, beginSelectionInteraction, updateSelectionInteraction,
  finishSelectionInteraction, updateSelectionCursor, refreshShapePreviewOnShift,
  updateStrokeCursor,
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
  compositeSelection, doCancel, hasActivePanel,
} from "./ss-output.js";
import { bindToolbar, showTextInput, updateUndoRedoButtons } from "./ss-toolbar.js";

// ── **临时**（0.11.7-f 调试用）：console 转发到后端 tracing ────────────
// TODO(0.11.7 收尾)：0.11.7 稳定后移除此块 + api.js 的 frontendLog + Rust 端 frontend_log command
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
  window.addEventListener('error', (e) => {
    frontendLog('error', `window.onerror: ${e.message} @ ${e.filename}:${e.lineno}:${e.colno}`);
  });
  window.addEventListener('unhandledrejection', (e) => {
    frontendLog('error', `unhandledrejection: ${e.reason && e.reason.stack ? e.reason.stack : e.reason}`);
  });
}

// ════════════════════════════════════════════════════════════
//  初始化
// ════════════════════════════════════════════════════════════

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

// 图标 sprite
ensureSpriteLoaded();
applyThemeFromConfig();

annot.init(ss.annotCanvas);
annot.setTool('select');
{
  const _hc = document.getElementById('ocr-hit-canvas');
  if (_hc) _hc.setAttribute('data-tool', 'select');
}

// 预热窗口跳过 loadScreenshot
const isPreheat = new URLSearchParams(window.location.search).get('preheat') === '1';
if (!isPreheat) {
  loadScreenshot();
}

window.__blinkReloadScreenshot = function () {
  resetState();
  loadScreenshot();
};

// ════════════════════════════════════════════════════════════
//  选区生命周期
// ════════════════════════════════════════════════════════════

/** 完全重置前端状态——每次 overlay 显示时都要走一遍 */
function resetState() {
  const { canvas, ctx, annotCanvas, annotCtx, sizeHint, toolbar, errorHint } = ss;
  ss.isDragging = false;
  ss.isAnnotDragging = false;
  ss.isAnnotating = false;
  ss.sent = false;
  ss.ocrBusy = false;
  ss.translationBusy = false;
  ss.selCss = null;
  ss.selectionInteraction = null;
  ss.selectionRevision++;
  ss.translationRevision++;
  canvas.style.cursor = 'crosshair';
  canvas.setAttribute('data-tool', 'select');
  ss.screenshot = null;
  if (ss.singleClickTimeout) { clearTimeout(ss.singleClickTimeout); ss.singleClickTimeout = null; }
  sizeHint.style.display = 'none';
  toolbar.style.display = 'none';
  annotCanvas.style.display = 'none';
  errorHint.style.display = 'none';
  errorHint.textContent = '';
  if (canvas.width > 0) {
    ctx.clearRect(0, 0, canvas.width, canvas.height);
  }
  if (annotCanvas.width > 0) {
    annotCtx.clearRect(0, 0, annotCanvas.width, annotCanvas.height);
  }
  annotCanvas.width = 0;
  annotCanvas.height = 0;
  exitReadingMode();
  screenshotSetAnnotationMode(false).catch((e) => console.error('setAnnotationMode(false) 失败', e));
  const oldOcr = document.getElementById('ocr-panel');
  if (oldOcr) oldOcr.remove();
  // 水印表单已内嵌进 text-dropdown（视图切回列表即可）
  const wmDropdown = document.getElementById('text-dropdown');
  if (wmDropdown) {
    wmDropdown.setAttribute('data-view', 'list');
    wmDropdown.setAttribute('data-open', 'false');
  }
  // 清 OCR 预热 + 缓存
  ss.ocrPrewarm = null;
  ss.ocrResultCache = null;
  ss.ocrBusy = false;
  ss.translationBusy = false;
  updateOutputButtonsDisabled();
  annot.clearOverlay();
  updateOverlayButtonsActive();
  // 清工具栏用户拖动位置
  toolbar.removeAttribute('data-user-moved');
  toolbar.style.left = '';
  toolbar.style.top = '';
}

function loadScreenshot() {
  ss.errorHint.style.display = 'none';

  // 配置读取与图像加载并行；失败时保留默认值。
  invoke('get_config_section', { key: 'screenshot:config' })
    .then((val) => {
      if (val && typeof val === 'object') {
        ss.screenshotConfig.prewarmOcr = val.prewarmOcr !== false;
      }
    })
    .catch((e) => console.debug('[screenshot] 读 screenshot:config 失败,用默认值', e));

  const img = new Image();
  // Tauri v2 在 Windows 上把自定义协议映射为 localhost URL；必须声明 CORS，
  // 否则后续 getImageData/toBlob 会因 canvas 污染而失败。
  img.crossOrigin = 'anonymous';
  img.onload = () => {
    const { canvas } = ss;
    ss.screenshot = img;
    canvas.width = img.width;
    canvas.height = img.height;
    drawDimmed();
    console.debug('[screenshot] screenshot loaded', { w: img.width, h: img.height });
  };
  img.onerror = (e) => {
    console.error('[screenshot] Image load failed', e);
    ss.errorHint.textContent = '截图加载失败，按 ESC 关闭';
    ss.errorHint.style.display = 'block';
  };
  img.src = 'http://blink-screenshot.localhost/capture?t=' + Date.now();
}

/** 进入标注模式：显示工具栏 + 定位标注 canvas + 通知后端 */
function enterAnnotationMode(rect) {
  console.debug('[screenshot] enterAnnotationMode', rect);
  const { annotCanvas, screenshot } = ss;

  ss.selCss = rect;
  ss.isAnnotating = true;
  ss.sent = false;

  const dpr = window.devicePixelRatio || 1;
  annotCanvas.style.display = 'block';
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
  screenshotSetAnnotationMode(true).catch((e) => console.error('setAnnotationMode(true) 失败', e));
  drawFinalSelection();
  positionToolbar(rect);
  triggerOcrPrewarm(pw, ph);
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
        .catch((err) => {
          console.warn('[screenshot] OCR 预热失败(用户点识别时会重试)', err);
          resolve(null);
        });
    });
  });
}

/** 退出标注模式（清除选区，回到可拖选状态） */
function exitAnnotationMode() {
  console.debug('[screenshot] exitAnnotationMode');
  const { canvas, annotCanvas, toolbar, sizeHint } = ss;
  ss.isAnnotating = false;
  ss.selCss = null;
  ss.selectionInteraction = null;
  ss.selectionRevision++;
  ss.translationRevision++;
  canvas.style.cursor = 'crosshair';
  annotCanvas.style.display = 'none';
  annotCanvas.width = 0;
  annotCanvas.height = 0;
  toolbar.style.display = 'none';
  sizeHint.style.display = 'none';
  ss.ocrPrewarm = null;
  ss.ocrBusy = false;
  ss.translationBusy = false;
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

// ════════════════════════════════════════════════════════════
//  画布事件绑定
// ════════════════════════════════════════════════════════════

const { canvas } = ss;

canvas.addEventListener('mousedown', (e) => {
  if (!ss.screenshot || e.button !== 0) return;

  const tool = annot.getTool();
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

  if (ss.isAnnotating && ss.selCss && pointInRect(e.offsetX, e.offsetY, ss.selCss)) {
    if (tool === 'watermark') return;
    const dpr = window.devicePixelRatio || 1;
    ss.annotStartX = (e.offsetX - ss.selCss.x) * dpr;
    ss.annotStartY = (e.offsetY - ss.selCss.y) * dpr;
    ss.annotCurrentX = ss.annotStartX;
    ss.annotCurrentY = ss.annotStartY;
    annot.startDraw(ss.annotStartX, ss.annotStartY);
    ss.isAnnotDragging = true;
    return;
  }

  if (ss.isAnnotating && ss.selCss) {
    console.debug('[screenshot] annotation tool click outside selection → no-op');
    return;
  }

  ss.isDragging = true;
  ss.sent = false;
  ss.startX = e.offsetX;
  ss.startY = e.offsetY;
  ss.endX = ss.startX;
  ss.endY = ss.startY;
});

canvas.addEventListener('mousemove', (e) => {
  if (!ss.screenshot) return;

  updateSelectionCursor(e.offsetX, e.offsetY);

  if (ss.selectionInteraction) {
    updateSelectionInteraction(e);
    return;
  }

  updateStrokeCursor(e.clientX, e.clientY);

  if (ss.isAnnotDragging && ss.selCss) {
    const dpr = window.devicePixelRatio || 1;
    ss.annotCurrentX = (e.offsetX - ss.selCss.x) * dpr;
    ss.annotCurrentY = (e.offsetY - ss.selCss.y) * dpr;
    if (e.shiftKey) {
      const constrained = applySquareConstraint(
        ss.annotStartX, ss.annotStartY, ss.annotCurrentX, ss.annotCurrentY, annot.getTool()
      );
      if (constrained) { ss.annotCurrentX = constrained.x; ss.annotCurrentY = constrained.y; }
    }
    annot.moveDraw(ss.annotCurrentX, ss.annotCurrentY);
    redrawAnnotPreview();
    return;
  }

  if (ss.isDragging) {
    ss.endX = e.offsetX;
    ss.endY = e.offsetY;
    drawSelection();
  }
});

canvas.addEventListener('mouseleave', () => {
  if (ss.strokeCursor) ss.strokeCursor.style.display = 'none';
  if (!ss.selectionInteraction) ss.canvas.style.cursor = annot.getTool() === 'select' ? 'default' : 'crosshair';
});

canvas.addEventListener('mouseup', (e) => {
  if (!ss.screenshot) return;

  if (finishSelectionInteraction(e)) return;

  if (ss.isAnnotDragging) {
    ss.isAnnotDragging = false;
    const dpr = window.devicePixelRatio || 1;
    ss.annotCurrentX = (e.offsetX - ss.selCss.x) * dpr;
    ss.annotCurrentY = (e.offsetY - ss.selCss.y) * dpr;
    if (e.shiftKey) {
      const constrained = applySquareConstraint(
        ss.annotStartX, ss.annotStartY, ss.annotCurrentX, ss.annotCurrentY, annot.getTool()
      );
      if (constrained) { ss.annotCurrentX = constrained.x; ss.annotCurrentY = constrained.y; }
    }

    const tool = annot.getTool();
    const dx = ss.annotCurrentX - ss.annotStartX;
    const dy = ss.annotCurrentY - ss.annotStartY;
    const minDrag = (tool === 'text' || tool === 'eraser' || tool === 'pencil' || tool === 'mosaic') ? 0 : 3;
    if (Math.abs(dx) < minDrag && Math.abs(dy) < minDrag) {
      console.debug('[screenshot] annotation drag too small, skip', { tool, dx, dy });
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
  enterAnnotationMode(rect);
});

canvas.addEventListener('dblclick', (e) => {
  console.debug('[screenshot] dblclick', { isAnnotating: ss.isAnnotating, hasSelCss: !!ss.selCss, sent: ss.sent });
  if (!ss.screenshot || ss.sent) return;
  if (ss.singleClickTimeout) { clearTimeout(ss.singleClickTimeout); ss.singleClickTimeout = null; }

  if (ss.isAnnotating && ss.selCss) {
    if (pointInRect(e.offsetX, e.offsetY, ss.selCss)) {
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
  if (e.key === 'Escape') {
    e.preventDefault();
    const ocrPanel = document.getElementById('ocr-panel');
    if (ocrPanel) {
      ocrPanel.remove();
      return;
    }
    const wmDropdown = document.getElementById('text-dropdown');
    if (wmDropdown && wmDropdown.getAttribute('data-view') === 'watermark' && wmDropdown.getAttribute('data-open') === 'true') {
      wmDropdown.setAttribute('data-view', 'list');
      wmDropdown.setAttribute('data-open', 'false');
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

window.addEventListener('blur', () => {
  if (ss.blurGuard) return;
  ss.blurGuard = true;
  setTimeout(() => { ss.blurGuard = false; }, 500);

  if (hasActivePanel()) {
    console.debug('[screenshot] window blur ignored (active panel)');
    return;
  }

  console.debug('[screenshot] window blur, hiding overlay');
  hideScreenshotOverlay().catch((e) => console.error('hideScreenshotOverlay 失败', e));
});

// ════════════════════════════════════════════════════════════
//  工具栏绑定
// ════════════════════════════════════════════════════════════

bindToolbar();
