/**
 * chat 状态管理（0.12.1 Phase 5）。
 *
 * 集中管理对话窗口的运行时状态，供 renderer / composer / ipc 模块读写。
 */

/** @type {string} 当前 conversation_id，新对话时重新生成 */
export let conversationId = crypto.randomUUID();

/** @type {string|null} 当前对话所属分组 ID（0.12.6，null = 默认/未分组） */
export let currentGroupId = null;

/** @type {Array<{role: string, content: string, toolName?: string, error?: boolean}>} */
export let messages = [];

/** @type {boolean} assistant 是否正在流式生成 */
export let isStreaming = false;

/** @type {number|null} 当前 active request_id，用于 abort 和 chunk 过滤 */
export let activeRequestId = null;

/** @type {string} 当前 assistant 消息的累积文本（流式阶段） */
export let streamBuffer = "";

/** @type {string} 当前 thinking/reasoning 的累积文本（流式阶段） */
export let thinkingBuffer = "";

/** @type {boolean} 是否正在接收 thinking 内容 */
export let isThinking = false;

/** @type {boolean} 是否启用深度思考（显示 thinking 块） */
export let thinkingEnabled = false;

/** @type {boolean} Provider 是否已配置 */
export let providerConfigured = true;

/**
 * 进行中的 Tool 卡片索引（0.12.2 §4.7）。
 * key = call_id，value = { el, tool }。ToolResult 到来时按 call_id 配对更新。
 */
export const toolCalls = new Map();

/** @type {{input_tokens: number, output_tokens: number}|null} 最近一次 Done 的 token 用量 */
export let lastUsage = null;

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

export function appendThinkingBuffer(text) {
  thinkingBuffer += text;
}

export function resetThinkingBuffer() {
  thinkingBuffer = "";
  isThinking = false;
}

export function setThinking(value) {
  isThinking = value;
}

export function setThinkingEnabled(value) {
  thinkingEnabled = value;
}

export function toggleThinkingEnabled() {
  thinkingEnabled = !thinkingEnabled;
  return thinkingEnabled;
}

export function setProviderConfigured(value) {
  providerConfigured = value;
}

export function setLastUsage(usage) {
  lastUsage = usage;
}

/** 设置当前 conversation_id（0.12.4 §6.1：ES module namespace import 只读，需 setter）。 */
export function setConversationId(id) {
  conversationId = id;
}

/** 设置当前对话所属分组 ID（0.12.6）。 */
export function setCurrentGroupId(id) {
  currentGroupId = id ?? null;
}

/** 记录一个进行中的 Tool 卡片（按 call_id 索引）。 */
export function trackToolCall(callId, entry) {
  if (callId) toolCalls.set(callId, entry);
}

/** 按 call_id 取出 Tool 卡片记录（不删除，结果可能多次到达）。 */
export function getToolCall(callId) {
  return callId ? toolCalls.get(callId) : null;
}

/** 清空所有进行中的 Tool 卡片索引。 */
export function clearToolCalls() {
  toolCalls.clear();
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
  thinkingBuffer = "";
  isThinking = false;
  toolCalls.clear();
  lastUsage = null;
  currentGroupId = null; // 0.12.6：重置分组上下文
  return conversationId;
}
