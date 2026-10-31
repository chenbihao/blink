/**
 * 搜索引擎 Tab 模块
 * 包含：应用搜索、文件搜索、计算器配置
 *
 * 保存策略：全部即时保存（change 事件 → saveConfig），不保留"保存配置"按钮。
 * 与其它自动保存卡片（context / general / chord）一致。
 */

import {confirmDialog, invoke, listen, messageDialog} from "../../shared/tauri.js";
import {onLangChange, t} from "../../i18n/index.js";
import {saveConfig} from "../../shared/config-keys.js";
import {EVENTS} from "../../shared/event-names.js";
import {formatBytes, progressPercent} from "../../shared/download-progress.js";
import {initApplicationSearch} from "./application-search.js";
import {initMediaRuntime} from "./engines/media-runtime.js";
import {
    initManagedEverything,
    refreshBadgeText as refreshManagedEverythingBadgeText,
    renderManagedEverything,
} from "./engines/managed-everything.js";

/**
 * 初始化搜索引擎 Tab
 * @param {Object} cfg - 初始配置
 */
export function initEnginesTab(cfg) {
    // 回填配置（0.9.5 拆分时丢失的 loadEngineConfig，0.9.5.1 补回）
    loadEngineConfig();
    initApplicationSearch();
    initCalcConfig();
    initFileSearchConfig();
    initBookmarksConfig();

    // 状态徽章文本是 JS 动态生成（状态结果 → i18n key），applyI18n 扫不到，
    // 语言切换时通过 i18n 订阅自行刷新
    onLangChange(() => {
        if (lastFileSearchStatus) renderFileSearchStatus(lastFileSearchStatus);
        refreshEverythingBadgeText();
        refreshBookmarksBadgeText();
        refreshManagedEverythingBadgeText();
        refreshScriptRuntimeBadgeText("python");
        refreshScriptRuntimeBadgeText("node");
    });
}

export function initManagedEnginesTab() {
    initMediaRuntime();
    initScriptRuntime();
    initManagedEverything({refreshStatus: refreshFileSearchStatus});
}

let lastFileSearchStatus = null;
let fileStatusRevision = 0;
let fileSaveChain = Promise.resolve();

/**
 * 加载并回填搜索引擎配置（应用搜索 / 文件搜索 / 计算器）
 * 拆自原 settings.js loadEngineConfig；loadBuiltinActions/loadPlugins 已归 plugins.js，不在此调。
 */
async function loadEngineConfig() {
    // 文件搜索
    try {
        const fileSearch = await invoke("get_engine_config", {engineId: "file_search"});
        const enabled = fileSearch.enabled !== false;
        const port = fileSearch.everything_port || 80;
        const maxResults = fileSearch.max_results || 20;

        const enabledEl = document.getElementById("file-search-enabled");
        const portEl = document.getElementById("everything-port");
        const maxResultsEl = document.getElementById("everything-max-results");

        if (enabledEl) enabledEl.checked = enabled;
        if (portEl) portEl.value = port;
        if (maxResultsEl) maxResultsEl.value = maxResults;

        refreshFileSearchStatus();
    } catch (e) {
        console.error("loadFileSearchConfig failed:", e);
        const portEl = document.getElementById("everything-port");
        if (portEl) portEl.value = 80;
    }

    // 计算器
    try {
        const calc = await invoke("get_calc_config");
        const enabledEl = document.getElementById("calc-enabled");
        if (enabledEl) enabledEl.checked = calc.enabled !== false;
    } catch (e) {
        console.error("loadCalcConfig failed:", e);
    }
}

/**
 * 初始化计算器配置
 */
function initCalcConfig() {
    document.getElementById("calc-enabled")?.addEventListener("change", async (e) => {
        try {
            await saveConfig("calc_config", {enabled: e.target.checked});
        } catch (err) {
            console.error("update_calc_config failed:", err);
            e.target.checked = !e.target.checked;
        }
    });
}

/**
 * 初始化文件搜索配置：任一字段变更即保存；探测按钮独立。
 * 0.26.15 移除「搜索模式」选择：来源决策收敛为自动（用户 Everything → 内置引擎 → 本地），
 * UI 固定写 data_source:"auto"；托管 Everything 的安装/启停在 managed-everything.js。
 * 0.26.23 单端口收敛：仅保留「Everything 端口」（探测/连接已有服务），
 * 托管实例监听端口内部自动分配，只经状态徽章展示。
 */
