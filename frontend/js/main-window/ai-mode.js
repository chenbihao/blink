//! AI 模式状态机 + 流式渲染 + 确认卡（0.17.6 B2.2 + B2.3）。
//!
//! 主窗口 AI 从 SearchService::trigger_ai 切换到 ChatService::prompt，
//! 与对话窗口统一调度模型。主窗口使用临时对话（EphemeralConversationMemory）。
//!
//! 状态机：
//!   SearchMode --Tab on AI Ghost--> AiMode --ESC--> SearchMode
//!   AiMode --失焦--> 窗口保持不隐藏（watchdog MAIN_WINDOW_AI_ACTIVE 跳过）
//!   AiMode --Alt+Space--> set_focus + #ai-query.focus()（不 toggle hide）
//!
//! 事件监听：
//!   CHAT_STREAM（按 request_id 过滤）
//!   CHAT_CONFIRM_ACTION（按 conversation_id 过滤）

import { listen } from "../shared/tauri.js";
import { EVENTS } from "../shared/event-names.js";
import { chatPrompt, chatAbort, confirmChatAction, promoteEphemeralConversation, getEphemeralModels, clearMainAiActive } from "../shared/api.js";
import { initMarkdown, renderMarkdown, renderMarkdownStream } from "../shared/markdown.js";
import {
  renderTypingIndicator,
  renderToolLine,
  renderToolResultLine,
  renderConfirmCard,
  createThrottledRenderer,
} from "../shared/message-render.js";
import {
  searchModeEl,
  aiModeEl,
  aiQueryEl,
  aiToolLineEl,
  aiRoundsEl,
  aiContentEl,
  aiDisplayEl,
  aiModelLabelEl,
  queryEl,
} from "./dom.js";
import { syncWindowSize, resetMaxHeight } from "./window-size.js";
import { t } from "../i18n/index.js";
import * as ghost from "./ghost.js";
import * as search from "./search.js";
import * as results from "./results.js";

// ── 状态 ──────────────────────────────────────────────────────────────────────

let active = false;
let conversationId = null;
let requestId = null;
let streamBuffer = "";
let streamRenderer = null;
let throttle = null;
let awaitingConfirm = false;
let unlistenStream = null;
let unlistenConfirm = null;
let currentQuestion = ""; // 当前轮用户问题文本（折叠摘要用）

// ── 初始化 ────────────────────────────────────────────────────────────────────

export function init() {
  initMarkdown();
  // 注册 CHAT_STREAM / CHAT_CONFIRM_ACTION 监听
  listen(EVENTS.CHAT_STREAM, handleStreamEvent);
  listen(EVENTS.CHAT_CONFIRM_ACTION, handleConfirmEvent);
}

/** 当前是否处于 AI 模式。 */
export function isActive() {
  return active;
}

/** 当前是否在等待确认（键盘 Enter 确认用）。 */
export function isAwaitingConfirm() {
  return awaitingConfirm;
}

/** 确认当前确认卡片（键盘 Enter 触发）。 */
export function confirmCurrentAction() {
  if (!awaitingConfirm) return;
  const approveBtn = aiContentEl.querySelector('[data-action="approve"]');
  if (approveBtn) {
    approveBtn.click();
  }
}

// ── 模式切换 ──────────────────────────────────────────────────────────────────

/**
 * 进入 AI 模式。
 * 生成 conversation_id，切换 DOM，调用 chat_prompt 发送首条消息。
 * @param {string} queryText 用户输入的查询文本
 */
export async function enterAiMode(queryText) {
  if (active) return;
  active = true;
  conversationId = crypto.randomUUID();
  streamBuffer = "";
  awaitingConfirm = false;
  currentQuestion = queryText;

  // DOM 切换
  searchModeEl.hidden = true;
  aiModeEl.hidden = false;
  aiQueryEl.value = "";
  aiQueryEl.focus();

  // 清空搜索结果 + statusbar（避免 AI 模式下残留搜索导航提示）
  results.clear();

  // 加载当前模型标签（异步，不阻塞首条消息发送）
  refreshModelLabel();

  // 清空展示区
  aiContentEl.innerHTML = "";
  aiToolLineEl.innerHTML = "";
  syncWindowSize();

  // 发送首条消息
  await sendPrompt(queryText);
}

/**
 * 退出 AI 模式。
 * 中止活跃请求（如有），清空展示区，回到搜索模式。
 */
