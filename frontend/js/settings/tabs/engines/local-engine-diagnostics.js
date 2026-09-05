/**
 * 引擎诊断面板 + 孤儿进程恢复（0.22.6 从 local-engine-card.js 拆出）。
 *
 * - 诊断是快照：面板内提供"重新诊断"按钮，异步回流校验 requestId。
 * - orphan 恢复：后端 DTO { present, actionable, reason }，前端只检查
 *   actionable 决定是否显示停止入口，不拼接 process/service 推断。
 * - 确认模态与通知只用 CSS class，不写 inline style、不硬编码颜色。
 * - 不使用原生 confirm()/alert()。
 *
 * @module local-engine-diagnostics
 */

import {renderIcon} from "../../../shared/icon.js";
import {tt, copyTextWithFeedback} from "./local-engine-card-utils.js";
import {formatLocalLogTimestamp} from "./local-engine-log-format.js";

/**
 * 展示引擎诊断信息（展开/收起切换）。
 * 从后端 get_engine_diagnostics 拉取并展示。
 * @param {Object} entry - EngineStateEntry
 * @param {Object} controller
 * @param {Object} i18n
 * @param {HTMLElement} anchorBtn - 诊断按钮
 * @param {HTMLElement} diagPanel - 诊断面板容器
 */
export function showEngineDiagnostics(entry, controller, i18n, anchorBtn, diagPanel) {
    const engineId = entry.catalog?.engine_id;
    if (!engineId || !controller?.getDiagnostics) return;

    if (!diagPanel.hidden) {
        diagPanel.hidden = true;
        anchorBtn.setAttribute("aria-expanded", "false");
        return;
    }

    diagPanel.hidden = false;
    anchorBtn.setAttribute("aria-expanded", "true");
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
        if (!diag || diag.engine_id !== engineId) {
            throw new Error(tt(i18n, "local_engine.diagnostic.engine_mismatch", "诊断响应与当前引擎不匹配"));
        }
        renderDiagnosticContent(diagPanel, diag, i18n, engineId, entry, controller, () => {
            refreshEngineDiagnostics(entry, controller, i18n, diagPanel);
        });
    }).catch((e) => {
        if (diagPanel.hidden || diagPanel.dataset.requestId !== requestId) return;
        diagPanel.textContent = "";
        const errEl = document.createElement("div");
        errEl.className = "le-diagnostic-error";
        errEl.textContent = e?.message || String(e);
        diagPanel.appendChild(errEl);
    });
}

function diagnosticTone(kind, value) {
    if (kind === "environment") {
        if (value === "ready") return "ok";
        if (value === "missing" || value === "needs_rebuild") return "warn";
        if (value === "broken") return "error";
    }
    if (kind === "process") {
        if (value === "running") return "ok";
        if (value === "starting" || value === "stopping") return "busy";
        if (value === "exited") return "error";
    }
    if (kind === "service") {
        if (value === "healthy") return "ok";
        if (value === "unreachable") return "error";
        if (value === "degraded") return "warn";
    }
    if (kind === "model") {
        if (value === "ready") return "ok";
        if (value === "loading" || value === "downloading") return "busy";
        if (value === "failed") return "error";
    }
    return "neutral";
}

function diagnosticStatusText(i18n, kind, value) {
    if (!value) return "—";
    const prefix = kind === "environment" ? "env"
        : kind === "model" ? "model_state"
            : kind;
    return tt(i18n, `local_engine.${prefix}.${value}`, value);
}

function adapterLabel(i18n, key) {
    return tt(i18n, `local_engine.diagnostic.adapter.${key}`, key.replaceAll("_", " "));
}

function adapterValue(i18n, item) {
    if (item.value === "true") return tt(i18n, "local_engine.diagnostic.check_passed", "通过");
    if (item.value === "false") return tt(i18n, "local_engine.diagnostic.check_failed", "未通过");
    return item.value || "—";
}

function adapterTone(item) {
    if (item.value === "false") return item.label === "error" ? "error" : "warn";
    if (item.label === "error") return "error";
    if (item.label === "warning") return "warn";
    if (item.value === "true") return "ok";
    return "neutral";
}

