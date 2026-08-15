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
    return { kind: "dash", reason: "unavailable" };
  }
  const bindings = feature.bindings || [];
  if (bindings.length === 0) {
    return { kind: "dash", reason: "no_binding" };
  }
  return { kind: "toggle", checked: bindings.every((b) => b.enabled) };
}

/**
 * AI / MCP 出口列开关状态。
 * - toggle：可授权开关（checked = 出口当前已授权）
 * - dash：Interaction-only 无投影 / 代码级禁止——不可点击，不伪装成关闭
 */
export function exitToggleState(feature, exit) {
  const p = feature.capability_projection;
  if (!p) {
    return { kind: "dash", reason: "no_projection" };
  }
  const status = exit === "ai" ? p.ai_status : p.mcp_status;
  if (status === "code_forbidden") {
    return { kind: "dash", reason: "code_forbidden" };
  }
  if (status === "not_applicable") {
    return { kind: "dash", reason: "no_projection" };
  }
  return { kind: "toggle", checked: status === "enabled" };
}

/** 为功能的全部本地 binding 生成 enable/disable ops（后端 BindingOp 契约） */
export function buildBindingOps(feature, enable) {
  const op = enable ? "enable" : "disable";
  return (feature.bindings || [])
    .filter((b) => Boolean(b.enabled) !== enable)
    .map((b) => ({ op, kind: b.kind, binding_id: b.binding_id }));
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

/** 组内本地状态汇总（三态展示）："all_enabled" | "partial" | "all_disabled" | "none" */
export function groupLocalStatus(features) {
  const toggleable = features.filter(
    (f) => localToggleState(f).kind === "toggle",
  );
  if (toggleable.length === 0) return "none";
  const on = toggleable.filter((f) => localToggleState(f).checked).length;
  if (on === toggleable.length) return "all_enabled";
  if (on === 0) return "all_disabled";
  return "partial";
}
