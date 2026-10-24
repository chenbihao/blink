/**
 * @ 附件面板（0.27.9，phase §5.9.4 / §8.2 / §九）。
 *
 * 结构：首项「选择文件…」（可信 picker）、「选择音频…」（0.23.12 音频通道）；
 * 随后「当前会话引用」（ResourceStore list 投影，过滤过期/跨会话在后端完成）
 * 与「最近复制的文件」（query_recent_file_list，逐项预检——非 UTF-8/超限/
 * 目录仅展示不可附加原因；附加经 attachment_attach_clipboard_files 由后端
 * 从 DB 读取路径签发，前端不传路径）。
 *
 * 触发契约：仅 composer 首个非空白字符为 `@` 且整段输入单行时触发；
 * Tab/Enter 采纳、Esc 只关面板不清输入、Shift+Enter 恒换行、IME 组合中
 * 不处理快捷采纳（composer 侧 guard，§九）。
 *
 * 互斥：经 composer-popups 仲裁（id "attach"），打开前关闭命令面板/模型
 * 下拉等其余弹层；异步结果回写前核对 composer inputRevision。
 */

import * as ipc from "./ipc.js";
import {iconHTML} from "../shared/icon.js";
import {escapeAttr, escapeText} from "./utils.js";
import {openExclusive, registerPopup} from "./composer-popups.js";
import {t} from "../i18n/index.js";

/** 附件种类 → chip/卡片图标（Lucide sprite 名）。 */
export const ATTACHMENT_ICONS = {
    audio: "audio-lines",
    text_file: "file-text",
    image: "image",
};

/** @type {HTMLElement} 面板容器（chat.html 静态节点） */
let paletteEl = null;

/** @type {{query: string}|null} 当前触发输入（@ 后的过滤词） */
let activeQuery = null;

/** @type {Array<object>} 当前可采纳行（kind: pick_file/pick_audio/session_ref/recent_files） */
let items = [];

/** @type {number} 高亮行索引 */
let highlightIndex = 0;

/** composer 输入代次（异步回写防过期，main.js 注入 getter） */
let getInputRevision = () => 0;

/** 当前会话 id getter（签发需绑定会话 owner） */
let getConversationId = () => "";

/** 附件添加回调（main.js 注入 composer.addAttachments） */
let addAttachments = () => {};

/** 面板打开的显式入口（attach 按钮，非 @ 触发） */
let manuallyOpened = false;

// ── 纯函数（供 attachment.test.mjs） ────────────────────────────────────────

/**
 * 解析 composer 输入是否构成 @ 面板触发。
 *
 * 仅当整段输入单行、首个非空白字符为 `@` 时触发；@ 前缀不要求紧跟词。
 * @param {string} text
 * @returns {{query: string}|null}
 */
export function splitAtQuery(text) {
    if (typeof text !== "string" || text.includes("\n")) return null;
    const match = text.match(/^[ \t]*@(.*)$/);
    if (!match) return null;
    return {query: (match[1] || "").trim()};
}

/**
 * 候选列表过滤：query 命中任一文件名/标签（大小写不敏感子串）。
 * @param {Array<{label: string, names?: string[]}>} rows 面板行（含展示标签）
 * @param {string} query
 * @returns {Array<object>}
 */
export function filterCandidateRows(rows, query) {
    const q = (query || "").toLowerCase();
    if (!q) return [...rows];
    return rows.filter((row) => {
        const haystack = [row.label, ...(row.names || [])]
            .filter(Boolean)
            .join("\n")
            .toLowerCase();
        return haystack.includes(q);
    });
}

/** 最近文件条目是否可整体附加（至少一个文件 attachable）。 */
export function entryAttachable(entry) {
    return (entry?.files || []).some((file) => file?.attachable);
}

