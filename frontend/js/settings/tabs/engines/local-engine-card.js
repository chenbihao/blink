/**
 * 通用引擎卡片 renderer + 日志组件（0.22.5 H3）。
 *
 * 同一个 renderer 渲染两张卡（FunASR / PaddleOCR），不按 engine_id 复制整套 DOM。
 * 通用 renderer 只处理生命周期；engine-specific 配置通过受限 adapter hook 完成。
 *
 * ## 安全铁则
 *
 * - renderer 绝不允许任意 engine id 动态注入 HTML、command 或字段路径。
 * - 图标使用现有 Lucide sprite（renderIcon / iconHTML），禁 emoji。
 * - 不新增 inline style.display；显隐使用 class / hidden / aria-expanded。
 * - 中文不斜体。
 * - 动态内容优先 textContent。
 * - 日志文本**绝不通过 innerHTML 注入**，只走 textContent。
 *
 * ## DOM contract
 *
 * 调用方提供一个容器元素，调用 `renderEngineCard(container, entry, controller, hooks)` 即可。
 * 容器内会创建以下结构（class 命名规范：`le-card-*`）：
 *
 * ```html
 * <div class="le-card" data-engine-id="funasr">
 *   <div class="le-card-header">
 *     <div class="le-card-icon"><svg class="icon">…</svg></div>
 *     <div class="le-card-info">
 *       <h3 class="le-card-title">FunASR</h3>
 *       <p class="le-card-desc">本地语音识别</p>
 *     </div>
 *     <div class="le-card-badges">…</div>
 *   </div>
 *   <div class="le-card-body">…状态区…</div>
 *   <div class="le-card-actions">…按钮区…</div>
 *   <div class="le-card-error" hidden>…错误区…</div>
 *   <div class="le-card-log" hidden>…日志区…</div>
 * </div>
 * ```
 *
 * @module local-engine-card
 */

import {renderIcon, iconHTML} from "../../../shared/icon.js";
import {copyToClipboard} from "../../../shared/api.js";
import {confirmDialog} from "../../../shared/tauri.js";
import {t} from "../../../i18n/index.js";
import {processDisplay, processClass} from "./local-engine-process.js";
import {formatLocalLogTimestamp, formatLogLine} from "./local-engine-log-format.js";
import {
    isEngineReady,
    hasActiveOperation,
    isOperationCancellable,
    getPrimaryAction,
    isActionBlocked,
    getEffectiveModelInstallState,
} from "./local-engine-state.js";

// ── 受限 adapter hook 注册表 ──────────────────────────────────────────────────

/**
 * 受限 adapter hook 注册表。
 *
 * 通用 renderer 只处理生命周期。
 * FunASR/PaddleOCR 的配置保存可通过按 engine_id 注册的受限 hook 完成，
 * 但不能复制 card lifecycle。
 *
 * hook 结构：
 * - `renderConfig(container, entry, controller)`: 渲染引擎专属配置区（可选）
 * - `onComputePreferenceChange(engineId, preference)`: compute preference 变更回调（可选）
 *
 * renderer 绝不允许任意 engine id 动态注入 HTML、command 或字段路径。
 */
const adapterHooks = new Map();

/**
 * 注册受限 adapter hook。
 * @param {string} engineId
 * @param {{renderConfig?: Function, onComputePreferenceChange?: Function}} hooks
 */
export function registerAdapterHook(engineId, hooks) {
    adapterHooks.set(engineId, hooks);
}

/**
 * 取消注册。
 * @param {string} engineId
 */
export function unregisterAdapterHook(engineId) {
    adapterHooks.delete(engineId);
}

// ── 卡片 renderer ─────────────────────────────────────────────────────────────

/**
 * 渲染单个引擎卡片到容器。
 *
 * @param {HTMLElement} container - 调用方提供的容器元素
 * @param {Object} entry - EngineStateEntry
 * @param {Object} controller - LocalEngineController
 * @param {Object} [i18n] - i18n 辅助（可选，默认用 t()）
 * @returns {void}
 */
export function renderEngineCard(container, entry, controller, i18n) {
    if (!container || !entry || !entry.catalog) return;

    const catalog = entry.catalog;
    const engineId = catalog.engine_id;

    // 检查是否已渲染（避免重复创建 DOM）
    const existing = container.querySelector(`[data-engine-id="${cssEscape(engineId)}"]`);
    if (existing) {
        // 已存在 → 只更新内容
        updateCardContent(existing, entry, controller, i18n);
        return;
    }

    // 创建卡片结构
    const card = document.createElement("div");
    card.className = "le-card extension-card";
    card.dataset.engineId = engineId;
    // tabindex=-1 使卡片可聚焦（语音页跳转 scrollIntoView 后 focus）
    card.setAttribute("tabindex", "-1");

    // ── header ────────────────────────────────────────────────────────────
    const header = document.createElement("div");
    header.className = "le-card-header extension-header";

    const iconWrap = document.createElement("div");
    iconWrap.className = "le-card-icon extension-icon";
    iconWrap.appendChild(renderIcon(catalog.icon || "cpu"));

    const info = document.createElement("div");
    info.className = "le-card-info extension-info";

    const title = document.createElement("h3");
    title.className = "le-card-title";
    title.textContent = catalog.display_name;
    info.appendChild(title);

    const desc = document.createElement("p");
    desc.className = "le-card-desc extension-desc";
    desc.textContent = catalog.description;
    info.appendChild(desc);

    header.appendChild(iconWrap);
    header.appendChild(info);

    // badges 区（能力类型 + 生命周期）
    const badges = document.createElement("div");
    badges.className = "le-card-badges";
    badges.appendChild(makeBadge(catalog.capability_kind, "le-badge-cap"));
    badges.appendChild(makeBadge(tt(i18n, `local_engine.lifecycle.${catalog.lifecycle}`, catalog.lifecycle), "le-badge-lifecycle"));
    header.appendChild(badges);

    card.appendChild(header);

    // ── body ─────────────────────────────────────────────────────────────
    const body = document.createElement("div");
    body.className = "le-card-body extension-body";

    // 四类状态：环境 / 进程 / 服务 / 模型
    body.appendChild(renderStatusGrid(entry, i18n));

    // operation 信息
    body.appendChild(renderOperationInfo(entry, i18n));

    // 计算设备三层信息
    body.appendChild(renderBackendInfo(entry, catalog, i18n));

    // 空间占用
    body.appendChild(renderStorageInfo(entry, catalog, i18n));

    card.appendChild(body);

    // ── adapter hook：引擎专属配置区 ────────────────────────────────────
    const hook = adapterHooks.get(engineId);
    if (hook && typeof hook.renderConfig === "function") {
        const configArea = document.createElement("div");
        configArea.className = "le-card-config";
        configArea.dataset.leConfigArea = engineId;
        body.appendChild(configArea);
        try {
            hook.renderConfig(configArea, entry, controller);
        } catch (e) {
            console.error(`[le-card] adapter hook renderConfig failed for ${engineId}:`, e);
        }
    }

    // ── 0.22.6: 模型列表区 ──────────────────────────────────────────────
    body.appendChild(renderModelList(entry, controller, i18n));

    // ── 0.22.6: 引擎目录 + 诊断按钮区 ────────────────────────────────────
    const toolBar = document.createElement("div");
    toolBar.className = "le-card-toolbar";

    const openDirBtn = document.createElement("button");
    openDirBtn.className = "btn btn-small le-action-btn";
    openDirBtn.type = "button";
    openDirBtn.appendChild(renderIcon("folder-open", {extraClass: "le-action-icon"}));
    const openDirLabel = document.createElement("span");
    openDirLabel.textContent = tt(i18n, "local_engine.foundation.open_engine_dir", "打开引擎目录");
    openDirBtn.appendChild(openDirLabel);
    openDirBtn.addEventListener("click", () => {
        controller?.openEngineFolder?.(engineId).catch(() => {});
    });
    toolBar.appendChild(openDirBtn);

    const diagBtn = document.createElement("button");
    diagBtn.className = "btn btn-small le-action-btn le-diagnostic-toggle";
    diagBtn.type = "button";
    diagBtn.setAttribute("aria-expanded", "false");
    diagBtn.appendChild(renderIcon("stethoscope", {extraClass: "le-action-icon"}));
    const diagLabel = document.createElement("span");
    diagLabel.textContent = tt(i18n, "local_engine.diagnostic.btn", "诊断");
    diagBtn.appendChild(diagLabel);
    diagBtn.appendChild(renderIcon("chevron-down", {extraClass: "le-action-icon le-disclosure-icon"}));
    const diagPanel = document.createElement("div");
    diagPanel.className = "le-diagnostic-inline";
    diagPanel.hidden = true;
    diagBtn.addEventListener("click", () => {
        showEngineDiagnostics(entry, controller, i18n, diagBtn, diagLabel, diagPanel);
    });
    toolBar.appendChild(diagBtn);

    body.appendChild(toolBar);
    body.appendChild(diagPanel);

    // ── actions ──────────────────────────────────────────────────────────
    const actions = document.createElement("div");
    actions.className = "le-card-actions";
    card.appendChild(actions);

    // ── error 区 ────────────────────────────────────────────────────────
    const errorArea = document.createElement("div");
    errorArea.className = "le-card-error";
    errorArea.hidden = true;
    card.appendChild(errorArea);

    // ── 日志区 ──────────────────────────────────────────────────────────
    const logArea = document.createElement("div");
    logArea.className = "le-card-log";
    logArea.hidden = true;
    logArea.appendChild(renderLogComponent(entry, controller, i18n));
    card.appendChild(logArea);

    container.appendChild(card);

    // 首次渲染后填充 actions 和 error
    updateCardContent(card, entry, controller, i18n);
}

