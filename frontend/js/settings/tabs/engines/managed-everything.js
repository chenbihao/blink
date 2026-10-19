/**
 * 内置搜索引擎（Blink 托管 Everything）UI 模块，0.26.15 自 engines.js 拆出。
 *
 * 同时驱动两张卡片（两 panel 同处一份 DOM，元素常驻，状态/事件一次刷新双端生效）：
 * - managed-engines 面板：完整管理面（状态徽章 + 主操作按钮 + 卸载 + UAC 服务）
 * - engines 面板文件搜索卡：轻量入口行（#file-search-managed-action + 进度条）
 *
 * 主操作按钮置于卡片头部件 le-card-primary 容器（对齐本地模型引擎 le-card），
 * 随状态变形：未安装 → 一键安装；已安装未运行 → 启动；运行中 → 停止；
 * 忙碌 → 禁用等待态。安装完成自动启动实例（装完即用，不要求二次点击）。
 */

import {confirmDialog, invoke, listen, messageDialog} from "../../../shared/tauri.js";
import {t} from "../../../i18n/index.js";
import {EVENTS} from "../../../shared/event-names.js";
import {formatBytes, progressPercent} from "../../../shared/download-progress.js";
import {renderIcon} from "../../../shared/icon.js";

/** 状态刷新回调（engines.js 的 refreshFileSearchStatus，init 时注入，避免循环依赖）。 */
let refreshStatus = async () => {};

let lastStatus = null;
/** 全局忙碌操作：null | install | start | stop | uninstall。两张卡片的按钮共用。 */
let busyOp = null;

// ── 渲染 ──────────────────────────────────────────────────────────────────

/** 主操作视图：kind 为 null 表示忙碌等待态（disabled）。 */
function primaryView(r) {
    if (busyOp) {
        return {kind: null, label: busyLabel(busyOp), disabled: true};
    }
    if (r?.running) return {kind: "stop", label: t("engine.managed.action.stop"), disabled: false};
    if (r?.installed) return {kind: "start", label: t("engine.managed.action.start"), disabled: false};
    return {kind: "install", label: t("engine.managed.action.install"), disabled: false};
}

function busyLabel(op) {
    if (op === "start") return t("engine.managed.status.starting");
    if (op === "stop") return t("engine.managed.action.stopping");
    if (op === "uninstall") return t("engine.managed.uninstalling");
    return t("engine.managed.installing");
}

/** 主操作图标（对齐 le-card iconForKind 映射；uninstall 为低强调入口）。 */
const ACTION_ICONS = {install: "download", start: "play", stop: "square", uninstall: "trash-2"};

/**
 * 渲染单张卡片的主操作按钮（le-card-primary 范式，对齐 local-engine-card
 * updatePrimary）：图标 + 文案，推进型（安装/启动）用 btn-primary 强调，
 * 停止/卸载与忙碌等待态为常规按钮；签名不变时跳过重建防 3s 轮询抖动。
 */
function renderManagedAction(btn, r) {
    const view = primaryView(r);
    const kind = view.kind || busyOp;
    const sig = `${kind || "-"}|${view.label}|${view.disabled ? 1 : 0}`;
    if (btn.dataset.sig === sig) return;
    btn.dataset.sig = sig;
    const emphatic = kind === "install" || kind === "start";
    btn.className = (emphatic ? "btn btn-primary btn-small " : "btn btn-small ") + "le-action-btn le-primary-btn";
    btn.disabled = view.disabled;
    btn.replaceChildren(
        renderIcon(ACTION_ICONS[kind] || "circle", {extraClass: "le-action-icon"}),
        Object.assign(document.createElement("span"), {textContent: view.label}),
    );
    btn.hidden = false;
}

/** 刷新两张卡片的主操作按钮与轻量入口行显隐。 */
function renderActionButtons(r) {
    for (const id of ["managed-everything-action", "file-search-managed-action"]) {
        const btn = document.getElementById(id);
        if (!btn) continue;
        renderManagedAction(btn, r);
    }
    // 用户自己的 Everything 已连通且内置引擎未安装 → 整行不推安装（有 Everything 只配
    // 端口即可）；首个状态到达前整行隐藏，避免给已有 Everything 的用户闪现安装入口
    const entryRow = document.getElementById("file-search-managed-row");
    if (entryRow) entryRow.hidden = !r || (!r.installed && r.userAvailable === true);
}

/** 托管面板状态徽章（transient 场景直接指定 i18n key）。 */
function setTransientBadge(textKey, className) {
    const statusEl = document.getElementById("managed-everything-status");
    if (!statusEl) return;
    statusEl.dataset.badgeState = "installing";
    statusEl.className = `status-badge ${className}`;
    statusEl.textContent = t(textKey);
}

