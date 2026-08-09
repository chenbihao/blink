/**
 * chat 窗口入口（0.12.1 Phase 5）。
 *
 * 装配 state / ipc / renderer / components / composer，注册事件监听。
 */

import * as state from "./state.js";
import * as ipc from "./ipc.js";
import { initRenderer, bindLinkOpener } from "./renderer.js";
import * as components from "./components.js";
import { forceScrollToBottom } from "./components.js";
import { escapeText, escapeAttr } from "./utils.js";
// 0.12.7 §6.3：显式导入 renderSignal，多处场景接入
import { initComposer, setStreamingMode, setInputMode, clearInput, setInputValue, focusInput, setThinkingEnabled as setComposerThinking, showVoiceIndicator, hideVoiceIndicator, showVoiceStatus, showVoiceError, updateVoiceLevel, updateVoicePartial, isVoiceRecording } from "./composer.js";
import { initSidebar, refreshSidebar, showSidebar, hideSidebar, toggleSidebar, setActiveConversation } from "./sidebar.js";
import { applyThemeFromConfig } from "../shared/theme.js";
import { listen, invoke, getCurrentWindow } from "../shared/tauri.js";
import { EVENTS } from "../shared/event-names.js";
import { promoteEphemeralConversation } from "../shared/api.js";
import { initComposerBarPopup, invalidateComposerBarCache, refreshPopupIfVisible } from "./composer-bar-popup.js";
// invalidateComposerBarCache 仍在 handleContextStatus 中使用
// 0.12.4 §6.5：openSettings 直接用 invoke，不再需要动态 import
import { t } from "../i18n/index.js";

/** 流式渲染节流：requestAnimationFrame 句柄 */
let rafHandle = 0;

/** 当前 assistant 消息 DOM 引用 */
let currentAssistantEl = null;

/** 对话窗口是否已被用户实际打开（区分预热 vs 真正显示）。
 *  preheat 在启动 3s 后创建隐藏的 chat 窗口，JS init() 会执行但窗口不可见。
 *  此标记用于阻止预热阶段触发 MCP 连接——只在窗口首次获得焦点（真正显示）后才连接。 */
let windowActivated = false;

/** 当前 Tool 状态卡 DOM 引用 */
let currentToolEl = null;

// ── 初始化 ──────────────────────────────────────

async function init() {
  // 主题初始化 + 实时跟随
  applyThemeFromConfig();
  // 0.12.2 §4.9: ai_config 变更时刷新模型选择器（复用 config-changed 事件）
  listen(EVENTS.CONFIG_CHANGED, (e) => {
    applyThemeFromConfig();
    if (e?.payload?.key === "ai_config") {
      refreshModelSelector();
      refreshToolPool(true);
      invalidateComposerBarCache();
      refreshPopupIfVisible();
    }
    // 0.13.0：MCP 配置变更时刷新 tool 池
    if (e?.payload?.key === "mcp:servers") {
      // 预热阶段（windowActivated=false）跳过——窗口未真正打开，不触发 MCP 连接
      if (!windowActivated) return;
      // 0.13.8: 设置页切换 MCP server 开关后，对话窗口需要重连 + 刷新 popup
      ipc.ensureMcpConnected().then(() => {
        refreshToolPool(true);
        invalidateComposerBarCache();
        refreshPopupIfVisible();
      }).catch(() => {
        // 即使 ensure_connected 失败，也要清缓存 + 刷新 popup，
        // 否则 popup 会显示过期的 online 状态
        refreshToolPool(true);
        invalidateComposerBarCache();
        refreshPopupIfVisible();
      });
    }
  });

  initRenderer();
// 拦截对话消息内的链接点击，通过外部浏览器打开（防止 Tauri WebView 内部导航崩溃）
bindLinkOpener();
  components.initComponents({ onEditMessage: handleEditMessage });

  initComposer({
    onSend: handleSend,
    onStop: handleStop,
    onThinkingToggle: handleThinkingToggle,
  });

  // 0.19：chat prefill 投递（revision 机制防残留/防旧事件误删）
  //
  // 两条路径都不丢文本：
  //   冷启动：listener 注册后 take 拉取 pending（take 清空）
  //   热窗口：listener 收到事件 → ack 清空 pending
  // revision 防止旧事件的 ack 误删新 pending。
  // appliedRevision 防止 take 和 event 同时命中时重复填充。
  let appliedRevision = 0;

  function applyPrefill(text, revision) {
    if (revision && revision === appliedRevision) return; // 已填充过同一 revision
    if (typeof text === "string" && text) {
      setInputValue(text);
      focusInput();
      appliedRevision = revision || 0;
    }
  }

  // 先 await listen，确认 listener 注册成功后再 take
  await listen(EVENTS.CHAT_PREFILL, (event) => {
    const payload = event?.payload;
    const text = payload?.text;
    const revision = payload?.revision || 0;
    applyPrefill(text, revision);
    // 热窗口路径：收到事件后 ack 清空 pending（revision 匹配才清）
    if (revision) {
      ipc.ackChatPrefill(revision).catch(() => {});
    }
  });

  // listener 已注册，现在 take 兜底冷启动路径
  // 两种顺序都不会丢：
  //   pending 先写入 → take 拉取 → event 后到同 revision 被 appliedRevision 去重
  //   take 返回 null → event 后到正常填充 + ack
  try {
    const pending = await ipc.takeChatPrefill();
    if (pending && typeof pending.text === "string" && pending.text) {
      applyPrefill(pending.text, pending.revision);
    }
  } catch (e) {
    console.warn("[chat] take_chat_prefill 失败:", e);
  }

  initSidebar({
    onSwitch: handleSwitchConversation,
    onNew: handleNewConversation,
    onRenamed: handleSidebarRenamed,
    onExport: handleExportConversation,
  });

  // 0.17.6a: 临时对话按钮
  const ephemeralBtn = document.getElementById("chat-sidebar-ephemeral");
  if (ephemeralBtn) {
    ephemeralBtn.addEventListener("click", () => handleNewEphemeralConversation());
  }

  // 0.17.6a: "转为持久"按钮
  const promoteBtn = document.getElementById("chat-promote-ephemeral");
  if (promoteBtn) {
    promoteBtn.addEventListener("click", () => handlePromoteEphemeral());
  }

  // 注册事件监听
  await ipc.listenChatStream(handleStreamEvent);
  await ipc.listenChatConfirm(handleConfirmEvent);
  await ipc.listenChatTitleUpdated(handleTitleUpdated);
  await ipc.listenContextStatus(handleContextStatus);
  await ipc.listenSkillActivated(handleSkillActivated);
  await ipc.listenVoicePartial(handleVoicePartial);
  await ipc.listenVoiceRecordingStart(handleVoiceRecordingStart);
  await ipc.listenVoiceRecordingEnd(handleVoiceRecordingEnd);
  await ipc.listenVoiceError(handleVoiceError);
  await ipc.listenVoiceLevel(handleVoiceLevel);
  await ipc.listenVoiceStatus(handleVoiceStatus);

  // 0.17.6a: promote 临时对话后，后端 emit CHAT_LOAD_CONVERSATION 通知切换
  listen(EVENTS.CHAT_LOAD_CONVERSATION, async (event) => {
    const convId = event?.payload;
    if (typeof convId === "string" && convId) {
      await handleSwitchConversation(convId);
    }
  });

  // 初始状态：检查 provider 配置 + 加载模型选择器
  try {
    const status = await ipc.getChatStatus();
    state.setProviderConfigured(status.provider_configured);

    // 0.13.6: 加载初始上下文窗口状态（即使为 null 也显示空环）
    try {
      const ctxStatus = await ipc.getContextWindowStatus();
      components.updateContextIndicator(ctxStatus || null);
      if (ctxStatus) {
        components.renderContextWarning(ctxStatus.usage_percent, handleContextWarningAction);
      }
    } catch (e) {
      // 即使获取失败也显示空环
      components.updateContextIndicator(null);
      console.warn("[chat] 加载上下文窗口状态失败:", e);
    }
    updateProviderLabel(status);
    components.renderEmptyState(status.provider_configured, openSettings);
  } catch (e) {
    console.error("[chat] getChatStatus 失败:", e);
    components.renderEmptyState(false, openSettings);
  }
  refreshModelSelector();

  // 0.13.0：加载 tool 池规模 + MCP tool 名称（供工具卡片来源标记）
  // 注意：此处不触发 MCP 连接——预热阶段窗口不可见，无需拉起 MCP server 子进程。
  // MCP lazy connect 在窗口首次获得焦点（真正显示给用户）时由 focus 事件触发。
  refreshToolPool(true);

  // Composer bar 悬浮预览 popup 初始化
  initComposerBarPopup();

  // 侧边栏 toggle
  const sidebarToggle = document.getElementById("chat-sidebar-toggle");
  if (sidebarToggle) {
    sidebarToggle.addEventListener("click", () => toggleSidebar());
  }

  // 模型选择器交互
  bindModelSelector();

  // 0.12.7 §1：面包屑标题编辑（事件委托）
  bindBreadcrumbEdit();

  // 0.12.7 §6.5：系统提示词横幅交互
  bindPromptBanner();

  // Esc 键：生成中 → abort；空闲 → 隐藏窗口
  document.addEventListener("keydown", handleEsc);

  // 0.12.6：重试按钮（assistant 消息的 retry action）
  document.addEventListener("chat:retry", handleRetry);

  // 窗口获得焦点时自动聚焦输入框 + 刷新 tool 池（MCP server 可能在设置页被启停）
  // 0.13.8: focus 时也触发 ensure_mcp_connected，让掉线的 server 重新连接
  // 0.13.9: 首次 focus 标记窗口已激活——preheat 创建的隐藏窗口不会收到 focus 事件，
  //         只有 show_chat_window → set_focus() 真正显示时才触发，避免预热阶段拉起 MCP。
  window.addEventListener("focus", () => {
    focusInput();
    windowActivated = true;
    ipc.ensureMcpConnected().then(() => {
      refreshToolPool(true);
      invalidateComposerBarCache();
      refreshPopupIfVisible();
    }).catch(() => {});
  });

  // 初始聚焦
  focusInput();
}

