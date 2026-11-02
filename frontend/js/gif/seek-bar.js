//! 录制编辑器进度定位纯逻辑（0.28.23）——时间 ↔ 播放序号映射、指针换算、
//! 键盘步进与悬浮预览定位；不碰 DOM/IPC，由 editor.js 组装。
//!
//! 定位基于**编辑后有效时长的前缀和**（`prefix[i]` = 播放序列前 i 帧累计 ms，
//! 长度 = 帧数 + 1），天然覆盖修剪、删帧、变速与手动停留——不是把进度比例
//! 直接乘原始帧数。

/**
 * 时间 → 播放位置。
 * - 空序列返回 null；
 * - `t <= 0` 落第 0 帧起点；`t >= total` 落末帧末尾（`atEnd=true`，帧内偏移 = 末帧时长）；
 * - 其余二分取最大 i 使 `prefix[i] <= t`。
 * @param {number[]} prefix
 * @param {number} t 毫秒
 * @returns {{playIdx:number, inFrameMs:number, atEnd:boolean}|null}
 */
export function locateTime(prefix, t) {
    const n = prefix.length - 1;
    if (n <= 0) return null;
    const total = prefix[n];
    if (!(t > 0) || total <= 0) return {playIdx: 0, inFrameMs: 0, atEnd: false};
    if (t >= total) return {playIdx: n - 1, inFrameMs: prefix[n] - prefix[n - 1], atEnd: true};
    let lo = 0;
    let hi = n - 1;
    while (lo < hi) {
        const mid = (lo + hi + 1) >> 1;
        if (prefix[mid] <= t) lo = mid;
        else hi = mid - 1;
    }
    return {playIdx: lo, inFrameMs: t - prefix[lo], atEnd: false};
}

/** 播放序号（+ 帧内偏移）→ 时间；序号越界时钳制到有效范围。 */
export function timeAt(prefix, playIdx, inFrameMs = 0) {
    const n = prefix.length - 1;
    if (n <= 0) return 0;
    const i = Math.min(Math.max(playIdx, 0), n - 1);
    const dur = prefix[i + 1] - prefix[i];
    return prefix[i] + Math.min(Math.max(inFrameMs, 0), dur);
}

/** 指针 clientX → 轨道比例 [0,1]；轨道宽 0 返回 0。 */
export function pointerRatio(clientX, rect) {
    const w = rect.width || 0;
    if (w <= 0) return 0;
    return Math.min(1, Math.max(0, (clientX - rect.left) / w));
}

/** 比例 → 时间（钳制）。 */
export function ratioToTime(ratio, totalMs) {
    if (!(totalMs > 0)) return 0;
    return Math.min(totalMs, Math.max(0, ratio * totalMs));
}

/**
 * 进度条键盘步进（定位语义：钳制不循环，与时间轴 ←/→ 的循环步进区分）。
 * 返回新播放序号；无关按键返回 null。
 */
export function keyboardStep(playIdx, frameCount, code) {
    if (frameCount <= 0) return null;
    const last = frameCount - 1;
    switch (code) {
        case 'ArrowLeft':
        case 'ArrowDown':
            return Math.max(0, playIdx - 1);
        case 'ArrowRight':
        case 'ArrowUp':
            return Math.min(last, playIdx + 1);
        case 'PageDown':
            return Math.max(0, playIdx - 10);
        case 'PageUp':
            return Math.min(last, playIdx + 10);
        case 'Home':
            return 0;
        case 'End':
            return last;
        default:
            return null;
    }
}

/** 悬浮预览小窗 left：水平居中到指针，钳制在容器内。 */
export function clampPreviewLeft(pointerX, previewW, containerW) {
    const half = previewW / 2;
    const max = Math.max(0, containerW - previewW);
    return Math.min(max, Math.max(0, pointerX - half));
}

/** 时间码 `m:ss.d`（进度条与悬浮预览用；与控制条 formatElapsed 同格式）。 */
export function formatTimecode(ms) {
    const total = Math.max(0, ms);
    const sec = Math.floor(total / 1000);
    const tenth = Math.floor((total % 1000) / 100);
    const m = Math.floor(sec / 60);
    const s = sec % 60;
    return `${m}:${String(s).padStart(2, '0')}.${tenth}`;
}

/**
 * 最新优先的请求代次：连续悬浮/拖动只认最后一个目标，迟到回流用
 * `isCurrent(gen)` 丢弃。会话/编辑 revision 变化时 `bump()` 整体失效。
 */
export function createLatestGen() {
    let gen = 0;
    return {
        next() {
            return ++gen;
        },
        current() {
            return gen;
        },
        isCurrent(g) {
            return g === gen;
        },
        bump() {
            gen++;
        },
    };
}
