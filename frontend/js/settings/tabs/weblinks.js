/**
 * 快捷超链设置 Tab（0.26.12 WebEngine）。
 *
 * 数据分片 `app.weblinks`（读 get_config_section / 写 saveConfig("weblinks")）。
 * 保存时后端做 keyword 冲突校验（表内/保留前缀/engine/插件触发词），冲突整体
 * 拒绝并回传结构化 conflicts——前端解析后逐条展示，本地预校验只挡明显错误。
 */
import {saveConfig} from "../../shared/config-keys.js";
import {onLangChange, t} from "../../i18n/index.js";
import {invoke} from "../../shared/tauri.js";
import {renderIcon} from "../../shared/icon.js";

// ── 纯函数（weblinks.test.mjs 覆盖） ─────────────────────────────────────────

/** URL 模板含 {query} 即搜索型（与后端 weblinks_engine::is_search_template 一致）。 */
export function isSearchTemplate(url) {
    return typeof url === "string" && url.includes("{query}");
}

/** 后端分片 → 渲染模型。null/损坏 → 空表（后端启动已 seed；null 仅异常窗口出现）。 */
export function normalizeConfig(raw) {
    if (!raw || typeof raw !== "object" || !Array.isArray(raw.entries)) {
        return {entries: [], seeded: true};
    }
    let seq = 0;
    return {
        seeded: raw.seeded !== false,
        entries: raw.entries
            .filter((e) => e && typeof e === "object")
            .map((e) => ({
                id: typeof e.id === "string" && e.id ? e.id : `entry-${Date.now()}-${seq++}`,
                name: String(e.name ?? ""),
                keyword: String(e.keyword ?? ""),
                url_template: String(e.url_template ?? ""),
                enabled: e.enabled !== false,
            })),
    };
}

/**
 * 保存前本地预校验（后端为真源）。返回 issue code 数组，空数组 = 通过。
 * code: name_empty | keyword_invalid | keyword_reserved | url_invalid
 */
