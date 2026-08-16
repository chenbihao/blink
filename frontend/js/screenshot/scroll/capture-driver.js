//! 长截图采集驱动：滚轮转发、稳定等待和完整帧抓取。不得包含定位或拼接决策。

import {screenshotCaptureBand, screenshotCaptureProbe, screenshotForwardWheel,} from '../../shared/api.js';
import {isProbeStable} from './stability.js';

const SETTLE_PROBE_INTERVAL_MS = 45;
const SETTLE_MAX_WAIT_MS = 900;
const SETTLE_FAST_MIN_WAIT_MS = 90;
const SETTLE_ANIMATED_MIN_WAIT_MS = 180;
const SETTLE_STABLE_SAMPLE_COUNT = 2;
const SETTLE_INITIAL_DELAY_MS = 35;
const MANUAL_WHEEL_PASSTHROUGH_MS = 72;

export const MANUAL_WHEEL_DEBOUNCE_MS = 45;
export const AUTO_WHEEL_PASSTHROUGH_MS = 24;

export function delay(ms) {
    return new Promise((resolve) => setTimeout(resolve, ms));
}

/** 静止页面走 90ms 快路径；一旦观察到运动，恢复 180ms 安全等待。 */
export function shouldCompleteVisualSettle(elapsedMs, stableSamples, sawMotion) {
    const minimum = sawMotion ? SETTLE_ANIMATED_MIN_WAIT_MS : SETTLE_FAST_MIN_WAIT_MS;
    return stableSamples >= SETTLE_STABLE_SAMPLE_COUNT && elapsedMs >= minimum;
}

function active(session, generation, requireAuto) {
    return generation === session.captureGeneration
        && session.scrollCapturePhase === 'capturing'
        && (!requireAuto || session.autoScroll);
}

async function captureProbe(state) {
    const buffer = await screenshotCaptureProbe(
        state.scrollBandX,
        state.scrollBandY,
        state.scrollBandW,
        state.scrollBandH,
    );
    return buffer ? new Uint8Array(buffer) : null;
}

export async function waitForVisualSettle(session, generation, requireAuto = false) {
    const overallStartedAt = performance.now();
    await delay(SETTLE_INITIAL_DELAY_MS);
    if (!active(session, generation, requireAuto)) return {aborted: true};
    let previous;
    try {
        previous = await captureProbe(session);
    } catch (error) {
        console.warn('[scroll] 稳定探针首次采集失败，使用短延时兜底', error);
        await delay(180);
        return {
            stable: false,
            fallback: true,
            mode: 'fallback',
            elapsedMs: Math.round(performance.now() - overallStartedAt),
        };
    }

    const startedAt = performance.now();
    let stableSamples = 0;
    let lastScore = Infinity;
    let sawMotion = false;
    while (performance.now() - startedAt < SETTLE_MAX_WAIT_MS) {
        await delay(SETTLE_PROBE_INTERVAL_MS);
        if (!active(session, generation, requireAuto)) return {aborted: true};
        let current;
        try {
            current = await captureProbe(session);
        } catch (error) {
            console.warn('[scroll] 稳定探针采集失败，继续等待', error);
            stableSamples = 0;
            continue;
        }
        const motion = isProbeStable(previous, current);
        lastScore = motion.score;
        if (!motion.stable) sawMotion = true;
        stableSamples = motion.stable ? stableSamples + 1 : 0;
        previous = current;
        const elapsed = performance.now() - startedAt;
        if (shouldCompleteVisualSettle(elapsed, stableSamples, sawMotion)) {
            return {
                stable: true,
                score: lastScore,
                mode: sawMotion ? 'animated' : 'fast',
                elapsedMs: Math.round(performance.now() - overallStartedAt),
                probeElapsedMs: Math.round(elapsed),
            };
        }
    }
    console.debug('[scroll] 稳定等待超时，交由全帧匹配确认', {score: lastScore});
    return {
        stable: false,
        timedOut: true,
        score: lastScore,
        mode: 'timeout',
        elapsedMs: Math.round(performance.now() - overallStartedAt),
    };
}

export async function captureBandFrame(state) {
    const {scrollBandX: x, scrollBandY: y, scrollBandW: width, scrollBandH: height} = state;
    const buffer = await screenshotCaptureBand(x, y, width, height);
    const expected = width * height * 4;
    if (!buffer || buffer.byteLength < expected) {
        return {frame: null, reason: 'short-buffer', expected, got: buffer?.byteLength};
    }
    return {
        frame: new ImageData(new Uint8ClampedArray(buffer), width, height),
        reason: null,
    };
}

export function queueManualWheel(session, delta, screenX, screenY) {
    if (session.queuedManualWheel) {
        session.queuedManualWheel.delta = Math.max(
            -480,
            Math.min(480, session.queuedManualWheel.delta + delta),
        );
        session.queuedManualWheel.screenX = screenX;
        session.queuedManualWheel.screenY = screenY;
    } else {
        session.queuedManualWheel = {delta, screenX, screenY};
    }
    if (!session.wheelForwardPending) void pumpManualWheel(session);
}

async function pumpManualWheel(session) {
    if (session.wheelForwardPending) return;
    session.wheelForwardPending = true;
    try {
        while (session.queuedManualWheel
        && session.scrollCapturePhase === 'capturing' && !session.autoScroll) {
            const wheel = session.queuedManualWheel;
            session.queuedManualWheel = null;
            await screenshotForwardWheel(
                session.scrollHwnd,
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
        if (session.queuedManualWheel
            && session.scrollCapturePhase === 'capturing' && !session.autoScroll) {
            void pumpManualWheel(session);
        }
    }
}

export function forwardAutoWheel(session, positionCursor, forceMessage = false) {
    return screenshotForwardWheel(
        session.scrollHwnd,
        session.autoWheelDelta,
        session.scrollTargetX,
        session.scrollTargetY,
        AUTO_WHEEL_PASSTHROUGH_MS,
        positionCursor,
        forceMessage,
    );
}
