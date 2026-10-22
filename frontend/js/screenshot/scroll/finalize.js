//! 长截图收尾控制器：单任务、有截止时间，尾帧确认后才能冻结输出。

const TAIL_STABLE_SPAN_MS = 180;
// 空闲采样周期为250ms；允许一次抓帧/处理开销，过期或断档证据不能走快路径。
const TAIL_EVIDENCE_MAX_AGE_MS = 350;

export function recordTailStability(session, result, metadata) {
    const at = metadata.captureStartedAt;
    if (result.decision.reason !== 'unchanged' || result.trackingState !== 'tracking'
        || result.pendingJump || metadata.discontinuity || metadata.finalizing || session.autoScroll
        || metadata.wheelVersion !== session.manualWheelVersion || !Number.isFinite(at)) {
        session.tailStability = null;
        return;
    }
    const previous = session.tailStability;
    const continuous = previous && previous.wheelVersion === metadata.wheelVersion
        && at > previous.lastAt && at - previous.lastAt <= TAIL_EVIDENCE_MAX_AGE_MS;
    session.tailStability = {since: continuous ? previous.since : at, lastAt: at,
        wheelVersion: metadata.wheelVersion};
}

function canReuseTailStability(session, now) {
    const evidence = session.tailStability;
    return evidence && session.scrollTrackingState === 'tracking' && !session.scrollPendingJump
        && evidence.wheelVersion === session.manualWheelVersion
        && evidence.lastAt - evidence.since >= TAIL_STABLE_SPAN_MS
        && now >= evidence.lastAt && now - evidence.lastAt <= TAIL_EVIDENCE_MAX_AGE_MS;
}

export function finalizeScrollCapture(session, options) {
    if (session.finalizeInFlight) return session.finalizeInFlight;
    if (session.scrollCapturePhase !== 'capturing') {
        return Promise.resolve({ok: session.scrollCapturePhase === 'finalizing'});
    }
    const wasAutoScroll = session.autoScroll;
    session.captureFinalizing = true;
    session.autoScroll = false;
    const generation = session.invalidate();
    const wheelStartedAtMs = session.scrollWheelStartedAtMs ?? performance.now();
    if (session._scrollCaptureTimer) clearTimeout(session._scrollCaptureTimer);
    session._scrollCaptureTimer = 0;
    const pending = [session.wheelForwardInFlight, session.autoScrollTask, session.captureInFlight,
        Promise.resolve().then(() => options.stopCapture?.())];
    const isCurrent = () => generation === session.captureGeneration
        && session.scrollCapturePhase === 'capturing';
    const {waitForSettle, captureFrame, delay} = options;
    const now = options.now || (() => performance.now());
    const setTimer = options.setTimer || setTimeout;
    const clearTimer = options.clearTimer || clearTimeout;
    let timer;
    const timeout = new Promise((resolve) => {
        timer = setTimer(() => resolve({ok: false, reason: 'timeout'}), options.timeoutMs ?? 3500);
    });
    const drain = async () => {
        await Promise.all(pending);
        // 只复用近期连续静止的观察时间；像素仍强制抓两次新帧，绝不复用旧尾帧。
        const confirmationStartedAt = now();
        const reuseStability = !wasAutoScroll && canReuseTailStability(session, confirmationStartedAt);
        for (let attempt = 0; attempt < 4 && isCurrent(); attempt++) {
            const settleStarted = now();
            const settled = reuseStability && attempt === 0
                ? {stable: true, mode: 'recent-tail'} : await waitForSettle(generation);
            if (!isCurrent() || settled.aborted) return {ok: false, reason: 'aborted'};
            // 相邻确认至少隔 90ms；稳定探针自身的观察时间计入间隔，不再串联等待两遍。
            const remaining = attempt > 0 ? Math.max(0, 90 - (now() - settleStarted),
                reuseStability && attempt === 1 ? TAIL_STABLE_SPAN_MS - (now() - confirmationStartedAt) : 0) : 0;
            if (remaining > 0) await delay(remaining);
            if (!isCurrent()) return {ok: false, reason: 'aborted'};
            const result = await captureFrame(0, generation, {
                settle: settled, finalizing: true, wheelStartedAtMs,
            });
            if (!isCurrent()) return {ok: false, reason: 'aborted'};
            // 上限已拒绝末段；允许明确告知后输出此前确认的内容，不能把用户困住。
            if (result.reason === 'frame-limit') return {ok: true, truncated: true};
            if (result.reason === 'unchanged' && settled.stable
                && session.scrollTrackingState === 'tracking' && !session.scrollPendingJump) {
                // 首次 unchanged 仍可能早于目标消费滚轮，再观察一轮。
                if (attempt > 0) return {ok: true, reusedStability: !!reuseStability};
            } else if (!result.moved && result.reason !== 'pending-confirmation'
                && result.reason !== 'unchanged') {
                return {ok: false, reason: result.reason || 'unconfirmed'};
            }
        }
        return {ok: false, reason: isCurrent() ? 'unconfirmed' : 'aborted'};
    };
    const task = Promise.race([drain(), timeout])
        .catch((error) => ({ok: false, reason: 'capture-error', error}))
        .then((result) => {
            if (!isCurrent()) return {ok: false, reason: 'aborted'};
            // 超时和失败也推进代际，阻止尚未返回的 IPC 推进任何状态。
            session.invalidate();
            session._scrollCapturing = false;
            session.captureFinalizing = result.ok;
            if (result.ok) session.scrollCapturePhase = 'finalizing';
            return result;
        }).finally(() => {
            clearTimer(timer);
            if (session.finalizeInFlight === task) session.finalizeInFlight = null;
        });
    session.finalizeInFlight = task;
    return task;
}
