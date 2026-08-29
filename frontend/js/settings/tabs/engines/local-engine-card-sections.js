/**
 * 引擎卡片状态区渲染（0.22.6 从 local-engine-card.js 拆出）。
 *
 * 只做视觉投影，不做业务推导：
 * - 状态网格：环境 / 进程 / 服务 / 模型四行。
 * - operation 行：kind + stage（busy 判定在后端 kind!=="idle"，前端只展示）。
 * - backend 行：请求设备 / 解析 profile / 实际后端。
 * - storage 行：预计空间 / 实际占用 / 可释放。
 *
 * 内部存储概念（generation/install_id/provider/refcount/transaction slot）
 * 不出现在任何展示文案中。
 *
 * @module local-engine-card-sections
 */

import {processDisplay, processClass} from "./local-engine-process.js";
import {tt, statusClass, makeLabel, makeValue, formatBytes, formatMB} from "./local-engine-card-utils.js";

// ── 状态网格 ──────────────────────────────────────────────────────────────────

/**
 * @param {Object} entry - EngineStateEntry
 * @param {Object} i18n
 * @returns {HTMLElement}
 */
export function renderStatusGrid(entry, i18n) {
    const grid = document.createElement("div");
    grid.className = "le-status-grid";
    updateStatusGrid(grid, entry, i18n);
    return grid;
}

/**
 * @param {HTMLElement} grid
 * @param {Object} entry
 * @param {Object} i18n
 */
export function updateStatusGrid(grid, entry, i18n) {
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

// ── operation 信息 ─────────────────────────────────────────────────────────────

/**
 * @param {Object} entry
 * @param {Object} i18n
 * @returns {HTMLElement}
 */
export function renderOperationInfo(entry, i18n) {
    const div = document.createElement("div");
    div.className = "le-operation-info";
    updateOperationInfo(div, entry, i18n);
    return div;
}

/**
 * 当前操作 = kind + stage。operation_id 是内部事务标识，不展示。
 *
 * 0.22.6.1：乐观 pending（请求已发出、后端 operation 尚未到达）时
 * 立即显示 "kind + 等待中"——点击启动/安装后不能看起来毫无反应。
 * @param {HTMLElement} div
 * @param {Object} entry
 * @param {Object} i18n
 */
export function updateOperationInfo(div, entry, i18n) {
    const op = entry.status?.status?.operation;
    if (!op || op.kind === "idle") {
        if (entry.pendingAction) {
            // 乐观 pending 展示
            div.hidden = false;
            div.textContent = "";

            const kind = document.createElement("span");
            kind.className = "le-op-kind";
            kind.textContent = tt(
                i18n,
                `local_engine.operation.${entry.pendingAction.kind}`,
                entry.pendingAction.kind
            );

            const stage = document.createElement("span");
            stage.className = "le-op-stage";
            stage.textContent = tt(i18n, "local_engine.operation.stage.pending", "pending");

            div.appendChild(kind);
            div.appendChild(stage);
            return;
        }
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

    div.appendChild(kind);
    div.appendChild(stage);
}

// ── backend 信息 ──────────────────────────────────────────────────────────────

/**
 * @param {Object} entry
 * @param {Object} catalog
 * @param {Object} i18n
 * @returns {HTMLElement}
 */
export function renderBackendInfo(entry, catalog, i18n) {
    const div = document.createElement("div");
    div.className = "le-backend-info";
    updateBackendInfo(div, entry, i18n);
    return div;
}

/**
 * 计算设备三层展示：请求设备 / 解析 profile / 实际后端。
 * fallback_reasons（rejected profile）是内部调度信息，不展示。
 * @param {HTMLElement} div
 * @param {Object} entry
 * @param {Object} i18n
 */
export function updateBackendInfo(div, entry, i18n) {
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
}

// ── 存储信息 ──────────────────────────────────────────────────────────────────

/**
 * @param {Object} entry
 * @param {Object} catalog
 * @param {Object} i18n
 * @returns {HTMLElement}
 */
export function renderStorageInfo(entry, catalog, i18n) {
    const div = document.createElement("div");
    div.className = "le-storage-info";
    updateStorageInfo(div, entry, i18n);
    return div;
}

/**
 * @param {HTMLElement} div
 * @param {Object} entry
 * @param {Object} i18n
 */
export function updateStorageInfo(div, entry, i18n) {
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
