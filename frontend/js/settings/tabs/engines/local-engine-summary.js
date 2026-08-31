/**
 * 卡片紧凑投影纯函数（中密度纵向重设计）。
 *
 * 全部为无 DOM、无 Tauri 依赖的纯投影：输入 EngineStateEntry（+ i18n 取词
 * 函数），输出渲染所需的结构化数据。DOM 装配在 local-engine-card.js，
 * 业务状态机在后端与 local-engine-state.js——本模块只做视觉投影，
 * 不派生新的业务真源（"综合摘要只能由现有后端状态字段派生"）。
 *
 * 投影项：
 * - computeEngineSummary：卡片头部综合摘要（状态 · 模型 · 设备/策略）
 * - computeFeedback：反馈槽（operation > 错误 > 模型操作 > 待重启 > 空闲）
 * - computeKeyline：关键状态行（环境 / 模型 / 服务 / 生命周期策略）
 * - computeModelSummary：selected/installed/active 三身份摘要
 * - primaryActionView：唯一主操作（按钮 kind/label/icon/disabled）
 * - computeRuntimeSummary：页面顶部运行时摘要（正常度/引擎数/运行数/占用）
 *
 * @module local-engine-summary
 */

import {
    hasActiveOperation,
    isOperationCancellable,
    getPrimaryAction,
} from "./local-engine-state.js";

// ── 内部常量 ─────────────────────────────────────────────────────────────────

/** operation 终态（与 state.js hasActiveOperation 保持一致）。 */
const OP_TERMINAL_STAGES = ["completed", "cancelled", "failed"];

/** 模型安装态中的"进行中"集合（用于反馈槽与摘要）。 */
const MODEL_ACTIVE_STATES = ["downloading", "staging", "verifying", "repairing", "deleting"];

// ── i18n 取词辅助 ─────────────────────────────────────────────────────────────

/**
 * 取词：命中返回文案，未命中返回**已插值**的 fallback。t 可为 settings i18n
 * 注入对象或 i18n/index 的 t。
 * @param {Function|null} t
 * @param {string} key
 * @param {string} fallback
 * @param {Object} [params]
 * @returns {string}
 */
function tx(t, key, fallback, params) {
    if (typeof t === "function") {
        const value = t(key, params);
        if (value && value !== key) return value;
    }
    if (!params) return fallback;
    return fallback.replace(/\{(\w+)\}/g, (_, name) => String(params[name] ?? ""));
}

/** wire value → i18n 文案（环境）。 */
function envLabel(t, value) {
    const map = {
        missing: ["local_engine.env.missing", "未安装"],
        ready: ["local_engine.env.ready", "已安装"],
        broken: ["local_engine.env.broken", "已损坏"],
        needs_rebuild: ["local_engine.env.needs_rebuild", "待重建"],
    };
    const hit = map[value];
    return hit ? tx(t, hit[0], hit[1]) : (value || "—");
}

/** wire value → i18n 文案（服务）。 */
function serviceLabel(t, value) {
    const map = {
        healthy: ["local_engine.service.healthy", "可用"],
        unreachable: ["local_engine.service.unreachable", "不可用"],
        degraded: ["local_engine.service.degraded", "降级"],
        unknown: ["local_engine.service.unknown", "未知"],
    };
    const hit = map[value];
    return hit ? tx(t, hit[0], hit[1]) : (value || "—");
}

/** wire value → i18n 文案（模型健康态）。 */
function modelLabel(t, value) {
    const map = {
        ready: ["local_engine.model_state.ready", "已就绪"],
        not_loaded: ["local_engine.model_state.not_loaded", "未加载"],
        downloading: ["local_engine.model_state.downloading", "下载中"],
        loading: ["local_engine.model_state.loading", "加载中"],
        failed: ["local_engine.model_state.failed", "失败"],
        unknown: ["local_engine.model_state.unknown", "未知"],
    };
    const hit = map[value];
    return hit ? tx(t, hit[0], hit[1]) : (value || "—");
}

