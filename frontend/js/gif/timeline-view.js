//! GIF 编辑器纯逻辑（0.28.7）——视图计算与命令构造，不碰 DOM/IPC。
//!
//! 与后端 domain `Timeline` 的投影语义对齐：
//! - 播放序列 = `frames[]`（编辑后有效帧，`srcIdx` 指素材原始帧）；
//! - 缩略图页 = 素材原始帧空间分页（被删/出修剪范围的帧也展示，标灰——
//!   用户需要看到被删的帧才能恢复/定位）；
//! - 编辑命令直接构造 `EditCommand` JSON（后端 serde tag `type` snake_case）。

/** 播放调度：给定播放序列与当前序号，返回下一帧序号与该帧停留时长。 */
export function nextPlayStep(frames, cursor) {
    if (!frames.length) return {cursor: 0, durMs: 0, atEnd: true};
    const cur = Math.min(Math.max(cursor, 0), frames.length - 1);
    const next = cur + 1 >= frames.length ? 0 : cur + 1; // 预览循环播放
    return {cursor: next, durMs: frames[cur].durMs, atEnd: false};
}

/** 帧在时间轴上的展示状态。 */
export function frameState(view, srcIdx) {
    const inTrim = srcIdx >= view.trimStart && srcIdx <= view.trimEnd;
    const deleted = (view.deleted || []).includes(srcIdx);
    const held = Boolean(view.holds && view.holds[String(srcIdx)]);
    return {inTrim, deleted, held, visible: inTrim && !deleted};
}

/** 预览帧号（素材空间）→ 播放序号：不在有效序列中返回 -1。 */
export function srcToPlayIndex(view, srcIdx) {
    const frames = view.frames || [];
    for (let i = 0; i < frames.length; i++) {
        if (frames[i].srcIdx === srcIdx) return i;
    }
    return -1;
}

/** 播放序号 → 素材帧号（无帧返回 null）。 */
export function playToSrc(view, playIdx) {
    const frames = view.frames || [];
    if (playIdx < 0 || playIdx >= frames.length) return null;
    return frames[playIdx].srcIdx;
}

/**
 * 编辑命令构造器（与后端 EditCommand serde 格式一致）。
 * undo/redo 不是 EditCommand 变体——后端 `gif_edit` 对 `{"type":"undo"|"redo"}`
 * 特判为栈操作（见 commands/gif.rs）。
 */
export const commands = {
    trimStart: frame => ({type: 'trim_start', frame}),
    trimEnd: frame => ({type: 'trim_end', frame}),
    deleteFrame: frame => ({type: 'delete_frame', frame}),
    deleteFrames: frames => ({type: 'delete_frames', frames}),
    setSpeed: factor => ({type: 'set_speed', factor}),
    setHold: (frame, ms) => ({type: 'set_hold', frame, ms}),
    setLoop: count => ({type: 'set_loop', count}),
    undo: () => ({type: 'undo'}),
    redo: () => ({type: 'redo'}),
};

/** 秒展示（ms → `1.2s` / `12.5s`）。 */
export function formatSeconds(ms) {
    const s = ms / 1000;
    return `${s >= 10 ? s.toFixed(1) : s.toFixed(1)}s`;
}

/** 帧时长展示（ms → `66ms` / `1.2s`）。 */
export function formatFrameDur(ms) {
    return ms >= 1000 ? formatSeconds(ms) : `${Math.round(ms)}ms`;
}