// ── 发送 ────────────────────────────────────────

async function handleSend(message, isEdit = false) {
  // 移除空状态
  components.removeEmptyState();

  // 添加用户消息
  components.renderUserMessage(message);
  // 用户发消息 → 强制滚到底部（重置上滚标记）
  forceScrollToBottom();
  state.addMessage({ role: "user", content: message });

  // 0.12.4 §6.7：新对话首条消息 → 截断生成标题（编辑重发不触发）
  // 0.17.6a: 临时对话跳过标题生成（不写 SQLite，promote 后由主窗口负责）
  const isNewConversation = !isEdit && state.messages.length === 1;
  if (isNewConversation && !state.ephemeralMode) {
    const truncatedTitle = message.slice(0, 20) + (message.length > 20 ? "…" : "");
    try {
      await ipc.renameChatConversation(state.conversationId, truncatedTitle);
      await updateBreadcrumb(truncatedTitle);
      // 0.12.5 §5.3：异步触发 LLM 命名（不等待，失败静默降级保持截断标题）
      ipc.generateConversationTitle(state.conversationId, message).catch((e) => {
        console.warn("[chat] LLM 标题生成失败:", e);
      });
    } catch (e) {
      console.warn("[chat] 截断标题设置失败:", e);
    }
  }

  // 切换到流式模式
  setStreamingMode();
  state.setStreaming(true);
  state.resetStreamBuffer();

  // 创建 assistant 消息 DOM
  currentAssistantEl = components.createAssistantMessage();

  try {
    // 0.17.6a: 临时对话模式传 ephemeral:true
    const opts = state.ephemeralMode
      ? { ephemeral: true, targetWindow: "chat" }
      : {};
    const requestId = await ipc.chatPrompt(state.conversationId, message, state.currentGroupId, opts);
    state.setActiveRequestId(requestId);
  } catch (e) {
    console.error("[chat] chatPrompt 失败:", e);
    // 移除空的 assistant 消息 DOM（createAssistantMessage 已创建但 chatPrompt 失败）
    if (currentAssistantEl) {
      currentAssistantEl.remove();
      currentAssistantEl = null;
    }
    // 友好化错误消息（0.12.5 §5.3）
    const errStr = String(e);
    let friendlyMsg;
    if (errStr.includes("AlreadyActive") || errStr.includes("已有对话请求")) {
      friendlyMsg = "已有对话正在生成，请等待完成或停止后再发送";
    } else if (errStr.includes("NotConfigured") || errStr.includes("未配置")) {
      friendlyMsg = "AI 服务未配置，请在设置中添加供应商和模型";
    } else if (errStr.includes("Timeout") || errStr.includes("超时")) {
      friendlyMsg = "AI 响应超时，请检查网络连接或模型状态";
    } else {
      friendlyMsg = `发送失败: ${errStr}`;
    }
    components.renderErrorMessage(friendlyMsg);
    finishStreaming();
    // 恢复输入框（不清空，让用户可以修改后重试）
    const input = document.getElementById("chat-input");
    if (input) input.disabled = false;
    refreshSidebar();
    return;
  }

  clearInput();
  // 刷新侧边栏（更新 last_active_at + 标题）
  // 注意：对话持久化由 rig memory.append 异步完成，done chunk 到达后再刷新一次
  refreshSidebar();
  setActiveConversation(state.conversationId);
}