/** 附件显示名兜底（label 缺失时按 kind 给通用名）。 */
export function attachmentDisplayName(item, kind) {
    const label = item?.label || item?.display || item?.name || "";
    if (label) return label;
    return {
        audio: t("chat.attach.audio_fallback"),
        text_file: t("chat.attach.text_fallback"),
        image: t("chat.attach.image_fallback"),
    }[kind] || t("chat.attach.text_fallback");
}

// ── 初始化与生命周期 ────────────────────────────────────────────────────────

/**
 * 初始化附件面板。
 * @param {{getConversationId: () => string, getInputRevision?: () => number,
 *          addAttachments?: (list: Array<{kind: string, ref: string, display: string, truncated?: boolean}>) => void}} callbacks
 */
export function initAttachmentPalette(callbacks) {
    paletteEl = document.getElementById("chat-attachment-palette");
    if (!paletteEl) return;
    if (typeof callbacks?.getConversationId === "function") {
        getConversationId = callbacks.getConversationId;
    }
    if (typeof callbacks?.getInputRevision === "function") {
        getInputRevision = callbacks.getInputRevision;
    }
    if (typeof callbacks?.addAttachments === "function") {
        addAttachments = callbacks.addAttachments;
    }
    registerPopup("attach", closeAttachmentPalette);

    // 附件按钮 title 走 i18n（chat.html 不再硬编码文案）
    const attachBtn = document.getElementById("chat-attach-btn");
    if (attachBtn) attachBtn.title = t("chat.attach.button_title");

    paletteEl.addEventListener("mousedown", (e) => {
        if (!e.target.closest("button, input")) e.preventDefault();
    });
    paletteEl.addEventListener("click", (e) => {
        const row = e.target.closest("[data-adopt]");
        if (row) {
            adoptRow(String(row.dataset.adopt), row.dataset.value || "");
        }
    });
    document.addEventListener("click", (e) => {
        if (!isAttachmentPaletteOpen()) return;
        if (paletteEl.contains(e.target)) return;
        const input = document.getElementById("chat-input");
        if (input && input.contains(e.target)) return;
        closeAttachmentPalette();
    });
}

/** attach 按钮入口：不经 @ 触发直接打开（显示全部候选）。 */
export function openAttachmentPalette() {
    if (!paletteEl) return;
    manuallyOpened = true;
    activeQuery = {query: ""};
    renderCandidates();
}

/** 面板是否打开。 */
export function isAttachmentPaletteOpen() {
    return activeQuery !== null && paletteEl !== null && !paletteEl.hidden;
}

/** 关闭面板（不清输入）。 */
export function closeAttachmentPalette() {
    activeQuery = null;
    manuallyOpened = false;
    items = [];
    if (paletteEl) paletteEl.hidden = true;
}

/**
 * composer input 事件入口。
 * @param {string} text textarea 当前值
 */
export function onComposerInputForAttachments(text) {
    const parsed = splitAtQuery(text);
    if (!parsed) {
        if (!manuallyOpened) closeAttachmentPalette();
        return;
    }
    if (manuallyOpened) return; // 按钮打开的面板不受 @ 输入驱动
    activeQuery = parsed;
    renderCandidates();
}

/**
 * composer keydown 路由（面板打开时由 composer 调用）。
 * @param {KeyboardEvent} e
 * @returns {boolean} true = 已处理
 */
export function handleAttachmentPaletteKeydown(e) {
    if (!isAttachmentPaletteOpen()) return false;
    switch (e.key) {
        case "ArrowUp":
            e.preventDefault();
            moveHighlight(-1);
            return true;
        case "ArrowDown":
            e.preventDefault();
            moveHighlight(1);
            return true;
        case "Tab":
        case "Enter":
            if (e.shiftKey) return false;
            e.preventDefault();
            adoptHighlighted();
            return true;
        case "Escape":
            e.preventDefault();
            e.stopPropagation();
            closeAttachmentPalette();
            return true;
        default:
            return false;
    }
}

// ── 渲染 ────────────────────────────────────────────────────────────────────

