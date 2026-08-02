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
//! - overlay 接收首个 wheel 事件 → 短时穿透，后续真实输入直达目标窗口
//! - 等待滚动停稳 → 调 `screenshot_capture_band` 采集一帧
//!
//! **自动滚动**：
//! - 前端每轮只注入一次滚轮，低分辨率探针确认画面稳定后才采集全帧
//! - 当前帧完成配准前不开始下一轮；无法配准立即暂停并保留已确认内容
//!
//! **帧拼接算法**（双向 SAD 模板匹配）：
//! 1. 同时估算上/下位移，滚轮方向只用于重复纹理的优先级
//! 2. 记录当前视口在长图中的绝对 top
//! 3. 仅保存新暴露的顶部/底部行；回滚到已有区域不重复追加

import { ss } from '../ss-state.js';
import {
  screenshotCancel, screenshotSetCaptureExclusion,
} from '../../shared/api.js';
import { findWindowForRect } from '../ss-hover.js';
import {
  commitTrackedFrame, compositePositionedFrames,
} from './stitch.js';
import { rememberScrollKeyframe as retainScrollKeyframe, trackScrollFrame } from './tracker.js';
import { runAutoScrollController } from './auto.js';
import {
  captureBandFrame, delay, forwardAutoWheel, MANUAL_WHEEL_DEBOUNCE_MS,
  queueManualWheel, waitForVisualSettle,
} from './capture-driver.js';
import {
  bindScrollDiagnostics, recordScrollDiagnostic, resetScrollDiagnostics,
} from './diagnostics.js';
import { encodeImageDataPng, outputScreenshotPng } from '../ss-output.js';
import {
  hideCaptureFrame, positionPreview, SCROLL_PREVIEW_GAP, showCaptureFrame, updatePreview,
} from './preview.js';

const session = ss.scrollSession;

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
  const generation = session.invalidate();
  session.manualWheelVersion = 0;
  session.autoWheelDelta = -120;
  session.autoLowConfidenceCount = 0;
  session.queuedManualWheel = null;
  session.captureFinalizing = false;
  const dpr = window.devicePixelRatio || 1;
  const meta = window.__blinkScreenMeta || { vx: 0, vy: 0 };

  // 记录采集带几何（物理像素，虚拟屏幕坐标系）
  session.scrollBandX = meta.vx + Math.round(rect.x * dpr);
  session.scrollBandY = meta.vy + Math.round(rect.y * dpr);
  session.scrollBandW = Math.round(rect.w * dpr);
  session.scrollBandH = Math.round(rect.h * dpr);
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
  resetScrollDiagnostics(session);

  // 优先锁定框选区域实际覆盖的外部窗口；兜底使用 Blink 唤起前保存的前台窗口。
  const pickedWindow = findWindowForRect(rect);
  session.scrollHwnd = pickedWindow?.hwnd || meta.fgHwnd || null;
  session.scrollTargetX = pickedWindow
    ? meta.vx + Math.round(pickedWindow.targetX * dpr)
    : session.scrollBandX + Math.floor(session.scrollBandW / 2);
  session.scrollTargetY = pickedWindow
    ? meta.vy + Math.round(pickedWindow.targetY * dpr)
    : session.scrollBandY + Math.floor(session.scrollBandH / 2);
  session.scrollSourceRect = { ...rect };

  // 设置 WDA_EXCLUDEFROMCAPTURE：overlay 在 BitBlt 中不可见
  try {
    await screenshotSetCaptureExclusion(true);
  } catch (e) {
    if (!captureStillActive(generation)) return false;
    console.warn('[scroll] 设置捕获排除失败，已中止长截图', e);
    resetScrollCaptureState();
    ss._showTransientHint?.('当前系统无法排除截图工具界面，长截图已取消');
    screenshotSetCaptureExclusion(false)
      .catch((cleanupError) => console.warn('[scroll] 捕获排除失败后的清理失败', cleanupError));
    return false;
  }
  // ESC、失焦、overlay 重载或新会话都可能发生在上面的 IPC 等待期间。
  // 旧入口不得继续修改新一代 canvas / toolbar 状态。
  if (!captureStillActive(generation)) return false;

  // 清除暗色蒙版，让用户看到真实背景。DOM pointer-events 不等于 Win32 窗口穿透；
  // wheel 仍由 overlay window listener 接收，再显式投递给目标 HWND。
  if (ss.ctx && ss.canvas) {
    ss.ctx.clearRect(0, 0, ss.canvas.width, ss.canvas.height);
    ss.canvas.style.pointerEvents = 'none';
  }
  if (ss.annotCanvas) ss.annotCanvas.classList.add('hidden');
  // 隐藏 OCR hit canvas 和像素放大镜
  if (ss.hitCanvas) ss.hitCanvas.style.pointerEvents = 'none';
  showCaptureFrame(rect);

  // 切换工具栏：隐藏默认，显示专属
  if (ss.toolbar) ss.toolbar.classList.add('hidden');
  const scrollTb = document.getElementById('scroll-toolbar');
  if (scrollTb) {
    scrollTb.classList.remove('hidden');
    // 定位到选区下方
    scrollTb.style.left = (rect.x + rect.w / 2 - 100) + 'px';
    scrollTb.style.top = (rect.y + rect.h + SCROLL_PREVIEW_GAP) + 'px';
  }

  // 显示预览缩略图——贴选区侧边
  // 纵向：贴右边；横向：贴下边
  positionPreview(rect);

  // 截取第一帧
  await captureFrame(0, generation);
  if (!captureStillActive(generation)) return false;
  if (!session.scrollHwnd) {
    ss._showTransientHint?.('未找到可滚动窗口，请重新框选目标窗口内的区域');
  }
  console.info('[scroll] enter capturing', {
    bandW: session.scrollBandW,
    bandH: session.scrollBandH,
    hwnd: session.scrollHwnd,
    target: pickedWindow?.processName || pickedWindow?.title || 'fallback',
  });
  return true;
}