/** operation kind → i18n 文案。 */
function opKindLabel(t, kind) {
    return tx(t, `local_engine.operation.${kind}`, kind);
}

/** operation stage → i18n 文案。 */
function opStageLabel(t, stage) {
    return tx(t, `local_engine.operation.stage.${stage}`, stage);
}

// ── 通用小工具 ────────────────────────────────────────────────────────────────

/** 字节数格式化（B/MB/GB）——与 card-utils 同规则，但保持本模块零依赖。 */
function formatBytesLocal(bytes) {
    if (!bytes || bytes <= 0) return "0 B";
    const mb = bytes / (1024 * 1024);
    if (mb < 1) return `${Math.max(1, Math.round(bytes / 1024))} KB`;
    if (mb < 1024) return `${Math.round(mb)} MB`;
    return `${(mb / 1024).toFixed(1)} GB`;
}

/** MB 数值格式化。 */
function formatMBLocal(mb) {
    if (mb == null) return null;
    if (mb < 1024) return `${Math.round(mb)} MB`;
    return `${(mb / 1024).toFixed(1)} GB`;
}

/** 截断文本（错误摘要单行化）。 */
function shortText(text, max = 48) {
    const s = String(text || "").trim();
    if (!s) return "";
    return s.length > max ? `${s.slice(0, max - 1)}…` : s;
}

/** 活跃 operation（未到终态且 kind != idle）。 */
function activeOp(entry) {
    const op = entry?.status?.status?.operation;
    if (!op || op.kind === "idle") return null;
    if (OP_TERMINAL_STAGES.includes(op.stage)) return null;
    return op;
}

/** 计算设备展示值：实际后端 > 请求偏好 > catalog 快照。 */
function deviceLabel(t, entry) {
    const backend = entry?.status?.status?.backend;
    const actual = backend?.backend_verification?.actual_backend;
    const requested = backend?.requested_preference
        || entry?.catalog?.current_compute_preference
        || "auto";
    const value = actual || requested;
    return tx(t, `local_engine.compute.${value}`, value);
}

/** 模型展示名：active > selected > catalog 默认。 */
function currentModelName(entry) {
    const models = entry?.models;
    if (Array.isArray(models)) {
        const active = models.find((m) => m.is_active);
        if (active) return active.display_name || active.model_id;
        const selected = models.find((m) => m.is_selected);
        if (selected) return selected.display_name || selected.model_id;
    }
    return entry?.catalog?.model_id || null;
}

/** 生命周期策略展示（FunASR 用 auto_start，其余用 catalog lifecycle）。 */
function policyLabel(t, entry) {
    const lifecycle = entry?.catalog?.lifecycle || "manual";
    if (lifecycle === "manual") {
        const autoStart = entry?.preferences?.auto_start;
        if (autoStart === true) {
            return tx(t, "local_engine.lifecycle.auto_start", "自动启动");
        }
        return tx(t, "local_engine.lifecycle.manual_entry", "手动启动");
    }
    return tx(t, `local_engine.lifecycle.${lifecycle}`, lifecycle);
}

/** 引擎是否按需类生命周期。 */
function isOnDemand(entry) {
    const lifecycle = entry?.catalog?.lifecycle;
    return lifecycle === "on_demand" || lifecycle === "stop_after_use";
}

// ── 综合摘要 ──────────────────────────────────────────────────────────────────

/**
 * 卡片头部综合摘要。
 *
 * 优先级（全部由后端状态字段派生，不持久化）：
 * 活跃 operation / 乐观 pending > backend mismatch > last_error >
 * 环境 missing/broken/needs_rebuild > 进程 starting/stopping >
 * 服务异常（进程存活但服务不可用，必须显式暴露）> available >
 * 已就绪/已停止。
 *
 * @param {Object} entry - EngineStateEntry
 * @param {Function|null} t - i18n 取词函数
 * @returns {{text: string, tone: "ok"|"busy"|"error"|"warn"|"muted"|"neutral"}}
 */
