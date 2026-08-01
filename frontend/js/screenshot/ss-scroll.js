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

import { ss } from './ss-state.js';
import {
  screenshotCancel, screenshotSetCaptureExclusion, screenshotCaptureBand, screenshotCaptureProbe,
  screenshotForwardWheel,
} from '../shared/api.js';
import { findWindowForRect } from './ss-hover.js';
import {
  compositePositionedFrames, createGrayFingerprint, createVerticalReference,
  estimateVerticalShift, extractRows, planPositionedIncrement, positionedFrameBounds,
  relocalizeFromKeyframes,
} from './ss-scroll-stitch.js';
import { isProbeStable } from './ss-scroll-stability.js';
import { encodeImageDataPng, outputScreenshotPng } from './ss-output.js';
import {
  hideCaptureFrame, positionPreview, SCROLL_PREVIEW_GAP, showCaptureFrame, updatePreview,
} from './ss-scroll-preview.js';

const session = ss.scrollSession;

/** 稳定探针采样间隔与最长等待时间（ms）。 */
const SETTLE_PROBE_INTERVAL_MS = 45;
const SETTLE_MAX_WAIT_MS = 900;
const SETTLE_MIN_WAIT_MS = 180;
const SETTLE_STABLE_SAMPLE_COUNT = 2;
/** wheel 后让首个画面变化发生，再开始比较相邻探针。 */
const SETTLE_INITIAL_DELAY_MS = 35;
/** 手动滚轮事件合并窗口；真正的等待由稳定探针决定。 */
const MANUAL_WHEEL_DEBOUNCE_MS = 45;
/** 手动滚轮穿透窗口。保持较短，避免页面在前端不可观测时连续滑过多个视口。 */
const MANUAL_WHEEL_PASSTHROUGH_MS = 72;
/** 自动滚动只需短时穿透；下一轮必须等本轮采集确认。 */
const AUTO_WHEEL_PASSTHROUGH_MS = 24;
/** 自动滚动连续多少帧不变后判定到底或驱动失败。 */
const AUTO_UNCHANGED_LIMIT = 3;
/** 关键帧包含纵向逐像素参考，设硬上限控制极长页面内存。 */
const MAX_SCROLL_KEYFRAMES = 64;