/**
 * 渲染统一状态 payload（engines.js renderFileSearchStatus 尾部调用）。
 * payload 形状：{ userAvailable, installed, running, port, version, lastError,
 * indexCount, serviceInstalled, ... }（托管快照经 #[serde(flatten)] 平铺）。
 */
export function renderManagedEverything(st) {
    lastStatus = st;
    const r = st;
    // 忙碌期间徽章/详情由忙碌路径显式接管（如「启动中…」），轮询不得覆盖
    if (!busyOp) {
        const statusEl = document.getElementById("managed-everything-status");
        if (statusEl) {
            if (r?.running) {
                statusEl.dataset.badgeState = "running";
                statusEl.dataset.port = r.port ?? "";
            } else if (r?.installed) {
                statusEl.dataset.badgeState = "installed";
                delete statusEl.dataset.port;
            } else {
                statusEl.dataset.badgeState = "not_installed";
                delete statusEl.dataset.port;
            }
            statusEl.dataset.version = r?.version || "";
            statusEl.title = r?.lastError || "";
            refreshBadgeText();
        }
        const detail = document.getElementById("managed-everything-detail");
        if (detail) {
            detail.textContent = r?.running
                ? t(r.indexCount > 0 ? "engine.managed.index_ready" : "engine.managed.index_empty", {count: r.indexCount ?? 0})
                : (r?.lastError || t("engine.managed.connection_auto"));
        }
        const service = document.getElementById("managed-everything-service-status");
        if (service) {
            service.textContent = t(r?.serviceInstalled ? "engine.managed.service_ready" : "engine.managed.service_missing");
            service.className = "status-badge " + (r?.serviceInstalled ? "status-available" : "status-unknown");
        }
        const uninstallBtn = document.getElementById("managed-everything-uninstall");
        if (uninstallBtn) uninstallBtn.hidden = !r?.installed;
    }
    const serviceButton = document.getElementById("managed-everything-service");
    if (serviceButton) serviceButton.disabled = !!busyOp || !r?.installed || r.serviceInstalled;
    renderActionButtons(r);
}

/** 徽章文本刷新（语言切换/状态更新共用）。 */
export function refreshBadgeText() {
    const statusEl = document.getElementById("managed-everything-status");
    if (!statusEl) return;
    const state = statusEl.dataset.badgeState;
    const version = statusEl.dataset.version || "";
    if (state === "running") {
        statusEl.textContent = t("engine.managed.status.running", {
            port: statusEl.dataset.port || "?",
        });
        statusEl.className = "status-badge status-available";
    } else if (state === "installed") {
        statusEl.textContent = t("engine.managed.status.installed", {version});
        statusEl.className = "status-badge status-unknown";
    } else if (state === "installing") {
        statusEl.textContent = t("engine.managed.status.installing");
        statusEl.className = "status-badge status-unknown";
    } else {
        statusEl.textContent = t("engine.managed.status.not_installed");
        statusEl.className = "status-badge status-unavailable";
    }
}

