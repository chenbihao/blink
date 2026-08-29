/**
 * 通用引擎卡片 renderer 入口（0.22.5 H3，0.22.6 收敛拆分）。
 *
 * 同一个 renderer 渲染两张卡（FunASR / PaddleOCR），不按 engine_id 复制整套 DOM。
 * 通用 renderer 只处理生命周期；engine-specific 配置通过受限 adapter hook 完成。
 *
 * 模块职责（装配、初始化与稳定导出）：
 * - 卡片 DOM 骨架创建与 updateCardContent 调度；
 * - actions 按钮组（install/start/stop/repair/cleanup/cancel/日志开关）；
 * - 受限 adapter hook 注册表。
 * 状态区/模型列表/诊断/日志的渲染拆分见：
 * local-engine-card-sections.js / local-engine-models.js /
 * local-engine-diagnostics.js / local-engine-log-view.js。
 *
 * ## 安全铁则
 *
 * - renderer 绝不允许任意 engine id 动态注入 HTML、command 或字段路径。
 * - 图标使用现有 Lucide sprite（renderIcon / iconHTML），禁 emoji。
 * - 不新增 inline style.display；显隐使用 class / hidden / aria-expanded。
 * - 中文不斜体。
 * - 日志文本**绝不通过 innerHTML 注入**，只走 textContent。
 *
 * ## DOM contract
 *
 * 调用方提供一个容器元素，调用 `renderEngineCard(container, entry, controller, hooks)` 即可。
 * 容器内会创建以下结构（class 命名规范：`le-card-*`）：
 *
 * ```html
 * <div class="le-card" data-engine-id="funasr">
 *   <div class="le-card-header">…</div>
 *   <div class="le-card-body">…状态区…</div>
 *   <div class="le-card-actions">…按钮区…</div>
 *   <div class="le-card-error" hidden>…错误区…</div>
 *   <div class="le-card-log" hidden>…日志区…</div>
 * </div>
 * ```
 *
 * @module local-engine-card
 */

import {renderIcon} from "../../../shared/icon.js";
import {
    renderStatusGrid,
    updateStatusGrid,
    renderOperationInfo,
    updateOperationInfo,
    renderBackendInfo,
    updateBackendInfo,
    renderStorageInfo,
    updateStorageInfo,
} from "./local-engine-card-sections.js";
import {renderModelList, updateModelList} from "./local-engine-models.js";
import {showEngineDiagnostics} from "./local-engine-diagnostics.js";
import {renderLogComponent, updateLogList} from "./local-engine-log-view.js";
import {tt, makeBadge, cssEscape, copyTextWithFeedback} from "./local-engine-card-utils.js";
import {
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

    // ── 模型列表区 ──────────────────────────────────────────────────────
    body.appendChild(renderModelList(entry, controller, i18n));

    // ── 引擎目录 + 诊断按钮区 ────────────────────────────────────────────
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
        // 新 operation 自动展开日志区：基于 entry.logAutoExpand 标志
        //（reducer 在新 operation 出现时设置）。DOM 上记录已展开的
        // operation_id，防止同一 operation 重复展开。用户手动收起后不会被
        // 重新展开——因为标志只在 operation_id 跃迁时设置。
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

    // 更新模型列表
    const modelList = card.querySelector(".le-model-list");
    if (modelList) {
        updateModelList(modelList, entry, controller, i18n);
    }
}

// ── actions 按钮 ───────────────────────────────────────────────────────────────

/**
 * 更新 actions 按钮组。
 * 脏检查：下载等高频状态推送期间，按钮组的外观输入未变时跳过重建。
 * 全量重建会丢失 hover/press 状态（闪烁、点击落空）和日志按钮的
 * aria-expanded——每条日志事件都会走到这里，重建必须杜绝。
 */
function updateActions(container, entry, controller, i18n) {
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
        // 只显示 primary + cancel + cleanup；cancel 仅在可取消时显示
        if (!isPrimary && btn.kind !== "cancel" && btn.kind !== "cleanup") continue;
        if (btn.kind === "cancel" && !isOperationCancellable(entry)) continue;

        const blocked = isActionBlocked(entry, btn.kind);

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

/**
 * actions 点击处理（生命周期动作）。cleanup 只打开 modal，绝不直接 invoke。
 */
function handleActionClick(kind, entry, controller, i18n) {
    const engineId = entry.catalog.engine_id;

    switch (kind) {
        case "install":
            // 不传 compute_preference：由后端从配置真源构造 AdapterConfig
            // 前端 catalog.current_compute_preference 可能是过期快照
            controller.install(engineId, null).catch(() => {});
            break;
        case "start":
            controller.start(engineId, null).catch(() => {});
            break;
        case "stop":
            controller.stop(engineId).catch(() => {});
            break;
        case "repair":
            controller.repair(engineId).catch(() => {});
            break;
        case "cleanup":
            // 清理只打开 cleanup modal。modal 从最新 storage DTO 渲染精确
            // targets，confirm 时只提交选中的 target_id。
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

/**
 * 更新 last_error 区。cancelled 不进错误区——已取消不是错误，
 * 由 operation 行的 stage 文案表达。
 */
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
