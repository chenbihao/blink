/**
 * chat IPC 封装（0.12.1 Phase 5）。
 *
 * 封装 invoke + listen，业务层只调语义化函数。
 */

import { invoke, listen, saveDialog } from "../shared/tauri.js";
import { EVENTS } from "../shared/event-names.js";

// ── Commands ─────────────────────────────────────

/**
 * 启动对话 prompt。
 *
 * 0.12.6：新增可选 `groupId` 参数——设置对话所属分组并注入分组级系统提示词。
 * 传 null/undefined 时保持现有分组不变（后端查询对话当前分组的 system_prompt）。
 *
 * @param {string} conversationId
 * @param {string} message
 * @param {string|null} [groupId] 分组 ID（0.12.6）
 * @returns {Promise<number>} request_id
 */
export function chatPrompt(conversationId, message, groupId = null, opts = {}) {
  return invoke("chat_prompt", {
    conversationId,
    message,
    groupId,
    targetWindow: opts.targetWindow ?? null,
    ephemeral: opts.ephemeral ?? null,
  });
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

// ── 导出对话（0.12.5 §5.6）──────────────────

/**
 * 保存文本到指定路径（后端 command 封装）。
 * @param {string} path 文件路径
 * @param {string} content 文本内容
 * @returns {Promise<void>}
 */
export function saveTextFile(path, content) {
  return invoke("save_text_file", { path, content });
}

/**
 * 导出对话为 Markdown 文件（0.12.5 §5.6）。
 *
 * 前端格式化 Markdown + Tauri `dialog.save()` 获取路径 + `save_text_file` 写文件。
 * 调用方传入对话标题（用于 Markdown 标题 + 文件名）。
 *
 * @param {string} conversationId
 * @param {string} title 对话标题
 * @returns {Promise<boolean>} true=成功保存，false=用户取消
 */
export async function exportConversation(conversationId, title) {
  // 1. 加载对话消息
  const messages = await getChatMessages(conversationId);

  // 2. 格式化为 Markdown
  const markdown = formatConversationMarkdown(title, messages);

  // 3. 打开保存对话框获取路径
  const safeName = (title || "对话")
    .replace(/[<>:"/\\|?*\x00-\x1f]/g, "_")
    .trim()
    .slice(0, 50) || "对话";
  const path = await saveDialog({
    defaultPath: `${safeName}.md`,
    filters: [{ name: "Markdown", extensions: ["md"] }],
  });
  if (!path) return false; // 用户取消

  // 4. 写文件
  await saveTextFile(path, markdown);
  return true;
}

/**
 * 将对话消息列表格式化为 Markdown 字符串。
 * @param {string} title 对话标题
 * @param {Array<{role: string, text: string, thinking: string|null, tool_name?: string, tool_result?: string}>} messages
 * @returns {string}
 */
function formatConversationMarkdown(title, messages) {
  let md = `# ${title || "对话"}\n\n`;
  for (const msg of messages) {
    if (msg.role === "user") {
      md += `## 用户\n\n${msg.text}\n\n---\n\n`;
    } else if (msg.role === "assistant") {
      // 纯工具调用（无文本回复）
      if (msg.tool_name && !msg.text) {
        md += `**工具调用：${msg.tool_name}**\n\n`;
        if (msg.tool_result) {
          md += `> ${msg.tool_result.replace(/\n/g, "\n> ")}\n\n`;
        }
        continue;
      }
      md += `## 助手\n\n`;
      if (msg.thinking) {
        md += `<details><summary>思考过程</summary>\n\n${msg.thinking}\n\n</details>\n\n`;
      }
      if (msg.text) {
        md += `${msg.text}\n\n`;
      }
      if (msg.tool_name) {
        md += `**工具调用：${msg.tool_name}**\n\n`;
        if (msg.tool_result) {
          md += `> ${msg.tool_result.replace(/\n/g, "\n> ")}\n\n`;
        }
      }
      md += `---\n\n`;
    }
    // system 消息跳过
  }
  return md.trimEnd() + "\n";
}

// ── 标题生成（0.12.5 §5.3）──────────────────

/**
 * 异步生成对话标题（0.12.5 §5.3）。
 * 后台异步调 LLM 生成标题，成功后 emit `chat-title-updated` 事件。
 * 失败静默降级，不阻塞对话流程。auto_title 关闭时后端直接返回。
 * @param {string} conversationId
 * @param {string} firstMessage 首条用户消息
 * @returns {Promise<void>}
 */
export function generateConversationTitle(conversationId, firstMessage) {
  return invoke("generate_conversation_title", { conversationId, firstMessage });
}

// ── 消息编辑重发（0.12.5 §5.5）──────────────────

/**
 * 截断对话消息——保留前 `keepCount` 条，删除其余（0.12.5 §5.5）。
 * @param {string} conversationId
 * @param {number} keepCount 保留的消息数
 * @returns {Promise<void>}
 */
export function truncateMessages(conversationId, keepCount) {
  return invoke("truncate_messages", { conversationId, keepCount });
}

// ── 对话分组（0.12.6）──────────────────────────────────────────

/**
 * 列出所有对话分组（按 sort_order 升序，含 parent_id 供前端构建树）。
 * @returns {Promise<Array<{id: string, name: string, system_prompt?: string, parent_id?: string, sort_order: number, expanded: boolean, created_at: number}>>}
 */
export function listConversationGroups() {
  return invoke("list_conversation_groups");
}

/**
 * 创建对话分组。`id` 由前端 `crypto.randomUUID()` 生成。
 * @param {string} id 分组 ID（前端生成）
 * @param {string} name 分组名
 * @param {string|null} [parentId] 父分组 ID（null = 顶层）
 * @returns {Promise<void>}
 */
export function createConversationGroup(id, name, parentId = null) {
  return invoke("create_conversation_group", { id, name, parentId });
}

/**
 * 重命名对话分组。
 * @param {string} groupId
 * @param {string} name 新名称
 * @returns {Promise<boolean>}
 */
export function renameConversationGroup(groupId, name) {
  return invoke("rename_conversation_group", { groupId, name });
}

/**
 * 删除对话分组。组内对话移至默认（group_id = NULL），子分组 re-parent。
 * @param {string} groupId
 * @returns {Promise<boolean>}
 */
export function deleteConversationGroup(groupId) {
  return invoke("delete_conversation_group", { groupId });
}

/**
 * 更新分组的系统提示词。`prompt` 为 null 时清除。
 * @param {string} groupId
 * @param {string|null} prompt 系统提示词（null = 清除）
 * @returns {Promise<boolean>}
 */
export function updateConversationGroupSystemPrompt(groupId, prompt) {
  return invoke("update_conversation_group_system_prompt", { groupId, prompt });
}

/**
 * 移动对话到指定分组。`groupId` 为 null 移至默认组。
 * @param {string} conversationId
 * @param {string|null} groupId 目标分组 ID（null = 默认组）
 * @returns {Promise<void>}
 */
export function moveConversationToGroup(conversationId, groupId) {
  return invoke("move_conversation_to_group", { conversationId, groupId });
}

/**
 * 设置分组排序权重（拖拽排序用）。
 * @param {string} groupId
 * @param {number} sortOrder
 * @returns {Promise<void>}
 */
export function setGroupSortOrder(groupId, sortOrder) {
  return invoke("set_group_sort_order", { groupId, sortOrder });
}

/**
 * 设置分组折叠状态。
 * @param {string} groupId
 * @param {boolean} expanded true=展开, false=折叠
 * @returns {Promise<void>}
 */
export function setGroupExpanded(groupId, expanded) {
  return invoke("set_group_expanded", { groupId, expanded });
}

/**
 * 查询对话的有效系统提示词（0.12.7 §6.5）。
 *
 * 返回对话所属分组的 system_prompt（直属分组，非祖先继承）。
 * 无分组或分组无提示词时返回 null。
 * @param {string} conversationId
 * @returns {Promise<string|null>}
 */
export function getConversationSystemPrompt(conversationId) {
  return invoke("get_conversation_system_prompt", { conversationId });
}

// ── MCP tool 池（0.13.0）──────────────────────────────────────────

/**
 * 0.13.8: 触发 MCP lazy connect——持久连接所有 enabled 但尚未连接的 server。
 * 供对话窗口打开时调用，让 popup 能显示正确的在线状态。
 * @returns {Promise<void>}
 */
export function ensureMcpConnected() {
  return invoke("ensure_mcp_connected");
}

/**
 * 获取对话窗口 tool 池规模（内置 + MCP，供前端显示）。
 * @returns {Promise<{builtin: number, mcp: number, total: number}>}
 */
export function getMcpToolPoolSize() {
  return invoke("get_mcp_tool_pool_size");
}

/**
 * 获取所有已连接 MCP server 的 tool 名称列表（供前端区分工具来源）。
 * @returns {Promise<string[]>}
 */
export function getMcpToolNames() {
  return invoke("get_mcp_tool_names");
}

// ── 0.13.6 上下文窗口状态 ──────────────────────────────────────────

/**
 * 获取当前上下文窗口状态（前端初始化时调用）。
 * @returns {Promise<{estimated_tokens: number, context_limit: number, usage_percent: number, last_compressed: boolean, last_compressed_count: number, last_recall_count: number}|null>}
 */
export function getContextWindowStatus() {
  return invoke("get_context_window_status");
}

/**
 * 强制压缩当前对话的上下文窗口。
 * @param {string} conversationId
 * @returns {Promise<{estimated_tokens: number, context_limit: number, usage_percent: number, last_compressed: boolean, last_compressed_count: number, last_recall_count: number}>}
 */
export function compressContextNow(conversationId) {
  return invoke("compress_context_now", { conversationId });
}

// ── 0.13.6 MCP tool 来源信息 ──────────────────────────────────────

/**
 * 获取 MCP tool 来源信息（含 server 名 + transport 类型）。
 * @returns {Promise<Array<{tool_name: string, server_name: string, transport: string}>|null>}
 */
export function getMcpToolSources() {
  return invoke("get_mcp_tool_sources");
}

// ── Composer bar 悬浮预览快照 ──────────────────────────────────────

/**
 * 获取 composer bar 悬浮预览快照（一次 IPC 聚合上下文 + 内置 tool + MCP 服务）。
 * @returns {Promise<{estimated_tokens: number, context_limit: number, usage_percent: number, last_compressed: boolean, last_compressed_count: number, last_recall_count: number, builtin_tools: Array<{name: string, description: string}>, mcp_servers: Array<{name: string, transport: string, online: boolean, tool_count: number, tool_names: string[]}>, builtin_count: number, mcp_count: number, total_count: number}>}
 */
export function getComposerBarSnapshot() {
  return invoke("get_composer_bar_snapshot");
}

// ── Events ───────────────────────────────────────

/**
 * 监听流式 chunk 事件。
 * @param {(event: {payload: {request_id: number, conversation_id: string, chunk: object}}) => void} handler
 * @returns {Promise<() => void>} unsubscribe
 */
export function listenChatStream(handler) {
  return listen(EVENTS.CHAT_STREAM, handler);
}

/**
 * 监听危险操作确认事件。
 * @param {(event: {payload: {confirm_id: number, tool_name: string, tool_type: string, arguments: object, danger_class: string, request_id: number, conversation_id: string}}) => void} handler
 * @returns {Promise<() => void>} unsubscribe
 */
export function listenChatConfirm(handler) {
  return listen(EVENTS.CHAT_CONFIRM_ACTION, handler);
}

/**
 * 监听对话标题自动更新事件（0.12.5 §5.3）。
 * @param {(event: {payload: {conversation_id: string, title: string}}) => void} handler
 * @returns {Promise<() => void>} unsubscribe
 */
export function listenChatTitleUpdated(handler) {
  return listen(EVENTS.CHAT_TITLE_UPDATED, handler);
}

/**
 * 监听上下文窗口状态更新事件（0.13.6）。
 * @param {(event: {payload: {estimated_tokens: number, context_limit: number, usage_percent: number, last_compressed: boolean, last_compressed_count: number, last_recall_count: number}}) => void} handler
 * @returns {Promise<() => void>} unsubscribe
 */
export function listenContextStatus(handler) {
  return listen(EVENTS.CHAT_CONTEXT_STATUS, handler);
}

/**
 * 监听 Skill 激活事件（0.13.6）。
 * @param {(event: {payload: {request_id: number, skills: Array<{name: string, source: string, trigger_type: string}>}}) => void} handler
 * @returns {Promise<() => void>} unsubscribe
 */
export function listenSkillActivated(handler) {
  return listen(EVENTS.CHAT_SKILL_ACTIVATED, handler);
}

// ── Voice Events（0.12.2 §4.3）─────────────────

/**
 * 监听录音开始事件（target="chat" 时进入录音模式）。
 */
export function listenVoiceRecordingStart(handler) {
  return listen(EVENTS.VOICE_RECORDING_START, handler);
}

/**
 * 监听语音识别 partial 文本（实时更新 textarea）。
 */
export function listenVoicePartial(handler) {
  return listen(EVENTS.VOICE_PARTIAL, handler);
}

/**
 * 监听录音结束事件（退出录音模式）。
 */
export function listenVoiceRecordingEnd(handler) {
  return listen(EVENTS.VOICE_RECORDING_END, handler);
}

/**
 * 监听语音错误事件。
 */
export function listenVoiceError(handler) {
  return listen(EVENTS.VOICE_ERROR, handler);
}

/**
 * 监听语音音量事件（波形动画）。
 */
export function listenVoiceLevel(handler) {
  return listen(EVENTS.VOICE_LEVEL, handler);
}

/**
 * 监听语音状态事件（模型加载中等）。
 */
export function listenVoiceStatus(handler) {
  return listen(EVENTS.VOICE_STATUS, handler);
}