function selectedModelChecks(entry, i18n) {
    const models = Array.isArray(entry?.models) ? entry.models : [];
    // 只有 STT 引擎有用户级可选模型目录（0.22.9 起唯一产品引擎 funasr）；
    // paddleocr 等固定模型契约的引擎不渲染"当前模型"检查——
    // 显示"未选择"警告反而是误导
    const hasModelCatalog = entry?.catalog?.capability_kind === "stt";
    if (!hasModelCatalog) {
        return [];
    }
    const selected = models.find((model) => model.is_selected);
    if (!selected) {
        const notSelected = tt(i18n, "local_engine.diagnostic.model_not_selected", "未选择");
        return [
            {
                label: tt(i18n, "local_engine.diagnostic.selected_model", "当前模型"),
                value: notSelected,
                tone: "warn",
            },
            {
                label: tt(i18n, "local_engine.diagnostic.model_artifact", "模型文件"),
                value: notSelected,
                tone: "warn",
            },
        ];
    }

    const displayName = selected.display_name || selected.model_id || "—";
    const installed = selected.install_state === "installed";
    const verified = selected.verification_state === "verified";
    const verificationFailed = selected.verification_state === "corrupted"
        || selected.verification_state === "mismatched";
    const assetTone = verificationFailed ? "error" : installed && verified ? "ok" : "warn";
    const assetValue = verificationFailed
        ? tt(i18n, `local_engine.model.verification.${selected.verification_state}`, selected.verification_state)
        : installed
            ? tt(i18n, `local_engine.model.verification.${selected.verification_state}`, selected.verification_state || "installed")
            : tt(i18n, "local_engine.model.install.not_installed", "未下载");

    return [
        {
            label: tt(i18n, "local_engine.diagnostic.selected_model", "当前模型"),
            value: displayName,
            tone: "neutral",
        },
        {
            label: tt(i18n, "local_engine.diagnostic.model_artifact", "模型文件"),
            value: assetValue,
            tone: assetTone,
        },
    ];
}

function renderCheck(container, check) {
    const row = document.createElement("div");
    row.className = `le-diagnostic-check le-diagnostic-check-${check.tone || "neutral"}`;
    // 图标语义（两引擎同一套）：✓ 通过 / ✗ 异常 / ⚠ 需要注意 / ○ 进行中。
    // neutral 是纯信息行（如"当前模型：SenseVoice Small"）——没有好坏，
    // 不渲染图标（空占位保持三列网格对齐），不再让读者猜圆圈含义。
    const tone = check.tone || "neutral";
    const iconName = tone === "ok" ? "check"
        : tone === "error" ? "x"
            : tone === "warn" ? "triangle-alert"
                : tone === "busy" ? "circle" : null;
    if (iconName) {
        row.appendChild(renderIcon(iconName, {extraClass: "le-diagnostic-check-icon"}));
    } else {
        const placeholder = document.createElement("span");
        placeholder.className = "le-diagnostic-check-icon";
        placeholder.setAttribute("aria-hidden", "true");
        row.appendChild(placeholder);
    }

    const label = document.createElement("span");
    label.className = "le-diagnostic-check-label";
    label.textContent = check.label;
    row.appendChild(label);

    const value = document.createElement("span");
    value.className = "le-diagnostic-check-value";
    value.textContent = check.value;
    row.appendChild(value);
    container.appendChild(row);
}

/**
 * 渲染诊断内容。
 */
