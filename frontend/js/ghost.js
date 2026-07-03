//! Ghost text 补全 overlay（0.8.1 §2.6）。
//!
//! **视觉分工**（对齐 Raycast / Warp / VS Code Copilot 做法）：
//! - **本模块（overlay）**：只负责在输入框里画"影子" ghost text（灰色 `→ fanyi`），
//!   不放键帽——ghost text 是"passive suggestion"语义，塞 [Tab] 按钮徽章会破坏视觉。
//! - **statusbar**：承载"按 [Tab] 接受"提示，用键帽 chip 表达"active UI"语义。
//!   两个层的视觉语言分开，各司其职。
//!
//! 数据流：`search.js` 在 `search_apps` 返回后调 `update(query, completionHint)`；
//! `hasHint()` 供 statusbar / keyboard 层查询是否处于可接受态；
//! 用户按 Tab（或 ArrowRight，视 `autosuggest_tab_key` 配置）时 `keyboard.js` 调
//! `acceptCurrent()` 把输入替换为 `hint.replacement` 并触发一次 input 事件（进入
//! 下一轮搜索，走完整 Takeover）。
//!
//! **两种 hint 形态**：
//! - `display` 非空（补全场景 `fy` → `fanyi`）：overlay 渲染 ` → fanyi`（灰影）
//! - `display` 为空（已完整无尾空格 `fanyi`）：overlay 不渲染任何字符，仅由 statusbar
//!   提示"按 [Tab] 进入参数模式"
//!
//! **Ghost 是"发现工具"，不是"召回工具"**：
//! - 部分拼音 `fan hello` 不进 route 匹配（翻译插件不出现在候选），但走独立 fuzzy
//!   通道触发 ghost `→ fanyi`。用户 Tab 后重新触发搜索时才命中 Takeover。

import { queryEl } from "./dom.js";

let currentHint = null; // { replacement, display, prefixLen }
let ghostTypedEl = null;
let ghostSuggestEl = null;

// hint 状态订阅者（statusbar 层）：状态变化时回调，参数为最新 hint（null 表示清空）。
// 一次一个订阅者就够了（当前只有 statusbar 消费）。多订阅者需求出现时再扩数组。
let onChangeCallback = null;

/** 初始化：绑定 overlay DOM。main.js 启动时调一次。 */
export function init() {
  ghostTypedEl = document.querySelector("#ghost-overlay .ghost-typed");
  ghostSuggestEl = document.querySelector("#ghost-overlay .ghost-suggest");
}

/**
 * 订阅 hint 状态变化。statusbar.js 在初始化时调用，收到回调后重绘状态栏。
 * @param {(hint: object|null) => void} cb
 */
export function onChange(cb) {
  onChangeCallback = cb;
}

function notify() {
  if (onChangeCallback) onChangeCallback(currentHint);
}

/** 更新 ghost 显示。hint 为 null/undefined 时清空。 */
export function update(query, hint) {
  const prev = currentHint;
  currentHint = hint || null;
  if (!ghostTypedEl || !ghostSuggestEl) return;
  if (!hint) {
    ghostTypedEl.textContent = "";
    ghostSuggestEl.textContent = "";
    if (prev) notify();
    return;
  }
  ghostTypedEl.textContent = query;

  // 只在补全场景（display 非空）画影子文字；已完整场景（display 为空）
  // overlay 保持空——用户已看到自己的完整输入，加任何影子都是冗余；提示交给 statusbar。
  ghostSuggestEl.textContent = hint.display ? ` → ${hint.display}` : "";
  notify();
}

/** 清空 ghost（reset / 窗口 hide / 用户 Esc 时调）。 */
export function clear() {
  const prev = currentHint;
  currentHint = null;
  if (ghostTypedEl) ghostTypedEl.textContent = "";
  if (ghostSuggestEl) ghostSuggestEl.textContent = "";
  if (prev) notify();
}

/**
 * 接受当前补全提示：把输入替换为 `hint.replacement` 并触发一次 input 事件走搜索路径。
 * 返回是否成功接受（无 hint 时返回 false，供 keyboard 层判断要不要 preventDefault）。
 *
 * 接受后立即 `clear()` 而不是等新一轮 search 返回覆盖——search 有 40ms debounce，
 * 期间旧 hint 若保留：输入框已是 `fanyi `（尾空格），statusbar 却还显示"按 Tab → fanyi"
 * ——视觉错位。清空是收敛终态：新 query `fanyi ` 尾空格触发 suggest 返回 None，
 * hint 保持空——正确。若新 query 仍能命中新 hint（如首拼再补全），下一轮回调自然回填。
 */
export function acceptCurrent() {
  if (!currentHint) return false;
  const rep = currentHint.replacement;
  queryEl.value = rep;
  queryEl.setSelectionRange(rep.length, rep.length);
  clear();
  // 派发 input 事件，让 search.js 的 onInput 走一遍——重新算 route + ghost。
  queryEl.dispatchEvent(new Event("input", { bubbles: true }));
  return true;
}

/** 是否有活跃 hint（keyboard / statusbar 层查询）。 */
export function hasHint() {
  return currentHint !== null;
}

/** 当前 hint 的 display（statusbar 展示 "→ fanyi" 时用）。 */
export function currentDisplay() {
  return currentHint?.display || "";
}