function initFileSearchConfig() {
    // 探测 Everything 状态
    document.getElementById("probe-everything")?.addEventListener("click", probeEverythingStatus);

    const enabledEl = document.getElementById("file-search-enabled");
    const portEl = document.getElementById("everything-port");
    const maxResultsEl = document.getElementById("everything-max-results");

    [enabledEl, portEl, maxResultsEl].forEach((el) => {
        el?.addEventListener("change", saveFileSearchConfig);
    });
}

async function saveFileSearchConfig(event) {
    const enabled = document.getElementById("file-search-enabled")?.checked ?? true;
    const maxResults = parseInt(document.getElementById("everything-max-results")?.value, 10) || 20;
    const patch = {enabled, data_source: "auto", max_results: maxResults};
    // 编辑对象由控件身份决定，连接状态刷新不能改变正在保存的端口归属。
    const targetId = event?.currentTarget?.id;
    if (targetId === "everything-port") {
        const port = Number(document.getElementById(targetId).value);
        if (!Number.isInteger(port) || port < 1 || port > 65535) return;
        patch.everything_port = port;
    }

    try {
        const pending = fileSaveChain.then(() => saveConfig("file_search", patch));
        fileSaveChain = pending.catch(() => {});
        await pending;
        if (targetId === "everything-port") await probeEverythingStatus();
        else await refreshFileSearchStatus();
    } catch (e) {
        console.error("update_file_search failed:", e);
        messageDialog(t("common.save_failed_msg", {err: e}), {title: t("common.error"), kind: "error"});
    }
}

/**
 * 探测 Everything 状态（0.26.14 统一面）：live 探测你的 Everything 端口，
 * 随后拉取统一状态（用户实例 + 托管实例）刷新两枚徽章。
 */
async function probeEverythingStatus() {
    const revision = ++fileStatusRevision;
    const button = document.getElementById("probe-everything");
    if (button) button.disabled = true;
    try {
        const payload = await invoke("probe_everything");
        if (revision === fileStatusRevision) renderFileSearchStatus(payload);
    } catch (error) {
        console.error("probe_everything failed:", error);
    } finally {
        if (button) button.disabled = false;
    }
}

/**
 * 拉取统一状态并渲染两枚徽章（0.26.14：一次 invoke，用户实例取引擎内存态，
 * 不做 live 探测——live 探测只在「重新探测」按钮里发生）。
 */
async function refreshFileSearchStatus() {
    const revision = ++fileStatusRevision;
    let payload = null;
    try {
        payload = await invoke("get_file_search_status");
    } catch (e) {
        console.error("get_file_search_status failed:", e);
        return;
    }
    if (revision === fileStatusRevision) renderFileSearchStatus(payload);
}

/**
 * 渲染统一状态 payload：
 * { userPort, userAvailable(null|bool), installed, running, port, version, lastError, restartBackoffMs }
 */
function renderFileSearchStatus(payload) {
    if (!payload) return;
    lastFileSearchStatus = payload;
    const active = document.getElementById("file-search-active-source");
    if (active) {
        active.textContent = t(`engine.file_search.source.${payload.activeSource || "unavailable"}`);
        active.className = "status-badge " + (["everything", "managed"].includes(payload.activeSource) ? "status-available" : "status-unknown");
    }
    renderPortRow(payload);
    // ── 内置搜索引擎（托管）──
    renderManagedEverything(payload);
}

/**
 * 端口行只剩用户端口徽章（0.26.23 单端口收敛）：探测/轮询只刷新状态，
 * 不重写用户正在编辑的偏好；托管实例状态徽章归 renderManagedEverything。
 */
function renderPortRow(payload) {
    const badge = document.getElementById("everything-status");
    if (badge) {
        badge.dataset.badgeState = payload.userProbing ? "probing"
            : payload.userAvailable === true ? "available"
                : payload.userAvailable === false ? "unavailable" : "unprobed";
        refreshEverythingBadgeText();
    }
}

/**
 * 刷新用户实例端口徽章文本（语言切换/状态更新共用）。
 */
