//! 0.15.7 长截图：三阶段状态机 + 滚动检测 + 帧拼接 + 预览 + 编辑入口。
//!
//! **三阶段**：
//! - `idle` → 用户点工具栏【长截图】按钮 → `capturing`
//! - `capturing` → 用户滚动（手动/自动），逐帧截取拼接 → 点【编辑】 → `editing`
//! - `editing` → 合成长图进标注模式 → pin/保存/复制 → 结束
//!
//! **帧采集**（0.15.7 修复）：
//! - 不再从静态主 canvas 读 getImageData（内容不变，每帧相同）
//! - 改为调用 Rust `screenshot_capture_band` 命令做 fresh BitBlt
//! - 进入 capturing 前设 `WDA_EXCLUDEFROMCAPTURE` 排除 overlay 自身
//!
//! **手动滚动**：
//! - 选区内部原生穿透，真实输入直达目标窗口；控件及失败降级保留滚轮转发
//! - 持续调用 `screenshot_capture_band`，保留有界 FIFO 中间帧，与输入解耦
//!
//! **自动滚动**：
//! - 每轮只注入一次滚轮，经过同一队列处理后才开始下一轮
//! - Worker 串行匹配/提交；稳定探针只用于最终收尾
//!
//! **帧拼接算法**（双向 SAD 模板匹配）：
//! 1. 同时估算上/下位移，滚轮方向只用于重复纹理的优先级
//! 2. 记录当前视口在长图中的绝对 top
//! 3. 仅保存新暴露的顶部/底部行；回滚到已有区域不重复追加

import {ss} from '../ss-state.js';
import {screenshotCancel, screenshotSetCaptureExclusion,} from '../../shared/api.js';
import {findWindowForRect} from '../ss-hover.js';
import {ContinuousScrollCapture} from './continuous.js';
import {ScrollTrackingClient} from './tracking-client.js';
import {runAutoScrollController} from './auto.js';
import {finalizeScrollCapture, recordTailStability} from './finalize.js';
import {
    captureBandFrame,
    delay,
    forwardAutoWheel,
    queueManualWheel,
    waitForVisualSettle,
} from './capture-driver.js';
import {bindScrollDiagnostics, canRetainScrollReplayFrame, recordScrollDiagnostic, recordScrollEvent, resetScrollDiagnostics,} from './diagnostics.js';
import {isLongScreenshotPan} from '../ss-editor-policy.js';
import {encodeImageDataPng, outputScreenshotPng} from '../ss-output.js';
import {cssPointToScreen, cssRectToBitmap} from '../ss-selection-geometry.js';
import {findDisplayCssAt} from '../ss-display.js';
import {IMAGE_SOURCE} from '../image-editor-session.js';
import {
    hideCaptureFrame,
    logScrollUiDiagnostics,
    positionPreview,
    positionScrollToolbar,
    resetPreviewRendering,
    scrollCaptureInputRegions,
    setScrollCapturePresentation,
    showCaptureFrame,
    showScrollInputFeedback,
    updatePreview,
} from './preview.js';

const session = ss.scrollSession;

function roundedMs(value) {
    return Number.isFinite(value) ? Math.round(value * 10) / 10 : null;
}

function captureStillActive(generation, requireAuto = false) {
    return generation === session.captureGeneration
        && session.scrollCapturePhase === 'capturing'
        && (!requireAuto || session.autoScroll);
}


/**
 * 进入长截图采集阶段（capturing）。
 * 由 index.js 的【长截图】按钮调用。
 *
 * @param {object} rect - 框选矩形 CSS 坐标 {x, y, w, h}
 */