function delay(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

function captureStillActive(generation, requireAuto = false) {
  return generation === session.captureGeneration
    && ss.scrollCapturePhase === 'capturing'
    && (!requireAuto || ss.autoScroll);
}

async function captureProbe() {
  const buffer = await screenshotCaptureProbe(
    ss.scrollBandX,
    ss.scrollBandY,
    ss.scrollBandW,
    ss.scrollBandH,
  );
  return buffer ? new Uint8Array(buffer) : null;
}

async function waitForVisualSettle(generation, requireAuto = false) {
  await delay(SETTLE_INITIAL_DELAY_MS);
  if (!captureStillActive(generation, requireAuto)) return { aborted: true };

  let previous;
  try {
    previous = await captureProbe();
  } catch (error) {
    console.warn('[scroll] 稳定探针首次采集失败，使用短延时兜底', error);
    await delay(180);
    return { stable: false, fallback: true };
  }

  const startedAt = performance.now();
  let stableSamples = 0;
  let lastScore = Infinity;
  while (performance.now() - startedAt < SETTLE_MAX_WAIT_MS) {
    await delay(SETTLE_PROBE_INTERVAL_MS);
    if (!captureStillActive(generation, requireAuto)) return { aborted: true };

    let current;
    try {
      current = await captureProbe();
    } catch (error) {
      console.warn('[scroll] 稳定探针采集失败，继续等待', error);
      stableSamples = 0;
      continue;
    }
    const motion = isProbeStable(previous, current);
    lastScore = motion.score;
    stableSamples = motion.stable ? stableSamples + 1 : 0;
    previous = current;
    if (stableSamples >= SETTLE_STABLE_SAMPLE_COUNT
        && performance.now() - startedAt >= SETTLE_MIN_WAIT_MS) {
      return { stable: true, score: lastScore };
    }
  }
  console.debug('[scroll] 稳定等待超时，交由全帧匹配确认', { score: lastScore });
  return { stable: false, timedOut: true, score: lastScore };
}

function queueManualWheel(delta, screenX, screenY) {
  if (session.queuedManualWheel) {
    session.queuedManualWheel.delta = Math.max(
      -480,
      Math.min(480, session.queuedManualWheel.delta + delta),
    );
    session.queuedManualWheel.screenX = screenX;
    session.queuedManualWheel.screenY = screenY;
  } else {
    session.queuedManualWheel = { delta, screenX, screenY };
  }
  if (!session.wheelForwardPending) void pumpManualWheel();
}

async function pumpManualWheel() {
  if (session.wheelForwardPending) return;
  session.wheelForwardPending = true;
  try {
    while (session.queuedManualWheel && ss.scrollCapturePhase === 'capturing' && !ss.autoScroll) {
      const wheel = session.queuedManualWheel;
      session.queuedManualWheel = null;
      await screenshotForwardWheel(
        ss.scrollHwnd,
        wheel.delta,
        wheel.screenX,
        wheel.screenY,
        MANUAL_WHEEL_PASSTHROUGH_MS,
      );
    }
  } catch (error) {
    console.warn('[scroll] wheel 转发失败', error);
  } finally {
    session.wheelForwardPending = false;
    if (session.queuedManualWheel && ss.scrollCapturePhase === 'capturing' && !ss.autoScroll) {
      void pumpManualWheel();
    }
  }
}

/**
 * 进入长截图采集阶段（capturing）。
 * 由 index.js 的【长截图】按钮调用。
 *
 * @param {object} rect - 框选矩形 CSS 坐标 {x, y, w, h}
 */
export async function enterScrollCapture(rect) {
  session.invalidate();
  session.manualWheelVersion = 0;
  session.autoWheelDelta = -120;
  session.queuedManualWheel = null;
  session.captureFinalizing = false;
  const dpr = window.devicePixelRatio || 1;
  const meta = window.__blinkScreenMeta || { vx: 0, vy: 0 };

  // 记录采集带几何（物理像素，虚拟屏幕坐标系）
  ss.scrollBandX = meta.vx + Math.round(rect.x * dpr);
  ss.scrollBandY = meta.vy + Math.round(rect.y * dpr);
  ss.scrollBandW = Math.round(rect.w * dpr);
  ss.scrollBandH = Math.round(rect.h * dpr);
  ss.scrollDirection = 'vertical';
  ss.autoScroll = false;
  ss.scrollFrames = [];
  ss.scrollLastFrame = null;
  ss.scrollKeyframes = [];
  ss.scrollTrackingState = 'tracking';
  ss.scrollCurrentTop = 0;
  ss.scrollPendingDirection = 0;
  ss.scrollUnchangedCount = 0;
  ss.scrollCapturePhase = 'capturing';

  // 优先锁定框选区域实际覆盖的外部窗口；兜底使用 Blink 唤起前保存的前台窗口。
  const pickedWindow = findWindowForRect(rect);
  ss.scrollHwnd = pickedWindow?.hwnd || meta.fgHwnd || null;
  ss.scrollTargetX = pickedWindow
    ? meta.vx + Math.round(pickedWindow.targetX * dpr)
    : ss.scrollBandX + Math.floor(ss.scrollBandW / 2);
  ss.scrollTargetY = pickedWindow
    ? meta.vy + Math.round(pickedWindow.targetY * dpr)
    : ss.scrollBandY + Math.floor(ss.scrollBandH / 2);
  ss.scrollSourceRect = { ...rect };

  // 设置 WDA_EXCLUDEFROMCAPTURE：overlay 在 BitBlt 中不可见
  try {
    await screenshotSetCaptureExclusion(true);
  } catch (e) {
    console.warn('[scroll] 设置捕获排除失败，BitBlt 可能拍到 overlay', e);
  }

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
  await captureFrame();
  if (!ss.scrollHwnd) {
    ss._showTransientHint?.('未找到可滚动窗口，请重新框选目标窗口内的区域');
  }
  console.info('[scroll] enter capturing', {
    bandW: ss.scrollBandW,
    bandH: ss.scrollBandH,
    hwnd: ss.scrollHwnd,
    target: pickedWindow?.processName || pickedWindow?.title || 'fallback',
  });
}

/**
 * 截取当前帧并拼接到 scrollFrames。
 * 每次滚动后调用（手动 wheel 或自动滚动回调）。
 */
async function captureFrame(expectedDirection = 0, generation = session.captureGeneration) {
  const task = captureFrameOnce(expectedDirection, generation);
  session.captureInFlight = task;
  try {
    return await task;
  } finally {
    if (session.captureInFlight === task) session.captureInFlight = null;
  }
}

async function captureFrameOnce(expectedDirection = 0, generation = session.captureGeneration) {
  if (!captureStillActive(generation)) return { appended: false, reason: 'inactive' };

  const w = ss.scrollBandW;
  const h = ss.scrollBandH;
  if (w <= 0 || h <= 0) return { appended: false, reason: 'invalid-band' };

  try {
    // 调用 Rust 端 fresh BitBlt 采集（WDA_EXCLUDEFROMCAPTURE 已设，overlay 不可见）
    const buffer = await screenshotCaptureBand(ss.scrollBandX, ss.scrollBandY, w, h);
    if (!captureStillActive(generation)) return { appended: false, reason: 'aborted' };
    if (!buffer || buffer.byteLength < w * h * 4) {
      console.warn('[scroll] captureBand 返回数据不足', { expected: w * h * 4, got: buffer?.byteLength });
      return { appended: false, reason: 'short-buffer' };
    }
    const rgba = new Uint8ClampedArray(buffer);
    const frame = new ImageData(rgba, w, h);
    if (!ss.scrollLastFrame) {
      ss.scrollFrames.push({ image: frame, top: 0 });
      ss.scrollLastFrame = frame;
      ss.scrollCurrentTop = 0;
      rememberScrollKeyframe(frame, 0);
      ss.scrollUnchangedCount = 0;
      updatePreview();
      console.debug('[scroll] first frame captured', { w, h });
      return { appended: true, addedRows: h, first: true };
    }

    const wasLost = ss.scrollTrackingState === 'lost';
    let match = wasLost
      ? { status: 'no-match', shift: 0, score: Infinity, reason: 'tracking-lost' }
      : estimateVerticalShift(ss.scrollLastFrame, frame, {
        expectedDirection,
        strictDirection: expectedDirection !== 0,
        rejectAmbiguous: true,
      });
    let nextTop = ss.scrollCurrentTop + match.shift;
    let relocalized = null;
    if (match.status === 'no-match') {
      relocalized = relocalizeFromKeyframes(
        ss.scrollFrames,
        ss.scrollKeyframes,
        frame,
        ss.scrollCurrentTop,
        expectedDirection,
        { trackingLost: wasLost },
      );
      if (relocalized) {
        match = relocalized.match;
        nextTop = relocalized.top;
      }
    }
    if ((match.status !== 'matched' && match.status !== 'unchanged')
        || (!relocalized && match.shift === 0)) {
      if (match.status === 'no-match') ss.scrollTrackingState = 'lost';
      ss.scrollUnchangedCount++;
      updatePreview();
      console.debug('[scroll] frame ignored', { ...match, unchangedCount: ss.scrollUnchangedCount });
      return { appended: false, reason: match.status, match };
    }

    const oldBounds = positionedFrameBounds(ss.scrollFrames);
    const previousTop = ss.scrollCurrentTop;
    ss.scrollCurrentTop = nextTop;
    const placement = planPositionedIncrement(oldBounds, ss.scrollCurrentTop, h);
    let addedRows = 0;
    if (placement.rowCount > 0) {
      const increment = extractRows(frame, placement.startRow, placement.rowCount);
      if (increment) {
        ss.scrollFrames.push({ image: increment, top: placement.targetTop });
        addedRows = increment.height;
      }
    }
    const extendsRange = addedRows > 0;
    ss.scrollTrackingState = 'tracking';
    ss.scrollLastFrame = frame;
    rememberScrollKeyframe(frame, ss.scrollCurrentTop);
    ss.scrollUnchangedCount = 0;
    updatePreview();
    console.debug('[scroll] movement captured', {
      frameCount: ss.scrollFrames.length,
      shift: ss.scrollCurrentTop - previousTop,
      matchShift: match.shift,
      currentTop: ss.scrollCurrentTop,
      extendsRange,
      score: match.score,
      relocalized: relocalized?.scope || false,
      placement: placement.edge,
    });
    return {
      appended: extendsRange,
      moved: true,
      addedRows,
      match,
      positionShift: ss.scrollCurrentTop - previousTop,
      relocalized: relocalized?.scope,
    };
  } catch (e) {
    console.warn('[scroll] captureFrame 失败', e);
    return { appended: false, reason: 'capture-error', error: e };
  }
}

function rememberScrollKeyframe(frame, top) {
  const existing = ss.scrollKeyframes.find((keyframe) => Math.abs(keyframe.top - top) <= 3);
  // 已有锚点的 probe 与当时写入 scrollFrames 的像素一致；回滚重访时不覆盖，
  // 否则动态内容可能让灰度召回和后续“已存原图复核”引用不同版本。
  if (existing) return;
  const probe = createGrayFingerprint(frame);
  const reference = createVerticalReference(frame);
  if (!probe || !reference) return;
  ss.scrollKeyframes.push({ top, probe, reference });
  if (ss.scrollKeyframes.length > MAX_SCROLL_KEYFRAMES) {
    // 按空间位置均匀抽样，而不是按滚动历史的插入顺序抽样。反复回滚时，插入
    // 顺序会聚集在局部，按其奇偶裁剪会意外丢掉远端锚点。
    const ordered = [...ss.scrollKeyframes].sort((a, b) => a.top - b.top);
    const thinned = [];
    for (let index = 0; index < MAX_SCROLL_KEYFRAMES; index++) {
      const sourceIndex = Math.round(index * (ordered.length - 1) / (MAX_SCROLL_KEYFRAMES - 1));
      const keyframe = ordered[sourceIndex];
      if (thinned.at(-1) !== keyframe) thinned.push(keyframe);
    }
    ss.scrollKeyframes = thinned;
  }
}

/**
 * 合成所有帧为一张长图 ImageData。
 * 使用 SAD 模板匹配去重叠。
 *
 * @returns {ImageData|null} 合成后的长图
 */
function compositeLongImage() {
  return compositePositionedFrames(ss.scrollFrames)?.image || null;
}

/**
 * 进入长图编辑阶段。
 * 合成所有帧 → 进入标注模式（复用 annot.reset）。
 *
 * 需要调用 `enterAnnotationWithCropData`（index.js 提供）。
 */
export async function enterScrollEdit() {
  if (ss.scrollCapturePhase !== 'capturing') return;
  if (ss.scrollFrames.length === 0) return;

  // 先停止自动滚动（如果在运行）
  if (ss.autoScroll) {
    await stopAutoScroll();
  }
  // 先阻止新 wheel。若已经进入最终 BitBlt，则让该帧完整提交；尚在防抖/稳定
  // 等待的任务随后通过代际失效，避免编辑快照与预览确认交叉。
  session.captureFinalizing = true;
  if (ss._scrollCaptureTimer) {
    clearTimeout(ss._scrollCaptureTimer);
    ss._scrollCaptureTimer = 0;
  }
  session.queuedManualWheel = null;
  const inFlight = session.captureInFlight;
  if (inFlight) await inFlight.catch(() => {});
  session.invalidate();
  ss.scrollCapturePhase = 'finalizing';

  ss.scrollCapturePhase = 'editing';
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
  const sourceRect = ss.scrollSourceRect ? { ...ss.scrollSourceRect } : null;
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
  if (ss.scrollCapturePhase !== 'capturing' || session.captureFinalizing) return;
  // 自动滚动模式下忽略手动 wheel
  if (ss.autoScroll) return;
  if (!ss.scrollHwnd) {
    ss._showTransientHint?.('未找到可滚动窗口');
    return;
  }

  const modeScale = e.deltaMode === 1 ? 40 : (e.deltaMode === 2 ? ss.scrollBandH : 1);
  const rawDelta = e.deltaY * modeScale;
  if (rawDelta === 0) return;
  // 首个滚轮由 SendInput 补发，随后的真实输入在短时窗口穿透期内直达底层应用。
  const forwardedMagnitude = Math.max(1, Math.min(480, Math.round(Math.abs(rawDelta))));
  const delta = rawDelta > 0 ? -forwardedMagnitude : forwardedMagnitude;
  ss.scrollPendingDirection = rawDelta > 0 ? 1 : -1;
  session.manualWheelVersion++;
  const dpr = window.devicePixelRatio || 1;
  const meta = window.__blinkScreenMeta || { vx: 0, vy: 0 };
  const cursorScreenX = Math.round(meta.vx + e.clientX * dpr);
  const cursorScreenY = Math.round(meta.vy + e.clientY * dpr);

  queueManualWheel(delta, cursorScreenX, cursorScreenY);

  scheduleManualCapture();
}

function scheduleManualCapture() {
  if (ss._scrollCaptureTimer) clearTimeout(ss._scrollCaptureTimer);
  ss._scrollCaptureTimer = setTimeout(runSettledManualCapture, MANUAL_WHEEL_DEBOUNCE_MS);
}

async function runSettledManualCapture() {
  ss._scrollCaptureTimer = 0;
  if (ss.scrollCapturePhase !== 'capturing' || ss.autoScroll) return;
  if (ss._scrollCapturing) {
    scheduleManualCapture();
    return;
  }
  const generation = session.captureGeneration;
  ss._scrollCapturing = true;
  let capturedWheelVersion = session.manualWheelVersion;
  try {
    do {
      capturedWheelVersion = session.manualWheelVersion;
      const settled = await waitForVisualSettle(generation);
      if (settled.aborted) return;
    } while (capturedWheelVersion !== session.manualWheelVersion);
    if (!captureStillActive(generation) || ss.autoScroll) return;
    const direction = ss.scrollPendingDirection;
    const result = await captureFrame(direction);
    ss.scrollPendingDirection = 0;
    if (result.reason === 'no-match') {
      const ambiguous = result.match?.reason === 'ambiguous';
      ss._showTransientHint?.(ambiguous
        ? '页面存在重复内容，暂时无法唯一定位；请缓慢滚回最近已捕获区域'
        : '暂未找到可靠重叠；请缓慢滚回最近已捕获区域后继续');
    } else if (result.relocalized && !result.appended) {
      ss._showTransientHint?.('已恢复到已捕获区域；越过长图边界后才会新增内容');
    } else if (!result.moved && ss.scrollUnchangedCount >= 2) {
      ss._showTransientHint?.('画面没有发生变化，请确认框选中心位于可滚动内容区域');
    }
  } finally {
    ss._scrollCapturing = false;
    if (captureStillActive(generation) && !ss.autoScroll
        && capturedWheelVersion !== session.manualWheelVersion) {
      scheduleManualCapture();
    }
  }
}

/**
 * 切换滚动方向（纵向/横向）。
 */
export function toggleScrollDirection() {
  ss.scrollDirection = 'vertical';
  ss._showTransientHint?.('当前版本先保障纵向长截图，横向滚动暂未开放');
}

/**
 * 切换自动滚动模式。
 * 开启时调 Rust 端 PostMessage 驱动滚动 + 监听 tick 事件。
 */
export async function toggleAutoScroll() {
  if (ss.autoScroll) {
    await stopAutoScroll();
    return;
  }
  if (!ss.scrollHwnd) {
    ss._showTransientHint?.('未找到可滚动窗口');
    return;
  }
  ss.autoScroll = true;
  const generation = ++session.captureGeneration;
  session.autoWheelDelta = -120;
  const btn = document.getElementById('scroll-auto');
  if (btn) {
    btn.classList.toggle('active', ss.autoScroll);
    btn.title = ss.autoScroll ? '自动滚动中（点击停止）' : '自动滚动';
  }

  void runAutoScrollLoop(generation);
}

async function stopAutoScroll(reason = null) {
  ss.autoScroll = false;
  session.invalidate();
  const btn = document.getElementById('scroll-auto');
  if (btn) {
    btn.classList.remove('active');
    btn.title = '自动滚动';
  }
  if (reason) ss._showTransientHint?.(reason);
}

function adaptAutoWheelDelta(result) {
  const shift = Math.abs(result?.positionShift ?? result?.match?.shift ?? 0);
  if (shift <= 0 || ss.scrollBandH <= 0) return;
  const targetShift = ss.scrollBandH * 0.45;
  const ratio = Math.max(0.65, Math.min(1.45, targetShift / shift));
  const magnitude = Math.max(60, Math.min(240, Math.round(Math.abs(session.autoWheelDelta) * ratio)));
  session.autoWheelDelta = -Math.max(30, Math.round(magnitude / 30) * 30);
}

async function runAutoScrollLoop(generation) {
  let positionCursor = true;
  let unchangedCount = 0;
  while (captureStillActive(generation, true)) {
    ss._scrollCapturing = true;
    try {
      await screenshotForwardWheel(
        ss.scrollHwnd,
        session.autoWheelDelta,
        ss.scrollTargetX,
        ss.scrollTargetY,
        AUTO_WHEEL_PASSTHROUGH_MS,
        positionCursor,
      );
      positionCursor = false;

      const settled = await waitForVisualSettle(generation, true);
      if (settled.aborted || !captureStillActive(generation, true)) return;
      const result = await captureFrame(1);
      if (result.moved) {
        unchangedCount = 0;
        adaptAutoWheelDelta(result);
      } else if (result.reason === 'unchanged') {
        unchangedCount++;
        if (unchangedCount >= AUTO_UNCHANGED_LIMIT) {
          await stopAutoScroll('已滚动到底，或目标窗口未响应滚轮');
          return;
        }
      } else {
        await stopAutoScroll('当前画面无法可靠配准，已暂停并保留已捕获内容');
        return;
      }
    } catch (error) {
      console.warn('[scroll] auto scroll failed', error);
      await stopAutoScroll('自动滚动失败，已保留当前长图');
      return;
    } finally {
      ss._scrollCapturing = false;
    }
    await delay(16);
  }
}

/**
 * 输出长图（pin/保存/复制）。
 * 如果在 editing 阶段，先合成最终图。
 *
 * @param {string} action - 'pin' | 'save' | 'copy'
 */
export async function outputLongImage(action) {
  if (ss.scrollCapturePhase === 'capturing') {
    if (ss.autoScroll) await stopAutoScroll();
    session.captureFinalizing = true;
    if (ss._scrollCaptureTimer) {
      clearTimeout(ss._scrollCaptureTimer);
      ss._scrollCaptureTimer = 0;
    }
    session.queuedManualWheel = null;
    const inFlight = session.captureInFlight;
    if (inFlight) await inFlight.catch(() => {});
    session.invalidate();
    ss.scrollCapturePhase = 'finalizing';
  }
  // 从标注引擎获取编辑后的图（如果在 editing 阶段）
  // 否则直接合成 scrollFrames
  let pngData = null;

  if (ss.scrollCapturePhase === 'editing' && ss._compositeSelection) {
    pngData = await ss._compositeSelection();
  } else {
    const longImage = compositeLongImage();
    if (!longImage) {
      if (ss.scrollCapturePhase === 'finalizing') ss.scrollCapturePhase = 'capturing';
      session.captureFinalizing = false;
      return;
    }
    pngData = await encodeImageDataPng(longImage);
  }

  if (!pngData) {
    if (ss.scrollCapturePhase === 'finalizing') ss.scrollCapturePhase = 'capturing';
    session.captureFinalizing = false;
    return;
  }

  const screenX = ss.scrollBandX;
  const screenY = ss.scrollBandY;

  try {
    await outputScreenshotPng(action, pngData, screenX, screenY);
  } catch (error) {
    if (ss.scrollCapturePhase === 'finalizing') ss.scrollCapturePhase = 'capturing';
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
