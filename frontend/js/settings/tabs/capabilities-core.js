/**
 * 能力目录纯逻辑核心（0.21.6 重写）
 *
 * 无 DOM / 无 invoke / 无 i18n 依赖——只做数据到状态的推导，
 * 供 capabilities.js 渲染层与 node --test 单测共用。
 *
 * 数据契约对齐 src/domain/feature_catalog/types.rs（serde snake_case）：
 * - FeatureCatalogItem { feature_id, title, description, group, source,
 *   capability_id, bindings[], local_availability, capability_projection, unavailable_reason }
 * - BindingSummary { binding_id, kind: "search_keyword"|"context_binding"|"chord_key",
 *   enabled, trigger_label }
 * - CatalogCapabilityProjection { capability_id, danger: "safe"|"dangerous", sensitive,
 *   ai_status / mcp_status: "enabled"|"disabled"|"code_forbidden"|"not_applicable", ... }
 * - LocalAvailability: "available"|"disabled"|"source_unavailable"
 * - BindingOp { op: "enable"|"disable", kind: BindingKind 值, binding_id }
 */

/** 组展示顺序 = 后端 FeatureGroup 序列化值（types.rs serde snake_case） */
export const GROUP_ORDER = [
    "apps_files_links",
    "clipboard_text",
    "image_color",
    "sticky_content",
    "chord_entry",
    "window_system",
    "blink_management",
    "other_plugin",
];

/** 未知组归入插件杂项组 */
export function groupKeyOf(feature) {
    return GROUP_ORDER.includes(feature.group) ? feature.group : "other_plugin";
}

/** 来源归类（builtin_capability 归入"内置"，供来源过滤与徽章使用） */
export function sourceClassOf(feature) {
    if (feature.source === "plugin") return "plugin";
    if (feature.source === "chord") return "chord";
    return "builtin";
}

/** 风险归类：dangerous > sensitive > safe；无投影（Interaction-only）按 safe */
export function riskOf(feature) {
    const p = feature.capability_projection;
    if (!p) return "safe";
    if (p.danger === "dangerous") return "dangerous";
    return p.sensitive ? "sensitive" : "safe";
}

/**
 * 本地列开关状态。
 * - toggle：功能有本地 binding，可启停（checked = 全部 binding 启用）
 * - dash：来源不可用 / 无本地入口（capability-only，如 read_clipboard）
 */
export function localToggleState(feature) {
    if (feature.local_availability === "source_unavailable") {
        return {kind: "dash", reason: "unavailable"};
    }
    const bindings = feature.bindings || [];
    if (bindings.length === 0) {
        return {kind: "dash", reason: "no_binding"};
    }
    return {kind: "toggle", checked: bindings.every((b) => b.enabled)};
}

/**
 * AI / MCP 出口列开关状态。
 * - toggle：可授权开关（checked = 出口当前已授权）
 * - dash：Interaction-only 无投影 / 代码级禁止——不可点击，不伪装成关闭
 */
export function exitToggleState(feature, exit) {
    const p = feature.capability_projection;
    if (!p) {
        return {kind: "dash", reason: "no_projection"};
    }
    const status = exit === "ai" ? p.ai_status : p.mcp_status;
    if (status === "code_forbidden") {
        return {kind: "dash", reason: "code_forbidden"};
    }
    if (status === "not_applicable") {
        return {kind: "dash", reason: "no_projection"};
    }
    return {kind: "toggle", checked: status === "enabled"};
}

/** 为功能的全部本地 binding 生成 enable/disable ops（后端 BindingOp 契约） */
export function buildBindingOps(feature, enable) {
    const op = enable ? "enable" : "disable";
    return (feature.bindings || [])
        .filter((b) => Boolean(b.enabled) !== enable)
        .map((b) => ({op, kind: b.kind, binding_id: b.binding_id}));
}

/** MCP exposed 列表的下一状态（去重 + 排序，保持持久化列表稳定） */
export function nextMcpExposed(current, capabilityId, expose) {
    const set = new Set(current || []);
    if (expose) {
        set.add(capabilityId);
    } else {
        set.delete(capabilityId);
    }
    return [...set].sort();
}

/** 组级 MCP 批量变更的应用结果（add/remove 集合 → 新 exposed 列表） */
export function applyMcpChanges(current, changes) {
    const set = new Set(current || []);
    for (const id of changes?.add || []) set.add(id);
    for (const id of changes?.remove || []) set.delete(id);
    return [...set].sort();
}

/**
 * 组合过滤：搜索词（标题/描述/feature_id/capability_id）+ 来源 + 风险 + 可用性 + 出口。
 * 全部条件 AND 组合；"all" 值跳过对应维度。
 */
export function matchesFilters(feature, filters) {
    const {
        query = "",
        source = "all",
        risk = "all",
        availability = "all",
        exit = "all",
    } = filters || {};

    if (source !== "all" && sourceClassOf(feature) !== source) return false;
    if (risk !== "all" && riskOf(feature) !== risk) return false;

    if (availability !== "all") {
        const avail =
            feature.local_availability === "available" ? "available" : "unavailable";
        if (avail !== availability) return false;
    }

    if (exit !== "all") {
        // 本地出口走 binding 聚合状态；AI/MCP 出口走投影授权状态
        const state =
            exit === "local" ? localToggleState(feature) : exitToggleState(feature, exit);
        if (state.kind !== "toggle" || !state.checked) return false;
    }

    if (query) {
        const q = String(query).trim().toLowerCase();
        if (q) {
            const haystack = [
                feature.title,
                feature.description,
                feature.feature_id,
                feature.capability_id,
            ]
                .filter(Boolean)
                .join("\n")
                .toLowerCase();
            if (!haystack.includes(q)) return false;
        }
    }

    return true;
}