export async function enterScrollCapture(rect) {
    session.reset();
    const generation = session.captureGeneration;
    session.manualWheelVersion = 0;
    session.autoWheelDelta = -120;
    session.autoLowConfidenceCount = 0;
    session.queuedManualWheel = null;
    session.captureFinalizing = false;
    const meta = window.__blinkScreenMeta || {vx: 0, vy: 0};

    // 记录采集带几何（物理像素，虚拟屏幕坐标系）
    // 统一使用 renderScale 换算
    const bandPos = cssPointToScreen(rect.x, rect.y, meta);
    session.scrollBandX = bandPos.x;
    session.scrollBandY = bandPos.y;
    const bandBmp = cssRectToBitmap(rect, meta);
    session.scrollBandW = bandBmp.w;
    session.scrollBandH = bandBmp.h;
    session.scrollDirection = 'vertical';
    session.autoScroll = false;
    session.scrollFrames = [];
    session.scrollLastFrame = null;
    session.scrollKeyframes = [];
    session.scrollTrackingState = 'tracking';
    session.scrollLostFrameCount = 0;
    session.scrollCurrentTop = 0;
    session.scrollPendingJump = null;
    session.scrollPendingDirection = 0;
    session.scrollUnchangedCount = 0;
    session.scrollCapturePhase = 'capturing';
    // 工具栏 click 进入采集后，不允许旧选区 pointer/mouseup 状态再次提交普通选区。
    ss.selectionInteraction = null;
    ss.pendingSnap = null;
    ss.isDragging = false;
    resetPreviewRendering();
    resetScrollDiagnostics(session);

    // 优先锁定框选区域实际覆盖的外部窗口；兜底使用 Blink 唤起前保存的前台窗口。
    const pickedWindow = findWindowForRect(rect);
    session.scrollHwnd = pickedWindow?.hwnd || meta.fgHwnd || null;
    // 统一使用 renderScale 换算
    const targetPos = pickedWindow
        ? cssPointToScreen(pickedWindow.targetX, pickedWindow.targetY, meta)
        : {
            x: session.scrollBandX + Math.floor(session.scrollBandW / 2),
            y: session.scrollBandY
        };
    session.scrollTargetX = pickedWindow
        ? targetPos.x
        : session.scrollBandX + Math.floor(session.scrollBandW / 2);
    session.scrollTargetY = pickedWindow
        ? targetPos.y
        : session.scrollBandY;
    session.scrollSourceRect = {...rect};

    // 确定来源显示器：使用选区中心或右下角内侧点作为锚点。
    // 锚点必须使用 rect.y + rect.h - 1（显示器排他边界内侧），
    // 不能使用 rect.y + rect.h（排他边界本身可能落在下一屏或虚拟桌面空洞）。
    const monitorAnchorX = rect.x + rect.w / 2;
    const monitorAnchorY = rect.y + Math.max(0, rect.h - 1);
    session.scrollSourceMonitor = findDisplayCssAt(monitorAnchorX, monitorAnchorY);

    // ── 0.23.11 修复：先就位全部采集 UI，最后才设 WDA_EXCLUDEFROMCAPTURE ──
    // 实测（Windows 本机 Console 会话）：设置 DisplayAffinity 后 WebView2 的上屏
    // 呈现可能停摆——DOM/输入命中/IPC 全部正常（诊断日志、rAF、elementsFromPoint
    // 均正常），唯独画面不再更新。旧顺序"先设排除再摆 UI"会把工具栏摆位帧吞掉，
    // 用户看到"蒙版消失但工具栏永远不出现、点击无反应"的冻屏。
    // 现改为：可见 UI 同步就位（单帧呈现）→ 设排除 → 首帧采集。即便呈现从此停摆，
    // 用户也已看到完整采集 UI，工具栏按钮的 DOM 命中不受呈现影响，采集/取消可用。

    // 清除暗色蒙版，让用户看到真实背景；原生输入区域在控制层就位后一起设置。
    if (ss.ctx && ss.canvas) {
        ss.ctx.clearRect(0, 0, ss.canvas.width, ss.canvas.height);
        ss.canvas.style.pointerEvents = 'none';
    }
    if (ss.annotCanvas) ss.annotCanvas.classList.add('hidden');
    setScrollCapturePresentation(true);
    // 隐藏 OCR hit canvas 和像素放大镜
    if (ss.hitCanvas) ss.hitCanvas.style.pointerEvents = 'none';
    showCaptureFrame(rect);

    // 主动隐藏 sizeHint——长截图采集阶段不需要截图坐标提示
    if (ss.sizeHint) ss.sizeHint.classList.add('hidden');

    // 切换工具栏：隐藏默认，显示专属（0.19.16 DPI 适配）
    if (ss.toolbar) ss.toolbar.classList.add('hidden');
    positionScrollToolbar(rect);

    // 显示预览缩略图——贴选区侧边
    // 纵向：贴右边；横向：贴下边
    positionPreview(rect);

    // 设置 WDA_EXCLUDEFROMCAPTURE：overlay 在整段采集会话中不进入 BitBlt。
    try {
        const inputToken = await screenshotSetCaptureExclusion(true, null, scrollCaptureInputRegions(rect));
        if (!captureStillActive(generation)) {
            await screenshotSetCaptureExclusion(false, inputToken);
            return false;
        }
        session.scrollInputToken = inputToken;
    } catch (e) {
        if (!captureStillActive(generation)) {
            console.warn('[scroll] 设置捕获排除期间会话已被并发失效，长截图静默中止', {
                generation,
                current: session.captureGeneration,
                phase: session.scrollCapturePhase,
            });
            return false;
        }
        console.warn('[scroll] 设置捕获排除失败，已中止长截图', e);
        resetScrollCaptureState();
        ss._showTransientHint?.('当前系统无法排除截图工具界面，长截图已取消');
        screenshotSetCaptureExclusion(false, session.scrollInputToken)
            .catch((cleanupError) => console.warn('[scroll] 捕获排除失败后的清理失败', cleanupError));
        return false;
    }
    // ESC、失焦、overlay 重载或新会话都可能发生在上面的 IPC 等待期间。
    // 旧入口不得继续修改新一代 canvas / toolbar 状态。
    if (!captureStillActive(generation)) {
        // 0.23.11：此处曾是完全静默 return——"点长截图没反应"类现场无法从日志定位
        console.warn('[scroll] 排除设置完成后会话已被并发失效，长截图静默中止', {
            generation,
            current: session.captureGeneration,
            phase: session.scrollCapturePhase,
        });
        return false;
    }

    logScrollUiDiagnostics(rect);

    // 截取第一帧
    try {
        createContinuousPipeline();
        await captureFrame(0, generation);
    } catch (error) {
        if (!captureStillActive(generation)) return false;
        console.warn('[scroll] 连续采集初始化失败', error);
        await exitScrollCapture(true);
        ss._showTransientHint?.('连续采集启动失败，请重新框选后重试');
        return false;
    }
    if (!captureStillActive(generation)) {
        console.warn('[scroll] 首帧采集后会话已被并发失效，长截图静默中止', {
            generation,
            current: session.captureGeneration,
            phase: session.scrollCapturePhase,
        });
        return false;
    }
    if (!session.scrollHwnd) {
        ss._showTransientHint?.('未找到可滚动窗口，请重新框选目标窗口内的区域');
    }
    session.continuousCapture.start();
    console.info('[scroll] enter capturing', {
        sourceRect: {x: rect.x, y: rect.y, w: rect.w, h: rect.h},
        sourceMonitor: session.scrollSourceMonitor,
        bandPhysicalRect: {
            x: session.scrollBandX,
            y: session.scrollBandY,
            w: session.scrollBandW,
            h: session.scrollBandH
        },
        hwnd: session.scrollHwnd,
        target: pickedWindow?.processName || pickedWindow?.title || 'fallback',
    });
    return true;
}

