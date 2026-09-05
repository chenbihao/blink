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
import {getSelection} from "./local-engine-selection.js";

const MODEL_STATE_I18N_PREFIX = "local_engine.model.state.";
const MODEL_VERIFICATION_I18N_PREFIX = "local_engine.model.verification.";

/**
 * t 的带 fallback 版本：key 未命中（t 返回 key 本身）时用 fallback，
 * fallback 支持 {param} 插值。
 */
function tf(key, fallback, params) {
    const label = t(key, params);
    if (label !== key) return label;
    if (!params) return fallback;
    return fallback.replace(/\{(\w+)\}/g, (_, name) => String(params[name] ?? ""));
}

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
 *
 * 0.22.9：签名包含 selection phase 与业务特征——切换在途时行按钮必须
 * 重渲染为禁用；业务特征（流式/资源画像）变化（如新候选进入目录）同样触发。
 *
 * @param {Object} model - ModelCatalogItemDto
 * @param {string} effectiveState - getEffectiveModelInstallState 结果
 * @param {string} [selectionPhase] - selection phase（无 = ""）
 * @returns {string}
 */
export function modelRowSignature(model, effectiveState, selectionPhase = "") {
    return `${model.model_id}:${effectiveState}`
        + `:${model.verification_state}:${model.is_selected ? 1 : 0}`
        + `:${model.is_active ? 1 : 0}`
        + `:${modelTraitSignature(model)}`
        + `:${selectionPhase}`;
}

/** 业务特征签名（决定特征行是否重渲染）。 */
function modelTraitSignature(model) {
    const chips = modelTraitChips(model);
    return chips.map((c) => c.text).join(",");
}

// ── 模型业务特征（0.22.9 Handoff 09）─────────────────────────────────────────

/**
 * 从 DTO 派生模型业务特征（display-only）。
 *
 * 数据真源全部来自后端 wire 值，前端**不硬编码 model_id → 能力映射**：
 * - 多语言：`stt_capabilities.languages`
 * - 真/伪流式：`stt_capabilities.true_streaming`（真流式优先展示）
 * - 资源占用：`business.resource_footprint`（shared_gguf_worker /
 *   dedicated_onnx_worker）
 * - 中文质量：`business.chinese_quality`（corpus_baseline）
 * - 下载大小：`estimated_size_mb`（独立单元格展示，不进特征行）
 *
 * @param {Object} model - ModelCatalogItemDto
 * @returns {{kind: string, text: string}[]}
 */
export function modelTraitChips(model) {
    const chips = [];
    const caps = model?.stt_capabilities;

    // 多语言：languages 非空才展示（空 = 未声明，不猜）
    if (caps && Array.isArray(caps.languages) && caps.languages.length > 0) {
        const codes = caps.languages.join("/");
        chips.push({
            kind: "languages",
            text: caps.languages.length > 1
                ? tf("local_engine.business.lang.multi", "多语种（{codes}）", {codes})
                : tf(`local_engine.business.lang.${caps.languages[0]}`, codes),
        });
    }

    // 真/伪流式：true_streaming 支持则展示"真流式"，否则伪流式
    if (caps?.true_streaming?.supported === "yes") {
        chips.push({kind: "true_streaming", text: tf("local_engine.business.streaming.true", "真流式")});
    } else if (caps?.pseudo_streaming?.supported === "yes") {
        chips.push({kind: "pseudo_streaming", text: tf("local_engine.business.streaming.pseudo", "伪流式")});
    }

    // 标点（0.22.9）：模型是否输出标点——paraformer 系无标点，
    // 下载前可见，避免"为什么没有句号"的困惑
    const punct = caps?.punctuation;
    if (punct?.supported === "yes") {
        chips.push({kind: "punctuation", text: tf("local_engine.business.punctuation.yes", "含标点")});
    } else if (punct?.supported === "no") {
        chips.push({kind: "punctuation", text: tf("local_engine.business.punctuation.no", "无标点")});
    }

    // 资源占用定位（后端声明，i18n 未命中显示原 wire 值）
    const footprint = model?.business?.resource_footprint;
    if (footprint) {
        chips.push({kind: "resource", text: tf(`local_engine.business.${footprint}`, footprint)});
    }

    // 中文质量定位
    const quality = model?.business?.chinese_quality;
    if (quality) {
        chips.push({kind: "quality", text: tf(`local_engine.business.${quality}`, quality)});
    }

    return chips;
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
    // 0.22.9：selection phase 参与签名——切换开始/结束时按钮禁用态必须更新。
    const models = entry.models;
    const selectionPhase = getSelection(entry)?.phase || "";
    const sigParts = [];
    for (const model of models || []) {
        sigParts.push(modelRowSignature(model, getEffectiveModelInstallState(entry, model), selectionPhase));
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
        table.appendChild(renderModelRow(model, entry, controller, i18n, selectionPhase));
    }

    container.appendChild(table);
}

/**
 * 渲染单个模型行。
 */
