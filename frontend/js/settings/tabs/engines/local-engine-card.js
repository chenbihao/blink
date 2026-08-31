/**
 * 通用引擎卡片 renderer 入口（0.22.5 H3，0.22.6 收敛拆分，中密度纵向重设计）。
 *
 * 同一个 renderer 渲染两张卡（FunASR / PaddleOCR），不按 engine_id 复制整套 DOM。
 * 通用 renderer 只处理生命周期；engine-specific 配置通过受限 adapter hook 完成。
 *
 * ## 卡片信息架构（默认态约 200-240px，全宽上下排列）
 *
 * ```html
 * <div class="le-card extension-card" data-engine-id="funasr" tabindex="-1">
 *   <div class="le-card-head extension-header">
 *     <div class="le-card-icon extension-icon">icon</div>
 *     <div class="le-card-info extension-info">
 *       <h3>display_name + capability</h3>
 *       <p class="le-card-summary extension-desc">● 运行中 · SenseVoiceSmall · CPU</p>
 *     </div>
 *     <div class="le-card-primary">[唯一主操作按钮]</div>
 *   </div>
 *   <div class="le-card-body extension-body">
 *     <div class="le-keyline">环境/模型/服务/策略 四枚 badge</div>
 *     <div class="le-card-config">adapter hook 受限配置区</div>
 *     <div class="le-feedback" role="status">反馈槽（固定 min-height）</div>
 *     <div class="le-card-tools">[管理模型(n)] [日志] [诊断] [打开目录] … [维护▾]</div>
 *   </div>
 *   <div class="le-model-list" hidden>模型列表（管理模型展开）</div>
 *   <div class="le-card-log" hidden>完整日志（日志按钮展开）</div>
 *   <div class="le-maintenance" hidden>修复环境/清理缓存（维护展开）</div>
 *   <div class="le-diagnostic-inline" hidden>诊断（诊断按钮展开）</div>
 * </div>
 * ```
 *
 * 默认可见：身份 + 综合摘要 + 唯一主操作 + 关键状态行 + 直接配置区 + 反馈槽
 * （含错误摘要）。展开才可见：完整日志、模型列表、维护操作、诊断。
 *
 * ## 安全铁则
 *
 * - renderer 绝不允许任意 engine id 动态注入 HTML、command 或字段路径。
 * - 图标使用现有 Lucide sprite（renderIcon / iconHTML），禁 emoji。
 * - 不新增 inline style.display；显隐使用 hidden / aria-expanded。
 * - 中文不斜体。
 * - 日志文本**绝不通过 innerHTML 注入**，只走 textContent。
 * - 所有子区更新走签名脏检查——高频日志/下载事件不得重建按钮组
 *   （hover 丢失、点击落空、aria-expanded 重置都由此产生）。
 *
 * @module local-engine-card
 */

import {renderIcon} from "../../../shared/icon.js";
import {t as tGlobal} from "../../../i18n/index.js";
import {updateKeyline} from "./local-engine-card-sections.js";
import {updateModelList} from "./local-engine-models.js";
import {showEngineDiagnostics} from "./local-engine-diagnostics.js";
import {renderLogComponent, updateLogList} from "./local-engine-log-view.js";
import {tt, cssEscape, copyTextWithFeedback} from "./local-engine-card-utils.js";
import {isActionBlocked} from "./local-engine-state.js";
import {
    computeEngineSummary,
    computeFeedback,
    computeKeyline,
    computeModelSummary,
    primaryActionView,
} from "./local-engine-summary.js";

// ── 受限 adapter hook 注册表 ──────────────────────────────────────────────────

/**
 * 受限 adapter hook 注册表。
 *
 * 通用 renderer 只处理生命周期。FunASR/PaddleOCR 的配置保存通过按 engine_id
 * 注册的受限 hook 完成，但不能复制 card lifecycle。
 *
 * hook 结构：
 * - `renderConfig(container, entry, controller)`: 渲染引擎专属配置区（受限）。
 *   renderer 在配置签名变化时重新调用（select/开关从 preferences 真源重建）。
 *
 * renderer 绝不允许任意 engine id 动态注入 HTML、command 或字段路径。
 */
const adapterHooks = new Map();