/**
 * 更新已存在卡片的内容（不重建 DOM，只更新动态内容）。
 */
function updateCardContent(card, entry, controller, i18n) {
    const engineId = entry.catalog.engine_id;

    // 更新状态网格
    const statusGrid = card.querySelector(".le-status-grid");
    if (statusGrid) {
        updateStatusGrid(statusGrid, entry, i18n);
    }

    // 更新 operation 信息
    const opInfo = card.querySelector(".le-operation-info");
    if (opInfo) {
        updateOperationInfo(opInfo, entry, i18n);
    }

    // 更新 backend 信息
    const backendInfo = card.querySelector(".le-backend-info");
    if (backendInfo) {
        updateBackendInfo(backendInfo, entry, i18n);
    }

    // 更新 storage 信息
    const storageInfo = card.querySelector(".le-storage-info");
    if (storageInfo) {
        updateStorageInfo(storageInfo, entry, i18n);
    }

    // 更新 actions
    const actions = card.querySelector(".le-card-actions");
    if (actions) {
        updateActions(actions, entry, controller, i18n);
    }

    // 更新 error 区
    const errorArea = card.querySelector(".le-card-error");
    if (errorArea) {
        updateErrorArea(errorArea, entry, i18n);
    }

    // 更新日志区
    const logArea = card.querySelector(".le-card-log");
    if (logArea) {
        // 0.22.6: 新 operation 自动展开日志区
        // 基于 entry.logAutoExpand 标志（reducer 在新 operation 出现时设置）。
        // DOM 上记录已展开的 operation_id，防止同一 operation 重复展开。
        // 用户手动收起后不会被重新展开——因为标志只在 operation_id 跃迁时设置。
        if (entry.logAutoExpand) {
            // 确定当前活跃的 operation_id
            let currentOpId = entry.pendingAction?.operationId || null;
            if (!currentOpId && entry.pendingModelActions) {
                for (const [, pa] of entry.pendingModelActions) {
                    if (pa.operationId) {
                        currentOpId = pa.operationId;
                        break;
                    }
                }
            }
            // 如果 DOM 上记录的 opId 与当前不同 → 展开
            const alreadyExpanded = logArea.dataset.autoExpandedOp === currentOpId;
            if (!alreadyExpanded && currentOpId) {
                logArea.hidden = false;
                logArea.dataset.autoExpandedOp = currentOpId;
                // 同步日志按钮的 aria-expanded
                const logBtn = card.querySelector('.le-log-toggle');
                if (logBtn) {
                    logBtn.setAttribute('aria-expanded', 'true');
                }
            }
        }

        const logList = logArea.querySelector(".le-log-list");
        if (logList) {
            updateLogList(logList, entry, i18n);
        }
    }

    // 0.22.6: 更新模型列表
    const modelList = card.querySelector(".le-model-list");
    if (modelList) {
        updateModelList(modelList, entry, controller, i18n);
    }
}

// ── 状态网格 ──────────────────────────────────────────────────────────────────

function renderStatusGrid(entry, i18n) {
    const grid = document.createElement("div");
    grid.className = "le-status-grid";
    updateStatusGrid(grid, entry, i18n);
    return grid;
}

function updateStatusGrid(grid, entry, i18n) {
    const status = entry.status?.status;
    if (!status) {
        grid.textContent = tt(i18n, "local_engine.status.no_data", "暂无状态数据");
        return;
    }

    const items = [
        {label: tt(i18n, "local_engine.status.environment", "环境"), value: status.environment, cls: statusClass(status.environment)},
        {label: tt(i18n, "local_engine.status.process", "进程"), value: processDisplay(status.process), cls: processClass(status.process)},
        {label: tt(i18n, "local_engine.status.service", "服务"), value: status.service, cls: statusClass(status.service)},
        {label: tt(i18n, "local_engine.status.model", "模型"), value: status.model, cls: statusClass(status.model)},
    ];

    grid.textContent = "";
    for (const item of items) {
        const row = document.createElement("div");
        row.className = "le-status-item";

        const label = document.createElement("span");
        label.className = "le-status-label";
        label.textContent = item.label;

        const value = document.createElement("span");
        value.className = `le-status-value status-badge ${item.cls}`;
        value.textContent = item.value;

        row.appendChild(label);
        row.appendChild(value);
        grid.appendChild(row);
    }
}

// processDisplay / processClass 已提取到 local-engine-process.js。

