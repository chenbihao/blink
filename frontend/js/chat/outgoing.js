/**
 * 外发消息负载组装（0.23.12；0.27.9 类型化附件）。
 *
 * **模型输入与用户可见正文分离**：
 * - `text` —— 用户可见/持久化正文：进用户气泡、state.messages、标题截断
 *   与 LLM 命名输入（`generateConversationTitle`）。纯附件发送时使用
 *   人类可读摘要，绝不出现 `rref_` 或技术提示。
 * - `attachments` —— 附件**结构化元数据** `{kind, ref, display}`（§8.2）：
 *   随本轮请求传给后端，由后端按 kind 解引用注入本轮模型输入（文本注入
 *   正文、图片走 Vision 通道）；不进入气泡/标题/历史。
 *
 * 附件技术块（ref + 使用提示/文件内容）由后端 `ChatService` 组装——前端
 * 不把 ref 拼进消息文本（直接拼进文本会把技术块带进气泡、标题与持久化历史）。
 */

import {t} from "../i18n/index.js";

/** 单轮允许的最大附件数（与后端 `MAX_CHAT_ATTACHMENTS` 对齐）。 */
const MAX_ATTACHMENTS = 8;

/**
 * 纯附件（空正文）时气泡/标题使用的摘要文案（按 kind 分组列举）。
 * 导出供消息渲染方判定"正文是否即附件摘要"——独立附件卡片场景下
 * 该摘要不再渲染为文本气泡，避免文件名显示两遍。
 * @param {Array<{kind?: string, display?: string}|string>} attachments
 * @returns {string}
 */
export function attachmentSummaryText(attachments) {
    const groups = new Map();
    for (const a of Array.isArray(attachments) ? attachments : []) {
        // 兼容纯字符串/旧徽标 {name} 形态（0.23.12 音频时代记录，缺省按 audio）
        const raw = typeof a === "string" ? a : (a?.display ?? a?.name);
        const name = sanitizeName(raw);
        if (!name) continue;
        const kind = (typeof a === "string" ? "audio" : a?.kind) || "audio";
        if (!groups.has(kind)) groups.set(kind, []);
        groups.get(kind).push(name);
    }
    const parts = [];
    for (const [kind, names] of groups) {
        const label = {
            audio: t("chat.attach.summary_audio"),
            text_file: t("chat.attach.summary_text"),
            image: t("chat.attach.summary_image"),
        }[kind] || t("chat.attach.summary_text");
        parts.push(`${label}：${names.join("、")}`);
    }
    if (parts.length === 0) return "";
    return `（${parts.join("；")}）`;
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
 * @param {Array<{kind?: string, ref: string, display?: string}>} attachments composer 当前附件
 * @returns {{text: string, attachments: Array<{kind: string, ref: string, display: string}>}|null}
 */
export function buildOutgoingMessage(text, attachments) {
    const cleanText = String(text ?? "").trim();
    const validAttachments = (Array.isArray(attachments) ? attachments : [])
        .filter((a) => typeof a?.ref === "string" && a.ref.startsWith("rref_"))
        .slice(0, MAX_ATTACHMENTS)
        .map((a) => ({
            kind: a.kind || "text_file",
            ref: a.ref,
            display: sanitizeName(a.display) || a.kind || t("chat.attach.text_fallback"),
        }));

    const displayText = cleanText || attachmentSummaryText(validAttachments);
    if (!displayText) return null;
    return {text: displayText, attachments: validAttachments};
}