/**
 * 注册受限 adapter hook。
 * @param {string} engineId
 * @param {{renderConfig?: Function}} hooks
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

/** 配置区签名：preferences + selected/active 身份（不含高频 install_state）。 */
function configAreaSignature(entry) {
    const summary = computeModelSummary(entry);
    return JSON.stringify(entry.preferences || null)
        + `|${summary.selectedName || ""}|${summary.activeName || ""}`
        + `|${entry.preferences?.requires_rebuild === true ? 1 : 0}`;
}

/**
 * summary 投影的 i18n 适配器：命中返回文案，未命中返回 undefined
 * （local-engine-summary 的 tx 会退回内置 fallback，避免露出裸 key）。
 * @param {Object|null} i18n
 * @returns {Function}
 */
function makeTAdapter(i18n) {
    if (i18n && typeof i18n.t === "function") {
        return (key, params) => {
            const value = i18n.t(key, params);
            return value && value !== key ? value : undefined;
        };
    }
    return (key, params) => {
        const value = tGlobal(key, params);
        return value !== key ? value : undefined;
    };
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

    // ── head：身份 + 综合摘要 + 唯一主操作 ────────────────────────────────
    const head = document.createElement("div");
    head.className = "le-card-head extension-header";

    const iconWrap = document.createElement("div");
    iconWrap.className = "le-card-icon extension-icon";
    iconWrap.appendChild(renderIcon(catalog.icon || "cpu"));

    const info = document.createElement("div");
    info.className = "le-card-info extension-info";
    const title = document.createElement("h3");
    title.className = "le-card-title";
    const titleText = document.createElement("span");
    titleText.textContent = catalog.display_name;
    const cap = document.createElement("span");
    cap.className = "le-card-cap";
    cap.textContent = tt(i18n, `local_engine.capability.${catalog.capability_kind}`, catalog.capability_kind);
    title.appendChild(titleText);
    title.appendChild(cap);

    const summary = document.createElement("p");
    summary.className = "le-card-summary extension-desc";
    info.appendChild(title);
    info.appendChild(summary);

    const primary = document.createElement("div");
    primary.className = "le-card-primary";

    head.appendChild(iconWrap);
    head.appendChild(info);
    head.appendChild(primary);
    card.appendChild(head);

    // ── body ─────────────────────────────────────────────────────────────
    const body = document.createElement("div");
    body.className = "le-card-body extension-body";

    // 关键状态行：环境 / 模型 / 服务 / 策略（进程态进摘要，PID 进诊断）
    const keyline = document.createElement("div");
    keyline.className = "le-keyline";
    body.appendChild(keyline);

    // adapter hook：引擎专属配置区（受限）
    const configArea = document.createElement("div");
    configArea.className = "le-card-config";
    configArea.dataset.leConfigArea = engineId;
    body.appendChild(configArea);

    // 反馈槽：operation 阶段 / 错误摘要 / 待重启 / 空闲说明（固定 min-height 防跳动）
    const feedback = document.createElement("div");
    feedback.className = "le-feedback";
    feedback.setAttribute("role", "status");
    feedback.setAttribute("aria-live", "polite");
    body.appendChild(feedback);

    // 底部工具栏：二级操作 + 维护（展开式）
    body.appendChild(buildToolsRow(entry, controller, i18n));

    card.appendChild(body);

    // ── 折叠层：模型列表（默认折叠） ──────────────────────────────────────
    const modelList = document.createElement("div");
    modelList.className = "le-model-list";
    modelList.hidden = true;
    body.appendChild(modelList);

    // ── 折叠层：完整日志（默认折叠） ──────────────────────────────────────
    const logArea = document.createElement("div");
    logArea.className = "le-card-log";
    logArea.hidden = true;
    logArea.appendChild(renderLogComponent(entry, controller, i18n));
    card.appendChild(logArea);

    // ── 折叠层：维护（默认折叠，含破坏性操作） ────────────────────────────
    card.appendChild(buildMaintenancePanel(entry, controller, i18n));

    // ── 折叠层：诊断（默认折叠） ──────────────────────────────────────────
    const diagPanel = document.createElement("div");
    diagPanel.className = "le-diagnostic-inline";
    diagPanel.hidden = true;
    card.appendChild(diagPanel);

    container.appendChild(card);

    // 首次渲染后填充动态内容
    updateCardContent(card, entry, controller, i18n);
}

