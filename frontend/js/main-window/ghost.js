//! Ghost text 补全 overlay（0.8.1 §2.6 / 0.8.3 §4.9；0.24.1 契约更新）。
//!
//! **视觉分工**（对齐 Raycast / Warp / VS Code Copilot 做法）：
//! - **本模块（overlay）**：只负责在输入框里画"影子" ghost text（灰色 `→ fanyi`），
//!   不放键帽——ghost text 是"passive suggestion"语义，塞 [Tab] 按钮徽章会破坏视觉。
//! - **statusbar**：承载"按 [Tab] 接受"提示，用键帽 chip 表达"active UI"语义。
//!   两个层的视觉语言分开，各司其职。
//!
//! 数据流：`search.js` 在 `search_apps` 返回后调 `update(query, suggestion)`
//! （0.24.1 起传 SuggestionSet.primary）；`hasHint()` 供 statusbar / keyboard 层查询
//! 是否处于可接受态；用户按 Tab（或 ArrowRight，视 `autosuggest_tab_key` 配置）时
//! `keyboard.js` 调 `acceptCurrent()` 执行建议携带的类型化 `action`
//! （0.24 §3.5：RouteQuery=改 query 重搜 / EnterAiMode=进 AI 模式）。
//!
//! **0.24.1 契约**：suggestion 形如 `{ id, kind, action, rankScore, display, prefixLen }`。
//! 按 `kind` 分视觉通道（弱区分 §4.9）：
//! - `completion`：输入补全（`fy` → `fanyi`）——常规灰度，`->` 前缀。
//! - `translate`：环境感知（选中英文 → 翻译）——不画影子（0.16.1），提示走 statusbar。
//! - `askAi`：AI 触发（"按 Tab 问 AI"）——灰影无前缀 + ghost-context 样式。
//! `action` 是类型化采纳动作：`{routeQuery:{query}}` / `{enterAiMode:{prompt}}`。
//!
//! **两种 hint 形态**（沿用 0.8.1）：
//! - `display` 非空（补全场景 `fy` → `fanyi`）：overlay 渲染灰影
//! - `display` 为空（已完整无尾空格 `fanyi`）：overlay 不渲染任何字符,仅由 statusbar
//!   提示"按 [Tab] 进入参数模式"
//!
//! **Ghost 是"发现工具"，不是"召回工具"**：
//! - 部分拼音 `fan hello` 不进 route 匹配（翻译插件不出现在候选），但走独立 fuzzy
//!   通道触发 ghost `→ fanyi`。用户 Tab 后重新触发搜索时才命中 Takeover。
//! - Translate 类同理：Context 命中不进 route()（不产 candidate），只出 Ghost。

import {queryEl} from "./dom.js";
import * as aiMode from "./ai-mode.js";
import {reportSuggestionAdoption} from "../shared/api.js";

// 当前 suggestion（SuggestionSet.primary），形如 { id, kind, action, rankScore, display, prefixLen, origin }
let currentSuggestion = null;
// 渲染当前 suggestion 时的 search seq（SuggestionSet.revision，0.24 §3.6 过期防护）。
// null = 无建议或未携带 revision（防御：后端契约保证有 set 才有 primary）。
let currentRevision = null;
// 过期校验回调（main.js 注入 search.isLiveRevision——避免 ghost→search 反向 import 成环）。
let stalenessCheck = null;
let ghostTypedEl = null;
let ghostSuggestEl = null;
let ghostOverlayEl = null;

// hint 状态订阅者（statusbar 层）：状态变化时回调，参数为最新 suggestion（null 表示清空）。
// 一次一个订阅者就够了（当前只有 statusbar 消费）。多订阅者需求出现时再扩数组。
let onChangeCallback = null;

