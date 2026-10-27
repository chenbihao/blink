/**
 * 配置同步（0.28.6：固定范围 WebDAV 同步；0.28.8：同步+动态流程合并单卡状态机；
 * 0.28.9：从独立 tab 合入存储页顶部——宿主 #sync-container 位置变了，逻辑不变）
 *
 * 交互流：
 * - 连接设置三项（地址/用户名/密码）——密码只存本机凭据管理器，回显掩码；
 * - 「配置同步」单卡状态机：idle（立即同步）/ 首次初始化 / 冲突裁决 / 同步报告。
 *   首次与冲突进行中主按钮让位给该轮流程操作（可取消回 idle）；
 *   报告态可收起，「立即同步」常驻可连续操作；
 * - 同步报告分组展示：已生效 / 本机覆盖 / 待装插件 / 待填密钥 / 保留的未知字段。
 */

import {confirmDialog, invoke, normalizeError} from "../../shared/tauri.js";
import {onLangChange, t} from "../../i18n/index.js";
import {iconHTML} from "../../shared/icon.js";

/** 模块状态（语言切换重渲染用） */
const state = {
    settings: null, // get_sync_settings 结果
    lastView: null, // 最近一次 run_sync 视图（needs_init/plan/report）
    busy: false, // 同步进行中（按钮禁用）
};

export function initSyncTab() {
    reloadSettings();
    onLangChange(() => render());
}

async function reloadSettings() {
    try {
        state.settings = await invoke("get_sync_settings");
    } catch (e) {
        state.settings = null;
        console.error("load sync settings failed:", e);
    }
    state.lastView = null;
    render();
}

function render() {
    const container = document.getElementById("sync-container");
    if (!container) return;
    container.innerHTML = renderAll();
    bindEvents(container);
}

// ── 渲染 ─────────────────────────────────────────────────────────────────────

function renderAll() {
    return renderSyncCard();
}

/**
 * 配置同步单卡（0.28.9：连接卡 + 同步卡合并——本质同一功能流）。
 * 卡内两个子区：连接设置（三字段 + 保存/测试）与同步状态机
 * （idle（立即同步）/ 首次初始化 / 冲突裁决 / 同步报告）。
 * 头部恒显上次同步状态；首次/冲突进行中主按钮让位给该轮流程；
 * 报告态报告与「立即同步」并存（分隔线隔开），可收起可连续同步。
 */
