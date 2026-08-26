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
import {t} from "../../../i18n/index.js";
import {processDisplay, processClass} from "./local-engine-process.js";
import {
    isEngineReady,
    hasActiveOperation,
    isOperationCancellable,
    getPrimaryAction,
    isActionBlocked,
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
        const logList = logArea.querySelector(".le-log-list");
        if (logList) {
            updateLogList(logList, entry, i18n);
        }
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
    container.textContent = "";

    const primary = getPrimaryAction(entry);
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
    const computePref = entry.catalog.current_compute_preference;

    switch (kind) {
        case "install":
            controller.install(engineId, computePref).catch(() => {});
            break;
        case "start":
            controller.start(engineId, computePref).catch(() => {});
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
        copyBtn.addEventListener("click", () => {
            navigator.clipboard?.writeText(detail).catch(() => {});
        });
        detailArea.appendChild(copyBtn);

        container.appendChild(detailArea);
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
        const text = entry.logs.map((l) => `[${l.timestamp}] [${l.level}] ${l.text}`).join("\n");
        navigator.clipboard?.writeText(text).catch(() => {});
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
    list.textContent = "";

    if (entry.logs.length === 0) {
        const empty = document.createElement("div");
        empty.className = "le-log-empty";
        empty.textContent = tt(i18n, "local_engine.log.empty", "暂无日志");
        list.appendChild(empty);
        return;
    }

    // instance 分隔线
    let lastInstance = null;
    for (const log of entry.logs) {
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
        time.textContent = log.timestamp;

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

    // 滚动到底部
    list.scrollTop = list.scrollHeight;
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

function tt(i18n, key, fallback) {
    if (i18n && typeof i18n.t === "function") return i18n.t(key) || fallback;
    return t(key) !== key ? t(key) : fallback;
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
