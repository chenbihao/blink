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

/** 表单预览：只提取 origin，保留子域名和端口；主页可手动覆盖。 */
export function inferHomepage(raw) {
    try {
        const url = new URL(raw.trim());
        if (!['http:', 'https:'].includes(url.protocol) || url.username || url.password
            || /[{}]/.test(url.hostname)) return '';
        return isSearchTemplate(raw) ? `${url.origin}/` : raw.trim();
    } catch { return ''; }
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
                home_url: String(e.home_url ?? ""),
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
    const home = (entry.home_url ?? '').trim();
    const url = (entry.url_template ?? '').trim();
    if ((!home && !url) || (home && (!inferHomepage(home) || isSearchTemplate(home)))
        || (url && !inferHomepage(url))) issues.push("url_invalid");
    if (home && url && !isSearchTemplate(url)) issues.push("search_query_missing");
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

/** 编辑只修改表单字段，保留条目的启用选择。 */
export function entryFromDraft(existing, fields) {
    return {...fields, enabled: existing?.enabled ?? true};
}

/** 列表视图：名称按字母/中文拼音排序，筛选不改变存储顺序或路由优先级。 */
export function visibleEntries(entries, filter = "") {
    const needle = filter.trim().toLocaleLowerCase();
    const collator = new Intl.Collator('zh-CN', {numeric: true, sensitivity: 'base'});
    return entries.filter(entry => !needle || [entry.name, entry.keyword, entry.home_url, entry.url_template]
        .some(value => String(value ?? '').toLocaleLowerCase().includes(needle)))
        // 拉丁名称在前；中文名称在后一组按拼音排序。
        .sort((a, b) => Number(!/^[a-z0-9]/i.test(a.name)) - Number(!/^[a-z0-9]/i.test(b.name))
            || collator.compare(a.name, b.name) || collator.compare(a.keyword, b.keyword) || a.id.localeCompare(b.id));
}

// ── Tab 初始化 ───────────────────────────────────────────────────────────────

export function initWeblinksTab() {
    const listEl = document.getElementById("weblinks-list");
    const editorEl = document.getElementById("weblinks-editor");
    const errorEl = document.getElementById("weblinks-error");
    const editorErrorEl = document.getElementById("weblinks-editor-error");
    const overlayEl = document.getElementById("weblinks-overlay");
    const filterEl = document.getElementById("weblinks-filter");
    const addEl = document.getElementById("weblinks-add");
    let returnFocus = null;
    let saving = false;
    const nameEl = document.getElementById("weblinks-f-name");
    const keywordEl = document.getElementById("weblinks-f-keyword");
    const urlEl = document.getElementById("weblinks-f-url");
    const homeEl = document.getElementById("weblinks-f-home");

    function updateHomepagePreview() {
        homeEl.placeholder = inferHomepage(urlEl.value) || 'https://example.com/';
    }
    urlEl.addEventListener('input', updateHomepagePreview);

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
        for (const el of [errorEl, editorErrorEl]) {
            el.classList.add("hidden");
            el.textContent = "";
        }
        if (!lines.length) return;
        const target = editingId === null ? errorEl : editorErrorEl;
        target.classList.remove("hidden");
        target.textContent = lines.join("\n");
    }

    function markKeywordInvalid(invalid) {
        keywordEl.classList.toggle("weblinks-input-error", invalid);
    }

    function render() {
        listEl.textContent = "";
        const entries = visibleEntries(config.entries, filterEl.value);
        if (!entries.length) {
            const empty = document.createElement("p");
            empty.className = "weblinks-empty";
            empty.textContent = t(config.entries.length ? "weblinks.no_matches" : "weblinks.empty");
            listEl.appendChild(empty);
        }
        for (const entry of entries) {
            listEl.appendChild(renderRow(entry));
        }
        overlayEl.classList.toggle("hidden", editingId === null);
        document.getElementById("weblinks-editor-title").textContent = t(editingId === "new" ? "weblinks.add_title" : "weblinks.edit_title");
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
        const home = entry.home_url || inferHomepage(entry.url_template);
        url.textContent = home;
        url.title = isSearchTemplate(entry.url_template)
            ? `${home}\n${entry.url_template}` : home;
        main.append(title, badge, url);
        if (!entry.enabled) row.classList.add("weblinks-row-disabled");

        const toggle = document.createElement("label");
        toggle.className = "switch";
        const cb = document.createElement("input");
        cb.type = "checkbox";
        cb.checked = entry.enabled;
        cb.addEventListener("change", () => persist({ ...entry, enabled: cb.checked }).catch(() => {}));
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
            persistConfig().catch(() => {});
        });

        row.append(icon, main, toggle, editBtn, delBtn);
        return row;
    }

    function openEditor(entry) {
        returnFocus = document.activeElement;
        editingId = entry ? entry.id : "new";
        nameEl.value = entry ? entry.name : "";
        keywordEl.value = entry ? entry.keyword : "";
        homeEl.value = entry ? (entry.home_url || (!isSearchTemplate(entry.url_template) ? entry.url_template : '')) : '';
        urlEl.value = entry && isSearchTemplate(entry.url_template) ? entry.url_template : '';
        updateHomepagePreview();
        markKeywordInvalid(false);
        showError([]);
        render();
        nameEl.focus();
    }

    function closeEditor() {
        if (saving) return;
        editingId = null;
        markKeywordInvalid(false);
        showError([]);
        render();
        (returnFocus?.isConnected ? returnFocus : addEl).focus({preventScroll: true});
    }

    filterEl.addEventListener("input", render);
    overlayEl.addEventListener("click", event => {
        if (event.target === overlayEl) closeEditor();
    });
    overlayEl.addEventListener("keydown", event => {
        if (event.key === "Escape") {
            event.preventDefault();
            event.stopPropagation();
            closeEditor();
        } else if (event.key === "Tab") {
            const controls = [...editorEl.querySelectorAll('input:not([disabled]), button:not([disabled])')];
            if (!controls.length) { event.preventDefault(); return; }
            const first = controls[0], last = controls.at(-1);
            if (event.shiftKey && document.activeElement === first) { event.preventDefault(); last.focus(); }
            else if (!event.shiftKey && document.activeElement === last) { event.preventDefault(); first.focus(); }
        }
    });

    /** 串行化保存（开关快速切换/连续操作不交错）；失败重载回滚乐观状态。 */
    function persistConfig() {
        const snapshot = structuredClone(config);
        const pending = saveChain.then(async () => {
            await saveConfig("weblinks", snapshot);
        }).catch(async (error) => {
            console.error("[weblinks] save failed:", error);
            const conflicts = parseConflictError(error);
            showError(conflicts
                ? describeConflicts(conflicts, t)
                : [t(String(error?.message || error).includes('weblink_url_invalid')
                    ? "weblinks.error.url_invalid" : "weblinks.error.save_failed")]);
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
        if (saving) return;
        const draft = entryFromDraft(config.entries.find((entry) => entry.id === editingId), {
            id: editingId === "new" ? `entry-${Date.now()}-${Math.floor(Math.random() * 1e6)}` : editingId,
            name: nameEl.value.trim(),
            keyword: keywordEl.value.trim(),
            home_url: homeEl.value.trim(),
            url_template: urlEl.value.trim(),
        });
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
        // 保存期间锁定当前草稿，旧响应不能关闭下一次编辑。
        saving = true;
        const controls = [...editorEl.querySelectorAll('input, button')];
        controls.forEach(control => { control.disabled = true; });
        persistConfig().then(() => {
            saving = false;
            closeEditor();
        }).catch(() => {/* persistConfig 已展示错误并重载列表 */}).finally(() => {
            saving = false;
            controls.forEach(control => { control.disabled = false; });
            if (editingId !== null) keywordEl.focus();
        });
        render();
    });

    onLangChange(() => render());
    load();
}
