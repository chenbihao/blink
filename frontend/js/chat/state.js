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

/** @type {boolean} 是否启用深度思考（默认开启；不支持等级的 provider 用简单开关） */
export let thinkingEnabled = true;

/**
 * @type {string|null} 当前模型配置的思考强度（reasoning_effort 线值，0.21.17）。
 * `null` = auto/未配置（跟随旧二值行为）；`"none"` = 关闭；`""` = 不发送（omit）；
 * 其余 = 该档位原样发送（minimal/low/medium/high/xhigh/max 或自定义）。
 */
export let thinkingEffort = null;

/** @type {boolean} 当前模型是否支持 reasoning_effort 等级（0.21.17）。 */
export let supportsEffort = false;

/** @type {boolean} Provider 是否已配置 */
export let providerConfigured = true;

/** 0.17.6a: 临时对话模式（不写入 SQLite，promote 后转持久） */
export let ephemeralMode = false;

/**
 * 进行中的 Tool 卡片索引（0.12.2 §4.7）。
 * key = call_id，value = { el, tool }。ToolResult 到来时按 call_id 配对更新。
 */
export const toolCalls = new Map();

/** @type {{input_tokens: number, output_tokens: number, total_tokens?: number, reasoning_tokens?: number, cached_input_tokens?: number, reported?: boolean}|null} 最近一次 Done 的 token 用量 */
export let lastUsage = null;

/**
 * 已关闭提示词横幅的对话集合（0.12.7 §6.5）。
 * key = conversation_id，value = true。仅在会话内有效，刷新后重置。
 */
export const dismissedPromptConvs = new Set();

/**
 * MCP tool 名称集合（0.13.0）。
 * 存储所有已连接 MCP server 提供的 tool 名称，用于在工具卡片上标记来源。
 * `null` = 尚未加载，`Set<string>` = 已加载。
 */
export let mcpToolNames = null;

/**
 * MCP tool 来源信息 Map（0.13.6）。
 * key = tool_name, value = { server_name, transport }。
 * `null` = 尚未加载。
 */
let mcpToolSourcesMap = null;

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

/** 0.21.17: 设置当前模型思考强度（reasoning_effort 线值，null = auto）。 */
export function setThinkingEffort(value) {
    thinkingEffort = value ?? null;
}

/** 0.21.17: 一次更新思考控件所需全部状态（等级 + 是否支持）。 */
export function setThinkingCapability({effort, supportsEffort: supports}) {
    thinkingEffort = effort ?? null;
    supportsEffort = Boolean(supports);
}

export function setProviderConfigured(value) {
    providerConfigured = value;
}

/** 0.17.6a: 设置临时对话模式。 */
export function setEphemeralMode(value) {
    ephemeralMode = value;
}

export function setLastUsage(usage) {
    lastUsage = usage;
}

/** 0.13.6: 设置上次 FTS5 召回的消息条数。 */
let lastRecallCount = 0;

export function setLastRecallCount(count) {
    lastRecallCount = count;
}

/** 设置 MCP tool 名称集合（0.13.0）。 */
export function setMcpToolNames(names) {
    mcpToolNames = names ? new Set(names) : null;
}

/** 判断 tool 名称是否来自 MCP server（0.13.0）。 */
export function isMcpTool(name) {
    return mcpToolNames != null && mcpToolNames.has(name);
}

/** 0.13.6: 设置 MCP tool 来源信息。 */
export function setMcpToolSources(sources) {
    mcpToolSourcesMap = sources ? new Map(sources.map((s) => [s.tool_name, s])) : null;
}

/** 0.13.6: 获取 tool 的 MCP 来源信息。返回 null 表示不是 MCP tool。 */
export function getMcpToolSource(name) {
    return mcpToolSourcesMap?.get(name) || null;
}

/** 设置当前 conversation_id（0.12.4 §6.1：ES module namespace import 只读，需 setter）。 */
export function setConversationId(id) {
    conversationId = id;
}

/** 设置当前对话所属分组 ID（0.12.6）。 */
export function setCurrentGroupId(id) {
    // 防御：groupId 必须是字符串或 null，避免对象/其他类型泄漏到 invoke("chat_prompt")
    // 导致后端 serde 报 "invalid type: map, expected a string"
    currentGroupId = typeof id === "string" ? id : null;
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
    ephemeralMode = false; // 0.17.6a：重置临时模式
    return conversationId;
}
