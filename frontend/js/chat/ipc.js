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
 * @returns {Promise<{active: {request_id: number, conversation_id: string}|null, provider_configured: boolean, provider_name: string|null, model_name: string|null}>}
 */
export function getChatStatus() {
  return invoke("get_chat_status");
}

/**
 * 列出 chat 可选的所有 Chat 能力模型（0.12.2 §4.4）。
 * @returns {Promise<Array<{id: string, provider_name: string, model_name: string, is_main: boolean, is_light: boolean, is_selected: boolean}>>}
 */
export function getChatModels() {
  return invoke("get_chat_models");
}

/**
 * 设置 chat 运行时选中模型（0.12.2 §4.4）。
 * @param {string|null} selectionId "{provider_id}:{model_id}"，null=恢复 Main 档
 * @returns {Promise<boolean>} true=切换成功
 */
export function selectChatModel(selectionId) {
  return invoke("select_chat_model", { selectionId });
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

/**
 * 启动 chat 窗口语音录音（0.12.2 §4.3）。
 * @returns {Promise<void>}
 */
export function startChatStt() {
  return invoke("start_chat_stt");
}

/**
 * 停止 chat 窗口语音录音（0.12.2 §4.3）。
 * @returns {Promise<void>}
 */
export function stopChatStt() {
  return invoke("stop_chat_stt");
}

// ── 多对话管理（0.12.3 Phase B）──────────────────

/**
 * 列出所有对话。
 * @returns {Promise<Array<{id: string, title: string|null, created_at: number, last_active_at: number, message_count: number}>>}
 */
export function listChatConversations() {
  return invoke("list_chat_conversations");
}

/**
 * 删除指定对话（级联删除消息）。
 * @param {string} conversationId
 * @returns {Promise<boolean>}
 */
export function deleteChatConversation(conversationId) {
  return invoke("delete_chat_conversation", { conversationId });
}

/**
 * 重命名对话。
 * @param {string} conversationId
 * @param {string} title
 * @returns {Promise<boolean>}
 */
export function renameChatConversation(conversationId, title) {
  return invoke("rename_chat_conversation", { conversationId, title });
}

/**
 * 加载对话的完整消息历史。
 * @param {string} conversationId
 * @returns {Promise<Array<{role: string, text: string, thinking: string|null}>>}
 */
export function getChatMessages(conversationId) {
  return invoke("get_chat_messages", { conversationId });
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

// ── Voice Events（0.12.2 §4.3）─────────────────

/**
 * 监听录音开始事件（target="chat" 时进入录音模式）。
 */
export function listenVoiceRecordingStart(handler) {
  return listen("blink://voice-recording-start", handler);
}

/**
 * 监听语音识别 partial 文本（实时更新 textarea）。
 */
export function listenVoicePartial(handler) {
  return listen("blink://voice-partial", handler);
}

/**
 * 监听录音结束事件（退出录音模式）。
 */
export function listenVoiceRecordingEnd(handler) {
  return listen("blink://voice-recording-end", handler);
}

/**
 * 监听语音错误事件。
 */
export function listenVoiceError(handler) {
  return listen("blink://voice-error", handler);
}

/**
 * 监听语音音量事件（波形动画）。
 */
export function listenVoiceLevel(handler) {
  return listen("blink://voice-level", handler);
}

/**
 * 监听语音状态事件（模型加载中等）。
 */
export function listenVoiceStatus(handler) {
  return listen("blink://voice-status", handler);
}