export function localValidate(entry) {
    const issues = [];
    if (!entry.name.trim()) issues.push("name_empty");
    const kw = entry.keyword.trim();
    if (!kw || kw.split(/\s+/).length !== 1) issues.push("keyword_invalid");
    if (kw.toLowerCase() === "ai") issues.push("keyword_reserved");
    const url = entry.url_template.trim();
    if (!/^https?:\/\//i.test(url)) issues.push("url_invalid");
    return issues;
}

/**
 * 解析后端 keyword_conflict 错误 → conflicts 数组（{keyword, kind, owner}）。
 * 非 keyword_conflict 错误返回 null（调用方走通用错误展示）。
 */
export function parseConflictError(error) {
    const message = typeof error === "string" ? error : String(error?.message ?? "");
    const marker = "keyword_conflict:";
    const idx = message.indexOf(marker);
    if (idx < 0) return null;
    try {
        const parsed = JSON.parse(message.slice(idx + marker.length).trim());
        return Array.isArray(parsed) ? parsed : null;
    } catch {
        return [];
    }
}

/** conflicts → 展示文本（i18n，测试注入 t）。 */
export function describeConflicts(conflicts, translate) {
    const tt = translate || ((key) => key);
    return conflicts.map((c) => {
        const kind = tt(`weblinks.conflict.${c.kind}`);
        const owner = c.owner ? ` (${c.owner})` : "";
        return `${c.keyword}: ${kind}${owner}`;
    });
}

// ── Tab 初始化 ───────────────────────────────────────────────────────────────

export function initWeblinksTab() {
    const listEl = document.getElementById("weblinks-list");
    const editorEl = document.getElementById("weblinks-editor");
    const errorEl = document.getElementById("weblinks-error");
    const nameEl = document.getElementById("weblinks-f-name");
    const keywordEl = document.getElementById("weblinks-f-keyword");
    const urlEl = document.getElementById("weblinks-f-url");

    /** @type {{entries: Array, seeded: boolean}} */
    let config = {entries: [], seeded: true};
    /** 编辑态：null=关闭；"new"=新增；其他=正在编辑的 entry id。 */
    let editingId = null;
    let saveChain = Promise.resolve();

    async function load() {
        let raw = null;
        try {
            raw = await invoke("get_config_section", {key: "app.weblinks"});
        } catch (error) {
            console.error("[weblinks] load failed:", error);
        }
        config = normalizeConfig(raw);
        // 不关闭编辑器：编辑中的条目可能刚被保存失败回滚（DB 无此 id），
        // 表单值仍在输入框，用户修正后重试即可。
        render();
    }

    function showError(lines) {
        if (!lines.length) {
            errorEl.classList.add("hidden");
            errorEl.textContent = "";
            return;
        }
        errorEl.classList.remove("hidden");
        errorEl.textContent = lines.join("\n");
    }

    function markKeywordInvalid(invalid) {
        keywordEl.classList.toggle("weblinks-input-error", invalid);
    }

    function render() {
        listEl.textContent = "";
        if (!config.entries.length && editingId === null) {
            const empty = document.createElement("p");
            empty.className = "weblinks-empty";
            empty.textContent = t("weblinks.empty");
            listEl.appendChild(empty);
        }
        for (const entry of config.entries) {
            listEl.appendChild(renderRow(entry));
        }
        editorEl.classList.toggle("hidden", editingId === null);
        if (editingId !== null) editorEl.dataset.editing = editingId;
    }

    function renderRow(entry) {
        const row = document.createElement("div");
        row.className = "weblinks-row";

        const icon = document.createElement("span");
        icon.className = "weblinks-row-icon";
        icon.appendChild(renderIcon(
            isSearchTemplate(entry.url_template) ? "globe" : "external-link",
            {ariaLabel: isSearchTemplate(entry.url_template) ? t("weblinks.kind.search") : t("weblinks.kind.direct")},
        ));

        const main = document.createElement("div");
        main.className = "weblinks-row-main";
        const title = document.createElement("span");
        title.className = "weblinks-row-name";
        title.textContent = entry.name;
        const badge = document.createElement("span");
        badge.className = "weblinks-keyword-badge";
        badge.textContent = entry.keyword;
        const url = document.createElement("span");
        url.className = "weblinks-row-url";
        url.textContent = entry.url_template;
        url.title = entry.url_template;
        main.append(title, badge, url);
        if (!entry.enabled) row.classList.add("weblinks-row-disabled");

        const toggle = document.createElement("label");
        toggle.className = "switch";
        const cb = document.createElement("input");
        cb.type = "checkbox";
        cb.checked = entry.enabled;
        cb.addEventListener("change", () => persist({ ...entry, enabled: cb.checked }));
        const slider = document.createElement("span");
        slider.className = "slider";
        toggle.append(cb, slider);

        const editBtn = document.createElement("button");
        editBtn.className = "btn btn-sm";
        editBtn.type = "button";
        editBtn.textContent = t("weblinks.edit");
        editBtn.addEventListener("click", () => openEditor(entry));

        const delBtn = document.createElement("button");
        delBtn.className = "btn btn-sm weblinks-delete";
        delBtn.type = "button";
        delBtn.textContent = t("weblinks.delete");
        delBtn.addEventListener("click", () => {
            config.entries = config.entries.filter((e) => e.id !== entry.id);
            if (editingId === entry.id) closeEditor();
            persistConfig();
        });

        row.append(icon, main, toggle, editBtn, delBtn);
        return row;
    }

    function openEditor(entry) {
        editingId = entry ? entry.id : "new";
        nameEl.value = entry ? entry.name : "";
        keywordEl.value = entry ? entry.keyword : "";
        urlEl.value = entry ? entry.url_template : "";
        markKeywordInvalid(false);
        showError([]);
        render();
        nameEl.focus();
    }

    function closeEditor() {
        editingId = null;
        markKeywordInvalid(false);
        showError([]);
        render();
    }

    /** 串行化保存（开关快速切换/连续操作不交错）；失败重载回滚乐观状态。 */
    function persistConfig() {
        const pending = saveChain.then(async () => {
            await saveConfig("weblinks", config);
        }).catch(async (error) => {
            console.error("[weblinks] save failed:", error);
            const conflicts = parseConflictError(error);
            showError(conflicts
                ? describeConflicts(conflicts, t)
                : [t("weblinks.error.save_failed")]);
            if (conflicts?.length) markKeywordInvalid(conflicts.some((c) =>
                c.keyword.trim().toLowerCase() === keywordEl.value.trim().toLowerCase()));
            await load();
            throw error;
        });
        saveChain = pending.catch(() => {});
        return pending;
    }

    function persist(entry) {
        config.entries = config.entries.map((e) => (e.id === entry.id ? entry : e));
        render();
        return persistConfig();
    }

    document.getElementById("weblinks-add").addEventListener("click", () => openEditor(null));
    document.getElementById("weblinks-cancel").addEventListener("click", closeEditor);
    editorEl.addEventListener("submit", (event) => {
        event.preventDefault();
        const draft = {
            id: editingId === "new" ? `entry-${Date.now()}-${Math.floor(Math.random() * 1e6)}` : editingId,
            name: nameEl.value.trim(),
            keyword: keywordEl.value.trim(),
            url_template: urlEl.value.trim(),
            enabled: true,
        };
        const issues = localValidate(draft);
        if (issues.length) {
            showError(issues.map((code) => t(`weblinks.error.${code}`)));
            markKeywordInvalid(issues.includes("keyword_invalid") || issues.includes("keyword_reserved"));
            return;
        }
        // id 已在表中 → 替换；否则 push（含保存失败回滚后重试的草稿）
        if (config.entries.some((e) => e.id === draft.id)) {
            config.entries = config.entries.map((e) => (e.id === draft.id ? draft : e));
        } else {
            config.entries.push(draft);
        }
        // 成功才关编辑器；失败时表单保持打开、输入值保留（persistConfig 已展示冲突）
        persistConfig().then(() => {
            editingId = null;
            showError([]);
            render();
        }).catch(() => {/* persistConfig 已展示错误并重载列表 */});
        render();
    });

    onLangChange(() => render());
    load();
}
