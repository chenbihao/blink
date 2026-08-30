/**
 * 引擎卡片模型列表渲染（0.22.6 从 local-engine-card.js 拆出）。
 *
 * 展示模型候选：名称 / 体积 / 安装状态 / selected·active 标记 / 操作按钮。
 *
 * 边界铁则：
 * - 安装、删除、冲突的业务规则在**后端**。前端只消费 install_state wire 值
 *   与 ModelOperationResultDto（删除冲突=成功 IPC 上的 success:false +
 *   结构化 error，其 message 用户可读），不解析原因结构、不复制规则。
 * - selected（is_selected）与 active（is_active，运行时冻结快照）独立标记，
 *   两者不一致是合法状态（配置已改、运行实例未重启）。
 *
 * @module local-engine-models
 */

import {renderIcon} from "../../../shared/icon.js";
import {confirmDialog} from "../../../shared/tauri.js";
import {t} from "../../../i18n/index.js";
import {tt, formatMB} from "./local-engine-card-utils.js";
import {getEffectiveModelInstallState} from "./local-engine-state.js";

const MODEL_STATE_I18N_PREFIX = "local_engine.model.state.";
const MODEL_VERIFICATION_I18N_PREFIX = "local_engine.model.verification.";

/**
 * install_state wire value → i18n 文案。
 * wire value 是 snake_case（如 "not_installed"、"download_failed"）。
 * @param {string} installState
 * @returns {string}
 */
function modelStateLabel(installState) {
    if (!installState) return "—";
    const key = `${MODEL_STATE_I18N_PREFIX}${installState}`;
    const label = t(key);
    return label !== key ? label : installState;
}

/**
 * verification_state wire value → i18n 文案。
 * @param {string} verificationState
 * @returns {string}
 */
function modelVerificationLabel(verificationState) {
    if (!verificationState) return "";
    const key = `${MODEL_VERIFICATION_I18N_PREFIX}${verificationState}`;
    const label = t(key);
    return label !== key ? label : verificationState;
}

/**
 * 模型行外观签名（脏检查用）：集合内每个模型的安装/校验/选中/加载态。
 * selected 与 active 独立参与签名——二者不一致时行会重渲染。
 * @param {Object} model - ModelCatalogItemDto
 * @param {string} effectiveState - getEffectiveModelInstallState 结果
 * @returns {string}
 */
export function modelRowSignature(model, effectiveState) {
    return `${model.model_id}:${effectiveState}`
        + `:${model.verification_state}:${model.is_selected ? 1 : 0}`
        + `:${model.is_active ? 1 : 0}`;
}

// ── 模型列表渲染 ─────────────────────────────────────────────────────────────

/**
 * @param {HTMLElement} container
 * @param {Object} entry
 * @param {Object} controller
 * @param {Object} i18n
 */
export function updateModelList(container, entry, controller, i18n) {
    // 脏检查：模型行的外观输入未变时跳过重建——下载期间高频状态推送
    // 不应重建模型行按钮（hover 闪烁、点击落空）。
    const models = entry.models;
    const sigParts = [];
    for (const model of models || []) {
        sigParts.push(modelRowSignature(model, getEffectiveModelInstallState(entry, model)));
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
        table.appendChild(renderModelRow(model, entry, controller, i18n));
    }

    container.appendChild(table);
}

/**
 * 渲染单个模型行。
 */
function renderModelRow(model, entry, controller, i18n) {
    const row = document.createElement("div");
    row.className = "le-model-row";
    row.dataset.modelId = model.model_id;

    // 模型名
    const nameCell = document.createElement("div");
    nameCell.className = "le-model-cell le-model-name";
    nameCell.textContent = model.display_name || model.model_id;
    // selected / active 独立徽章（active 只在运行时有值，二者可并存或不一致）
    if (model.is_selected) {
        const badge = document.createElement("span");
        badge.className = "le-badge le-badge-cap le-model-badge";
        badge.textContent = tt(i18n, "local_engine.model.configured", "配置");
        nameCell.appendChild(badge);
    }
    if (model.is_active) {
        const badge = document.createElement("span");
        badge.className = "le-badge le-badge-lifecycle le-model-badge";
        badge.textContent = tt(i18n, "local_engine.model.active", "实际加载");
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
    statusCell.textContent = modelStateLabel(getEffectiveModelInstallState(entry, model));
    // 校验状态附加
    const verText = modelVerificationLabel(model.verification_state);
    if (verText && verText !== model.verification_state) {
        const verSpan = document.createElement("span");
        verSpan.className = "le-model-verification";
        verSpan.textContent = `(${verText})`;
        statusCell.appendChild(verSpan);
    }
    row.appendChild(statusCell);

    // 操作按钮
    const actionsCell = document.createElement("div");
    actionsCell.className = "le-model-cell le-model-actions";
    actionsCell.appendChild(renderModelActions(model, entry, controller, i18n));
    row.appendChild(actionsCell);

    return row;
}

/**
 * 渲染单个模型的操作按钮。根据 install_state 决定可用操作。
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
                controller?.deleteModel?.(engineId, modelId).then((result) => {
                    // 删除冲突 = 成功 IPC 上的 success:false + 结构化 error
                    //（message 用户可读）；业务规则解析在后端。
                    if (result && result.success === false && result.error) {
                        showModelRowError(modelId, result.error.message, i18n);
                    }
                }).catch((err) => {
                    showModelRowError(modelId, err?.message || String(err), i18n);
                });
            });
        });
        frag.appendChild(delBtn);
    }

    // delete_blocked → 不可操作
    if (state === "delete_blocked") {
        const span = document.createElement("span");
        span.className = "le-model-status le-model-state-blocked";
        span.textContent = tt(i18n, "local_engine.model.state.delete_blocked", "删除被阻止");
        frag.appendChild(span);
    }

    return frag;
}

/**
 * 在对应模型行下方展示一条操作失败/冲突文案。
 * @param {string} modelId
 * @param {string} message - 用户可读文案（后端 error.message）
 * @param {Object} i18n
 */
function showModelRowError(modelId, message, i18n) {
    const row = document.querySelector(`.le-model-row[data-model-id="${CSS.escape(modelId)}"]`);
    if (!row) return;

    // 同行旧提示先移除
    const old = row.parentElement?.querySelector(`.le-model-conflict[data-for-model="${CSS.escape(modelId)}"]`);
    if (old) old.remove();

    const conflict = document.createElement("div");
    conflict.className = "le-model-conflict";
    conflict.dataset.forModel = modelId;
    const title = document.createElement("div");
    title.className = "le-model-conflict-title";
    title.textContent = tt(i18n, "local_engine.model.conflict.title", "无法删除");
    conflict.appendChild(title);
    const reason = document.createElement("div");
    reason.className = "le-model-conflict-reason";
    reason.textContent = message || tt(i18n, "local_engine.error.unknown_error", "未知错误");
    conflict.appendChild(reason);

    row.parentElement?.insertBefore(conflict, row.nextSibling);
}