// ── 冻结机制（0.10 语音预览独占 overlay）──────────────────────────────────────
// 语音录音期间 voice-partial 直接写 ghostSuggest 显示 preview 文本。
// 0.10.7 起 voice-partial 不再 dispatch input（避免录音期间触发无意义空 query 搜索），
// 但 awareness-updated（剪贴板变化等）→ search.retrigger() → fetchContextSuggestions
// → ghost.update() 仍可能在录音期间发生并覆写 overlay → 闪屏。freeze 后 update/clear
// 只更新内部状态 + notify，不碰 DOM；unfreeze 时恢复当前 suggestion 到 DOM。
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
 * （旧公式 `maxScroll - margin` 反而往左滚，把文本末尾推出可见区域，导致用户看不到最新输入）
 *
 * 因此文本溢出时直接滚到 `maxScroll`（文本末尾贴右边缘可见），
 * 影子文本被 `overflow: hidden` 裁切——文本可见性优先于影子可见性。
 * 文本不溢出时不做任何事（maxScroll <= 0），影子和文本都自然可见。
 *
 * @param {number} [ratio=0.8] - 预留参数（当前未使用）。`<input>` 无法在文本末尾
 *   右侧留空间，ratio 仅在未来换用可换行元素时才有意义。
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
 * 订阅 hint 状态变化。statusbar.js 在初始化时调用，收到回调后重绘状态栏。
 * @param {(sug: object|null) => void} cb
 */
export function onChange(cb) {
    onChangeCallback = cb;
}

/**
 * 注入过期校验回调（0.24 §3.6）。main.js 启动时接 `search.isLiveRevision`——
 * ghost 反向 import search 会成环（search 已 import ghost），经注入解耦。
 * @param {(revision: number) => boolean} fn 返回 false = 建议已过期
 */
export function setStalenessCheck(fn) {
    stalenessCheck = fn;
}

function notify() {
    if (onChangeCallback) onChangeCallback(currentSuggestion);
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
    if (!currentSuggestion) {
        ghostTypedEl.textContent = "";
        ghostSuggestEl.textContent = "";
        ghostSuggestEl.classList.remove("ghost-context");
        queryEl.removeAttribute("data-ghost-active");
        return;
    }
    ghostTypedEl.textContent = query;

    // 只在补全场景（display 非空）画影子文字；已完整场景（display 为空）
    // overlay 保持空--用户已看到自己的完整输入，加任何影子都是冗余；提示交给 statusbar。
    //
    // 0.24.1：按 kind 分支（原 source 字段退役）。
    // 0.16.1：Translate 类不再画影子文字（环境感知是弱信号，不应占输入框影子位打扰
    // 用户）。overlay 保持空，采纳提示只走 statusbar。currentSuggestion 状态仍正常
    // 持有--hasHint()/currentDisplay()/currentOrigin() 供 statusbar 读取。
    if (!currentSuggestion.display) {
        ghostSuggestEl.textContent = "";
    } else if (currentSuggestion.kind === "translate") {
        // 0.16.1：translate（原 context）不画影子，只持有状态供 statusbar 提示
        ghostSuggestEl.textContent = "";
    } else if (currentSuggestion.kind === "askAi") {
        ghostSuggestEl.textContent = ` ${currentSuggestion.display}`;
    } else {
        ghostSuggestEl.textContent = ` -> ${currentSuggestion.display}`;
    }
    ghostSuggestEl.classList.toggle(
        "ghost-context",
        currentSuggestion.kind === "askAi",
    );
    // 0.16.1：translate 类不画影子，也不需要 ghost-context 样式（影子为空）
    if (currentSuggestion.display && currentSuggestion.kind !== "translate") {
        // 0.16.1：translate 类不画影子，不设 data-ghost-active（overlay 无视觉变化），
        // 也不需要 scrollWithMargin（没有影子要留空间）。但 currentSuggestion 仍持有，
        // hasHint() 返回 true，statusbar 会展示采纳提示。
        queryEl.setAttribute("data-ghost-active", "");
        // 0.10.6: 有影子时调整滚动留出右侧空间给 preview 文本
        scrollWithMargin();
    } else {
        queryEl.removeAttribute("data-ghost-active");
        syncScroll();
    }
}

/** 更新 ghost 显示。suggestion 为 null/undefined 时清空。
 * revision 为该 SuggestionSet 的 search seq（0.24 §3.6，采纳时校验）。 */
export function update(query, suggestion, revision) {
    const prev = currentSuggestion;
    currentSuggestion = suggestion || null;
    currentRevision = revision ?? null;
    lastQuery = query;
    if (!ghostTypedEl || !ghostSuggestEl) return;
    if (frozen) {
        // 语音录音期间：voice-partial 独占 overlay DOM，只更新状态 + notify
        if (prev !== currentSuggestion) notify();
        return;
    }
    renderToDom(query);
    if (prev !== currentSuggestion || suggestion) notify();
}

