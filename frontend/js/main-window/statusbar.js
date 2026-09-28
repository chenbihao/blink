//! ResultFooter（0.24.4 §5.5；元素 id 沿用 #statusbar，避免无谓的 DOM 迁移）。
//!
//! **职责收窄**：只显示结果导航/动作提示（左）+ 翻页（右）+ 空 query 闲置态的
//! Chord 一行提示（受 ChordConfig.chord_hint_visible 控制）。建议采纳提示移居
//! SuggestionBar（suggestion-bar.js），Chord 完整键帽表移居 Cheat Sheet
//! （cheat-sheet.js，Alt 按住时）。
//!
//! 另承载剪贴板多选等**持续状态覆写**（setOverride——非瞬态，与
//! TransientFeedback 的 loading/success/error 分工不同）。
//!
//! **文案键帽化**：含键位的提示全部走 i18n 模板 `{{key:X}}` + `renderHint`。
//! `#statusbar { min-height }` 稳定基线高度，避免切换时窗口抖动。

import {actionHint} from "./hints.js";
import {t} from "../i18n/index.js";
import * as chord from "./chord.js";
import {renderHint} from "../shared/kbd.js";
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
    // chord 配置刷新/Alt 可见性变化 → 闲置行可能增减 → 重绘 + 窗口 resize
    chord.onVisibilityChange(() => {
        render();
        syncWindowSize();
    });
    // 闲置态（无结果）下首字符输入：闲置行条件翻转，立即重绘不等 40ms 防抖
    queryEl.addEventListener("input", () => {
        if (!lastActive) render();
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

    // 可见条件：有左侧内容（导航/覆写/闲置行）或有翻页
    if (!left && !right) {
        el.classList.remove("visible");
        return;
    }
    el.classList.add("visible");
}

/** 左侧内容：覆写 > 导航提示 > 闲置 Chord 一行；均无则 null（整条隐藏）。 */
function buildLeft() {
    // 1. 持续状态覆写（剪贴板多选计数）
    if (overrideText) {
        const span = document.createElement("span");
        span.className = "hint-primary";
        span.textContent = overrideText;
        return span;
    }

    // 2. 常规态：导航 · Alt+数字 · 动作提示——三段用 · 分隔，各段都走 renderHint 支持键帽。
    if (lastActive) {
        const primary = document.createElement("span");
        primary.className = "hint-primary";
        primary.appendChild(renderHint(t("hint.navigate")));
        primary.appendChild(document.createTextNode(" · "));
        primary.appendChild(renderHint(t("hint.alt_number")));
        primary.appendChild(document.createTextNode(" · "));
        const {template, params} = actionHint(lastActive.actions?.[0], lastClipboardMode);
        primary.appendChild(renderHint(template, params));
        return primary;
    }

    // 3. 空 query 闲置态：Chord 一行提示（0.24.4 §5.5，受 chord_hint_visible 控制；
    //    chord.getActions 在开关关闭时返回 []）。空文本时过滤 hint_hidden_when_empty
    //    的 contextual 动作（与旧 ghost-chord 闲置行为一致）。
    const idle = buildIdleChordLine();
    return idle;
}

/** 闲置 Chord 一行：空 query + chord 可用动作非空时返回一行键帽，否则 null。 */
function buildIdleChordLine() {
    if (queryEl.value.trim()) return null;
    const actions = chord.getActions().filter((a) => !a.hint_hidden_when_empty);
    if (!actions.length) return null;

    const line = document.createElement("span");
    line.className = "hint-primary hint-idle-chord";
    actions.forEach((a, i) => {
        if (i > 0) {
            const sep = document.createElement("span");
            sep.className = "chord-sep";
            sep.textContent = "│";
            line.appendChild(sep);
        }
        // 紧凑格式：单 kbd 显示键名（Alt 修饰键隐含，省空间）——完整键帽表走 Cheat Sheet
        const kbd = document.createElement("kbd");
        kbd.className = "kbd chord-key";
        kbd.textContent = chord.chordKeyLabel(a.key);
        line.appendChild(kbd);
        const label = document.createElement("span");
        label.className = "chord-label";
        label.textContent = a.label;
        line.appendChild(label);
    });
    return line;
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
