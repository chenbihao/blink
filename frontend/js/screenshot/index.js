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
//! - 画布事件绑定（pointerdown / pointermove / pointerup / pointercancel / dblclick / contextmenu / keydown / blur）
//!
//! 拆分模块：
//! - ss-state.js    — 共享状态 + 常量 + initDOM
//! - ss-utils.js     — 纯工具函数（norm / computeDragRect / pointInRect / applySquareConstraint）
//! - ss-draw.js      — 绘制函数（drawDimmed / drawFinalSelection / redrawAnnot*）
//! - ss-live-selection.js — 0.23.15：拖动期间的实时选区 DOM 层（单一调度入口）
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
//! - 指针事件 offsetX/Y = CSS 像素
//! - 选区 selCss 存 CSS 像素；annot-canvas 内部像素 = 物理像素
//! - 标注坐标使用物理像素相对裁剪区
//! - 0.23.15：拖动期间只有实时 DOM 层逐帧变化，interaction-canvas 零绘制

import {copyToClipboard, hideScreenshotOverlay, invoke, ocrImage, screenshotSetAnnotationMode,} from "../shared/api.js";
import {normalizeError} from "../shared/tauri.js";
import * as annot from "./annotation-engine.js";
import {ensureSpriteLoaded} from "../shared/icon.js";
import {applyThemeFromConfig} from "../shared/theme.js";

// ── 子模块 ──────────────────────────────────────────────
import {initDOM, PREWARM_MIN_HEIGHT, PREWARM_MIN_WIDTH, ss, TOOL_CAPS} from "./ss-state.js";
import {IMAGE_SOURCE} from './image-editor-session.js';
import {
    applySquareConstraint,
    computeCanvasEditorInitialPosition,
    computeDragRect,
    computePanAxisBounds,
    pointInRect
} from "./ss-utils.js";
import {
    cssRectToBitmap,
    getRenderScale,
    pointNearWindowEdge,
    shouldStartFreeSelection,
    syncRenderScale
} from "./ss-selection-geometry.js";
import {
    drawDimmed,
    drawFinalSelection,
    redrawAnnotFull,
    redrawAnnotPreview,
    syncInteractionCanvasSize
} from "./ss-draw.js";
// 0.23.15：拖动期间的实时选区预览（单一调度入口）
import {resetLiveSelection, updateLiveSelection} from "./ss-live-selection.js";
// 0.23.15：Pointer Events 的 capture / 采样 / 中断判定（纯逻辑，可单测）
import {capturePointer, hasActiveDragInteraction, pointerPoint, releasePointer} from "./ss-pointer.js";
import {invalidateDisplaysCache, positionToolbar} from "./ss-display.js";
import {
    beginSelectionInteraction,
    cycleMagnifierFormat,
    finishSelectionInteraction,
    getMagnifierColorText,
    getSelectionHandle,
    hidePixelMagnifier,
    moveSelection1px,
    refreshShapePreviewOnShift,
    updatePixelMagnifier,
    updateSelectionCursor,
    updateSelectionInteraction,
    updateStrokeCursor,
} from "./ss-interaction.js";
import {copyReadingSelection, exitReadingMode, getReadingSelectionText, showReadingContextMenu,} from "./ss-reading.js";
import {
    activateSilentOcrReading,
    cancelActiveOcr,
    doPanelToggle,
    hideSelLoading,
    showOcrResult,
    showTransientHint,
    updateOutputButtonsDisabled,
    updateOverlayButtonsActive,
} from "./ss-ocr.js";
import {updateOcrButtonBusy} from "./ss-ocr-busy.js";
import {
    cleanupCanvasVisuals,
    compositeSelection,
    doCancel,
    doCopyFullScreen,
    doCopySelection,
    doPinSelection,
    hasActivePanel,
    outputEditorPng,
} from "./ss-output.js";
import {
    bindToolbar,
    cycleToolInGroup,
    resetToolbarDropdowns,
    selectTool,
    showTextInput,
    TOOL_GROUPS,
    updateUndoRedoButtons
} from "./ss-toolbar.js";
import {resetPaletteState} from "./ss-palette.js";
// 0.15.8：智能窗口吸附 + 像素放大镜
import {
    clearHover,
    clearPickableWindows,
    getHoveredWindowRect,
    hideWindowHintIfVisible,
    loadPickableWindows,
    showWindowHintIfPending,
    updateWindowHover
} from "./ss-hover.js";
// 0.18.2：控件级智能吸附（跨屏预选版）
import {
    clearControlHints,
    clearControlHover,
    getHoveredControlRect,
    prefetchControlHints,
    setControlTarget,
    updateControlHover,
} from "./ss-control-hints.js";
// 0.15.7：长截图
import {
    bindScrollToolbar,
    exitScrollCapture,
    isScrollCaptureActive,
    isScrollCapturing,
    onScrollWheel,
    resetScrollCaptureSession,
} from "./scroll/index.js";
import {refreshDiagnosticsVisibility} from "./scroll/diagnostics.js";
import {refreshOcrDiagnosticsVisibility} from "./ss-ocr-diagnostics.js";

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
    console.debug('[screenshot] maybeStartCaptureHints fired', {screenshotReady, renderScaleReady, configReady});
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
// 0.23.12：hide 前全量清场（cleanupCanvasVisuals 经此回调调用，避免循环依赖）
ss._cleanupSessionVisuals = cleanupSessionVisuals;
// 0.23.11.2：划词层真空白双击 → 复制选区（hit-canvas 与主 canvas 是兄弟元素，
// dblclick 冒泡不可达，经此回调转发；语义对齐主 canvas dblclick 的标注分支）
ss._dblClickBlankCopy = function (e) {
    if (ss.isAnnotating && ss.selCss && pointInEditableImage(e)) {
        doCopySelection();
    }
};
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
    const {canvas} = ss;
    const meta = window.__blinkScreenMeta;
    if (!canvas || !meta) return;
    if (syncRenderScale(canvas, meta)) {
        // M7 优化：renderScale 变化后失效 displays 缓存
        invalidateDisplaysCache();
        console.debug('[screenshot] resize: renderScale re-synced', {
            scaleX: meta.renderScaleX,
            scaleY: meta.renderScaleY
        });
    }
});

const SCREENSHOT_RAW_TIMEOUT_MS = 1500;
const SCREENSHOT_RAW_MAX_ATTEMPTS = 2;
let activeScreenshotFetchController = null;
let screenshotFetchEpoch = 0;

function cancelActiveScreenshotFetch() {
    screenshotFetchEpoch++;
    if (activeScreenshotFetchController) {
        activeScreenshotFetchController.abort();
        activeScreenshotFetchController = null;
    }
}

/**
 * 拉取活动显示器 raw BGRA。每次请求有硬超时并允许一次重试，防止 WebView2
 * 自定义协议偶发不返回时 loadScreenshot 永久 await、遮罩永久不可拖选。
 */
