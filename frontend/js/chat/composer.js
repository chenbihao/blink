/**
 * chat 输入组件（0.12.1 Phase 5, 0.12.2 深度思考开关, 0.12.3 语音指示器对齐主窗口）。
 *
 * 自增高 textarea + 底部工具栏（深度思考开关 + 发送/停止按钮）。
 * Enter 发送，Shift+Enter 换行。生成中按钮切换为停止。
 *
 * 0.27.7 斜杠命令（phase §九）：
 * - composer 维护单调递增 inputRevision（用户输入 / 面板开闭 / 附件增删时 +1），
 *   命令面板等弹层的异步结果回写前经 main.js 注入的 getter 核对，过期即丢弃
 * - IME 组合（compositionstart~end）期间不处理面板快捷采纳
 * - 旧 /skill 专属提示弹层（0.13.3 skill-hint）由 command-palette.js 统一接管
 *
 * 0.23.12 音频附件：
 * - 当前会话 id 经 initComposer({getConversationId}) 注入，不依赖未导入的全局绑定
 * - picker 异步返回后校验会话一致，切换/新建会话则撤销刚签发的 ref 并丢弃
 * - 发送负载 = {text（用户可见正文）, attachments（结构化元数据）}；
 *   附件 ref 只注入本轮模型输入（后端组装），不进入气泡/标题/持久化历史
 */

import * as ipc from "./ipc.js";
import {iconHTML} from "../shared/icon.js";
import {getCurrentWebview} from "../shared/tauri.js";
import {buildOutgoingMessage} from "./outgoing.js";
import {t} from "../i18n/index.js";
import {closePalette, handlePaletteKeydown, onComposerInput} from "./command-palette.js";
import {
    ATTACHMENT_ICONS,
    closeAttachmentPalette,
    handleAttachmentPaletteKeydown,
    onComposerInputForAttachments,
    openAttachmentPalette,
} from "./attachment-palette.js";

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

/** @type {(() => string)|null} 获取当前会话 id（main.js 注入，0.23.12） */
let getConversationId = null;

/** @type {boolean} 是否正在语音录音 */
let voiceRecording = false;

/** @type {string} 录音开始前的 textarea 文本（用作 base，识别结果追加其后） */
let voiceBaseText = "";

/** @type {HTMLElement} 附件 chips 容器（0.23.12；0.27.9 泛化） */
let attachmentsEl = null;

/** @type {HTMLButtonElement|null} 附件按钮（0.27.9：打开 @ 附件面板） */
let attachBtn = null;

/**
 * 当前附件（0.27.9 类型化：audio/text_file/image）。
 * @type {Array<{kind: string, ref: string, display: string, truncated?: boolean, tokenEst?: number|null}>}
 */
let attachments = [];

/** 单轮附件上限（与后端 MAX_CHAT_ATTACHMENTS 对齐）。 */
const MAX_ATTACHMENTS = 8;

/** 超限未附加提示（一次性；点击提示 chip 或下次批量添加时清除）。 */
let attachmentOverflowNotice = null;

/**
 * 0.27.7 composer 输入代次（§九）：用户输入、面板开/关、附件增删时 +1。
 * 弹层异步结果回写前核对，不匹配即丢弃（沿用 skillHintRevision 先例）。
 */
let inputRevision = 0;

/** 当前输入代次（command-palette 经 main.js 注入此 getter 做过期校验）。 */
export function composerInputRevision() {
    return inputRevision;
}

/** 推进输入代次（弹层数据捕获点之前的输入变化都会使旧结果失效）。 */
function bumpInputRevision() {
    inputRevision += 1;
}

/** IME 组合中标志——组合期间不处理面板快捷采纳（§8.3/§九）。 */
let composing = false;

/**
 * 初始化 composer。
 * @param {{ onSend: (payload: {text: string, attachments: Array}|null) => void,
 *           onStop: () => void,
 *           getConversationId?: () => string }} callbacks
 */
