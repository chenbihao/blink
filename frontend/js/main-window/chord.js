//! Chord 提示（0.8.5 §6.1 / §6.4 视觉重构）：
//! 按住 Alt + 用户未开始交互（chordEligible: query 空 + 结果空）时，
//! 在 ghost overlay 里以影子形式提示可用 Alt+字母 动作。
//!
//! **视觉分工**（与 ghost 模块的分层）：
//! - `.ghost-suggest`（ghost.js 管）：keyword/context 补全 → 强意图信号，优先展示
//! - `.ghost-chord`（本模块管）：待命入口列表 → 弱信号，仅在 suggest 空 + chord-visible 时显示
//! CSS `:has()` 保证同时命中时 chord 让位 suggest——不重叠、不撑布局。
//!
//! **渲染**：走 `kbd.js::renderCombo` 工具生成键帽（与 statusbar / 设置页同源），
//! 组合键内嵌 `+` 让"Alt+A 是一组"的心智清晰；不同 chord 之间用竖线加大间距的分隔符。
//! 详见 product-principles-原则 §14.7 键盘提示样式统一。
//!
//! **开关**（0.8.5.1 §6.6）：
//! - `chord_enabled=false` → refresh 直接清空 actions,不 render;keyboard.js 的
//!   `chordEligible` 通过 `isEnabled()` 读同一 flag,保触发链一致
//! - `chord_hint_visible=false` → refresh 清空 ghost-chord DOM,但 actions 仍在（用户
//!   仍可 Alt+字母 触发,只是不显示提示）
//!
//! 悬浮球形态（Alt+Q 划词）是独立 webview（chord-ball.html），不在此模块。
//!
//! 0.11: Alt+Q 划词翻译 chord 已移除，chord-ball 悬浮球已删除。Alt+Space 语音输入
//! 作为 display-only chord 条目加入提示条（触发仍走 native hotkey hold，不走 trigger_chord）。

import { invoke } from "../shared/tauri.js";
import { listChordActions } from "../shared/api.js";

let chordActions = [];
let ghostChordEl = null;

// 配置快照（lifecycle shown 或 config-changed 时刷新）
// 初值 false：config 还没到时保守禁用,避免"用户没开却弹提示"的一瞬闪现
let chordEnabled = false;
let hintVisible = true;

/** 初始化：绑定 overlay DOM。main.js 启动时调一次。 */
export function init() {
  ghostChordEl = document.querySelector("#ghost-overlay .ghost-chord");
}

/** 是否启用 Chord（keyboard.js chordEligible 读此值,统一门禁）。 */
export function isEnabled() {
  return chordEnabled;
}

/** 当前 Chord 动作列表（statusbar 降级渲染用）。 */
export function getActions() {
  return chordEnabled && hintVisible ? chordActions : [];
}

/**
 * 当前生效的 tap 语义 chord 键集合（0.10.7）。
 *
 * keyboard.js 的 `onChordTrigger` 用此集合判断 Alt+字母是否触发 chord。
 * 只含 `semantic === "tap"` 的动作——hold 语义（如语音输入 Alt+Space）
 * 由 native hotkey hook 的 hold 状态机处理，不走前端 keydown 路径。
 *
 * 键已 toLowerCase，与 `e.key.toLowerCase()` 直接比对。
 */
export function getTapKeys() {
  const set = new Set();
  if (!chordEnabled) return set;
  for (const a of chordActions) {
    if (a.semantic === "tap") {
      set.add(String(a.key).toLowerCase());
    }
  }
  return set;
}

// ── 可见性变化回调（statusbar 订阅用）─────────────────────────────────────
let onVisibilityChangeCallback = null;

/** 订阅 chord-visible 状态变化（statusbar.init 调一次）。 */
export function onVisibilityChange(cb) {
  onVisibilityChangeCallback = cb;
}

/** keyboard.js setAlt() 调用，通知订阅者重绘。 */
export function notifyVisibilityChange() {
  if (onVisibilityChangeCallback) onVisibilityChangeCallback();
}

/** 拉取 Chord 动作列表并渲染（shown / config-changed 时调）。 */
export async function refresh() {
  // 先刷新配置快照
  try {
    const cfg = await invoke("get_config");
    if (cfg) {
      // 0.8.7:chord 默认关,读取用 === true 精确判定,不再"缺失当 true"
      chordEnabled = cfg.chord_enabled === true;
      hintVisible = cfg.chord_hint_visible !== false;
    }
  } catch (e) {
    /* 保持默认 false */
  }

  if (!chordEnabled) {
    // 总开关关 → 清空,不 render
    chordActions = [];
    if (ghostChordEl) ghostChordEl.replaceChildren();
    return;
  }

  try {
    chordActions = await listChordActions();
  } catch (e) {
    console.warn("[chord] list_chord_actions 失败", e);
    chordActions = [];
  }
  render();
}

/**
 * 把 chord action 的 key 转成渲染用的键名。
 * key=' '（语音输入）→ "Space"；其它直接大写。
 * kbd.js 的 normalize() 会把 "Space" 映射到 KEY_META.space → 显示 "Space"。
 */
export function chordKeyLabel(key) {
  return key === " " ? "Space" : key.toUpperCase();
}

function render() {
  if (!ghostChordEl) return;
  ghostChordEl.replaceChildren();
  // hint_visible=false 时不 render 提示条（触发仍生效）
  if (!hintVisible) return;
  if (!chordActions.length) return;

  // 前导两个非断行空格避免紧贴用户光标位（overlay whitespace: pre 保留）
  ghostChordEl.appendChild(document.createTextNode("  "));

  chordActions.forEach((a, i) => {
    if (i > 0) {
      const sep = document.createElement("span");
      sep.className = "chord-sep";
      sep.textContent = "│"; // 竖线,比 · 视觉更强,能区分不同 chord 分组
      ghostChordEl.appendChild(sep);
    }
    const item = document.createElement("span");
    item.className = "chord-item";
    // 紧凑格式：单 kbd 显示键名（Alt 修饰键隐含，省空间）
    const kbd = document.createElement("kbd");
    kbd.className = "kbd chord-key";
    kbd.textContent = chordKeyLabel(a.key);
    item.appendChild(kbd);
    const label = document.createElement("span");
    label.className = "chord-label";
    label.textContent = a.label;
    item.appendChild(label);
    ghostChordEl.appendChild(item);
  });
}