export function computeEngineSummary(entry, t) {
    const s = entry?.status?.status;
    if (!s) {
        return {text: tx(t, "local_engine.summary.loading", "状态加载中"), tone: "muted"};
    }

    // 活跃 operation / 乐观 pending → "正在{kind} · {stage}"
    const op = activeOp(entry);
    if (op) {
        return {
            text: tx(t, "local_engine.summary.op", "正在{kind} · {stage}", {
                kind: opKindLabel(t, op.kind),
                stage: opStageLabel(t, op.stage),
            }),
            tone: "busy",
        };
    }
    if (entry.pendingAction) {
        return {
            text: tx(t, "local_engine.summary.pending", "{kind} · 等待响应", {
                kind: opKindLabel(t, entry.pendingAction.kind),
            }),
            tone: "busy",
        };
    }

    // backend mismatch（期望与实际后端不一致——启动失败的典型形态）
    const verification = s.backend?.backend_verification;
    if (verification && verification.actual_backend
        && verification.expected_backend
        && verification.actual_backend !== verification.expected_backend) {
        return {
            text: tx(t, "local_engine.summary.backend_mismatch", "启动失败 · 后端身份不匹配"),
            tone: "error",
        };
    }

    // last_error（operation 非活跃时的残留错误）
    if (s.last_error) {
        const err = s.last_error;
        const brief = shortText(err.action_hint || err.message || err.code);
        return {
            text: tx(t, "local_engine.summary.error", "出现错误 · {detail}", {detail: brief || "—"}),
            tone: "error",
        };
    }

    // 环境状态
    switch (s.environment) {
        case "unknown":
            return {text: tx(t, "local_engine.summary.checking", "正在检查环境"), tone: "muted"};
        case "missing":
            return {text: tx(t, "local_engine.summary.missing", "未安装 · 需要安装环境"), tone: "muted"};
        case "broken":
            return {text: tx(t, "local_engine.summary.broken", "环境损坏 · 需要修复"), tone: "error"};
        case "needs_rebuild":
            return {text: tx(t, "local_engine.summary.needs_rebuild", "待重建 · 计算设备已变更"), tone: "warn"};
        default:
            break;
    }

    // 进程瞬态
    const processState = s.process?.state;
    if (processState === "starting") {
        return {text: tx(t, "local_engine.summary.starting", "启动中"), tone: "busy"};
    }
    if (processState === "stopping") {
        return {text: tx(t, "local_engine.summary.stopping", "停止中"), tone: "busy"};
    }

    // 进程存活但服务不可用——不能用"运行中"掩盖服务异常
    if (processState === "running" && !s.available) {
        if (s.service === "unreachable" || s.service === "unknown") {
            return {
                text: tx(t, "local_engine.summary.service_unreachable", "服务异常 · 进程仍在运行"),
                tone: "error",
            };
        }
        if (s.service === "degraded") {
            return {
                text: tx(t, "local_engine.summary.service_degraded", "服务降级 · 可用性受限"),
                tone: "warn",
            };
        }
        // 服务 healthy 但模型未就绪（下载/加载中）
        if (s.model === "downloading" || s.model === "loading") {
            return {
                text: `${modelLabel(t, s.model)} · ${currentModelName(entry) || ""}`.replace(/ · $/, ""),
                tone: "busy",
            };
        }
        if (s.model === "failed") {
            return {
                text: `${tx(t, "local_engine.summary.model_failed", "模型加载失败")} · ${currentModelName(entry) || ""}`.replace(/ · $/, ""),
                tone: "error",
            };
        }
    }

    // 可用 → 运行中 · 模型 · 设备
    if (s.available) {
        const parts = [tx(t, "local_engine.summary.running", "运行中")];
        const model = currentModelName(entry);
        if (model) parts.push(model);
        parts.push(deviceLabel(t, entry));
        return {text: parts.join(" · "), tone: "ok"};
    }

    // 已安装未运行 → 已就绪 · 模型 · 策略
    const parts = [tx(t, "local_engine.summary.ready", "已就绪")];
    const model = currentModelName(entry);
    if (model) parts.push(model);
    parts.push(policyLabel(t, entry));
    return {text: parts.join(" · "), tone: "neutral"};
}

