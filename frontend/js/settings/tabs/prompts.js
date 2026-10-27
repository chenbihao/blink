/**
 * 提示词库（0.27.5；0.28.9 从独立 tab 合入 AI 对话设置面板）
 *
 * 可复用指令资产：内置条目（整理/精简/中英互译，随版本默认内容 + 可覆盖）、
 * 自建条目（chat / editor 作用域）。Chat 插入与编辑器预设（0.27.6）共用。
 *
 * 宿主结构：ai-chat 面板「提示词库」卡的 details 折叠区内（#prompts-container）。
 * 条目为行式列表（非嵌套卡），编辑器为卡内子面板——避免 extension-card 套卡。
 *
 * 后端命令：prompt_library_list / upsert / delete / reset / reorder；
 * 变更经 blink://prompt-library-changed 广播（多窗口同步刷新）。
 */

import {invoke, listen, normalizeError} from "../../shared/tauri.js";
import {EVENTS} from "../../shared/event-names.js";
import {onLangChange, t} from "../../i18n/index.js";
import {iconHTML} from "../../shared/icon.js";

/** 作用域过滤器状态（all / chat / editor）。 */
let scopeFilter = "all";

/** 当前已载入条目（变更事件到达时整表重载，无本地乐观写入）。 */
let entries = [];

/** 正在编辑的条目 id（null = 未编辑；"__new__" = 新建）。 */
let editingId = null;

/**
 * 初始化提示词库 Tab。
 * @param {Object} cfg - 初始配置（未使用，保持 tab init 签名一致）
 */
export function initPromptsTab(cfg) {
    const container = document.getElementById("prompts-container");
    if (!container) return;
    void reload(true);

    // 变更事件：多窗口/编辑器消费方修改后同步刷新（隐藏中的条目也重列）；
    // 正在编辑时保留编辑卡 DOM（不重建，防丢输入），仅刷新列表区
    listen(EVENTS.PROMPT_LIBRARY_CHANGED, () => {
        void reload(false, editingId !== null);
    });
    onLangChange(() => {
        if (editingId === null) void reload(false);
    });
}

/**
 * 重新载入列表并渲染。
 * @param {boolean} resetEditing - true 清除编辑态（首次进入）
 * @param {boolean} [preserveEditor] - true 时保留编辑卡 DOM 不重建（变更事件
 *        到达但用户正在编辑——列表刷新、输入不丢；编辑卡按钮监听随节点保留）
 */
async function reload(resetEditing, preserveEditor = false) {
    if (resetEditing) editingId = null;
    try {
        entries = await invoke("prompt_library_list", {scope: null});
    } catch (e) {
        console.error("prompt_library_list failed:", e);
        entries = [];
    }
    render(!preserveEditor || editingId === null);
}

/** 按当前过滤器可见的条目（纯展示过滤，不改数据）。 */
export function visibleEntries(list, filter) {
    if (filter === "all") return [...list];
    return list.filter((entry) => entry.scope === filter);
}

/**
 * 上移/下移后的全量 id 顺序（重排序 payload；只动同 filter 视图内相邻项，
 * 未在视图中的条目保持原相对位置——后端只更新列出的行）。
 */
export function reorderedIdsAfterMove(list, id, direction) {
    const ids = list.map((entry) => entry.id);
    const index = ids.indexOf(id);
    const target = direction === "up" ? index - 1 : index + 1;
    if (index < 0 || target < 0 || target >= ids.length) return null;
    [ids[index], ids[target]] = [ids[target], ids[index]];
    return ids;
}

/** 把后端错误 code 翻译为用户可读文案（冲突/内置锁定/无效输入分类展示）。 */
export function describePromptError(code, message) {
    switch (code) {
        case "prompt_conflict":
            return t("prompts.err.conflict");
        case "prompt_builtin_locked":
            return t("prompts.err.builtin_locked");
        case "prompt_not_found":
            return t("prompts.err.not_found");
        case "prompt_invalid":
            return message || t("prompts.err.invalid");
        default:
            return message || t("prompts.err.save_failed");
    }
}

