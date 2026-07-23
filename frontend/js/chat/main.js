/**
 * chat 窗口入口（0.12.1 Phase 5）。
 *
 * 装配 state / ipc / renderer / components / composer，注册事件监听。
 */

import * as state from "./state.js";
import * as ipc from "./ipc.js";
import { initRenderer } from "./renderer.js";
import * as components from "./components.js";
import { initComposer, setStreamingMode, setInputMode, clearInput, focusInput } from "./composer.js";

/** 流式渲染节流：requestAnimationFrame 句柄 */
let rafHandle = 0;

/** 当前 assistant 消息 DOM 引用 */
let currentAssistantEl = null;

/** 当前 Tool 状态卡 DOM 引用 */
let currentToolEl = null;

// ── 初始化 ──────────────────────────────────────

async function init() {
  initRenderer();
  components.initComponents();

  initComposer({
    onSend: handleSend,
    onStop: handleStop,
  });

  // 注册事件监听
  await ipc.listenChatStream(handleStreamEvent);
  await ipc.listenChatConfirm(handleConfirmEvent);

  // 初始状态：检查 provider 配置
  try {
    const status = await ipc.getChatStatus();
    state.setProviderConfigured(status.provider_configured);
    components.renderEmptyState(status.provider_configured, openSettings);
  } catch (e) {
    console.error("[chat] getChatStatus 失败:", e);
    components.renderEmptyState(false, openSettings);
  }

  // Esc 键：生成中 → abort；空闲 → 隐藏窗口
  document.addEventListener("keydown", handleEsc);

  // 窗口显示时聚焦输入框
  focusInput();
}

// ── 发送 ────────────────────────────────────────

async function handleSend(message) {
  // 移除空状态
  components.removeEmptyState();

  // 添加用户消息
  components.renderUserMessage(message);
  state.addMessage({ role: "user", content: message });

  // 切换到流式模式
  setStreamingMode();
  state.setStreaming(true);
  state.resetStreamBuffer();

  // 创建 assistant 消息 DOM
  currentAssistantEl = components.createAssistantMessage();

  try {
    const requestId = await ipc.chatPrompt(state.conversationId, message);
    state.setActiveRequestId(requestId);
  } catch (e) {
    console.error("[chat] chatPrompt 失败:", e);
    components.renderErrorMessage(`发送失败: ${e}`);
    finishStreaming();
  }

  clearInput();
}

// ── 停止 ────────────────────────────────────────

async function handleStop() {
  if (state.activeRequestId != null) {
    await ipc.chatAbort(state.activeRequestId);
  }
}

// ── 流式事件处理 ────────────────────────────────

function handleStreamEvent(event) {
  const { request_id, conversation_id, chunk } = event.payload;

  // 忽略不属于当前请求的 chunk
  if (request_id !== state.activeRequestId) return;
  if (conversation_id !== state.conversationId) return;

  switch (chunk.kind) {
    case "text":
      state.appendStreamBuffer(chunk.text);
      scheduleRender();
      break;

    case "tool_call":
      // 结束当前 assistant 消息的流式状态
      if (state.streamBuffer && currentAssistantEl) {
        components.finalizeAssistantMessage(currentAssistantEl, state.streamBuffer);
        state.addMessage({ role: "assistant", content: state.streamBuffer });
        state.resetStreamBuffer();
        currentAssistantEl = null;
      }
      // 显示 Tool 状态卡
      currentToolEl = components.renderToolStatus(chunk.tool);
      break;

    case "done":
      finalizeDone();
      break;

    case "error":
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
      components.updateAssistantMessage(currentAssistantEl, state.streamBuffer);
    }
  });
}

/**
 * Done：最终渲染 + 清理。
 */
function finalizeDone() {
  if (currentAssistantEl) {
    components.finalizeAssistantMessage(currentAssistantEl, state.streamBuffer);
    state.addMessage({ role: "assistant", content: state.streamBuffer });
  }
  // 完成 Tool 状态卡
  if (currentToolEl) {
    components.finalizeToolStatus(currentToolEl, true);
    currentToolEl = null;
  }
  finishStreaming();
}

/**
 * 结束流式状态，恢复 composer。
 */
function finishStreaming() {
  state.setStreaming(false);
  state.setActiveRequestId(null);
  state.resetStreamBuffer();
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

  components.renderConfirmCard(payload, async (confirmId, approved) => {
    try {
      await ipc.confirmChatAction(confirmId, approved);
    } catch (e) {
      console.error("[chat] confirmChatAction 失败:", e);
    }
  });
}

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

// ── 新对话 ──────────────────────────────────────

function handleNewConversation() {
  if (state.isStreaming) {
    handleStop();
  }
  state.resetConversation();
  components.clearMessages();
  components.renderEmptyState(state.providerConfigured, openSettings);
  focusInput();
}

// ── 设置 ────────────────────────────────────────

function openSettings() {
  // 通过内置动作打开设置页
  import("../tauri.js").then(({ invoke }) => {
    invoke("run_builtin_action", { id: "open_settings" });
  });
}

// ── 绑定 header 按钮 ───────────────────────────

function bindHeaderButtons() {
  const newBtn = document.getElementById("chat-new-btn");
  const settingsBtn = document.getElementById("chat-settings-btn");

  if (newBtn) newBtn.addEventListener("click", handleNewConversation);
  if (settingsBtn) settingsBtn.addEventListener("click", openSettings);
}

// ── 启动 ────────────────────────────────────────

document.addEventListener("DOMContentLoaded", () => {
  bindHeaderButtons();
  init();
});