function renderModelRow(model, entry, controller, i18n, selectionPhase = "") {
    const row = document.createElement("div");
    row.className = "le-model-row";
    row.dataset.modelId = model.model_id;

    // 模型名
    const nameCell = document.createElement("div");
    nameCell.className = "le-model-cell le-model-name";
    nameCell.textContent = model.display_name || model.model_id;
    // 0.22.9：目录推荐标记（display-only，来自 descriptor.business.recommended）
    if (model.business?.recommended) {
        const badge = document.createElement("span");
        badge.className = "le-badge le-model-badge-recommended";
        badge.textContent = tt(i18n, "local_engine.model.badge.recommended", "推荐");
        nameCell.appendChild(badge);
    }
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
    // 切换目标徽章：switching 在途时只在目标模型行标注"切换中"，
    // 不与配置/实际加载混淆（非目标行不加，避免误读为多模型并发切换）
    if (selectionPhase === "switching"
        && entry.selection?.targetModelId === model.model_id) {
        const badge = document.createElement("span");
        badge.className = "le-badge le-model-badge le-model-badge-switching";
        badge.textContent = tt(i18n, "local_engine.selection.badge", "切换中");
        nameCell.appendChild(badge);
    }

    // 业务特征行（多语言/真伪流式/资源占用/中文质量——DTO 驱动，无硬编码映射）。
    // 放在名称单元格内作为子行，不增加模型表格的网格列。
    const traits = modelTraitChips(model);
    if (traits.length > 0) {
        const traitsLine = document.createElement("div");
        traitsLine.className = "le-model-traits";
        traitsLine.textContent = traits.map((c) => c.text).join(" · ");
        nameCell.appendChild(traitsLine);
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
    actionsCell.appendChild(renderModelActions(model, entry, controller, i18n, selectionPhase));
    row.appendChild(actionsCell);

    return row;
}

/**
 * 渲染单个模型的操作按钮。根据 install_state 决定可用操作。
 *
 * 0.22.9：切换事务在途（selectionPhase="switching"）时禁用全部写操作——
 * 后端事务持有 claim 本会拒绝，前端同步禁用避免无效点击。
 */
function renderModelActions(model, entry, controller, i18n, selectionPhase = "") {
    const frag = document.createDocumentFragment();
    const state = getEffectiveModelInstallState(entry, model);
    const engineId = model.engine_id;
    const modelId = model.model_id;
    const switchInProgress = selectionPhase === "switching";

    // NotInstalled → 下载
    if (state === "not_installed" || state === "download_failed"
        || state === "staging_failed" || state === "verification_failed") {
        const btn = document.createElement("button");
        btn.className = "le-model-btn le-model-btn-primary";
        btn.type = "button";
        btn.disabled = switchInProgress;
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
        // 0.22.7：模型选择唯一写入口——已安装模型行显示"使用"按钮或状态标记
        // is_selected + is_active 一致 → "当前"标签（选中且运行中一致）
        // is_selected + !is_active → "等待重启"标签（选中但运行实例仍用旧模型）
        // !is_selected → "使用"按钮（点击后选为此引擎的当前模型）
        if (model.is_selected) {
            if (model.is_active) {
                // 选中且运行中一致 → "当前"标记
                const badge = document.createElement("span");
                badge.className = "le-model-btn le-model-btn-current";
                badge.textContent = tt(i18n, "local_engine.model.action.current", "当前");
                frag.appendChild(badge);
            } else {
                // 选中但运行实例不一致 → "等待重启"标记
                const badge = document.createElement("span");
                badge.className = "le-model-btn le-model-btn-pending-restart";
                badge.textContent = tt(i18n, "local_engine.model.action.pending_restart", "等待重启");
                frag.appendChild(badge);
            }
        } else {
            // 未选中 → "使用"按钮（切换在途时禁用）
            const useBtn = document.createElement("button");
            useBtn.className = "le-model-btn le-model-btn-primary";
            useBtn.type = "button";
            useBtn.disabled = switchInProgress;
            useBtn.appendChild(renderIcon("check", {extraClass: "le-action-icon"}));
            const useLabel = document.createElement("span");
            useLabel.textContent = tt(i18n, "local_engine.model.action.use", "使用");
            useBtn.appendChild(useLabel);
            useBtn.addEventListener("click", () => {
                // 服务运行中切换 = 完整事务（stop → commit → start，失败回滚），
                // 0.22.7 契约要求经用户确认；未运行时仅提交 selected，无副作用。
                const activeImpl = entry?.status?.status?.active_implementation;
                if (!activeImpl) {
                    controller?.selectModel?.(engineId, modelId).catch(() => {});
                    return;
                }
                const confirmMsg = tt(i18n, "local_engine.model.action.switch_confirm_desc",
                    "服务运行中切换模型：将停止当前模型并启动新模型，失败时自动回滚。确认切换？");
                confirmDialog(confirmMsg, {kind: "info"}).then((ok) => {
                    if (ok) controller?.selectModel?.(engineId, modelId).catch(() => {});
                });
            });
            frag.appendChild(useBtn);
        }

        const repairBtn = document.createElement("button");
        repairBtn.className = "le-model-btn";
        repairBtn.type = "button";
        repairBtn.disabled = switchInProgress;
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
        delBtn.disabled = switchInProgress;
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
