/**
 * chat UI 组件（0.12.1 Phase 5）。
 *
 * 负责创建和更新消息 bubble、Tool 状态卡、确认卡片和空状态。
 */

import { renderMarkdown, highlightCodeBlocks } from "./renderer.js";
import { escapeText } from "./utils.js";
import { isMcpTool } from "./state.js";

/** @type {HTMLElement} 消息容器 */
let messagesEl = null;

/** @type {((msgIndex: number, newText: string) => void)|null} 编辑消息回调（0.12.5 §5.5） */
let onEditMessage = null;

/**
 * 初始化组件模块，绑定 DOM 引用。
 * @param {{onEditMessage?: (msgIndex: number, newText: string) => void}} [callbacks]
 */
export function initComponents(callbacks = {}) {
  messagesEl = document.getElementById("chat-messages");
  onEditMessage = callbacks.onEditMessage || null;
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
 * @returns {HTMLElement|null} 消息元素引用（0.12.5 §5.5 编辑重发需要）
 */
export function renderUserMessage(text) {
  if (!messagesEl) return null;
  const el = document.createElement("div");
  el.className = "chat-msg chat-msg-user";
  el.textContent = text;
  el.dataset.rawText = text;
  // 0.12.6：hover 操作行（复制 + 编辑）
  const actions = createActionsRow();
  actions.appendChild(createCopyAction(rawText => el.dataset.rawText || rawText));
  if (onEditMessage) {
    actions.appendChild(createEditAction(() => startEditMessage(el, el.dataset.rawText || text)));
  }
  el.appendChild(actions);
  messagesEl.appendChild(el);
  scrollToBottom();
  return el;
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
  // 0.12.6：hover 操作行（复制 + 重试）
  const actions = createActionsRow();
  actions.appendChild(createCopyAction(() => el.dataset.rawText || ""));
  actions.appendChild(createRetryAction());
  el.appendChild(actions);
  injectCodeCopyButtons(el);
  highlightCodeBlocks(el); // 0.12.5 §5.7：代码块语法高亮
  scrollToBottom();
}

// ── hover 操作行（0.12.6 重构）────────────────────────────────────────────────

/** SVG 图标常量 */
const ICONS = {
  copy: `<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><rect x="9" y="9" width="13" height="13" rx="2" ry="2"/><path d="M5 15H4a2 2 0 0 1-2-2V4a2 2 0 0 1 2-2h9a2 2 0 0 1 2 2v1"/></svg>`,
  check: `<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.5" stroke-linecap="round" stroke-linejoin="round"><polyline points="20 6 9 17 4 12"/></svg>`,
  retry: `<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M3 12a9 9 0 0 1 9-9 9.75 9.75 0 0 1 6.74 2.74L21 8"/><path d="M21 3v5h-5"/><path d="M21 12a9 9 0 0 1-9 9 9.75 9.75 0 0 1-6.74-2.74L3 16"/><path d="M8 16H3v5"/></svg>`,
  edit: `<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M11 4H4a2 2 0 0 0-2 2v14a2 2 0 0 0 2 2h14a2 2 0 0 0 2-2v-7"/><path d="M18.5 2.5a2.121 2.121 0 0 1 3 3L12 15l-4 1 1-4 9.5-9.5z"/></svg>`,
  // 0.12.7：工具卡片完成图标（Lucide wrench / check / x）
  toolSuccess: `<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M14.7 6.3a1 1 0 0 0 0 1.4l1.6 1.6a1 1 0 0 0 1.4 0l3.77-3.77a6 6 0 0 1-7.94 7.94l-6.91 6.91a2.12 2.12 0 0 1-3-3l6.91-6.91a6 6 0 0 1 7.94-7.94l-3.76 3.76z"/></svg>`,
  toolFail: `<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><circle cx="12" cy="12" r="10"/><line x1="15" y1="9" x2="9" y2="15"/><line x1="9" y1="9" x2="15" y2="15"/></svg>`,
};

/**
 * 创建操作行容器。hover 时 visibility hidden→visible + 淡入动画。
 * AI 消息浮在左下角外侧，用户消息浮在右下角外侧。
 * @returns {HTMLElement}
 */
function createActionsRow() {
  const row = document.createElement("div");
  row.className = "chat-msg-actions";
  return row;
}

/**
 * 创建图标按钮（通用）。
 * @param {string} iconKey ICONSTab key
 * @param {string} title tooltip
 * @param {() => void} onClick
 * @returns {HTMLButtonElement}
 */
function createIconButton(iconKey, title, onClick) {
  const btn = document.createElement("button");
  btn.className = "chat-action-btn";
  btn.type = "button";
  btn.title = title;
  btn.innerHTML = ICONS[iconKey] || "";
  btn.addEventListener("click", onClick);
  return btn;
}

/**
 * 复制操作按钮。点击复制文本，1.5s 内显示 ✓。
 * @param {() => string | string} rawTextOrGetter 原始文本或获取函数
 * @returns {HTMLButtonElement}
 */
function createCopyAction(rawTextOrGetter) {
  const btn = createIconButton("copy", "复制", async () => {
    const text = typeof rawTextOrGetter === "function" ? rawTextOrGetter() : rawTextOrGetter;
    try {
      await navigator.clipboard.writeText(text || "");
      flashIconDone(btn);
    } catch (e) {
      console.error("[chat] 复制失败:", e);
    }
  });
  return btn;
}

/**
 * 重试按钮（仅 assistant 消息）。点击触发 resend 事件。
 * @returns {HTMLButtonElement}
 */
function createRetryAction() {
  return createIconButton("retry", "重试", () => {
    // 重试 = 重新发送最后一条用户消息
    // 通过自定义事件冒泡到 main.js 处理
    const evt = new CustomEvent("chat:retry", { bubbles: true });
    document.dispatchEvent(evt);
  });
}

/**
 * 编辑按钮（仅用户消息）。
 * @param {() => void} onStartEdit
 * @returns {HTMLButtonElement}
 */
function createEditAction(onStartEdit) {
  return createIconButton("edit", "编辑", onStartEdit);
}

/**
 * 图标按钮短暂切换为 ✓ 反馈（1.2s）。
 * @param {HTMLButtonElement} btn
 */
function flashIconDone(btn) {
  const original = btn.innerHTML;
  btn.innerHTML = ICONS.check;
  btn.classList.add("chat-action-btn-done");
  setTimeout(() => {
    btn.innerHTML = original;
    btn.classList.remove("chat-action-btn-done");
  }, 1200);
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
        flashIconDone(btn);
      } catch (e) {
        console.error("[chat] 代码块复制失败:", e);
      }
    });
    pre.appendChild(btn);
  });
}