/** 组内某出口实际可操作的行（本地 = 有 binding 可启停；AI/MCP = 投影可授权） */
function toggleableFeatures(features, exit) {
    return (features || []).filter((f) =>
        exit === "local"
            ? localToggleState(f).kind === "toggle"
            : exitToggleState(f, exit).kind === "toggle",
    );
}

/**
 * 组级出口状态（组头列开关用）。
 * - none：组内没有该出口可操作的行 → 不渲染开关
 * - toggle：checked = 全开；partial = 部分开（渲染 indeterminate 三态）
 */
export function groupExitState(features, exit) {
    const rows = toggleableFeatures(features, exit);
    if (rows.length === 0) return {kind: "none"};
    const on = rows.filter((f) =>
        exit === "local" ? localToggleState(f).checked : exitToggleState(f, exit).checked,
    ).length;
    return {
        kind: "toggle",
        checked: on === rows.length,
        partial: on > 0 && on < rows.length,
    };
}

/** 组级本地批量 ops（BindingOp[]，只含状态需要变化的 binding） */
export function groupLocalOps(features, enable) {
    return toggleableFeatures(features, "local").flatMap((f) =>
        buildBindingOps(f, enable),
    );
}

/** 组级 AI 批量（[capability_id, enable][]，只含状态需要变化的行） */
export function groupAiOps(features, enable) {
    return toggleableFeatures(features, "ai")
        .filter((f) => f.capability_id && exitToggleState(f, "ai").checked !== enable)
        .map((f) => [f.capability_id, enable]);
}

/** 组级 MCP 批量变更（add/remove capability_id 列表） */
export function groupMcpChanges(features, enable) {
    const add = [];
    const remove = [];
    for (const f of toggleableFeatures(features, "mcp")) {
        if (!f.capability_id) continue;
        const exposed = exitToggleState(f, "mcp").checked;
        if (enable && !exposed) add.push(f.capability_id);
        if (!enable && exposed) remove.push(f.capability_id);
    }
    return {add, remove};
}

// ── 推荐态与姿态摘要（0.21.10） ────────────────────────────────────────────────

/**
 * 单出口推荐态（phase 0.21 §4.1b 定案的默认策略）。
 * - local：可启停行推荐启用；dash 行（无 binding / 不可用）不适用，返回 null
 * - ai：可授权行推荐开启，dangerous 推荐关闭（sensitive 走运行时确认，默认开）
 * - mcp：仅推荐暴露无风险能力（非 dangerous、非 sensitive）
 */
export function recommendedExitState(feature, exit) {
    if (exit === "local") {
        return localToggleState(feature).kind === "toggle" ? true : null;
    }
    const state = exitToggleState(feature, exit);
    if (state.kind !== "toggle") return null;
    if (exit === "mcp") return riskOf(feature) === "safe";
    return feature.capability_projection?.danger === "dangerous" ? false : true;
}

/**
 * 全目录与推荐态的差集（"恢复推荐"用），只含需要变化的项，幂等。
 * 写回走既有三条批量命令：apply_binding_ops / toggle_ai_capabilities / set_mcp_server_config。
 */
export function recommendedDiff(features) {
    const bindingOps = [];
    const aiOps = [];
    const mcpAdd = [];
    const mcpRemove = [];
    for (const f of features || []) {
        if (recommendedExitState(f, "local") === true) {
            bindingOps.push(...buildBindingOps(f, true));
        }
        const aiRec = recommendedExitState(f, "ai");
        if (aiRec !== null && f.capability_id && exitToggleState(f, "ai").checked !== aiRec) {
            aiOps.push([f.capability_id, aiRec]);
        }
        const mcpRec = recommendedExitState(f, "mcp");
        if (mcpRec !== null && f.capability_id) {
            const mcpEnabled = exitToggleState(f, "mcp").checked;
            if (mcpRec && !mcpEnabled) mcpAdd.push(f.capability_id);
            if (!mcpRec && mcpEnabled) mcpRemove.push(f.capability_id);
        }
    }
    return {
        bindingOps,
        aiOps,
        mcpAdd,
        mcpRemove,
        isClean:
            bindingOps.length === 0 &&
            aiOps.length === 0 &&
            mcpAdd.length === 0 &&
            mcpRemove.length === 0,
    };
}

/**
 * 姿态摘要（顶部状态行）：可授权出口的开/总数 + 危险能力对 AI 开放的计数。
 */
export function postureSummary(features) {
    const s = {aiOn: 0, aiTotal: 0, mcpOn: 0, mcpTotal: 0, dangerousAiOn: 0};
    for (const f of features || []) {
        const ai = exitToggleState(f, "ai");
        if (ai.kind === "toggle") {
            s.aiTotal++;
            if (ai.checked) {
                s.aiOn++;
                if (riskOf(f) === "dangerous") s.dangerousAiOn++;
            }
        }
        const mcp = exitToggleState(f, "mcp");
        if (mcp.kind === "toggle") {
            s.mcpTotal++;
            if (mcp.checked) s.mcpOn++;
        }
    }
    return s;
}