/** 清空 ghost（reset / 窗口 hide / 用户 Esc 时调）。 */
export function clear() {
    const prev = currentSuggestion;
    currentSuggestion = null;
    currentRevision = null;
    lastQuery = "";
    if (frozen) {
        if (prev) notify();
        return;
    }
    if (ghostTypedEl) ghostTypedEl.textContent = "";
    if (ghostSuggestEl) {
        ghostSuggestEl.textContent = "";
        ghostSuggestEl.classList.remove("ghost-context");
    }
    queryEl.removeAttribute("data-ghost-active");
    if (prev) notify();
}

/**
 * 接受当前建议。行为按 action 分派（0.24 §3.5 类型化动作）：
 *
 * - `routeQuery`（Completion / Translate）：把输入替换为 `action.query` 并触发一次
 *   input 事件走搜索路径，触发下一轮 search_apps。
 * - `enterAiMode`（AskAi，0.9.2 Phase 5b / 0.17.6 重构）：**不改输入框**，直接进入
 *   AI 模式（aiMode.enterAiMode），由 ChatService 接管对话流。
 *
 * 接受后立即 `clear()` 而不是等新一轮 search 返回覆盖——search 有 40ms debounce，
 * 期间旧 hint 若保留：输入框已是 `fanyi `（尾空格），statusbar 却还显示"按 Tab → fanyi"
 * ——视觉错位。清空是收敛终态：新 query `fanyi ` 尾空格触发 suggest 返回 None，
 * hint 保持空——正确。若新 query 仍能命中新 hint（如首拼再补全），下一轮回调自然回填。
 *
 * 返回是否成功接受（无 suggestion / 无可执行 action 时返回 false，
 * 供 keyboard 层判断要不要 preventDefault）。
 */
export function acceptCurrent() {
    if (!currentSuggestion) return false;

    // 0.24 §3.6 过期防护：revision 与前端当前 seq 不等 → 本地拒绝（零 IPC、零延迟）。
    // 建议来自较早一轮搜索（用户已继续输入）——视为无建议，Tab 不被吞。
    if (stalenessCheck && !stalenessCheck(currentRevision)) {
        clear();
        return false;
    }

    const action = currentSuggestion.action;
    // 采纳遥测（§3.6 单向 fire-and-forget）：本地执行后上报，失败静默
    const adopted = {id: currentSuggestion.id, revision: currentRevision};
    const report = () => {
        reportSuggestionAdoption(adopted.id, "primary", adopted.revision).catch(() => {});
    };

    // AskAi 类:进入 AI 模式（0.17.6: 改走 ChatService，不再调 trigger_ai）
    if (action?.enterAiMode) {
        const {prompt} = action.enterAiMode;
        clear();
        report();
        aiMode.enterAiMode(prompt);
        return true;
    }

    if (action?.routeQuery) {
        const rep = action.routeQuery.query;
        queryEl.value = rep;
        queryEl.setSelectionRange(rep.length, rep.length);
        clear();
        report();
        // 派发 input 事件，让 search.js 的 onInput 走一遍——重新算 route + ghost。
        queryEl.dispatchEvent(new Event("input", {bubbles: true}));
        return true;
    }

    // 防御：后端契约保证 action 必填；缺失时视为不可采纳，避免误吞 Tab
    clear();
    return false;
}

/** 是否有活跃 suggestion（keyboard / statusbar 层查询）。 */
export function hasHint() {
    return currentSuggestion !== null;
}

/** 当前 suggestion 的 display（statusbar 展示 "→ fanyi" 时用）。 */
export function currentDisplay() {
    return currentSuggestion?.display || "";
}

/** 当前 suggestion 的 kind（statusbar 按语义类分文案时用）。
 *  值为 "completion" | "translate" | "askAi" | null（0.24.1 起 kind 取代 source）。 */
export function currentKind() {
    return currentSuggestion?.kind || null;
}

/** 当前 suggestion 的 origin（Context 类才有，statusbar 用来展示"来自划词/剪贴板"）。
 *  值为 "selection" | "clipboard" | null。Keyword 类恒 null。 */
export function currentOrigin() {
    return currentSuggestion?.origin || null;
}

// ── 冻结 API（0.10 语音预览独占 overlay）──────────────────────────────────────

/**
 * 冻结 overlay DOM 写入。语音录音开始时调。
 * update/clear 只更新内部状态 + notify，不碰 DOM。
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