export function refreshEverythingBadgeText() {
    const statusEl = document.getElementById("everything-status");
    if (!statusEl) return;
    const key =
        statusEl.dataset.badgeState === "available" ? "engine.status.available" :
            statusEl.dataset.badgeState === "unavailable" ? "engine.status.unavailable" :
                statusEl.dataset.badgeState === "failed" ? "engine.status.failed" :
                    statusEl.dataset.badgeState === "unprobed" ? "engine.status.unprobed" :
                        "engine.status.probing";
    statusEl.textContent = t(key);
    statusEl.className =
        statusEl.dataset.badgeState === "available"
            ? "status-badge status-available"
            : statusEl.dataset.badgeState === "unprobed"
                ? "status-badge status-unknown"
                : "status-badge status-unavailable";
}

// ── 浏览器书签（BookmarkEngine，0.26.13）──────────────────────────────────
//
// 数据分片 app.bookmarks（读 get_config_section / 写 saveConfig("bookmarks")）。
// 保存时后端做 keyword 冲突校验（与 weblinks 同套），冲突拒绝并回传结构化
// conflict——前端解析后标红 keyword 输入框。索引状态经 get_bookmarks_status 只读查询。

/** 后端冲突 kind → i18n key（与 weblinks.conflict.* 复用同一组文案）。 */
function describeBookmarkConflict(conflict) {
    const kind = t(`weblinks.conflict.${conflict.kind}`);
    const owner = conflict.owner ? ` (${conflict.owner})` : "";
    return `${conflict.keyword}: ${kind}${owner}`;
}

/** 解析后端 keyword_conflict 错误 → conflict 对象；非该类错误返回 null。 */
function parseBookmarkConflictError(error) {
    const message = typeof error === "string" ? error : String(error?.message ?? "");
    const marker = "keyword_conflict:";
    const idx = message.indexOf(marker);
    if (idx < 0) return null;
    try {
        const parsed = JSON.parse(message.slice(idx + marker.length).trim());
        return Array.isArray(parsed) ? parsed[0] ?? null : null;
    } catch {
        return null;
    }
}

/** 索引状态徽章（条目数 / 扫描中 / 已禁用 / 截断提示）。 */
let bookmarksStatusRevision = 0;
let lastBookmarksStatus = null;
let bookmarksRefreshPending = false;

function applyBookmarksStatus(st) {
    const statusEl = document.getElementById("bookmarks-status");
    if (!statusEl) return;
    lastBookmarksStatus = st;
    const refreshButton = document.getElementById("bookmarks-refresh");
    if (refreshButton) refreshButton.disabled = bookmarksRefreshPending || !st?.enabled || st.scanning;
    statusEl.removeAttribute("title");
    if (!st?.enabled) {
        statusEl.dataset.badgeState = "off";
        statusEl.textContent = t("engine.bookmarks.status.off");
        statusEl.className = "status-badge status-unavailable";
        return;
    }
    if (st.scanning) {
        statusEl.dataset.badgeState = "scanning";
        statusEl.textContent = t("engine.bookmarks.status.scanning");
        statusEl.className = "status-badge status-unknown";
        return;
    }
    statusEl.dataset.badgeState = "ready";
    statusEl.dataset.entries = st.entries;
    statusEl.dataset.truncated = st.truncated ? "1" : "0";
    statusEl.dataset.failedFiles = st.failedFiles || 0;
    statusEl.className = "status-badge " + (st.failedFiles ? "status-unknown"
        : st.entries > 0 ? "status-available" : "status-unavailable");
    refreshBookmarksBadgeText();
}

async function renderBookmarksStatus() {
    if (bookmarksRefreshPending) return;
    const revision = ++bookmarksStatusRevision;
    try {
        const status = await invoke("get_bookmarks_status");
        if (revision === bookmarksStatusRevision) applyBookmarksStatus(status);
    } catch (error) {
        console.error("get_bookmarks_status failed:", error);
    }
}

