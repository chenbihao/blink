//! 剪贴板模式类型筛选纯逻辑（0.25.23）。
//!
//! 只管循环顺序与合法性，不碰 DOM/IPC——可单测（clipboard-kind.test.mjs）。
//! 文件/音频待采集支持（CF_HDROP 独立监听）后再进 KIND_ORDER。

/** Tab 循环顺序（Tab 前进 / Shift+Tab 后退）。 */
export const KIND_ORDER = ["all", "text", "image"];

/** 是否合法 kind 值。 */
export function isValidKind(kind) {
    return KIND_ORDER.includes(kind);
}

/**
 * 循环切换到下一个 kind。
 * @param {string} kind 当前 kind（非法值按 "all" 处理）
 * @param {boolean} [backward] true = Shift+Tab 反向
 * @returns {string} 循环后的 kind
 */
export function nextKind(kind, backward = false) {
    const idx = KIND_ORDER.indexOf(kind);
    const base = idx < 0 ? 0 : idx;
    const step = backward ? -1 : 1;
    return KIND_ORDER[(base + step + KIND_ORDER.length) % KIND_ORDER.length];
}