export function exitAiMode() {
  if (!active) return;

  // 中止活跃请求
  if (requestId !== null) {
    chatAbort(requestId).catch((e) =>
      console.warn("[ai-mode] chatAbort 失败:", e),
    );
    requestId = null;
  }

  // 0.18.0: 清除看门狗 AI 活跃标志（即使 request 已 Done 也要清）
  clearMainAiActive().catch((e) =>
    console.warn("[ai-mode] clearMainAiActive 失败:", e),
  );

  // 清空状态
  active = false;
  conversationId = null;
  streamBuffer = "";
  awaitingConfirm = false;
  currentQuestion = "";
  if (throttle) {
    throttle.cancel();
    throttle = null;
  }
  streamRenderer = null;

  // DOM 切换回搜索模式
  aiModeEl.hidden = true;
  searchModeEl.hidden = false;
  aiContentEl.innerHTML = "";
  aiToolLineEl.innerHTML = "";
  aiRoundsEl.innerHTML = "";
  aiModelLabelEl.textContent = "";
  // 重置 footer 隐藏态（下次 enterAiMode 由 refreshModelLabel 控制）
  const footer = aiModelLabelEl.parentElement;
  if (footer) footer.classList.add("hidden");
  // 重建 #ai-content（被确认卡片替换后需要恢复）
  aiRoundsEl.appendChild(aiContentEl);

  // 搜索模式复位
  queryEl.value = "";
  queryEl.focus();
  search.reset();
  ghost.clear();
  // 0.18.0: 归零 maxHeight，让窗口从 AI 模式的高大高度收缩回搜索模式的紧凑高度
  resetMaxHeight();
  syncWindowSize();
}

// ── 发送消息 ──────────────────────────────────────────────────────────────────

/**
 * 调用 chat_prompt 发送消息。
 * 在 #ai-content 中先渲染用户消息气泡，再创建 AI 回复区放 typing 动画。
 * @param {string} message 用户消息文本
 */
async function sendPrompt(message) {
  streamBuffer = "";
  awaitingConfirm = false;
  currentQuestion = message;

  // 构建当前轮 DOM：用户消息 + AI 回复区（含 typing 动画）
  aiContentEl.innerHTML = "";

  // 用户消息气泡
  const userMsg = document.createElement("div");
  userMsg.className = "ai-user-message";
  userMsg.textContent = message;
  aiContentEl.appendChild(userMsg);

  // AI 回复区
  const responseArea = document.createElement("div");
  responseArea.className = "ai-response-area";
  responseArea.innerHTML = renderTypingIndicator();
  aiContentEl.appendChild(responseArea);

  if (throttle) {
    throttle.cancel();
  }
  streamRenderer = null;

  // 滚动到最新内容
  scrollToBottom();
  syncWindowSize();

  try {
    const rid = await chatPrompt(conversationId, message, {
      targetWindow: "main",
      ephemeral: true,
    });
    requestId = rid;
  } catch (e) {
    handlePromptError(e);
  }
}

/**
 * 发送追问消息（0.17.6a）。
 * 当前轮收进折叠摘要条后再发送新消息，保持同一 conversation_id。
 * @param {string} text 追问文本
 */
export async function askFollowup(text) {
  if (!active || !text.trim()) return;
  // 当前轮有内容时收进折叠摘要条
  if (streamBuffer) {
    collapseToSummary(currentQuestion, streamBuffer);
  }
  currentQuestion = text;
  await sendPrompt(text);
}

/**
 * 0.17.6a: 将当前临时对话提升为持久对话（Chord-Q）。
 *
 * AiMode 下 Alt+Q 触发：
 * 1. 调用后端 promote_ephemeral_conversation
 * 2. 后端 abort 当前请求 -> 导出临时消息 -> 写入 SQLite -> 清空临时记忆 -> 打开对话窗口
 * 3. 前端 exitAiMode → SearchMode
 */
export async function promoteToChat() {
  if (!active || !conversationId) return;

  // 如果有活跃请求且有内容，先 finalize（保留已有内容导出）
  if (streamBuffer && throttle) {
    throttle.cancel();
    if (streamRenderer) {
      streamRenderer.write(streamBuffer);
    }
  }

  try {
    await promoteEphemeralConversation(conversationId);
  } catch (e) {
    console.error("[ai-mode] promoteEphemeralConversation 失败:", e);
    // 即使失败也退出 AI 模式
  }

  // 退出 AI 模式回到搜索模式
  exitAiMode();
}

// ── 流式事件处理 ──────────────────────────────────────────────────────────────

/**
 * 处理 CHAT_STREAM 事件。
 * 按 request_id 过滤，分发 chunk.kind。
 */
function handleStreamEvent(event) {
  if (!active) return;
  const { request_id, conversation_id, chunk } = event.payload;
  // 忽略不属于当前请求的 chunk
  if (request_id !== requestId) return;
  if (conversation_id !== conversationId) return;

  switch (chunk.kind) {
    case "text":
      handleTextChunk(chunk.text);
      break;
    case "thinking":
      // 0.17.6 暂不展示 thinking（0.17.6a 可折叠展示）
      break;
    case "tool_call":
      handleToolCall(chunk.tool, chunk.arguments);
      break;
    case "tool_result":
      handleToolResult(chunk.call_id, chunk.success, chunk.summary);
      break;
    case "done":
      handleDone(chunk);
      break;
    case "error":
      handleError(chunk.message);
      break;
    case "max_turns_reached":
      handleMaxTurns(chunk.max_turns);
      break;
  }
}