/**
 * 底部工具栏：管理模型(n) / 日志 / 诊断 / 打开目录 … 维护。
 * 按钮只创建一次；后续更新只改文案（模型数量），不重建。
 */
function buildToolsRow(entry, controller, i18n) {
    const tools = document.createElement("div");
    tools.className = "le-card-tools";
    const engineId = entry.catalog.engine_id;

    // 管理模型（展开模型列表）
    const modelsBtn = document.createElement("button");
    modelsBtn.className = "btn btn-small le-action-btn le-models-toggle";
    modelsBtn.type = "button";
    modelsBtn.dataset.actionKind = "models";
    modelsBtn.setAttribute("aria-expanded", "false");
    modelsBtn.appendChild(renderIcon("list", {extraClass: "le-action-icon"}));
    const modelsLabel = document.createElement("span");
    modelsLabel.className = "le-models-toggle-label";
    modelsLabel.textContent = tt(i18n, "local_engine.action.manage_models", "管理模型");
    modelsBtn.appendChild(modelsLabel);
    modelsBtn.addEventListener("click", () => {
        toggleExclusivePanel(modelsBtn, ".le-model-list");
    });
    tools.appendChild(modelsBtn);

    // 日志（展开完整日志组件）
    const logBtn = document.createElement("button");
    logBtn.className = "btn btn-small le-action-btn le-log-toggle";
    logBtn.type = "button";
    logBtn.dataset.actionKind = "log";
    logBtn.setAttribute("aria-expanded", "false");
    logBtn.appendChild(renderIcon("terminal", {extraClass: "le-action-icon"}));
    const logLabel = document.createElement("span");
    logLabel.textContent = tt(i18n, "local_engine.action.logs", "日志");
    logBtn.appendChild(logLabel);
    logBtn.addEventListener("click", () => {
        toggleExclusivePanel(logBtn, ".le-card-log");
    });
    tools.appendChild(logBtn);

    // 诊断（展开内联诊断面板）
    const diagBtn = document.createElement("button");
    diagBtn.className = "btn btn-small le-action-btn le-diagnostic-toggle";
    diagBtn.type = "button";
    diagBtn.dataset.actionKind = "diagnostics";
    diagBtn.setAttribute("aria-expanded", "false");
    diagBtn.appendChild(renderIcon("stethoscope", {extraClass: "le-action-icon"}));
    const diagLabel = document.createElement("span");
    diagLabel.textContent = tt(i18n, "local_engine.diagnostic.btn", "诊断");
    diagBtn.appendChild(diagLabel);
    diagBtn.addEventListener("click", () => {
        const card = diagBtn.closest(".le-card");
        const panel = card?.querySelector(".le-diagnostic-inline");
        if (panel) {
            if (panel.hidden) collapseSiblingPanels(card, ".le-diagnostic-inline");
            showEngineDiagnostics(entry, controller, i18n, diagBtn, panel);
        }
    });
    tools.appendChild(diagBtn);

    // 维护（与模型/日志/诊断组成互斥面板组）
    const maintBtn = document.createElement("button");
    maintBtn.className = "btn btn-small le-action-btn le-maintenance-toggle";
    maintBtn.type = "button";
    maintBtn.dataset.actionKind = "maintenance";
    maintBtn.setAttribute("aria-expanded", "false");
    maintBtn.appendChild(renderIcon("wrench", {extraClass: "le-action-icon"}));
    const maintLabel = document.createElement("span");
    maintLabel.textContent = tt(i18n, "local_engine.maintenance.btn", "维护");
    maintBtn.appendChild(maintLabel);
    maintBtn.addEventListener("click", () => {
        toggleExclusivePanel(maintBtn, ".le-maintenance");
    });
    tools.appendChild(maintBtn);

    // 弹性间隔：前四个面板入口在左，打开目录固定在右
    const spacer = document.createElement("span");
    spacer.className = "le-tools-spacer";
    tools.appendChild(spacer);

    // 打开目录（独立动作，不参与面板切换）
    const openDirBtn = document.createElement("button");
    openDirBtn.className = "btn btn-small le-action-btn";
    openDirBtn.type = "button";
    openDirBtn.dataset.actionKind = "open-dir";
    openDirBtn.appendChild(renderIcon("folder-open", {extraClass: "le-action-icon"}));
    const openDirLabel = document.createElement("span");
    openDirLabel.textContent = tt(i18n, "local_engine.foundation.open_engine_dir", "打开引擎目录");
    openDirBtn.appendChild(openDirLabel);
    openDirBtn.addEventListener("click", () => {
        controller?.openEngineFolder?.(engineId).catch(() => {});
    });
    tools.appendChild(openDirBtn);

    return tools;
}

