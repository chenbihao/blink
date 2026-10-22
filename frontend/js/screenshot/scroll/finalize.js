//! 长截图收尾控制器：单任务、有截止时间，尾帧确认后才能冻结输出。

export function finalizeScrollCapture(session, options) {
    if (session.finalizeInFlight) return session.finalizeInFlight;
    if (session.scrollCapturePhase !== 'capturing') {
        return Promise.resolve({ok: session.scrollCapturePhase === 'finalizing'});
    }
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
    const setTimer = options.setTimer || setTimeout;
    const clearTimer = options.clearTimer || clearTimeout;
    let timer;
    const timeout = new Promise((resolve) => {
        timer = setTimer(() => resolve({ok: false, reason: 'timeout'}), options.timeoutMs ?? 3500);
    });
    const drain = async () => {
        await Promise.all(pending);
        // 不复用旧防抖/探针结论：已发生的滚动在这里重新采集。
        for (let attempt = 0; attempt < 4 && isCurrent(); attempt++) {
            const settled = await waitForSettle(generation);
            if (!isCurrent() || settled.aborted) return {ok: false, reason: 'aborted'};
            const result = await captureFrame(0, generation, {
                settle: settled, finalizing: true, wheelStartedAtMs,
            });
            if (!isCurrent()) return {ok: false, reason: 'aborted'};
            // 上限已拒绝末段；允许明确告知后输出此前确认的内容，不能把用户困住。
            if (result.reason === 'frame-limit') return {ok: true, truncated: true};
            if (result.reason === 'unchanged' && settled.stable
                && session.scrollTrackingState === 'tracking' && !session.scrollPendingJump) {
                // 首次 unchanged 仍可能早于目标消费滚轮，再观察一轮。
                if (attempt > 0) return {ok: true};
            } else if (!result.moved && result.reason !== 'pending-confirmation'
                && result.reason !== 'unchanged') {
                return {ok: false, reason: result.reason || 'unconfirmed'};
            }
            await delay(90);
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