function renderSyncCard() {
    const s = state.settings || {url: "", username: "", has_password: false, password_hint: "", is_http: false};
    const last = s.last_sync;
    const pending = s.has_pending_conflicts;
    const overrides = s.overrides || 0;
    const view = state.lastView;

    let flowHtml = "";
    if (view?.outcome === "needs_init") flowHtml = renderInitBody(view);
    else if (view?.outcome === "awaiting_resolution") flowHtml = renderConflictBody(view.plan);
    else if (view?.outcome === "done") flowHtml = renderReportBody(view.report);
    // up_to_date 与无视图都落 idle（up_to_date 由状态行回显）
    const inFlow = view?.outcome === "needs_init" || view?.outcome === "awaiting_resolution";

    const connActions = `
        <div class="sync-actions">
          <button class="btn-small" data-action="save-conn" ${state.busy ? "disabled" : ""}>${t("sync.conn.save")}</button>
          <button class="btn-small" data-action="test-conn" ${state.busy ? "disabled" : ""}>${t("sync.conn.test")}</button>
          ${s.has_password ? `<button class="btn-small btn-danger" data-action="clear-password" ${state.busy ? "disabled" : ""}>${t("sync.conn.clear_password")}</button>` : ""}
        </div>`;

    const idleActions = `
        <div class="sync-actions">
          <button class="btn-primary" data-action="run-sync" ${state.busy ? "disabled" : ""}>${state.busy ? t("sync.run.busy") : t("sync.run.now")}</button>
          ${overrides > 0 ? `<button class="btn-small" data-action="reset-overrides" ${state.busy ? "disabled" : ""}>${t("sync.run.reset_overrides")}</button>` : ""}
        </div>`;

    return `
    <div class="extension-card">
      <div class="extension-header">
        <div class="extension-icon">${iconHTML("refresh-cw")}</div>
        <div class="extension-info">
          <h3>${t("sync.card.title")}</h3>
          <p class="extension-desc">${t("sync.card.desc")}</p>
        </div>
      </div>
      <div class="extension-body">
        <div class="sync-subhead">${t("sync.conn.title")}</div>
        <p class="extension-desc">${t("sync.conn.desc")}</p>
        <div class="setting-row">
          <label class="setting-label">${t("sync.conn.url")}</label>
          <input type="text" class="input-wide plugin-field" data-key="sync_url"
                 placeholder="${t("sync.conn.url.ph")}" value="${escapeAttr(s.url)}" />
        </div>
        <div class="setting-row">
          <label class="setting-label">${t("sync.conn.user")}</label>
          <input type="text" class="input-wide plugin-field" data-key="sync_username"
                 placeholder="${t("sync.conn.user.ph")}" value="${escapeAttr(s.username)}" />
        </div>
        <div class="setting-row">
          <label class="setting-label">${t("sync.conn.password")}</label>
          <input type="password" class="input-wide plugin-field" data-key="sync_password"
                 placeholder="${s.has_password ? t("sync.conn.password.saved", {hint: s.password_hint}) : t("sync.conn.password.ph")}" />
        </div>
        ${connActions}
        <p class="network-status hidden" id="sync-conn-status"></p>
        ${s.is_http ? `<p class="extension-desc sync-http-warning">${iconHTML("triangle-alert")} ${t("sync.conn.http_warning")}</p>` : ""}
        <p class="extension-desc">${t("sync.conn.scope_note")}</p>

        <div class="sync-subhead">${t("sync.run.title")}</div>
        <p class="extension-desc">${t("sync.run.desc")}</p>
        ${pending && !inFlow ? `<p class="extension-desc sync-pending-hint">${iconHTML("triangle-alert")} ${t("sync.run.pending_hint")}</p>` : ""}
        <p class="extension-desc">${last ? renderLastSync(last) : t("sync.run.never")}</p>
        ${flowHtml}
        ${flowHtml && !inFlow ? `<div class="sync-flow-divider"></div>` : ""}
        ${inFlow ? "" : idleActions}
        <p class="network-status hidden" id="sync-run-status"></p>
      </div>
    </div>`;
}

function renderLastSync(last) {
    const outcomeText = {
        up_to_date: t("sync.outcome.up_to_date"),
        done: t("sync.outcome.done"),
        awaiting_resolution: t("sync.outcome.awaiting"),
        error: t("sync.outcome.error"),
    }[last.outcome] || last.outcome;
    return t("sync.run.last", {
        time: formatSyncTime(last.at),
        revision: last.revision,
        outcome: outcomeText,
    });
}

/** 首次初始化（needs_init）：远端状态 + 方向选择，取消回 idle。 */
function renderInitBody(view) {
    const remote = view.remote_revision != null
        ? t("sync.init.remote_found", {
            revision: view.remote_revision,
            time: formatSyncTime(view.remote_updated_at || 0),
            device: view.remote_device || "?",
        })
        : t("sync.init.remote_empty");
    return `
        <p class="extension-desc"><strong>${t("sync.init.title")}</strong> — ${t("sync.init.desc")}</p>
        <p class="extension-desc">${remote}</p>
        <p class="extension-desc">${t("sync.init.local_items", {count: view.local_items})}</p>
        <div class="sync-actions">
          <button class="btn-primary" data-action="init-upload" ${state.busy ? "disabled" : ""}>${t("sync.init.upload")}</button>
          <button class="btn-small" data-action="init-download" ${state.busy || view.remote_revision == null ? "disabled" : ""}>${t("sync.init.download")}</button>
          <button class="btn-small" data-action="cancel-flow" ${state.busy ? "disabled" : ""}>${t("sync.flow.cancel")}</button>
        </div>
        <p class="extension-desc">${t("sync.init.confirm_note")}</p>`;
}