/**
 * 截取当前帧并拼接到 scrollFrames。
 * 每次滚动后调用（手动 wheel 或自动滚动回调）。
 */
function createContinuousPipeline() {
    const lifecycleId = session.lifecycleId;
    session.tailStability = null;
    let firstObservation = session.scrollFrames.length > 0;
    const client = new ScrollTrackingClient({frames: session.scrollFrames,
        currentTop: session.scrollCurrentTop, height: session.scrollBandH,
        trackingState: session.scrollTrackingState});
    let pipeline;
    const current = () => lifecycleId === session.lifecycleId && session.continuousCapture === pipeline;
    pipeline = new ContinuousScrollCapture({
        frameBytes: session.scrollBandW * session.scrollBandH * 4,
        metadata: () => ({wheelVersion: session.manualWheelVersion,
            wheelStartedAtMs: session.scrollWheelStartedAtMs}),
        capture: () => {
            recordScrollEvent(session, 'capture-start');
            return captureBandFrame(session);
        },
        process: async (frame, metadata) => {
            if (!current()) throw new Error('长截图采集会话已失效');
            metadata.trackingReset = firstObservation;
            firstObservation = false;
            const diagnosticFrame = canRetainScrollReplayFrame(session, frame.data.byteLength)
                ? new ImageData(frame.data.slice(), frame.width, frame.height) : null;
            const startedAt = performance.now();
            const result = await client.process(frame, {
                expectedDirection: metadata.expectedDirection || 0,
                motionTimedOut: metadata.settle?.timedOut === true,
                discontinuity: metadata.discontinuity,
            });
            return {...result, diagnosticFrame, queueMs: startedAt - metadata.capturedAt,
                workerRoundTripMs: performance.now() - startedAt};
        },
        onResult: (result, metadata) => {
            if (!current()) return;
            const {decision} = result;
            recordTailStability(session, result, metadata);
            if (decision.accepted) {
                const previous = new Map(session.scrollFrames.map(item => [item.id, item]));
                session.scrollFrames = result.segments.map(item => ({id: item.id, top: item.top,
                    image: item.buffer
                        ? new ImageData(new Uint8ClampedArray(item.buffer), item.width, item.height)
                        : previous.get(item.id).image,
                    preview: item.preview
                        ? new ImageData(new Uint8ClampedArray(item.preview.buffer), item.preview.width, item.preview.height)
                        : previous.get(item.id)?.preview}));
                session.scrollCurrentTop = result.currentTop;
                session.scrollUnchangedCount = 0;
                session.lastMovementResult = {moved: true, appended: result.appended,
                    decision, positionShift: result.positionShift, trackingState: result.trackingState};
                if (result.positionShift) session.scrollLastAcceptedShift = result.positionShift;
            } else session.scrollUnchangedCount++;
            session.scrollPendingJump = result.pendingJump;
            session.scrollTrackingState = result.trackingState;
            session.scrollLostFrameCount = result.lostFrameCount;
            const candidateTop = decision.reason === 'pending-confirmation' ? decision.candidateTop : null;
            const previewMs = updatePreview({candidateTop,
                pendingInput: metadata.wheelVersion !== session.manualWheelVersion});
            const completedAt = performance.now();
            recordScrollDiagnostic(session, result.diagnosticFrame, decision, {
                ...metadata, tracking: result.tracking,
                timing: {
                    preCaptureMs: roundedMs(metadata.wheelStartedAtMs == null ? null
                        : metadata.captureStartedAt - metadata.wheelStartedAtMs),
                    captureMs: roundedMs(metadata.capturedAt - metadata.captureStartedAt),
                    queueMs: roundedMs(result.queueMs),
                    trackMs: roundedMs(result.timing.trackMs),
                    commitMs: roundedMs(result.timing.commitMs),
                    workerRoundTripMs: roundedMs(result.workerRoundTripMs),
                    previewMs: roundedMs(previewMs),
                    totalLatencyMs: roundedMs(completedAt - (metadata.wheelStartedAtMs ?? metadata.captureStartedAt)),
                    lastWheelToPreviewMs: roundedMs(session.scrollLastWheelAtMs == null ? null
                        : completedAt - session.scrollLastWheelAtMs),
                },
            });
            if (result.becameLost) ss._showTransientHint?.('暂未找到可靠重叠；请缓慢滚回最近已捕获区域后继续');
            if (decision.reason === 'frame-limit') {
                pipeline.pause();
                ss._showTransientHint?.('长截图已达上限，请点击完成按钮结束');
            }
            if (metadata.wheelVersion === session.manualWheelVersion && result.moved) {
                session.scrollPendingDirection = 0;
                session.scrollWheelStartedAtMs = null;
            }
        },
        onPressure: stats => {
            if (!current()) return;
            recordScrollEvent(session, 'buffer-pressure', stats);
            ss._showTransientHint?.('采集处理积压，已保留中间帧；请减慢滚动，必要时滚回已捕获区域');
        },
        onError: error => {
            if (!current()) return;
            console.warn('[scroll] 连续采集失败，保留已确认长图', error);
            pipeline.dispose();
            client.dispose();
            ss._showTransientHint?.('连续采集已暂停，已保留长图；请点击完成重试收尾');
        },
    });
    session.trackingClient = client;
    session.continuousCapture = pipeline;
    return pipeline;
}