/**
 * 截取当前帧并拼接到 scrollFrames。
 * 每次滚动后调用（手动 wheel 或自动滚动回调）。
 */
async function captureFrame(expectedDirection = 0, generation = session.captureGeneration, metadata = {}) {
  const task = captureFrameOnce(expectedDirection, generation, metadata);
  session.captureInFlight = task;
  try {
    return await task;
  } finally {
    if (session.captureInFlight === task) session.captureInFlight = null;
  }
}

async function captureFrameOnce(expectedDirection = 0, generation = session.captureGeneration, metadata = {}) {
  if (!captureStillActive(generation)) return { appended: false, reason: 'inactive' };

  const w = session.scrollBandW;
  const h = session.scrollBandH;
  if (w <= 0 || h <= 0) return { appended: false, reason: 'invalid-band' };

  try {
    // 调用 Rust 端 fresh BitBlt 采集（WDA_EXCLUDEFROMCAPTURE 已设，overlay 不可见）
    const captured = await captureBandFrame(session);
    if (!captureStillActive(generation)) return { appended: false, reason: 'aborted' };
    if (!captured.frame) {
      console.warn('[scroll] captureBand 返回数据不足', {
        expected: captured.expected,
        got: captured.got,
      });
      return { appended: false, reason: captured.reason };
    }
    const frame = captured.frame;
    const tracked = trackScrollFrame({
      frames: session.scrollFrames,
      keyframes: session.scrollKeyframes,
      lastFrame: session.scrollLastFrame,
      currentTop: session.scrollCurrentTop,
      trackingState: session.scrollTrackingState,
      pendingJump: session.scrollPendingJump,
      lostFrameCount: session.scrollLostFrameCount,
    }, frame, {
      expectedDirection,
      motionTimedOut: metadata.settle?.timedOut === true,
    });
    const { decision, match, relocalized, placement } = tracked;
    session.scrollPendingJump = tracked.pendingJump;
    recordScrollDiagnostic(session, frame, decision, metadata);
    if (!decision.accepted) {
      const rejectedRecovery = decision.reason === 'low-confidence'
        && decision.source !== 'adjacent';
      if (match.status === 'no-match' || rejectedRecovery) {
        session.scrollTrackingState = 'lost';
      }
      if (match.status === 'no-match' || rejectedRecovery
          || decision.reason === 'pending-confirmation') {
        session.scrollLostFrameCount = (session.scrollLostFrameCount || 0) + 1;
      }
      session.scrollUnchangedCount++;
      updatePreview();
      console.debug('[scroll] frame ignored', {
        reason: decision.reason,
        source: decision.source,
        score: decision.bestScore,
        unchangedCount: session.scrollUnchangedCount,
      });
      return { appended: false, reason: decision.reason, match, decision };
    }

    const previousTop = session.scrollCurrentTop;
    session.scrollCurrentTop = tracked.nextTop;
    const committed = commitTrackedFrame(
      session.scrollFrames,
      session.scrollLastFrame,
      frame,
      tracked,
    );
    session.scrollFrames = committed.frames;
    const addedRows = committed.addedRows;
    const extendsRange = addedRows > 0;
    session.scrollTrackingState = 'tracking';
    session.scrollLostFrameCount = 0;
    session.scrollLastFrame = frame;
    rememberScrollKeyframe(committed.committedFrame, session.scrollCurrentTop);
    session.scrollUnchangedCount = 0;
    updatePreview();
    console.debug('[scroll] movement captured', {
      frameCount: session.scrollFrames.length,
      shift: session.scrollCurrentTop - previousTop,
      matchShift: match?.shift ?? 0,
      currentTop: session.scrollCurrentTop,
      extendsRange,
      score: decision.bestScore,
      relocalized: relocalized?.scope || false,
      placement: placement.edge,
      fixedTiles: committed.fixedTileCount,
    });
    return {
      appended: extendsRange,
      moved: true,
      first: decision.reason === 'first-frame',
      addedRows,
      match,
      decision,
      positionShift: session.scrollCurrentTop - previousTop,
      relocalized: relocalized?.scope,
    };
  } catch (e) {
    console.warn('[scroll] captureFrame 失败', e);
    return { appended: false, reason: 'capture-error', error: e };
  }
}

