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

// ── 浏览器导入（0.26.21）纯函数（weblinks.test.mjs 覆盖） ────────────────────

/** 后端候选 → 预览行模型：跳过项不勾选；keyword 草稿初始为后端值（可编辑）。 */
export function normalizeCandidates(raw) {
    if (!Array.isArray(raw)) return [];
    return raw
        .filter((c) => c && typeof c === "object")
        .map((c) => ({
            name: String(c.name ?? ""),
            keyword: String(c.keyword ?? ""),
            urlTemplate: String(c.url_template ?? ""),
            skip: c.skip ?? null,
            checked: !c.skip,
        }));
}

/**
 * 预览行 keyword 编辑后的问题码（null = 通过）。
 * 与 localValidate 同规则（空/空白/保留 ai）+ 超链表内重复 + 批内（其他选中行）重复；
 * 与 engine/插件触发词的冲突由保存时后端校验最终把关。
 */
export function importKeywordIssue(keyword, existingKeywords, otherSelectedKeywords) {
    const kw = String(keyword ?? "").trim();
    if (!kw || kw.split(/\s+/).length !== 1) return "keyword_invalid";
    if (kw.toLowerCase() === "ai") return "keyword_reserved";
    const lower = kw.toLowerCase();
    if ((existingKeywords ?? []).some((k) => String(k).toLowerCase() === lower)) return "keyword_exists";
    if ((otherSelectedKeywords ?? []).some((k) => String(k).toLowerCase() === lower)) return "keyword_conflict_in_batch";
    return null;
}

/** 由预览行构造导入条目：仅勾选且 keyword 校验通过的行；批内重复先到先得。 */
export function buildImportEntries(rows, existingKeywords, now = Date.now()) {
    const taken = [];
    const entries = [];
    for (const [index, row] of (rows ?? []).entries()) {
        if (!row.checked || row.skip) continue;
        const keyword = String(row.keyword ?? "").trim();
        if (importKeywordIssue(keyword, existingKeywords, taken)) continue;
        taken.push(keyword);
        entries.push({
            id: `entry-${now}-${index}-${Math.floor(Math.random() * 1e6)}`,
            name: String(row.name ?? "").trim() || keyword,
            keyword,
            home_url: "",
            url_template: row.urlTemplate,
            enabled: true,
        });
    }
    return entries;
}

/** 全选/全不选：只作用于可导入行（跳过行恒不勾选）。 */
export function applySelectAll(rows, checked) {
    return (rows ?? []).map((row) => (row.skip ? row : {...row, checked}));
}

/** 反选：仅翻转可导入行。 */
export function invertSelection(rows) {
    return (rows ?? []).map((row) => (row.skip ? row : {...row, checked: !row.checked}));
}

