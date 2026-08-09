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

/** @type {HTMLElement} /skill 命令提示弹层 */
let skillHintEl = null;

/** @type {Array} 缓存的 skill 列表（避免每次输入都请求） */
let cachedSkills = null;

/** @type {number} skill 缓存过期时间戳 */
let skillCacheExpiry = 0;

/** @type {boolean|null} 当前对话模式是否允许 Skill；null 表示尚未读取配置 */
let skillHintsEnabled = null;

/** 异步提示请求代次；输入或配置变化后，旧请求不得回写 DOM。 */
let skillHintRevision = 0;

/** AI 配置变化时由 chat 入口调用，避免纯对话切换后继续使用旧 Skill 缓存。 */
export function invalidateSkillCache() {
  cachedSkills = null;
  skillCacheExpiry = 0;
  skillHintsEnabled = null;
  skillHintRevision += 1;
  if (skillHintEl) skillHintEl.hidden = true;
}

/** 只向对话输入提示实际可激活的 Skill；设置页仍可展示 disabled 条目。 */
export function filterActiveSkills(skills, query = "") {
  const normalizedQuery = query.trim().toLowerCase();
  return (Array.isArray(skills) ? skills : []).filter((skill) =>
    !skill.disabled &&
    (!normalizedQuery || String(skill.name || "").toLowerCase().includes(normalizedQuery))
  );
}

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

  // textarea 自增高 + 更新发送按钮状态 + /skill 检测
  textarea.addEventListener("input", () => {
    autoResize();
    updateSendButtonState();
    checkSkillHint();
  });

  // Enter 发送 / Shift+Enter 换行 / Escape 关闭 skill hint
  // 流式模式（stop 按钮）下 Enter 不发送——避免用户预输入时误触发
  textarea.addEventListener("keydown", (e) => {
    if (e.key === "Enter" && !e.shiftKey) {
      if (sendBtn.classList.contains("chat-stop-btn")) return;
      e.preventDefault();
      handleSend();
    } else if (e.key === "Escape" && skillHintEl && !skillHintEl.hidden) {
      skillHintEl.hidden = true;
      e.stopPropagation();
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

  // 0.13.3: /skill 命令提示初始化
  skillHintEl = document.getElementById("skill-hint");

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
 * 设置 composer 为流式模式（显示停止按钮，保留输入能力）。
 * 不再禁用 textarea——用户可在 AI 生成期间预先输入下一条消息，
 * Enter 在流式模式下不会发送（仅 Shift+Enter 换行），需点击停止按钮后再 Enter 发送。
 */
export function setStreamingMode() {
  if (!sendBtn || !textarea) return;
  sendBtn.classList.add("chat-stop-btn");
  sendBtn.innerHTML = stopIcon;
  sendBtn.disabled = false;
  // 不禁用 textarea——用户可继续输入
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
 * 设置输入框文本（0.16.2：chord Alt+Q 带文本触发时填充用）。
 * 仅填充，不自动发送。光标移到末尾，触发 autoResize + sendButton 状态更新。
 * @param {string} text
 */
export function setInputValue(text) {
  if (!textarea) return;
  textarea.value = text;
  // 光标移到末尾
  textarea.setSelectionRange(text.length, text.length);
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
    // 录音开始：波形切回绿色（移除加载态蓝色 + 错误态红色）
    voiceIndicator.querySelector(".voice-wave")?.classList.remove("voice-loading", "voice-error");
  }
  if (sendBtn) sendBtn.disabled = true;
  // 0.12.4 §6.6：录音期间 textarea 设为 readOnly，防止 Space 穿透
  if (textarea) textarea.readOnly = true;
}

/**
 * 语音录音结束：隐藏指示器 + 恢复发送。
 */
export function hideVoiceIndicator() {
  voiceRecording = false;
  if (voiceIndicator) {
    voiceIndicator.classList.add("hidden");
    // 恢复波形条高度 + 清除加载态/错误态
    voiceIndicator.querySelector(".voice-wave")?.classList.remove("voice-loading", "voice-error");
    vwBars.forEach((bar) => (bar.style.height = "4px"));
    const label = voiceIndicator.querySelector(".voice-label");
    if (label) label.textContent = "语音输入中";
  }
  // 0.12.4 §6.6：录音结束恢复 textarea
  if (textarea) textarea.readOnly = false;
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
  // 模型加载中：波形转蓝色（清除可能残留的错误态红色）
  voiceIndicator.querySelector(".voice-wave")?.classList.remove("voice-error");
  voiceIndicator.querySelector(".voice-wave")?.classList.add("voice-loading");
}

/**
 * 语音错误提示（STT 未配置 / 服务未启动等）。
 * 对齐主窗口 lifecycle.js 的 voice-error 处理。
 * 设计铁则：所有语音状态统一在波形动画区域展示——
 * 绿色=录音中 · 蓝色=加载中 · 红色=错误。
 * @param {string} message
 */
export function showVoiceError(message) {
  if (!voiceIndicator) return;
  voiceIndicator.classList.remove("hidden");
  const label = voiceIndicator.querySelector(".voice-label");
  if (label) label.textContent = message;
  // 错误：波形转红色（清除可能残留的加载态蓝色）
  voiceIndicator.querySelector(".voice-wave")?.classList.remove("voice-loading");
  voiceIndicator.querySelector(".voice-wave")?.classList.add("voice-error");
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
  // 流式模式（停止按钮）或语音录音期间不受输入框文本影响
  if (sendBtn.classList.contains("chat-stop-btn")) return;
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

// ── 0.13.3 /skill 命令提示 ────────────────────────────────────────────────────

/**
 * 检测 textarea 内容是否以 /skill 开头，若是则显示可用技能列表。
 *
 * 提示弹层出现在 textarea 上方，列出匹配的 skill 名称 + 来源 + 描述。
 * 点击 skill 条目自动填充 `/skill <name> ` 到 textarea 并隐藏提示。
 * 按 Escape 隐藏提示。
 */
async function checkSkillHint() {
  if (!skillHintEl || !textarea) return;

  const revision = ++skillHintRevision;

  const value = textarea.value.trim();
  if (!value.toLowerCase().startsWith("/skill")) {
    skillHintEl.hidden = true;
    return;
  }

  // 加载 skill 列表（缓存 30s）
  const now = Date.now();
  if (!cachedSkills || now > skillCacheExpiry) {
    try {
      const { invoke } = await import("../shared/tauri.js");
      const aiConfig = await invoke("get_config_section", { key: "app.ai" });
      if (revision !== skillHintRevision) return;
      const agentMode = aiConfig?.chat_config?.agent_mode
        || (aiConfig?.chat_config?.pure_chat ? "pure_chat" : "full");
      skillHintsEnabled = agentMode !== "pure_chat";
      if (!skillHintsEnabled) {
        cachedSkills = [];
        skillCacheExpiry = now + 30000;
        skillHintEl.hidden = true;
        return;
      }
      cachedSkills = await invoke("list_skills");
      if (revision !== skillHintRevision) return;
      skillCacheExpiry = now + 30000; // 30s 缓存
    } catch (e) {
      console.error("[skill-hint] list_skills failed:", e);
      skillHintEl.hidden = true;
      return;
    }
  }

  if (!skillHintsEnabled || revision !== skillHintRevision) {
    skillHintEl.hidden = true;
    return;
  }

  // 等待 IPC 期间用户可能已经删除/替换了命令，旧结果不能重新打开提示。
  if (!textarea.value.trim().toLowerCase().startsWith("/skill")) return;

  const activeSkills = filterActiveSkills(cachedSkills);
  if (activeSkills.length === 0) {
    skillHintEl.innerHTML = `<div class="skill-hint-empty">未发现 Skill。请在设置页 AI tab 配置来源目录并刷新。</div>`;
    skillHintEl.hidden = false;
    return;
  }

  // 提取 /skill 后面的输入（用于过滤）
  const query = value.slice(6).trim().toLowerCase(); // 去掉 "/skill"
  const filtered = filterActiveSkills(activeSkills, query);

  if (filtered.length === 0) {
    skillHintEl.innerHTML = `<div class="skill-hint-empty">未匹配到 "${escapeHtml(query)}" 的 Skill</div>`;
    skillHintEl.hidden = false;
    return;
  }

  // 渲染提示列表
  skillHintEl.innerHTML = filtered
    .slice(0, 8) // 最多 8 条
    .map((s) => {
      const sourceLabel = { blink: "Blink", claude: "Claude", zcode: "ZCode" }[s.source] || s.source;
      return `<div class="skill-hint-item" data-skill-name="${escapeAttr(s.name)}">
        <span class="skill-hint-name">${escapeHtml(s.name)}</span>
        <span class="skill-hint-source">${sourceLabel}</span>
        <span class="skill-hint-desc">${escapeHtml(s.description)}</span>
      </div>`;
    })
    .join("");

  skillHintEl.hidden = false;

  // 绑定点击事件
  skillHintEl.querySelectorAll(".skill-hint-item").forEach((item) => {
    item.addEventListener("click", () => {
      const name = item.dataset.skillName;
      textarea.value = `/skill ${name} `;
      textarea.focus();
      // 光标移到末尾
      const len = textarea.value.length;
      textarea.setSelectionRange(len, len);
      skillHintEl.hidden = true;
      autoResize();
      updateSendButtonState();
    });
  });
}

function escapeHtml(s) {
  return String(s ?? "")
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;")
    .replace(/'/g, "&#39;");
}

function escapeAttr(s) {
  return escapeHtml(s);
}