// ── 渲染 ────────────────────────────────────────────────────────────────────

/**
 * 整表渲染。
 * @param {boolean} [includeEditor] - false 时保留现有编辑面板 DOM（节点摘下
 *        再挂回，用户输入与按钮监听不丢），仅重建工具栏与列表
 */
function render(includeEditor = true) {
    const container = document.getElementById("prompts-container");
    if (!container) return;
    const editorEl = !includeEditor ? document.getElementById("prompt-editor") : null;
    if (editorEl) editorEl.remove();
    container.innerHTML = renderToolbar() + renderList() + (includeEditor ? renderEditor() : "");
    if (editorEl) container.appendChild(editorEl);
    bindEvents(container, editorEl);
    updateSummary(entries.length);
}

/** 折叠区摘要带条目数（details 宿主在 ai-chat 卡内，跨渲染保持文案新鲜）。 */
function updateSummary(count) {
    const summary = document.querySelector("#prompt-library-details summary");
    if (summary) summary.textContent = t("prompts.card.summary_count", {count});
}

function renderToolbar() {
    const options = ["all", "chat", "editor"]
        .map((scope) => {
            const active = scopeFilter === scope ? " active" : "";
            return `<button class="btn-small prompt-scope-filter${active}" data-scope="${scope}" type="button">${t(`prompts.scope.${scope}`)}</button>`;
        })
        .join("");
    return `
    <div class="prompt-toolbar">
      <div class="prompt-scope-filters">${options}</div>
      <button class="btn-small" id="prompt-new" type="button">${iconHTML("plus")} ${t("prompts.action.new")}</button>
    </div>`;
}

function renderList() {
    const visible = visibleEntries(entries, scopeFilter);
    if (visible.length === 0) {
        return `<p class="extension-desc">${t("prompts.empty")}</p>`;
    }
    return visible.map(renderEntry).join("");
}

function renderEntry(entry) {
    const scopeLabel = t(`prompts.scope.${entry.scope}`);
    const badges = [
        `<span class="prompt-badge">${entry.builtin ? iconHTML("lock") : iconHTML(entry.scope === "editor" ? "sparkles" : "message-square")} ${entry.builtin ? t("prompts.badge.builtin") : scopeLabel}</span>`,
        entry.hidden ? `<span class="prompt-badge prompt-badge-muted">${t("prompts.badge.hidden")}</span>` : "",
        entry.builtin && entry.overrideContent
            ? `<span class="prompt-badge prompt-badge-muted">${t("prompts.badge.overridden")}</span>`
            : "",
    ].join("");

    const actions = [
        `<button class="btn-small" data-act="edit" data-id="${entry.id}" type="button">${iconHTML("pencil")} ${t("prompts.action.edit")}</button>`,
        `<button class="btn-small" data-act="duplicate" data-id="${entry.id}" type="button">${iconHTML("copy")} ${t("prompts.action.duplicate")}</button>`,
        `<button class="btn-small" data-act="move-up" data-id="${entry.id}" type="button">${t("prompts.action.move_up")}</button>`,
        `<button class="btn-small" data-act="move-down" data-id="${entry.id}" type="button">${t("prompts.action.move_down")}</button>`,
        entry.builtin && entry.overrideContent
            ? `<button class="btn-small" data-act="reset" data-id="${entry.id}" type="button">${iconHTML("rotate-ccw")} ${t("prompts.action.reset")}</button>`
            : "",
        entry.builtin
            ? `<button class="btn-small" data-act="toggle-hidden" data-id="${entry.id}" type="button">${entry.hidden ? iconHTML("eye") : iconHTML("eye-off")} ${entry.hidden ? t("prompts.action.show") : t("prompts.action.hide")}</button>`
            : `<button class="btn-small prompt-danger" data-act="delete" data-id="${entry.id}" type="button">${iconHTML("trash-2")} ${t("prompts.action.delete")}</button>`,
    ].join("");

    return `
    <div class="prompt-entry${entry.hidden ? " prompt-entry-hidden" : ""}" data-entry="${entry.id}">
      <div class="prompt-entry-head">
        <span class="prompt-entry-name">${escapeHtml(entry.name)}</span>
        <div class="prompt-badges">${badges}</div>
      </div>
      <p class="extension-desc prompt-preview">${escapeHtml(previewText(entry))}</p>
      <div class="prompt-actions">${actions}</div>
    </div>`;
}

