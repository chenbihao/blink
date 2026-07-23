/**
 * chat 输入组件（0.12.1 Phase 5）。
 *
 * 自增高 textarea + 发送/停止按钮。
 * Enter 发送，Shift+Enter 换行。生成中按钮切换为停止。
 */

/** @type {HTMLTextAreaElement} */
let textarea = null;

/** @type {HTMLButtonElement} */
let sendBtn = null;

/** @type {(message: string) => void} */
let onSend = null;

/** @type {() => void} */
let onStop = null;

/**
 * 初始化 composer。
 * @param {{ onSend: (message: string) => void, onStop: () => void }} callbacks
 */
export function initComposer(callbacks) {
  textarea = document.getElementById("chat-input");
  sendBtn = document.getElementById("chat-send-btn");
  onSend = callbacks.onSend;
  onStop = callbacks.onStop;

  if (!textarea || !sendBtn) return;

  // textarea 自增高
  textarea.addEventListener("input", autoResize);

  // Enter 发送 / Shift+Enter 换行
  textarea.addEventListener("keydown", (e) => {
    if (e.key === "Enter" && !e.shiftKey) {
      e.preventDefault();
      handleSend();
    }
  });

  // 发送/停止按钮
  sendBtn.addEventListener("click", () => {
    if (sendBtn.classList.contains("chat-stop-btn")) {
      handleStop();
    } else {
      handleSend();
    }
  });

  // 初始状态
  updateSendButtonState();
}

/**
 * 设置 composer 为流式模式（显示停止按钮，禁用输入）。
 */
export function setStreamingMode() {
  if (!sendBtn || !textarea) return;
  sendBtn.classList.add("chat-stop-btn");
  sendBtn.innerHTML = stopIcon;
  sendBtn.disabled = false;
  textarea.disabled = true;
}

/**
 * 设置 composer 为输入模式（显示发送按钮，启用输入）。
 */
export function setInputMode() {
  if (!sendBtn || !textarea) return;
  sendBtn.classList.remove("chat-stop-btn");
  sendBtn.innerHTML = sendIcon;
  textarea.disabled = false;
  textarea.focus();
  updateSendButtonState();
}

/**
 * 清空输入框。
 */
export function clearInput() {
  if (!textarea) return;
  textarea.value = "";
  autoResize();
  updateSendButtonState();
}

/**
 * 聚焦输入框。
 */
export function focusInput() {
  if (textarea) textarea.focus();
}

// ── 内部 ─────────────────────────────────────────

function handleSend() {
  const text = textarea?.value?.trim();
  if (!text) return;
  if (onSend) onSend(text);
}

function handleStop() {
  if (onStop) onStop();
}

function autoResize() {
  if (!textarea) return;
  textarea.style.height = "auto";
  textarea.style.height = Math.min(textarea.scrollHeight, 200) + "px";
}

function updateSendButtonState() {
  if (!sendBtn || !textarea) return;
  const hasText = textarea.value.trim().length > 0;
  sendBtn.disabled = !hasText;
}

// ── Icons ────────────────────────────────────────

const sendIcon = `<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><line x1="22" y1="2" x2="11" y2="13"/><polygon points="22 2 15 22 11 13 2 9 22 2"/></svg>`;

const stopIcon = `<svg viewBox="0 0 24 24" fill="currentColor"><rect x="6" y="6" width="12" height="12" rx="2"/></svg>`;