function resumeContinuousCapture() {
    if (!session.continuousCapture || session.continuousCapture.closed || session.trackingClient?.closed) {
        session.continuousCapture?.dispose();
        session.trackingClient?.dispose();
        createContinuousPipeline();
    }
    session.continuousCapture.start();
}

async function captureFrame(expectedDirection = 0, generation = session.captureGeneration, metadata = {}) {
    if (!captureStillActive(generation)) return {reason: 'aborted'};
    const task = session.continuousCapture.sample({...metadata, expectedDirection});
    session.captureInFlight = task;
    try { return await task; }
    finally { if (session.captureInFlight === task) session.captureInFlight = null; }
}
/**
 * 合成所有帧为一张长图 ImageData。
 * 使用 SAD 模板匹配去重叠。
 *
 * @returns {ImageData|null} 合成后的长图
 */
async function compositeLongImage() {
    return session.trackingClient?.compose() || null;
}

/**
 * 进入长图编辑阶段。
 * 合成所有帧 → 进入标注模式（复用 annot.reset）。
 *
 * 需要调用来源无关的 `enterCanvasImageEditor`（index.js 提供）。
 */
export async function enterScrollEdit() {
    return runScrollOutput(enterScrollEditOnce);
}

async function finishCapture() {
    const lifecycleId = session.lifecycleId;
    const startedAt = performance.now();
    recordScrollEvent(session, 'finalize-start');
    if (session.continuousCapture?.closed || session.trackingClient?.closed) {
        session.continuousCapture?.dispose();
        session.trackingClient?.dispose();
        createContinuousPipeline();
    }
    const result = await finalizeScrollCapture(session, {
        stopCapture: () => session.continuousCapture.pauseAndDrain(),
        waitForSettle: (generation) => waitForVisualSettle(session, generation),
        captureFrame,
        delay,
    });
    if (session.lifecycleId !== lifecycleId) return false;
    recordScrollEvent(session, 'finalize-end', {
        ok: result.ok, reason: result.reason || null, durationMs: roundedMs(performance.now() - startedAt),
        reusedStability: result.reusedStability === true,
    });
    const btn = document.getElementById('scroll-auto');
    if (result.reason !== 'aborted' && btn) {
        btn.classList.remove('active');
        btn.title = '自动滚动';
    }
    if (result.truncated) ss._showTransientHint?.('已达长截图上限，本次输出仅包含已确认内容');
    if (!result.ok && result.reason !== 'aborted') {
        console.warn('[scroll] 尾段未确认，保留采集会话', {reason: result.reason});
        ss._showTransientHint?.('末段尚未确认，已保留长图；请停稳或滚回已捕获区域后重试完成');
        // 超时可能仍有在途 Worker。终止旧实例，从已确认像素恢复，防止迟到提交。
        session.continuousCapture.dispose();
        session.trackingClient.dispose();
        resumeContinuousCapture();
    }
    return result.ok;
}