// ── 消息编辑重发（0.12.5 §5.5） ────────────────

/**
 * 消息编辑重发：截断后续消息（DB + state）→ 重新渲染 → 重新发送。
 * @param {number} msgIndex 被编辑消息在 state.messages 中的索引
 * @param {string} newText 编辑后的文本
 */
async function handleEditMessage(msgIndex, newText) {
  if (state.isStreaming) {
    await handleStop();
  }

  // 1. 截断 DB 消息（保留前 msgIndex 条）
  try {
    await ipc.truncateMessages(state.conversationId, msgIndex);
  } catch (e) {
    console.error("[chat] 截断消息失败:", e);
    return;
  }

  // 2. 截断 state.messages
  state.messages.length = msgIndex;

  // 3. 重新渲染消息列表
  components.clearMessages();
  for (const msg of state.messages) {
    if (msg.role === "user") {
      components.renderUserMessage(msg.content);
    } else if (msg.role === "assistant") {
      const el = components.createAssistantMessage();
      if (el) {
        components.finalizeAssistantMessage(el, msg.content, "");
      }
    }
  }

  // 0.12.7 §6.3：消息已编辑 Signal
  components.renderSignal("消息已编辑", "info");

  // 4. 重新发送编辑后的消息（isEdit=true 跳过截断标题逻辑）
  await handleSend(newText, true);
}

// ── 重试（0.12.6：assistant 消息的 retry 按钮） ────────────────

/**
 * 重试：重新发送最后一条用户消息。
 * 截断掉最后一条 assistant 回复（及可能的 tool 消息），
 * 找到最后一条 user 消息文本，重新发送。
 */
async function handleRetry() {
  if (state.isStreaming) return; // 生成中不允许重试

  // 从 state.messages 反向找最后一条 user 消息
  let lastUserIndex = -1;
  for (let i = state.messages.length - 1; i >= 0; i--) {
    if (state.messages[i].role === "user") {
      lastUserIndex = i;
      break;
    }
  }
  if (lastUserIndex < 0) return;

  const lastUserText = state.messages[lastUserIndex].content;

  // 截断到最后一条 user 消息之前（保留到 lastUserIndex-1）
  // 然后重新发送该用户消息（isEdit=true 跳过标题逻辑）
  await handleEditMessage(lastUserIndex, lastUserText);
}

// ── 停止 ────────────────────────────────────────

async function handleStop() {
  if (state.activeRequestId != null) {
    try {
      await ipc.chatAbort(state.activeRequestId);
    } catch (e) {
      console.warn("[chat] chatAbort 失败:", e);
    }
  }
  // 立即结束流式状态，不依赖后端兜底 Done chunk。
  // abort 后后端的兜底 Done 会被 request_id 校验过滤（activeRequestId 已清空）。
  // 0.18.0: trim 判断——纯空白 text/thinking 走 remove 分支
  if (currentAssistantEl && ((state.streamBuffer || "").trim() || (state.thinkingBuffer || "").trim())) {
    components.finalizeAssistantMessage(currentAssistantEl, state.streamBuffer, state.thinkingBuffer);
    state.addMessage({ role: "assistant", content: state.streamBuffer });
  } else if (currentAssistantEl) {
    // 无内容的空气泡直接移除
    currentAssistantEl.remove();
  }
  if (currentToolEl) {
    components.finalizeToolStatus(currentToolEl, true);
    currentToolEl = null;
  }
  // 0.12.7 §6.3：停止生成 Signal
  components.renderSignal("已停止生成", "info");
  finishStreaming();
}

// ── 深度思考开关 ────────────────────────────────

function handleThinkingToggle(enabled) {
  state.setThinkingEnabled(enabled);
}

// ── 流式事件处理 ────────────────────────────────

function handleStreamEvent(event) {
  const { request_id, conversation_id, chunk } = event.payload;

  // 忽略不属于当前请求的 chunk
  if (request_id !== state.activeRequestId) return;
  if (conversation_id !== state.conversationId) return;

  switch (chunk.kind) {
    case "thinking":
      // 未启用深度思考时丢弃 thinking chunk
      if (!state.thinkingEnabled) break;
      state.appendThinkingBuffer(chunk.text);
      state.setThinking(true);
      // 首条 thinking 时创建 assistant DOM
      if (!currentAssistantEl) {
        currentAssistantEl = components.createAssistantMessage();
      }
      scheduleRender();
      break;

    case "text":
      // 首条 text 到达时标记 thinking 结束（折叠）
      if (state.isThinking) {
        state.setThinking(false);
      }
      // tool_call 后 currentAssistantEl 被置 null，新 text 到达时需创建新气泡
      if (!currentAssistantEl) {
        currentAssistantEl = components.createAssistantMessage();
      }
      state.appendStreamBuffer(chunk.text);
      scheduleRender();
      break;

    case "tool_call":
      // 结束当前 assistant 消息的流式状态（可能有 thinking 而无 text）
      // 0.18.0: trim 判断——纯空白 text/thinking 走 remove 分支
      if (currentAssistantEl && ((state.streamBuffer || "").trim() || (state.thinkingBuffer || "").trim())) {
        components.finalizeAssistantMessage(currentAssistantEl, state.streamBuffer, state.thinkingBuffer);
        state.addMessage({ role: "assistant", content: state.streamBuffer });
        state.resetStreamBuffer();
        state.resetThinkingBuffer();
        currentAssistantEl = null;
      } else if (currentAssistantEl) {
        // 模型直接 tool_call（无 text/thinking）：移除空的 typing 占位气泡
        currentAssistantEl.remove();
        currentAssistantEl = null;
      }
      // 显示 Tool 状态卡（0.12.2：按 call_id 跟踪，供 ToolResult 配对）
      currentToolEl = components.renderToolStatus(chunk.tool, chunk.arguments);
      if (chunk.call_id) {
        state.trackToolCall(chunk.call_id, { el: currentToolEl, tool: chunk.tool });
      }
      break;

    case "tool_result": {
      // 0.12.2 §4.7：tool 执行结果，按 call_id 配对到对应卡片
      const entry = chunk.call_id ? state.getToolCall(chunk.call_id) : null;
      const targetEl = entry?.el || currentToolEl;
      if (targetEl) {
        components.finalizeToolStatus(targetEl, chunk.success);
        components.appendToolResult(targetEl, chunk.summary, chunk.success);
      }
      break;
    }

    case "max_turns_reached":
      // 0.12.3 Phase C: tool loop 触顶提示
      if (currentAssistantEl) {
        components.finalizeAssistantMessage(currentAssistantEl, state.streamBuffer, state.thinkingBuffer);
        state.addMessage({ role: "assistant", content: state.streamBuffer });
      }
      components.renderSignal(`已达工具调用上限（${chunk.max_turns}轮），请缩减任务或开启新对话`, "warning");
      finishStreaming();
      break;

    case "done":
      finalizeDone(chunk);
      break;

    case "error":
      // 移除空的 streaming assistant DOM（有内容则先 finalize）
      if (currentAssistantEl) {
      // 0.18.0: trim 判断——纯空白走 remove 分支
      if ((state.streamBuffer || "").trim() || (state.thinkingBuffer || "").trim()) {
        components.finalizeAssistantMessage(currentAssistantEl, state.streamBuffer, state.thinkingBuffer);
      } else {
        currentAssistantEl.remove();
      }
      }
      components.renderErrorMessage(chunk.message);
      finishStreaming();
      break;
  }
}