export function initComposer(callbacks) {
    textarea = document.getElementById("chat-input");
    sendBtn = document.getElementById("chat-send-btn");
    thinkingBtn = document.getElementById("chat-thinking-btn");
    voiceIndicator = document.getElementById("voice-indicator");
    vwBars = voiceIndicator ? voiceIndicator.querySelectorAll(".vw-bar") : [];
    onSend = callbacks.onSend;
    onStop = callbacks.onStop;
    getConversationId = callbacks.getConversationId ?? null;

    if (!textarea || !sendBtn) return;

    // textarea 自增高 + 更新发送按钮状态 + 0.27.7 斜杠命令面板 / 0.27.9 @ 附件面板触发
    // IME 组合中不触发面板（§8.3 组合输入不执行），compositionend 按终稿刷新
    textarea.addEventListener("input", () => {
        autoResize();
        updateSendButtonState();
        bumpInputRevision();
        if (!composing) {
            onComposerInput(textarea.value);
            onComposerInputForAttachments(textarea.value);
        }
    });

    // IME 组合 guard（§九）：组合期间不处理快捷采纳；结束后按终稿刷新面板
    textarea.addEventListener("compositionstart", () => {
        composing = true;
    });
    textarea.addEventListener("compositionend", () => {
        composing = false;
        bumpInputRevision();
        onComposerInput(textarea.value);
        onComposerInputForAttachments(textarea.value);
    });

    // 面板快捷键优先（命令面板 / 附件面板，IME 组合中跳过）；
    // Enter 发送 / Shift+Enter 换行——流式模式（stop 按钮）下 Enter 不发送，
    // 避免用户预输入时误触发
    textarea.addEventListener("keydown", (e) => {
        if (!composing && handlePaletteKeydown(e)) return;
        if (!composing && handleAttachmentPaletteKeydown(e)) return;
        if (e.key === "Enter" && !e.shiftKey) {
            if (sendBtn.classList.contains("chat-stop-btn")) return;
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

    // 0.27.9: 附件按钮打开 @ 附件面板（与 @ 触发共用，phase §5.9.4）；
    // stopPropagation 阻止冒泡到 document 的 click-outside 关闭逻辑（面板开即关）
    attachBtn = document.getElementById("chat-attach-btn");
    attachmentsEl = document.getElementById("chat-audio-attachments");
    if (attachBtn)
        attachBtn.addEventListener("click", (e) => {
            e.stopPropagation();
            openAttachmentPalette();
        });
    renderAttachments();

    // 0.27.10: 粘贴图片——剪贴板有文本时走默认文本粘贴（不覆盖已有文字）；
    // 无文本时读 OS 剪贴板图片签发为会话内 VisionImage 附件
    textarea.addEventListener("paste", (e) => {
        handlePaste(e);
    });

    // 0.27.10: 拖放文件——Tauri 接管 OS 拖放（DOM drop 不触发），经
    // onDragDropEvent 取路径，后端按内容嗅探分类签发（图片/文本）
    try {
        const webview = getCurrentWebview();
        if (webview?.onDragDropEvent) {
            webview.onDragDropEvent((event) => {
                const payload = event?.payload;
                if (payload?.type !== "drop") return;
                handleDroppedPaths(payload.paths || []);
            });
        }
    } catch (error) {
        console.warn("[composer] 拖放事件注册失败:", error);
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
    bumpInputRevision();
    closePalette();
    closeAttachmentPalette();
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
    bumpInputRevision();
    onComposerInput(text);
}

/**
 * 聚焦输入框。
 */
export function focusInput() {
    if (textarea) textarea.focus();
}

/**
 * 0.27.8: 在光标/选区处插入文本（/prompt 提示词插入用，phase §5.8.5）。
 *
 * 保留选区外其他草稿与附件、不自动发送；插入后光标移到插入文本末尾并显式
 * 滚动跟随（spec-frontend §5.8 程序化写入铁则）。无焦点时退化为追加到末尾。
 * @param {string} text
 */
export function insertAtCursor(text) {
    if (!textarea || !text) return;
    const start = textarea.selectionStart ?? textarea.value.length;
    const end = textarea.selectionEnd ?? start;
    textarea.value = textarea.value.slice(0, start) + text + textarea.value.slice(end);
    const pos = start + text.length;
    textarea.setSelectionRange(pos, pos);
    autoResize();
    updateSendButtonState();
    bumpInputRevision();
    closePalette();
    closeAttachmentPalette();
    textarea.focus();
    // 程序化写入不假设浏览器跟随光标——显式滚到光标处
    requestAnimationFrame(() => {
        textarea.scrollTop = textarea.scrollHeight;
    });
}

/**
 * 设置思考控件状态（0.21.17）。
 *
 * 支持 reasoning_effort 的模型显示「思考 · 档位」标签（label 由 main.js 计算）；
 * 不支持等级的模型退化为简单开关——`enabled` 控制 `.active` 高亮。
 * @param {{enabled: boolean, label?: string}} state
 */
export function setThinkingState({enabled, label}) {
    if (!thinkingBtn) return;
    thinkingBtn.classList.toggle("active", enabled);
    if (label != null) {
        const labelEl = document.getElementById("chat-thinking-label");
        if (labelEl) labelEl.textContent = label;
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
    bumpInputRevision();
}

/**
 * 获取是否正在语音录音。
 * @returns {boolean}
 */
export function isVoiceRecording() {
    return voiceRecording;
}

// ── 附件（0.23.12 音频闭环；0.27.9 泛化为 audio/text_file/image）──────────

/** 当前附件快照（只读副本）。 */
export function getAttachments() {
    return [...attachments];
}

/** 是否有附件。 */
export function hasAttachments() {
    return attachments.length > 0;
}

/**
 * 批量添加附件（@ 面板采纳 / open_chat 草稿事务预填入口）。
 *
 * 按 ref 去重（同一 ref 重复附加不堆叠）；超出单轮上限的项不静默丢弃——
 * chip 区显示可关闭的超限提示，并经返回值供调用方感知。
 * 会话一致性由调用方保证（面板/预填流在会话切换时撤销或转移）。
 * @param {Array<{kind: string, ref: string, display: string, truncated?: boolean, tokenEst?: number|null}>} list
 * @returns {{rejected: Array<object>}} 因超限未添加的项
 */
export function addAttachments(list) {
    const incoming = (Array.isArray(list) ? list : [])
        .filter((a) => a && typeof a.ref === "string" && a.ref.startsWith("rref_"));
    const rejected = [];
    for (const item of incoming) {
        if (attachments.length >= MAX_ATTACHMENTS) {
            rejected.push(item);
            continue;
        }
        if (attachments.some((existing) => existing.ref === item.ref)) continue;
        attachments.push({
            kind: item.kind || "text_file",
            ref: item.ref,
            display: (item.display || "").trim() || item.kind || "附件",
            truncated: item.truncated || false,
            tokenEst: item.tokenEst ?? null,
        });
    }
    attachmentOverflowNotice = rejected.length
        ? t("chat.attach.overflow", {count: rejected.length, max: MAX_ATTACHMENTS})
        : null;
    renderAttachments();
    updateSendButtonState();
    return {rejected};
}

/**
 * 仅清空 UI 附件状态（不触发后端 revoke——撤销由调用方按 owner 批量处理）。
 * 会话切换/新对话时由 main.js 调用。
 */
export function clearAttachments() {
    attachments = [];
    attachmentOverflowNotice = null;
    renderAttachments();
    updateSendButtonState();
}

/**
 * 消息发出后把**本次已发送**的附件从输入框移除（0.23.12）。
 *
 * 不撤销后端 ref——本轮 agent 仍要消费它（转写/续读/编码）；生命周期由会话
 * 切换/新对话的按 owner 批量撤销统一回收。只过滤匹配的 ref，发送等待期间
 * 用户新附的文件不受影响；ref 由 CSPRNG 签发不会撞号。
 * @param {Array<{ref: string}>} sent 随消息发出的附件
 */
export function clearSentAttachments(sent) {
    const sentRefs = new Set((Array.isArray(sent) ? sent : []).map((a) => a?.ref).filter(Boolean));
    if (sentRefs.size === 0) return;
    attachments = attachments.filter((a) => !sentRefs.has(a.ref));
    renderAttachments();
    updateSendButtonState();
}

/** 0.27.10：粘贴——有文本走默认；无文本尝试剪贴板图片附件（会话守卫）。 */
function handlePaste(e) {
    const dt = e.clipboardData;
    const hasText = dt && (dt.getData("text/plain") || dt.getData("text") || "");
    if (hasText) return; // 文本粘贴：默认行为（图片与文本并存不丢文字）
    e.preventDefault();
    const requested = getConversationId?.() ?? null;
    ipc.attachmentAttachClipboardImage(requested).then((signed) => {
        if (getConversationId?.() !== requested) return; // 会话已切换：丢弃
        if (signed?.ref) {
            addAttachments([{
                kind: "image",
                ref: signed.ref,
                display: signed.display || t("chat.attach.clipboard_image"),
            }]);
        }
    }).catch(() => {
        // 剪贴板无图片——无文本无图粘贴为 no-op
    });
}

/** 0.27.10：拖放文件路径 → 后端嗅探分类签发（逐项反馈）。 */
function handleDroppedPaths(paths) {
    if (!Array.isArray(paths) || paths.length === 0) return;
    const requested = getConversationId?.() ?? null;
    if (!requested) return;
    ipc.attachmentSignDroppedFiles(paths, requested).then((outcomes) => {
        if (getConversationId?.() !== requested) return;
        const okList = (Array.isArray(outcomes) ? outcomes : [])
            .filter((o) => o?.ok?.attachment?.ref || o?.ok?.ref)
            .map((o) => {
                const a = o.ok.attachment || o.ok;
                return {kind: a.kind, ref: a.ref, display: a.display};
            });
        if (okList.length > 0) addAttachments(okList);
    }).catch((error) => {
        console.warn("[composer] 拖放签发失败:", error);
    });
}

const CHIP_KIND_HINTS = {
    audio: "chat.attach.chip_hint_audio",
    text_file: "chat.attach.chip_hint_text",
    image: "chat.attach.chip_hint_image",
};

function renderAttachments() {
    if (!attachmentsEl) return;
    bumpInputRevision(); // 附件增删推进输入代次（§九：弹层异步结果防过期）
    attachmentsEl.innerHTML = "";
    for (const attachment of attachments) {
        const chip = document.createElement("span");
        chip.className = "chat-audio-chip";
        const hint = t(CHIP_KIND_HINTS[attachment.kind] || "chat.attach.chip_hint_text");
        const truncatedNote = attachment.truncated ? " · " + t("chat.attach.truncated") : "";
        chip.title = `${attachment.display}${truncatedNote}（${hint}）`;

        const icon = document.createElement("span");
        icon.className = "chat-audio-chip-icon";
        icon.innerHTML = iconHTML(ATTACHMENT_ICONS[attachment.kind] || "file-text");

        const name = document.createElement("span");
        name.className = "chat-audio-chip-name";
        name.textContent = attachment.display;

        const remove = document.createElement("button");
        remove.type = "button";
        remove.className = "chat-audio-chip-remove";
        remove.title = t("chat.attach.remove");
        remove.setAttribute("aria-label", t("chat.attach.remove"));
        remove.innerHTML = iconHTML("x");
        remove.addEventListener("click", () => removeAttachment(attachment.ref));

        chip.append(icon, name, remove);
        attachmentsEl.appendChild(chip);
    }
    if (attachmentOverflowNotice) {
        const notice = document.createElement("button");
        notice.type = "button";
        notice.className = "chat-audio-chip chat-audio-chip-notice";
        notice.title = attachmentOverflowNotice;
        notice.textContent = attachmentOverflowNotice;
        notice.addEventListener("click", () => {
            attachmentOverflowNotice = null;
            renderAttachments();
        });
        attachmentsEl.appendChild(notice);
    }
    attachmentsEl.classList.toggle("hidden", attachments.length === 0 && !attachmentOverflowNotice);
}

async function removeAttachment(ref) {
    attachments = attachments.filter((item) => item.ref !== ref);
    renderAttachments();
    updateSendButtonState();
    try {
        await ipc.removeChatAttachment(ref);
    } catch (error) {
        console.warn("chat attachment revoke failed:", error);
    }
}

// ── 内部 ─────────────────────────────────────────

function handleSend() {
    const text = textarea?.value?.trim() ?? "";
    // 0.27.9：负载 = {text（可见正文）, attachments（{kind, ref, display} 结构化元数据）}；
    // 纯附件（空正文）时 text 为人类可读摘要，气泡/标题/历史保持无 rref_
    const payload = buildOutgoingMessage(text, attachments);
    if (!payload) return;
    if (onSend) onSend(payload);
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
    // 0.23.12：附件存在时也允许发送
    const hasContent = textarea.value.trim().length > 0 || attachments.length > 0;
    sendBtn.disabled = !hasContent;
}

// ── Icons ────────────────────────────────────────

const sendIcon = `<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><line x1="22" y1="2" x2="11" y2="13"/><polygon points="22 2 15 22 11 13 2 9 22 2"/></svg>`;

const stopIcon = `<svg viewBox="0 0 24 24" fill="currentColor"><rect x="6" y="6" width="12" height="12" rx="2"/></svg>`;