function rememberScrollKeyframe(frame, top) {
  session.scrollKeyframes = retainScrollKeyframe(session.scrollKeyframes, frame, top);
}

/**
 * 合成所有帧为一张长图 ImageData。
 * 使用 SAD 模板匹配去重叠。
 *
 * @returns {ImageData|null} 合成后的长图
 */
function compositeLongImage() {
  return compositePositionedFrames(session.scrollFrames)?.image || null;
}

/**
 * 进入长图编辑阶段。
 * 合成所有帧 → 进入标注模式（复用 annot.reset）。
 *
 * 需要调用 `enterAnnotationWithCropData`（index.js 提供）。
 */
export async function enterScrollEdit() {
  if (session.scrollCapturePhase !== 'capturing') return;
  if (session.scrollFrames.length === 0) return;

  // 先停止自动滚动（如果在运行）
  if (session.autoScroll) {
    await stopAutoScroll();
  }
  // 先阻止新 wheel。若已经进入最终 BitBlt，则让该帧完整提交；尚在防抖/稳定
  // 等待的任务随后通过代际失效，避免编辑快照与预览确认交叉。
  session.captureFinalizing = true;
  if (session._scrollCaptureTimer) {
    clearTimeout(session._scrollCaptureTimer);
    session._scrollCaptureTimer = 0;
  }
  session.queuedManualWheel = null;
  const inFlight = session.captureInFlight;
  if (inFlight) await inFlight.catch(() => {});
  session.invalidate();
  session.scrollCapturePhase = 'finalizing';

  session.scrollCapturePhase = 'editing';
  session.captureFinalizing = false;
  hideCaptureFrame();
  const longImage = compositeLongImage();
  if (!longImage) {
    console.warn('[scroll] compositeLongImage returned null');
    return;
  }

  // 清除捕获排除
  try {
    await screenshotSetCaptureExclusion(false);
  } catch (e) {
    console.warn('[scroll] 清除捕获排除失败', e);
  }

  // 切回默认工具栏
  const scrollTb = document.getElementById('scroll-toolbar');
  if (scrollTb) scrollTb.classList.add('hidden');
  if (ss.toolbar) ss.toolbar.classList.remove('hidden');

  // 隐藏预览
  if (ss.scrollPreviewCanvas) ss.scrollPreviewCanvas.classList.add('hidden');
  hideCaptureFrame();

  // 调用 index.js 提供的 enterAnnotationWithCropData 回调
  if (ss._enterAnnotationWithCropData) {
    ss._enterAnnotationWithCropData(longImage, longImage.width, longImage.height);
  }
  console.info('[scroll] enter editing', { w: longImage.width, h: longImage.height });
}