function renderDiagnosticContent(container, diag, i18n, engineId, entry, controller, onRefresh) {
    container.textContent = "";

    // 头部：标题 + 重新诊断 + 复制诊断（诊断是快照）
    const header = document.createElement("div");
    header.className = "le-diagnostic-header";
    const title = document.createElement("span");
    title.className = "le-diagnostic-title";
    title.textContent = tt(i18n, "local_engine.diagnostic.title", "引擎诊断");
    header.appendChild(title);
    const headerActions = document.createElement("div");
    headerActions.className = "le-diagnostic-actions";
    if (typeof onRefresh === "function") {
        const refreshBtn = document.createElement("button");
        refreshBtn.className = "btn btn-small le-action-btn le-diagnostic-refresh";
        refreshBtn.type = "button";
        refreshBtn.appendChild(renderIcon("refresh-cw", {extraClass: "le-action-icon"}));
        const refreshLabel = document.createElement("span");
        refreshLabel.textContent = tt(i18n, "local_engine.diagnostic.refresh", "重新诊断");
        refreshBtn.appendChild(refreshLabel);
        refreshBtn.addEventListener("click", onRefresh);
        headerActions.appendChild(refreshBtn);
    }
    const copyBtn = document.createElement("button");
    copyBtn.className = "btn btn-small le-diagnostic-copy";
    copyBtn.textContent = tt(i18n, "local_engine.diagnostic.copy", "复制诊断");
    copyBtn.addEventListener("click", () => {
        const text = JSON.stringify(diag, null, 2);
        copyTextWithFeedback(copyBtn, text, i18n);
    });
    headerActions.appendChild(copyBtn);
    header.appendChild(headerActions);
    container.appendChild(header);

    // 可行动检查清单：先展示用户关心的安装/模型/进程/服务，再展示 adapter
    // 专属环境检查。日志只是后续证据，不再占据诊断主体。
    const checklist = document.createElement("div");
    checklist.className = "le-diagnostic-checklist";
    const processState = diag.process?.state || "unknown";
    const checks = [
        {
            label: tt(i18n, "local_engine.diagnostic.engine_deployment", "引擎部署"),
            value: diagnosticStatusText(i18n, "environment", diag.environment),
            tone: diagnosticTone("environment", diag.environment),
        },
        ...selectedModelChecks(entry, i18n),
        {
            label: tt(i18n, "local_engine.diagnostic.model_runtime", "模型加载"),
            value: diagnosticStatusText(i18n, "model", diag.model),
            tone: diagnosticTone("model", diag.model),
        },
        {
            label: tt(i18n, "local_engine.diagnostic.process", "进程"),
            value: diagnosticStatusText(i18n, "process", processState),
            tone: diagnosticTone("process", processState),
        },
        {
            label: tt(i18n, "local_engine.diagnostic.service", "服务"),
            value: diagnosticStatusText(i18n, "service", diag.service),
            tone: diagnosticTone("service", diag.service),
        },
        ...(diag.adapter_diagnostics || []).map((item) => ({
            label: adapterLabel(i18n, item.key),
            value: adapterValue(i18n, item),
            tone: adapterTone(item),
        })),
    ];
    for (const check of checks) renderCheck(checklist, check);
    container.appendChild(checklist);

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
    const logsDetails = document.createElement("details");
    logsDetails.className = "le-diagnostic-logs";
    const logsTitle = document.createElement("summary");
    logsTitle.className = "le-diagnostic-logs-title";
    logsTitle.textContent = tt(i18n, "local_engine.diagnostic.recent_logs", "最近日志")
        + `（${logs.length}）`
        + (logsSource === "session"
            ? tt(i18n, "local_engine.diagnostic.logs_from_session", "（本次会话）")
            : "");
    logsDetails.appendChild(logsTitle);
    if (logs.length > 0) {

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
        logsDetails.appendChild(logsList);
    } else {
        const noLogs = document.createElement("div");
        noLogs.className = "le-diagnostic-logs-empty";
        noLogs.textContent = tt(i18n, "local_engine.diagnostic.no_logs", "无日志");
        logsDetails.appendChild(noLogs);
    }
    container.appendChild(logsDetails);

    // ── 停止孤儿引擎按钮 ──────────────────────────────────────────
    // 后端提供闭合 DTO { present, actionable, reason }，
    // 前端只检查 actionable === true 来决定是否显示停止入口。
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

// ── orphan 确认模态 + 通知（样式在 settings-local-runtime.css）───────────────

/**
 * 显示确认模态（不使用原生 confirm()）。
 * @param {string} message - 确认文案
 * @param {Function} onConfirm - 用户确认时回调
 */
function showOrphanConfirmModal(message, onConfirm) {
    // 已有 overlay 先移除，避免叠加
    document.querySelector(".le-orphan-modal-overlay")?.remove();

    const overlay = document.createElement("div");
    overlay.className = "le-orphan-modal-overlay";

    const modal = document.createElement("div");
    modal.className = "le-orphan-modal";

    const msg = document.createElement("p");
    msg.className = "le-orphan-modal-msg";
    msg.textContent = message;
    modal.appendChild(msg);

    const btnRow = document.createElement("div");
    btnRow.className = "le-orphan-modal-actions";

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
 * 显示通知消息（不使用原生 alert()），3 秒后自动消失。
 * @param {string} message - 通知文案
 * @param {boolean} isSuccess - 是否为成功消息
 */
function showOrphanNotification(message, isSuccess) {
    document.querySelector(".le-orphan-notification")?.remove();

    const notif = document.createElement("div");
    notif.className = isSuccess
        ? "le-orphan-notification le-orphan-notification-ok"
        : "le-orphan-notification le-orphan-notification-err";
    notif.textContent = message;
    document.body.appendChild(notif);

    setTimeout(() => notif.remove(), 3000);
}