async function refreshBookmarksNow() {
    if (bookmarksRefreshPending || lastBookmarksStatus?.scanning || !lastBookmarksStatus?.enabled) return;
    bookmarksRefreshPending = true;
    applyBookmarksStatus(lastBookmarksStatus);
    const revision = ++bookmarksStatusRevision;
    try {
        const status = await invoke("refresh_bookmarks");
        if (revision === bookmarksStatusRevision) applyBookmarksStatus(status);
    } catch (error) {
        console.error("refresh_bookmarks failed:", error);
        const statusEl = document.getElementById("bookmarks-status");
        if (revision === bookmarksStatusRevision && statusEl) {
            statusEl.dataset.badgeState = "failed";
            refreshBookmarksBadgeText();
        }
    } finally {
        bookmarksRefreshPending = false;
        const button = document.getElementById("bookmarks-refresh");
        if (button) button.disabled = !lastBookmarksStatus?.enabled || lastBookmarksStatus.scanning;
    }
}

/** 徽章文本刷新（语言切换 / 状态更新共用）。 */
export function refreshBookmarksBadgeText() {
    const statusEl = document.getElementById("bookmarks-status");
    if (!statusEl) return;
    const state = statusEl.dataset.badgeState;
    if (state === "off") {
        statusEl.textContent = t("engine.bookmarks.status.off");
    } else if (state === "scanning") {
        statusEl.textContent = t("engine.bookmarks.status.scanning");
    } else if (state === "ready") {
        const count = Number(statusEl.dataset.entries || 0);
        const failed = Number(statusEl.dataset.failedFiles || 0);
        statusEl.textContent = failed ? t("engine.bookmarks.status.partial", {count, failed}) : count > 0
            ? t("engine.bookmarks.status.ready", {count})
            : t("engine.bookmarks.status.empty");
        if (statusEl.dataset.truncated === "1") {
            statusEl.title = t("engine.bookmarks.status.truncated_hint");
        } else {
            statusEl.removeAttribute("title");
        }
    } else if (state === "failed") {
        statusEl.textContent = t("engine.bookmarks.error.refresh_failed");
        statusEl.className = "status-badge status-unavailable";
    }
}

/**
 * 初始化书签配置：加载分片 + 状态，任一控件变更即保存（触发词回车确认）
 */
function initBookmarksConfig() {
    const enabledEl = document.getElementById("bookmarks-enabled");
    const keywordEl = document.getElementById("bookmarks-keyword");
    const mixEl = document.getElementById("bookmarks-mix");
    const browserEls = ["chrome", "edge", "brave"].map(
        (b) => document.getElementById(`bookmarks-browser-${b}`),
    );
    const errorEl = document.getElementById("bookmarks-error");
    document.getElementById("bookmarks-refresh")?.addEventListener("click", refreshBookmarksNow);
    // 查询只读状态，首扫/配置重扫/手动重扫的完成结果都会自动回填。
    setInterval(() => {
        if (!document.hidden && document.querySelector("#engines.active")) void renderBookmarksStatus();
    }, 3000);

    let config = null;
    let saveChain = Promise.resolve();

    function showError(lines) {
        if (!lines.length) {
            errorEl.classList.add("hidden");
            errorEl.textContent = "";
            return;
        }
        errorEl.classList.remove("hidden");
        errorEl.textContent = lines.join("\n");
    }

    function collect() {
        return {
            enabled: enabledEl?.checked ?? true,
            keyword: (keywordEl?.value || "").trim(),
            mix_into_global: mixEl?.checked ?? false,
            browsers: browserEls
                .map((el, i) => (el?.checked ? ["chrome", "edge", "brave"][i] : null))
                .filter(Boolean),
            max_entries: config?.max_entries ?? 50000,
        };
    }

    /** 串行化保存；冲突标红 keyword，失败展示错误。 */
    function persist() {
        const value = collect();
        const pending = saveChain.then(async () => {
            await saveConfig("bookmarks", value);
            config = value;
        }).catch(async (error) => {
            console.error("[bookmarks] save failed:", error);
            const conflict = parseBookmarkConflictError(error);
            showError(conflict ? [describeBookmarkConflict(conflict)] : [t("engine.bookmarks.error.save_failed")]);
            if (conflict) keywordEl?.classList.add("bookmarks-input-error");
            await load();
            throw error;
        });
        saveChain = pending.catch(() => {});
        return pending;
    }

    async function load() {
        let raw = null;
        try {
            raw = await invoke("get_config_section", {key: "app.bookmarks"});
        } catch (error) {
            console.error("[bookmarks] load failed:", error);
        }
        // null / 损坏 → 默认（后端启动已写默认分片，null 仅异常窗口出现）
        config = raw && typeof raw === "object" ? raw : {browsers: ["chrome", "edge", "brave"]};
        if (enabledEl) enabledEl.checked = config.enabled !== false;
        if (keywordEl) keywordEl.value = config.keyword || "bm";
        if (mixEl) mixEl.checked = config.mix_into_global === true;
        for (const [i, b] of ["chrome", "edge", "brave"].entries()) {
            const el = browserEls[i];
            if (el) el.checked = !Array.isArray(config.browsers) || config.browsers.includes(b);
        }
        renderBookmarksStatus();
    }

    [enabledEl, mixEl, ...browserEls].forEach((el) => {
        el?.addEventListener("change", () => {
            keywordEl?.classList.remove("bookmarks-input-error");
            showError([]);
            persist().then(renderBookmarksStatus).catch(() => {});
        });
    });
    // 触发词：blur / 回车确认（输入中途不逐键保存）
    keywordEl?.addEventListener("keydown", (e) => {
        if (e.key === "Enter") {
            e.preventDefault();
            keywordEl.blur();
        }
    });
    keywordEl?.addEventListener("blur", () => {
        if (!config || (keywordEl.value || "").trim() === (config.keyword || "bm")) return;
        keywordEl.classList.remove("bookmarks-input-error");
        showError([]);
        persist().catch(() => {});
    });

    load();
}