/**
 * 退出长截图模式（取消或输出后调）。
 * 清理状态，隐藏专属工具栏和预览。
 */
export async function exitScrollCapture(restoreSelection = true) {
  const sourceRect = session.scrollSourceRect ? { ...session.scrollSourceRect } : null;
  resetScrollCaptureState();
  const cleanupGeneration = session.captureGeneration;

  // 清除捕获排除
  try {
    await screenshotSetCaptureExclusion(false);
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
  ss._longImageBaseCanvas = null;
  ss._longImagePan = null;
  ss.canvas?.classList.remove('long-image-editing');

  document.getElementById('scroll-toolbar')?.classList.add('hidden');
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
  resetScrollCaptureState();
  screenshotSetCaptureExclusion(false)
    .catch((error) => console.warn('[scroll] 重置时清除捕获排除失败', error));
}

export function isScrollCaptureActive() {
  return session.active || !!ss._longImagePan;
}

/**
 * 手动滚动检测：wheel 事件 → 转发给目标窗口 → 等待 → 采集。
 * 由 index.js 在 capturing 阶段绑定时调用。
 *
 * @param {WheelEvent} e
 */
export function onScrollWheel(e) {
  if (session.scrollCapturePhase !== 'capturing' || session.captureFinalizing) return;
  // 自动滚动模式下忽略手动 wheel
  if (session.autoScroll) return;
  if (!session.scrollHwnd) {
    ss._showTransientHint?.('未找到可滚动窗口');
    return;
  }

  const modeScale = e.deltaMode === 1 ? 40 : (e.deltaMode === 2 ? session.scrollBandH : 1);
  const rawDelta = e.deltaY * modeScale;
  if (rawDelta === 0) return;
  // 首个滚轮由 SendInput 补发，随后的真实输入在短时窗口穿透期内直达底层应用。
  const forwardedMagnitude = Math.max(1, Math.min(480, Math.round(Math.abs(rawDelta))));
  const delta = rawDelta > 0 ? -forwardedMagnitude : forwardedMagnitude;
  session.scrollPendingDirection = rawDelta > 0 ? 1 : -1;
  session.manualWheelVersion++;
  const dpr = window.devicePixelRatio || 1;
  const meta = window.__blinkScreenMeta || { vx: 0, vy: 0 };
  const cursorScreenX = Math.round(meta.vx + e.clientX * dpr);
  const cursorScreenY = Math.round(meta.vy + e.clientY * dpr);

  queueManualWheel(session, delta, cursorScreenX, cursorScreenY);

  scheduleManualCapture();
}

function scheduleManualCapture() {
  if (session._scrollCaptureTimer) clearTimeout(session._scrollCaptureTimer);
  session._scrollCaptureTimer = setTimeout(runSettledManualCapture, MANUAL_WHEEL_DEBOUNCE_MS);
}

async function runSettledManualCapture() {
  session._scrollCaptureTimer = 0;
  if (session.scrollCapturePhase !== 'capturing' || session.autoScroll) return;
  if (session._scrollCapturing) {
    scheduleManualCapture();
    return;
  }
  const generation = session.captureGeneration;
  session._scrollCapturing = true;
  let capturedWheelVersion = session.manualWheelVersion;
  let settled = null;
  try {
    do {
      capturedWheelVersion = session.manualWheelVersion;
      settled = await waitForVisualSettle(session, generation);
      if (settled.aborted) return;
    } while (capturedWheelVersion !== session.manualWheelVersion);
    if (!captureStillActive(generation) || session.autoScroll) return;
    const direction = session.scrollPendingDirection;
    let result = await captureFrame(direction, generation, { settle: settled });
    if (result.reason === 'pending-confirmation'
        && capturedWheelVersion === session.manualWheelVersion
        && captureStillActive(generation)) {
      const confirmationSettle = await waitForVisualSettle(session, generation);
      if (confirmationSettle.aborted) return;
      result = await captureFrame(0, generation, { settle: confirmationSettle });
    }
    session.scrollPendingDirection = 0;
    if (['ambiguous', 'low-confidence', 'no-overlap', 'motion-timeout'].includes(result.reason)) {
      ss._showTransientHint?.(result.reason === 'ambiguous'
        ? '页面存在重复内容，暂时无法唯一定位；请缓慢滚回最近已捕获区域'
        : '暂未找到可靠重叠；请缓慢滚回最近已捕获区域后继续');
    } else if (result.reason === 'pending-confirmation') {
      ss._showTransientHint?.('远距离定位尚未通过连续确认，请保持画面稳定后重试');
    } else if (result.relocalized && !result.appended) {
      ss._showTransientHint?.('已恢复到已捕获区域；越过长图边界后才会新增内容');
    } else if (!result.moved && session.scrollUnchangedCount >= 2) {
      ss._showTransientHint?.('画面没有发生变化，请确认框选中心位于可滚动内容区域');
    }
  } finally {
    session._scrollCapturing = false;
    if (captureStillActive(generation) && !session.autoScroll
        && capturedWheelVersion !== session.manualWheelVersion) {
      scheduleManualCapture();
    }
  }
}

/**
 * 切换滚动方向（纵向/横向）。
 */
export function toggleScrollDirection() {
  session.scrollDirection = 'vertical';
  ss._showTransientHint?.('当前版本先保障纵向长截图，横向滚动暂未开放');
}

/**
 * 切换自动滚动模式。
 * 开启时调 Rust 端 PostMessage 驱动滚动 + 监听 tick 事件。
 */
export async function toggleAutoScroll() {
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

  void runAutoScrollController({
    generation,
    session,
    isActive: captureStillActive,
    waitForSettle: (gen, requireAuto) => waitForVisualSettle(session, gen, requireAuto),
    captureFrame,
    forwardWheel: (positionCursor, forceMessage) => (
      forwardAutoWheel(session, positionCursor, forceMessage)
    ),
    stop: stopAutoScroll,
    delay,
  });
}

async function stopAutoScroll(reason = null) {
  session.autoScroll = false;
  session.invalidate();
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
  if (session.scrollCapturePhase === 'capturing') {
    if (session.autoScroll) await stopAutoScroll();
    session.captureFinalizing = true;
    if (session._scrollCaptureTimer) {
      clearTimeout(session._scrollCaptureTimer);
      session._scrollCaptureTimer = 0;
    }
    session.queuedManualWheel = null;
    const inFlight = session.captureInFlight;
    if (inFlight) await inFlight.catch(() => {});
    session.invalidate();
    session.scrollCapturePhase = 'finalizing';
  }
  // 从标注引擎获取编辑后的图（如果在 editing 阶段）
  // 否则直接合成 scrollFrames
  let pngData = null;

  if (session.scrollCapturePhase === 'editing' && ss._compositeSelection) {
    pngData = await ss._compositeSelection();
  } else {
    const longImage = compositeLongImage();
    if (!longImage) {
      if (session.scrollCapturePhase === 'finalizing') session.scrollCapturePhase = 'capturing';
      session.captureFinalizing = false;
      return;
    }
    pngData = await encodeImageDataPng(longImage);
  }

  if (!pngData) {
    if (session.scrollCapturePhase === 'finalizing') session.scrollCapturePhase = 'capturing';
    session.captureFinalizing = false;
    return;
  }

  const screenX = session.scrollBandX;
  const screenY = session.scrollBandY;

  try {
    await outputScreenshotPng(action, pngData, screenX, screenY);
  } catch (error) {
    if (session.scrollCapturePhase === 'finalizing') session.scrollCapturePhase = 'capturing';
    session.captureFinalizing = false;
    throw error;
  }
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
