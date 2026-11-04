//! 删除前等待本会话的导出真正结束，迟到确认不能操作另一段录制。
export async function discardRecording({sessionId, invoke, isCurrent, pendingStart = () => false,
    wait = ms => new Promise(resolve => setTimeout(resolve, ms)), now = () => performance.now(), timeout = 30000}) {
    const deadline = now() + timeout;
    const check = () => {
        if (!isCurrent()) throw {code: 'gif_stale_session', message: '编辑会话已切换，请重新确认删除'};
        if (now() >= deadline) throw {code: 'gif_discard_timeout', message: '导出仍在收尾，请稍后重试删除'};
    };
    while (pendingStart()) { check(); await wait(100); }
    let cancelled = false;
    for (;;) {
        check();
        const job = await invoke('gif_export_status', {sessionId, jobId: null});
        check();
        if (!job || ['done', 'cancelled', 'failed'].includes(job.stage)) break;
        if (!cancelled) {
            await invoke('gif_export_cancel', {sessionId});
            cancelled = true;
        }
        await wait(100);
    }
    check();
    await invoke('gif_discard_session', {sessionId, discardCurrent: true});
}