function statusClass(value) {
    if (!value) return "status-unknown";
    const map = {
        missing: "status-unavailable",
        ready: "status-available",
        broken: "status-unavailable",
        needs_rebuild: "status-warning",
        unknown: "status-unknown",
        unreachable: "status-unavailable",
        healthy: "status-available",
        degraded: "status-warning",
        not_loaded: "status-unknown",
        downloading: "status-warning",
        loading: "status-warning",
        failed: "status-unavailable",
    };
    return map[value] || "status-unknown";
}

// ── operation 信息 ─────────────────────────────────────────────────────────────

function renderOperationInfo(entry, i18n) {
    const div = document.createElement("div");
    div.className = "le-operation-info";
    updateOperationInfo(div, entry, i18n);
    return div;
}

function updateOperationInfo(div, entry, i18n) {
    const op = entry.status?.status?.operation;
    if (!op || op.kind === "idle") {
        div.hidden = true;
        div.textContent = "";
        return;
    }

    div.hidden = false;
    div.textContent = "";

    const kind = document.createElement("span");
    kind.className = "le-op-kind";
    kind.textContent = tt(i18n, `local_engine.operation.${op.kind}`, op.kind);

    const stage = document.createElement("span");
    stage.className = "le-op-stage";
    stage.textContent = tt(i18n, `local_engine.operation.stage.${op.stage}`, op.stage);

    const opIdShort = op.operation_id ? `…${op.operation_id.slice(-6)}` : "";
    const idSpan = document.createElement("span");
    idSpan.className = "le-op-id";
    idSpan.textContent = opIdShort;

    div.appendChild(kind);
    div.appendChild(stage);
    div.appendChild(idSpan);
}

// ── backend 信息 ──────────────────────────────────────────────────────────────

function renderBackendInfo(entry, catalog, i18n) {
    const div = document.createElement("div");
    div.className = "le-backend-info";
    updateBackendInfo(div, entry, i18n);
    return div;
}

function updateBackendInfo(div, entry, i18n) {
    const backend = entry.status?.status?.backend;
    const catalog = entry.catalog;
    div.textContent = "";

    // requested preference
    const requested = document.createElement("div");
    requested.className = "le-backend-row";
    requested.appendChild(makeLabel(tt(i18n, "local_engine.backend.requested", "请求设备")));
    const reqVal = backend?.requested_preference || catalog?.current_compute_preference || "auto";
    requested.appendChild(makeValue(reqVal));
    div.appendChild(requested);

    // resolved profile
    const resolved = backend?.resolved_profile;
    if (resolved) {
        const row = document.createElement("div");
        row.className = "le-backend-row";
        row.appendChild(makeLabel(tt(i18n, "local_engine.backend.resolved", "解析配置")));
        row.appendChild(makeValue(resolved.profile_id || resolved.backend || "—"));
        div.appendChild(row);
    }

    // actual backend
    const verification = backend?.backend_verification;
    if (verification && verification.actual_backend) {
        const row = document.createElement("div");
        row.className = "le-backend-row";
        row.appendChild(makeLabel(tt(i18n, "local_engine.backend.actual", "实际后端")));
        row.appendChild(makeValue(verification.actual_backend));
        if (verification.device_name) {
            row.appendChild(makeValue(verification.device_name));
        }
        div.appendChild(row);
    }

    // fallback reasons
    const reasons = backend?.fallback_reasons || [];
    for (const r of reasons) {
        const row = document.createElement("div");
        row.className = "le-backend-fallback";
        row.appendChild(makeLabel(tt(i18n, "local_engine.backend.fallback", "回退原因")));
        const detail = `${r.rejected_profile}: ${r.reason}`;
        row.appendChild(makeValue(detail));
        div.appendChild(row);
    }
}

// ── 存储信息 ──────────────────────────────────────────────────────────────────

function renderStorageInfo(entry, catalog, i18n) {
    const div = document.createElement("div");
    div.className = "le-storage-info";
    updateStorageInfo(div, entry, i18n);
    return div;
}

function updateStorageInfo(div, entry, i18n) {
    const storage = entry.storage;
    const catalog = entry.catalog;
    div.textContent = "";

    // 预计空间（来自 catalog resource_budget）
    const budget = catalog?.resource_budget;
    if (budget) {
        const envDisk = budget.estimated_env_disk_mb;
        const modelDisk = budget.estimated_model_disk_mb;
        if (envDisk != null || modelDisk != null) {
            const row = document.createElement("div");
            row.className = "le-storage-estimated";
            row.appendChild(makeLabel(tt(i18n, "local_engine.storage.estimated", "预计空间")));
            const parts = [];
            if (envDisk != null) parts.push(`${formatMB(envDisk)}`);
            if (modelDisk != null) parts.push(`${formatMB(modelDisk)} (${tt(i18n, "local_engine.storage.model", "模型")})`);
            row.appendChild(makeValue(parts.join(" + ")));
            div.appendChild(row);
        }
    }

    // 实际空间（来自 storage snapshot）
    if (storage) {
        const total = formatBytes(storage.total_size_bytes);
        const releasable = formatBytes(storage.releasable_size_bytes);
        const row = document.createElement("div");
        row.className = "le-storage-actual";
        row.appendChild(makeLabel(tt(i18n, "local_engine.storage.actual", "实际占用")));
        row.appendChild(makeValue(total));
        if (storage.releasable_size_bytes > 0) {
            const rel = document.createElement("span");
            rel.className = "le-storage-releasable";
            rel.textContent = `${tt(i18n, "local_engine.storage.releasable", "可释放")} ${releasable}`;
            row.appendChild(rel);
        }
        div.appendChild(row);
    }
}

// ── actions 按钮 ───────────────────────────────────────────────────────────────

