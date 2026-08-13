//! 0.20.0 主窗口动作错误投影 helper。
//!
//! 职责：结构化错误 → 本地化消息 → statusbar 可见反馈。
//! 日志只记录 action id、error code 和长度/尺寸，不记录正文内容。
//!
//! 设计原则（spec-frontend）：
//! - 内置动作失败统一进入可见 toast/status 反馈，不静默吞错
//! - 错误消息走 i18n 本地化，按 code 分档映射
//! - 日志只记录 action id + error code，不记录正文/剪贴板/选区原文

import { normalizeError } from "../shared/tauri.js";
import { t } from "../i18n/index.js";
import { syncWindowSize } from "./window-size.js";

/** statusbar 元素引用。 */
const statusbarEl = document.getElementById("statusbar");

/** 当前错误提示的定时器 ID（用于清除上一个提示）。 */
let errorTimer = null;

/** 当前错误提示 DOM 元素。 */
let errorEl = null;

/** 错误提示持续时间（毫秒）。 */
const ERROR_DURATION_MS = 3500;

/**
 * code → i18n key 映射表。
 * 后端 CommandError 的 code 值（snake_case）→ 前端 i18n key。
 * 未命中 code 走 fallback 通用错误文案。
 */
const CODE_I18N_KEYS = {
  invalid_args: "action.error.invalid_args",
  invalid_state: "action.error.invalid_state",
  conflict: "action.error.conflict",
  invalid_data: "action.error.invalid_data",
  permission_denied: "action.error.permission_denied",
  timeout: "action.error.timeout",
  cancelled: "action.error.cancelled",
  not_found: "action.error.not_found",
  internal_error: "action.error.internal",
  runtime_error: "action.error.runtime",
  missing_arg: "action.error.missing_arg",
  unknown_error: "action.error.unknown",
};

/**
 * 将结构化错误投影为本地化用户消息。
 *
 * @param {{ code: string, message: string, detail?: *, retryable: boolean }} err
 * @returns {string} 本地化消息
 */
function projectErrorMessage(err) {
  const key = CODE_I18N_KEYS[err.code] ?? CODE_I18N_KEYS.unknown_error;
  // i18n 模板 {message} 使用后端提供的 message（已是用户可读简短说明）
  return t(key, { message: err.message });
}

/**
 * 在 statusbar 显示动作错误反馈。
 *
 * 在 statusbar 内追加一个绝对定位的错误提示元素，3.5 秒后自动移除。
 * 不替换 statusbar 原有内容，避免丢失事件监听或破坏 statusbar.js 的渲染状态。
 * 若上一个错误提示仍在，会被新的替换（不叠加）。
 *
 * @param {string} actionId 动作标识（如 "copy_clipboard_image"、"pin_clipboard_image"、
 *                          "edit_text_item"、"pin_text_item"、"run_builtin_action"、
 *                          "launch_app"、"get_clipboard_text"）
 * @param {string|object} err invoke promise rejection 值
 */
export function showActionError(actionId, err) {
  const normalized = normalizeError(err);

  // 日志只记录 action id + error code + message 长度，不记录正文
  console.error(
    `[action-error] action=${actionId} code=${normalized.code} msg_len=${normalized.message.length} retryable=${normalized.retryable}`
  );

  if (!statusbarEl) return;

  // 清除上一个错误提示
  if (errorTimer) {
    clearTimeout(errorTimer);
    errorTimer = null;
  }
  if (errorEl) {
    errorEl.remove();
    errorEl = null;
  }

  // 创建错误提示元素并叠加在 statusbar 上方
  const message = projectErrorMessage(normalized);
  errorEl = document.createElement("div");
  errorEl.className = "action-error-hint";
  errorEl.setAttribute("role", "alert");
  errorEl.textContent = message;
  statusbarEl.appendChild(errorEl);
  statusbarEl.classList.add("has-action-error");
  syncWindowSize();

  // 定时移除错误提示
  errorTimer = setTimeout(() => {
    errorTimer = null;
    if (errorEl) {
      errorEl.remove();
      errorEl = null;
    }
    statusbarEl.classList.remove("has-action-error");
    syncWindowSize();
  }, ERROR_DURATION_MS);
}
