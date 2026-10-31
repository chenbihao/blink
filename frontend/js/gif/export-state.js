//! 双格式导出的纯状态：事件必须同时匹配会话、job 和递增序列。
export const isTerminal = snapshot => ['done', 'failed', 'cancelled'].includes(snapshot?.stage);
export function exportPath(path, format) {
    if (!path) return '';
    return /\.(gif|mp4)$/i.test(path) ? path.replace(/\.(gif|mp4)$/i, `.${format}`) : `${path}.${format}`;
}
export class ExportJobState {
    constructor() { this.reset(null); }
    reset(sessionId) { this.sessionId = sessionId; this.jobId = null; this.sequence = 0; this.snapshot = null; }
    attach(jobId) { this.jobId = jobId; this.sequence = 0; this.snapshot = null; }
    accept(snapshot, adopt = false) {
        if (!snapshot || snapshot.sessionId !== this.sessionId || !snapshot.jobId || !Number.isSafeInteger(snapshot.sequence)) return false;
        if (adopt && snapshot.jobId !== this.jobId) this.attach(snapshot.jobId);
        if (snapshot.jobId !== this.jobId || snapshot.sequence <= this.sequence || isTerminal(this.snapshot)) return false;
        this.sequence = snapshot.sequence;
        this.snapshot = snapshot;
        return true;
    }
    get running() { return !!this.jobId && !isTerminal(this.snapshot); }
}
export function progressView(snapshot) {
    const ratio = Math.max(0, Math.min(1, (snapshot.framesDone || 0) / (snapshot.framesTotal || 1)));
    const gif = snapshot.format === 'gif';
    const stage = snapshot.stage;
    if (stage === 'done') return {value: 100, text: '导出完成'};
    if (stage === 'cancelled') return {value: 0, text: '已取消，录制素材已保留'};
    if (stage === 'failed') return {value: 0, text: `导出失败：${snapshot.error || '请重试'}`};
    if (stage === 'preparing') return {value: gif ? ratio * 45 : 0, text: gif ? '准备调色板…' : '准备视频…'};
    if (stage === 'quantizing') return {value: 45, text: '量化调色板…'};
    if (stage === 'verifying') return {value: 85 + ratio * 13, text: '验证完整成品…'};
    if (stage === 'committing') return {value: 99, text: '保存文件…'};
    return {value: (gif ? 45 : 0) + ratio * (gif ? 40 : 85), text: `编码中 ${snapshot.framesDone || 0}/${snapshot.framesTotal || 0}`};
}