/**
 * requestAnimationFrame 节流渲染。
 */
function scheduleRender() {
  if (rafHandle) return;
  rafHandle = requestAnimationFrame(() => {
    rafHandle = 0;
    if (currentAssistantEl) {
      const thinkingDone = !state.isThinking && state.thinkingBuffer.length > 0;
      components.updateAssistantMessage(
        currentAssistantEl,
        state.streamBuffer,
        state.thinkingBuffer,
        thinkingDone
      );
    }
  });
}

/**
 * Done：最终渲染 + 清理（0.12.2：chunk 携带 token 用量）。
 * @param {{input_tokens: number, output_tokens: number}} chunk
 */
function finalizeDone(chunk) {
  // 0.18.0: 空内容（纯空白）时不 finalize，直接移除元素，不添加空消息到 state
  if (currentAssistantEl) {
    if ((state.streamBuffer || "").trim() || (state.thinkingBuffer || "").trim()) {
      components.finalizeAssistantMessage(currentAssistantEl, state.streamBuffer, state.thinkingBuffer);
      state.addMessage({ role: "assistant", content: state.streamBuffer });
    } else {
      currentAssistantEl.remove();
    }
  }
  // 完成 Tool 状态卡（若有未配对到 result 的卡片，默认成功）
  if (currentToolEl) {
    components.finalizeToolStatus(currentToolEl, true);
    currentToolEl = null;
  }
  // 0.12.3：在最后一条 assistant 消息底部显示模型名
  if (chunk && chunk.model_name) {
    const targetEl = currentAssistantEl || components.getLastAssistantEl?.();
    if (targetEl) {
      components.renderModelLabel(targetEl, chunk.model_name);
    }
  }
  // 0.12.2 §4.8：在最后一条 assistant 消息底部显示 token 用量
  if (chunk && (chunk.input_tokens || chunk.output_tokens)) {
    // currentAssistantEl 已 null（如纯 tool 调用后 done）时，回查 DOM 最后一条 assistant
    const targetEl = currentAssistantEl || components.getLastAssistantEl?.();
    if (targetEl) {
      components.renderTokenUsage(targetEl, chunk.input_tokens, chunk.output_tokens);
    }
    state.setLastUsage({
      input_tokens: chunk.input_tokens,
      output_tokens: chunk.output_tokens,
    });
  }
  // 0.13.6: 记忆召回 badge
  if (state.lastRecallCount > 0) {
    const targetEl = currentAssistantEl || components.getLastAssistantEl?.();
    if (targetEl) {
      components.renderRecallBadge(targetEl, state.lastRecallCount);
    }
  }
  finishStreaming();
  // 持久化已完成（rig memory.append 在 done 前执行），刷新侧边栏更新 last_active_at
  refreshSidebar();
}

/**
 * 结束流式状态，恢复 composer。
 */
function finishStreaming() {
  state.setStreaming(false);
  state.setActiveRequestId(null);
  state.resetStreamBuffer();
  state.resetThinkingBuffer();
  state.clearToolCalls();
  currentAssistantEl = null;
  currentToolEl = null;
  setInputMode();
}

// ── 确认事件处理 ────────────────────────────────

function handleConfirmEvent(event) {
  const payload = event.payload;
  // 忽略不属于当前请求的确认
  // request_id=0 是 ChatService 未注册或无 active 请求时的降级值，仍处理以确保用户可操作
  if (payload.request_id !== 0 && payload.request_id !== state.activeRequestId) return;
  // 切换对话时 abort 与状态更新存在极短竞态窗口；同时校验 conversation_id，
  // 防止旧请求的确认卡片落入刚切换的新对话。
  if (payload.conversation_id && payload.conversation_id !== state.conversationId) return;

  components.renderConfirmCard(payload, async (confirmId, approved) => {
    try {
      await ipc.confirmChatAction(confirmId, approved);
    } catch (e) {
      console.error("[chat] confirmChatAction 失败:", e);
    }
  });
}

// ── 标题自动更新（0.12.5 §5.3） ────────────────

/**
* 对话标题自动更新事件处理（0.12.5 §5.3）。
* LLM 生成标题后后端 emit `chat-title-updated`，前端更新 header + 刷新侧边栏。
*/
async function handleTitleUpdated(event) {
  const { conversation_id, title } = event.payload;
  if (conversation_id === state.conversationId) {
    await updateBreadcrumb(title);
  }
  refreshSidebar();
}

// ── 上下文窗口状态（0.13.6） ───────────────────

/**
 * 上下文窗口状态更新事件处理（0.13.6）。
 * 后端在每次 prompt 前计算 token 估算 + 压缩/召回统计并推送。
 */
function handleContextStatus(event) {
  const status = event.payload;
  components.updateContextIndicator(status);
  components.renderContextWarning(status.usage_percent, handleContextWarningAction);
  // 0.13.6: 保存 recall_count 供 finalizeDone 渲染
  state.setLastRecallCount(status.last_recall_count || 0);
  // MCP 拓扑可能在 ensure_connected 时变化（lazy connect 重连成功），
  // context-status 事件在 ensure_provider 之后推送，此时 tool 池可能已变，
  // 刷新 composer bar 文本 + 清除 popup 缓存。
  refreshToolPool(true);
  invalidateComposerBarCache();
  refreshPopupIfVisible();
}

