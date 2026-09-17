/**
 * 外发消息负载组装（0.23.14）。
 *
 * **模型输入与用户可见正文分离**：
 * - `text` —— 用户可见/持久化正文：进用户气泡、state.messages、标题截断
 *   与 LLM 命名输入（`generateConversationTitle`）。纯附件发送时使用
 *   人类可读摘要，绝不出现 `rref_` 或技术提示。
 * - `attachments` —— 附件**结构化元数据**：随本轮请求传给后端，由后端
 *   只注入本轮模型输入的附件上下文（含 audio_ref）；不进入气泡/标题/历史。
 *
 * 附件技术块（audio_ref + 使用提示）由后端 `ChatService` 组装——前端不再把
 * ref 拼进消息文本（0.23.13 的做法会把技术块带进气泡、标题与持久化历史）。
 */

/** 单轮允许的最大附件数（与后端 `MAX_CHAT_ATTACHMENTS` 对齐）。 */
const MAX_ATTACHMENTS = 8;

/**
 * 纯附件（空正文）时气泡/标题使用的摘要文案。
 * 导出供消息渲染方判定"正文是否即附件摘要"——独立附件卡片场景下
 * 该摘要不再渲染为文本气泡，避免文件名显示两遍。
 */
export function attachmentSummaryText(attachments) {
    const names = attachments
        .map((a) => sanitizeName(a?.displayName ?? a))
        .filter(Boolean);
    if (names.length === 0) return "";
    return `（音频附件：${names.join("、")}）`;
}

function sanitizeName(name) {
    return String(name ?? "")
        // 控制字符（含换行）不进入可见正文
        .replace(/[\u0000-\u001f\u007f]/g, "")
        .trim()
        .slice(0, 160);
}

/**
 * 组装外发负载；无可发送内容时返回 null。
 *
 * @param {string} text 用户输入的正文（已 trim 或原始文本）
 * @param {Array<{audioRef: string, displayName: string}>} attachments composer 当前附件
 * @returns {{text: string, attachments: Array<{audioRef: string, displayName: string}>}|null}
 */
export function buildOutgoingMessage(text, attachments) {
    const cleanText = String(text ?? "").trim();
    const validAttachments = (Array.isArray(attachments) ? attachments : [])
        .filter((a) => typeof a?.audioRef === "string" && a.audioRef.startsWith("rref_"))
        .slice(0, MAX_ATTACHMENTS)
        .map((a) => ({
            audioRef: a.audioRef,
            displayName: sanitizeName(a.displayName) || "WAV 文件",
        }));

    const displayText = cleanText || attachmentSummaryText(validAttachments);
    if (!displayText) return null;
    return {text: displayText, attachments: validAttachments};
}