function updateActions(container, entry, controller, i18n) {
    // 脏检查：下载等高频状态推送期间，按钮组的外观输入未变时跳过重建。
    // 全量重建会丢失 hover/press 状态（闪烁、点击落空）和日志按钮的
    // aria-expanded ——每条日志事件都会走到这里，重建必须杜绝。
    const primary = getPrimaryAction(entry);
    const sigParts = [`primary=${primary}`];
    for (const kind of ["install", "start", "stop", "repair", "cleanup"]) {
        sigParts.push(`${kind}=${isActionBlocked(entry, kind)}`);
    }
    sigParts.push(`cancel=${isOperationCancellable(entry)}`);
    const sig = sigParts.join("|");
    if (container.dataset.renderSig === sig) return;
    container.dataset.renderSig = sig;

    container.textContent = "";

    const buttons = [
        {kind: "install", label: tt(i18n, "local_engine.action.install", "安装"), icon: "download"},
        {kind: "start", label: tt(i18n, "local_engine.action.start", "启动"), icon: "play"},
        {kind: "stop", label: tt(i18n, "local_engine.action.stop", "停止"), icon: "square"},
        {kind: "repair", label: tt(i18n, "local_engine.action.repair", "修复"), icon: "wrench"},
        {kind: "cleanup", label: tt(i18n, "local_engine.action.cleanup", "清理"), icon: "trash-2"},
        {kind: "cancel", label: tt(i18n, "local_engine.action.cancel", "取消"), icon: "x"},
    ];

    for (const btn of buttons) {
        const isPrimary = primary === btn.kind;
        const blocked = isActionBlocked(entry, btn.kind);

        // 决定按钮显隐
        if (!isPrimary && btn.kind !== "cancel" && btn.kind !== "log" && btn.kind !== "cleanup") {
            // 只显示 primary + cancel + cleanup
            if (btn.kind !== "cleanup") continue;
        }
        if (btn.kind === "cancel" && !isOperationCancellable(entry)) {
            continue;
        }

        const el = document.createElement("button");
        el.className = isPrimary ? "btn btn-primary btn-small le-action-btn" : "btn btn-small le-action-btn";
        el.dataset.actionKind = btn.kind;
        el.disabled = blocked;
        el.appendChild(renderIcon(btn.icon, {extraClass: "le-action-icon"}));
        const labelSpan = document.createElement("span");
        labelSpan.textContent = btn.label;
        el.appendChild(labelSpan);

        el.addEventListener("click", () => {
            handleActionClick(btn.kind, entry, controller, i18n);
        });

        container.appendChild(el);
    }

    // 日志按钮（独立 toggle）
    const logBtn = document.createElement("button");
    logBtn.className = "btn btn-small le-action-btn le-log-toggle";
    logBtn.dataset.actionKind = "log";
    logBtn.appendChild(renderIcon("terminal", {extraClass: "le-action-icon"}));
    const logLabel = document.createElement("span");
    logLabel.textContent = tt(i18n, "local_engine.action.logs", "日志");
    logBtn.appendChild(logLabel);
    logBtn.addEventListener("click", () => {
        const logArea = container.closest(".le-card")?.querySelector(".le-card-log");
        if (logArea) {
            const isHidden = logArea.hidden;
            logArea.hidden = !isHidden;
            logBtn.setAttribute("aria-expanded", String(isHidden));
        }
    });
    container.appendChild(logBtn);
}

function handleActionClick(kind, entry, controller, i18n) {
    const engineId = entry.catalog.engine_id;

    switch (kind) {
        case "install":
            // 不传 compute_preference：由后端从配置真源构造 AdapterConfig
            // 前端 catalog.current_compute_preference 可能是过期快照
            controller.install(engineId, null).catch(() => {});
            break;
        case "start":
            // 不传 compute_preference：由后端从配置真源构造 AdapterConfig
            controller.start(engineId, null).catch(() => {});
            break;
        case "stop":
            controller.stop(engineId).catch(() => {});
            break;
        case "repair":
            controller.repair(engineId).catch(() => {});
            break;
        case "cleanup":
            // 清理只打开 cleanup modal，绝不直接 invoke cleanup。
            // modal 从最新 storage DTO 渲染精确 targets，confirm 时只提交选中的 target_id。
            // 实际 modal 控制由 settings/index.js 注入的 onCleanupOpen 回调完成。
            if (i18n && typeof i18n.onCleanupOpen === "function") {
                i18n.onCleanupOpen(engineId, entry, controller);
            }
            break;
        case "cancel":
            if (entry.status?.status?.operation?.operation_id) {
                controller.cancel(engineId, entry.status.status.operation.operation_id).catch(() => {});
            }
            break;
    }
}

// ── 错误展示 ──────────────────────────────────────────────────────────────────

function updateErrorArea(container, entry, i18n) {
    const error = entry.status?.status?.last_error;
    if (!error) {
        container.hidden = true;
        container.textContent = "";
        return;
    }

    container.hidden = false;
    container.textContent = "";

    // 根据 stable code/phase/i18n 映射用户文案
    const mainText = error.action_hint || error.message || tt(i18n, `local_engine.error.${error.code}`, error.code || "unknown_error");
    const main = document.createElement("div");
    main.className = "le-error-main";
    main.textContent = mainText;
    container.appendChild(main);

    // action_hint 展示
    if (error.action_hint && error.action_hint !== mainText) {
        const hint = document.createElement("div");
        hint.className = "le-error-hint";
        hint.textContent = error.action_hint;
        container.appendChild(hint);
    }

    // detail 放可展开诊断区
    const detail = error.detail || (error.phase ? `[${error.phase}]` : "");
    if (detail) {
        const detailArea = document.createElement("details");
        detailArea.className = "le-error-detail";
        const summary = document.createElement("summary");
        summary.textContent = tt(i18n, "local_engine.error.detail", "诊断详情");
        detailArea.appendChild(summary);
        const pre = document.createElement("pre");
        pre.textContent = detail; // textContent，不 innerHTML
        detailArea.appendChild(pre);

        // 复制诊断按钮
        const copyBtn = document.createElement("button");
        copyBtn.className = "btn btn-small le-error-copy";
        copyBtn.textContent = tt(i18n, "local_engine.error.copy", "复制诊断");
        copyBtn.addEventListener("click", () => copyTextWithFeedback(copyBtn, detail, i18n));
        detailArea.appendChild(copyBtn);

        container.appendChild(detailArea);
    }
}

// ── 0.22.6: 模型状态 i18n 映射 ────────────────────────────────────────────────

const MODEL_STATE_I18N_PREFIX = "local_engine.model.state.";
const MODEL_VERIFICATION_I18N_PREFIX = "local_engine.model.verification.";
const MODEL_COMPATIBILITY_I18N_PREFIX = "local_engine.model.compatibility.";

/**
 * 将 install_state wire value 映射为 i18n 文案。
 * wire value 是 snake_case（如 "not_installed"、"download_failed"）。
 */
function modelStateLabel(installState) {
    if (!installState) return "—";
    const key = `${MODEL_STATE_I18N_PREFIX}${installState}`;
    const label = t(key);
    return label !== key ? label : installState;
}

/**
 * 将 verification_state wire value 映射为 i18n 文案。
 */
function modelVerificationLabel(verificationState) {
    if (!verificationState) return "";
    const key = `${MODEL_VERIFICATION_I18N_PREFIX}${verificationState}`;
    const label = t(key);
    return label !== key ? label : verificationState;
}

// ── 0.22.6: 模型列表渲染 ─────────────────────────────────────────────────────

/**
 * 渲染模型列表区。
 * @param {Object} entry - EngineStateEntry
 * @param {Object} controller
 * @param {Object} i18n
 * @returns {HTMLElement}
 */
function renderModelList(entry, controller, i18n) {
    const div = document.createElement("div");
    div.className = "le-model-list";
    updateModelList(div, entry, controller, i18n);
    return div;
}

/**
 * 更新模型列表内容。
 * @param {HTMLElement} container
 * @param {Object} entry
 * @param {Object} controller
 * @param {Object} i18n
 */
