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
 * 取最后一条已渲染的 assistant 消息元素（0.12.2 §4.8 token 用量挂载兜底）。
 * 当流式 currentAssistantEl 已被清空（如纯 tool 调用后 done），从 DOM 回查。
 */
export function getLastAssistantEl() {
  if (!messagesEl) return null;
  return messagesEl.querySelector(".chat-msg-assistant:last-child");
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
  el.className = "chat-msg chat-msg-assistant streaming waiting";
  // 等待响应：显示 typing 指示器（三点跳动），首条内容到达时由 update/finalize 替换
  el.innerHTML = renderTypingIndicator();
  messagesEl.appendChild(el);
  scrollToBottom();
  return el;
}

/**
 * 渲染 typing 指示器（等待响应态，0.12.2 设计优化）。
 * 三点跳动动画，首条 text/thinking/tool chunk 到达后由内容渲染自然替换。
 * @returns {string} HTML 字符串
 */
function renderTypingIndicator() {
  return `<div class="chat-typing"><span></span><span></span><span></span></div>`;
}

/**
 * 更新 assistant 消息内容（流式渲染）。
 * 0.12.3：思考过程独立气泡（不再嵌在 assistant 气泡内部），渲染为独立卡片。
 * @param {HTMLElement} el 消息元素
 * @param {string} text 累积 Markdown 文本
 * @param {string} [thinkingText] 累积 thinking 文本（可选）
 * @param {boolean} [thinkingDone=false] thinking 是否已结束（收起折叠）
 */
export function updateAssistantMessage(el, text, thinkingText, thinkingDone) {
  if (!el) return;
  el.classList.remove("waiting");
  // 思考过程作为独立气泡，渲染在 assistant 气泡之前
  const thinkingHtml = renderThinkingBlock(thinkingText, thinkingDone);
  el.innerHTML = thinkingHtml + `<div class="chat-assistant-content">${renderMarkdown(text)}</div>`;
  scrollToBottom();
}

/**
 * 完成 assistant 消息（移除 streaming 样式，最终渲染）。
 * 0.12.2 §4.6：注入 hover 复制按钮 + 代码块复制按钮，原始 Markdown 存 dataset。
 * 0.12.3：思考过程独立气泡。
 * @param {HTMLElement} el 消息元素
 * @param {string} text 完整 Markdown 文本
 * @param {string} [thinkingText] 完整 thinking 文本（可选）
 */
export function finalizeAssistantMessage(el, text, thinkingText) {
  if (!el) return;
  el.classList.remove("streaming", "waiting");
  el.dataset.rawText = text;
  const thinkingHtml = renderThinkingBlock(thinkingText, true);
  el.innerHTML = thinkingHtml + `<div class="chat-assistant-content">${renderMarkdown(text)}</div>`;
  injectCopyButton(el, text);
  injectCodeCopyButtons(el);
  scrollToBottom();
}

/**
 * 注入消息级 hover 复制按钮（0.12.2 §4.6）。
 * 按钮绝对定位在消息右上角，点击复制原始 Markdown 文本。
 * @param {HTMLElement} el 消息元素
 * @param {string} rawText 原始 Markdown 文本
 */
function injectCopyButton(el, rawText) {
  const actions = document.createElement("div");
  actions.className = "chat-msg-actions";
  const btn = document.createElement("button");
  btn.className = "chat-copy-btn";
  btn.title = "复制";
  btn.textContent = "复制";
  btn.addEventListener("click", async () => {
    try {
      await navigator.clipboard.writeText(rawText || "");
      flashCopyDone(btn);
    } catch (e) {
      console.error("[chat] 复制失败:", e);
    }
  });
  actions.appendChild(btn);
  el.appendChild(actions);
}

/**
 * 给消息内每个 `<pre><code>` 注入代码块复制按钮（0.12.2 §4.6）。
 * 按钮绝对定位在代码块右上角，点击复制 code.textContent。
 * @param {HTMLElement} el 消息元素
 */
function injectCodeCopyButtons(el) {
  const codeBlocks = el.querySelectorAll("pre code");
  codeBlocks.forEach((code) => {
    const pre = code.parentElement; // <pre>
    if (!pre || pre.querySelector(".chat-code-copy")) return; // 防重复注入
    const btn = document.createElement("button");
    btn.className = "chat-code-copy";
    btn.title = "复制代码";
    btn.textContent = "复制";
    btn.addEventListener("click", async () => {
      try {
        await navigator.clipboard.writeText(code.textContent || "");
        flashCopyDone(btn);
      } catch (e) {
        console.error("[chat] 代码块复制失败:", e);
      }
    });
    pre.appendChild(btn);
  });
}

