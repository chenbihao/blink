/**
 * chat IPC 封装（0.12.1 Phase 5）。
 *
 * 封装 invoke + listen，业务层只调语义化函数。
 */

import { invoke, listen } from "../tauri.js";

// ── Commands ─────────────────────────────────────

/**
 * 启动对话 prompt。
 * @param {string} conversationId
 * @param {string} message
 * @returns {Promise<number>} request_id
 */
export function chatPrompt(conversationId, message) {
  return invoke("chat_prompt", { conversationId, message });
}

/**
 * 中止指定请求。
 * @param {number} requestId
 * @returns {Promise<boolean>}
 */
export function chatAbort(requestId) {
  return invoke("chat_abort", { requestId });
}

/**
 * 获取 chat 状态。
 * @returns {Promise<{active: {request_id: number, conversation_id: string}|null, provider_configured: boolean}>}
 */
export function getChatStatus() {
  return invoke("get_chat_status");
}

/**
 * 确认/拒绝危险操作。
 * @param {number} confirmId
 * @param {boolean} approved
 * @returns {Promise<boolean>}
 */
export function confirmChatAction(confirmId, approved) {
  return invoke("confirm_chat_action", { confirmId, approved });
}

/**
 * 隐藏 chat 窗口。
 */
export function hideChatWindow() {
  return invoke("hide_chat_window");
}

// ── Events ───────────────────────────────────────

/**
 * 监听流式 chunk 事件。
 * @param {(event: {payload: {request_id: number, conversation_id: string, chunk: object}}) => void} handler
 * @returns {Promise<() => void>} unsubscribe
 */
export function listenChatStream(handler) {
  return listen("blink://chat-stream", handler);
}

/**
 * 监听危险操作确认事件。
 * @param {(event: {payload: {confirm_id: number, tool_name: string, tool_type: string, arguments: object, danger_class: string, request_id: number, conversation_id: string}}) => void} handler
 * @returns {Promise<() => void>} unsubscribe
 */
export function listenChatConfirm(handler) {
  return listen("blink://chat-confirm-action", handler);
}

/**
 * 监听 Provider 变更事件。
 * @param {(event: object) => void} handler
 * @returns {Promise<() => void>} unsubscribe
 */
export function listenProviderChanged(handler) {
  return listen("blink://chat-provider-changed", handler);
}