async function renderCandidates() {
    const revision = getInputRevision();
    openExclusive("attach");
    paletteEl.innerHTML = `<div class="cp-loading">${escapeText(t("chat.attach.loading"))}</div>`;
    paletteEl.hidden = false;

    const conversationId = getConversationId();
    let candidates = null;
    try {
        candidates = await ipc.attachmentListCandidates(conversationId);
    } catch (error) {
        console.warn("[attachment-palette] 候选加载失败:", error);
    }
    if (revision !== getInputRevision() || activeQuery === null) return;

    const query = activeQuery.query;
    const rows = [];
    rows.push({kind: "pick_file", label: t("chat.attach.pick_file"), icon: "folder-open"});
    rows.push({kind: "pick_audio", label: t("chat.attach.pick_audio"), icon: "audio-lines"});
    for (const ref of candidates?.sessionRefs || []) {
        rows.push({
            kind: "session_ref",
            value: JSON.stringify(ref),
            label: attachmentDisplayName(ref, ref.kind),
            names: [ref.kind],
            icon: ATTACHMENT_ICONS[ref.kind] || "file-text",
            meta: t("chat.attach.session_ref"),
        });
    }
    for (const image of candidates?.recentImages || []) {
        rows.push({
            kind: "recent_image",
            value: image.id,
            label: t("chat.attach.recent_image", {
                w: image.width || "?",
                h: image.height || "?",
            }),
            names: [image.id],
            icon: "image",
            meta: "",
        });
    }
    for (const entry of candidates?.recentFiles || []) {
        const attachable = entryAttachable(entry);
        rows.push({
            kind: "recent_files",
            value: entry.id,
            label: entry.files?.length === 1
                ? (entry.files[0]?.name || "")
                : t("chat.attach.file_group", {n: entry.files?.length ?? 0}),
            names: (entry.files || []).map((f) => f.name),
            icon: "clipboard",
            meta: attachable ? "" : t("chat.attach.none_attachable"),
            disabled: !attachable,
        });
    }

    const matched = filterCandidateRows(rows, query).filter((row) => !row.disabled);
    if (matched.length === 0) {
        items = [];
        paletteEl.innerHTML = `<div class="cp-empty">${escapeText(t("chat.attach.empty"))}</div>`;
        return;
    }

    items = matched;
    paletteEl.innerHTML =
        `<div class="cp-section-title">${escapeText(t("chat.attach.title"))}</div>` +
        matched
            .map((row) => `<div class="cp-row" data-adopt="${escapeAttr(row.kind)}" data-value="${escapeAttr(row.value || "")}">
                <span class="cp-row-icon">${iconHTML(row.icon)}</span>
                <span class="cp-row-title">${escapeText(row.label)}</span>
                ${row.meta ? `<span class="cp-row-desc">${escapeText(row.meta)}</span>` : ""}
            </div>`)
            .join("");
    paletteEl.hidden = false;
    applyHighlight();
}

function applyHighlight() {
    const rows = paletteEl.querySelectorAll(".cp-row[data-adopt]");
    rows.forEach((row, index) => {
        row.classList.toggle("cp-row-highlight", index === highlightIndex);
    });
    const highlighted = rows[highlightIndex];
    if (highlighted) highlighted.scrollIntoView({block: "nearest"});
}

function moveHighlight(delta) {
    const count = items.length;
    if (count === 0) return;
    highlightIndex = (highlightIndex + delta + count) % count;
    applyHighlight();
}

// ── 采纳 ────────────────────────────────────────────────────────────────────

function adoptHighlighted() {
    if (items.length === 0) return;
    const item = items[Math.min(highlightIndex, items.length - 1)];
    if (item?.kind) adoptRow(item.kind, item.value || "");
}