// ── 反馈槽 ────────────────────────────────────────────────────────────────────

/**
 * 卡片反馈槽内容。
 *
 * 优先级（高 → 低）：
 * 1. 引擎 operation（活跃）或乐观 pending
 * 2. 模型级进行中操作（下载/校验/修复/删除）
 * 3. 当前引擎瞬时命令错误
 * 4. last_error（错误摘要默认直接可见；detail 折叠展示）
 * 5. selected ≠ active（待重启/未生效）
 * 6. requires_rebuild / environment needs_rebuild
 * 7. 空闲说明（按环境 + 生命周期派生）
 *
 * @param {Object} entry
 * @param {Function|null} t
 * @returns {{tone: string, text: string, detail?: string}}
 */
export function computeFeedback(entry, t) {
    const s = entry?.status?.status;

    // 1. 引擎 operation
    const op = activeOp(entry);
    if (op) {
        return {
            tone: "busy",
            text: tx(t, "local_engine.feedback.op", "正在{kind} · {stage}", {
                kind: opKindLabel(t, op.kind),
                stage: opStageLabel(t, op.stage),
            }),
        };
    }
    if (entry.pendingAction) {
        return {
            tone: "busy",
            text: tx(t, "local_engine.feedback.pending", "{kind}已发出 · 等待后端响应", {
                kind: opKindLabel(t, entry.pendingAction.kind),
            }),
        };
    }

    // 2. 模型级进行中操作
    const modelOp = findActiveModelOp(entry, t);
    if (modelOp) {
        return {tone: "busy", text: modelOp.text};
    }

    // 3. 当前引擎瞬时命令错误（页面级错误区不得承接单引擎错误）
    if (entry?.transientError) {
        const err = entry.transientError;
        const main = err.action_hint || err.message
            || tx(t, `local_engine.error.${err.code}`, err.code)
            || tx(t, "local_engine.error.unknown_error", "未知错误");
        const detail = err.detail || (err.phase ? `[${err.phase}]` : "");
        return {tone: "error", text: main, detail: detail || undefined};
    }

    // 4. last_error
    if (s?.last_error) {
        const err = s.last_error;
        const main = err.action_hint || err.message
            || tx(t, `local_engine.error.${err.code}`, err.code)
            || tx(t, "local_engine.error.unknown_error", "未知错误");
        const detail = err.detail || (err.phase ? `[${err.phase}]` : "");
        return {tone: "error", text: main, detail: detail || undefined};
    }

    // 5. selected ≠ active
    const modelSummary = computeModelSummary(entry);
    if (modelSummary.mismatch) {
        return {
            tone: "warn",
            text: tx(t, "local_engine.model.mismatch_hint", "配置与实际加载不一致，待重启后生效"),
        };
    }

    // 6. 待重建
    if (s?.environment === "needs_rebuild"
        || entry?.preferences?.requires_rebuild === true) {
        return {
            tone: "warn",
            text: tx(t, "local_engine.config.requires_rebuild_hint", "计算设备已变更，需要重建环境才能生效。"),
        };
    }

    // 6. 空闲说明
    if (!s || !entry?.catalog) {
        return {tone: "muted", text: tx(t, "local_engine.status.no_data", "暂无状态数据")};
    }
    if (s.environment === "unknown") {
        return {tone: "muted", text: tx(t, "local_engine.feedback.checking", "正在确认引擎安装状态…")};
    }
    if (s.environment === "missing") {
        const budget = entry.catalog.resource_budget;
        const env = budget?.estimated_env_disk_mb;
        const model = budget?.estimated_model_disk_mb;
        let size = null;
        if (env != null && model != null) size = formatMBLocal(env + model);
        else if (env != null) size = formatMBLocal(env);
        if (size) {
            return {
                tone: "muted",
                text: tx(t, "local_engine.feedback.install_budget", "安装预计需要 {size} 磁盘空间", {size}),
            };
        }
        return {tone: "muted", text: tx(t, "local_engine.summary.missing", "未安装 · 需要安装环境")};
    }
    if (s.environment === "broken") {
        return {
            tone: "error",
            text: tx(t, "local_engine.summary.broken", "环境损坏 · 需要修复"),
        };
    }
    if (s.available) {
        return isOnDemand(entry)
            ? {tone: "ok", text: tx(t, "local_engine.feedback.idle.available_ondemand", "服务运行中 · 空闲后自动回收")}
            : {tone: "ok", text: tx(t, "local_engine.feedback.idle.available_manual", "服务运行中，可处理识别请求")};
    }
    // ready + stopped
    return isOnDemand(entry)
        ? {tone: "muted", text: tx(t, "local_engine.feedback.idle.ready_ondemand", "按需启动 · 首次使用时自动运行")}
        : {tone: "muted", text: tx(t, "local_engine.feedback.idle.ready_manual", "环境已就绪 · 点击「启动」运行服务")};
}

