/**
 * VAD 切句参数纯逻辑（0.23.7 从 voice.js 抽出，便于无 DOM 单测）。
 *
 * 默认值与安全范围与 Rust 侧 `VadConfig`（default_vad_* / VAD_*_MIN/MAX_S）
 * 一一对应；`normalizeVadWindows` 与 Rust `VadConfig::sanitize` 规则一致：
 * 先各自 clamp 到安全范围，再按 `soft < hard <= max_uncommitted` 前移。
 */

export const VAD_DEFAULTS = {
    silence_threshold: 0.005,
    min_silence_ms: 300,
    min_sentence_ms: 800,
    soft_window_s: 8,
    hard_window_s: 12,
    max_uncommitted_s: 12,
};

export const VAD_WINDOW_KEYS = ["soft_window_s", "hard_window_s", "max_uncommitted_s"];

export const VAD_WINDOW_RANGE = {
    soft_window_s: {min: 3, max: 30},
    hard_window_s: {min: 5, max: 30},
    max_uncommitted_s: {min: 5, max: 30},
};

/**
 * 窗口组合归一化：保证 soft < hard <= max_uncommitted。
 * 就地修改并返回是否发生调整；默认值组合（8/12/12）恒为不动点。
 */
export function normalizeVadWindows(vad) {
    let changed = false;
    for (const key of VAD_WINDOW_KEYS) {
        const range = VAD_WINDOW_RANGE[key];
        const clamped = Math.min(range.max, Math.max(range.min, vad[key]));
        if (clamped !== vad[key]) {
            vad[key] = clamped;
            changed = true;
        }
    }
    if (vad.hard_window_s > vad.max_uncommitted_s) {
        vad.hard_window_s = vad.max_uncommitted_s;
        changed = true;
    }
    if (vad.soft_window_s >= vad.hard_window_s) {
        vad.soft_window_s = vad.hard_window_s - 1;
        changed = true;
    }
    return changed;
}

/**
 * 补齐旧配置缺失的窗口字段（0.23.7 前只有 3 个参数）并归一化非法旧值。
 */
export function ensureVadWindowFields(vad) {
    for (const key of VAD_WINDOW_KEYS) {
        if (typeof vad[key] !== "number") {
            vad[key] = VAD_DEFAULTS[key];
        }
    }
    return normalizeVadWindows(vad);
}
