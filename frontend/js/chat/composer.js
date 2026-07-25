/**
 * chat 输入组件（0.12.1 Phase 5, 0.12.2 深度思考开关, 0.12.3 语音指示器对齐主窗口）。
 *
 * 自增高 textarea + 底部工具栏（深度思考开关 + 发送/停止按钮）。
 * Enter 发送，Shift+Enter 换行。生成中按钮切换为停止。
 *
 * 0.12.3 语音输入对齐主窗口：
 * - 移除麦克风按钮，改为热键驱动（hold Alt+Space）
 * - 语音指示器（5 条波形 + "语音输入中" 标签）同主窗口 G1 风格
 * - voice-partial(target="chat") 实时更新 textarea
 * - voice-recording-start/end + voice-level 驱动指示器 show/hide + 波形动画
 */

/** @type {HTMLTextAreaElement} */
let textarea = null;

/** @type {HTMLButtonElement} */
let sendBtn = null;

/** @type {HTMLButtonElement} */
let thinkingBtn = null;

/** @type {HTMLElement} 语音指示器 */
let voiceIndicator = null;

/** @type {NodeListOf<HTMLElement>} 波形条 */
let vwBars = [];

/** @type {(message: string) => void} */
let onSend = null;

/** @type {() => void} */
let onStop = null;

/** @type {(enabled: boolean) => void} */
let onThinkingToggle = null;

/** @type {boolean} 是否正在语音录音 */
let voiceRecording = false;

/** @type {string} 录音开始前的 textarea 文本（用作 base，识别结果追加其后） */
let voiceBaseText = "";

/**
 * 初始化 composer。
 * @param {{ onSend: (message: string) => void, onStop: () => void, onThinkingToggle: (enabled: boolean) => void }} callbacks
 */
export function initComposer(callbacks) {
  textarea = document.getElementById("chat-input");
  sendBtn = document.getElementById("chat-send-btn");
  thinkingBtn = document.getElementById("chat-thinking-btn");
  voiceIndicator = document.getElementById("voice-indicator");
  vwBars = voiceIndicator ? voiceIndicator.querySelectorAll(".vw-bar") : [];
  onSend = callbacks.onSend;
  onStop = callbacks.onStop;
  onThinkingToggle = callbacks.onThinkingToggle;

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

  // 深度思考开关
  if (thinkingBtn) {
    thinkingBtn.addEventListener("click", () => {
      const newState = !thinkingBtn.classList.contains("active");
      thinkingBtn.classList.toggle("active", newState);
      if (onThinkingToggle) onThinkingToggle(newState);
    });
  }

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
  if (thinkingBtn) thinkingBtn.disabled = true;
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
  if (thinkingBtn) thinkingBtn.disabled = false;
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

/**
 * 设置深度思考开关状态。
 * @param {boolean} enabled
 */
export function setThinkingEnabled(enabled) {
  if (thinkingBtn) {
    thinkingBtn.classList.toggle("active", enabled);
  }
}

// ── 语音指示器（0.12.3 对齐主窗口 G1）────────────────────

/**
 * 语音录音开始：显示指示器 + 禁用发送。
 * 对齐主窗口 lifecycle.js 的 voice-recording-start 处理。
 */
export function showVoiceIndicator() {
  voiceRecording = true;
  voiceBaseText = textarea ? textarea.value : "";
  if (voiceIndicator) {
    voiceIndicator.classList.remove("hidden");
    // 录音开始：波形切回绿色（移除加载态蓝色）
    voiceIndicator.querySelector(".voice-wave")?.classList.remove("voice-loading");
  }
  if (sendBtn) sendBtn.disabled = true;
}

/**
 * 语音录音结束：隐藏指示器 + 恢复发送。
 */
export function hideVoiceIndicator() {
  voiceRecording = false;
  if (voiceIndicator) {
    voiceIndicator.classList.add("hidden");
    // 恢复波形条高度 + 清除加载态
    voiceIndicator.querySelector(".voice-wave")?.classList.remove("voice-loading");
    vwBars.forEach((bar) => (bar.style.height = "4px"));
    const label = voiceIndicator.querySelector(".voice-label");
    if (label) label.textContent = "语音输入中";
  }
  updateSendButtonState();
  if (textarea) {
    autoResize();
    textarea.focus();
  }
}

/**
 * 语音状态提示（模型加载中等，非错误性质）。
 * 对齐主窗口 lifecycle.js 的 voice-status 处理。
 * @param {string} message
 */
export function showVoiceStatus(message) {
  if (!voiceIndicator) return;
  voiceIndicator.classList.remove("hidden");
  const label = voiceIndicator.querySelector(".voice-label");
  if (label) label.textContent = message;
  // 模型加载中：波形转蓝色
  voiceIndicator.querySelector(".voice-wave")?.classList.add("voice-loading");
}

/**
 * 语音音量更新：驱动波形条高度动画。
 * 对齐主窗口 lifecycle.js 的 voice-level 处理。
 * @param {number} level 0.0~1.0
 */
export function updateVoiceLevel(level) {
  if (!voiceIndicator || voiceIndicator.classList.contains("hidden")) {
    voiceIndicator?.classList.remove("hidden");
  }
  const lv = Math.max(0, Math.min(1, level || 0));
  vwBars.forEach((bar, i) => {
    const factor = [0.6, 0.85, 1.0, 0.85, 0.6][i] || 0.7;
    // jitter 独立于 lv：即使安静时也有微妙呼吸感
    const jitter = (Math.sin(Date.now() / 80 + i * 1.3) + 1) * 0.08;
    const h = Math.max(4, (lv * factor + jitter) * 20);
    bar.style.height = h + "px";
  });
}

/**
 * 用语音识别的 partial 文本更新 textarea。
 *
 * 伪流式引擎返回 {confirmed, preview}，纯文本引擎返回 {text}。
 * 最终结果（stop 后）也通过 {text} 传递。
 *
 * @param {{confirmed?: string, preview?: string, text?: string}} data
 */
export function updateVoicePartial(data) {
  if (!textarea) return;
  let partial = "";
  if (data.text != null) {
    partial = data.text;
  } else if (data.confirmed != null || data.preview != null) {
    partial = (data.confirmed || "") + (data.preview || "");
  }
  // base + 识别文本
  const base = voiceBaseText ? voiceBaseText + (voiceBaseText.endsWith("\n") ? "" : " ") : "";
  textarea.value = base + partial;
  autoResize();
}

/**
 * 获取是否正在语音录音。
 * @returns {boolean}
 */
export function isVoiceRecording() {
  return voiceRecording;
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
  if (voiceRecording) {
    sendBtn.disabled = true;
    return;
  }
  const hasText = textarea.value.trim().length > 0;
  sendBtn.disabled = !hasText;
}

// ── Icons ────────────────────────────────────────

const sendIcon = `<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><line x1="22" y1="2" x2="11" y2="13"/><polygon points="22 2 15 22 11 13 2 9 22 2"/></svg>`;

const stopIcon = `<svg viewBox="0 0 24 24" fill="currentColor"><rect x="6" y="6" width="12" height="12" rx="2"/></svg>`;
