/**
 * 能力目录（0.21.6 重写版）测试
 *
 * 1. capabilities-core.js 纯逻辑：状态推导 / BindingOp 构造 / MCP 列表更新 / 组合过滤
 * 2. 静态契约核对：侧边栏"功能"三子页、后端 command 字面量、i18n key、
 *    旧错误字段路径回归防护（feature.ai_status 等顶层误读曾在 0.21.6 首版全列渲染错误）
 */
import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";

import {
  GROUP_ORDER,
  groupKeyOf,
  sourceClassOf,
  riskOf,
  localToggleState,
  exitToggleState,
  buildBindingOps,
  nextMcpExposed,
  matchesFilters,
  groupLocalStatus,
} from "./capabilities-core.js";

// ── 1. 分组 / 来源 / 风险 ─────────────────────────────────────────────────────

// 组 key 必须与后端 FeatureGroup serde 值一致（types.rs snake_case）
assert.deepEqual(GROUP_ORDER, [
  "apps_files_links",
  "clipboard_text",
  "image_color",
  "sticky_content",
  "window_system",
  "blink_management",
  "other_plugin",
]);

assert.equal(groupKeyOf({ group: "clipboard_text" }), "clipboard_text");
assert.equal(groupKeyOf({ group: "unknown_group" }), "other_plugin");
assert.equal(groupKeyOf({}), "other_plugin");

assert.equal(sourceClassOf({ source: "builtin" }), "builtin");
assert.equal(sourceClassOf({ source: "builtin_capability" }), "builtin");
assert.equal(sourceClassOf({ source: "plugin" }), "plugin");
assert.equal(sourceClassOf({ source: "chord" }), "chord");

assert.equal(riskOf({ capability_projection: { danger: "dangerous" } }), "dangerous");
assert.equal(riskOf({ capability_projection: { danger: "safe", sensitive: true } }), "sensitive");
assert.equal(riskOf({ capability_projection: { danger: "safe", sensitive: false } }), "safe");
assert.equal(riskOf({}), "safe"); // Interaction-only 无投影

// ── 2. 本地列开关状态 ─────────────────────────────────────────────────────────

assert.deepEqual(localToggleState({ local_availability: "source_unavailable", bindings: [{}] }), {
  kind: "dash",
  reason: "unavailable",
});
assert.deepEqual(localToggleState({ local_availability: "available", bindings: [] }), {
  kind: "dash",
  reason: "no_binding",
});

const allOn = { local_availability: "available", bindings: [{ enabled: true }, { enabled: true }] };
const partial = { local_availability: "available", bindings: [{ enabled: true }, { enabled: false }] };
assert.deepEqual(localToggleState(allOn), { kind: "toggle", checked: true });
assert.deepEqual(localToggleState(partial), { kind: "toggle", checked: false });

// ── 3. AI / MCP 出口开关状态 ──────────────────────────────────────────────────

// Interaction-only（无投影）→ dash
assert.deepEqual(exitToggleState({}, "ai"), { kind: "dash", reason: "no_projection" });
assert.deepEqual(exitToggleState({}, "mcp"), { kind: "dash", reason: "no_projection" });

const proj = (ai, mcp) => ({ capability_projection: { ai_status: ai, mcp_status: mcp } });
assert.deepEqual(exitToggleState(proj("code_forbidden", "disabled"), "ai"), {
  kind: "dash",
  reason: "code_forbidden",
});
assert.deepEqual(exitToggleState(proj("enabled", "code_forbidden"), "mcp"), {
  kind: "dash",
  reason: "code_forbidden",
});
assert.deepEqual(exitToggleState(proj("not_applicable", "disabled"), "ai"), {
  kind: "dash",
  reason: "no_projection",
});
assert.deepEqual(exitToggleState(proj("enabled", "enabled"), "ai"), { kind: "toggle", checked: true });
assert.deepEqual(exitToggleState(proj("disabled", "enabled"), "ai"), { kind: "toggle", checked: false });
assert.deepEqual(exitToggleState(proj("enabled", "disabled"), "mcp"), { kind: "toggle", checked: false });

// ── 4. BindingOp 构造（后端契约 {op, kind, binding_id}） ──────────────────────

assert.deepEqual(
  buildBindingOps(
    {
      bindings: [
        { binding_id: "lock", kind: "search_keyword", enabled: false },
        { binding_id: "chord.screenshot", kind: "chord_key", enabled: true },
      ],
    },
    true,
  ),
  [{ op: "enable", kind: "search_keyword", binding_id: "lock" }],
);

assert.deepEqual(
  buildBindingOps(
    {
      bindings: [
        { binding_id: "lock", kind: "search_keyword", enabled: false },
        { binding_id: "chord.screenshot", kind: "chord_key", enabled: true },
      ],
    },
    false,
  ),
  [{ op: "disable", kind: "chord_key", binding_id: "chord.screenshot" }],
);

// 状态已一致的 binding 不产生 op（幂等）
assert.deepEqual(buildBindingOps(allOn, true), []);
assert.deepEqual(buildBindingOps({ bindings: [] }, true), []);

// ── 5. MCP exposed 列表更新 ───────────────────────────────────────────────────

assert.deepEqual(nextMcpExposed([], "read_clipboard", true), ["read_clipboard"]);
assert.deepEqual(nextMcpExposed(["read_clipboard"], "read_clipboard", true), ["read_clipboard"]);
assert.deepEqual(nextMcpExposed(["read_clipboard"], "read_clipboard", false), []);
assert.deepEqual(
  nextMcpExposed(["read_clipboard", "search_apps"], "ocr_image", true),
  ["ocr_image", "read_clipboard", "search_apps"],
);

