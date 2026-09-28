//! CompletionGhost（0.8.1 §2.6 / 0.8.3 §4.9；0.24.4 收窄为输入延伸）。
//!
//! **0.24.4 视觉分工**（§5.5 五类 UI 语义）：本 overlay 只渲染**输入的自然延伸**——
//! Completion 影子文字（`fy` → `fanyi`）与语音转写预览（组字中"用户自己的话"，
//! 不穿信任边界）。Translate / AskAi 建议走 #suggestion-bar（suggestion-bar.js），
//! Chord 键帽走 .ghost-chord（chord.js 渲染，CSS :has() 与补全影子互斥）。
//!
//! 数据流：`search.js` 在 `search_apps` 返回后调 `update(query, suggestion, revision)`
//! （只传 primary 为 Completion 的建议）；用户按 Tab（或 ArrowRight，视
//! `autosuggest_tab_key` 配置）时 `keyboard.js` 调 `acceptCurrent()`，委托
//! `suggestion-accept.js` 的统一采纳路径（0.24 §3.5/§3.6）。
//!
//! **两种 hint 形态**（沿用 0.8.1）：
//! - `display` 非空（补全场景 `fy` → `fanyi`）：overlay 渲染灰影
//! - `display` 为空（已完整无尾空格 `fanyi`）：overlay 不渲染任何字符，
//!   采纳提示由 SuggestionBar 承载（bar 渲染非 Completion 槽位）
//!
//! **Ghost 是"发现工具"，不是"召回工具"**：
//! - 部分拼音 `fan hello` 不进 route 匹配（翻译插件不出现在候选），但走独立 fuzzy
//!   通道触发 ghost `→ fanyi`。用户 Tab 后重新触发搜索时才命中 Takeover。

import {queryEl} from "./dom.js";
import {acceptSuggestion} from "./suggestion-accept.js";

// 当前 suggestion（SuggestionSet.primary 中 kind=completion 的建议），
// 形如 { id, kind, action, rankScore, display, prefixLen, origin }
let currentSuggestion = null;
// 渲染当前 suggestion 时的 search seq（SuggestionSet.revision，0.24 §3.6 过期防护）。
// null = 无建议或未携带 revision（防御：后端契约保证有 set 才有 primary）。
let currentRevision = null;
let ghostTypedEl = null;
let ghostSuggestEl = null;
let ghostOverlayEl = null;

// ── 冻结机制（0.10 语音预览独占 overlay）──────────────────────────────────────
// 语音录音期间 voice-partial 直接写 ghostSuggest 显示 preview 文本。
// 0.10.7 起 voice-partial 不再 dispatch input（避免录音期间触发无意义空 query 搜索），
// 但 awareness-updated（剪贴板变化等）→ search.retrigger() → fetchContextSuggestions
// → ghost.update() 仍可能在录音期间发生并覆写 overlay → 闪屏。freeze 后 update/clear
// 只更新内部状态，不碰 DOM；unfreeze 时恢复当前 suggestion 到 DOM。
let frozen = false;
let lastQuery = "";

/**
 * 同步 ghost overlay 的水平滚动位置到 #query。
 *
 * `#query` 是 `<input>`，文本超长时浏览器自动水平滚动（光标保持可见）。
 * `#ghost-overlay` 是 `<div>` + `overflow: hidden`，不会自动滚动——
 * 如果不同步，ghost-typed（透明占位）从左边缘开始，ghost-suggest（影子）
 * 会被推到裁切区之外完全不可见。
 *
 * overflow:hidden 的元素支持 programmatic scrollLeft，所以只需在 input 滚动时
 * 把 scrollLeft 同步过来即可。（0.10.6）
 */
export function syncScroll() {
    if (ghostOverlayEl) {
        ghostOverlayEl.scrollLeft = queryEl.scrollLeft;
    }
}

/**
 * 确保输入框文本末尾（光标位置）在可见区域内，并同步 ghost overlay 滚动。（0.10.6）
 *
 * **设计约束**：`<input>` 的 `scrollLeft` 上限为 `maxScroll = scrollWidth - clientWidth`，
 * 此时文本末尾贴右边缘。无法在文本末尾右侧预留空间给影子——浏览器不允许滚过去。
 *
 * 因此文本溢出时直接滚到 `maxScroll`（文本末尾贴右边缘可见），
 * 影子文本被 `overflow: hidden` 裁切——文本可见性优先于影子可见性。
 * 文本不溢出时不做任何事（maxScroll <= 0），影子和文本都自然可见。
 */
export function scrollWithMargin(_ratio = 0.8) {
    const maxScroll = queryEl.scrollWidth - queryEl.clientWidth;
    if (maxScroll <= 0) {
        syncScroll();
        return;
    }
    queryEl.scrollLeft = maxScroll;
    syncScroll();
}