/** 冲突裁决（awaiting_resolution）：逐项/批量选择 + 提交，取消回 idle。 */
function renderConflictBody(plan) {
    const rows = plan.conflicts.map((c, i) => {
        const kindText = {
            value: t("sync.conflict.kind.value"),
            delete_vs_modify: t("sync.conflict.kind.delete"),
            order: t("sync.conflict.kind.order"),
            reference: t("sync.conflict.kind.reference"),
        }[c.kind] || c.kind;
        const hint = c.hint ? `<p class="sync-conflict-hint">${escapeHtml(c.hint)}</p>` : "";
        return `
        <div class="sync-conflict-row" data-conflict-idx="${i}">
          <div class="sync-conflict-head">
            <code class="sync-conflict-key">${escapeHtml(c.key)}</code>
            <span class="sync-conflict-kind">${kindText}</span>
          </div>
          ${hint}
          <div class="sync-conflict-choices">
            <label class="radio radio-inline"><input type="radio" name="conflict-${i}" value="local" /><span class="radio-dot"></span><span class="label-text">${t("sync.conflict.local")}</span></label>
            <pre class="sync-conflict-value">${escapeHtml(summarizeValue(c.local))}</pre>
            <label class="radio radio-inline"><input type="radio" name="conflict-${i}" value="remote" /><span class="radio-dot"></span><span class="label-text">${t("sync.conflict.remote")}</span></label>
            <pre class="sync-conflict-value">${escapeHtml(summarizeValue(c.remote))}</pre>
          </div>
        </div>`;
    }).join("");
    return `
        <p class="extension-desc"><strong>${t("sync.conflict.title", {count: plan.conflicts.length})}</strong> — ${t("sync.conflict.desc")}</p>
        ${rows}
        <div class="sync-actions">
          <button class="btn-small" data-action="conflict-all-local">${t("sync.conflict.all_local")}</button>
          <button class="btn-small" data-action="conflict-all-remote">${t("sync.conflict.all_remote")}</button>
          <button class="btn-primary" data-action="conflict-commit" ${state.busy ? "disabled" : ""}>${t("sync.conflict.commit")}</button>
          <button class="btn-small" data-action="cancel-flow" ${state.busy ? "disabled" : ""}>${t("sync.flow.cancel")}</button>
        </div>`;
}

/** 同步报告（done）：明细 + 收起；下方常驻「立即同步」可连续操作。 */
function renderReportBody(report) {
    const counts = countSummary(report.summary || []);
    const entries = (report.summary || []).map((e) => {
        const row = (icon, label, detail, cls) => `
          <div class="sync-report-row ${cls || ""}">
            <span class="sync-report-icon">${iconHTML(icon)}</span>
            <span class="sync-report-label">${escapeHtml(label)}</span>
            <span class="sync-report-detail">${escapeHtml(detail)}</span>
          </div>`;
        switch (e.kind) {
            case "applied":
                return row(
                    "check",
                    t("sync.report.applied"),
                    `${shortItem(e.item)} · ${e.from === "remote" ? t("sync.report.from_remote") : t("sync.report.from_local")}`,
                );
            case "overridden":
                return row("triangle-alert", t("sync.report.overridden"), `${shortItem(e.item)} · ${e.reason}`, "sync-report-warn");
            case "pending_plugin":
                return row("download", t("sync.report.pending_plugin"), e.plugin_id);
            case "pending_secret":
                return row("shield", t("sync.report.pending_secret"), e.provider_id);
            case "preserved_unknown":
                return row("shield", t("sync.report.preserved"), shortItem(e.item));
            default:
                return "";
        }
    }).join("");
    return `
        ${report.degraded_cas ? `<p class="extension-desc sync-http-warning">${iconHTML("triangle-alert")} ${t("sync.report.degraded")}</p>` : ""}
        <p class="extension-desc"><strong>${t("sync.report.title", {revision: report.revision})}</strong> · ${t("sync.report.counts", {
            remote: counts.appliedRemote, local: counts.appliedLocal, overridden: counts.overridden,
            plugins: counts.pendingPlugins, secrets: counts.pendingSecrets, unknown: counts.preservedUnknown,
        })}</p>
        ${entries || `<p class="extension-desc">${t("sync.report.empty")}</p>`}
        <div class="sync-actions">
          <button class="btn-small" data-action="dismiss-report">${t("sync.report.dismiss")}</button>
        </div>
        ${report.backup ? `<p class="extension-desc sync-backup-note">${t("sync.report.backup", {file: report.backup})}</p>` : ""}`;
}