// ── 脚本运行时（Blink 托管，0.25.20 取代系统解释器探测）──────────────────
//
// Python / Node 发行版由 Blink 下载到独立目录并锁定版本，与用户系统 PATH
// 隔离；本区块只做状态展示与安装/卸载操作，进度经
// EVENTS.SCRIPT_INTERPRETER_INSTALL 事件实时更新。

/** 解释器种类列表（DOM id 与后端 kind 一致）。 */
const RUNTIME_KINDS = ["python", "node"];

/**
 * 渲染单个运行时行的状态徽章与操作按钮
 * @param {"python"|"node"} kind
 * @param {Object} st - 后端状态 { kind, version, installed, exe_path }
 */
function renderRuntimeRow(kind, st) {
    const statusEl = document.getElementById(`script-${kind}-status`);
    const actionEl = document.getElementById(`script-${kind}-action`);
    if (!statusEl) return;

    if (st?.installed) {
        statusEl.dataset.badgeState = "installed";
        statusEl.dataset.version = st.version || "";
        statusEl.title = st.exe_path || "";
    } else {
        statusEl.dataset.badgeState = "not_installed";
        delete statusEl.dataset.version;
        statusEl.title = "";
    }
    refreshScriptRuntimeBadgeText(kind);
    if (actionEl) {
        actionEl.dataset.installed = st?.installed ? "1" : "0";
        if (!actionEl.dataset.busy) {
            actionEl.textContent = st?.installed
                ? t("engine.script_runtime.uninstall")
                : t("engine.script_runtime.install");
        }
    }
}

/**
 * 刷新运行时徽章文本（语言切换 / 状态更新共用）
 * @param {"python"|"node"} kind
 */
export function refreshScriptRuntimeBadgeText(kind) {
    const statusEl = document.getElementById(`script-${kind}-status`);
    if (!statusEl) return;
    if (statusEl.dataset.badgeState === "installed") {
        statusEl.textContent = t("engine.script_runtime.installed", {version: statusEl.dataset.version || ""});
        statusEl.className = "status-badge status-available";
    } else {
        statusEl.textContent = t("engine.script_runtime.not_installed");
        statusEl.className = "status-badge status-unavailable";
    }
}

/**
 * 拉取并渲染全部运行时状态（只读 command，无副作用）
 */
async function refreshScriptRuntimeStatus() {
    try {
        const list = await invoke("script_interpreters_status");
        for (const kind of RUNTIME_KINDS) {
            renderRuntimeRow(kind, (list || []).find((x) => x.kind === kind));
        }
    } catch (e) {
        console.error("script_interpreters_status failed:", e);
    }
}

/**
 * 更新某 kind 的下载进度条（downloaded 为 null 时隐藏）
 * @param {"python"|"node"} kind
 * @param {number|null} downloaded 已下载字节
 * @param {number|null} total 总字节
 */