/** 初始化：绑定 overlay DOM + scroll 同步监听。main.js 启动时调一次。 */
export function init() {
    ghostOverlayEl = document.querySelector("#ghost-overlay");
    ghostTypedEl = document.querySelector("#ghost-overlay .ghost-typed");
    ghostSuggestEl = document.querySelector("#ghost-overlay .ghost-suggest");

    // 0.10.6: #query 水平滚动时同步 ghost overlay——覆盖打字 / IME / 语音输入所有场景
    queryEl.addEventListener("scroll", syncScroll);
}

/**
 * 同步 IME 组字文字到 ghost-typed（透明占位），让 ghost-suggest 跟随后移。
 * 中文输入法 composition 期间调用：拼音每变化一次就同步一次，避免 ghost 与输入重叠。
 * @param {string} text - 当前 IME 组字中的文字（可能带拼音/候选字）
 */
export function syncTypedText(text) {
    if (ghostTypedEl) ghostTypedEl.textContent = text;
    syncScroll();
}

/**
 * 将 currentSuggestion 渲染到 DOM（内部函数）。
 * 调用方保证 ghostTypedEl / ghostSuggestEl 已初始化。
 */
function renderToDom(query) {
    if (!currentSuggestion || !currentSuggestion.display) {
        // 无建议 / 已完整场景（display 为空）：overlay 不渲染字符——用户已看到
        // 自己的完整输入，加任何影子都是冗余；提示交给 SuggestionBar。
        ghostTypedEl.textContent = currentSuggestion ? query : "";
        ghostSuggestEl.textContent = "";
        queryEl.removeAttribute("data-ghost-active");
        syncScroll();
        return;
    }
    ghostTypedEl.textContent = query;
    ghostSuggestEl.textContent = ` -> ${currentSuggestion.display}`;
    queryEl.setAttribute("data-ghost-active", "");
    // 0.10.6: 有影子时调整滚动留出右侧空间给 preview 文本
    scrollWithMargin();
}

/**
 * 更新 ghost 显示。suggestion 为 null/undefined 时清空。
 * revision 为该 SuggestionSet 的 search seq（0.24 §3.6，采纳时校验）。
 * 0.24.4 起 search.js 只传 Completion 类建议（输入延伸），其余 kind 走 SuggestionBar。
 */
export function update(query, suggestion, revision) {
    currentSuggestion = suggestion || null;
    currentRevision = revision ?? null;
    lastQuery = query;
    if (!ghostTypedEl || !ghostSuggestEl) return;
    if (frozen) {
        // 语音录音期间：voice-partial 独占 overlay DOM，只更新状态不碰 DOM
        return;
    }
    renderToDom(query);
}

/** 清空 ghost（reset / 窗口 hide / 用户 Esc 时调）。 */
export function clear() {
    currentSuggestion = null;
    currentRevision = null;
    lastQuery = "";
    if (frozen) {
        return;
    }
    if (ghostTypedEl) ghostTypedEl.textContent = "";
    if (ghostSuggestEl) ghostSuggestEl.textContent = "";
    queryEl.removeAttribute("data-ghost-active");
}

/**
 * 接受当前建议（primary Completion）。委托 suggestion-accept.js 统一采纳路径
 * （0.24 §3.6：staleness 校验 → 类型化 action 本地执行 → 遥测），键盘与
 * SuggestionBar 点击共用同一实现。
 *
 * 返回是否成功接受（无 suggestion / 过期 / 无可执行 action 时返回 false，
 * 供 keyboard 层判断要不要 preventDefault）。
 */
export function acceptCurrent() {
    return acceptSuggestion(currentSuggestion, "primary", currentRevision, {
        onAccepted: () => clear(),
        onStale: () => clear(),
    });
}

/** 是否有活跃 suggestion（keyboard 层 Tab 分派查询：true = Tab 作用于 ghost）。 */
export function hasHint() {
    return currentSuggestion !== null;
}

// ── 冻结 API（0.10 语音预览独占 overlay）──────────────────────────────────────

/**
 * 冻结 overlay DOM 写入。语音录音开始时调。
 * update/clear 只更新内部状态，不碰 DOM。
 * voice-partial handler 直接管理 ghostSuggest.textContent 显示 preview。
 */
export function freeze() {
    frozen = true;
}

/**
 * 解冻并恢复当前 suggestion 到 DOM。语音录音结束时调。
 * 清除 voice-partial 残留的 voice-preview-text 样式后，用当前 suggestion 重绘。
 */
export function unfreeze() {
    frozen = false;
    if (ghostSuggestEl) {
        ghostSuggestEl.classList.remove("voice-preview-text");
    }
    if (ghostTypedEl && ghostSuggestEl) {
        renderToDom(lastQuery);
    }
    // renderToDom 内部已按有无 display 调用 scrollWithMargin / syncScroll
}
