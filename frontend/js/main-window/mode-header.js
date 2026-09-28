//! ModeHeader（0.24 §5.5）：持续模式的唯一呈现。
//!
//! AI / 命令 / 剪贴板三种独占模式的徽章统一投影到 #mode-header——
//! 替换此前各自为政的三个容器（.ai-mode-indicator / #command-hint /
//! #clipboard-mode-badge）。无模式时隐藏不占高度；模式切换是离散状态转换，
//! 允许窗口高度随之变化（与逐键抖动不同类）。

import {iconHTML} from "../shared/icon.js";

let el = null;
let iconEl = null;
let labelEl = null;
let hintEl = null;

/** 当前投影的模式 id（null = 无模式）。 */
let current = null;

/** 初始化：绑定 DOM。main.js 启动时调一次。 */
export function init() {
    el = document.getElementById("mode-header");
    iconEl = el?.querySelector(".mode-icon") ?? null;
    labelEl = el?.querySelector(".mode-label") ?? null;
    hintEl = el?.querySelector(".mode-hint") ?? null;
}

/**
 * 投影一个持续模式。
 * @param {{id: string, icon?: string, label: string, hint?: string}} mode
 *   icon 为 Lucide sprite 内图标名（省略则不显示图标）
 */
export function set(mode) {
    if (!el) return;
    current = mode?.id ?? null;
    if (!current) return clear();
    if (iconEl) iconEl.innerHTML = mode.icon ? iconHTML(mode.icon) : "";
    if (labelEl) labelEl.textContent = mode.label ?? "";
    if (hintEl) {
        hintEl.textContent = mode.hint ?? "";
        hintEl.classList.toggle("hidden", !mode.hint);
    }
    el.classList.remove("hidden");
}

/** 清空模式投影（退出模式时调）。 */
export function clear() {
    if (!el) return;
    current = null;
    el.classList.add("hidden");
    if (iconEl) iconEl.innerHTML = "";
    if (labelEl) labelEl.textContent = "";
    if (hintEl) hintEl.textContent = "";
}

/** 当前模式 id（供模式模块幂等判断）。 */
export function activeId() {
    return current;
}
