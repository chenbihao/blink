//! 长截图自动滚动速度反馈。纯函数，不负责注入滚轮或会话生命周期。

const AUTO_UNCHANGED_LIMIT = 5;
const AUTO_DELAYED_MOTION_RECHECK_MS = 90;

function quantizeWheelMagnitude(value) {
    return Math.max(60, Math.min(240, Math.round(value / 30) * 30));
}

export function nextAutoWheelState(state, result, bandHeight, settle = {}) {
    const currentMagnitude = Math.abs(state.delta || -120);
    const shift = Math.abs(result?.positionShift ?? result?.match?.shift ?? 0);
    if (shift <= 0 || bandHeight <= 0) return {...state};
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
        forwardWheel, previewWheel, stop, delay,
    } = options;

    const recoverTracking = async () => {
        const settled = await waitForSettle(generation, true);
        if (settled.aborted || !isActive(generation, true)) return null;
        let result = await captureFrame(0, generation, {settle: settled});
        if (result.reason === 'pending-confirmation') {
            const confirmationSettle = await waitForSettle(generation, true);
            if (confirmationSettle.aborted || !isActive(generation, true)) return null;
            result = await captureFrame(0, generation, {settle: confirmationSettle});
        }
        return result;
    };

    let positionCursor = true;
    let unchangedCount = 0;
    if (session.scrollTrackingState !== 'tracking') {
        const recovered = await recoverTracking();
        if (!recovered?.moved) {
            await stop('当前位置仍无法恢复定位；请手动滚回已捕获区域');
            return;
        }
    }

    while (isActive(generation, true)) {
        session._scrollCapturing = true;
        try {
            // SendInput 成功只表示事件已注入，不表示目标滚动容器消费了它。连续无变化时
            // 先重新定位光标，再绕过 SendInput 直接向已识别的目标 HWND 发消息。
            positionCursor = positionCursor || unchangedCount >= 1;
            const forceMessage = unchangedCount >= 2;
            const wheelStartedAtMs = performance.now();
            previewWheel?.(1);
            await forwardWheel(positionCursor, forceMessage);
            positionCursor = false;
            let settled = await waitForSettle(generation, true);
            if (settled.aborted || !isActive(generation, true)) return;
            let result = await captureFrame(1, generation, {settle: settled, wheelStartedAtMs});
            // 目标窗口繁忙时，快路径可能在滚轮真正生效前得到 unchanged。复用同一次
            // wheel 做一次延迟采集，避免继续注入滚轮并造成后续大跨度跳转。
            if (result.reason === 'unchanged') {
                await delay(AUTO_DELAYED_MOTION_RECHECK_MS);
                if (!isActive(generation, true)) return;
                settled = await waitForSettle(generation, true);
                if (settled.aborted || !isActive(generation, true)) return;
                result = await captureFrame(1, generation, {settle: settled, wheelStartedAtMs});
            }
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