/** 有效内容单行预览（内置显示「默认/已覆盖」前缀提示）。 */
function previewText(entry) {
    const body = entry.effectiveContent || "";
    const flat = body.replace(/\s+/g, " ").trim();
    const prefix = entry.builtin
        ? entry.overrideContent
            ? t("prompts.preview.overridden")
            : t("prompts.preview.default")
        : "";
    return prefix + flat.slice(0, 120) + (flat.length > 120 ? "…" : "");
}

function renderEditor() {
    if (editingId === null) return "";
    const isNew = editingId === "__new__";
    const entry = isNew ? null : entries.find((e) => e.id === editingId);
    if (!isNew && !entry) {
        editingId = null;
        return "";
    }
    const scopeOptions = ["chat", "editor"]
        .map((scope) => `<option value="${scope}" ${isNew && scope === "chat" ? "selected" : ""}>${t(`prompts.scope.${scope}`)}</option>`)
        .join("");
    const scopeSelect = isNew
        ? `<div class="setting-row"><label class="setting-label">${t("prompts.editor.scope")}</label><select class="plugin-field" data-field="scope">${scopeOptions}</select></div>`
        : "";
    // 内置条目：正文只读展示默认内容 + 覆盖输入框；自建/新建：正文输入框
    const bodyField = !isNew && entry.builtin
        ? `
        <div class="setting-row">
          <label class="setting-label">${t("prompts.editor.default_content")}</label>
          <textarea class="input-wide plugin-field prompt-content-readonly" readonly rows="4">${escapeHtml(entry.content)}</textarea>
        </div>
        <div class="setting-row">
          <label class="setting-label">${t("prompts.editor.override")}</label>
          <textarea class="input-wide plugin-field" data-field="body" rows="6" placeholder="${t("prompts.editor.override_ph")}">${escapeHtml(entry.overrideContent || "")}</textarea>
        </div>`
        : `
        <div class="setting-row">
          <label class="setting-label">${t("prompts.editor.content")}</label>
          <textarea class="input-wide plugin-field" data-field="body" rows="8" placeholder="${t("prompts.editor.content_ph")}">${escapeHtml(entry?.content || "")}</textarea>
        </div>`;

    return `
    <div class="prompt-editor-panel" id="prompt-editor">
      <div class="prompt-editor-title">${isNew ? t("prompts.editor.new_title") : t("prompts.editor.title")}</div>
      <div class="setting-row">
        <label class="setting-label">${t("prompts.editor.name")}</label>
        <input type="text" class="input-wide plugin-field" data-field="name" value="${escapeAttr(isNew ? "" : entry.name)}" placeholder="${t("prompts.editor.name_ph")}" />
      </div>
      ${scopeSelect}
      ${bodyField}
      <p class="extension-desc hidden" id="prompt-editor-error"></p>
      <div class="prompt-actions">
        <button class="btn" data-act="save" type="button">${t("prompts.action.save")}</button>
        <button class="btn-small" data-act="cancel" type="button">${t("prompts.action.cancel")}</button>
      </div>
    </div>`;
}

// ── 事件 ────────────────────────────────────────────────────────────────────

function bindEvents(container, preservedEditor = null) {
    container.querySelectorAll(".prompt-scope-filter").forEach((button) => {
        button.addEventListener("click", () => {
            scopeFilter = button.dataset.scope;
            render();
        });
    });

    container.querySelector("#prompt-new")?.addEventListener("click", () => {
        editingId = "__new__";
        render();
    });

    container.querySelectorAll("[data-act]").forEach((button) => {
        // 保留挂回的编辑卡节点已带监听，重复绑定会双触发动作
        if (preservedEditor?.contains(button)) return;
        const act = button.dataset.act;
        const id = button.dataset.id;
        button.addEventListener("click", () => void handleAction(act, id, button));
    });
}