/**
 * 找到第一个进行中的模型操作（模型下载/修复/删除）。
 * @param {Object} entry
 * @param {Function|null} t
 * @returns {{text: string}|null}
 */
function findActiveModelOp(entry, t) {
    // 乐观 pending（前端已发起、列表未刷新）
    const pendings = entry?.pendingModelActions;
    if (pendings && pendings.size > 0) {
        for (const [modelId, action] of pendings) {
            const name = modelDisplayName(entry, modelId) || modelId;
            const text = modelOpText(t, action.kind, name);
            if (text) return {text};
        }
    }
    // 后端观测的模型状态
    const models = entry?.models;
    if (Array.isArray(models)) {
        for (const model of models) {
            if (MODEL_ACTIVE_STATES.includes(model.install_state)) {
                const name = model.display_name || model.model_id;
                const text = modelOpText(t, model.install_state, name);
                if (text) return {text};
            }
        }
    }
    return null;
}

/** 模型操作 → 反馈文案（kind 与 install_state 共用一组 wire 值）。 */
function modelOpText(t, kind, modelName) {
    const map = {
        install: ["local_engine.feedback.model.downloading", "{model} · 下载中"],
        downloading: ["local_engine.feedback.model.downloading", "{model} · 下载中"],
        staging: ["local_engine.feedback.model.downloading", "{model} · 下载中"],
        verifying: ["local_engine.feedback.model.verifying", "{model} · 校验中"],
        repair: ["local_engine.feedback.model.repairing", "{model} · 修复中"],
        repairing: ["local_engine.feedback.model.repairing", "{model} · 修复中"],
        delete: ["local_engine.feedback.model.deleting", "{model} · 删除中"],
        deleting: ["local_engine.feedback.model.deleting", "{model} · 删除中"],
    };
    const hit = map[kind];
    return hit ? tx(t, hit[0], hit[1], {model: modelName}) : null;
}

/** 按 model_id 找展示名。 */
function modelDisplayName(entry, modelId) {
    const model = (entry?.models || []).find((m) => m.model_id === modelId);
    return model?.display_name || null;
}

// ── 关键状态行 ────────────────────────────────────────────────────────────────

/**
 * 关键状态行（环境 / 模型 / 服务 / 生命周期策略）。
 * 进程状态不再同权重常驻——瞬态进综合摘要，PID 进诊断。
 *
 * @param {Object} entry
 * @param {Function|null} t
 * @returns {{label: string, value: string, cls: string}[]}
 */
