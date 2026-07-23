/**
 * chat 状态管理（0.12.1 Phase 5）。
 *
 * 集中管理对话窗口的运行时状态，供 renderer / composer / ipc 模块读写。
 */

/** @type {string} 当前 conversation_id，新对话时重新生成 */
export let conversationId = crypto.randomUUID();

/** @type {Array<{role: string, content: string, toolName?: string, error?: boolean}>} */
export let messages = [];

/** @type {boolean} assistant 是否正在流式生成 */
export let isStreaming = false;

/** @type {number|null} 当前 active request_id，用于 abort 和 chunk 过滤 */
export let activeRequestId = null;

/** @type {string} 当前 assistant 消息的累积文本（流式阶段） */
export let streamBuffer = "";

/** @type {boolean} Provider 是否已配置 */
export let providerConfigured = true;

// ── 状态变更 ─────────────────────────────────────

export function setStreaming(value) {
  isStreaming = value;
}

export function setActiveRequestId(id) {
  activeRequestId = id;
}

export function appendStreamBuffer(text) {
  streamBuffer += text;
}

export function resetStreamBuffer() {
  streamBuffer = "";
}

export function setProviderConfigured(value) {
  providerConfigured = value;
}

export function addMessage(msg) {
  messages.push(msg);
}

/**
 * 新对话：重置所有状态，生成新 conversation_id。
 * @returns {string} 新的 conversation_id
 */
export function resetConversation() {
  conversationId = crypto.randomUUID();
  messages.length = 0;
  isStreaming = false;
  activeRequestId = null;
  streamBuffer = "";
  return conversationId;
}