// ── 6. 组合过滤 ───────────────────────────────────────────────────────────────

const feature = {
  feature_id: "blink.open_url",
  title: "打开链接",
  description: "用默认浏览器打开 URL",
  source: "builtin",
  local_availability: "available",
  bindings: [{ binding_id: "open_url", kind: "search_keyword", enabled: true }],
  capability_projection: { danger: "safe", sensitive: false, ai_status: "enabled", mcp_status: "disabled" },
};

assert.equal(matchesFilters(feature, {}), true);
assert.equal(matchesFilters(feature, null), true);
assert.equal(matchesFilters(feature, { query: "URL" }), true);
assert.equal(matchesFilters(feature, { query: "open_url" }), true); // capability/feature id 命中
assert.equal(matchesFilters(feature, { query: "不存在" }), false);
assert.equal(matchesFilters(feature, { source: "builtin" }), true);
assert.equal(matchesFilters(feature, { source: "plugin" }), false);
assert.equal(matchesFilters(feature, { risk: "safe" }), true);
assert.equal(matchesFilters(feature, { risk: "dangerous" }), false);
assert.equal(matchesFilters(feature, { availability: "available" }), true);
assert.equal(matchesFilters(feature, { availability: "unavailable" }), false);
assert.equal(matchesFilters(feature, { exit: "ai" }), true);
assert.equal(matchesFilters(feature, { exit: "mcp" }), false);
assert.equal(matchesFilters(feature, { exit: "local" }), true);

// 组合条件 AND
assert.equal(matchesFilters(feature, { source: "builtin", risk: "dangerous" }), false);
assert.equal(matchesFilters(feature, { source: "builtin", exit: "ai", query: "链接" }), true);

// 不可用（插件禁用）项的可用性过滤
const disabledFeature = { ...feature, local_availability: "disabled" };
assert.equal(matchesFilters(disabledFeature, { availability: "unavailable" }), true);
assert.equal(matchesFilters(disabledFeature, { availability: "available" }), false);

// ── 7. 组级三态 ───────────────────────────────────────────────────────────────

assert.equal(groupLocalStatus([allOn, { ...allOn }]), "all_enabled");
assert.equal(groupLocalStatus([allOn, partial]), "partial");
assert.equal(groupLocalStatus([partial, { ...partial }]), "all_disabled");
assert.equal(groupLocalStatus([{ bindings: [] }]), "none"); // 无可启停项

// ── 8. 静态契约核对 ───────────────────────────────────────────────────────────

const settingsHtml = await readFile(new URL("../../../settings.html", import.meta.url), "utf8");
const capsSource = await readFile(new URL("./capabilities.js", import.meta.url), "utf8");
const zh = await readFile(new URL("../../i18n/zh.js", import.meta.url), "utf8");
const en = await readFile(new URL("../../i18n/en.js", import.meta.url), "utf8");

// 侧边栏："功能"分组下三个平级子页（引擎/插件/能力与操作），无残留"搜索"分组
const functionalityGroup = settingsHtml.match(
  /sidebar\.group\.functionality[\s\S]*?<\/div>\s*<div class="sidebar-group">/,
);
assert.ok(functionalityGroup, "功能分组应存在");
for (const tab of ['data-tab="engines"', 'data-tab="plugins"', 'data-tab="capabilities"']) {
  assert.ok(functionalityGroup[0].includes(tab), `功能分组应包含 ${tab}`);
}
assert.doesNotMatch(settingsHtml, /sidebar\.group\.search/);
assert.doesNotMatch(zh, /sidebar\.group\.search/);
assert.doesNotMatch(en, /sidebar\.group\.search/);

// 出口过滤 select 存在（MCP 深链目标）
assert.match(settingsHtml, /id="filter-exit"/);

// invoke 字面量只允许已注册 command（§6.2.1 拆分护栏）
const invokedCommands = [...capsSource.matchAll(/invoke\("([\w]+)"/g)].map((m) => m[1]);
assert.deepEqual(
  [...new Set(invokedCommands)].sort(),
  [
    "apply_binding_ops",
    "get_mcp_server_config",
    "list_feature_catalog",
    "set_mcp_server_config",
    "toggle_ai_capability",
  ],
);

// 回归防护：不得再按顶层路径读 ai_status / mcp_status / danger / sensitive
// （0.21.6 首版因此导致三出口列恒显错误值）
assert.doesNotMatch(capsSource, /feature\.ai_status/);
assert.doesNotMatch(capsSource, /feature\.mcp_status/);
assert.doesNotMatch(capsSource, /feature\.danger\b/);
assert.doesNotMatch(capsSource, /feature\.sensitive\b/);
assert.doesNotMatch(capsSource, /aiStatus\.(allowed|reason)/);

// 组 i18n key 与后端 serde 值对齐
for (const key of ["apps_files_links", "other_plugin"]) {
  assert.ok(zh.includes(`"capabilities.group.${key}"`), `zh 缺少 capabilities.group.${key}`);
  assert.ok(en.includes(`"capabilities.group.${key}"`), `en 缺少 capabilities.group.${key}`);
}

// spec-frontend §6.1：不用 style.display 切换显隐
assert.doesNotMatch(capsSource, /style\.display/);

console.log("capabilities tab tests passed");