export function computeKeyline(entry, t) {
    const s = entry?.status?.status;
    if (!s) {
        return [{label: tx(t, "local_engine.status.no_data", "暂无状态数据"), value: "", cls: "status-unknown"}];
    }

    return [
        {
            label: tx(t, "local_engine.status.environment", "环境"),
            value: envLabel(t, s.environment),
            cls: envClass(s.environment),
        },
        {
            label: tx(t, "local_engine.status.model", "模型"),
            value: modelLabel(t, s.model),
            cls: modelClass(s.model),
        },
        {
            label: tx(t, "local_engine.status.service", "服务"),
            value: serviceLabel(t, s.service),
            cls: serviceClass(s.service),
        },
        {
            label: tx(t, "local_engine.keyline.policy", "策略"),
            value: policyLabel(t, entry),
            cls: "le-keyline-policy",
        },
    ];
}

/** 环境 wire value → status class。 */
function envClass(value) {
    const map = {
        missing: "status-unknown",
        ready: "status-available",
        broken: "status-unavailable",
        needs_rebuild: "status-warning",
    };
    return map[value] || "status-unknown";
}

/** 服务 wire value → status class。 */
function serviceClass(value) {
    const map = {
        healthy: "status-available",
        unreachable: "status-unavailable",
        degraded: "status-warning",
        unknown: "status-unknown",
    };
    return map[value] || "status-unknown";
}

/** 模型 wire value → status class。 */
function modelClass(value) {
    const map = {
        ready: "status-available",
        not_loaded: "status-unknown",
        downloading: "status-warning",
        loading: "status-warning",
        failed: "status-unavailable",
        unknown: "status-unknown",
    };
    return map[value] || "status-unknown";
}

// ── 模型三身份摘要 ────────────────────────────────────────────────────────────

/**
 * selected / installed / active 三身份摘要（列表折叠时仍默认可见的信息源）。
 *
 * @param {Object} entry
 * @returns {{selectedName: string|null, activeName: string|null,
 *            installedCount: number, totalCount: number, mismatch: boolean}}
 */
export function computeModelSummary(entry) {
    const models = Array.isArray(entry?.models) ? entry.models : [];
    const selected = models.find((m) => m.is_selected) || null;
    const active = models.find((m) => m.is_active) || null;
    return {
        selectedName: selected ? (selected.display_name || selected.model_id) : null,
        activeName: active ? (active.display_name || active.model_id) : null,
        installedCount: models.filter((m) => m.install_state === "installed").length,
        totalCount: models.length,
        mismatch: Boolean(selected && active && selected.model_id !== active.model_id),
    };
}

/** 动作 → Lucide 图标名（sprite 内已存在，禁 emoji）。 */
function iconForKind(kind) {
    const map = {
        install: "download",
        start: "play",
        stop: "square",
        repair: "wrench",
        cancel: "x",
    };
    return map[kind] || "circle";
}

// ── 唯一主操作 ────────────────────────────────────────────────────────────────

/**
 * 卡片头部唯一主操作投影。
 *
 * 映射：
 * - cancellable operation → 取消
 * - 忙碌（乐观 pending / 活跃 operation 且不可取消）→ 不可点的"{kind}中"
 * - missing → 安装环境；broken/needs_rebuild → 修复环境
 * - stopped/exited → 启动
 * - starting/running/stopping → 停止服务
 *
 * kind 为 null 时按钮 disabled（等待态）。
 *
 * @param {Object} entry
 * @param {Function|null} t
 * @returns {{kind: string|null, label: string, icon: string, disabled: boolean}}
 */
