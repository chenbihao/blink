//! Ghost text 补全 overlay（0.8.1 §2.6 / 0.8.3 §4.9）。
//!
//! **视觉分工**（对齐 Raycast / Warp / VS Code Copilot 做法）：
//! - **本模块（overlay）**：只负责在输入框里画"影子" ghost text（灰色 `→ fanyi`），
//!   不放键帽——ghost text 是"passive suggestion"语义，塞 [Tab] 按钮徽章会破坏视觉。
//! - **statusbar**：承载"按 [Tab] 接受"提示，用键帽 chip 表达"active UI"语义。
//!   两个层的视觉语言分开，各司其职。
//!
//! 数据流：`search.js` 在 `search_apps` 返回后调 `update(query, suggestion)`；
//! `hasHint()` 供 statusbar / keyboard 层查询是否处于可接受态；
//! 用户按 Tab（或 ArrowRight，视 `autosuggest_tab_key` 配置）时 `keyboard.js` 调
//! `acceptCurrent()` 把输入替换为 `suggestion.replacement` 并触发一次 input 事件（进入
//! 下一轮搜索，走完整 Takeover）。
//!
//! **0.8.3 契约变更**：入参从 0.8.1 的 `CompletionHint` 换成 `Suggestion { source, ... }`。
//! source 分两类,视觉弱区分（§4.9）：
//! - `keyword`：0.8.1 输入补全（`fy` → `fanyi`）——常规灰度。
//! - `context`：0.8.3 环境感知（选中英文 → 翻译）——更浅灰度,让用户分辨「环境猜」vs「打字补全」。
//!
//! **两种 hint 形态**（沿用 0.8.1）：
//! - `display` 非空（补全场景 `fy` → `fanyi` / Context `翻译 "the..."`）：overlay 渲染灰影
//! - `display` 为空（已完整无尾空格 `fanyi`）：overlay 不渲染任何字符,仅由 statusbar
//!   提示"按 [Tab] 进入参数模式"
//!
//! **Ghost 是"发现工具"，不是"召回工具"**：
//! - 部分拼音 `fan hello` 不进 route 匹配（翻译插件不出现在候选），但走独立 fuzzy
//!   通道触发 ghost `→ fanyi`。用户 Tab 后重新触发搜索时才命中 Takeover。
//! - 0.8.3 Context 类同理：Context 命中不进 route()（不产 candidate），只出 Ghost。

import { queryEl } from "./dom.js";
import { invoke } from "./api.js";
import * as search from "./search.js";

// 当前 suggestion，形如 { display, replacement, source, confidence, prefixLen }
let currentSuggestion = null;
let ghostTypedEl = null;
let ghostSuggestEl = null;

// hint 状态订阅者（statusbar 层）：状态变化时回调，参数为最新 suggestion（null 表示清空）。
// 一次一个订阅者就够了（当前只有 statusbar 消费）。多订阅者需求出现时再扩数组。
let onChangeCallback = null;

/** 初始化：绑定 overlay DOM。main.js 启动时调一次。 */
export function init() {
  ghostTypedEl = document.querySelector("#ghost-overlay .ghost-typed");
  ghostSuggestEl = document.querySelector("#ghost-overlay .ghost-suggest");
}

/**
 * 订阅 hint 状态变化。statusbar.js 在初始化时调用，收到回调后重绘状态栏。
 * @param {(sug: object|null) => void} cb
 */
export function onChange(cb) {
  onChangeCallback = cb;
}

function notify() {
  if (onChangeCallback) onChangeCallback(currentSuggestion);
}

/** 更新 ghost 显示。suggestion 为 null/undefined 时清空。 */
export function update(query, suggestion) {
  const prev = currentSuggestion;
  currentSuggestion = suggestion || null;
  if (!ghostTypedEl || !ghostSuggestEl) return;
  if (!suggestion) {
    ghostTypedEl.textContent = "";
    ghostSuggestEl.textContent = "";
    ghostSuggestEl.classList.remove("ghost-context");
    // Ghost 消失 → 恢复 placeholder（`data-ghost-active` 由 CSS 侧隐藏 placeholder,
    // 避免空 query + Context Ghost 时 placeholder 与 Ghost 文字重叠）
    queryEl.removeAttribute("data-ghost-active");
    if (prev) notify();
    return;
  }
  ghostTypedEl.textContent = query;

  // 只在补全场景（display 非空）画影子文字；已完整场景（display 为空）
  // overlay 保持空——用户已看到自己的完整输入，加任何影子都是冗余；提示交给 statusbar。
  //
  // 0.8.3：Context 类的 display 已是完整独立文本（"翻译 \"the...\""）,不需要 `→` 前缀。
  // Keyword 类保留 `→` 前缀（表达"补全为..."的语义）。
  // 0.9.2:AI 类 display 是 "按 Tab 问 AI",不属于补全语义,也不用 `→` 前缀。
  if (!suggestion.display) {
    ghostSuggestEl.textContent = "";
  } else if (suggestion.source === "context" || suggestion.source === "ai") {
    // Context / AI 类:完整独立文本 + 弱区分样式
    ghostSuggestEl.textContent = ` ${suggestion.display}`;
  } else {
    // Keyword 类（默认）：`→ fanyi` 补全语义
    ghostSuggestEl.textContent = ` → ${suggestion.display}`;
  }
  // 视觉弱区分（§4.9）：Context / AI 加 class,CSS 侧调更浅灰度
  ghostSuggestEl.classList.toggle(
    "ghost-context",
    suggestion.source === "context" || suggestion.source === "ai",
  );
  // 有内容 Ghost 时隐藏 placeholder（避免空 query 场景 placeholder 叠在 Ghost 上）
  if (suggestion.display) {
    queryEl.setAttribute("data-ghost-active", "");
  } else {
    queryEl.removeAttribute("data-ghost-active");
  }
  notify();
}