/** 采纳：picker / 会话引用 / 最近文件条目。失败结果逐项反馈，不静默丢项。 */
async function adoptRow(kind, value) {
    if (kind === "pick_file") {
        closeAttachmentPalette();
        const outcomes = await runPick(() => ipc.attachmentPickFiles(getConversationId()));
        applyOutcomes(outcomes);
        return;
    }
    if (kind === "pick_audio") {
        closeAttachmentPalette();
        const requested = getConversationId();
        try {
            const picked = await ipc.pickChatAudioAttachment(requested);
            if (getConversationId() !== requested) {
                if (picked?.audioRef) ipc.removeChatAudioAttachment(picked.audioRef).catch(() => {});
                return;
            }
            if (picked?.audioRef) {
                addAttachments([{
                    kind: "audio",
                    ref: picked.audioRef,
                    display: picked.displayName || t("chat.attach.audio_fallback"),
                }]);
            }
        } catch (error) {
            console.warn("[attachment-palette] 音频选择失败:", error);
        }
        return;
    }
    if (kind === "session_ref") {
        closeAttachmentPalette();
        let ref = null;
        try {
            ref = JSON.parse(value);
        } catch {
            return;
        }
        addAttachments([{
            kind: ref.kind,
            ref: ref.ref,
            display: attachmentDisplayName(ref, ref.kind),
        }]);
        return;
    }
    if (kind === "recent_image") {
        closeAttachmentPalette();
        try {
            const signed = await ipc.attachmentSignClipboardHistoryImage(value, getConversationId());
            if (signed?.ref) {
                addAttachments([{
                    kind: "image",
                    ref: signed.ref,
                    display: signed.display || t("chat.attach.image_fallback"),
                }]);
            }
        } catch (error) {
            console.warn("[attachment-palette] 图片签发失败:", error);
        }
        return;
    }
    if (kind === "recent_files") {
        closeAttachmentPalette();
        const outcomes = await runPick(() =>
            ipc.attachmentAttachClipboardFiles(value, getConversationId())
        );
        applyOutcomes(outcomes);
        return;
    }
}

async function runPick(invoker) {
    const requested = getConversationId();
    try {
        const outcomes = await invoker();
        if (getConversationId() !== requested) {
            // 会话已切换：撤销本次签发结果由 revoke_by_owner 统一回收，结果丢弃
            return [];
        }
        return Array.isArray(outcomes) ? outcomes : [];
    } catch (error) {
        console.warn("[attachment-palette] 附件签发失败:", error);
        return [];
    }
}

/** 把签发结果落进 composer；失败项留在面板内逐项展示（不静默丢项）。 */
function applyOutcomes(outcomes) {
    const okList = [];
    const failures = [];
    for (const outcome of outcomes) {
        const attachment = outcome?.ok?.attachment || outcome?.ok;
        if (attachment?.ref) {
            okList.push({
                kind: attachment.kind,
                ref: attachment.ref,
                display: attachment.display || outcome.display,
                truncated: outcome.ok?.meta?.truncated || false,
                tokenEst: outcome.ok?.meta?.tokenEst ?? null,
            });
        } else if (outcome?.display) {
            failures.push({display: outcome.display, reason: outcome.reason || ""});
        }
    }
    if (okList.length > 0) addAttachments(okList);
    if (failures.length > 0) {
        // 重开面板显示失败原因（含成功提示），用户可继续选择其他文件
        manuallyOpened = true;
        activeQuery = {query: ""};
        paletteEl.innerHTML =
            `<div class="cp-section-title">${escapeText(t("chat.attach.fail_title"))}</div>` +
            failures
                .map((f) => `<div class="cp-row cp-row-static">
                    <span class="cp-row-icon">${iconHTML("file-x")}</span>
                    <span class="cp-row-title">${escapeText(f.display)}</span>
                    <span class="cp-row-desc">${escapeText(f.reason)}</span>
                </div>`)
                .join("") +
            `<div class="cp-row" data-adopt="pick_file">
                <span class="cp-row-icon">${iconHTML("folder-open")}</span>
                <span class="cp-row-title">${escapeText(t("chat.attach.pick_file"))}</span>
            </div>`;
        items = [{kind: "pick_file"}];
        highlightIndex = 0;
        paletteEl.hidden = false;
        openExclusive("attach");
        applyHighlight();
    }
}