/**
 * 面板开关：同一卡片内模型/日志/诊断/维护互斥，只允许一个展开。
 * @param {HTMLElement} btn - 触发按钮（含 aria-expanded）
 * @param {string} selector - 折叠层选择器（相对 btn.closest(".le-card")）
 */
function toggleExclusivePanel(btn, selector) {
    const card = btn.closest(".le-card");
    const panel = card?.querySelector(selector);
    if (!panel) return;
    const willExpand = panel.hidden;
    collapseSiblingPanels(card, selector);
    if (willExpand) {
        panel.hidden = false;
        btn.setAttribute("aria-expanded", "true");
    } else {
        panel.hidden = true;
        btn.setAttribute("aria-expanded", "false");
    }
}

function collapseSiblingPanels(card, exceptSelector = null) {
    const bindings = [
        [".le-models-toggle", ".le-model-list"],
        [".le-log-toggle", ".le-card-log"],
        [".le-diagnostic-toggle", ".le-diagnostic-inline"],
        [".le-maintenance-toggle", ".le-maintenance"],
    ];
    for (const [buttonSelector, panelSelector] of bindings) {
        if (panelSelector === exceptSelector) continue;
        const button = card?.querySelector(buttonSelector);
        const sibling = card?.querySelector(panelSelector);
        if (sibling) sibling.hidden = true;
        if (button) button.setAttribute("aria-expanded", "false");
    }
}

/**
 * 维护面板（默认折叠）：修复环境 / 清理引擎缓存 / 实际占用。
 * 危险/破坏性操作与普通操作视觉区分，确认流程保留（cleanup 走 modal）。
 */
function buildMaintenancePanel(entry, controller, i18n) {
    const engineId = entry.catalog.engine_id;
    const panel = document.createElement("div");
    panel.className = "le-maintenance";
    panel.hidden = true;

    const hint = document.createElement("p");
    hint.className = "le-maintenance-hint";
    hint.textContent = tt(i18n, "local_engine.maintenance.hint",
        "以下操作影响引擎环境或已下载的资产，执行前会再次确认。");
    panel.appendChild(hint);

    const actions = document.createElement("div");
    actions.className = "le-maintenance-actions";

    // 修复环境
    const repairBtn = document.createElement("button");
    repairBtn.className = "btn btn-small le-action-btn le-maintenance-repair";
    repairBtn.type = "button";
    repairBtn.dataset.actionKind = "repair";
    repairBtn.appendChild(renderIcon("wrench", {extraClass: "le-action-icon"}));
    const repairLabel = document.createElement("span");
    repairLabel.textContent = tt(i18n, "local_engine.maintenance.repair_env", "修复环境");
    repairBtn.appendChild(repairLabel);
    repairBtn.addEventListener("click", () => {
        controller?.repair(engineId).catch(() => {});
    });
    actions.appendChild(repairBtn);

    // 清理引擎缓存（modal 确认，绝不直接 invoke）
    const cleanupBtn = document.createElement("button");
    cleanupBtn.className = "btn btn-small le-action-btn le-maintenance-cleanup le-btn-danger";
    cleanupBtn.type = "button";
    cleanupBtn.dataset.actionKind = "cleanup";
    cleanupBtn.appendChild(renderIcon("trash-2", {extraClass: "le-action-icon"}));
    const cleanupLabel = document.createElement("span");
    cleanupLabel.textContent = tt(i18n, "local_engine.maintenance.cleanup", "清理引擎缓存");
    cleanupBtn.appendChild(cleanupLabel);
    cleanupBtn.addEventListener("click", () => {
        if (typeof i18n?.onCleanupOpen === "function") {
            i18n.onCleanupOpen(engineId, entry, controller);
        }
    });
    actions.appendChild(cleanupBtn);

    panel.appendChild(actions);

    // 实际占用 / 可释放（storage DTO 到达后更新）
    const storage = document.createElement("div");
    storage.className = "le-maintenance-storage";
    panel.appendChild(storage);

    return panel;
}