/** 主复选框三态：全部勾选 / 部分勾选（indeterminate）/ 无可导入行。 */
export function selectAllState(rows) {
    const importable = (rows ?? []).filter((row) => !row.skip);
    const selected = importable.filter((row) => row.checked).length;
    return {
        importable: importable.length,
        selected,
        checked: importable.length > 0 && selected === importable.length,
        indeterminate: selected > 0 && selected < importable.length,
    };
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
    const countEl = document.getElementById("weblinks-count");
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
        countEl.textContent = t("weblinks.count", {n: config.entries.length});
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
            // 0.26.21 修复（0.26.12 起存量 bug）：普通状态删除从未重渲染——仅编辑中
            // 经 closeEditor 间接刷新。行残留 DOM 表现为「点了没反应」，对残留行
            // 重复点击又是空操作保存（日志反复 entries=N 同值即此特征）。
            if (editingId === entry.id) closeEditor();
            else render();
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

    /** 串行化保存底座（0.26.21 从 persistConfig 提取；导入流程共用同一队列）。 */
    function enqueueSave() {
        const snapshot = structuredClone(config);
        const pending = saveChain.then(async () => {
            await saveConfig("weblinks", snapshot);
        });
        saveChain = pending.catch(() => {});
        return pending;
    }

    /** 串行化保存（开关快速切换/连续操作不交错）；失败重载回滚乐观状态。 */
    function persistConfig() {
        return enqueueSave().catch(async (error) => {
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

    // ── 浏览器导入（0.26.21）：选浏览器 → 只读扫描 → 预览勾选 → 合并走既有保存 ──
    const importOverlayEl = document.getElementById("weblinks-import-overlay");
    const importBrowserEl = document.getElementById("weblinks-import-browser");
    const importProfileEl = document.getElementById("weblinks-import-profile");
    const importProfileLabelEl = document.getElementById("weblinks-import-profile-label");
    const importStatusEl = document.getElementById("weblinks-import-status");
    const importListEl = document.getElementById("weblinks-import-list");
    const importConfirmEl = document.getElementById("weblinks-import-confirm");
    const selectAllRowEl = document.getElementById("weblinks-import-selectall-row");
    const selectAllCheckEl = document.getElementById("weblinks-import-selectall-check");
    /** 导入会话状态：sources（浏览器→profiles）、rows（预览行）、seq（竞态守卫）。 */
    let importSources = [];
    let importRows = [];
    let importSeq = 0;

    function setImportStatus(text, isError = false) {
        importStatusEl.textContent = text ?? "";
        importStatusEl.classList.toggle("weblinks-import-error", Boolean(text) && isError);
    }

    function selectedProfiles() {
        const source = importSources.find((s) => s.browser === importBrowserEl.value);
        return source ? source.profiles : [];
    }

    function renderImportControls() {
        // 重建选项前保留当前选中值——change 事件内重建会把 select 重置回首项
        // （切换浏览器时表现为「选了 Edge 还是 Chrome」，0.26.21 修复）。
        const previousBrowser = importBrowserEl.value;
        importBrowserEl.textContent = "";
        for (const source of importSources) {
            const option = document.createElement("option");
            option.value = source.browser;
            option.textContent = t(`weblinks.import.browser.${source.browser}`);
            importBrowserEl.appendChild(option);
        }
        if (previousBrowser && [...importBrowserEl.options].some((o) => o.value === previousBrowser)) {
            importBrowserEl.value = previousBrowser;
        }
        const profiles = selectedProfiles();
        importProfileEl.classList.toggle("hidden", profiles.length <= 1);
        importProfileLabelEl.classList.toggle("hidden", profiles.length <= 1);
        const previousProfile = importProfileEl.value;
        importProfileEl.textContent = "";
        for (const profile of profiles) {
            const option = document.createElement("option");
            option.value = profile;
            option.textContent = profile;
            importProfileEl.appendChild(option);
        }
        if (previousProfile && [...importProfileEl.options].some((o) => o.value === previousProfile)) {
            importProfileEl.value = previousProfile;
        }
    }

    /** 确认按钮 = 可导入数（勾选且 keyword 校验通过）；0 时禁用。 */
    function updateImportConfirm() {
        const entries = buildImportEntries(importRows, config.entries.map((e) => e.keyword));
        importConfirmEl.textContent = t("weblinks.import.confirm", {n: entries.length});
        importConfirmEl.disabled = entries.length === 0;
    }

    /** 主复选框三态同步（全选/部分/无）；无可导入行时隐藏整行。 */
    function updateSelectAll() {
        const state = selectAllState(importRows);
        selectAllRowEl.classList.toggle("hidden", state.importable === 0);
        selectAllCheckEl.checked = state.checked;
        selectAllCheckEl.indeterminate = state.indeterminate;
    }

    /** 编辑后即时校验单行 keyword（本地规则 + 表内 + 批内选中行）。 */
    function refreshImportRowState(row, keywordInput, existingKeywords) {
        const issueEl = keywordInput.parentElement.querySelector(".weblinks-import-issue");
        const others = importRows
            .filter((r) => r !== row && r.checked && !r.skip)
            .map((r) => r.keyword);
        const code = importKeywordIssue(row.keyword, existingKeywords, others);
        keywordInput.classList.toggle("weblinks-input-error", Boolean(code));
        if (issueEl) {
            issueEl.textContent = code ? t(`weblinks.import.issue.${code}`) : "";
            issueEl.classList.toggle("hidden", !code);
        }
    }

    function renderImportRow(row, existingKeywords) {
        const container = document.createElement("div");
        container.className = "weblinks-import-row" + (row.skip ? " weblinks-import-row-skip" : "");

        // 应用 .checkbox 组件（隐藏原生框 + checkmark），主题色/焦点环与全局一致
        const checkLabel = document.createElement("label");
        checkLabel.className = "checkbox";
        const check = document.createElement("input");
        check.type = "checkbox";
        check.checked = row.checked;
        check.disabled = Boolean(row.skip);
        check.setAttribute("aria-label", row.name);
        const mark = document.createElement("span");
        mark.className = "checkmark";
        checkLabel.append(check, mark);
        check.addEventListener("change", () => {
            row.checked = check.checked;
            updateSelectAll();
            updateImportConfirm();
        });

        const name = document.createElement("span");
        name.className = "weblinks-import-name";
        name.textContent = row.name;
        name.title = row.name;

        const keyword = document.createElement("input");
        keyword.className = "input-small weblinks-import-keyword";
        keyword.type = "text";
        keyword.maxLength = 20;
        keyword.value = row.keyword;
        keyword.disabled = Boolean(row.skip);
        keyword.setAttribute("aria-label", t("weblinks.field.keyword"));
        keyword.addEventListener("input", () => {
            row.keyword = keyword.value;
            refreshImportRowState(row, keyword, existingKeywords);
            updateImportConfirm();
        });

        const url = document.createElement("span");
        url.className = "weblinks-import-url";
        url.textContent = row.urlTemplate;
        url.title = row.urlTemplate;

        container.append(checkLabel, name, keyword, url);
        if (row.skip) {
            const badge = document.createElement("span");
            badge.className = "weblinks-import-skip-badge";
            badge.textContent = t(`weblinks.import.skip.${row.skip}`);
            container.appendChild(badge);
        } else {
            const issue = document.createElement("span");
            issue.className = "weblinks-import-issue hidden";
            container.appendChild(issue);
            refreshImportRowState(row, keyword, existingKeywords);
        }
        return container;
    }

    function renderImport() {
        importListEl.textContent = "";
        const existingKeywords = config.entries.map((entry) => entry.keyword);
        for (const row of importRows) {
            importListEl.appendChild(renderImportRow(row, existingKeywords));
        }
        if (!importRows.length) {
            const empty = document.createElement("p");
            empty.className = "weblinks-empty";
            empty.textContent = t("weblinks.import.empty");
            importListEl.appendChild(empty);
        }
        updateSelectAll();
        updateImportConfirm();
    }

    async function scanImport() {
        if (!importBrowserEl.value) return;
        const browser = importBrowserEl.value;
        const profile = importProfileEl.classList.contains("hidden") ? null : importProfileEl.value;
        const seq = ++importSeq;
        importRows = [];
        importListEl.textContent = "";
        setImportStatus(t("weblinks.import.scanning"));
        updateImportConfirm();
        try {
            const scan = await invoke("scan_browser_search_engines", {browser, profile});
            if (seq !== importSeq) return; // 迟到响应丢弃（切浏览器/重扫/关弹窗）
            importRows = normalizeCandidates(scan?.candidates);
            const importable = importRows.filter((row) => !row.skip).length;
            setImportStatus(t("weblinks.import.summary", {total: importRows.length, importable}));
        } catch (error) {
            if (seq !== importSeq) return;
            console.error("[weblinks] scan failed:", error);
            setImportStatus(String(error?.message ?? error) || t("weblinks.import.scan_failed"), true);
        } finally {
            if (seq === importSeq) renderImport();
        }
    }

    async function openImport() {
        importOverlayEl.classList.remove("hidden");
        importRows = [];
        importListEl.textContent = "";
        setImportStatus(t("weblinks.import.loading_sources"));
        updateImportConfirm();
        const seq = ++importSeq;
        try {
            const sources = await invoke("list_browser_search_sources");
            if (seq !== importSeq) return;
            importSources = Array.isArray(sources)
                ? sources.filter((s) => s && Array.isArray(s.profiles) && s.profiles.length)
                : [];
            if (!importSources.length) {
                setImportStatus(t("weblinks.import.no_sources"), true);
                return;
            }
            renderImportControls();
            await scanImport();
        } catch (error) {
            if (seq !== importSeq) return;
            console.error("[weblinks] list sources failed:", error);
            setImportStatus(String(error?.message ?? error), true);
        }
    }

    function closeImport() {
        importSeq++; // 使在途扫描/枚举响应失效
        importOverlayEl.classList.add("hidden");
        importRows = [];
        importSources = [];
        importListEl.textContent = "";
        setImportStatus("");
    }

    document.getElementById("weblinks-import-open").addEventListener("click", openImport);
    document.getElementById("weblinks-import-cancel").addEventListener("click", closeImport);
    document.getElementById("weblinks-import-rescan").addEventListener("click", scanImport);
    // 全选（三态主复选框，只作用于可导入行）与反选
    selectAllCheckEl.addEventListener("change", () => {
        importRows = applySelectAll(importRows, selectAllCheckEl.checked);
        renderImport();
    });
    document.getElementById("weblinks-import-invert").addEventListener("click", () => {
        importRows = invertSelection(importRows);
        renderImport();
    });
    importBrowserEl.addEventListener("change", () => {
        renderImportControls();
        scanImport();
    });
    importProfileEl.addEventListener("change", scanImport);
    importOverlayEl.addEventListener("click", (event) => {
        if (event.target === importOverlayEl) closeImport();
    });
    importOverlayEl.addEventListener("keydown", (event) => {
        if (event.key === "Escape") {
            event.preventDefault();
            event.stopPropagation();
            closeImport();
        }
    });
    document.getElementById("weblinks-import-form").addEventListener("submit", (event) => {
        event.preventDefault();
        const entries = buildImportEntries(importRows, config.entries.map((e) => e.keyword));
        if (!entries.length) {
            setImportStatus(t("weblinks.import.none_selected"), true);
            return;
        }
        importConfirmEl.disabled = true;
        config.entries.push(...entries);
        render();
        enqueueSave().then(() => {
            closeImport();
        }).catch((error) => {
            // 冲突整体拒绝：详情显示在导入弹窗内（主列表错误区被遮罩挡住）；
            // 本地乐观状态由 load() 回滚为后端真源，预览行保留供修正 keyword 后重试。
            console.error("[weblinks] import save failed:", error);
            const conflicts = parseConflictError(error);
            setImportStatus(conflicts
                ? describeConflicts(conflicts, t).join("\n")
                : String(error?.message ?? error), true);
            load();
        });
    });

    onLangChange(() => {
        render();
        if (!importOverlayEl.classList.contains("hidden")) {
            renderImportControls();
            renderImport();
        }
    });
    load();
}