/**
 * 压缩提示条操作回调（0.13.6）。
 * - compress: 调后端 compress_context_now 强制走一遍 token_aware_truncate
 * - clear: 清空当前对话历史
 */
async function handleContextWarningAction(action) {
  if (action === "compress") {
    try {
      await ipc.compressContextNow(state.conversationId);
      components.removeContextWarning();
    } catch (e) {
      console.error("[chat] 压缩失败:", e);
    }
  } else if (action === "clear") {
    // 复用 truncate_messages 清空所有消息（保留 0 条）
    try {
      await ipc.truncateMessages(state.conversationId, 0);
      components.removeContextWarning();
      components.clearMessages();
      components.renderEmptyState(state.providerConfigured, () => ipc.hideChatWindow());
      state.resetConversation();
    } catch (e) {
      console.error("[chat] 清除对话失败:", e);
    }
  }
}

// ── Skill 激活 Signal（0.13.6） ───────────────────

/**
 * Skill 激活事件处理（0.13.6）。
 * 后端在 resolve_skill_triggers 完成后推送，前端渲染 Signal 消息。
 */
function handleSkillActivated(event) {
  const { request_id, skills } = event.payload;
  // 忽略不属于当前请求的事件
  if (request_id !== state.activeRequestId) return;
  for (const skill of skills) {
    const icon = skill.trigger_type === "explicit" ? "🎯" : "🔍";
    components.renderSignal(
      `${icon} Skill 已激活: ${skill.name} (${skill.source})`,
      "info"
    );
  }
}

// ── Alt 系统菜单抑制 ──────────────────────────────
// 无边框窗口按 Alt 键会触发 Win32 默认菜单（左上角弹出）。
// 拦截 Alt keydown 的默认行为，防止系统菜单弹出。
// 不影响 Alt 作为修饰键的功能（如 Alt+Space 语音输入由 hook 处理）。

document.addEventListener("keydown", (e) => {
  if (e.key === "Alt") {
    e.preventDefault();
  }
});

// ── Esc 键 ──────────────────────────────────────

function handleEsc(e) {
  if (e.key !== "Escape") return;
  e.preventDefault();
  if (state.isStreaming) {
    handleStop();
  } else {
    ipc.hideChatWindow();
  }
}

// ── 语音输入（0.12.3 对齐主窗口：热键驱动，非 IPC 按钮）─────────────

/** voice-recording-start: 显示语音指示器（仅 target="chat"） */
function handleVoiceRecordingStart(event) {
  const payload = event.payload;
  if (payload?.target !== "chat") return;
  showVoiceIndicator();
}

/** voice-level: 波形动画（仅 target="chat"） */
function handleVoiceLevel(event) {
  const payload = event.payload;
  if (payload?.target !== "chat") return;
  updateVoiceLevel(payload?.level ?? 0);
}

/** voice-status: 模型加载中等状态提示（仅 target="chat"） */
function handleVoiceStatus(event) {
  const payload = event.payload;
  if (payload?.target !== "chat") return;
  if (payload?.message) {
    showVoiceStatus(payload.message);
  }
}

/** voice-partial: 实时更新 textarea（仅 target="chat"） */
function handleVoicePartial(event) {
  const payload = event.payload;
  if (payload?.target !== "chat") return;
  updateVoicePartial(payload);
}

/** voice-recording-end: 隐藏语音指示器 */
function handleVoiceRecordingEnd() {
  if (isVoiceRecording()) {
    hideVoiceIndicator();
  }
}

/** voice-error: 在语音指示器上显示错误（红色波形 + 错误文案） */
function handleVoiceError(event) {
  const payload = event.payload;
  if (payload?.target !== "chat") return;
  if (isVoiceRecording()) {
    hideVoiceIndicator();
  }
  if (payload?.message) {
    showVoiceError(payload.message);
  }
}

// ── 新对话 ──────────────────────────────────────

async function handleNewConversation(groupId = null) {
  if (state.isStreaming) {
    handleStop();
  }
  state.resetConversation();
  state.setCurrentGroupId(groupId);
  components.clearMessages();
  components.renderEmptyState(state.providerConfigured, openSettings);
  setActiveConversation(state.conversationId);
  await updateBreadcrumb("新对话");
  // 0.12.7 §6.5：新对话无提示词，隐藏横幅
  await updatePromptBanner(state.conversationId);
  updateEphemeralBadge();
  refreshSidebar();
  focusInput();
}

// ── 临时对话（0.17.6a）──────────────────────────

/**
 * 新建临时对话。
 * resetConversation + 标记 ephemeralMode=true + 显示"临时"badge。
 * 不刷新侧边栏（临时对话不出现在持久化列表中）。
 */
async function handleNewEphemeralConversation() {
  if (state.isStreaming) {
    handleStop();
  }
  state.resetConversation();
  state.setEphemeralMode(true);
  components.clearMessages();
  components.renderEmptyState(state.providerConfigured, openSettings);
  await updateBreadcrumb(t("ai.ephemeral_title"));
  await updatePromptBanner(state.conversationId);
  updateEphemeralBadge();
  // 不刷新侧边栏——临时对话不在列表中
  focusInput();
}

/**
 * 将当前临时对话提升为持久对话。
 * 调用后端 promote_ephemeral_conversation 后退出临时模式。
 */
async function handlePromoteEphemeral() {
  if (!state.ephemeralMode) return;
  try {
    await promoteEphemeralConversation(state.conversationId);
    // promote 成功：后端已写入 SQLite + 打开对话窗口（已是当前窗口）
    // 切换到持久模式（handleSwitchConversation 会被 CHAT_LOAD_CONVERSATION 事件触发）
    state.setEphemeralMode(false);
    updateEphemeralBadge();
    refreshSidebar();
  } catch (e) {
    console.error("[chat] promote 临时对话失败:", e);
    components.renderErrorMessage(String(e));
  }
}

/**
 * 更新标题栏"临时"badge + "转为持久"按钮显隐。
 */
function updateEphemeralBadge() {
  const badge = document.getElementById("chat-ephemeral-badge");
  const promoteBtn = document.getElementById("chat-promote-ephemeral");
  if (badge) badge.hidden = !state.ephemeralMode;
  if (promoteBtn) promoteBtn.hidden = !state.ephemeralMode;
}