// ── 内容更新（签名脏检查） ────────────────────────────────────────────────────

/**
 * 更新已存在卡片的内容（不重建 DOM，只更新动态内容）。
 */
function updateCardContent(card, entry, controller, i18n) {
    // 综合摘要
    const summary = card.querySelector(".le-card-summary");
    if (summary) {
        updateSummary(summary, entry, i18n);
    }

    // 唯一主操作
    const primary = card.querySelector(".le-card-primary");
    if (primary) {
        updatePrimary(primary, entry, controller, i18n);
    }

    // 关键状态行
    const keyline = card.querySelector(".le-keyline");
    if (keyline) {
        updateKeyline(keyline, entry, i18n);
    }

    // adapter hook 配置区（签名变化时整体重渲染——受控控件从 preferences 重建）
    const configArea = card.querySelector(".le-card-config");
    if (configArea) {
        updateConfigArea(configArea, entry, controller, i18n);
    }

    // 反馈槽
    const feedback = card.querySelector(".le-feedback");
    if (feedback) {
        updateFeedback(feedback, entry, i18n);
    }

    // 工具栏：管理模型数量（只改文案，不重建按钮）
    const modelsLabel = card.querySelector(".le-models-toggle-label");
    if (modelsLabel) {
        const count = computeModelSummary(entry).installedCount;
        const text = count > 0
            ? tt(i18n, "local_engine.action.manage_models_count", "管理模型（{count}）", {count})
            : tt(i18n, "local_engine.action.manage_models", "管理模型");
        if (modelsLabel.textContent !== text) {
            modelsLabel.textContent = text;
        }
    }

    // 维护面板：忙碌时禁用按钮 + 更新占用
    const maintenance = card.querySelector(".le-maintenance");
    if (maintenance) {
        updateMaintenance(maintenance, entry, i18n);
    }

    // 日志列表（展开/折叠都按脏检查更新）
    const logList = card.querySelector(".le-log-list");
    if (logList) {
        updateLogList(logList, entry, i18n);
    }

    // 模型列表
    const modelList = card.querySelector(".le-model-list");
    if (modelList) {
        updateModelList(modelList, entry, controller, i18n);
    }
}

/** 综合摘要更新。 */
function updateSummary(el, entry, i18n) {
    const summary = computeEngineSummary(entry, makeTAdapter(i18n));
    const sig = `${summary.tone}|${summary.text}`;
    if (el.dataset.sig === sig) return;
    el.dataset.sig = sig;
    // 状态刷新只替换语气类，必须保留设置页通用摘要视觉基类。
    el.className = `le-card-summary extension-desc le-tone-${summary.tone}`;

    el.textContent = "";
    const dot = document.createElement("span");
    dot.className = "le-summary-dot";
    dot.setAttribute("aria-hidden", "true");
    el.appendChild(dot);
    el.appendChild(document.createTextNode(summary.text));
}

/** 唯一主操作更新（kind/label/disabled 变化才重建）。 */
function updatePrimary(el, entry, controller, i18n) {
    const view = primaryActionView(entry, makeTAdapter(i18n));
    const sig = `${view.kind || "-"}|${view.label}|${view.disabled}`;
    if (el.dataset.sig === sig) return;
    el.dataset.sig = sig;

    el.textContent = "";

    const btn = document.createElement("button");
    btn.type = "button";
    // 安装/启动/修复是推进型主操作，用 primary 强调；停止/取消为常规按钮
    const emphatic = view.kind === "install" || view.kind === "start" || view.kind === "repair";
    btn.className = emphatic
        ? "btn btn-primary btn-small le-action-btn le-primary-btn"
        : "btn btn-small le-action-btn le-primary-btn";
    btn.dataset.actionKind = view.kind || "busy";
    btn.disabled = view.disabled;
    btn.appendChild(renderIcon(view.icon, {extraClass: "le-action-icon"}));
    const label = document.createElement("span");
    label.textContent = view.label;
    btn.appendChild(label);
    btn.addEventListener("click", () => {
        if (!view.kind) return;
        handleActionClick(view.kind, entry, controller, i18n);
    });
    el.appendChild(btn);
}