export function primaryActionView(entry, t) {
    if (!entry?.status?.status) {
        return {
            kind: null,
            label: tx(t, "local_engine.summary.loading", "状态加载中"),
            icon: "circle",
            disabled: true,
        };
    }

    if (isOperationCancellable(entry)) {
        return {kind: "cancel", label: tx(t, "local_engine.action.cancel", "取消"), icon: "x", disabled: false};
    }

    const busyKind = entry.pendingAction?.kind || activeOp(entry)?.kind;
    if (busyKind || hasActiveOperation(entry)) {
        const kind = busyKind || "start";
        return {
            kind: null,
            label: tx(t, "local_engine.action.busy_kind", "{kind}中", {kind: opKindLabel(t, kind)}),
            icon: iconForKind(kind),
            disabled: true,
        };
    }

    // 进程瞬态（starting/stopping）：主操作为禁用的等待态，
    // 不提供同权重的启动/停止竞争按钮
    const processStateValue = entry.status.status.process?.state;
    if (processStateValue === "starting" || processStateValue === "stopping") {
        const kind = processStateValue === "starting" ? "start" : "stop";
        return {
            kind: null,
            label: tx(t, "local_engine.action.busy_kind", "{kind}中", {kind: opKindLabel(t, kind)}),
            icon: iconForKind(kind),
            disabled: true,
        };
    }

    const primary = getPrimaryAction(entry);
    switch (primary) {
        case "install":
            return {
                kind: "install",
                label: tx(t, "local_engine.action.install_env", "安装环境"),
                icon: "download",
                disabled: false,
            };
        case "repair":
            return {
                kind: "repair",
                label: tx(t, "local_engine.action.repair_env", "修复环境"),
                icon: "wrench",
                disabled: false,
            };
        case "start":
            return {
                kind: "start",
                label: tx(t, "local_engine.action.start", "启动"),
                icon: "play",
                disabled: false,
            };
        case "stop":
            return {
                kind: "stop",
                label: tx(t, "local_engine.action.stop_service", "停止服务"),
                icon: "square",
                disabled: false,
            };
        default:
            return {
                kind: null,
                label: tx(t, "local_engine.action.working", "处理中"),
                icon: "circle",
                disabled: true,
            };
    }
}

// ── 运行时摘要（页面顶部） ────────────────────────────────────────────────────

/**
 * 页面顶部运行时摘要行。
 *
 * @param {Map} state - engine_id → EngineStateEntry
 * @param {{foundationError?: boolean, foundationLoading?: boolean}} [flags]
 * @param {Function|null} t
 * @returns {{text: string, tone: string}}
 */
export function computeRuntimeSummary(state, flags = {}, t) {
    if (flags.foundationError) {
        return {
            text: tx(t, "local_engine.runtime.summary.foundation_failed", "运行时状态加载失败 · 点击「运行时与缓存」重试"),
            tone: "error",
        };
    }

    const entries = Array.from(state?.values() || []);
    if (entries.length === 0) {
        return {
            text: tx(t, "local_engine.loading", "正在加载引擎信息…"),
            tone: "muted",
        };
    }

    const running = entries.filter((e) => e?.status?.status?.available === true
        || e?.status?.status?.process?.state === "running").length;
    const attention = entries.filter((e) => {
        const s = e?.status?.status;
        return s && (s.environment === "broken" || s.environment === "needs_rebuild" || s.last_error);
    }).length;

    let totalBytes = 0;
    let hasStorage = false;
    for (const e of entries) {
        if (e?.storage?.total_size_bytes) {
            totalBytes += e.storage.total_size_bytes;
            hasStorage = true;
        }
    }

    if (attention > 0) {
        const base = tx(t, "local_engine.runtime.summary.attention",
            "{attention} 个引擎需要关注 · {engines} 个引擎 · 总占用 {size}", {
                attention,
                engines: entries.length,
                size: hasStorage ? formatBytesLocal(totalBytes) : "—",
            });
        return {text: base, tone: "warn"};
    }

    const params = {
        engines: entries.length,
        running,
        size: hasStorage ? formatBytesLocal(totalBytes) : null,
    };
    const text = hasStorage
        ? tx(t, "local_engine.runtime.summary.ok",
            "运行正常 · {engines} 个引擎 · {running} 个运行中 · 总占用 {size}", params)
        : tx(t, "local_engine.runtime.summary.ok_no_size",
            "运行正常 · {engines} 个引擎 · {running} 个运行中", params);
    return {text, tone: "ok"};
}