async function runScrollOutput(action) {
    if (session.outputBusy) return;
    if (session.scrollCapturePhase === 'capturing'
        && (!session.continuousCapture || session.scrollFrames.length === 0)) return;
    const lifecycleId = session.lifecycleId;
    session.outputBusy = true;
    try {
        return await action();
    } catch (error) {
        if (session.lifecycleId === lifecycleId && session.scrollCapturePhase === 'finalizing') {
            session.scrollCapturePhase = 'capturing';
            session.captureFinalizing = false;
            resumeContinuousCapture();
        }
        throw error;
    } finally {
        if (session.lifecycleId === lifecycleId) session.outputBusy = false;
    }
}

async function enterScrollEditOnce() {
    if (session.scrollCapturePhase !== 'capturing') return;
    if (session.scrollFrames.length === 0) return;

    if (!await finishCapture()) return;
    const lifecycleId = session.lifecycleId;
    const longImage = await compositeLongImage();
    if (session.lifecycleId !== lifecycleId) return;
    if (!longImage) {
        console.warn('[scroll] compositeLongImage returned null');
        session.scrollCapturePhase = 'capturing';
        session.captureFinalizing = false;
        resumeContinuousCapture();
        return;
    }

    // 编辑入口同步取得自有底图；成功前保留已确认片段和捕获排除，失败仍可重试收尾。
    if (typeof ss._enterCanvasImageEditor !== 'function') throw new Error('长图编辑入口尚未就绪');
    ss._enterCanvasImageEditor(longImage, longImage.width, longImage.height, IMAGE_SOURCE.LONG_SCREENSHOT, session.scrollSourceMonitor);
    if (session.lifecycleId !== lifecycleId) return;
    session.scrollCapturePhase = 'editing';
    session.captureFinalizing = false;
    const releasedFrameCount = session.scrollFrames.length;
    const releasedReplayBytes = session.scrollReplayBytes;
    session.releaseCaptureResources();
    setScrollCapturePresentation(false);
    resetPreviewRendering();

    // 切回默认工具栏
    const scrollTb = document.getElementById('scroll-toolbar');
    if (scrollTb) scrollTb.classList.add('hidden');
    if (ss.toolbar) ss.toolbar.classList.remove('hidden');

    // 隐藏预览
    if (ss.scrollPreviewCanvas) ss.scrollPreviewCanvas.classList.add('hidden');
    document.getElementById('scroll-diagnostics')?.classList.add('hidden');
    hideCaptureFrame();

    try {
        await screenshotSetCaptureExclusion(false, session.scrollInputToken);
    } catch (e) {
        console.warn('[scroll] 清除捕获排除失败', e);
    }
    if (session.lifecycleId !== lifecycleId) return;
    console.info('[scroll] enter editing', {
        w: longImage.width,
        h: longImage.height,
        sourceMonitor: session.scrollSourceMonitor,
        releasedFrameCount, releasedReplayBytes,
    });
}