async function handleAction(act, id, button) {
    const entry = entries.find((e) => e.id === id);
    switch (act) {
        case "edit":
            editingId = id;
            render();
            break;
        case "duplicate":
            if (!entry) return;
            await save({
                name: t("prompts.duplicate_suffix", {name: entry.name}),
                content: entry.effectiveContent,
                scope: entry.scope,
            });
            break;
        case "move-up":
        case "move-down":
            await move(id, act === "move-up" ? "up" : "down");
            break;
        case "reset":
            await call("prompt_library_reset", {id, expectedRevision: entry?.revision ?? null}, button);
            break;
        case "toggle-hidden":
            if (!entry) return;
            await call(
                "prompt_library_upsert",
                {
                    entry: {
                        id: entry.id,
                        name: entry.name,
                        hidden: !entry.hidden,
                        expectedRevision: entry.revision,
                    },
                },
                button,
            );
            break;
        case "delete":
            if (!entry || !window.confirm(t("prompts.confirm_delete", {name: entry.name}))) return;
            await call("prompt_library_delete", {id}, button);
            break;
        case "save":
            await saveFromEditor(button);
            break;
        case "cancel":
            editingId = null;
            render();
            break;
        default:
            break;
    }
}

/** 从编辑卡收集表单并保存（新建 or 更新）。 */
async function saveFromEditor(button) {
    const card = document.getElementById("prompt-editor");
    if (!card) return;
    const field = (name) => card.querySelector(`[data-field="${name}"]`)?.value?.trim() ?? "";
    const isNew = editingId === "__new__";
    const existing = isNew ? null : entries.find((e) => e.id === editingId);

    const payload = isNew
        ? {name: field("name"), content: field("body"), scope: field("scope") || "chat"}
        : {
              id: existing.id,
              name: field("name"),
              // 内置条目：body 是覆盖内容；自建：body 是正文
              ...(existing.builtin
                  ? {overrideContent: field("body") === "" ? null : field("body")}
                  : {content: field("body")}),
              expectedRevision: existing.revision,
          };
    await save(payload, button);
}

/** 统一保存入口（新建/复制/编辑共用）。 */
async function save(payload, button) {
    try {
        await invoke("prompt_library_upsert", {entry: payload});
        editingId = null;
        await reload(false);
    } catch (e) {
        showEditorError(describePromptError(normalizeError(e).code, normalizeError(e).message));
    } finally {
        if (button) button.disabled = false;
    }
}

async function move(id, direction) {
    // 只在当前过滤视图内换位（与注释语义一致）：全量列表会把视图外相邻条目拉进来
    const ids = reorderedIdsAfterMove(visibleEntries(entries, scopeFilter), id, direction);
    if (!ids) return;
    try {
        await invoke("prompt_library_reorder", {orderedIds: ids});
    } catch (e) {
        console.error("prompt_library_reorder failed:", e);
    }
    await reload(false);
}

async function call(command, args, button) {
    if (button) button.disabled = true;
    try {
        await invoke(command, args);
        await reload(false);
    } catch (e) {
        const err = normalizeError(e);
        showEditorError(describePromptError(err.code, err.message));
    } finally {
        if (button) button.disabled = false;
    }
}

function showEditorError(message) {
    const el = document.getElementById("prompt-editor-error");
    if (el) {
        el.classList.remove("hidden");
        el.textContent = message;
    } else {
        // 编辑卡未开（如列表上的隐藏/删除动作失败）——console + 顶部空态提示
        console.warn("prompts:", message);
    }
}

function escapeHtml(str) {
    const div = document.createElement("div");
    div.textContent = str ?? "";
    return div.innerHTML;
}

function escapeAttr(str) {
    return (str ?? "").replace(/"/g, "&quot;").replace(/'/g, "&#39;");
}