function updateModelList(container, entry, controller, i18n) {
    // 脏检查：模型行的外观输入（模型集合 + 各自安装/校验/选中状态 + pending
    // 操作）未变时跳过重建——下载期间高频状态推送不应重建模型行按钮
    // （hover 闪烁、点击落空）。
    const models = entry.models;
    const sigParts = [];
    for (const model of models || []) {
        sigParts.push(
            `${model.model_id}:${getEffectiveModelInstallState(entry, model)}`
            + `:${model.verification_state}:${model.is_selected ? 1 : 0}`
            + `:${model.is_active ? 1 : 0}`
        );
    }
    const sig = sigParts.join("|");
    if (container.dataset.renderSig === sig) return;
    container.dataset.renderSig = sig;

    container.textContent = "";

    if (!models || models.length === 0) {
        const empty = document.createElement("div");
        empty.className = "le-model-empty";
        empty.textContent = tt(i18n, "local_engine.model.list.empty", "暂无模型候选");
        container.appendChild(empty);
        return;
    }

    const title = document.createElement("div");
    title.className = "le-model-list-title";
    title.textContent = tt(i18n, "local_engine.model.list.title", "模型");
    container.appendChild(title);

    const table = document.createElement("div");
    table.className = "le-model-table";

    for (const model of models) {
        const row = document.createElement("div");
        row.className = "le-model-row";

        // 模型名
        const nameCell = document.createElement("div");
        nameCell.className = "le-model-cell le-model-name";
        nameCell.textContent = model.display_name || model.model_id;
        // 标记 selected/active
        if (model.is_selected) {
            const badge = document.createElement("span");
            badge.className = "le-badge le-badge-cap";
            badge.textContent = tt(i18n, "local_engine.model.configured", "配置");
            badge.style.marginLeft = "0.375rem";
            nameCell.appendChild(badge);
        }
        if (model.is_active) {
            const badge = document.createElement("span");
            badge.className = "le-badge le-badge-lifecycle";
            badge.textContent = tt(i18n, "local_engine.model.active", "实际加载");
            badge.style.marginLeft = "0.375rem";
            nameCell.appendChild(badge);
        }
        row.appendChild(nameCell);

        // 体积
        const sizeCell = document.createElement("div");
        sizeCell.className = "le-model-cell le-model-size";
        sizeCell.textContent = model.estimated_size_mb != null
            ? formatMB(model.estimated_size_mb)
            : "—";
        row.appendChild(sizeCell);

        // 状态
        const statusCell = document.createElement("div");
        statusCell.className = "le-model-cell le-model-status";
        const stateText = modelStateLabel(getEffectiveModelInstallState(entry, model));
        statusCell.textContent = stateText;
        // 校验状态附加
        const verText = modelVerificationLabel(model.verification_state);
        if (verText && verText !== model.verification_state) {
            const verSpan = document.createElement("span");
            verSpan.style.marginLeft = "0.25rem";
            verSpan.style.fontSize = "var(--text-2xs)";
            verSpan.style.color = "var(--text-dim)";
            verSpan.textContent = `(${verText})`;
            statusCell.appendChild(verSpan);
        }
        row.appendChild(statusCell);

        // 操作按钮
        const actionsCell = document.createElement("div");
        actionsCell.className = "le-model-cell le-model-actions";
        actionsCell.appendChild(renderModelActions(model, entry, controller, i18n));
        row.appendChild(actionsCell);

        table.appendChild(row);
    }

    container.appendChild(table);
}

/**
 * 渲染单个模型的操作按钮。
 * 根据 install_state 决定可用操作。
 */
function renderModelActions(model, entry, controller, i18n) {
    const frag = document.createDocumentFragment();
    const state = getEffectiveModelInstallState(entry, model);
    const engineId = model.engine_id;
    const modelId = model.model_id;

    // NotInstalled → 下载
    if (state === "not_installed" || state === "download_failed"
        || state === "staging_failed" || state === "verification_failed") {
        const btn = document.createElement("button");
        btn.className = "le-model-btn le-model-btn-primary";
        btn.type = "button";
        btn.appendChild(renderIcon("download", {extraClass: "le-action-icon"}));
        const label = document.createElement("span");
        label.textContent = tt(i18n, "local_engine.model.action.download", "下载");
        btn.appendChild(label);
        btn.addEventListener("click", () => {
            // 确认下载（confirmDialog：原生 confirm 在 Tauri 2 WebView2 下被
            // tauri.js 拦截抛错，绝不可用）
            const sizeText = model.estimated_size_mb != null
                ? formatMB(model.estimated_size_mb)
                : "—";
            const confirmMsg = tt(i18n,
                "local_engine.model.action.download_confirm_desc",
                "预计体积 {size}，下载过程中可取消。是否开始下载？", {size: sizeText});
            confirmDialog(confirmMsg, {kind: "info"}).then((ok) => {
                if (!ok) return;
                controller?.installModel?.(engineId, modelId).catch(() => {});
            });
        });
        frag.appendChild(btn);
    }

    // Downloading/Staging/Verifying/Repairing/Deleting → 取消
    if (["downloading", "staging", "verifying", "repairing", "deleting"].includes(state)) {
        const btn = document.createElement("button");
        btn.className = "le-model-btn";
        btn.type = "button";
        btn.appendChild(renderIcon("x", {extraClass: "le-action-icon"}));
        const label = document.createElement("span");
        label.textContent = tt(i18n, "local_engine.model.action.cancel", "取消");
        btn.appendChild(label);
        btn.addEventListener("click", () => {
            // 从 pending model action 中获取真实 operationId
            const pendingActions = entry.pendingModelActions || new Map();
            const pending = pendingActions.get(modelId);
            const opId = pending?.operationId || "";
            if (!opId) {
                console.warn("[local-engine] cancel: no operationId for", modelId);
                return;
            }
            controller?.cancelModelOperation?.(engineId, modelId, opId).catch(() => {});
        });
        frag.appendChild(btn);
    }

    // Installed → 修复 + 删除
    if (state === "installed") {
        const repairBtn = document.createElement("button");
        repairBtn.className = "le-model-btn";
        repairBtn.type = "button";
        repairBtn.appendChild(renderIcon("wrench", {extraClass: "le-action-icon"}));
        const repairLabel = document.createElement("span");
        repairLabel.textContent = tt(i18n, "local_engine.model.action.repair", "修复");
        repairBtn.appendChild(repairLabel);
        repairBtn.addEventListener("click", () => {
            controller?.repairModel?.(engineId, modelId).catch(() => {});
        });
        frag.appendChild(repairBtn);

        const delBtn = document.createElement("button");
        delBtn.className = "le-model-btn le-model-btn-danger";
        delBtn.type = "button";
        delBtn.appendChild(renderIcon("trash-2", {extraClass: "le-action-icon"}));
        const delLabel = document.createElement("span");
        delLabel.textContent = tt(i18n, "local_engine.model.action.delete", "删除");
        delBtn.appendChild(delLabel);
        delBtn.addEventListener("click", () => {
            const confirmMsg = tt(i18n,
                "local_engine.model.action.delete_confirm_desc",
                "删除后将释放缓存空间，无法撤销。");
            confirmDialog(confirmMsg, {kind: "warning"}).then((ok) => {
                if (!ok) return;
                controller?.deleteModel?.(engineId, modelId).catch((err) => {
                    // 结构化冲突展示
                    showModelDeleteConflict(err, engineId, modelId, i18n, delBtn);
                });
            });
        });
        frag.appendChild(delBtn);
    }

    // delete_blocked → 不可操作
    if (state === "delete_blocked") {
        const span = document.createElement("span");
        span.className = "le-model-status";
        span.style.color = "var(--red)";
        span.textContent = tt(i18n, "local_engine.model.state.delete_blocked", "删除被阻止");
        frag.appendChild(span);
    }

    return frag;
}