/** 反馈槽更新（固定槽位：operation > 错误 > 模型操作 > 待重启 > 空闲）。 */
function updateFeedback(el, entry, i18n) {
    const feedback = computeFeedback(entry, makeTAdapter(i18n));
    const sig = `${feedback.tone}|${feedback.text}|${feedback.detail || ""}`;
    if (el.dataset.sig === sig) return;
    el.dataset.sig = sig;

    el.className = `le-feedback le-feedback-${feedback.tone}`;
    el.textContent = "";

    const main = document.createElement("span");
    main.className = "le-feedback-text";
    main.textContent = feedback.text;
    el.appendChild(main);

    // 原始错误详情折叠展示（textContent 渲染，绝不 innerHTML）
    if (feedback.detail) {
        const details = document.createElement("details");
        details.className = "le-feedback-detail";
        const summary = document.createElement("summary");
        summary.textContent = tt(i18n, "local_engine.error.detail", "诊断详情");
        details.appendChild(summary);
        const pre = document.createElement("pre");
        pre.textContent = feedback.detail;
        details.appendChild(pre);
        const copyBtn = document.createElement("button");
        copyBtn.className = "btn btn-small le-error-copy";
        copyBtn.textContent = tt(i18n, "local_engine.error.copy", "复制诊断");
        copyBtn.addEventListener("click", () => copyTextWithFeedback(copyBtn, feedback.detail, i18n));
        details.appendChild(copyBtn);
        el.appendChild(details);
    }
}

/** 维护面板更新：忙碌禁用 + 占用统计。 */
function updateMaintenance(panel, entry, i18n) {
    const repairBtn = panel.querySelector(".le-maintenance-repair");
    const cleanupBtn = panel.querySelector(".le-maintenance-cleanup");
    const blocked = isActionBlocked(entry, "repair");
    if (repairBtn && repairBtn.disabled !== blocked) {
        repairBtn.disabled = blocked;
    }
    const cleanupBlocked = isActionBlocked(entry, "cleanup");
    if (cleanupBtn && cleanupBtn.disabled !== cleanupBlocked) {
        cleanupBtn.disabled = cleanupBlocked;
    }

    const storageEl = panel.querySelector(".le-maintenance-storage");
    if (!storageEl) return;
    const storage = entry.storage;
    const total = storage?.total_size_bytes;
    const releasable = storage?.releasable_size_bytes;
    let text;
    if (total == null) {
        text = tt(i18n, "local_engine.storage.no_data", "占用统计加载中…");
    } else {
        const totalText = formatBytesLocal(total);
        text = releasable > 0
            ? `${tt(i18n, "local_engine.storage.actual", "实际占用")} ${totalText} · ${tt(i18n, "local_engine.storage.releasable", "可释放")} ${formatBytesLocal(releasable)}`
            : `${tt(i18n, "local_engine.storage.actual", "实际占用")} ${totalText}`;
    }
    if (storageEl.dataset.sig !== text) {
        storageEl.dataset.sig = text;
        storageEl.textContent = text;
    }
}

/** 字节格式化（与 card-utils 同规则；局部使用避免引入 Tauri 依赖链）。 */
function formatBytesLocal(bytes) {
    if (!bytes || bytes <= 0) return "0 B";
    const mb = bytes / (1024 * 1024);
    if (mb < 1) return `${Math.max(1, Math.round(bytes / 1024))} KB`;
    if (mb < 1024) return `${Math.round(mb)} MB`;
    return `${(mb / 1024).toFixed(1)} GB`;
}

/** adapter hook 配置区更新。 */
function updateConfigArea(container, entry, controller, i18n) {
    const engineId = entry.catalog.engine_id;
    const hook = adapterHooks.get(engineId);
    if (!hook || typeof hook.renderConfig !== "function") return;

    const sig = configAreaSignature(entry);
    if (container.dataset.renderSig === sig) return;
    container.dataset.renderSig = sig;

    container.textContent = "";
    try {
        hook.renderConfig(container, entry, controller);
    } catch (e) {
        console.error(`[le-card] adapter hook renderConfig failed for ${engineId}:`, e);
    }
}

// ── actions 点击处理 ───────────────────────────────────────────────────────────

/**
 * 主操作点击处理（生命周期动作）。cleanup 只打开 modal，绝不直接 invoke。
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
        case "cancel":
            if (entry.status?.status?.operation?.operation_id) {
                controller.cancel(engineId, entry.status.status.operation.operation_id).catch(() => {});
            }
            break;
    }
}