/** 下载进度条（downloaded 为 null 时隐藏；两张卡片共用同一事件驱动）。 */
function renderProgress(downloaded, total = null) {
    for (const id of ["managed-everything-progress", "file-search-managed-progress"]) {
        const wrap = document.getElementById(id);
        if (!wrap) continue;
        const fill = wrap.querySelector(".download-progress__fill");
        const text = wrap.querySelector(".download-progress__text");
        if (downloaded === null || downloaded === undefined) {
            wrap.hidden = true;
            continue;
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
}

// ── 操作流 ────────────────────────────────────────────────────────────────

/** 一键安装；完成收尾由 done 事件驱动（自动启动）。 */
async function installFlow() {
    if (busyOp) return;
    busyOp = "install";
    renderManagedEverything(lastStatus);
    try {
        await invoke("install_managed_everything");
    } catch (err) {
        console.error("install managed everything failed:", err);
        messageDialog(String(err?.message || err), {title: t("common.error"), kind: "error"});
        busyOp = null;
        setTransientBadge("engine.managed.status.not_installed", "status-unavailable");
        await refreshStatus();
        return;
    }
    // done 事件先到时 busyOp 已被自动启动接管（"start"），此处只兜底错过事件的情形
    if (busyOp === "install") {
        busyOp = null;
        await refreshStatus();
    }
}

/** 安装完成 → 自动启动实例（装完即用）。 */
async function autoStartAfterInstall() {
    busyOp = "start";
    setTransientBadge("engine.managed.status.starting", "status-unknown");
    renderManagedEverything(lastStatus);
    try {
        await invoke("start_managed_everything");
    } catch (err) {
        console.error("auto start managed everything failed:", err);
    }
    busyOp = null;
    await refreshStatus();
}

async function startFlow() {
    if (busyOp) return;
    busyOp = "start";
    setTransientBadge("engine.managed.status.starting", "status-unknown");
    renderManagedEverything(lastStatus);
    try {
        await invoke("start_managed_everything");
    } catch (err) {
        messageDialog(String(err?.message || err), {title: t("common.error"), kind: "error"});
    }
    busyOp = null;
    await refreshStatus();
}

async function stopFlow() {
    if (busyOp) return;
    busyOp = "stop";
    // 乐观置为 installed 徽章（与旧实现一致，语言切换路径安全），轮询在忙碌期间不覆盖
    const statusEl = document.getElementById("managed-everything-status");
    if (statusEl) {
        statusEl.dataset.badgeState = "installed";
        statusEl.dataset.version = statusEl.dataset.version || "";
        refreshBadgeText();
    }
    renderManagedEverything(lastStatus);
    try {
        await invoke("stop_managed_everything");
    } catch (err) {
        console.error("stop managed everything failed:", err);
    }
    busyOp = null;
    await refreshStatus();
}

/** 安装进度事件（stage 机：downloading/extracting/verifying/promoting/done/failed）。 */
function handleInstallEvent(payload) {
    const {stage, downloaded, total, message} = payload || {};
    if (stage === "downloading" && downloaded !== undefined) {
        busyOp = "install";
        renderProgress(downloaded, total ?? null);
        renderManagedEverything(lastStatus);
        return;
    }
    if (["extracting", "verifying", "promoting"].includes(stage)) {
        busyOp = "install";
        renderProgress(null);
        setTransientBadge(`engine.script_runtime.stage.${stage}`, "status-unknown");
        renderManagedEverything(lastStatus);
        return;
    }
    if (stage === "done") {
        renderProgress(null);
        void autoStartAfterInstall();
        return;
    }
    if (stage === "failed") {
        renderProgress(null);
        busyOp = null;
        const statusEl = document.getElementById("managed-everything-status");
        if (statusEl) {
            statusEl.dataset.badgeState = "not_installed";
            statusEl.className = "status-badge status-unavailable";
            statusEl.textContent = t("engine.managed.install_failed", {message: message || "unknown"});
            statusEl.title = message || "";
        }
        renderActionButtons(lastStatus);
    }
}

// ── 初始化 ────────────────────────────────────────────────────────────────

/** 初始化托管 Everything 区块（engines 面板 + managed-engines 面板两卡片）。 */
export function initManagedEverything({refreshStatus: refresh}) {
    refreshStatus = refresh;

    // 周期轮询（3s，仅设置窗口存活期间；统一状态一次 invoke 覆盖两枚徽章，
    // 用户实例徽章取引擎内存态不做 live 探测——live 探测归「重新探测」按钮）
    setInterval(() => {
        if (!document.hidden && document.querySelector("#engines.active, #managed-engines.active")) refreshStatus();
    }, 3000);

    // 主操作按钮（两张卡片共用一套变形逻辑）
    for (const id of ["managed-everything-action", "file-search-managed-action"]) {
        document.getElementById(id)?.addEventListener("click", async () => {
            const view = primaryView(lastStatus);
            if (view.kind === "install") await installFlow();
            else if (view.kind === "start") await startFlow();
            else if (view.kind === "stop") await stopFlow();
        });
    }

    // 卸载（低强调常驻入口，escape hatch）
    document.getElementById("managed-everything-uninstall")?.addEventListener("click", async (e) => {
        const btn = e.currentTarget;
        if (busyOp) return;
        const ok = await confirmDialog(t("engine.managed.uninstall_confirm"), {
            title: t("engine.managed.uninstall"), kind: "warning",
        });
        if (!ok) return;
        busyOp = "uninstall";
        btn.disabled = true;
        renderManagedEverything(lastStatus);
        try {
            await invoke("uninstall_managed_everything");
        } catch (err) {
            console.error("uninstall managed everything failed:", err);
            messageDialog(String(err?.message || err), {title: t("common.error"), kind: "error"});
        }
        busyOp = null;
        btn.disabled = false;
        await refreshStatus();
    });

    // UAC 服务安装：仅发起提权请求；安装后提示重启实例
    document.getElementById("managed-everything-service")?.addEventListener("click", async (e) => {
        const btn = e.currentTarget;
        const ok = await confirmDialog(t("engine.managed.service.confirm"), {
            title: t("engine.managed.service.label"), kind: "warning",
        });
        if (!ok) return;
        btn.disabled = true;
        btn.dataset.busy = "1";
        try {
            await invoke("install_everything_service");
            messageDialog(t("engine.managed.service.installed_hint"), {
                title: t("engine.managed.service.label"), kind: "info",
            });
        } catch (err) {
            messageDialog(String(err?.message || err), {
                title: t("common.error"), kind: "error",
            });
        }
        delete btn.dataset.busy;
        btn.disabled = false;
        await refreshStatus();
    });

    listen(EVENTS.MANAGED_RUNTIME_INSTALL, (event) => {
        handleInstallEvent(event.payload);
    });
    void refreshStatus();
}