// ── 消息编辑重发（0.12.5 §5.5 → 0.12.6 样式重构）───────────────────────────────

/**
 * 启动消息编辑——将消息气泡变为内联编辑区。
 * Enter 重发，Esc 取消。Shift+Enter 换行。底部有确认/取消按钮。
 * @param {HTMLElement} el 用户消息元素
 * @param {string} originalText 原始文本
 */
function startEditMessage(el, originalText) {
  // 计算消息索引（在 state.messages 中的位置）
  const allMsgs = messagesEl.querySelectorAll(".chat-msg");
  const msgIndex = Array.from(allMsgs).indexOf(el);
  if (msgIndex < 0) return;

  // 移除操作行
  const actions = el.querySelector(".chat-msg-actions");
  if (actions) actions.remove();

  // 构建编辑容器
  const editWrap = document.createElement("div");
  editWrap.className = "chat-edit-wrap";

  const textarea = document.createElement("textarea");
  textarea.className = "chat-edit-textarea";
  textarea.value = originalText;
  textarea.rows = 1;
  editWrap.appendChild(textarea);

  // 底部操作行：取消 + 发送
  const editBar = document.createElement("div");
  editBar.className = "chat-edit-bar";

  const hint = document.createElement("span");
  hint.className = "chat-edit-hint";
  hint.textContent = "Enter 发送 · Esc 取消";

  const cancelBtn = document.createElement("button");
  cancelBtn.className = "chat-edit-cancel";
  cancelBtn.textContent = "取消";

  const sendBtn = document.createElement("button");
  sendBtn.className = "chat-edit-send";
  sendBtn.textContent = "发送";

  editBar.appendChild(hint);
  editBar.appendChild(cancelBtn);
  editBar.appendChild(sendBtn);
  editWrap.appendChild(editBar);

  el.textContent = "";
  el.appendChild(editWrap);

  // 自适应高度
  const autoResize = () => {
    textarea.style.height = "auto";
    textarea.style.height = Math.min(textarea.scrollHeight, 200) + "px";
  };
  textarea.addEventListener("input", autoResize);
  autoResize();

  textarea.focus();
  textarea.select();

  let confirmed = false;

  /** 取消编辑，恢复原消息显示 */
  const restoreMessage = (text) => {
    el.textContent = text;
    el.dataset.rawText = text;
    // 重新注入操作行
    const row = createActionsRow();
    row.appendChild(createCopyAction(() => el.dataset.rawText || ""));
    if (onEditMessage) {
      row.appendChild(createEditAction(() => startEditMessage(el, el.dataset.rawText || text)));
    }
    el.appendChild(row);
  };

  /** 确认编辑 → 调回调 */
  const finishEdit = () => {
    if (confirmed) return;
    confirmed = true;
    const newText = textarea.value.trim();
    if (newText && newText !== originalText) {
      onEditMessage(msgIndex, newText);
    } else {
      restoreMessage(originalText);
    }
  };

  /** 取消编辑 */
  const cancelEdit = () => {
    if (confirmed) return;
    confirmed = true;
    restoreMessage(originalText);
  };

  textarea.addEventListener("keydown", (e) => {
    if (e.key === "Enter" && !e.shiftKey) {
      e.preventDefault();
      finishEdit();
    } else if (e.key === "Escape") {
      e.preventDefault();
      cancelEdit();
    }
  });
  sendBtn.addEventListener("click", finishEdit);
  cancelBtn.addEventListener("click", cancelEdit);
  // blur 时延迟检测，避免点按钮时 textarea 先 blur 导致提前取消
  textarea.addEventListener("blur", () => {
    setTimeout(() => {
      if (!confirmed && document.activeElement !== sendBtn && document.activeElement !== cancelBtn) {
        finishEdit();
      }
    }, 100);
  });
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
 * 渲染 Tool 状态卡（0.12.7 §6.6 增强：参数预览 + 耗时追踪）。
 * @param {string} toolName
 * @param {string} [args] 工具参数 JSON 字符串（可选，折叠展示）
 * @returns {HTMLElement} 卡片元素引用
 */
export function renderToolStatus(toolName, args, opts = {}) {
  if (!messagesEl) return null;
  const el = document.createElement("div");
  el.className = "chat-tool-card";
  if (!opts.skipTiming) {
    el.dataset.startTime = Date.now(); // 0.12.7：记录开始时间，finalize 时算耗时
  }
  // 0.12.7：暂存参数，finalizeToolStatus 时合入详情区
  if (args && args.trim() && args.trim() !== "{}") {
    el.dataset.args = formatJson(args);
  }

  const header = document.createElement("div");
  header.className = "chat-tool-card-header";
  // 0.13.0：MCP 来源标记——tool 名在 MCP tool 名称集合中时显示 (MCP) 徽章
  const sourceBadge = isMcpTool(toolName) ? '<span class="chat-tool-card-source mcp">MCP</span>' : "";
  header.innerHTML = `
    <div class="chat-tool-card-spinner"></div>
    <span class="chat-tool-card-name">${escapeText(toolName)}</span>
    ${sourceBadge}
  `;
  el.appendChild(header);

  messagesEl.appendChild(el);
  scrollToBottom();
  return el;
}

/**
 * 更新 Tool 状态卡为完成状态（0.12.7：图标替换 + 追加耗时）。
 * @param {HTMLElement} el 卡片元素
 * @param {boolean} success
 */
export function finalizeToolStatus(el, success) {
  if (!el) return;
  const spinner = el.querySelector(".chat-tool-card-spinner");
  if (spinner) {
    spinner.className = "chat-tool-card-icon";
    spinner.innerHTML = success ? ICONS.toolSuccess : ICONS.toolFail;
    if (!success) {
      spinner.classList.add("chat-tool-card-icon-error");
    }
  }

  // 0.12.7 §6.6：计算并显示耗时（仅对有 startTime 的卡片生效，历史恢复不显示）
  const startTime = el.dataset.startTime;
  if (startTime) {
    const duration = Date.now() - Number(startTime);
    if (duration >= 0) {
      const headerEl = el.querySelector(".chat-tool-card-header");
      if (headerEl && !headerEl.querySelector(".chat-tool-card-duration")) {
        const durText = duration < 1000
          ? `${duration}ms`
          : `${(duration / 1000).toFixed(1)}s`;
        const durSpan = document.createElement("span");
        durSpan.className = "chat-tool-card-duration";
        durSpan.textContent = durText;
        headerEl.appendChild(durSpan);
      }
    }
  }
}

/**
 * 在 Tool 卡片内追加可折叠的结果摘要（0.12.2 §4.7，0.12.7 §6.6 合并参数+结果为一区）。
 * @param {HTMLElement} el 卡片元素
 * @param {string} summary 结果摘要文本
 * @param {boolean} success 是否成功
 */
export function appendToolResult(el, summary, success) {
  if (!el) return;
  // 若已有详情区，先移除（防重复）
  const existing = el.querySelector(".chat-tool-detail");
  if (existing) existing.remove();

  const detail = document.createElement("details");
  detail.className = "chat-tool-detail" + (success ? "" : " chat-tool-detail-error");
  const summaryEl = document.createElement("summary");
  summaryEl.textContent = success ? "详情" : "详情（失败）";
  detail.appendChild(summaryEl);

  // 0.12.7：如果有参数，先显示参数区块
  const argsText = el.dataset.args;
  if (argsText) {
    const argsLabel = document.createElement("div");
    argsLabel.className = "chat-tool-detail-label";
    argsLabel.textContent = "参数";
    detail.appendChild(argsLabel);
    const argsPre = document.createElement("pre");
    argsPre.textContent = argsText;
    injectToolCopyBtn(argsPre);
    detail.appendChild(argsPre);
  }

  // 结果区块
  const resultLabel = document.createElement("div");
  resultLabel.className = "chat-tool-detail-label";
  resultLabel.textContent = "结果";
  detail.appendChild(resultLabel);
  const pre = document.createElement("pre");
  pre.textContent = formatJson(summary) || "(空结果)";
  injectToolCopyBtn(pre);
  detail.appendChild(pre);

  el.appendChild(detail);
}

/**
 * 给工具详情 pre 元素注入复制按钮（复用代码块复制按钮样式）。
 */
function injectToolCopyBtn(pre) {
  const btn = document.createElement("button");
  btn.className = "chat-code-copy";
  btn.title = "复制";
  btn.textContent = "复制";
  btn.addEventListener("click", async () => {
    try {
      await navigator.clipboard.writeText(pre.textContent || "");
      flashIconDone(btn);
    } catch (e) {
      console.error("[chat] 工具详情复制失败:", e);
    }
  });
  pre.appendChild(btn);
}

/**
 * 在 assistant 消息底部追加模型名标签（0.12.3）。
 * 与 token 用量共用 `.chat-msg-footer` 容器（同一行）。
 * @param {HTMLElement} el assistant 消息元素
 * @param {string} modelName 模型显示名
 */
export function renderModelLabel(el, modelName) {
  if (!el || !modelName) return;
  let footer = el.querySelector(".chat-msg-footer");
  if (!footer) {
    footer = document.createElement("div");
    footer.className = "chat-msg-footer";
    el.appendChild(footer);
  }
  let label = footer.querySelector(".chat-msg-model");
  if (!label) {
    label = document.createElement("span");
    label.className = "chat-msg-model";
    footer.appendChild(label);
  }
  label.textContent = modelName;
}

/**
 * 在 assistant 消息底部追加 token 用量（0.12.2 §4.8）。
 * 与模型名共用 `.chat-msg-footer` 容器（同一行）。
 * @param {HTMLElement} el assistant 消息元素
 * @param {number} inputTokens
 * @param {number} outputTokens
 */
export function renderTokenUsage(el, inputTokens, outputTokens) {
  if (!el) return;
  let footer = el.querySelector(".chat-msg-footer");
  if (!footer) {
    footer = document.createElement("div");
    footer.className = "chat-msg-footer";
    el.appendChild(footer);
  }
  let usage = footer.querySelector(".chat-token-usage");
  if (!usage) {
    usage = document.createElement("span");
    usage.className = "chat-token-usage";
    footer.appendChild(usage);
  }
  usage.textContent = `↑ ${inputTokens} · ↓ ${outputTokens} tokens`;
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
 * 渲染时间分隔符（0.12.7 §6.4）。
 *
 * IM 式时间标签——在消息流中居中显示，标记时间跨度。
 * 前端根据消息 created_at 判断是否需要插入（同日间隔 >5 分钟才插入）。
 *
 * @param {number} timestamp Unix 秒
 * @returns {HTMLElement}
 */
export function renderTimeSeparator(timestamp) {
  if (!messagesEl) return null;
  const el = document.createElement("div");
  el.className = "chat-time-sep";
  el.textContent = formatTimeSeparator(timestamp);
  messagesEl.appendChild(el);
  return el;
}

/**
 * 格式化时间分隔符文字。
 *
 * - 今天：HH:MM
 * - 昨天：昨天 HH:MM
 * - 本周内：周X HH:MM
 * - 更早：MM/DD HH:MM
 * @param {number} ts Unix 秒
 * @returns {string}
 */
function formatTimeSeparator(ts) {
  const date = new Date(ts * 1000);
  const now = new Date();
  const hh = String(date.getHours()).padStart(2, "0");
  const mm = String(date.getMinutes()).padStart(2, "0");
  const timeStr = `${hh}:${mm}`;

  const isSameDay = (a, b) =>
    a.getFullYear() === b.getFullYear() &&
    a.getMonth() === b.getMonth() &&
    a.getDate() === b.getDate();

  if (isSameDay(date, now)) {
    return timeStr;
  }

  const yesterday = new Date(now);
  yesterday.setDate(now.getDate() - 1);
  if (isSameDay(date, yesterday)) {
    return `昨天 ${timeStr}`;
  }

  // 7 天内显示星期
  const diffDays = Math.floor((now - date) / (24 * 60 * 60 * 1000));
  if (diffDays < 7) {
    const weekdays = ["日", "一", "二", "三", "四", "五", "六"];
    return `周${weekdays[date.getDay()]} ${timeStr}`;
  }

  const M = String(date.getMonth() + 1).padStart(2, "0");
  const D = String(date.getDate()).padStart(2, "0");
  return `${M}/${D} ${timeStr}`;
}

/**
 * 渲染 Signal 信号消息（0.12.7 §6.3）。
 *
 * IM 式居中系统通知——轻量、无气泡，不干扰对话流但传递重要状态信息。
 * 参考 Telegram「XX 加入群组」/ Discord「频道已创建」/ 微信「XX 撤回消息」。
 *
 * @param {string} text 信号文字
 * @param {"info"|"warning"|"error"|"success"} [type="info"] 信号类型
 * @returns {HTMLElement} 信号元素引用
 */
export function renderSignal(text, type = "info") {
  if (!messagesEl) return null;
  const el = document.createElement("div");
  el.className = `chat-signal chat-signal-${type}`;
  el.textContent = text;
  messagesEl.appendChild(el);
  scrollToBottom();
  return el;
}

/**
 * 渲染错误消息（0.12.7：重构为 Signal error 的封装，保持调用方兼容）。
 * @param {string} message
 */
export function renderErrorMessage(message) {
  renderSignal(message, "error");
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
    // 0.12.5 §5.2：引导泡泡——点击预填充到输入框
    const GUIDE_PROMPTS = [
      { text: "帮我打开微信", hint: "应用" },
      { text: "翻译 hello world", hint: "翻译" },
      { text: "截取屏幕", hint: "截图" },
      { text: "今天天气怎么样", hint: "问答" },
    ];
    const bubblesEl = document.createElement("div");
    bubblesEl.className = "chat-guide-bubbles";
    for (const b of GUIDE_PROMPTS) {
      const btn = document.createElement("button");
      btn.className = "chat-guide-bubble";
      const textSpan = document.createElement("span");
      textSpan.className = "chat-guide-bubble-text";
      textSpan.textContent = b.text;
      const hintSpan = document.createElement("span");
      hintSpan.className = "chat-guide-bubble-hint";
      hintSpan.textContent = b.hint;
      btn.appendChild(textSpan);
      btn.appendChild(hintSpan);
      btn.addEventListener("click", () => {
        const input = document.getElementById("chat-input");
        if (input) {
          input.value = b.text;
          input.focus();
          input.dispatchEvent(new Event("input", { bubbles: true }));
        }
      });
      bubblesEl.appendChild(btn);
    }
    el.appendChild(bubblesEl);
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
 * 尝试格式化 JSON 字符串（0.12.7 §6.6）。
 *
 * 如果输入是合法 JSON，则美化输出（2 空格缩进）；
 * 否则原样返回（可能是纯文本结果）。
 * @param {string} str
 * @returns {string}
 */
function formatJson(str) {
  if (!str) return str;
  try {
    const parsed = JSON.parse(str);
    return JSON.stringify(parsed, null, 2);
  } catch {
    return str;
  }
}
