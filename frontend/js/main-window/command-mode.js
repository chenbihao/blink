//! 0.18.6 命令执行 MVP：`> ` 前缀命令模式状态机。
//!
//! 用户在主窗口输入 `> ` 前缀 → 进入命令模式（清结果、停搜索、停 ghost、显示 hint）；
//! 退格删 `>` → 恢复搜索模式。回车 → 调 `runInTerminal` 在外部终端执行，关主窗。
//!
//! **模式切换铁则**（§3.5）：进入命令模式后，结果区的渲染/键盘逻辑完全切支
//! （不触发 search、不显示 ghost、回车不走默认搜索项执行）。退出则完整恢复。
//!
//! 前缀解析纯函数与后端 `terminal.rs::is_command_mode` / `extract_command` 等价对齐，
//! Rust 侧为权威 spec + 单测载体，此处为运行时执行体。

import { queryEl } from "./dom.js";
import * as results from "./results.js";
import * as ghost from "./ghost.js";
import { syncWindowSize } from "./window-size.js";
import { runInTerminal, hideWindow } from "../shared/api.js";
import { t } from "../i18n/index.js";

/** 当前是否处于命令模式。 */
let active = false;

/** hint DOM 元素（动态创建，插入 #search-mode 内 #results 之后）。 */
let hintEl = null;

// ── 前缀解析纯函数（与 Rust terminal.rs 对齐）─────────────────────────────

/**
 * 判断输入是否处于命令模式（以 `>` 开头）。
 * @param {string} input
 * @returns {boolean}
 */
export function isCommandMode(input) {
  return input.startsWith(">");
}

/**
 * 从命令模式输入中提取命令文本。
 * @param {string} input
 * @returns {string|null} 命令文本（无命令时返回 null）
 */
export function extractCommand(input) {
  if (!input.startsWith(">")) return null;
  const command = input.slice(1).trimStart();
  return command || null;
}

// ── 状态查询 ──────────────────────────────────────────────────────────────

/** 当前是否处于命令模式。 */
export function isActive() {
  return active;
}

// ── 生命周期 ──────────────────────────────────────────────────────────────

/** 初始化：创建 hint DOM 元素。main.js 启动时调一次。 */
export function init() {
  hintEl = document.createElement("div");
  hintEl.id = "command-hint";
  hintEl.className = "command-hint hidden";
  const searchMode = document.getElementById("search-mode");
  searchMode.appendChild(hintEl);
}

/** 复位：退出命令模式 + 隐藏 hint（lifecycle shown/hidden 调用）。 */
export function reset() {
  active = false;
  hideHint();
}

// ── 输入处理 ──────────────────────────────────────────────────────────────

/**
 * 处理输入变化，判断是否进入/退出命令模式。
 * 由 search.js::onInput 在搜索逻辑之前调用。
 *
 * @param {string} value 当前输入框值
 * @returns {boolean} true = 命令模式已处理（search.js 应跳过搜索）；false = 非命令模式（继续搜索）
 */
export function handleInput(value) {
  if (isCommandMode(value)) {
    if (!active) {
      enter();
    }
    // 有命令时隐藏 hint，无命令时显示 hint
    const cmd = extractCommand(value);
    if (cmd) {
      hideHint();
    } else {
      showHint();
    }
    return true;
  }

  // 退出命令模式：隐藏 hint，让 search.js 继续正常搜索
  if (active) {
    exit();
  }
  return false;
}

// ── 执行 ──────────────────────────────────────────────────────────────────

/**
 * 在终端中执行当前输入的命令（回车触发）。
 * 由 keyboard.js::onNavigation 在命令模式下调用。
 *
 * @returns {Promise<boolean>} true = 已执行（无论成功失败）；false = 无命令可执行
 */
export async function execute() {
  const cmd = extractCommand(queryEl.value);
  if (!cmd) return false;

  try {
    await runInTerminal(cmd);
    hideWindow();
    return true;
  } catch (e) {
    console.error("[command-mode] runInTerminal failed:", e);
    showError(e);
    return true;
  }
}

// ── 内部 ──────────────────────────────────────────────────────────────────

/** 进入命令模式：清结果 + 清 ghost。 */
function enter() {
  active = true;
  results.clear();
  ghost.clear();
}

/** 退出命令模式：隐藏 hint。 */
function exit() {
  active = false;
  hideHint();
}

/** 显示 hint（无命令时）。 */
function showHint() {
  if (!hintEl) return;
  hintEl.textContent = t("command.hint");
  hintEl.classList.remove("hidden");
  hintEl.classList.remove("command-error");
  syncWindowSize();
}

/** 隐藏 hint。 */
function hideHint() {
  if (!hintEl) return;
  hintEl.classList.add("hidden");
  hintEl.classList.remove("command-error");
  syncWindowSize();
}

/** 显示执行错误。 */
function showError(e) {
  if (!hintEl) return;
  const msg = typeof e === "string" ? e : e?.message || String(e);
  hintEl.textContent = t("command.error", { message: msg });
  hintEl.classList.remove("hidden");
  hintEl.classList.add("command-error");
  syncWindowSize();
}