/**
 * 处理文本 chunk：追加到 buffer，rAF 节流渲染。
 */
function handleTextChunk(text) {
  streamBuffer += text;

  // 首次 text 到达时：移除 typing 动画，创建流式渲染器
  if (!streamRenderer) {
    const responseArea = aiContentEl.querySelector(".ai-response-area");
    if (!responseArea) return;
    responseArea.innerHTML = "";
    streamRenderer = renderMarkdownStream(responseArea);
    throttle = createThrottledRenderer((accumulated) => {
      if (streamRenderer) {
        streamRenderer.write(accumulated);
      }
      // 流式渲染期间同步窗口高度 + 滚动到底部
      scrollToBottom();
      syncWindowSize();
    });
  }

  throttle.schedule(streamBuffer);
}

/**
 * 处理 tool_call：更新工具行为单行描述。
 */
function handleToolCall(toolName, args) {
  aiToolLineEl.innerHTML = renderToolLine(toolName, args);
  scrollToBottom();
  syncWindowSize();
}

/**
 * 处理 tool_result：更新工具行为完成状态。
 * 注意：tool_result 只有 call_id，没有 tool name，
 * 保持工具行不变（tool_call 已设置名称），只追加结果摘要。
 */
function handleToolResult(callId, success, summary) {
  // 从工具行读取当前工具名
  const nameEl = aiToolLineEl.querySelector(".ai-tool-name");
  const toolName = nameEl ? nameEl.textContent : "";
  if (toolName) {
    aiToolLineEl.innerHTML = renderToolResultLine(toolName, success, summary);
  }
  scrollToBottom();
}

/**
 * 处理 done：完成流式渲染。
 */
function handleDone(chunk) {
  // 最终渲染（确保最后的内容已写入）
  if (throttle) {
    throttle.cancel();
    if (streamRenderer && streamBuffer) {
      streamRenderer.write(streamBuffer);
    }
  }

  // 如果有内容，注入复制按钮
  if (streamBuffer) {
    injectCopyButton();
  }

  // 清理状态
  requestId = null;
  if (throttle) {
    throttle = null;
  }
  streamRenderer = null;

  // 工具行延迟清空（让用户看到最后工具的结果）
  setTimeout(() => {
    if (!active) return;
    aiToolLineEl.innerHTML = "";
    // 工具行消失后需要同步窗口高度（窗口可能缩短）
    syncWindowSize();
  }, 2000);

  scrollToBottom();
  syncWindowSize();
}

/**
 * 处理错误。
 */
function handleError(message) {
  requestId = null;
  if (throttle) {
    throttle.cancel();
    throttle = null;
  }
  streamRenderer = null;

  const responseArea = aiContentEl.querySelector(".ai-response-area");
  if (responseArea) {
    responseArea.innerHTML = `<div class="ai-error-message">${escapeHtml(message)}</div>`;
  }
  scrollToBottom();
  syncWindowSize();
}

/**
 * 处理最大轮次到达。
 */
function handleMaxTurns(maxTurns) {
  const responseArea = aiContentEl.querySelector(".ai-response-area");
  if (responseArea) {
    responseArea.insertAdjacentHTML(
      "beforeend",
      `<div class="ai-max-turns-warning">${escapeHtml(t("ai.max_turns_warning", { max: maxTurns }))}</div>`,
    );
  }
  scrollToBottom();
  syncWindowSize();
}

/**
 * 处理 chat_prompt 调用错误。
 */
function handlePromptError(e) {
  const msg = String(e);
  // AlreadyActive 错误
  if (msg.startsWith("AlreadyActive:")) {
    const win = msg.slice("AlreadyActive:".length);
    setResponseError(escapeHtml(t("ai.already_active", { window: win })));
  } else {
    setResponseError(escapeHtml(msg));
  }
}

/** 在 AI 回复区显示错误信息。 */
function setResponseError(html) {
  const responseArea = aiContentEl.querySelector(".ai-response-area");
  if (responseArea) {
    responseArea.innerHTML = `<div class="ai-error-message">${html}</div>`;
  }
  scrollToBottom();
  syncWindowSize();
}

// ── 确认事件处理 ──────────────────────────────────────────────────────────────

/**
 * 处理 CHAT_CONFIRM_ACTION 事件。
 */
