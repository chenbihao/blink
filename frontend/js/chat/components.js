/**
 * chat UI 组件（0.12.1 Phase 5）。
 *
 * 负责创建和更新消息 bubble、Tool 状态卡、确认卡片和空状态。
 */

import { renderMarkdown } from "./renderer.js";

/** @type {HTMLElement} 消息容器 */
let messagesEl = null;

/**
 * 初始化组件模块，绑定 DOM 引用。
 */
export function initComponents() {
  messagesEl = document.getElementById("chat-messages");
}

/**
 * 清空消息容器。
 */
export function clearMessages() {
  if (messagesEl) messagesEl.innerHTML = "";
}

/**
 * 渲染用户消息 bubble。
 * @param {string} text
 */
export function renderUserMessage(text) {
  if (!messagesEl) return;
  const el = document.createElement("div");
  el.className = "chat-msg chat-msg-user";
  el.textContent = text;
  messagesEl.appendChild(el);
  scrollToBottom();
}

/**
 * 创建 assistant 消息 DOM（流式追加用）。
 * @returns {HTMLElement} 消息元素引用
 */
export function createAssistantMessage() {
  if (!messagesEl) return null;
  const el = document.createElement("div");
  el.className = "chat-msg chat-msg-assistant streaming";
  el.innerHTML = "";
  messagesEl.appendChild(el);
  scrollToBottom();
  return el;
}

/**
 * 更新 assistant 消息内容（流式渲染）。
 * @param {HTMLElement} el 消息元素
 * @param {string} text 累积 Markdown 文本
 */
export function updateAssistantMessage(el, text) {
  if (!el) return;
  el.innerHTML = renderMarkdown(text);
  scrollToBottom();
}

/**
 * 完成 assistant 消息（移除 streaming 样式，最终渲染）。
 * @param {HTMLElement} el 消息元素
 * @param {string} text 完整 Markdown 文本
 */
export function finalizeAssistantMessage(el, text) {
  if (!el) return;
  el.classList.remove("streaming");
  el.innerHTML = renderMarkdown(text);
  scrollToBottom();
}

/**
 * 渲染 Tool 状态卡。
 * @param {string} toolName
 * @returns {HTMLElement} 卡片元素引用
 */
export function renderToolStatus(toolName) {
  if (!messagesEl) return null;
  const el = document.createElement("div");
  el.className = "chat-tool-card";
  el.innerHTML = `
    <div class="chat-tool-card-spinner"></div>
    <span>正在调用 ${escapeText(toolName)}...</span>
  `;
  messagesEl.appendChild(el);
  scrollToBottom();
  return el;
}

/**
 * 更新 Tool 状态卡为完成状态。
 * @param {HTMLElement} el 卡片元素
 * @param {boolean} success
 */
export function finalizeToolStatus(el, success) {
  if (!el) return;
  const spinner = el.querySelector(".chat-tool-card-spinner");
  if (spinner) {
    spinner.className = "chat-tool-card-icon";
    spinner.textContent = success ? "✓" : "✗";
  }
}

/**
 * 渲染危险操作确认卡片。
 * @param {{confirm_id: number, tool_name: string, tool_type: string, danger_class: string}} payload
 * @param {(confirmId: number, approved: boolean) => void} onConfirm
 * @returns {HTMLElement} 卡片元素引用
 */
export function renderConfirmCard(payload, onConfirm) {
  if (!messagesEl) return null;
  const el = document.createElement("div");
  el.className = "chat-confirm-card";
  el.innerHTML = `
    <div class="chat-confirm-card-title">⚠ 危险操作确认</div>
    <div class="chat-confirm-card-tool">
      ${escapeText(payload.tool_type)}: <strong>${escapeText(payload.tool_name)}</strong>
    </div>
    <div class="chat-confirm-card-actions">
      <button class="chat-confirm-btn chat-confirm-btn-reject" data-action="reject">拒绝</button>
      <button class="chat-confirm-btn chat-confirm-btn-approve" data-action="approve">允许执行</button>
    </div>
  `;
  // 绑定按钮事件
  el.querySelector("[data-action='reject']").addEventListener("click", () => {
    onConfirm(payload.confirm_id, false);
    el.querySelector(".chat-confirm-card-actions").innerHTML =
      '<span style="color: var(--text-muted); font-size: 13px;">已拒绝</span>';
  });
  el.querySelector("[data-action='approve']").addEventListener("click", () => {
    onConfirm(payload.confirm_id, true);
    el.querySelector(".chat-confirm-card-actions").innerHTML =
      '<span style="color: var(--warning); font-size: 13px;">执行中...</span>';
  });
  messagesEl.appendChild(el);
  scrollToBottom();
  return el;
}

/**
 * 渲染错误消息。
 * @param {string} message
 */
export function renderErrorMessage(message) {
  if (!messagesEl) return;
  const el = document.createElement("div");
  el.className = "chat-msg-error";
  el.textContent = message;
  messagesEl.appendChild(el);
  scrollToBottom();
}

/**
 * 渲染空状态（provider 未配置或首次打开）。
 * @param {boolean} providerConfigured
 * @param {() => void} onOpenSettings
 */
export function renderEmptyState(providerConfigured, onOpenSettings) {
  if (!messagesEl) return;
  messagesEl.innerHTML = "";
  const el = document.createElement("div");
  el.className = "chat-empty";
  if (providerConfigured) {
    el.innerHTML = `
      <div class="chat-empty-icon">AI</div>
      <h2>Blink AI 对话</h2>
      <p>输入消息开始对话。AI 可以调用工具帮你完成操作。</p>
    `;
  } else {
    el.innerHTML = `
      <div class="chat-empty-icon">!</div>
      <h2>AI 未配置</h2>
      <p>请先在设置中配置 AI Provider 和模型。</p>
      <button class="chat-empty-btn" id="chat-open-settings">打开设置</button>
    `;
    // 延迟绑定（DOM 刚插入）
    requestAnimationFrame(() => {
      const btn = document.getElementById("chat-open-settings");
      if (btn) btn.addEventListener("click", onOpenSettings);
    });
  }
  messagesEl.appendChild(el);
}

/**
 * 移除空状态。
 */
export function removeEmptyState() {
  if (!messagesEl) return;
  const empty = messagesEl.querySelector(".chat-empty");
  if (empty) empty.remove();
}

/**
 * 自动滚动到底部。
 */
export function scrollToBottom() {
  if (!messagesEl) return;
  requestAnimationFrame(() => {
    messagesEl.scrollTop = messagesEl.scrollHeight;
  });
}

/**
 * 文本转义（防 XSS）。
 * @param {string} text
 * @returns {string}
 */
function escapeText(text) {
  const div = document.createElement("div");
  div.textContent = text;
  return div.innerHTML;
}