/**
 * 退出长截图模式（取消或输出后调）。
 * 清理状态，隐藏专属工具栏和预览。
 */
export async function exitScrollCapture(restoreSelection = true) {
    console.debug('[scroll] exit capture', {
        restoreSelection,
        phase: session.scrollCapturePhase,
        frameCount: session.scrollFrames?.length || 0,
    });
    const sourceRect = session.scrollSourceRect ? {...session.scrollSourceRect} : null;
    const inputToken = session.scrollInputToken;
    resetScrollCaptureState();
    const cleanupGeneration = session.captureGeneration;

    // 清除捕获排除
    try {
        await screenshotSetCaptureExclusion(false, inputToken);
    } catch (e) {
        console.warn('[scroll] 清除捕获排除失败', e);
    }

    if (restoreSelection && sourceRect && session.captureGeneration === cleanupGeneration
        && typeof ss._enterAnnotationMode === 'function') {
        if (ss.screenshot && ss.canvas) {
            ss.canvas.width = ss.screenshot.width;
            ss.canvas.height = ss.screenshot.height;
            ss.canvas.style.left = '';
            ss.canvas.style.top = '';
            ss.canvas.style.width = '';
            ss.canvas.style.height = '';
        }
        ss._enterAnnotationMode(sourceRect);
    }
}

function resetScrollCaptureState() {
    session.reset();
    setScrollCapturePresentation(false);
    resetPreviewRendering();
    ss.editorSession.reset();
    ss._imagePan = null;
    ss.canvas?.classList.remove('long-image-editing');

    document.getElementById('scroll-toolbar')?.classList.add('hidden');
    document.getElementById('scroll-diagnostics')?.classList.add('hidden');
    const autoButton = document.getElementById('scroll-auto');
    autoButton?.classList.remove('active');
    if (autoButton) autoButton.title = '自动滚动';
    ss.scrollPreviewCanvas?.classList.add('hidden');
    hideCaptureFrame();
    if (ss.canvas) ss.canvas.style.pointerEvents = '';
    if (ss.hitCanvas) ss.hitCanvas.style.pointerEvents = '';
}

/** overlay 重载使用的同步失效入口；平台排除状态异步尽力清除。 */
export function resetScrollCaptureSession() {
    const inputToken = session.scrollInputToken;
    resetScrollCaptureState();
    if (inputToken == null) return;
    screenshotSetCaptureExclusion(false, inputToken)
        .catch((error) => console.warn('[scroll] 重置时清除捕获排除失败', error));
}

export function isScrollCaptureActive() {
    // 0.23.19：_imagePan 在**所有** canvas 编辑器（长图/剪贴板/历史/pin）都会被
    // enterCanvasImageEditor 设置（图片平移用），不能据此判定"长截图会话活跃"——
    // 否则普通图片编辑器里 ESC 首按会被"退出长截图"分支吃掉（exitScrollCapture
    // 移除 long-image-editing 蓝色描边 + reset editorSession，编辑器却留在桌面，
    // 用户须按第二次 ESC 才真正取消）。长图编辑态改用 source 判定
    // （0.23.19-fix：判定抽至 ss-editor-policy.isLongScreenshotPan，行为测试锁定）。
    if (session.active) return true;
    return isLongScreenshotPan(ss._imagePan, ss.editorSession.source);
}