/**
 * 切换到指定对话（0.12.3 Phase B）。
 * 停止当前生成 → 设置 conversation_id → 从后端加载历史消息 → 渲染。
 * @param {string} conversationId
 */
async function handleSwitchConversation(conversationId, groupId = null) {
  if (state.isStreaming) {
    handleStop();
  }

  // 更新 state（0.12.4 §6.1：用 setter 替代直接赋值，避免 ES module 只读绑定 TypeError）
  state.setConversationId(conversationId);
  state.setCurrentGroupId(groupId);
  state.setEphemeralMode(false); // 0.17.6a: 切换到持久对话，退出临时模式
  state.messages.length = 0;
  state.setStreaming(false);
  state.setActiveRequestId(null);
  state.resetStreamBuffer();
  state.resetThinkingBuffer();
  state.clearToolCalls();

  components.clearMessages();
  setActiveConversation(conversationId);

  try {
    const messages = await ipc.getChatMessages(conversationId);
    if (messages.length === 0) {
      components.renderEmptyState(state.providerConfigured, openSettings);
    } else {
      // 0.12.7 §6.4：时间分隔符——首条消息 + 间隔 >5 分钟时插入
      let lastTs = null;
      for (const msg of messages) {
        // 插入时间分隔符
        if (msg.created_at) {
          if (lastTs === null || Math.abs(msg.created_at - lastTs) > 300) {
            components.renderTimeSeparator(msg.created_at);
          }
          lastTs = msg.created_at;
        }

        if (msg.role === "user") {
          components.renderUserMessage(msg.text);
          state.addMessage({ role: "user", content: msg.text });
        } else if (msg.role === "assistant") {
          // 包含 tool_name 的 assistant 消息渲染为工具调用卡片
          if (msg.tool_name && !msg.text) {
            const toolEl = components.renderToolStatus(msg.tool_name, msg.tool_arguments, { skipTiming: true });
            if (toolEl) {
              components.finalizeToolStatus(toolEl, true);
              // 有结果摘要时追加可折叠详情
              if (msg.tool_result) {
                components.appendToolResult(toolEl, msg.tool_result, true);
              }
            }
            continue;
          }
          const el = components.createAssistantMessage();
          if (el) {
            components.finalizeAssistantMessage(el, msg.text, msg.thinking || "");
            state.addMessage({ role: "assistant", content: msg.text });
          }
        }
      }
    }
  } catch (e) {
    console.error("[chat] 加载对话历史失败:", e);
    components.renderEmptyState(state.providerConfigured, openSettings);
  }

  // 0.12.7 §1：切换对话后更新面包屑（使用 DB 中的 group_id 更可靠）
  const convs = await ipc.listChatConversations();
  const conv = convs.find((c) => c.id === conversationId);
  state.setCurrentGroupId(conv?.group_id || groupId);
  await updateBreadcrumb(conv?.title || "新对话");
  // 0.12.7 §6.5：查询并显示分组系统提示词
  await updatePromptBanner(conversationId);
  updateEphemeralBadge(); // 0.17.6a: 确保 badge 状态正确

  focusInput();
}

// ── 侧边栏重命名同步 ──────────────────────────

/**
 * 侧边栏重命名后同步更新 header 标题（仅当前活跃对话）。
 * @param {string} conversationId
 * @param {string} newTitle
 */
async function handleSidebarRenamed(conversationId, newTitle) {
  if (conversationId === state.conversationId) {
    await updateBreadcrumb(newTitle);
  }
}

// ── 导出对话（0.12.5 §5.6） ────────────────────

/**
 * 导出对话为 Markdown 文件。
 * 委托 ipc.exportConversation：加载消息 → 格式化 Markdown → Tauri save 对话框 → 写文件。
 * @param {string} conversationId
 * @param {string} title 对话标题（用于文件名和 Markdown 标题）
 */
async function handleExportConversation(conversationId, title) {
  try {
    await ipc.exportConversation(conversationId, title);
  } catch (e) {
    console.error("[chat] 导出对话失败:", e);
  }
}

// ── 面包屑标题（0.12.7 §1） ────────────────────────

/**
 * 构建分组路径（从根到当前分组的链路）。
 * @param {Array} groups 平铺分组列表
 * @param {string|null} groupId 当前分组 ID
 * @returns {Array<{id: string, name: string, systemPrompt: string|null}>} 路径段数组（从根到当前）
 */
function buildGroupPath(groups, groupId) {
  if (!groupId) return [];
  const map = new Map(groups.map(g => [g.id, g]));
  const path = [];
  let cur = groupId;
  while (cur) {
    const g = map.get(cur);
    if (!g) break;
    path.unshift({
      id: g.id,
      name: g.name,
      systemPrompt: g.system_prompt || null,
    });
    cur = g.parent_id || null;
  }
  return path;
}

/**
 * 更新面包屑标题：文件夹路径 + 对话标题。
 * 异步获取分组列表，构建从根到当前分组的路径，渲染为面包屑。
 * 仅对话标题（最后一段）可编辑，文件夹段为静态展示。
 * 直属分组有系统提示词时，在该段显示 accent 圆点指示器。
 * @param {string} title 对话标题
 */
async function updateBreadcrumb(title) {
  const breadcrumbEl = document.getElementById("chat-breadcrumb");
  if (!breadcrumbEl) return;

  // 编辑中不更新（避免破坏 input）
  if (breadcrumbEl.querySelector(".chat-title-edit-input")) return;

  // 获取分组列表，构建路径
  let path = [];
  if (state.currentGroupId) {
    try {
      const groups = await ipc.listConversationGroups();
      path = buildGroupPath(groups, state.currentGroupId);
    } catch (e) {
      console.warn("[chat] 获取分组列表失败:", e);
    }
  }

  // 渲染面包屑：文件夹段 + 分隔符 + 对话标题
  let html = "";
  for (let i = 0; i < path.length; i++) {
    const seg = path[i];
    const isDirectGroup = i === path.length - 1;
    // 仅直属分组显示系统提示词指示器（ancestor 的提示词不继承）
    const hasPromptCls = isDirectGroup && seg.systemPrompt ? " has-prompt" : "";
    const segTitle = seg.systemPrompt ? `${seg.name}（含系统提示词）` : seg.name;
    html += `<span class="chat-breadcrumb-segment${hasPromptCls}" title="${escapeAttr(segTitle)}">${escapeText(seg.name)}</span>`;
    html += `<span class="chat-breadcrumb-sep">/</span>`;
  }
  html += `<span class="chat-breadcrumb-title" id="chat-conversation-title" title="点击重命名">${escapeText(title || "新对话")}</span>`;
  breadcrumbEl.innerHTML = html;
}