/** 清空 ghost（reset / 窗口 hide / 用户 Esc 时调）。 */
export function clear() {
  const prev = currentSuggestion;
  currentSuggestion = null;
  if (ghostTypedEl) ghostTypedEl.textContent = "";
  if (ghostSuggestEl) {
    ghostSuggestEl.textContent = "";
    ghostSuggestEl.classList.remove("ghost-context");
  }
  queryEl.removeAttribute("data-ghost-active");
  if (prev) notify();
}

/**
 * 接受当前补全提示。行为按 source 分:
 *
 * - `keyword` / `context`:把输入替换为 `suggestion.replacement` 并触发一次 input
 *   事件走搜索路径。触发下一轮 search_apps。
 * - `ai`(0.9.2 Phase 5b):**不改输入框**,直接 invoke `trigger_ai` command 让后端
 *   显式发起一次 completion(前端占位由后端 emit blink://results 补)。
 *   不复用 input 事件是为了避免"采纳 AI Ghost → 输入不变 → 又产 AI Ghost →
 *   再采纳..."的空转;AI 靠 Tab 一次触发 + 后端 spawn 单次调用。
 *   **seq 必须与 search.js 的 seq 同源**(见下方实现注释)。
 *
 * 接受后立即 `clear()` 而不是等新一轮 search 返回覆盖——search 有 40ms debounce，
 * 期间旧 hint 若保留：输入框已是 `fanyi `（尾空格），statusbar 却还显示"按 Tab → fanyi"
 * ——视觉错位。清空是收敛终态：新 query `fanyi ` 尾空格触发 suggest 返回 None，
 * hint 保持空——正确。若新 query 仍能命中新 hint（如首拼再补全），下一轮回调自然回填。
 *
 * 返回是否成功接受（无 suggestion 时返回 false，供 keyboard 层判断要不要 preventDefault）。
 */
export function acceptCurrent() {
  if (!currentSuggestion) return false;

  // AI 类:走独立触发路径(不改 input,不触发新搜索)
  if (currentSuggestion.source === "ai") {
    const query = currentSuggestion.replacement;
    // seq 必须与 search.js 的 seq 同源——`blink://results` listener 校验的是它。
    // 若这里自造 Date.now() 会撞不上,导致后端 emit 的占位/真结果**全部被前端丢弃**
    // (用户看到的现象:按 Tab 后 ghost 清空、statusbar 因 ghost.onChange 重绘退回
    //  常规态或整条 remove('visible') 隐藏,结果列表毫无反应)。
    const seq = search.getSeq();
    clear();
    invoke("trigger_ai", { query, seq }).catch((e) =>
      console.warn("[ghost] trigger_ai failed", e),
    );
    return true;
  }

  const rep = currentSuggestion.replacement;
  queryEl.value = rep;
  queryEl.setSelectionRange(rep.length, rep.length);
  clear();
  // 派发 input 事件，让 search.js 的 onInput 走一遍——重新算 route + ghost。
  queryEl.dispatchEvent(new Event("input", { bubbles: true }));
  return true;
}

/** 是否有活跃 suggestion（keyboard / statusbar 层查询）。 */
export function hasHint() {
  return currentSuggestion !== null;
}

/** 当前 suggestion 的 display（statusbar 展示 "→ fanyi" 时用）。 */
export function currentDisplay() {
  return currentSuggestion?.display || "";
}

/** 当前 suggestion 的 source（statusbar 按源分文案时用）。 */
export function currentSource() {
  return currentSuggestion?.source || null;
}

/** 当前 suggestion 的 origin（Context 类才有，statusbar 用来展示"来自划词/剪贴板"）。
 *  值为 "selection" | "clipboard" | null。Keyword 类恒 null。 */
export function currentOrigin() {
  return currentSuggestion?.origin || null;
}