function handleConfirmEvent(event) {
  if (!active) return;
  const payload = event.payload;
  if (!payload) return;
  // 按 conversation_id 过滤
  if (payload.conversation_id && payload.conversation_id !== conversationId) return;

  awaitingConfirm = true;

  // 在 AI 回复区显示确认卡片
  const card = renderConfirmCard(payload, async (confirmId, approved) => {
    try {
      await confirmChatAction(confirmId, approved);
    } catch (e) {
      console.error("[ai-mode] confirmChatAction 失败:", e);
    }
    // 确认后移除卡片，恢复流式输出
    card.remove();
    awaitingConfirm = false;
  });

  const responseArea = aiContentEl.querySelector(".ai-response-area");
  if (responseArea) {
    responseArea.innerHTML = "";
    responseArea.appendChild(card);
  }
  scrollToBottom();
  syncWindowSize();
}

// ── 折叠摘要 ──────────────────────────────────────────────────────────────────

/**
 * 将当前轮收进折叠摘要条，插入 #ai-rounds 中 #ai-content 之前。
 * 摘要条默认折叠，点击展开后显示完整回答。
 * @param {string} question 用户问题文本
 * @param {string} answer assistant 回答 Markdown 文本
 */
function collapseToSummary(question, answer) {
  const summary = document.createElement("div");
  summary.className = "ai-round-summary";
  summary.dataset.collapsed = "";

  // 摘要头：用户问题截断 + 展开按钮
  const header = document.createElement("div");
  header.className = "ai-round-summary-header";

  const qText = document.createElement("span");
  qText.className = "ai-round-summary-q";
  const truncated = question.length > 60
    ? question.slice(0, 60) + "…"
    : question;
  qText.textContent = truncated;
  qText.title = question;

  const toggleBtn = document.createElement("button");
  toggleBtn.className = "ai-round-summary-toggle";
  toggleBtn.textContent = t("ai.round_expand");
  toggleBtn.addEventListener("click", () => {
    const isCollapsed = summary.hasAttribute("data-collapsed");
    if (isCollapsed) {
      summary.removeAttribute("data-collapsed");
      toggleBtn.textContent = t("ai.round_collapse");
    } else {
      summary.setAttribute("data-collapsed", "");
      toggleBtn.textContent = t("ai.round_expand");
    }
  });

  header.appendChild(qText);
  header.appendChild(toggleBtn);
  summary.appendChild(header);

  // 摘要体：完整回答（Cherry 渲染）
  const body = document.createElement("div");
  body.className = "ai-round-summary-body";
  // 0.17.6a: 用 renderMarkdown 一次性渲染（非流式，历史轮已完整）
  renderMarkdown(answer, { container: body });
  summary.appendChild(body);

  // 插入到 #ai-rounds 中 #ai-content 之前
  aiRoundsEl.insertBefore(summary, aiContentEl);
}

// ── 辅助函数 ──────────────────────────────────────────────────────────────────

/**
 * 在 AI 回复区底部注入复制按钮。
 */
function injectCopyButton() {
  const responseArea = aiContentEl.querySelector(".ai-response-area");
  if (!responseArea) return;
  const btn = document.createElement("button");
  btn.className = "ai-copy-btn";
  btn.textContent = t("ai.copy");
  btn.addEventListener("click", async () => {
    try {
      await navigator.clipboard.writeText(streamBuffer);
      btn.textContent = t("ai.copied");
      setTimeout(() => { btn.textContent = t("ai.copy"); }, 1500);
    } catch (e) {
      console.error("[ai-mode] 复制失败:", e);
    }
  });
  responseArea.appendChild(btn);
}

/**
 * 滚动 #ai-display 到底部，确保最新内容可见。
 */
function scrollToBottom() {
  if (aiDisplayEl) {
    aiDisplayEl.scrollTop = aiDisplayEl.scrollHeight;
  }
}

/**
 * 异步拉取当前 Ephemeral 模型并更新底部标签。
 * 失败静默降级（隐藏标签栏）。
 */
async function refreshModelLabel() {
  const footer = aiModelLabelEl.parentElement;
  try {
    const models = await getEphemeralModels();
    if (!models || !models.length) {
      aiModelLabelEl.textContent = "";
      if (footer) footer.classList.add("hidden");
      return;
    }
    const selected = models.find((m) => m.is_selected);
    if (selected) {
      aiModelLabelEl.textContent = selected.model_name;
      if (footer) footer.classList.remove("hidden");
    } else {
      aiModelLabelEl.textContent = "";
      if (footer) footer.classList.add("hidden");
    }
  } catch (e) {
    console.warn("[ai-mode] 获取模型列表失败:", e);
    aiModelLabelEl.textContent = "";
    if (footer) footer.classList.add("hidden");
  }
}

/** HTML 特殊字符转义。 */
function escapeHtml(text) {
  const div = document.createElement("div");
  div.textContent = String(text ?? "");
  return div.innerHTML;
}