/**
 * 绑定面包屑标题编辑（事件委托，支持 innerHTML 重渲染后仍生效）。
 * 点击对话标题段 → 内联编辑 → Enter 确认（重命名）/ Esc 取消。
 */
function bindBreadcrumbEdit() {
  const breadcrumbEl = document.getElementById("chat-breadcrumb");
  if (!breadcrumbEl) return;

  breadcrumbEl.addEventListener("click", (e) => {
    const titleEl = e.target.closest(".chat-breadcrumb-title");
    if (!titleEl) return;

    const oldTitle = titleEl.textContent;
    const input = document.createElement("input");
    input.type = "text";
    input.value = oldTitle;
    input.className = "chat-title-edit-input";
    titleEl.replaceWith(input);
    input.focus();
    input.select();

    let confirmed = false;

    /** 创建新的标题 span 替换 input */
    const restoreTitle = (text) => {
      const span = document.createElement("span");
      span.className = "chat-breadcrumb-title";
      span.id = "chat-conversation-title";
      span.textContent = text;
      span.title = "点击重命名";
      input.replaceWith(span);
    };

    const finishEdit = async () => {
      if (confirmed) return;
      confirmed = true;
      const newTitle = input.value.trim();
      if (newTitle && newTitle !== oldTitle) {
        try {
          await ipc.renameChatConversation(state.conversationId, newTitle);
          restoreTitle(newTitle);
          refreshSidebar();
        } catch (err) {
          console.error("[chat] 重命名失败:", err);
          restoreTitle(oldTitle);
        }
      } else {
        restoreTitle(oldTitle);
      }
    };

    input.addEventListener("keydown", (ev) => {
      if (ev.key === "Enter") {
        ev.preventDefault();
        finishEdit();
      } else if (ev.key === "Escape") {
        confirmed = true;
        restoreTitle(oldTitle);
      }
    });
    input.addEventListener("blur", finishEdit);
  });
}

// ── 系统提示词横幅（0.12.7 §6.5） ──────────────

/**
 * 绑定提示词横幅交互（折叠/展开 + 关闭）。
 *
 * 横幅位置在消息区上方，可随消息区滚动（非 sticky）——用户看一眼即可滚走。
 * - toggle 按钮：展开/折叠 body
 * - close 按钮：本会话隐藏（state.dismissedPromptConvs 记录）
 */
function bindPromptBanner() {
  const banner = document.getElementById("chat-prompt-banner");
  if (!banner) return;

  const toggle = document.getElementById("chat-prompt-banner-toggle");
  toggle?.addEventListener("click", () => {
    if (banner.hasAttribute("data-collapsed")) {
      banner.removeAttribute("data-collapsed");
    } else {
      banner.setAttribute("data-collapsed", "");
    }
  });

  const close = document.getElementById("chat-prompt-banner-close");
  close?.addEventListener("click", () => {
    banner.hidden = true;
    if (state.conversationId) {
      state.dismissedPromptConvs.add(state.conversationId);
    }
  });
}

/**
 * 查询并更新提示词横幅。
 *
 * 切换对话时调用——查询 `get_conversation_system_prompt`，
 * 有提示词且未被本会话关闭则显示，否则隐藏。
 * @param {string} conversationId
 */
async function updatePromptBanner(conversationId) {
  const banner = document.getElementById("chat-prompt-banner");
  const body = document.getElementById("chat-prompt-banner-body");
  if (!banner || !body) return;

  if (!conversationId) {
    banner.hidden = true;
    return;
  }

  // 本会话已关闭
  if (state.dismissedPromptConvs.has(conversationId)) {
    banner.hidden = true;
    return;
  }

  try {
    const prompt = await ipc.getConversationSystemPrompt(conversationId);
    if (prompt && prompt.trim()) {
      body.textContent = prompt;
      banner.hidden = false;
      // 默认展开
      banner.removeAttribute("data-collapsed");
    } else {
      banner.hidden = true;
    }
  } catch (e) {
    banner.hidden = true;
  }
}

// ── 设置 ────────────────────────────────────────

function openSettings() {
  // 跳转到设置页「AI 对话设置」tab
  invoke("open_settings_tab", { tab: "ai-chat" });
}

// ── 模型选择器（0.12.2 §4.4） ──────────────────

/** @type {HTMLElement} 模型触发器按钮 */
let modelTrigger = null;
/** @type {HTMLElement} 下拉容器 */
let modelDropdown = null;

/**
 * 更新 header 的 provider/model 标签。
 * 只显示模型 display name，不显示 provider 名和 badge。
 * @param {object} status ChatStatus（model_name）
 */
function updateProviderLabel(status) {
  const label = document.getElementById("chat-provider-label");
  if (!label) return;
  label.textContent = status.model_name || "未配置模型";
}

/**
 * 刷新 tool 池规模指示 + MCP tool 名称集合（0.13.0）。
 *
 * 加载内置 + MCP tool 数量，在 composer bar 显示「内置 N + MCP M = K tools」。
 * 同时加载 MCP tool 名称集合，供工具卡片渲染时标记来源。
 *
 * 0.13.8: 加 5 秒节流——窗口 focus 时无脑刷新会导致 3 个 IPC 频繁调用
 * （其中 get_mcp_tool_pool_size 内部还遍历所有 server），节流后只在
 * 首次 focus 或距上次刷新 >5s 时才发 IPC。
 */
let _refreshToolPoolLastTs = 0;
const _REFRESH_TOOL_POOL_THROTTLE_MS = 5000;

async function refreshToolPool(force = false) {
  // 节流：5 秒内不重复刷新（force=true 时跳过节流，供事件驱动的关键刷新用）
  const now = Date.now();
  if (!force && now - _refreshToolPoolLastTs < _REFRESH_TOOL_POOL_THROTTLE_MS) return;
  _refreshToolPoolLastTs = now;

  const el = document.getElementById("chat-tool-pool");
  try {
    const [size, names, sources] = await Promise.all([
      ipc.getMcpToolPoolSize(),
      ipc.getMcpToolNames(),
      ipc.getMcpToolSources(),
    ]);
    state.setMcpToolNames(names);
    state.setMcpToolSources(sources);

    // 隐藏 tool pool 文本——composer bar 只显示圆圈进度条
    // tool 数量信息已聚合到 hover popup 中展示
    if (el) el.classList.add('hidden');
  } catch (e) {
    console.error("[chat] refreshToolPool 失败:", e);
    if (el) el.classList.add('hidden');
  }
}

