//! ResultFooter（0.24.4 §5.5；元素 id 沿用 #statusbar，避免无谓的 DOM 迁移）。
//!
//! **职责收窄**：只显示结果导航/动作提示（左）+ 翻页（右）+ Alt 按住时的
//! Chord 副行（非空 query，0.24.7 回退恢复；0.24.4 的闲置常驻行经实测多余
//! 已删，见 0.24 §5.9）。建议采纳提示移居 SuggestionBar（suggestion-bar.js）。
//!
//! 另承载剪贴板多选等**持续状态覆写**（setOverride——非瞬态，与
//! TransientFeedback 的 loading/success/error 分工不同）。
//!
//! **文案键帽化**：含键位的提示全部走 i18n 模板 `{{key:X}}` + `renderHint`。
//! `#statusbar { min-height }` 稳定基线高度，避免切换时窗口抖动。

import {actionHint} from "./hints.js";
import {t} from "../i18n/index.js";
import * as chord from "./chord.js";
import {renderHint, renderKey} from "../shared/kbd.js";
import {syncWindowSize} from "./window-size.js";
import {queryEl} from "./dom.js";

const el = document.getElementById("statusbar");

/** 缓存最近一次 update() 的入参，供状态变化时重绘用。 */
let lastActive = null;
let lastPaging = {page: 1, pageCount: 1};
/** 0.21.x: 是否剪贴板模式（影响 Enter 动作提示文案：上屏 vs 复制）。 */
let lastClipboardMode = false;
/** 持续状态覆写文本（剪贴板多选计数等）；null = 无覆写。 */
let overrideText = null;

/** 初始化：订阅 chord 配置/可见性变化 + query 输入（闲置行条件依赖 query 空/非空）。 */
export function init() {
    // chord 配置刷新/Alt 可见性变化 → 副行出现/消失（statusbar 高度可能变化）
    // → 重绘 + 窗口 resize
    chord.onVisibilityChange(() => {
        render();
        syncWindowSize();
    });
}

/**
 * 刷新提示栏。
 * 0.16.1: active.actions 数组，取 actions[0] 的 kind/hint 生成提示。
 * 0.21.x: clipboardMode 参数供 Enter 提示文案区分（上屏 / 复制）。
 * @param {{actions?: Array<{kind: string, hint?: string}>}|null} active 当前选中项
 * @param {{page: number, pageCount: number}} paging 翻页信息
 * @param {boolean} [clipboardMode] 是否处于剪贴板模式
 */
export function update(active, paging, clipboardMode) {
    lastActive = active;
    lastPaging = paging || {page: 1, pageCount: 1};
    lastClipboardMode = !!clipboardMode;
    render();
}

/**
 * 设置持续状态覆写（剪贴板多选计数等）。非空时左侧只显示该文本，
 * 导航提示让位；clearOverride 恢复。
 * @param {string|null} text
 */
export function setOverride(text) {
    overrideText = text || null;
    render();
}

/** 清除持续状态覆写。 */
export function clearOverride() {
    setOverride(null);
}

function render() {
    el.replaceChildren();

    const left = buildLeft();
    if (left) el.appendChild(left);
    const right = buildRight(lastPaging);
    if (right) el.appendChild(right);

    // 可见条件：有左侧内容（导航/覆写/闲置行/副行）或有翻页
    if (!left && !right) {
        el.classList.remove("visible");
        return;
    }
    el.classList.add("visible");
}

/** 左侧内容：覆写 > 导航提示 > Chord 副行；均无则 null（整条隐藏）。
 *  返回 .hint-left 容器（主行 + 可选副行垂直 stack）。 */
function buildLeft() {
    let primary = null;

    // 1. 持续状态覆写（剪贴板多选计数）
    if (overrideText) {
        const span = document.createElement("span");
        span.className = "hint-primary";
        span.textContent = overrideText;
        primary = span;
    } else if (lastActive) {
        // 2. 常规态：导航 · Alt+数字 · 动作提示——三段用 · 分隔，各段都走 renderHint 支持键帽。
        const span = document.createElement("span");
        span.className = "hint-primary";
        span.appendChild(renderHint(t("hint.navigate")));
        span.appendChild(document.createTextNode(" · "));
        span.appendChild(renderHint(t("hint.alt_number")));
        span.appendChild(document.createTextNode(" · "));
        const {template, params} = actionHint(lastActive.actions?.[0], lastClipboardMode);
        span.appendChild(renderHint(template, params));
        primary = span;
    }

    // 3. Chord 副行：非空 query + Alt 按住 → 输入框 overlay 让位给补全影子，
    //    chord 提示转由此副行承接（0.16.1 互斥分工，0.24.7 回退恢复）。
    //    空 query 不按 Alt 无任何 chord 提示（0.24.4 闲置常驻行经实测多余已删，
    //    见 0.24 §5.9——待命提示只按 Alt 时就近出现，零迁移）。
    const secondary = buildSecondary();

    if (!primary && !secondary) return null;
    const left = document.createElement("div");
    left.className = "hint-left";
    if (primary) left.appendChild(primary);
    if (secondary) left.appendChild(secondary);
    return left;
}

/** Chord 副行：非空 query + Alt 按住（chord-visible）+ 有可用动作时返回一行键帽，
 *  否则 null。非空 query 下 getActions 已按 available_with_query 过滤。
 *  （0.16.1 互斥分工，0.24.4 曾移居 Cheat Sheet，0.24.7 回退恢复。） */
function buildSecondary() {
    if (!queryEl.value.trim()) return null;
    if (!document.body.classList.contains("chord-visible")) return null;
    const actions = chord.getActions();
    if (!actions.length) return null;

    const secondary = document.createElement("div");
    secondary.className = "hint-secondary";
    actions.forEach((a, i) => {
        if (i > 0) {
            const sep = document.createElement("span");
            sep.className = "chord-sep";
            sep.textContent = "│";
            secondary.appendChild(sep);
        }
        // 紧凑格式：单 kbd 显示键名（Alt 修饰键隐含，省空间）
        const kbd = renderKey(chord.chordKeyLabel(a.key));
        secondary.appendChild(kbd);
        const label = document.createElement("span");
        label.className = "chord-label";
        label.textContent = a.label;
        secondary.appendChild(label);
    });
    return secondary;
}

/** 右侧：翻页提示（多于一屏才显示）。 */
function buildRight(paging) {
    if (paging.pageCount <= 1) return null;
    const right = document.createElement("span");
    right.className = "hint-right";
    right.appendChild(renderHint(t("statusbar.paging"), {
        page: paging.page,
        pageCount: paging.pageCount,
    }));
    return right;
}