/**
 * 复制按钮短暂显示 ✓ 反馈（1.5s）。
 * @param {HTMLButtonElement} btn
 */
function flashCopyDone(btn) {
  const original = btn.textContent;
  btn.textContent = "✓";
  btn.classList.add("chat-copy-btn-done");
  setTimeout(() => {
    btn.textContent = original;
    btn.classList.remove("chat-copy-btn-done");
  }, 1500);
}

/**
 * 渲染 thinking 折叠块（0.12.2 样式优化：卡片风格，与 tool card 统一）。
 * @param {string} text thinking 文本
 * @param {boolean} collapsed 是否收起（默认展开）
 * @returns {string} HTML 字符串
 */
function renderThinkingBlock(text, collapsed) {
  if (!text) return "";
  const openAttr = collapsed ? "" : " open";
  // Lucide brain 图标内联（chat 窗口未引入 sprite，与 chat.html 现有内联 SVG 风格一致）
  const icon = `<svg class="thinking-icon" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M12 5a3 3 0 1 0-5.997.125 4 4 0 0 0-2.526 5.77 4 4 0 0 0 .556 6.588A4 4 0 1 0 12 18Z"/><path d="M12 5a3 3 0 1 1 5.997.125 4 4 0 0 1 2.526 5.77 4 4 0 0 1-.556 6.588A4 4 0 1 1 12 18Z"/><path d="M15 13a4.5 4.5 0 0 1-3-4 4.5 4.5 0 0 1-3 4"/><path d="M17.599 6.5a3 3 0 0 0 .399-1.375"/><path d="M6.003 5.125A3 3 0 0 0 6.401 6.5"/><path d="M3.477 10.896a4 4 0 0 1 .585-.396"/><path d="M19.938 10.5a4 4 0 0 1 .585.396"/><path d="M6 18a4 4 0 0 1-1.967-.516"/><path d="M19.967 17.484A4 4 0 0 1 18 18"/></svg>`;
  return `<details class="chat-card thinking-block"${openAttr}><summary>${icon}<span class="thinking-label">思考过程</span></summary><div class="thinking-content">${renderMarkdown(text)}</div></details>`;
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
 * 在 Tool 卡片内追加可折叠的结果摘要（0.12.2 §4.7）。
 * @param {HTMLElement} el 卡片元素
 * @param {string} summary 结果摘要文本
 * @param {boolean} success 是否成功
 */
export function appendToolResult(el, summary, success) {
  if (!el) return;
  // 若已有结果区，先移除（防重复）
  const existing = el.querySelector(".chat-tool-result");
  if (existing) existing.remove();

  const result = document.createElement("details");
  result.className = "chat-tool-result" + (success ? "" : " chat-tool-result-error");
  const summaryEl = document.createElement("summary");
  summaryEl.textContent = success ? "结果" : "结果（失败）";
  const pre = document.createElement("pre");
  pre.textContent = summary || "(空结果)";
  result.appendChild(summaryEl);
  result.appendChild(pre);
  el.appendChild(result);
}

/**
 * 在 assistant 消息底部追加模型名标签（0.12.3）。
 * @param {HTMLElement} el assistant 消息元素
 * @param {string} modelName 模型显示名
 */
export function renderModelLabel(el, modelName) {
  if (!el || !modelName) return;
  const existing = el.querySelector(".chat-msg-model");
  if (existing) existing.remove();
  const label = document.createElement("div");
  label.className = "chat-msg-model";
  label.textContent = modelName;
  el.appendChild(label);
}

/**
 * 在 assistant 消息底部追加 token 用量（0.12.2 §4.8）。
 * @param {HTMLElement} el assistant 消息元素
 * @param {number} inputTokens
 * @param {number} outputTokens
 */
export function renderTokenUsage(el, inputTokens, outputTokens) {
  if (!el) return;
  // 移除已有的 token 用量（流式刷新场景）
  const existing = el.querySelector(".chat-token-usage");
  if (existing) existing.remove();
  const usage = document.createElement("div");
  usage.className = "chat-token-usage";
  usage.textContent = `↑ ${inputTokens} · ↓ ${outputTokens} tokens`;
  el.appendChild(usage);
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
