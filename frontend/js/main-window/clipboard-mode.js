//! 剪贴板模式状态机（Alt+C 独占模式）。
//!
//! 与 AI 模式（Tab on Ghost 进入）、命令模式（`> ` 前缀进入）并列，
//! 构成主窗口三大独占模式之一。
//!
//! **设计动机**（0.19.15）：
//! 原 Alt+C 走 `CHORD_FILL_QUERY "剪贴板 "` → 前端填搜索框 →
//! SearchService::search → IntentRouter route → EngineTakeover → ClipboardEngine。
//! 整条链路多走路由检测 + keyword 剥离 + history 加载。
//!
//! 剪贴板模式直接 bypass SearchService pipeline：
//! Alt+C → `CHORD_ENTER_MODE { mode: "clipboard" }` → 前端进入模式 →
//! 用户输入直接调 `search_clipboard` IPC → ClipboardEngine。
//!
//! **模式切换铁则**（§3.5）：进入剪贴板模式后，搜索逻辑完全切支
//! （不触发 searchApps、不显示 ghost）。退出则完整恢复。
//!
//! **UX**：
//! - 进入：输入框清空 + placeholder 变为 "搜索剪贴板历史…" + 模式徽章
//! - 输入：每个字符直接调 searchClipboard（40ms 防抖）
//! - ESC：退出剪贴板模式 → 回到正常搜索（不隐藏窗口）
//! - 选中外壳隐藏 / SHOWN / HIDDEN：自动复位

import { queryEl } from "./dom.js";
import * as results from "./results.js";
import * as ghost from "./ghost.js";
import { syncWindowSize } from "./window-size.js";
import { searchClipboard } from "../shared/api.js";
import { t } from "../i18n/index.js";

/** 当前是否处于剪贴板模式。 */
let active = false;

/** 原始 placeholder（退出时恢复）。 */
let savedPlaceholder = "";

/** 防抖定时器。 */
let timer = null;

/** 请求序号（防竞态，与 search.js 独立）。 */
let seq = 0;

/** 模式徽章 DOM 元素。 */
let badgeEl = null;

// ── 状态查询 ──────────────────────────────────────────────────────────────

/** 当前是否处于剪贴板模式。 */
export function isActive() {
  return active;
}

// ── 生命周期 ──────────────────────────────────────────────────────────────

/** 初始化：创建徽章 DOM 元素。main.js 启动时调一次。 */
export function init() {
  badgeEl = document.createElement("div");
  badgeEl.id = "clipboard-mode-badge";
  badgeEl.className = "clipboard-mode-badge hidden";
  badgeEl.innerHTML =
    '<svg class="icon"><use href="#icon-copy"/></svg><span>剪贴板</span>';
  const searchMode = document.getElementById("search-mode");
  searchMode.appendChild(badgeEl);
}

/** 复位：退出剪贴板模式（lifecycle shown/hidden 调用）。 */
export function reset() {
  if (active) {
    exit();
  }
}

// ── 模式切换 ──────────────────────────────────────────────────────────────

/**
 * 进入剪贴板模式。
 * 清空输入框 + 切换 placeholder + 显示徽章 + 立即拉取最近剪贴板历史。
 */
export function enter() {
  if (active) return;
  active = true;

  // 保存并切换 placeholder
  savedPlaceholder = queryEl.placeholder;
  queryEl.placeholder = t("clipboard.mode_placeholder");

  // 清空输入框
  queryEl.value = "";
  queryEl.focus();

  // 显示徽章
  if (badgeEl) {
    badgeEl.classList.remove("hidden");
  }

  // 清空 ghost + 结果
  ghost.clear();
  results.clear();

  // 标记 body——CSS 据此隐藏 chord 提示（独占模式下不显示 Alt+字母 待命列表）
  document.body.classList.add("clipboard-mode-active");

  // 立即拉取最近剪贴板历史（空 query）
  doSearch("");

  syncWindowSize();
}

/**
 * 退出剪贴板模式。
 * 恢复 placeholder + 隐藏徽章 + 清结果 + 恢复正常搜索。
 */
export function exit() {
  if (!active) return;
  active = false;

  // 恢复 placeholder
  queryEl.placeholder = savedPlaceholder;

  // 隐藏徽章
  if (badgeEl) {
    badgeEl.classList.add("hidden");
  }

  // 取消防抖
  clearTimeout(timer);
  seq++;

  // 移除 body 标记——恢复 chord 提示可见性
  document.body.classList.remove("clipboard-mode-active");

  // 清空输入框 + 结果
  queryEl.value = "";
  results.clear();
  ghost.clear();

  // 恢复正常搜索的 Context Suggestion
  // 不直接调 search.fetchContextSuggestions 避免循环依赖，
  // 由调用方（keyboard.js ESC / lifecycle reset）触发
  syncWindowSize();
}

// ── 输入处理 ──────────────────────────────────────────────────────────────

/**
 * 处理输入变化，在剪贴板模式下拦截搜索。
 * 由 search.js::onInput 在搜索逻辑之前调用。
 *
 * @param {string} value 当前输入框值
 * @returns {boolean} true = 剪贴板模式已处理（search.js 应跳过搜索）；false = 非剪贴板模式（继续搜索）
 */
export function handleInput(value) {
  if (!active) return false;

  // 40ms 防抖（与正常搜索一致，合并极快连打）
  clearTimeout(timer);
  const q = value.trim();
  timer = setTimeout(() => {
    doSearch(q);
  }, 40);

  return true;
}

// ── 内部 ──────────────────────────────────────────────────────────────────

/**
 * 调用 search_clipboard IPC 并渲染结果。
 * @param {string} query 搜索词（已 trim）
 */
async function doSearch(query) {
  const mySeq = ++seq;
  try {
    const resp = await searchClipboard(query, mySeq);
    // 竞态防护：用户已输入新 query 或已退出模式
    if (mySeq !== seq) return;
    if (!active) return;
    results.render(resp.entries || [], mySeq);
  } catch (e) {
    console.error("[clipboard-mode] searchClipboard failed:", e);
  }
}