/**
 * 是否正处于逐帧采集阶段。
 *
 * 长截图会把滚轮交给底层窗口，overlay 因此可能正常失焦；只有 capturing
 * 需要忽略这类 blur。editing/finalizing 继续沿用普通截图的失焦退出语义。
 */
export function isScrollCapturing() {
    return session.capturing;
}

/**
 * 控件及原生穿透失败时的手动滚轮兜底；选区内部真实输入直接到达底层窗口。
 * 图像仍由独立连续采集队列观察，不等待此转发路径。
 *
 * @param {WheelEvent} e
 */
export function onScrollWheel(e) {
    if (session.scrollCapturePhase !== 'capturing' || session.captureFinalizing) return;
    if (!session.scrollFrames.length) return;
    // 自动滚动模式下忽略手动 wheel
    if (session.autoScroll) return;
    if (!session.scrollHwnd) {
        ss._showTransientHint?.('未找到可滚动窗口');
        return;
    }

    const modeScale = e.deltaMode === 1 ? 40 : (e.deltaMode === 2 ? session.scrollBandH : 1);
    const rawDelta = e.deltaY * modeScale;
    if (rawDelta === 0) return;
    // 仅处理仍被 overlay 截获的滚轮；选区内部不经过 JS 或补发。
    const forwardedMagnitude = Math.max(1, Math.min(480, Math.round(Math.abs(rawDelta))));
    const delta = rawDelta > 0 ? -forwardedMagnitude : forwardedMagnitude;
    session.scrollPendingDirection = rawDelta > 0 ? 1 : -1;
    session.scrollWheelStartedAtMs ??= performance.now();
    session.scrollLastWheelAtMs = performance.now();
    session.manualWheelVersion++;
    recordScrollEvent(session, 'wheel', {
        wheelVersion: session.manualWheelVersion, direction: session.scrollPendingDirection,
    });
    // 统一使用 renderScale 换算
    const meta = window.__blinkScreenMeta || {vx: 0, vy: 0};
    const cursorPos = cssPointToScreen(e.clientX, e.clientY, meta);
    const cursorScreenX = cursorPos.x;
    const cursorScreenY = cursorPos.y;

    queueManualWheel(session, delta, cursorScreenX, cursorScreenY);
    showScrollInputFeedback(session.scrollPendingDirection);

    session.continuousCapture?.wake();
}

export function toggleScrollDirection() {
    session.scrollDirection = 'vertical';
    ss._showTransientHint?.('当前版本先保障纵向长截图，横向滚动暂未开放');
}

/**
 * 切换自动滚动模式。
 * 单生产者依次注入滚轮、等待画面并采集；无变化时再使用窗口消息兜底。
 */
export async function toggleAutoScroll() {
    if (session.captureFinalizing || session.scrollCapturePhase !== 'capturing') return;
    if (!session.scrollFrames.length) return;
    if (session.autoScroll) {
        await stopAutoScroll();
        return;
    }
    if (!session.scrollHwnd) {
        ss._showTransientHint?.('未找到可滚动窗口');
        return;
    }
    session.autoScroll = true;
    const generation = ++session.captureGeneration;
    session.autoWheelDelta = -120;
    session.autoLowConfidenceCount = 0;
    const btn = document.getElementById('scroll-auto');
    if (btn) {
        btn.classList.toggle('active', session.autoScroll);
        btn.title = session.autoScroll ? '自动滚动中（点击停止）' : '自动滚动';
    }

    let wheelTop = session.scrollCurrentTop;
    const task = runAutoScrollController({
        generation,
        session,
        isActive: captureStillActive,
        waitForSettle: async (gen, requireAuto) => {
            await delay(33);
            return {stable: true, mode: 'continuous', aborted: !captureStillActive(gen, requireAuto)};
        },
        captureFrame: async (...args) => {
            const result = await captureFrame(...args);
            // 持续采样可能已提交本次 wheel 的运动；强制复核的 unchanged 不等于没有滚动。
            if (captureStillActive(generation, true) && result.reason === 'unchanged'
                && session.scrollCurrentTop !== wheelTop) {
                return {...session.lastMovementResult, positionShift: session.scrollCurrentTop - wheelTop};
            }
            return result;
        },
        forwardWheel: async (positionCursor, forceMessage) => {
            // 注入前先排空积压，降低自动输入速率，同时保留所有中间帧。
            const pipeline = session.continuousCapture;
            await pipeline.pauseAndDrain();
            if (!captureStillActive(generation, true)) return;
            wheelTop = session.scrollCurrentTop;
            await forwardAutoWheel(session, positionCursor, forceMessage);
            if (captureStillActive(generation, true)) pipeline.start();
        },
        previewWheel: showScrollInputFeedback,
        stop: (reason) => {
            if (captureStillActive(generation)) return stopAutoScroll(reason);
        },
        delay,
    }).finally(() => {
        if (session.autoScrollTask === task) session.autoScrollTask = null;
    });
    session.autoScrollTask = task;
}