async function fetchScreenshotRaw(monitor, reason) {
    const epoch = screenshotFetchEpoch;
    let lastError = null;
    for (let attempt = 1; attempt <= SCREENSHOT_RAW_MAX_ATTEMPTS; attempt++) {
        if (epoch !== screenshotFetchEpoch) {
            throw new Error('stale raw screenshot fetch');
        }
        const controller = new AbortController();
        activeScreenshotFetchController = controller;
        const startedAt = performance.now();
        let timeoutId = 0;
        const timeout = new Promise((_, reject) => {
            timeoutId = setTimeout(() => {
                controller.abort();
                const error = new Error(`raw screenshot fetch timed out after ${SCREENSHOT_RAW_TIMEOUT_MS}ms`);
                error.name = 'TimeoutError';
                reject(error);
            }, SCREENSHOT_RAW_TIMEOUT_MS);
        });
        try {
            const response = await Promise.race([
                fetch(
                    `http://blink-screenshot.localhost/raw?monitor=${monitor}&t=${Date.now()}`,
                    {signal: controller.signal},
                ),
                timeout,
            ]);
            if (!response.ok) throw new Error(`fetch failed: ${response.status}`);
            const buffer = await Promise.race([
                response.arrayBuffer(),
                timeout,
            ]);
            console.debug('[screenshot] raw bgra fetched', {
                reason,
                attempt,
                monitor,
                ms: Math.round(performance.now() - startedAt),
                bytes: buffer.byteLength,
            });
            return buffer;
        } catch (error) {
            if (epoch !== screenshotFetchEpoch) {
                throw error;
            }
            lastError = error;
            console.warn('[screenshot] raw bgra fetch failed', {
                reason,
                attempt,
                monitor,
                timeout: error?.name === 'AbortError' || error?.name === 'TimeoutError',
                error,
            });
        } finally {
            clearTimeout(timeoutId);
            if (activeScreenshotFetchController === controller) {
                activeScreenshotFetchController = null;
            }
        }
    }
    throw lastError || new Error('raw screenshot fetch failed');
}

// P0 优化：clearVisual 不再 clearRect 暗罩（保留 resetState 画的 P5 暗罩），
// 并立即启动有界 fetch 预取——与 double rAF 并行。
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
    window.__blinkScreenshotPreload = fetchScreenshotRaw(_activeMonitor, 'preload')
        .then(buf => {
            console.debug('[screenshot] preload fetch done', {
                ms: Math.round(performance.now() - _tPreload),
                bytes: buf.byteLength,
                monitor: _activeMonitor
            });
            return buf;
        })
        .catch(e => {
            console.error('[screenshot] preload fetch error', e);
            return null;
        });
};

/**
 * 后端复用 overlay 时的单一会话入口。
 * meta、active display、reset 与 reload 在同一次 eval 中提交，避免快速 hide→show
 * 时多个 fire-and-forget eval 只执行了一部分，留下“只有暗罩、无法拖选”的半会话。
 */