/**
 * 展示删除模型的结构化冲突。
 * 从 CommandError 的 detail 字段解析 ModelDeleteConflictDto。
 */
function showModelDeleteConflict(err, engineId, modelId, i18n, anchorEl) {
    // CommandError shape: { code, message, detail, retryable }
    const detail = err?.detail;
    if (!detail || typeof detail !== "object") {
        // 直接展示 message
        const conflict = document.createElement("div");
        conflict.className = "le-model-conflict";
        const title = document.createElement("div");
        title.className = "le-model-conflict-title";
        title.textContent = tt(i18n, "local_engine.model.conflict.title", "无法删除");
        conflict.appendChild(title);
        const reason = document.createElement("div");
        reason.className = "le-model-conflict-reason";
        reason.textContent = err?.message || String(err);
        conflict.appendChild(reason);
        anchorEl.parentElement?.insertBefore(conflict, anchorEl.nextSibling);
        return;
    }

    const conflict = document.createElement("div");
    conflict.className = "le-model-conflict";
    const title = document.createElement("div");
    title.className = "le-model-conflict-title";
    title.textContent = tt(i18n, "local_engine.model.conflict.title", "无法删除");
    conflict.appendChild(title);

    // detail.reasons 是 DeleteConflictReasonDto 数组
    const reasons = detail.reasons || [];
    for (const r of reasons) {
        const reasonEl = document.createElement("div");
        reasonEl.className = "le-model-conflict-reason";
        if (r.referenced_by_config) {
            reasonEl.textContent = tt(i18n,
                "local_engine.model.conflict.referenced_by_config",
                "配置 {field}={value} 引用了此模型",
                {field: r.referenced_by_config.config_field, value: r.referenced_by_config.config_value});
        } else if (r.active_in_running_instance) {
            reasonEl.textContent = tt(i18n,
                "local_engine.model.conflict.active_in_instance",
                "运行实例 {instance} 正在使用此模型",
                {instance: r.active_in_running_instance.instance_id});
        } else if (r.referenced_by_descriptor) {
            reasonEl.textContent = tt(i18n,
                "local_engine.model.conflict.referenced_by_descriptor",
                "引擎默认模型 {model} 引用了此模型",
                {model: r.referenced_by_descriptor.descriptor_model_id});
        } else {
            reasonEl.textContent = JSON.stringify(r);
        }
        conflict.appendChild(reasonEl);
    }

    // 插入到 anchor 元素后面
    anchorEl.parentElement?.insertBefore(conflict, anchorEl.nextSibling);
}

/**
 * 展示引擎诊断信息。
 * 从后端 get_engine_diagnostics 拉取并展示；面板内提供"重新诊断"按钮。
 */
function showEngineDiagnostics(entry, controller, i18n, anchorBtn, label, diagPanel) {
    const engineId = entry.catalog?.engine_id;
    if (!engineId || !controller?.getDiagnostics) return;

    if (!diagPanel.hidden) {
        diagPanel.hidden = true;
        anchorBtn.setAttribute("aria-expanded", "false");
        label.textContent = tt(i18n, "local_engine.diagnostic.btn", "诊断");
        return;
    }

    diagPanel.hidden = false;
    anchorBtn.setAttribute("aria-expanded", "true");
    label.textContent = tt(i18n, "local_engine.diagnostic.collapse", "收起诊断");
    refreshEngineDiagnostics(entry, controller, i18n, diagPanel);
}

/**
 * 拉取并渲染诊断内容（可重复调用——"重新诊断"按钮复用）。
 */
function refreshEngineDiagnostics(entry, controller, i18n, diagPanel) {
    const engineId = entry.catalog?.engine_id;
    diagPanel.textContent = "";
    const requestId = String((Number(diagPanel.dataset.requestId) || 0) + 1);
    diagPanel.dataset.requestId = requestId;

    const loading = document.createElement("div");
    loading.className = "le-diagnostic-loading";
    loading.textContent = tt(i18n, "local_engine.diagnostic.loading", "诊断中…");
    diagPanel.appendChild(loading);

    controller.getDiagnostics(engineId).then((diag) => {
        if (diagPanel.hidden || diagPanel.dataset.requestId !== requestId) return;
        renderDiagnosticContent(diagPanel, diag, i18n, engineId, entry, () => {
            refreshEngineDiagnostics(entry, controller, i18n, diagPanel);
        });
    }).catch((e) => {
        if (diagPanel.hidden || diagPanel.dataset.requestId !== requestId) return;
        const errEl = document.createElement("div");
        errEl.className = "le-diagnostic-error";
        errEl.textContent = e?.message || String(e);
        diagPanel.appendChild(errEl);
    });
}

/**
 * 渲染诊断内容。
 */
