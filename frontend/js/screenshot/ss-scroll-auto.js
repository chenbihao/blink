//! 长截图自动滚动速度反馈。纯函数，不负责注入滚轮或会话生命周期。

const AUTO_UNCHANGED_LIMIT = 3;

function quantizeWheelMagnitude(value) {
  return Math.max(60, Math.min(240, Math.round(value / 30) * 30));
}

export function nextAutoWheelState(state, result, bandHeight, settle = {}) {
  const currentMagnitude = Math.abs(state.delta || -120);
  const shift = Math.abs(result?.positionShift ?? result?.match?.shift ?? 0);
  if (shift <= 0 || bandHeight <= 0) return { ...state };
  const confidence = result?.decision?.confidence ?? 1;
  const lowSignal = settle.timedOut === true
    || confidence < 0.55
    || shift > bandHeight * 0.7;
  if (lowSignal) {
    return {
      delta: -quantizeWheelMagnitude(currentMagnitude * 0.72),
      lowConfidenceCount: (state.lowConfidenceCount || 0) + 1,
    };
  }

  const targetShift = bandHeight * 0.45;
  const recovering = (state.lowConfidenceCount || 0) > 0;
  const maxAcceleration = recovering ? 1.15 : 1.45;
  const ratio = Math.max(0.65, Math.min(maxAcceleration, targetShift / shift));
  let nextMagnitude = quantizeWheelMagnitude(currentMagnitude * ratio);
  if (ratio > 1 && nextMagnitude <= currentMagnitude) {
    nextMagnitude = Math.min(240, currentMagnitude + 30);
  }
  return {
    delta: -nextMagnitude,
    lowConfidenceCount: Math.max(0, (state.lowConfidenceCount || 0) - 1),
  };
}

/** 自动滚动单生产者控制器。平台注入、采集和 UI 停止动作全部由调用方注入。 */
export async function runAutoScrollController(options) {
  const {
    generation, session, isActive, waitForSettle, captureFrame,
    forwardWheel, stop, delay,
  } = options;

  const recoverTracking = async () => {
    const settled = await waitForSettle(generation, true);
    if (settled.aborted || !isActive(generation, true)) return null;
    let result = await captureFrame(0, generation, { settle: settled });
    if (result.reason === 'pending-confirmation') {
      const confirmationSettle = await waitForSettle(generation, true);
      if (confirmationSettle.aborted || !isActive(generation, true)) return null;
      result = await captureFrame(0, generation, { settle: confirmationSettle });
    }
    return result;
  };

  let positionCursor = true;
  let unchangedCount = 0;
  if (session.scrollTrackingState === 'lost') {
    const recovered = await recoverTracking();
    if (!recovered?.moved) {
      await stop('当前位置仍无法恢复定位；请手动滚回已捕获区域');
      return;
    }
  }

  while (isActive(generation, true)) {
    session._scrollCapturing = true;
    try {
      await forwardWheel(positionCursor);
      positionCursor = false;
      const settled = await waitForSettle(generation, true);
      if (settled.aborted || !isActive(generation, true)) return;
      let result = await captureFrame(1, generation, { settle: settled });
      if (result.reason === 'pending-confirmation') {
        result = await recoverTracking();
        if (!result) return;
      }
      if (result.moved) {
        unchangedCount = 0;
        const next = nextAutoWheelState({
          delta: session.autoWheelDelta,
          lowConfidenceCount: session.autoLowConfidenceCount,
        }, result, session.scrollBandH, settled);
        session.autoWheelDelta = next.delta;
        session.autoLowConfidenceCount = next.lowConfidenceCount;
        if (session.autoLowConfidenceCount >= 3) {
          await stop('连续低置信或页面持续运动，自动滚动已暂停');
          return;
        }
      } else if (result.reason === 'unchanged') {
        unchangedCount++;
        if (unchangedCount >= AUTO_UNCHANGED_LIMIT) {
          await stop('已滚动到底，或目标窗口未响应滚轮');
          return;
        }
      } else {
        await stop('当前画面无法可靠配准，已暂停并保留已捕获内容');
        return;
      }
    } catch (error) {
      console.warn('[scroll] auto scroll failed', error);
      await stop('自动滚动失败，已保留当前长图');
      return;
    } finally {
      session._scrollCapturing = false;
    }
    await delay(16);
  }
}