window.__blinkStartScreenshotSession = function (meta, activeDisplay) {
    window.__blinkScreenMeta = meta;
    window.__blinkActiveDisplay = activeDisplay ?? meta?.activeDisplay ?? 0;
    window.__blinkClearScreenshotVisual();
    // resetState 已同步清理上一轮并画好暗罩，此时才允许 WebView 可见。
    document.documentElement.classList.remove('screenshot-session-inactive');
    requestAnimationFrame(() => {
        requestAnimationFrame(() => window.__blinkReloadScreenshot());
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
        ss.errorHint.classList.add('ss-toast-error');
    }
};

window.__blinkOpenImageEditor = function () {
    console.info('[image-editor] __blinkOpenImageEditor called');
    try {
        resetState();
        document.documentElement.classList.remove('screenshot-session-inactive');
        loadEditorImage(window.__blinkEditorSource?.kind || IMAGE_SOURCE.CLIPBOARD);
    } catch (e) {
        console.error('[image-editor] 初始化失败', e);
        ss.errorHint.textContent = '图片编辑初始化失败，按 ESC 关闭';
        ss.errorHint.classList.remove('hidden');
        ss.errorHint.classList.add('ss-toast-error');
    }
};

// ════════════════════════════════════════════════════════════
//  选区生命周期
// ════════════════════════════════════════════════════════════

/**
 * 0.23.12：hide 前全量清场——清理 cleanupCanvasVisuals 画布/工具栏之外的
 * 全部可见残留：划词 hit-canvas、窗口/控件预选虚线框、interaction 层、
 * OCR 面板、像素放大镜、sel-loading、precision hint、toast、文本输入框。
 *
 * 0.23.15：同时把**拖选状态**归零（isDragging / selectionInteraction /
 * pendingSnap）——它们是"迟到的 pointercancel / lostpointercapture"会不会被
 * 判成一次有效中断的依据，收尾后必须失效，详见 abortSelectionInteraction。
 *
 * 动机：overlay 窗口复用（cloak hide → 下次 show 先于 resetState eval），
 * show 与 reset 之间上一轮残留图层会闪现；0.23.11 预热静默划词让上一轮
 * 更常处于"划词已激活"退出，词框/全选高亮的闪现由此变得明显。
 * 幂等，可重复调用；由 ss-output.cleanupCanvasVisuals 经 ss._cleanupSessionVisuals 调用。
 */
function cleanupSessionVisuals() {
    try {
        exitReadingMode();
    } catch (e) {
        console.warn('[screenshot] cleanup: exitReadingMode failed', e);
    }
    try {
        const panel = document.getElementById('ocr-panel');
        if (panel) panel.remove();
        updateOverlayButtonsActive();
    } catch (e) {
        console.warn('[screenshot] cleanup: remove ocr-panel failed', e);
    }
    try {
        clearHover();
        clearControlHints();
    } catch (e) {
        console.warn('[screenshot] cleanup: clear hints failed', e);
    }
    if (ss.interactionCanvas && ss.interactionCtx && ss.interactionCanvas.width > 0) {
        ss.interactionCtx.clearRect(0, 0, ss.interactionCanvas.width, ss.interactionCanvas.height);
    }
    // 0.23.15：实时选区层同样属于"上一轮残留"，必须复位（含在途 rAF）
    resetLiveSelection();
    // 0.23.15：拖选状态也必须在清场时归零。否则 ESC / 失焦 / 输出收尾之后，若浏览器
    // 因窗口隐藏回收指针并补发 pointercancel / lostpointercapture，
    // hasActiveDragInteraction 仍为 true，abortSelectionInteraction 会在一个已经 cancel
    // 的会话上重建标注模式（重新裁图 + screenshotSetAnnotationMode(true) + OCR prewarm）。
    ss.isDragging = false;
    ss.selectionInteraction = null;
    ss.pendingSnap = null;
    try {
        hidePixelMagnifier();
    } catch (e) {
        console.warn('[screenshot] cleanup: hide magnifier failed', e);
    }
    // W4 例外：strokeCursor 高频逐帧更新，直接写 display
    if (ss.strokeCursor) ss.strokeCursor.style.display = 'none';
    try {
        hideSelLoading();
    } catch (e) {
        console.warn('[screenshot] cleanup: hideSelLoading failed', e);
    }
    if (ss.precisionHint) ss.precisionHint.classList.add('hidden');
    ss.errorHint.classList.add('hidden');
    ss.errorHint.classList.remove('ss-toast-error');
    ss.errorHint.textContent = '';
    const staleTextInput = document.querySelector('.text-annot-input');
    if (staleTextInput) staleTextInput.remove();
}

/** 完全重置前端状态——每次 overlay 显示时都要走一遍 */
function resetState() {
    const _t0 = performance.now();
    console.debug('[screenshot] resetState start');
    cancelActiveScreenshotFetch();
    // Task 6: 取消在途 OCR 请求
    cancelActiveOcr();
    resetScrollCaptureSession();
    cancelSelectionNudge();
    const {canvas, ctx, annotCanvas, annotCtx, sizeHint, toolbar, errorHint} = ss;
    ss._loadGen++;  // BUG1 fix: 使待处理的旧 img.onload 回调失效
    // 0.15.9：取消待执行的标注预览 rAF
    if (ss._annotRaf) {
        cancelAnimationFrame(ss._annotRaf);
        ss._annotRaf = 0;
    }
    // 0.23.15：取消待执行的实时预览 rAF 并复位实时选区层（含几何清零）。
    // 会话切换后旧 rAF 不得重新显示实时层。
    resetLiveSelection();
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
    // 0.20.x：恢复 pin 来源编辑时隐藏的「钉图」按钮
    const pinButton = document.getElementById('btn-pin');
    if (pinButton) pinButton.hidden = false;
    if (ss.singleClickTimeout) {
        clearTimeout(ss.singleClickTimeout);
        ss.singleClickTimeout = null;
    }
    sizeHint.classList.add('hidden');
    toolbar.classList.add('hidden');
    annotCanvas.classList.add('hidden');
    errorHint.classList.add('hidden');
    ss.errorHint.classList.remove('ss-toast-error');
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
    try {
        exitReadingMode();
    } catch (e) {
        console.warn('[screenshot] resetState: exitReadingMode failed', e);
    }
    try {
        screenshotSetAnnotationMode(false).catch((e) => console.error('[screenshot] setAnnotationMode(false) 失败', e));
    } catch (e) {
        console.warn('[screenshot] resetState: screenshotSetAnnotationMode threw', e);
    }
    try {
        const oldOcr = document.getElementById('ocr-panel');
        if (oldOcr) oldOcr.remove();
    } catch (e) {
        console.warn('[screenshot] resetState: remove ocr-panel failed', e);
    }
    try {
        const wmDropdown = document.getElementById('text-dropdown');
        if (wmDropdown) {
            wmDropdown.setAttribute('data-view', 'list');
            wmDropdown.setAttribute('data-open', 'false');
        }
    } catch (e) {
        console.warn('[screenshot] resetState: text-dropdown reset failed', e);
    }
    // P0 优化：清理预取 promise，防止上一轮截图的预取残留到新一轮
    window.__blinkScreenshotPreload = null;
    ss.ocrPrewarm = null;
    ss.ocrResultCache = null;
    ss.ocrBusy = false;
    ss.translationBusy = false;
    try {
        updateOutputButtonsDisabled();
    } catch (e) {
        console.warn('[screenshot] resetState: updateOutputButtonsDisabled failed', e);
    }
    try {
        annot.clearOverlay();
    } catch (e) {
        console.warn('[screenshot] resetState: annot.clearOverlay failed', e);
    }
    try {
        updateOverlayButtonsActive();
    } catch (e) {
        console.warn('[screenshot] resetState: updateOverlayButtonsActive failed', e);
    }
    try {
        toolbar.removeAttribute('data-user-moved');
        toolbar.style.left = '';
        toolbar.style.top = '';
    } catch (e) {
        console.warn('[screenshot] resetState: toolbar reset failed', e);
    }
    try {
        clearPickableWindows();
    } catch (e) {
        console.warn('[screenshot] resetState: clearPickableWindows failed', e);
    }
    // 0.18.2：清除控件提示列表
    try {
        clearControlHints();
    } catch (e) {
        console.warn('[screenshot] resetState: clearControlHints failed', e);
    }
    // 0.18.x：重置控件预选门控
    captureHintsStarted = false;
    screenshotReady = false;
    renderScaleReady = false;
    configReady = false;
    if (ss.magnifierRaf) {
        cancelAnimationFrame(ss.magnifierRaf);
        ss.magnifierRaf = 0;
    }
    ss._magnifierSampleGen = (ss._magnifierSampleGen || 0) + 1;
    ss._pendingMagnifierPos = null;
    // 0.20.6：重置取色器状态
    ss.colorPickerMode = 'idle';
    ss._pickerBitmapPos = null;
    if (ss.precisionHint) ss.precisionHint.classList.add('hidden');
    try {
        resetToolbarDropdowns();
    } catch (e) {
        console.warn('[screenshot] resetState: dropdown reset failed', e);
    }
    try {
        resetPaletteState();
    } catch (e) {
        console.warn('[screenshot] resetState: palette reset failed', e);
    }
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
    } catch (e) {
        console.warn('[screenshot] resetState: pointer-events restore failed', e);
    }
    // P5: 立即画暗罩——不等截图加载完成，消除"锁死感"
    // 用户看到暗色背景 + 十字光标，可以立即移动鼠标
    // 截图加载后 drawDimmed() 会叠加截图图像
    if (canvas.width > 0 && ctx) {
        ctx.fillStyle = 'rgba(0, 0, 0, 0.45)';
        ctx.fillRect(0, 0, canvas.width, canvas.height);
    }
    console.debug('[screenshot] resetState done', {ms: Math.round(performance.now() - _t0)});
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
    ss.errorHint.classList.remove('ss-toast-error');

    // 配置读取与图像加载并行；失败时保留默认值。
    loadEditorConfig(true);

    // 加载代际守卫
    const gen = ++ss._loadGen;

    // 0.15.9：加载超时检测——5 秒未完成则提示错误（防止协议请求静默失败）
    const timeoutId = setTimeout(() => {
        if (gen !== ss._loadGen) return;
        if (ss.screenshot) return;
        console.error('[screenshot] 加载超时（5s），协议请求可能失败', {gen});
        ss.errorHint.textContent = '截图加载超时，按 ESC 重试';
        ss.errorHint.classList.remove('hidden');
        ss.errorHint.classList.add('ss-toast-error');
    }, 5000);

    try {
        // P0 优化：复用 clearVisual 阶段启动的预取 fetch，避免重复请求。
        // 预取在 show 之前就开始了，这里可能已经完成（0ms 等待）。
        const preloadPromise = window.__blinkScreenshotPreload;
        let rawBuffer;
        if (preloadPromise) {
            console.debug('[screenshot] reusing preload fetch', {gen});
            rawBuffer = await preloadPromise;
            window.__blinkScreenshotPreload = null;
            if (gen !== ss._loadGen) {
                console.debug('[screenshot] 丢弃过期截图加载回调', {gen, cur: ss._loadGen});
                return;
            }
            if (!rawBuffer) throw new Error('preload fetch 返回 null');
        } else {
            console.debug('[screenshot] requesting raw bgra (no preload)', {gen});
            const _tFetchStart = performance.now();
            const _activeMonitor = window.__blinkActiveDisplay ?? 0;
            rawBuffer = await fetchScreenshotRaw(_activeMonitor, 'direct');
            console.debug('[screenshot] raw bgra fetched', {
                gen,
                ms: Math.round(performance.now() - _tFetchStart),
                bytes: rawBuffer.byteLength
            });
            if (gen !== ss._loadGen) {
                console.debug('[screenshot] 丢弃过期截图加载回调', {gen, cur: ss._loadGen});
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
        const {canvas} = ss;
        canvas.width = w;
        canvas.height = h;

        ss.screenshotOffscreen = document.createElement('canvas');
        ss.screenshotOffscreen.width = w;
        ss.screenshotOffscreen.height = h;
        const offCtx = ss.screenshotOffscreen.getContext('2d', {willReadFrequently: true});

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
        console.debug('[screenshot] putImageData done', {
            w: activeData.width,
            h: activeData.height,
            ox: activeData.offsetX,
            oy: activeData.offsetY,
            ms: Math.round(_tPutEnd - _tPutStart)
        });

        // 等布局稳定后同步 renderScale，再绘制和加载窗口/控件
        requestAnimationFrame(() => {
            if (gen !== ss._loadGen) return;
            const _tRafStart = performance.now();
            const meta = window.__blinkScreenMeta || {vx: 0, vy: 0};
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
            console.debug('[screenshot] rAF render done', {
                ms: Math.round(_tRafEnd - _tRafStart),
                totalMs: Math.round(performance.now() - _t0)
            });
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
                    console.debug('[screenshot] lazy monitor loaded', {
                        monitor: i,
                        ms: Math.round(performance.now() - _tLazy)
                    });
                    // 仅在暗色蒙版态（未拖选/未标注）时刷新主 canvas，
                    // 拖选中/标注中不调 drawDimmed（下次 mousemove/redraw 会自然读到新数据）
                    if (!ss.isDragging && !ss.isAnnotating && !ss.selectionInteraction) {
                        drawDimmed();
                    }
                })
                .catch(e => console.warn('[screenshot] lazy monitor load failed', e));
        }

        console.debug('[screenshot] screenshot loaded', {
            w,
            h,
            gen,
            activeMonitor: _activeIdx,
            totalMonitors: _displays.length,
            ms: Math.round(performance.now() - _t0)
        });
    } catch (e) {
        clearTimeout(timeoutId);
        if (gen !== ss._loadGen) return;
        console.error('[screenshot] loadScreenshot failed', e, {gen});
        ss.errorHint.textContent = '截图加载失败，按 ESC 重试';
        ss.errorHint.classList.remove('hidden');
        ss.errorHint.classList.add('ss-toast-error');
    }
}

function loadEditorConfig(includeCaptureHints) {
    const _t0 = performance.now();
    invoke('get_config_section', {key: 'screenshot:config'})
        .then((val) => {
            console.debug('[screenshot] loadEditorConfig done', {
                ms: Math.round(performance.now() - _t0),
                hasVal: !!val
            });
            if (val && typeof val === 'object') {
                ss.screenshotConfig.prewarmOcr = val.prewarmOcr !== false;
                ss.screenshotConfig.scrollDebug = val.scrollDebug === true;
                ss.screenshotConfig.ocrDebug = val.ocrDebug === true;
                ss.screenshotConfig.controlSnap = val.controlSnap === true;
                ss.screenshotConfig.controlSnapDepth = val.controlSnapDepth ?? 15;
                ss.screenshotConfig.controlSnapDeadlineMs = val.controlSnapDeadlineMs ?? 1000;
                ss.screenshotConfig.controlSnapMinSize = val.controlSnapMinSize ?? 50;
                ss.screenshotConfig.windowEdgeSnap = val.windowEdgeSnap ?? 10;
                refreshDiagnosticsVisibility();
                refreshOcrDiagnosticsVisibility();
                // 0.18.x：配置加载完成，触发统一门控
                configReady = true;
                maybeStartCaptureHints();
            } else {
                // 配置分片未写入（全新安装/从未保存过截图配置）：沿用 ss-state 默认值，
                // 同样要放行门控，否则窗口列表 / 控件吸附永远不会加载
                configReady = true;
                maybeStartCaptureHints();
            }
        })
        .catch((e) => {
            console.warn('[image-editor] 读 screenshot:config 失败,用默认值', e);
            console.debug('[screenshot] loadEditorConfig failed', {ms: Math.round(performance.now() - _t0)});
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
    ss.errorHint.classList.remove('ss-toast-error');
    const gen = ++ss._loadGen;
    const img = new Image();
    img.crossOrigin = 'anonymous';
    const timeoutId = setTimeout(() => {
        if (gen !== ss._loadGen || ss.editorSession.active) return;
        ss.errorHint.textContent = '图片加载超时，按 ESC 关闭';
        ss.errorHint.classList.remove('hidden');
        ss.errorHint.classList.add('ss-toast-error');
    }, 5000);
    img.onload = () => {
        clearTimeout(timeoutId);
        if (gen !== ss._loadGen) return;
        try {
            const baseCanvas = document.createElement('canvas');
            baseCanvas.width = img.width;
            baseCanvas.height = img.height;
            const baseCtx = baseCanvas.getContext('2d', {willReadFrequently: true});
            baseCtx.drawImage(img, 0, 0);
            const imageData = baseCtx.getImageData(0, 0, img.width, img.height);
            enterCanvasImageEditor(imageData, img.width, img.height, source);
            triggerOcrPrewarm(img.width, img.height);
            console.info('[image-editor] image loaded', {source, w: img.width, h: img.height, gen});
        } catch (e) {
            console.error('[image-editor] image onload 处理失败', e);
            ss.errorHint.textContent = '图片渲染失败，按 ESC 关闭';
            ss.errorHint.classList.remove('hidden');
            ss.errorHint.classList.add('ss-toast-error');
        }
    };
    img.onerror = (error) => {
        clearTimeout(timeoutId);
        if (gen !== ss._loadGen) return;
        console.error('[image-editor] image load failed', {source, error});
        ss.errorHint.textContent = '图片加载失败，按 ESC 关闭';
        ss.errorHint.classList.remove('hidden');
        ss.errorHint.classList.add('ss-toast-error');
    };
    img.src = `http://blink-screenshot.localhost/editor?t=${Date.now()}`;
}

/** 进入标注模式：显示工具栏 + 定位标注 canvas + 通知后端 */
function enterAnnotationMode(rect) {
    const _t0 = performance.now();
    console.debug('[screenshot] enterAnnotationMode', rect);
    const {annotCanvas, screenshot} = ss;

    // 新选区（含移动/缩放后重新进入）不能沿用旧图的聚类、展开与基准色。
    resetPaletteState();

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
    const meta = window.__blinkScreenMeta || {vx: 0, vy: 0};
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
        console.debug('[screenshot] enterAnnotationMode: crop+getImageData', {
            pw,
            ph,
            ms: Math.round(performance.now() - _tCropStart)
        });
    } catch (e) {
        console.warn('[screenshot] 提取裁剪区图像失败（马赛克功能不可用）', e);
    }

    const _tResetStart = performance.now();
    annot.reset(pw, ph, cropData);
    updateUndoRedoButtons();
    console.debug('[screenshot] enterAnnotationMode: annot.reset', {ms: Math.round(performance.now() - _tResetStart)});
    screenshotSetAnnotationMode(true).catch((e) => console.error('[screenshot] setAnnotationMode(true) 失败', e));
    const _tDrawStart = performance.now();
    drawFinalSelection();
    console.debug('[screenshot] enterAnnotationMode: drawFinalSelection', {ms: Math.round(performance.now() - _tDrawStart)});
    positionToolbar(rect);
    const _tOcrStart = performance.now();
    triggerOcrPrewarm(pw, ph);
    console.debug('[screenshot] enterAnnotationMode: triggerOcrPrewarm (sync part)', {ms: Math.round(performance.now() - _tOcrStart)});
    console.debug('[screenshot] enterAnnotationMode: total', {ms: Math.round(performance.now() - _t0)});
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
    console.debug('[image-editor] enterCanvasImageEditor', {source, pw, ph, sourceMonitor});
    const {annotCanvas, toolbar} = ss;
    // CSS 尺寸 = bitmap 尺寸 / renderScale
    const meta = window.__blinkScreenMeta || {vx: 0, vy: 0};
    const {scaleX: rsx, scaleY: rsy} = getRenderScale(meta);
    const cssW = pw / rsx;
    const cssH = ph / rsy;

    // 使用来源显示器矩形作为定位容器，默认居中到该屏幕，而不是虚拟桌面。
    const mon = sourceMonitor || {x: 0, y: 0, w: window.innerWidth, h: window.innerHeight};

    const initial = computeCanvasEditorInitialPosition(cssW, cssH, mon);
    const initialX = initial.x;
    const initialY = initial.y;
    ss.selCss = {x: initialX, y: initialY, w: cssW, h: cssH};
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
    // 0.20.x：pin 来源编辑器的勾按钮已是「替换回原窗口」，再保留「钉图」会制造第二份 pin，隐藏掉。
    const pinButton = document.getElementById('btn-pin');
    if (pinButton) pinButton.hidden = source === IMAGE_SOURCE.PIN;

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
    // 0.23.15：canvas-backed 编辑模式不使用实线选区实时层，进入前确保已复位
    resetLiveSelection();

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
        console.debug('[screenshot] 预热 OCR 跳过(选区过小)', {pw, ph});
        return;
    }
    // Task 6: prewarm 被替换时取消旧的
    if (ss.ocrPrewarm) {
        cancelActiveOcr();
    }
    const revision = ss.selectionRevision;
    const startTs = performance.now();
    ss.ocrPrewarm = new Promise((resolve) => {
        compositeSelection((pngBytes) => {
            // Task 6: 使用 handle 接口，支持取消
            const handle = ocrImage(pngBytes, ss.editorSession.epoch, revision);
            ss.activeOcrHandle = handle;
            // OCR 按钮呼吸动效：预热请求在途即点亮（handle 生命周期内）
            ss.ocrPrewarmActive = true;
            updateOcrButtonBusy();
            handle.promise
                .then(({result}) => {
                    if (revision !== ss.selectionRevision) {
                        console.debug('[screenshot] 丢弃旧选区 OCR 预热结果', {
                            revision,
                            current: ss.selectionRevision
                        });
                        resolve(null);
                        return;
                    }
                    const elapsed = Math.round(performance.now() - startTs);
                    console.debug('[screenshot] OCR 预热完成', {ms: elapsed, textLen: result?.text?.length ?? 0});
                    // 预热开关开启时，结果回来即静默激活划词（不开面板、无提示）；
                    // 内部跳过 ocrBusy/reading 已激活场景，交给正常识别链路
                    activateSilentOcrReading(result);
                    resolve(result);
                })
                .catch((rawErr) => {
                    const err = normalizeError(rawErr);
                    console.warn(`[screenshot] OCR 预热失败 [${err.code}] (用户点识别时会重试)`);
                    resolve(null);
                })
                .finally(() => {
                    if (ss.activeOcrHandle === handle) {
                        ss.activeOcrHandle = null;
                        // 预热 settle/失败即熄灭呼吸动效（Promise 本体保留作缓存）
                        ss.ocrPrewarmActive = false;
                        updateOcrButtonBusy();
                    }
                });
        });
    });
}

/** 退出标注模式（清除选区，回到可拖选状态） */
function exitAnnotationMode() {
    console.debug('[screenshot] exitAnnotationMode');
    // Task 6: 取消在途 OCR 请求
    cancelActiveOcr();
    cancelSelectionNudge();
    if (ss._annotRaf) {
        cancelAnimationFrame(ss._annotRaf);
        ss._annotRaf = 0;
    }
    // 0.23.15：退出标注模式时复位实时选区层（含在途预览 rAF）
    resetLiveSelection();
    // 0.15.10：清除快照
    ss._committedSnapshot = null;
    const {canvas, annotCanvas, toolbar, sizeHint} = ss;
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
    const {annotCanvas, toolbar, sizeHint} = ss;
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

const {canvas} = ss;

/** 长图画布移动后，offsetX/Y 已经是图片局部坐标，不能再减 selCss 偏移。 */
function annotationPoint(e) {
    // CSS 局部坐标按实际 annotation canvas backing/CSS 比例（= renderScale）转换
    const meta = window.__blinkScreenMeta || {vx: 0, vy: 0};
    const {scaleX: rsx, scaleY: rsy} = getRenderScale(meta);
    if (ss._imagePan) return {x: e.offsetX * rsx, y: e.offsetY * rsy};
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
    const mon = ss._imagePan?.monitor || {x: 0, y: 0, w: window.innerWidth, h: window.innerHeight};
    const xBounds = computePanAxisBounds(w, mon.w, mon.x);
    const yBounds = computePanAxisBounds(h, mon.h, mon.y);
    return {minX: xBounds.min, maxX: xBounds.max, minY: yBounds.min, maxY: yBounds.max};
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
    const {annotCanvas, selCss} = ss;
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

// ── 0.23.15：主画布输入统一走 Pointer Events ────────────────────────────
//
// 迁移原因：截图拖选必须支持"指针离开 canvas / 窗口后仍能正常结束交互"（快速拖出
// 边缘再松手），这依赖真实的 pointer capture。此前监听挂在 MouseEvent 上，
// `e.pointerId` 恒为 undefined，capture 分支从未生效，只能靠 window 层兜底。
//
// 迁移约定：
// - pointerdown 起即 setPointerCapture，pointerup / pointercancel 安全释放
// - 只保留 pointer 监听，不再同时挂 mouse 拖选监听（兼容鼠标事件会双处理）
// - dblclick / contextmenu 等非拖选语义保持 MouseEvent 不变
// - WebView2 提供 getCoalescedEvents() 时只取最后一个采样点，不重放历史点
//
// capture 的**适用范围**：只有拖选（pending-snap / move / resize / 新建自由拖选）与
// 长图平移起 capture。标注笔画（`ss.isAnnotDragging`）仍走非 capture 路径，"拖出窗口
// 再松手也送达"的保证**不适用于笔画**——这是刻意保持改造前行为，避免扩大改动面；
// 同理 `ss-pointer.hasActiveDragInteraction` 也不含笔画态。
//
// capture / 采样 / 中断判定的实现见 ss-pointer.js（纯逻辑，可单测）。

canvas.addEventListener('pointerdown', (e) => {
    if (!_firstMousedownLogged) {
        _firstMousedownLogged = true;
        const delta = _screenshotReadyTs > 0 ? Math.round(performance.now() - _screenshotReadyTs) : -1;
        console.debug('[screenshot] first pointerdown', {hasScreenshot: !!ss.screenshot, deltaSinceReady: delta});
    }
    if (!ss.screenshot && !ss._imagePan) return;

    const tool = annot.getTool();

    // 默认选取工具左键即可平移；其它工具仍可用 Space/中键临时平移。
    if (ss._imagePan && (_spaceDown || e.button === 1 || (e.button === 0 && tool === 'select'))) {
        capturePointer(canvas, e);
        beginLongImagePan(e);
        return;
    }

    if (e.button !== 0) return;

    const point = pointerPoint(e);

    // 0.15.8 R2 + 0.18.2：吸附——pending-snap 状态机（控件优先于窗口）
    // pointerdown 只记录候选矩形和起点，不立即吸附；
    // pointerup 时若总位移 < 3px 才采用矩形；pointermove 达到阈值转 free-selecting。
    if (!ss.isAnnotating && !ss.selectionInteraction) {
        // 0.18.2：控件优先于窗口——控件命中时用控件矩形，否则回退窗口矩形
        const snapRect = getHoveredControlRect() || getHoveredWindowRect();
        if (snapRect) {
            ss.pendingSnap = {
                startX: point.offsetX,
                startY: point.offsetY,
                winRect: snapRect,
                pointerId: e.pointerId,
            };
            // 0.23.15：capture 保证快速拖出 canvas 后仍能收到 pointerup。
            // 迁移前此处的 e.pointerId 恒为 undefined，该分支从未真正生效。
            capturePointer(canvas, e);
            return;
        }
    }

    if (ss.isAnnotating && ss.selCss && tool === 'select') {
        const handle = getSelectionHandle(point.offsetX, point.offsetY, ss.selCss);
        if (handle) {
            capturePointer(canvas, e);
            beginSelectionInteraction('resize', point, handle);
            return;
        }
        if (pointInRect(point.offsetX, point.offsetY, ss.selCss)) {
            capturePointer(canvas, e);
            beginSelectionInteraction('move', point);
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

    if (ss.isAnnotating && ss.selCss && pointInEditableImage(point)) {
        if (tool === 'watermark') return;
        const annotPt = annotationPoint(point);
        ss.annotStartX = annotPt.x;
        ss.annotStartY = annotPt.y;
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
    // 0.23.15：capture 让"拖出 canvas 边缘再松手"由浏览器保证送达
    capturePointer(canvas, e);
    ss.isDragging = true;
    ss.sent = false;
    ss.startX = point.offsetX;
    ss.startY = point.offsetY;
    ss.endX = ss.startX;
    ss.endY = ss.startY;
});

canvas.addEventListener('pointermove', (e) => {
    // 0.15.7：长图平移拖拽
    if (moveLongImagePan(e)) return;

    if (!ss.screenshot && !ss.editorSession.canvasBacked) return;

    // 0.23.15：只用最新采样点——同帧内的历史点对选区没有意义
    const point = pointerPoint(e);
    const {offsetX, offsetY} = point;

    if (!ss._imagePan) updateSelectionCursor(offsetX, offsetY);

    // 0.18.2：选区拖拽阶段智能吸附（控件优先于窗口）
    // 0.18.x：跨屏预选——先命中全局顶层窗口 → 得到 hovered hwnd → setControlTarget → 控件 hit-test
    // 手动框选拖拽中（isDragging）不更新吸附提示，避免实线选区与虚线预选区同时出现
    // 0.19.14-fix：先 hit-test 窗口获取 hwnd 但不显示 hint，等控件 hit-test 结果再决定显示哪个 hint，
    // 避免控件命中时每帧 show→hide 窗口 hint 导致蓝色虚线框闪烁
    if (!ss.isAnnotating && !ss.selectionInteraction && !ss.isDragging) {
        // 第一步：窗口 hit-test（仅更新内部索引，不显示 hint）
        updateWindowHover(offsetX, offsetY, {skipShowHint: true});
        const winRect = getHoveredWindowRect();
        // 0.21.x：控件级吸附受 control_snap 开关门控——此前仅预热被门控、运行期恒启用，导致开关无效
        if (ss.screenshotConfig.controlSnap) {
            // 窗口边缘吸附：鼠标落在窗口四边 R px 内 → 清控件态、只显示窗口级蓝色框，
            // 保证控件铺满窗口时仍能选到整窗
            const edgePx = ss.screenshotConfig.windowEdgeSnap || 0;
            if (winRect?.hwnd && edgePx > 0 && pointNearWindowEdge(offsetX, offsetY, winRect, edgePx)) {
                setControlTarget(null);
                showWindowHintIfPending();
            } else {
                setControlTarget(winRect?.hwnd ?? null);
                // 第二步：控件优先 hit-test
                if (updateControlHover(offsetX, offsetY)) {
                    // 控件命中：隐藏窗口 hint（可能上次鼠标在窗口空白处时显示了）
                    hideWindowHintIfVisible();
                } else {
                    // 控件未命中或尚未加载：显示窗口级蓝色预选框
                    showWindowHintIfPending();
                }
            }
        } else {
            // 控件级吸附关闭：仅显示窗口级蓝色预选框
            showWindowHintIfPending();
        }
        updatePixelMagnifier(offsetX, offsetY);
        // 0.15.12：存储最新位置供 Shift 切格式时强制刷新
        ss._lastMagnifierPos = {x: offsetX, y: offsetY};
    } else if (ss.eyedropperActive) {
        // 0.15.10：取色器模式下显示像素放大镜预览
        updatePixelMagnifier(offsetX, offsetY);
        ss._lastMagnifierPos = {x: offsetX, y: offsetY};
    } else if (ss.magnifierEl) {
        hidePixelMagnifier();
    }

    // 0.15.8 R2：pending-snap 阈值检测——达到 3px 转为自由框选
    if (ss.pendingSnap) {
        if (shouldStartFreeSelection(
            ss.pendingSnap.startX,
            ss.pendingSnap.startY,
            offsetX,
            offsetY,
        )) {
            // 达到阈值，清除候选并从原始按下点开始自由框选
            clearHover();
            clearControlHover();
            ss.startX = ss.pendingSnap.startX;
            ss.startY = ss.pendingSnap.startY;
            ss.endX = offsetX;
            ss.endY = offsetY;
            ss.pendingSnap = null;
            ss.isDragging = true;
            ss.sent = false;
            ss.snappedHwnd = null;
            // 0.23.15：实时预览交给统一调度（首次调用会清空 interaction-canvas 一次）
            updateLiveSelection(
                computeDragRect(ss.startX, ss.startY, offsetX, offsetY, !!point.shiftKey)
            );
        }
        return;
    }

    if (ss.selectionInteraction) {
        updateSelectionInteraction(point);
        return;
    }

    updateStrokeCursor(e.clientX, e.clientY);

    if (ss.isAnnotDragging && ss.selCss) {
        const annotPt = annotationPoint(point);
        ss.annotCurrentX = annotPt.x;
        ss.annotCurrentY = annotPt.y;
        if (point.shiftKey) {
            const constrained = applySquareConstraint(
                ss.annotStartX, ss.annotStartY, ss.annotCurrentX, ss.annotCurrentY, annot.getTool()
            );
            if (constrained) {
                ss.annotCurrentX = constrained.x;
                ss.annotCurrentY = constrained.y;
            }
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
        // 0.20.6：Shift 按下时强制 1:1 正方形约束（自由框选路径，与 release 共用同一纯函数）
        ss.endX = offsetX;
        ss.endY = offsetY;
        // 0.23.15：单飞实时预览——事件侧只写"最新矩形"，每帧最多更新一次 DOM
        updateLiveSelection(
            computeDragRect(ss.startX, ss.startY, offsetX, offsetY, !!point.shiftKey)
        );
    }
});

canvas.addEventListener('pointerleave', () => {
    // W4 例外：strokeCursor 是高频逐帧更新的画笔预览光标，直接写 style.display 性能更好
    if (ss.strokeCursor) ss.strokeCursor.style.display = 'none';
    // 0.15.8 R2：离开 canvas 时清除 pending-snap 状态
    // （pointer capture 生效期间本事件被抑制，拖选不会因为指针移出而中断）
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

canvas.addEventListener('pointerup', (e) => {
    // 0.23.15：先取消未落地的实时预览 rAF 并复位实时层；canvas 提交由下面各条路径
    // 在同一 JS task 内同步完成。两者同帧生效，因此不会闪白、双边框或短暂无蒙版。
    resetLiveSelection();
    // 0.15.7：长图平移结束
    if (endLongImagePan()) {
        releasePointer(canvas, e);
        return;
    }

    if (!ss.screenshot && !ss.editorSession.canvasBacked) {
        releasePointer(canvas, e);
        return;
    }

    // 0.23.15：release 也取最新采样点——被合帧丢弃的历史点不应决定最终矩形
    const point = pointerPoint(e);

    // 0.15.8 R2：pending-snap 完成——未达阈值，采用窗口矩形
    if (ss.pendingSnap) {
        const winRect = ss.pendingSnap.winRect;
        ss.pendingSnap = null;
        // 释放 pointer capture
        releasePointer(canvas, e);
        if (winRect.w >= 5 && winRect.h >= 5) {
            ss.snappedHwnd = winRect.hwnd || null;
            ss.startX = winRect.x;
            ss.startY = winRect.y;
            ss.endX = winRect.x + winRect.w;
            ss.endY = winRect.y + winRect.h;
            ss.isDragging = false;
            console.debug('[screenshot] window snap (pending-snap confirmed)', winRect);
            try {
                enterAnnotationMode({x: winRect.x, y: winRect.y, w: winRect.w, h: winRect.h});
            } catch (err) {
                console.error('[screenshot] window snap enterAnnotationMode threw', err);
            }
        }
        return;
    }

    if (finishSelectionInteraction(point)) {
        releasePointer(canvas, e);
        return;
    }

    if (ss.isAnnotDragging) {
        ss.isAnnotDragging = false;
        // 0.15.9：取消待执行的 rAF，确保最终重绘是最新的
        if (ss._annotRaf) {
            cancelAnimationFrame(ss._annotRaf);
            ss._annotRaf = 0;
        }
        // 0.15.10：清除快照
        ss._committedSnapshot = null;
        const annotPt = annotationPoint(point);
        ss.annotCurrentX = annotPt.x;
        ss.annotCurrentY = annotPt.y;
        if (point.shiftKey) {
            const constrained = applySquareConstraint(
                ss.annotStartX, ss.annotStartY, ss.annotCurrentX, ss.annotCurrentY, annot.getTool()
            );
            if (constrained) {
                ss.annotCurrentX = constrained.x;
                ss.annotCurrentY = constrained.y;
            }
        }

        const tool = annot.getTool();
        const dx = ss.annotCurrentX - ss.annotStartX;
        const dy = ss.annotCurrentY - ss.annotStartY;
        // 0.15.1：用 TOOL_CAPS 替代硬编码 minDrag 列表
        const minDrag = (TOOL_CAPS[tool] || TOOL_CAPS.select).minDrag;
        if (Math.abs(dx) < minDrag && Math.abs(dy) < minDrag) {
            console.debug('[screenshot] annotation drag too small, skip', {tool, dx, dy});
            ss._committedSnapshot = null;
            redrawAnnotFull();
            releasePointer(canvas, e);
            return;
        }

        const result = annot.endDraw(ss.annotCurrentX, ss.annotCurrentY);
        if (result && result.needsText) {
            showTextInput(result.x, result.y);
        }
        redrawAnnotFull();
        updateUndoRedoButtons();
        releasePointer(canvas, e);
        return;
    }

    if (!ss.isDragging) {
        releasePointer(canvas, e);
        return;
    }
    ss.isDragging = false;
    ss.endX = point.offsetX;
    ss.endY = point.offsetY;
    // 0.20.6：Shift 按下时强制 1:1 正方形约束（release 路径与拖动共用同一纯函数）
    const rect = computeDragRect(ss.startX, ss.startY, point.offsetX, point.offsetY, !!point.shiftKey);
    if (rect.w < 5 || rect.h < 5) {
        console.debug('[screenshot] rect too small, wait for dblclick', rect);
        if (ss.singleClickTimeout) clearTimeout(ss.singleClickTimeout);
        ss.singleClickTimeout = setTimeout(() => {
            ss.singleClickTimeout = null;
            if (!ss.isAnnotating && !ss.sent) {
                console.debug('[screenshot] single click → hide overlay');
                // 0.23.12：此路径原先不做清理，是旧图层残留闪现的出口之一
                cleanupCanvasVisuals();
                hideScreenshotOverlay().catch((err) => console.error('hideScreenshotOverlay 失败', err));
            }
        }, 200);
        releasePointer(canvas, e);
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
    releasePointer(canvas, e);
});

/**
 * 0.23.15：会话是否已进入收尾（由 ss-output.cleanupCanvasVisuals 置位）。
 * 收尾态下只允许做状态复位，不允许重建任何会话内 UI。
 */
function isSessionTearingDown() {
    return document.documentElement.classList.contains('screenshot-session-inactive');
}

/**
 * 0.23.15：拖选被异常中断（pointercancel / lostpointercapture）时的收口。
 *
 * 与 finish 的区别是**不提交**本次拖拽，恢复到进入交互前的可用状态：
 * - 移动 / 缩放：回到交互开始前的 original 矩形。激活时已执行
 *   invalidateSelectionContent（标注层与工具栏被清），因此需要重新进入标注模式，
 *   否则用户会看到"有选区但没有工具栏"的半残状态
 * - 新建拖选：丢弃半程矩形（`kind === 'new'` 目前只有 ss-interaction.test.mjs 构造，
 *   生产的新建拖选由 `ss.isDragging` 路径承载，因此这里的 selCss 归位实际只作用于
 *   move / resize）
 *
 * 会话已进入收尾时（ESC / 失焦 / 输出清场之后浏览器补发的中断事件）**只复位状态、
 * 不重建 UI**：重建会重新裁图、发 screenshotSetAnnotationMode(true) 并触发 OCR
 * prewarm，等于把一个已经 cancel 的会话拉回标注态。
 */
function abortSelectionInteraction(e = null) {
    resetLiveSelection();
    releasePointer(canvas, e);
    ss.pendingSnap = null;
    const interaction = ss.selectionInteraction;
    ss.selectionInteraction = null;
    ss.isDragging = false;
    if (interaction && interaction.kind !== 'new') {
        ss.selCss = {...interaction.original};
    }
    if (isSessionTearingDown()) {
        // 与 cleanupSessionVisuals 的拖选状态归零是双保险：那道清理挂在
        // ss._cleanupSessionVisuals 回调上，而调用方是 try/catch 容错调用的
        // （钩子没接上也只 warn）。收尾态下重建 UI 的代价极高，这里再判一次。
        console.debug('[screenshot] abort ignored: session tearing down');
        return;
    }
    ss.canvas.style.cursor = annot.getTool() === 'select' ? 'default' : 'crosshair';
    if (ss.isAnnotating && ss.selCss) {
        try {
            enterAnnotationMode({...ss.selCss});
        } catch (err) {
            console.error('[screenshot] abort 后重建标注模式失败', err);
        }
    } else {
        drawDimmed();
    }
}

canvas.addEventListener('pointercancel', (e) => {
    // 指针被系统或浏览器回收（触屏手势介入、窗口被抢占等）：按取消处理，
    // 不把半程结果变成最终选区
    if (!hasActiveDragInteraction(ss)) return;
    console.debug('[screenshot] pointercancel → abort selection interaction');
    abortSelectionInteraction(e);
});

canvas.addEventListener('lostpointercapture', (e) => {
    // capture 被隐式释放（正常 pointerup 之后也会触发）。只有仍存在未结束的拖选时
    // 才视为异常中断，避免把正常提交二次处理成取消。
    if (!hasActiveDragInteraction(ss)) return;
    console.debug('[screenshot] lostpointercapture with active drag → abort');
    abortSelectionInteraction(e);
});

canvas.addEventListener('dblclick', (e) => {
    console.debug('[screenshot] dblclick', {isAnnotating: ss.isAnnotating, hasSelCss: !!ss.selCss, sent: ss.sent});
    if ((!ss.screenshot && !ss.editorSession.canvasBacked) || ss.sent) return;
    if (ss.singleClickTimeout) {
        clearTimeout(ss.singleClickTimeout);
        ss.singleClickTimeout = null;
    }

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

// ── 0.20.7：方向键移动选区合帧 ────────────────────────────────────────────
// 按住方向键 auto-repeat（~25-30 次/秒）时，逐键执行 canvas 裁剪 + putImageData
// + OCR 预热 IPC 会产生可观开销与预热风暴。改为 rAF 内合并增量、单次裁剪，
// OCR 预热 debounce，最后一步停下才触发。
//
// 0.20.7 fix：rAF 和 OCR timer 绑定 selectionRevision/session generation，
// 旧会话排队的任务在新会话中只清理自己的状态，不能改新会话。

/** 方向键增量合帧的 rAF id（0 = 无待刷新帧） */
let nudgeRafId = 0;
/** 待合并的方向键增量（bitmap px） */
let nudgePendingDx = 0;
let nudgePendingDy = 0;
/** OCR 预热 debounce 定时器 */
let nudgeOcrPrewarmTimer = 0;
/** OCR 预热 debounce 时长 */
const NUDGE_OCR_PREWARM_DEBOUNCE_MS = 300;
/** 排队时的 selectionRevision，回调执行前校验 */
let nudgeSelectionRevision = 0;

/**
 * 取消方向键 nudge 的 rAF、OCR timer 和 pending 增量。
 * 在 resetState、exitAnnotationMode 和会话切换时调用，
 * 确保旧会话排队的任务不会影响新会话。
 */
function cancelSelectionNudge() {
    if (nudgeRafId) {
        cancelAnimationFrame(nudgeRafId);
        nudgeRafId = 0;
    }
    if (nudgeOcrPrewarmTimer) {
        clearTimeout(nudgeOcrPrewarmTimer);
        nudgeOcrPrewarmTimer = 0;
    }
    nudgePendingDx = 0;
    nudgePendingDy = 0;
}

/** rAF 回调：一次性应用累积增量的选区移动 + 裁剪 + 刷新。 */
function flushSelectionNudge() {
    nudgeRafId = 0;
    const dx = nudgePendingDx;
    const dy = nudgePendingDy;
    nudgePendingDx = 0;
    nudgePendingDy = 0;
    if (dx === 0 && dy === 0) return;
    // 会话已退出/选区已清除/selectionRevision 已变时丢弃过期增量
    if (!ss.isAnnotating || !ss.selCss || annot.getTool() !== 'select') return;
    if (nudgeSelectionRevision !== ss.selectionRevision) return;

    const meta = window.__blinkScreenMeta || {vx: 0, vy: 0};
    const newSelCss = moveSelection1px(ss.selCss, dx, dy, meta);
    if (!newSelCss) return;
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
    // P1-4 fix：移动后更新 nudgeSelectionRevision 为最新值。
    // queueSelectionNudge 捕获的是移动前的 revision，flushSelectionNudge
    // 执行 ss.selectionRevision++ 后两者不再相等，导致 OCR 预热 timer
    // 回调中的 revision 校验永远失败、预热永远不触发。
    // 在此同步为最新 revision，让 timer 回调校验能通过。
    nudgeSelectionRevision = ss.selectionRevision;
    ss.ocrPrewarm = null;
    ss.ocrResultCache = null;
    if (typeof ss._invalidateSelectionContent === 'function') {
        ss._invalidateSelectionContent();
    }
    drawFinalSelection();
    // 刷新 OCR 预热（debounce：连续按键只在停下后触发一次）
    clearTimeout(nudgeOcrPrewarmTimer);
    nudgeOcrPrewarmTimer = setTimeout(() => {
        nudgeOcrPrewarmTimer = 0;
        // 校验 selectionRevision：旧会话的 timer 不能在新会话上触发 OCR 预热
        if (nudgeSelectionRevision !== ss.selectionRevision) return;
        triggerOcrPrewarm(pw, ph);
    }, NUDGE_OCR_PREWARM_DEBOUNCE_MS);
}

/** 排队一次方向键增量（rAF 合帧）。 */
function queueSelectionNudge(dx, dy) {
    // 捕获当前 selectionRevision，回调执行前校验
    nudgeSelectionRevision = ss.selectionRevision;
    nudgePendingDx += dx;
    nudgePendingDy += dy;
    if (!nudgeRafId) nudgeRafId = requestAnimationFrame(flushSelectionNudge);
}

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
            if (e.key === 'ArrowLeft') {
                dx = -1;
            } else if (e.key === 'ArrowRight') {
                dx = 1;
            } else if (e.key === 'ArrowUp') {
                dy = -1;
            } else if (e.key === 'ArrowDown') {
                dy = 1;
            }
            if (dx !== 0 || dy !== 0) {
                e.preventDefault();
                // 0.20.7：合帧处理——rAF 内合并增量、单次裁剪、OCR 预热 debounce
                queueSelectionNudge(dx, dy);
                return;
            }
        }
    }

    if (e.key === 'Escape') {
        e.preventDefault();
        // 0.15.7：长截图采集阶段——ESC 先退出长截图模式
        if (isScrollCaptureActive()) {
            exitScrollCapture().catch(() => {
            });
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
window.addEventListener('wheel', onScrollWheel, {passive: true});

// 长图平移（Space / 中键）的窗口层兜底。
// 0.23.15 后 canvas 的 pointerdown 已起 pointer capture，canvas 内发起的平移由
// pointermove/pointerup 正常送达；这里保留 window 层兜底以覆盖"平移由非 canvas
// 元素发起或 capture 被浏览器回收"的路径。capture 生效期间鼠标事件会被重定向到
// canvas（e.target === canvas），因此不会与 canvas 的 pointer 处理双跑。
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
window.addEventListener('blur', () => {
    delete document.body.dataset.altDown;
});

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
        setTimeout(() => {
            ss.blurGuard = false;
        }, 2000);
        console.debug('[screenshot] window blur ignored (image editor mode, extended blurGuard)');
        return;
    }
    if (ss.blurGuard) return;
    ss.blurGuard = true;
    setTimeout(() => {
        ss.blurGuard = false;
    }, 500);

    if (hasActivePanel()) {
        console.debug('[screenshot] window blur ignored (active panel)');
        return;
    }

    // 0.23.15：取消未落地的实时预览 rAF 并复位实时层
    resetLiveSelection();
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

// 所有正常交互 handler 已完成注册；从此 ESC 只能走模块的分层关闭/会话清理路径。
window.__blinkDisableEmergencyScreenshotEscape?.();