function renderDiagnosticContent(container, diag, i18n, engineId, entry, onRefresh) {
    container.textContent = "";

    // 头部：标题 + 重新诊断按钮（诊断是快照——状态变化后需手动重新拉取）
    const header = document.createElement("div");
    header.className = "le-diagnostic-header";
    const title = document.createElement("span");
    title.className = "le-diagnostic-title";
    title.textContent = tt(i18n, "local_engine.diagnostic.title", "引擎诊断");
    header.appendChild(title);
    if (typeof onRefresh === "function") {
        const refreshBtn = document.createElement("button");
        refreshBtn.className = "btn btn-small le-action-btn";
        refreshBtn.type = "button";
        refreshBtn.appendChild(renderIcon("refresh-cw", {extraClass: "le-action-icon"}));
        const refreshLabel = document.createElement("span");
        refreshLabel.textContent = tt(i18n, "local_engine.diagnostic.refresh", "重新诊断");
        refreshBtn.appendChild(refreshLabel);
        refreshBtn.addEventListener("click", onRefresh);
        header.appendChild(refreshBtn);
    }
    container.appendChild(header);

    // 基本信息
    const grid = document.createElement("div");
    grid.className = "le-diagnostic-grid";

    const items = [
        {label: tt(i18n, "local_engine.diagnostic.env", "环境"), value: diag.environment || "—"},
        {label: tt(i18n, "local_engine.diagnostic.process", "进程"), value: diag.process?.state || "—"},
        {label: tt(i18n, "local_engine.diagnostic.service", "服务"), value: diag.service || "—"},
    ];

    for (const item of items) {
        const row = document.createElement("div");
        row.className = "le-diagnostic-item";
        const label = document.createElement("span");
        label.className = "le-info-label";
        label.textContent = item.label;
        const value = document.createElement("span");
        value.className = "le-info-value";
        value.textContent = item.value;
        row.appendChild(label);
        row.appendChild(value);
        grid.appendChild(row);
    }
    container.appendChild(grid);

    // 最近日志：后端只保存引擎 server 进程日志——引擎从未运行（如安装模型/
    // 启动失败排查场景）时为空。此时回退到前端实时积累的日志（含安装/
    // 操作日志），保证诊断面板始终有可看的内容。
    let logs = diag.recent_logs || [];
    let logsSource = "server";
    if (logs.length === 0 && entry?.logs?.length > 0) {
        logs = entry.logs.slice(-50).map((l) => ({
            timestamp: l.timestamp,
            level: l.level,
            text: l.text,
        }));
        logsSource = "session";
    }
    if (logs.length > 0) {
        const logsTitle = document.createElement("div");
        logsTitle.className = "le-diagnostic-logs-title";
        logsTitle.textContent = tt(i18n, "local_engine.diagnostic.recent_logs", "最近日志")
            + (logsSource === "session"
                ? tt(i18n, "local_engine.diagnostic.logs_from_session", "（本次会话）")
                : "");
        container.appendChild(logsTitle);

        const logsList = document.createElement("div");
        logsList.className = "le-diagnostic-logs-list";
        for (const log of logs) {
            const line = document.createElement("div");
            line.className = "le-log-line";
            const time = document.createElement("span");
            time.className = "le-log-time";
            time.textContent = formatLocalLogTimestamp(log.timestamp);
            time.title = log.timestamp || "";
            const level = document.createElement("span");
            level.className = "le-log-level";
            level.textContent = log.level || "info";
            const text = document.createElement("span");
            text.className = "le-log-text";
            text.textContent = log.text;
            line.appendChild(time);
            line.appendChild(level);
            line.appendChild(text);
            logsList.appendChild(line);
        }
        container.appendChild(logsList);
    } else {
        const noLogs = document.createElement("div");
        noLogs.className = "le-diagnostic-logs-empty";
        noLogs.textContent = tt(i18n, "local_engine.diagnostic.no_logs", "无日志");
        container.appendChild(noLogs);
    }

    // 复制诊断按钮
    const copyBtn = document.createElement("button");
    copyBtn.className = "btn btn-small le-diagnostic-copy";
    copyBtn.textContent = tt(i18n, "local_engine.diagnostic.copy", "复制诊断");
    copyBtn.addEventListener("click", () => {
        const text = JSON.stringify(diag, null, 2);
        copyTextWithFeedback(copyBtn, text, i18n);
    });
    container.appendChild(copyBtn);

    // ── 0.22.6: 停止孤儿引擎按钮 ──────────────────────────────────
    // 后端提供闭合 DTO { present, actionable, reason }，
    // 前端只检查 actionable === true 来决定是否显示停止入口。
    // 不拼接 process/service，不检查 legacy_venv_exists。
    const orphanRecovery = diag.orphan_recovery;
    if (orphanRecovery && orphanRecovery.actionable === true && controller?.stopOrphan) {
        const orphanBtn = document.createElement("button");
        orphanBtn.className = "btn btn-small le-action-btn le-orphan-stop";
        orphanBtn.type = "button";
        orphanBtn.appendChild(renderIcon("ghost", {extraClass: "le-action-icon"}));
        const orphanLabel = document.createElement("span");
        orphanLabel.textContent = tt(i18n, "local_engine.diagnostic.stop_orphan", "停止孤儿进程");
        orphanBtn.appendChild(orphanLabel);
        orphanBtn.addEventListener("click", () => {
            // 使用确认模态，不使用原生 confirm()
            const confirmMsg = tt(i18n,
                "local_engine.diagnostic.stop_orphan_confirm",
                "将终止遗留的引擎进程并清理 lease。是否继续？");
            showOrphanConfirmModal(confirmMsg, () => {
                // 防重入：per-engine in-flight
                if (orphanBtn.dataset.inFlight === "true") return;
                orphanBtn.dataset.inFlight = "true";
                orphanBtn.disabled = true;
                controller.stopOrphan(engineId).then((result) => {
                    orphanBtn.disabled = false;
                    orphanBtn.dataset.inFlight = "false";
                    // 使用通知机制，不使用 alert()
                    const msg = result?.stopped
                        ? tt(i18n, "local_engine.diagnostic.orphan_stopped", "孤儿进程已终止")
                        : tt(i18n, "local_engine.diagnostic.orphan_not_stopped", "未能终止进程：{reason}", {reason: result?.reason || "unknown"});
                    showOrphanNotification(msg, result?.stopped !== false);
                    // 刷新 diagnostics 和 status
                    if (typeof controller.refreshStatus === "function") {
                        controller.refreshStatus().catch(() => {});
                    }
                }).catch((e) => {
                    orphanBtn.disabled = false;
                    orphanBtn.dataset.inFlight = "false";
                    const errMsg = e?.message || String(e);
                    showOrphanNotification(
                        tt(i18n, "local_engine.diagnostic.orphan_error", "操作失败：{error}", {error: errMsg}),
                        false
                    );
                });
            });
        });
        container.appendChild(orphanBtn);
    }
}

// ── 日志组件 ──────────────────────────────────────────────────────────────────

function renderLogComponent(entry, controller, i18n) {
    const wrapper = document.createElement("div");
    wrapper.className = "le-log-wrapper";

    // 工具栏
    const toolbar = document.createElement("div");
    toolbar.className = "le-log-toolbar";

    const instanceLabel = document.createElement("span");
    instanceLabel.className = "le-log-instance";
    instanceLabel.textContent = entry.currentInstanceId
        ? `${tt(i18n, "local_engine.log.instance", "实例")}: …${entry.currentInstanceId.slice(-8)}`
        : tt(i18n, "local_engine.log.no_instance", "无实例");
    toolbar.appendChild(instanceLabel);

    const copyBtn = document.createElement("button");
    copyBtn.className = "btn btn-small le-log-copy";
    copyBtn.textContent = tt(i18n, "local_engine.log.copy", "复制");
    copyBtn.addEventListener("click", () => {
        const text = entry.logs.map(formatLogLine).join("\n");
        copyTextWithFeedback(copyBtn, text, i18n);
    });
    toolbar.appendChild(copyBtn);

    const clearBtn = document.createElement("button");
    clearBtn.className = "btn btn-small le-log-clear";
    clearBtn.textContent = tt(i18n, "local_engine.log.clear", "清空");
    clearBtn.addEventListener("click", () => {
        controller.clearLogBuffer(entry.catalog.engine_id);
    });
    toolbar.appendChild(clearBtn);

    wrapper.appendChild(toolbar);

    // 日志列表
    const list = document.createElement("div");
    list.className = "le-log-list";
    updateLogList(list, entry, i18n);
    wrapper.appendChild(list);

    return wrapper;
}

