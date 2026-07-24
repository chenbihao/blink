/**
 * chat 窗口入口（0.12.1 Phase 5）。
 *
 * 装配 state / ipc / renderer / components / composer，注册事件监听。
 */

import * as state from "./state.js";
import * as ipc from "./ipc.js";
import { initRenderer } from "./renderer.js";
import * as components from "./components.js";
import { initComposer, setStreamingMode, setInputMode, clearInput, focusInput, setThinkingEnabled as setComposerThinking } from "./composer.js";
import { applyThemeFromConfig } from "../theme.js";
import { listen, invoke } from "../tauri.js";

/** 流式渲染节流：requestAnimationFrame 句柄 */
let rafHandle = 0;

/** 当前 assistant 消息 DOM 引用 */
let currentAssistantEl = null;

/** 当前 Tool 状态卡 DOM 引用 */
let currentToolEl = null;

// ── 初始化 ──────────────────────────────────────

async function init() {
  // 主题初始化 + 实时跟随
  applyThemeFromConfig();
  // 0.12.2 §4.9: ai_config 变更时刷新模型选择器（复用 config-changed 事件）
  listen("blink://config-changed", (e) => {
    applyThemeFromConfig();
    if (e?.payload?.key === "ai_config") {
      refreshModelSelector();
    }
  });

  initRenderer();
  components.initComponents();

  initComposer({
    onSend: handleSend,
    onStop: handleStop,
    onThinkingToggle: handleThinkingToggle,
  });

  // 注册事件监听
  await ipc.listenChatStream(handleStreamEvent);
  await ipc.listenChatConfirm(handleConfirmEvent);

  // 初始状态：检查 provider 配置 + 加载模型选择器
  try {
    const status = await ipc.getChatStatus();
    state.setProviderConfigured(status.provider_configured);
    updateProviderLabel(status);
    components.renderEmptyState(status.provider_configured, openSettings);
  } catch (e) {
    console.error("[chat] getChatStatus 失败:", e);
    components.renderEmptyState(false, openSettings);
  }
  refreshModelSelector();

  // 模型选择器交互
  bindModelSelector();

  // Esc 键：生成中 → abort；空闲 → 隐藏窗口
  document.addEventListener("keydown", handleEsc);

  // 窗口获得焦点时自动聚焦输入框
  window.addEventListener("focus", () => focusInput());

  // 初始聚焦
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
      state.appendStreamBuffer(chunk.text);
      scheduleRender();
      break;

    case "tool_call":
      // 结束当前 assistant 消息的流式状态（可能有 thinking 而无 text）
      if (currentAssistantEl && (state.streamBuffer || state.thinkingBuffer)) {
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
      currentToolEl = components.renderToolStatus(chunk.tool);
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

    case "done":
      finalizeDone(chunk);
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
  if (currentAssistantEl) {
    components.finalizeAssistantMessage(currentAssistantEl, state.streamBuffer, state.thinkingBuffer);
    state.addMessage({ role: "assistant", content: state.streamBuffer });
  }
  // 完成 Tool 状态卡（若有未配对到 result 的卡片，默认成功）
  if (currentToolEl) {
    components.finalizeToolStatus(currentToolEl, true);
    currentToolEl = null;
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
  finishStreaming();
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

// ── 模型选择器（0.12.2 §4.4） ──────────────────

/** @type {HTMLElement} 模型触发器按钮 */
let modelTrigger = null;
/** @type {HTMLElement} 下拉容器 */
let modelDropdown = null;

/**
 * 更新 header 的 provider/model 标签。
 * 0.12.2：当选中模型是主档/轻量档时，label 前显示对应 badge。
 * @param {object} status ChatStatus（provider_name/model_name）
 * @param {Array} [models] 模型列表，用于判断当前选中项的档位
 */
function updateProviderLabel(status, models) {
  const label = document.getElementById("chat-provider-label");
  if (!label) return;

  // 判断当前选中模型是否是主档/轻量档（从 models 的 is_selected 项读）
  const selected = models?.find((m) => m.is_selected);
  const tier = selected?.is_main ? "main" : selected?.is_light ? "light" : "";
  const badgeHtml = tier
    ? `<span class="chat-model-badge chat-model-badge-${tier}">${tier === "main" ? "主" : "轻"}</span>`
    : "";

  const providerHtml = status.provider_name
    ? `<span class="chat-model-provider">${escapeText(status.provider_name)}</span>`
    : "";
  const modelHtml = status.model_name
    ? `<span class="chat-model-name">${escapeText(status.model_name)}</span>`
    : `<span class="chat-model-name">未配置模型</span>`;

  label.innerHTML = `${badgeHtml}${providerHtml}${modelHtml}`;
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
    updateProviderLabel(status, models);
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
 * @param {string|null} id 模型 id（null = Main 档快捷项，传 null 给 selectChatModel 恢复默认）
 * @param {string} label 显示名（主档/轻量档传模型名，会与 provider 拼接）
 * @param {string} providerName provider 名
 * @param {boolean} selected 是否当前选中
 * @param {string} badge 角标 "main"/"light"/""
 */
function renderModelOption(id, label, providerName, selected, badge) {
  const badgeHtml = badge
    ? `<span class="chat-model-badge chat-model-badge-${badge}">${badge === "main" ? "主" : "轻"}</span>`
    : '<span class="chat-model-badge-placeholder"></span>';
  // 显示文案：供应商 · 模型名（供应商为空时只显示模型名）
  const displayText = providerName ? `${providerName} · ${label}` : label;
  return `<div class="chat-model-option${selected ? " chat-model-option-selected" : ""}" data-model-id="${id ?? ""}" title="${escapeText(displayText)}">
    ${badgeHtml}
    <span class="chat-model-option-name">${escapeText(displayText)}</span>
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
  modelDropdown.addEventListener("click", async (e) => {
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

/**
 * 文本转义（防 XSS）。复用 components 的 escapeText 不便（未导出），本地实现。
 */
function escapeText(text) {
  const div = document.createElement("div");
  div.textContent = String(text ?? "");
  return div.innerHTML;
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

/** 获取当前窗口对象（Tauri v2 Window） */
function getCurrentWindow() {
  return window.__TAURI__?.window?.getCurrentWindow?.();
}

// ── 启动 ────────────────────────────────────────

document.addEventListener("DOMContentLoaded", () => {
  bindHeaderButtons();
  init();
});
