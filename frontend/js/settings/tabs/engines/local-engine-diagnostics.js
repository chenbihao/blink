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
        renderDiagnosticContent(diagPanel, diag, i18n, engineId, entry, () => {
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

/**
 * 渲染诊断内容。
 */
function renderDiagnosticContent(container, diag, i18n, engineId, entry, onRefresh) {
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