/** 拉取模型列表 + 状态，刷新下拉和标签 */
async function refreshModelSelector() {
  try {
    const [models, status] = await Promise.all([
      ipc.getChatModels(),
      ipc.getChatStatus(),
    ]);
    state.setProviderConfigured(status.provider_configured);
    renderModelDropdown(models);
    updateProviderLabel(status);
  } catch (e) {
    console.error("[chat] 刷新模型选择器失败:", e);
  }
}

/**
 * 渲染模型选择器下拉。
 * 结构：快捷档（主档/轻量档）+ 分隔线 + 全部模型列表。
 * @param {Array<{id, provider_name, model_name, is_main, is_light, is_selected}>} models
 */
function renderModelDropdown(models) {
  if (!modelDropdown) return;
  if (!models || models.length === 0) {
    modelDropdown.innerHTML =
      '<div class="chat-model-empty">暂无可用模型，请先在设置中配置</div>';
    return;
  }

  const mainModel = models.find((m) => m.is_main);
  const lightModel = models.find((m) => m.is_light);
  const others = models;

  let html = "";
  // 快捷档区：主档 / 轻量档，显示「档位 · 模型显示名」+ provider 名作为副标题
  html += '<div class="chat-model-group">';
  if (mainModel) {
    html += renderModelOption(
      mainModel.id,
      mainModel.model_name,
      mainModel.provider_name,
      mainModel.is_selected,
      "main"
    );
  } else {
    // Main 档未配置：给出占位提示
    html += renderModelOption(null, "主档未配置", "", false, "main");
  }
  if (lightModel) {
    html += renderModelOption(
      lightModel.id,
      lightModel.model_name,
      lightModel.provider_name,
      lightModel.is_selected,
      "light"
    );
  }
  html += "</div>";

  // 全部模型列表
  html += '<div class="chat-model-separator"></div>';
  html += '<div class="chat-model-group">';
  html += '<div class="chat-model-group-title">所有模型</div>';
  for (const m of others) {
    html += renderModelOption(
      m.id,
      m.model_name,
      m.provider_name,
      m.is_selected,
      ""
    );
  }
  html += "</div>";

  modelDropdown.innerHTML = html;
}

/**
 * 渲染单个下拉选项 HTML。
 * 0.12.3 重新设计：供应商名 + 模型名两行布局，左对齐，供应商用弱色小字区分。
 * @param {string|null} id
 * @param {string} label 模型显示名
 * @param {string} providerName 供应商名
 * @param {boolean} selected
 * @param {string} badge "main"/"light"/""
 */
function renderModelOption(id, label, providerName, selected, badge) {
  const badgeHtml = badge
    ? `<span class="chat-model-badge chat-model-badge-${badge}">${badge === "main" ? "主" : "轻"}</span>`
    : '<span class="chat-model-badge-placeholder"></span>';
  const providerHtml = providerName
    ? `<span class="chat-model-option-provider">${escapeText(providerName)}</span>`
    : '';
  return `<div class="chat-model-option${selected ? " chat-model-option-selected" : ""}" data-model-id="${id ?? ""}" title="${escapeText(providerName ? providerName + ' · ' + label : label)}">
    ${badgeHtml}
    <div class="chat-model-option-text">
      <span class="chat-model-option-name">${escapeText(label)}</span>
      ${providerHtml}
    </div>
    ${selected ? '<span class="chat-model-check">✓</span>' : '<span class="chat-model-check-placeholder"></span>'}
  </div>`;
}

/** 绑定模型选择器交互（触发器 toggle + 选项点击 + 外部关闭） */
function bindModelSelector() {
  modelTrigger = document.getElementById("chat-model-trigger");
  modelDropdown = document.getElementById("chat-model-dropdown");
  if (!modelTrigger || !modelDropdown) return;

  // 触发器点击 toggle 下拉
  modelTrigger.addEventListener("click", (e) => {
    e.stopPropagation();
    toggleDropdown();
  });

  // 下拉项点击（事件委托，因 innerHTML 重渲染）
  // 0.12.4 §6.2：加 stopPropagation 阻止事件冒泡到 trigger，避免 hideDropdown 后被 toggle 重开
  modelDropdown.addEventListener("click", async (e) => {
    e.stopPropagation();
    const opt = e.target.closest(".chat-model-option");
    if (!opt) return;
    const id = opt.dataset.modelId || null;
    hideDropdown();
    try {
      const ok = await ipc.selectChatModel(id);
      if (ok) {
        await refreshModelSelector();
      }
    } catch (err) {
      console.error("[chat] 切换模型失败:", err);
    }
  });

  // 点击外部关闭下拉（下拉现在在触发器内部，所以检查点击是否在触发器外）
  document.addEventListener("click", (e) => {
    if (!modelDropdown.hidden && !modelTrigger.contains(e.target)) {
      hideDropdown();
    }
  });
}

function toggleDropdown() {
  if (modelDropdown.hidden) {
    modelDropdown.hidden = false;
    modelTrigger.classList.add("active");
  } else {
    hideDropdown();
  }
}

function hideDropdown() {
  if (!modelDropdown) return;
  modelDropdown.hidden = true;
  if (modelTrigger) modelTrigger.classList.remove("active");
}

// ── 绑定 header 按钮 ───────────────────────────

function bindHeaderButtons() {
  const newBtn = document.getElementById("chat-new-btn");
  const settingsBtn = document.getElementById("chat-settings-btn");

  if (newBtn) newBtn.addEventListener("click", handleNewConversation);
  if (settingsBtn) settingsBtn.addEventListener("click", openSettings);

  // Titlebar 按钮
  const minimizeBtn = document.getElementById("titlebar-minimize");
  const maximizeBtn = document.getElementById("titlebar-maximize");
  const closeBtn = document.getElementById("titlebar-close");

  if (minimizeBtn) minimizeBtn.addEventListener("click", () => {
    getCurrentWindow().minimize();
  });
  if (maximizeBtn) maximizeBtn.addEventListener("click", async () => {
    const win = getCurrentWindow();
    const isMax = await win.isMaximized();
    isMax ? win.unmaximize() : win.maximize();
  });
  if (closeBtn) closeBtn.addEventListener("click", () => {
    ipc.hideChatWindow();
  });
}

// ── 启动 ────────────────────────────────────────

document.addEventListener("DOMContentLoaded", () => {
  bindHeaderButtons();
  init();
});