function renderRuntimeProgress(kind, downloaded, total) {
    const wrap = document.getElementById(`script-${kind}-progress`);
    if (!wrap) return;
    const fill = wrap.querySelector(".download-progress__fill");
    const text = wrap.querySelector(".download-progress__text");
    if (downloaded === null || downloaded === undefined) {
        wrap.hidden = true;
        return;
    }
    wrap.hidden = false;
    const percent = progressPercent(downloaded, total);
    fill.classList.toggle("download-progress__fill--indeterminate", percent === null);
    fill.style.width = percent === null ? "" : `${percent}%`;
    if (text) {
        text.textContent = total
            ? `${formatBytes(downloaded)} / ${formatBytes(total)} · ${percent ?? "--"}%`
            : formatBytes(downloaded);
    }
}

/**
 * 安装/卸载按钮态（busy 时禁用并显示进行中文案）
 */
function setRuntimeBusy(kind, busy, stage) {
    const actionEl = document.getElementById(`script-${kind}-action`);
    if (!actionEl) return;
    if (busy) {
        actionEl.dataset.busy = "1";
        actionEl.disabled = true;
        actionEl.textContent = t("engine.script_runtime.installing");
        if (stage) {
            const statusEl = document.getElementById(`script-${kind}-status`);
            if (statusEl) {
                statusEl.dataset.badgeState = "installing";
                statusEl.className = "status-badge status-unknown";
                statusEl.textContent = t(`engine.script_runtime.stage.${stage}`);
            }
        }
    } else {
        delete actionEl.dataset.busy;
        actionEl.disabled = false;
    }
}

/**
 * 安装进度事件 → 单 kind 路由（stage 机：downloading/extracting/verifying/
 * promoting/done/failed）
 */
function handleInstallEvent(payload) {
    const {kind, stage, downloaded, total, message} = payload || {};
    if (!RUNTIME_KINDS.includes(kind)) return;

    if (stage === "downloading" && downloaded !== undefined) {
        setRuntimeBusy(kind, true, "downloading");
        renderRuntimeProgress(kind, downloaded, total ?? null);
        return;
    }
    if (["extracting", "verifying", "promoting"].includes(stage)) {
        renderRuntimeProgress(kind, null);
        setRuntimeBusy(kind, true, stage);
        return;
    }
    if (stage === "done") {
        renderRuntimeProgress(kind, null);
        setRuntimeBusy(kind, false);
        refreshScriptRuntimeStatus();
        return;
    }
    if (stage === "failed") {
        renderRuntimeProgress(kind, null);
        setRuntimeBusy(kind, false);
        const statusEl = document.getElementById(`script-${kind}-status`);
        if (statusEl) {
            statusEl.dataset.badgeState = "failed";
            statusEl.className = "status-badge status-unavailable";
            statusEl.textContent = t("engine.script_runtime.install_failed", {
                message: message || "unknown",
            });
            statusEl.title = message || "";
        }
    }
}

/**
 * 初始化脚本运行时区块：状态拉取 + 按钮绑定 + 进度事件监听
 */
function initScriptRuntime() {
    for (const kind of RUNTIME_KINDS) {
        document.getElementById(`script-${kind}-action`)?.addEventListener("click", async (e) => {
            const btn = e.currentTarget;
            const installed = btn.dataset.installed === "1";
            try {
                if (installed) {
                    // 卸载确认：依赖该运行时的脚本插件会失效
                    const ok = await confirmDialog(
                        t("engine.script_runtime.uninstall_confirm"),
                        {title: t("engine.script_runtime.uninstall"), kind: "warning"},
                    );
                    if (!ok) return;
                    await invoke("uninstall_script_interpreter", {kind});
                } else {
                    setRuntimeBusy(kind, true, "downloading");
                    await invoke("install_script_interpreter", {kind});
                }
                refreshScriptRuntimeStatus();
            } catch (err) {
                console.error(`script runtime action (${kind}) failed:`, err);
                setRuntimeBusy(kind, false);
                messageDialog(String(err?.message || err), {
                    title: t("common.error"), kind: "error",
                });
                refreshScriptRuntimeStatus();
            }
        });
    }

    listen(EVENTS.SCRIPT_INTERPRETER_INSTALL, (event) => {
        handleInstallEvent(event.payload);
    });

    refreshScriptRuntimeStatus();
}