// ── 事件 ─────────────────────────────────────────────────────────────────────

function bindEvents(container) {
    // kind 三态：true=失败（红）/ "ok"=成功（绿）/ 其他=中性（进行中、已保存等）
    const status = (id, message, kind) => {
        const el = container.querySelector(id);
        if (!el) return;
        if (!message) {
            el.classList.add("hidden");
            el.textContent = "";
            return;
        }
        el.classList.remove("hidden");
        el.textContent = message;
        el.classList.toggle("network-status-error", kind === true);
        el.classList.toggle("network-status-ok", kind === "ok");
    };

    const field = (key) => container.querySelector(`.plugin-field[data-key="${key}"]`)?.value?.trim() || "";
    const setBusy = (busy) => {
        state.busy = busy;
        render();
        // 重渲染后恢复状态行
        if (busy) status("#sync-run-status", t("sync.run.busy"), false);
    };

    container.querySelectorAll("button[data-action]").forEach((btn) => {
        btn.addEventListener("click", async () => {
            const action = btn.dataset.action;
            try {
                switch (action) {
                    case "save-conn": {
                        const url = field("sync_url");
                        const username = field("sync_username");
                        const password = container.querySelector('.plugin-field[data-key="sync_password"]')?.value || "";
                        if (!url || !username) {
                            status("#sync-conn-status", t("sync.err.incomplete"), true);
                            return;
                        }
                        state.settings = await invoke("save_sync_settings", {
                            url, username,
                            password: password || null,
                        });
                        render();
                        status("#sync-conn-status", t("sync.conn.saved"), false);
                        break;
                    }
                    case "clear-password":
                        state.settings = await invoke("clear_sync_password");
                        render();
                        status("#sync-conn-status", t("sync.conn.password.cleared"), false);
                        break;
                    case "test-conn": {
                        status("#sync-conn-status", t("sync.conn.testing"), false);
                        // 表单直测：传输入框当前值（密码空时后端回退已存密码），
                        // 保存前即可验证；后端在值为空时回退已保存设置。
                        const result = await invoke("test_sync_connection", {
                            url: field("sync_url") || null,
                            username: field("sync_username") || null,
                            password: container.querySelector('.plugin-field[data-key="sync_password"]')?.value || null,
                        });
                        if (result.status === "ok") {
                            status("#sync-conn-status", t("sync.conn.ok", {revision: result.head_revision ?? 0}), "ok");
                        } else {
                            status("#sync-conn-status", t("sync.conn.empty_server"), "ok");
                        }
                        break;
                    }
                    case "run-sync":
                    case "conflict-commit": {
                        const resolutions = action === "conflict-commit" ? collectResolutions(container) : [];
                        if (action === "conflict-commit" && resolutions == null) {
                            status("#sync-run-status", t("sync.conflict.incomplete"), true);
                            return;
                        }
                        setBusy(true);
                        try {
                            const planId = action === "conflict-commit" ? state.lastView?.plan?.plan_id : null;
                            state.lastView = await invoke("run_sync", {
                                resolutions,
                                planId: planId || null,
                            });
                        } finally {
                            setBusy(false);
                        }
                        await refreshSettingsQuiet();
                        if (state.lastView?.outcome === "up_to_date") {
                            status("#sync-run-status", t("sync.outcome.up_to_date"), false);
                        }
                        break;
                    }
                    case "conflict-all-local":
                    case "conflict-all-remote": {
                        const value = action.endsWith("local") ? "local" : "remote";
                        container.querySelectorAll("input[type=radio][name^=conflict-]").forEach((r) => {
                            r.checked = r.value === value;
                        });
                        return;
                    }
                    case "init-upload":
                    case "init-download": {
                        const directionKey = action === "init-upload" ? "upload" : "download";
                        const ok = await confirmDialog(t("sync.init.confirm", {direction: t(`sync.init.${directionKey}`)}));
                        if (!ok) return;
                        setBusy(true);
                        try {
                            const report = await invoke("sync_initialize", {
                                direction: action === "init-upload" ? "upload" : "download",
                            });
                            state.lastView = {outcome: "done", report};
                        } finally {
                            setBusy(false);
                        }
                        await refreshSettingsQuiet();
                        break;
                    }
                    case "cancel-flow":
                    case "dismiss-report": {
                        // 放弃本轮首次/冲突流程（回 idle）或收起报告
                        state.lastView = null;
                        render();
                        return;
                    }
                    case "reset-overrides": {
                        await invoke("reset_sync_overrides");
                        await reloadSettings();
                        return;
                    }
                    default:
                        return;
                }
            } catch (e) {
                console.error("sync action failed:", e);
                const err = normalizeError(e);
                const message = err.code === "sync_needs_init"
                    ? t("sync.err.needs_init")
                    : err.code === "sync_stale_plan"
                        ? t("sync.err.stale_plan")
                        : err.code === "sync_schema_too_new"
                            ? t("sync.err.schema_too_new")
                            : err.message;
                // 0.28.8 单卡状态机后同步流程共用一个状态行：连接卡三操作回连接卡，
                // 其余（同步/首次/冲突/报告）全落同步卡唯一状态行
                const target = ["save-conn", "test-conn", "clear-password"].includes(action)
                    ? "#sync-conn-status"
                    : "#sync-run-status";
                status(target, message, true);
            }
        });
    });
}