function updateLogList(list, entry, i18n) {
    // 脏检查：日志集合未变时跳过（长度 + 尾行 source:seq 唯一标识；
    // 截断丢头部时尾行 seq 变化仍会触发重建）。
    const logs = entry.logs;
    const last = logs[logs.length - 1];
    const sig = `${logs.length}:${last ? `${last.source}:${last.seq}` : ""}`;
    if (list.dataset.renderSig === sig) return;
    list.dataset.renderSig = sig;

    // 用户上翻查看历史日志时不要强制拉底——仅当已停在底部附近才跟随滚动。
    const nearBottom = list.scrollHeight - list.scrollTop - list.clientHeight < 40;

    list.textContent = "";

    if (logs.length === 0) {
        const empty = document.createElement("div");
        empty.className = "le-log-empty";
        empty.textContent = tt(i18n, "local_engine.log.empty", "暂无日志");
        list.appendChild(empty);
        return;
    }

    // instance 分隔线
    let lastInstance = null;
    for (const log of logs) {
        // instance 切换时插入分隔线
        if (lastInstance !== null && lastInstance !== log.instanceId) {
            const sep = document.createElement("div");
            sep.className = "le-log-separator";
            sep.textContent = `── ${tt(i18n, "local_engine.log.instance_changed", "实例切换")} ──`;
            list.appendChild(sep);
        }
        lastInstance = log.instanceId;

        const line = document.createElement("div");
        line.className = `le-log-line le-log-${log.level || "info"}`;

        const time = document.createElement("span");
        time.className = "le-log-time";
        time.textContent = formatLocalLogTimestamp(log.timestamp);
        time.title = log.timestamp || "";

        const level = document.createElement("span");
        level.className = "le-log-level";
        level.textContent = log.level || "info";

        const text = document.createElement("span");
        text.className = "le-log-text";
        text.textContent = log.text; // textContent，绝不 innerHTML

        line.appendChild(time);
        line.appendChild(level);
        line.appendChild(text);
        list.appendChild(line);
    }

    // 滚动到底部（仅当更新前已在底部附近）
    if (nearBottom) {
        list.scrollTop = list.scrollHeight;
    }
}

/**
 * 通过后端剪贴板 command 复制文本。
 * WebView 中 navigator.clipboard 可能因权限/焦点被静默拒绝，不能作为可靠路径。
 */
async function copyTextWithFeedback(button, text, i18n) {
    if (!text || button.dataset.copying === "true") return;
    const original = button.textContent;
    button.dataset.copying = "true";
    button.disabled = true;
    try {
        await copyToClipboard(text);
        button.textContent = tt(i18n, "local_engine.log.copied", "已复制");
    } catch (error) {
        console.error("[local-engine] copy failed:", error);
        button.textContent = tt(i18n, "local_engine.log.copy_failed", "复制失败");
    } finally {
        window.setTimeout(() => {
            button.textContent = original;
            button.disabled = false;
            button.dataset.copying = "false";
        }, 1200);
    }
}

// ── 辅助函数 ──────────────────────────────────────────────────────────────────

// processDisplay / processClass 已提取到 local-engine-process.js。

function makeBadge(text, cls) {
    const badge = document.createElement("span");
    badge.className = `le-badge ${cls} status-badge`;
    badge.textContent = text;
    return badge;
}

function makeLabel(text) {
    const span = document.createElement("span");
    span.className = "le-info-label";
    span.textContent = text;
    return span;
}

function makeValue(text) {
    const span = document.createElement("span");
    span.className = "le-info-value";
    span.textContent = text;
    return span;
}

function tt(i18n, key, fallback, params) {
    if (i18n && typeof i18n.t === "function") return i18n.t(key, params) || fallback;
    const raw = t(key, params);
    return raw !== key ? raw : fallback;
}

function formatBytes(bytes) {
    if (!bytes || bytes === 0) return "0 B";
    const mb = bytes / (1024 * 1024);
    if (mb < 1024) return `${Math.round(mb)} MB`;
    return `${(mb / 1024).toFixed(1)} GB`;
}

function formatMB(mb) {
    if (mb == null) return "—";
    if (mb < 1024) return `${mb} MB`;
    return `${(mb / 1024).toFixed(1)} GB`;
}

function cssEscape(s) {
    return String(s).replace(/[^a-zA-Z0-9_-]/g, (c) => `\\${c}`);
}

// ── 0.22.6: orphan 确认模态 + 通知 ──────────────────────────────────────────

/**
 * 显示确认模态（不使用原生 confirm()）。
 * 使用已有的 overlay/modal 样式，模拟 cleanup modal 的行为。
 * @param {string} message - 确认文案
 * @param {Function} onConfirm - 用户确认时回调
 */
function showOrphanConfirmModal(message, onConfirm) {
    // 查找或创建 overlay
    let overlay = document.querySelector(".le-orphan-modal-overlay");
    if (overlay) {
        overlay.remove();
    }

    overlay = document.createElement("div");
    overlay.className = "le-orphan-modal-overlay";
    overlay.style.cssText =
        "position:fixed;inset:0;display:flex;align-items:center;justify-content:center;" +
        "background:rgba(0,0,0,0.4);z-index:10000;";

    const modal = document.createElement("div");
    modal.className = "le-orphan-modal";
    modal.style.cssText =
        "max-width:400px;padding:1.5rem;border-radius:var(--radius-md,8px);" +
        "background:var(--bg-card,#1e1e2e);border:1px solid var(--surface);" +
        "font-style:normal;text-align:center;";

    const msg = document.createElement("p");
    msg.style.cssText = "margin:0 0 1rem 0;color:var(--text);font-size:var(--text-sm);";
    msg.textContent = message;
    modal.appendChild(msg);

    const btnRow = document.createElement("div");
    btnRow.style.cssText = "display:flex;gap:0.5rem;justify-content:center;";

    const cancelBtn = document.createElement("button");
    cancelBtn.className = "btn btn-small";
    cancelBtn.textContent = tt(null, "local_engine.diagnostic.cancel", "取消");
    cancelBtn.addEventListener("click", () => overlay.remove());

    const confirmBtn = document.createElement("button");
    confirmBtn.className = "btn btn-primary btn-small";
    confirmBtn.textContent = tt(null, "local_engine.diagnostic.confirm", "确认");
    confirmBtn.addEventListener("click", () => {
        overlay.remove();
        onConfirm();
    });

    btnRow.appendChild(cancelBtn);
    btnRow.appendChild(confirmBtn);
    modal.appendChild(btnRow);
    overlay.appendChild(modal);

    // 点击 overlay 外部关闭
    overlay.addEventListener("click", (e) => {
        if (e.target === overlay) overlay.remove();
    });

    document.body.appendChild(overlay);
}

/**
 * 显示通知消息（不使用原生 alert()）。
 * @param {string} message - 通知文案
 * @param {boolean} isSuccess - 是否为成功消息
 */
function showOrphanNotification(message, isSuccess) {
    let notif = document.querySelector(".le-orphan-notification");
    if (notif) notif.remove();

    notif = document.createElement("div");
    notif.className = "le-orphan-notification";
    notif.style.cssText =
        "position:fixed;bottom:1rem;right:1rem;padding:0.75rem 1rem;" +
        "border-radius:var(--radius-sm,4px);font-size:var(--text-sm);font-style:normal;" +
        "z-index:10000;max-width:360px;word-break:break-word;" +
        `background:${isSuccess ? "var(--green-dim,#1a3a1a)" : "var(--red-dim,#3a1a1a)"};` +
        `color:${isSuccess ? "var(--green)" : "var(--red)"};` +
        "border:1px solid var(--surface);";
    notif.textContent = message;
    document.body.appendChild(notif);

    // 3 秒后自动消失
    setTimeout(() => notif.remove(), 3000);
}
