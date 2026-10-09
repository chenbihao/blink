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
    initScriptRuntime();
    // 状态徽章文本是 JS 动态生成（状态结果 → i18n key），applyI18n 扫不到，
    // 语言切换时通过 i18n 订阅自行刷新
    onLangChange(() => {
        refreshEverythingBadgeText();
        refreshScriptRuntimeBadgeText("python");
        refreshScriptRuntimeBadgeText("node");
    });
}

/**
 * 加载并回填搜索引擎配置（应用搜索 / 文件搜索 / 计算器）
 * 拆自原 settings.js loadEngineConfig；loadBuiltinActions/loadPlugins 已归 plugins.js，不在此调。
 */
async function loadEngineConfig() {
    // 文件搜索
    try {
        const fileSearch = await invoke("get_engine_config", {engineId: "file_search"});
        const enabled = fileSearch.enabled !== false;
        const dataSource = fileSearch.data_source || "auto";
        const port = fileSearch.everything_port || 80;
        const maxResults = fileSearch.max_results || 20;

        const enabledEl = document.getElementById("file-search-enabled");
        const dataSourceEl = document.getElementById("file-search-data-source");
        const portEl = document.getElementById("everything-port");
        const maxResultsEl = document.getElementById("everything-max-results");

        if (enabledEl) enabledEl.checked = enabled;
        if (dataSourceEl) dataSourceEl.value = dataSource;
        if (portEl) portEl.value = port;
        if (maxResultsEl) maxResultsEl.value = maxResults;

        // 页面加载后自动探测一次（非 local 模式）
        if (dataSource !== "local") {
            setTimeout(probeEverythingStatus, 500);
        }
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
 * 初始化文件搜索配置：任一字段变更即保存；探测按钮独立
 */
function initFileSearchConfig() {
    // 探测 Everything 状态
    document.getElementById("probe-everything")?.addEventListener("click", probeEverythingStatus);

    const enabledEl = document.getElementById("file-search-enabled");
    const dataSourceEl = document.getElementById("file-search-data-source");
    const portEl = document.getElementById("everything-port");
    const maxResultsEl = document.getElementById("everything-max-results");

    [enabledEl, dataSourceEl, portEl, maxResultsEl].forEach((el) => {
        el?.addEventListener("change", saveFileSearchConfig);
    });
}

/**
 * 保存文件搜索配置（校验端口 + saveConfig + 视需重探）
 */
async function saveFileSearchConfig() {
    const enabled = document.getElementById("file-search-enabled")?.checked ?? true;
    const dataSource = document.getElementById("file-search-data-source")?.value || "auto";
    const port = parseInt(document.getElementById("everything-port")?.value, 10);
    const maxResults = parseInt(document.getElementById("everything-max-results")?.value, 10) || 20;

    if (!Number.isFinite(port) || port < 1 || port > 65535) {
        // 用户还在输入中（例如清空端口再输），静默跳过；等输到合法值再自动保存
        console.warn("[engines] file-search port invalid, skip auto-save:", port);
        return;
    }

    try {
        await saveConfig("file_search", {
            enabled,
            data_source: dataSource,
            everything_port: port,
            max_results: maxResults,
        });
        if (dataSource !== "local") {
            probeEverythingStatus();
        }
    } catch (e) {
        console.error("update_file_search failed:", e);
        messageDialog(t("common.save_failed_msg", {err: e}), {title: t("common.error"), kind: "error"});
    }
}

/**
 * 探测 Everything 状态
 */
async function probeEverythingStatus() {
    const statusEl = document.getElementById("everything-status");
    const portInput = document.getElementById("everything-port");
    const port = parseInt(portInput?.value || "80", 10);

    statusEl.textContent = t("engine.status.probing");
    statusEl.className = "status-badge status-unknown";
    statusEl.dataset.badgeState = "probing";

    try {
        const available = await invoke("probe_everything", {port});
        if (available) {
            statusEl.textContent = t("engine.status.available");
            statusEl.className = "status-badge status-available";
            statusEl.dataset.badgeState = "available";
        } else {
            statusEl.textContent = t("engine.status.unavailable");
            statusEl.className = "status-badge status-unavailable";
            statusEl.dataset.badgeState = "unavailable";
        }
    } catch (e) {
        statusEl.textContent = t("engine.status.failed");
        statusEl.className = "status-badge status-unavailable";
        statusEl.dataset.badgeState = "failed";
        console.error("probe_everything failed:", e);
    }
}

/**
 * 刷新 Everything 徽章文本（语言切换时）
 */
export function refreshEverythingBadgeText() {
    const statusEl = document.getElementById("everything-status");
    if (!statusEl) return;
    const key =
        statusEl.dataset.badgeState === "available" ? "engine.status.available" :
            statusEl.dataset.badgeState === "unavailable" ? "engine.status.unavailable" :
                statusEl.dataset.badgeState === "failed" ? "engine.status.failed" :
                    "engine.status.probing";
    statusEl.textContent = t(key);
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