async function refreshSettingsQuiet() {
    try {
        state.settings = await invoke("get_sync_settings");
    } catch (e) {
        console.error("refresh sync settings failed:", e);
    }
    render();
}

/** 收集冲突裁决；未全部选择时返回 null */
function collectResolutions(container) {
    const plan = state.lastView?.plan;
    if (!plan) return [];
    const resolutions = [];
    for (let i = 0; i < plan.conflicts.length; i += 1) {
        const choice = container.querySelector(`input[type=radio][name="conflict-${i}"]:checked`)?.value;
        if (!choice) return null;
        resolutions.push({key: plan.conflicts[i].key, choice});
    }
    return resolutions;
}

// ── 纯工具（node --test 覆盖）────────────────────────────────────────────────

/** 计数摘要（报告头）。 */
export function countSummary(summary) {
    const counts = {appliedRemote: 0, appliedLocal: 0, overridden: 0, pendingPlugins: 0, pendingSecrets: 0, preservedUnknown: 0};
    for (const entry of summary) {
        switch (entry.kind) {
            case "applied":
                if (entry.from === "remote") counts.appliedRemote += 1;
                else counts.appliedLocal += 1;
                break;
            case "overridden": counts.overridden += 1; break;
            case "pending_plugin": counts.pendingPlugins += 1; break;
            case "pending_secret": counts.pendingSecrets += 1; break;
            case "preserved_unknown": counts.preservedUnknown += 1; break;
            default: break;
        }
    }
    return counts;
}

/** 冲突值的人读摘要：长字符串截断、对象紧凑化。 */
export function summarizeValue(value) {
    if (value === null || value === undefined) return "∅";
    let text;
    if (typeof value === "string") {
        text = value;
    } else {
        try {
            text = JSON.stringify(value);
        } catch {
            text = String(value);
        }
    }
    if (text.length > 80) {
        return `${text.slice(0, 77)}…`;
    }
    return text || '""';
}

/** item key 的短展示（去掉分片前缀，保留可读段）。 */
export function shortItem(item) {
    const hash = item.indexOf("#");
    return hash >= 0 ? item.slice(hash + 1) : item;
}

/** Unix 秒 → 本地时间文本（无效返回 "-"）。 */
export function formatSyncTime(unixSeconds) {
    if (!unixSeconds) return "-";
    const date = new Date(unixSeconds * 1000);
    const pad = (n) => String(n).padStart(2, "0");
    return `${date.getFullYear()}-${pad(date.getMonth() + 1)}-${pad(date.getDate())} ${pad(date.getHours())}:${pad(date.getMinutes())}`;
}

function escapeHtml(str) {
    return String(str).replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;");
}

function escapeAttr(str) {
    return String(str).replace(/"/g, "&quot;").replace(/'/g, "&#39;");
}