async function stopAutoScroll(reason = null) {
    session.autoScroll = false;
    session.invalidate();
    // 旧任务的 finally 不能跨代清理。暂停方释放调度标记，实际抓帧仍由队列串行化。
    session._scrollCapturing = false;
    if (!session.captureFinalizing) session.continuousCapture?.start();
    const btn = document.getElementById('scroll-auto');
    if (btn) {
        btn.classList.remove('active');
        btn.title = '自动滚动';
    }
    if (reason) ss._showTransientHint?.(reason);
}

/**
 * 输出长图（pin/保存/复制）。
 * 如果在 editing 阶段，先合成最终图。
 *
 * @param {string} action - 'pin' | 'save' | 'copy'
 */
export async function outputLongImage(action) {
    return runScrollOutput(() => outputLongImageOnce(action));
}

async function outputLongImageOnce(action) {
    if (session.scrollCapturePhase === 'capturing') {
        if (!await finishCapture()) return;
    }
    if (!['finalizing', 'editing'].includes(session.scrollCapturePhase)) return;
    const lifecycleId = session.lifecycleId;
    const screenX = session.scrollBandX;
    const screenY = session.scrollBandY;
    // 从标注引擎获取编辑后的图（如果在 editing 阶段）
    // 否则直接合成 scrollFrames
    let pngData = null;

    if (session.scrollCapturePhase === 'editing') {
        if (typeof ss._compositeSelection !== 'function') throw new Error('长图编辑输出尚未就绪');
        pngData = await ss._compositeSelection();
    } else {
        const longImage = await compositeLongImage();
        if (session.lifecycleId !== lifecycleId) return;
        if (!longImage) {
            if (session.scrollCapturePhase === 'finalizing') session.scrollCapturePhase = 'capturing';
            session.captureFinalizing = false;
            resumeContinuousCapture();
            return;
        }
        pngData = await encodeImageDataPng(longImage);
    }

    if (session.lifecycleId !== lifecycleId) return;
    if (!pngData) {
        if (session.scrollCapturePhase === 'finalizing') session.scrollCapturePhase = 'capturing';
        session.captureFinalizing = false;
        if (session.scrollCapturePhase === 'capturing') resumeContinuousCapture();
        return;
    }

    try {
        await outputScreenshotPng(action, pngData, screenX, screenY);
    } catch (error) {
        if (session.lifecycleId !== lifecycleId) return;
        if (session.scrollCapturePhase === 'finalizing') session.scrollCapturePhase = 'capturing';
        session.captureFinalizing = false;
        if (session.scrollCapturePhase === 'capturing') resumeContinuousCapture();
        throw error;
    }
    if (session.lifecycleId !== lifecycleId) return;
    await exitScrollCapture(false);
}

/**
 * 绑定长截图专属工具栏事件。
 * 在 index.js initDOM 后调用。
 */
export function bindScrollToolbar() {
    bindScrollDiagnostics(session, ss._showTransientHint);
    document.getElementById('scroll-direction')?.addEventListener('click', toggleScrollDirection);
    document.getElementById('scroll-auto')?.addEventListener('click', () => toggleAutoScroll());
    document.getElementById('scroll-edit')?.addEventListener('click', () => enterScrollEdit());
    document.getElementById('scroll-pin')?.addEventListener('click', () => outputLongImage('pin'));
    document.getElementById('scroll-save')?.addEventListener('click', () => outputLongImage('save'));
    document.getElementById('scroll-copy')?.addEventListener('click', () => outputLongImage('copy'));
    document.getElementById('scroll-cancel')?.addEventListener('click', async () => {
        await exitScrollCapture(false);
        screenshotCancel();
    });
}

// 注册唯一生命周期入口；输出模块通过 session 请求退出，无需反向 import。
session.exitHandler = exitScrollCapture;
