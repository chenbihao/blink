/**
 * 能力目录（0.21.6 重写版）测试
 *
 * 1. capabilities-core.js 纯逻辑：状态推导 / BindingOp 构造 / MCP 列表更新 / 组合过滤
 * 2. 静态契约核对：侧边栏"功能"三子页、后端 command 字面量、i18n key、
 *    旧错误字段路径回归防护（feature.ai_status 等顶层误读曾在 0.21.6 首版全列渲染错误）
 */
import assert from "node:assert/strict";
import {readFile} from "node:fs/promises";

import {
    applyMcpChanges,
    buildBindingOps,
    exitToggleState,
    GROUP_ORDER,
    groupAiOps,
    groupExitState,
    groupKeyOf,
    groupLocalOps,
    groupMcpChanges,
    localToggleState,
    matchesFilters,
    nextMcpExposed,
    postureSummary,
    recommendedDiff,
    recommendedExitState,
    riskOf,
    sourceClassOf,
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

assert.equal(groupKeyOf({group: "clipboard_text"}), "clipboard_text");
assert.equal(groupKeyOf({group: "unknown_group"}), "other_plugin");
assert.equal(groupKeyOf({}), "other_plugin");

assert.equal(sourceClassOf({source: "builtin"}), "builtin");
assert.equal(sourceClassOf({source: "builtin_capability"}), "builtin");
assert.equal(sourceClassOf({source: "plugin"}), "plugin");
assert.equal(sourceClassOf({source: "chord"}), "chord");

assert.equal(riskOf({capability_projection: {danger: "dangerous"}}), "dangerous");
assert.equal(riskOf({capability_projection: {danger: "safe", sensitive: true}}), "sensitive");
assert.equal(riskOf({capability_projection: {danger: "safe", sensitive: false}}), "safe");
assert.equal(riskOf({}), "safe"); // Interaction-only 无投影

// ── 2. 本地列开关状态 ─────────────────────────────────────────────────────────

assert.deepEqual(localToggleState({local_availability: "source_unavailable", bindings: [{}]}), {
    kind: "dash",
    reason: "unavailable",
});
assert.deepEqual(localToggleState({local_availability: "available", bindings: []}), {
    kind: "dash",
    reason: "no_binding",
});

const allOn = {local_availability: "available", bindings: [{enabled: true}, {enabled: true}]};
const partial = {local_availability: "available", bindings: [{enabled: true}, {enabled: false}]};
assert.deepEqual(localToggleState(allOn), {kind: "toggle", checked: true});
assert.deepEqual(localToggleState(partial), {kind: "toggle", checked: false});

// ── 3. AI / MCP 出口开关状态 ──────────────────────────────────────────────────

// Interaction-only（无投影）→ dash
assert.deepEqual(exitToggleState({}, "ai"), {kind: "dash", reason: "no_projection"});
assert.deepEqual(exitToggleState({}, "mcp"), {kind: "dash", reason: "no_projection"});

const proj = (ai, mcp) => ({capability_projection: {ai_status: ai, mcp_status: mcp}});
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
assert.deepEqual(exitToggleState(proj("enabled", "enabled"), "ai"), {kind: "toggle", checked: true});
assert.deepEqual(exitToggleState(proj("disabled", "enabled"), "ai"), {kind: "toggle", checked: false});
assert.deepEqual(exitToggleState(proj("enabled", "disabled"), "mcp"), {kind: "toggle", checked: false});

// ── 4. BindingOp 构造（后端契约 {op, kind, binding_id}） ──────────────────────

assert.deepEqual(
    buildBindingOps(
        {
            bindings: [
                {binding_id: "lock", kind: "search_keyword", enabled: false},
                {binding_id: "chord.screenshot", kind: "chord_key", enabled: true},
            ],
        },
        true,
    ),
    [{op: "enable", kind: "search_keyword", binding_id: "lock"}],
);

assert.deepEqual(
    buildBindingOps(
        {
            bindings: [
                {binding_id: "lock", kind: "search_keyword", enabled: false},
                {binding_id: "chord.screenshot", kind: "chord_key", enabled: true},
            ],
        },
        false,
    ),
    [{op: "disable", kind: "chord_key", binding_id: "chord.screenshot"}],
);

// 状态已一致的 binding 不产生 op（幂等）
assert.deepEqual(buildBindingOps(allOn, true), []);
assert.deepEqual(buildBindingOps({bindings: []}, true), []);

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
    bindings: [{binding_id: "open_url", kind: "search_keyword", enabled: true}],
    capability_projection: {danger: "safe", sensitive: false, ai_status: "enabled", mcp_status: "disabled"},
};

assert.equal(matchesFilters(feature, {}), true);
assert.equal(matchesFilters(feature, null), true);
assert.equal(matchesFilters(feature, {query: "URL"}), true);
assert.equal(matchesFilters(feature, {query: "open_url"}), true); // capability/feature id 命中
assert.equal(matchesFilters(feature, {query: "不存在"}), false);
assert.equal(matchesFilters(feature, {source: "builtin"}), true);
assert.equal(matchesFilters(feature, {source: "plugin"}), false);
assert.equal(matchesFilters(feature, {risk: "safe"}), true);
assert.equal(matchesFilters(feature, {risk: "dangerous"}), false);
assert.equal(matchesFilters(feature, {availability: "available"}), true);
assert.equal(matchesFilters(feature, {availability: "unavailable"}), false);
assert.equal(matchesFilters(feature, {exit: "ai"}), true);
assert.equal(matchesFilters(feature, {exit: "mcp"}), false);
assert.equal(matchesFilters(feature, {exit: "local"}), true);

// 组合条件 AND
assert.equal(matchesFilters(feature, {source: "builtin", risk: "dangerous"}), false);
assert.equal(matchesFilters(feature, {source: "builtin", exit: "ai", query: "链接"}), true);

// 不可用（插件禁用）项的可用性过滤
const disabledFeature = {...feature, local_availability: "disabled"};
assert.equal(matchesFilters(disabledFeature, {availability: "unavailable"}), true);
assert.equal(matchesFilters(disabledFeature, {availability: "available"}), false);

// ── 7. 组级三出口状态 / 批量 ops（0.21.9 组头按列开关） ──────────────────────

const row = (over = {}) => ({
    feature_id: "blink.x",
    title: "X",
    source: "builtin",
    local_availability: "available",
    bindings: [{binding_id: "x", kind: "search_keyword", enabled: true}],
    capability_projection: {danger: "safe", sensitive: false, ai_status: "enabled", mcp_status: "disabled"},
    ...over,
});

// groupExitState：local（binding 聚合）
assert.deepEqual(groupExitState([row(), row()], "local"), {
    kind: "toggle",
    checked: true,
    partial: false,
});
assert.deepEqual(
    groupExitState([row(), row({bindings: [{binding_id: "x", kind: "search_keyword", enabled: false}]})], "local"),
    {kind: "toggle", checked: false, partial: true},
);
assert.deepEqual(groupExitState([{bindings: []}, {local_availability: "source_unavailable"}], "local"), {
    kind: "none",
});

// groupExitState：ai / mcp（投影授权）
assert.deepEqual(groupExitState([row(), row()], "ai"), {kind: "toggle", checked: true, partial: false});
assert.deepEqual(
    groupExitState([row(), row({capability_projection: {ai_status: "disabled"}})], "ai"),
    {kind: "toggle", checked: false, partial: true},
);
assert.deepEqual(groupExitState([row()], "mcp"), {kind: "toggle", checked: false, partial: false});
// code_forbidden / 无投影行不参与组级聚合
assert.deepEqual(groupExitState([{}, {capability_projection: null}], "ai"), {kind: "none"});

// groupLocalOps：只含状态需要变化的 binding
assert.deepEqual(
    groupLocalOps(
        [
            row({bindings: [{binding_id: "a", kind: "search_keyword", enabled: false}]}),
            row({bindings: [{binding_id: "b", kind: "search_keyword", enabled: true}]}),
        ],
        true,
    ),
    [{op: "enable", kind: "search_keyword", binding_id: "a"}],
);

// groupAiOps：[capability_id, enable][]，跳过已一致与无 capability_id 的行
assert.deepEqual(
    groupAiOps(
        [
            row({feature_id: "blink.a", capability_id: "a", capability_projection: {ai_status: "disabled"}}),
            row({feature_id: "blink.b", capability_id: "b", capability_projection: {ai_status: "enabled"}}),
            row({feature_id: "blink.c", capability_id: null}),
        ],
        true,
    ),
    [["a", true]],
);

// groupMcpChanges：add/remove 集合
assert.deepEqual(
    groupMcpChanges(
        [
            row({feature_id: "blink.a", capability_id: "a", capability_projection: {mcp_status: "disabled"}}),
            row({feature_id: "blink.b", capability_id: "b", capability_projection: {mcp_status: "enabled"}}),
        ],
        true,
    ),
    {add: ["a"], remove: []},
);
assert.deepEqual(
    groupMcpChanges(
        [
            row({feature_id: "blink.a", capability_id: "a", capability_projection: {mcp_status: "disabled"}}),
            row({feature_id: "blink.b", capability_id: "b", capability_projection: {mcp_status: "enabled"}}),
        ],
        false,
    ),
    {add: [], remove: ["b"]},
);

// applyMcpChanges：组级批量增删
assert.deepEqual(applyMcpChanges(["b"], {add: ["a", "b"], remove: ["c"]}), ["a", "b"]);
assert.deepEqual(applyMcpChanges(["a", "b"], {remove: ["a"]}), ["b"]);
assert.deepEqual(applyMcpChanges(null, {}), []);

// ── 7b. 推荐态 / 恢复推荐 diff / 姿态摘要（0.21.10） ──────────────────────────

// recommendedExitState：local 可启停 → true；dash 行不适用
assert.equal(recommendedExitState(row(), "local"), true);
assert.equal(recommendedExitState(row({bindings: []}), "local"), null);
assert.equal(recommendedExitState(row({local_availability: "source_unavailable"}), "local"), null);

// recommendedExitState：ai 非危险开 / 危险关；mcp 仅安全能力开；不可授权行 null
assert.equal(recommendedExitState(row(), "ai"), true);
assert.equal(
    recommendedExitState(row({capability_projection: {danger: "dangerous", ai_status: "enabled"}}), "ai"),
    false,
);
// sensitive 默认开（走运行时确认，不是授权开关的推荐态）
assert.equal(
    recommendedExitState(row({capability_projection: {danger: "safe", sensitive: true, ai_status: "enabled"}}), "ai"),
    true,
);
assert.equal(recommendedExitState(row(), "mcp"), true);
assert.equal(
    recommendedExitState(row({
        capability_projection: {
            danger: "safe",
            sensitive: true,
            mcp_status: "disabled"
        }
    }), "mcp"),
    false,
);
assert.equal(
    recommendedExitState(row({
        capability_projection: {
            danger: "dangerous",
            sensitive: false,
            mcp_status: "disabled"
        }
    }), "mcp"),
    false,
);
assert.equal(recommendedExitState({}, "ai"), null);
assert.equal(
    recommendedExitState(row({capability_projection: {ai_status: "code_forbidden"}}), "ai"),
    null,
);

// recommendedDiff：干净目录（全推荐态）isClean，不产生任何 op
assert.deepEqual(
    recommendedDiff([
        row({capability_projection: {danger: "safe", sensitive: false, ai_status: "enabled", mcp_status: "enabled"}}),
    ]),
    {bindingOps: [], aiOps: [], mcpAdd: [], mcpRemove: [], isClean: true},
);

// recommendedDiff：三出口各产生需要的变更，已一致的项不重复（幂等）
const dirtyFeature = row({
    feature_id: "blink.lock",
    capability_id: "lock",
    bindings: [{binding_id: "lock", kind: "search_keyword", enabled: true}], // 推荐启用 → 无 local op
    capability_projection: {danger: "dangerous", ai_status: "enabled", mcp_status: "enabled"}, // 双双偏离
});
const offLocal = row({
    feature_id: "blink.off",
    capability_id: "off",
    bindings: [{binding_id: "off", kind: "search_keyword", enabled: false}], // 推荐启用 → 产生 enable op
    capability_projection: {danger: "safe", ai_status: "disabled", mcp_status: "disabled"}, // ai 推荐开
});
const diff = recommendedDiff([dirtyFeature, offLocal, row()]);
assert.equal(diff.isClean, false);
assert.deepEqual(diff.bindingOps, [{op: "enable", kind: "search_keyword", binding_id: "off"}]);
assert.deepEqual(diff.aiOps, [
    ["lock", false], // 危险 → 推荐关
    ["off", true], // 非危险 → 推荐开
]);
assert.deepEqual(diff.mcpAdd, ["off"]); // safe → 推荐暴露
assert.deepEqual(diff.mcpRemove, ["lock"]); // dangerous → 推荐不暴露

// recommendedDiff：空目录 / null 安全
assert.deepEqual(recommendedDiff([]), {bindingOps: [], aiOps: [], mcpAdd: [], mcpRemove: [], isClean: true});
assert.equal(recommendedDiff(null).isClean, true);

// postureSummary：可授权出口计数 + 危险对 AI 开放计数（dash 行不计入分母）
assert.deepEqual(
    postureSummary([
        row(), // ai on / mcp off
        row({capability_projection: {danger: "dangerous", ai_status: "enabled", mcp_status: "enabled"}}), // ai on + 危险 / mcp on
        row({capability_projection: {ai_status: "disabled", mcp_status: "disabled"}}), // ai off / mcp off
        {}, // Interaction-only：两口都 dash，不计入
        row({capability_projection: {ai_status: "code_forbidden", mcp_status: "disabled"}}), // ai dash / mcp 计
    ]),
    {aiOn: 2, aiTotal: 3, mcpOn: 1, mcpTotal: 4, dangerousAiOn: 1},
);
assert.deepEqual(postureSummary([]), {aiOn: 0, aiTotal: 0, mcpOn: 0, mcpTotal: 0, dangerousAiOn: 0});
assert.equal(postureSummary(null).aiTotal, 0);

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

// 控件区为单行工具栏：搜索 + 4 个筛选下拉（不再用 80px 定宽等宽字体的 .input-small）
assert.match(settingsHtml, /class="cap-toolbar"/);
assert.equal((settingsHtml.match(/class="cap-filter"/g) || []).length, 4);
assert.doesNotMatch(settingsHtml, /id="filter-\w+" class="input-small"/);

// invoke 字面量只允许已注册 command（§6.2.1 拆分护栏）
const invokedCommands = [...capsSource.matchAll(/invoke\("([\w]+)"/g)].map((m) => m[1]);
assert.deepEqual(
    [...new Set(invokedCommands)].sort(),
    [
        "apply_binding_ops",
        "get_mcp_server_config",
        "list_feature_catalog",
        "set_mcp_server_config",
        "toggle_ai_capabilities",
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

// 0.21.9/0.21.10 key（组级列 checkbox / 计数 / 无匹配 / context 芯片提示 / 姿态摘要 /
// 恢复推荐）双语言齐备；旧"全部启用/禁用"组级按钮 key 已随按列开关移除
for (const key of [
    "capabilities.group_toggle.local",
    "capabilities.group_toggle.ai",
    "capabilities.group_toggle.mcp",
    "capabilities.group_count",
    "capabilities.no_match",
    "capabilities.binding.context_hint",
    "capabilities.column.risk",
    "capabilities.posture.ai",
    "capabilities.posture.mcp",
    "capabilities.posture.dangerous_ai",
    "capabilities.posture.clean",
    "capabilities.posture.reset",
    "capabilities.reset.confirm",
]) {
    assert.ok(zh.includes(`"${key}"`), `zh 缺少 ${key}`);
    assert.ok(en.includes(`"${key}"`), `en 缺少 ${key}`);
}
assert.doesNotMatch(zh, /capabilities\.group\.enable_all/);
assert.doesNotMatch(en, /capabilities\.group\.enable_all/);

// 死 key 清理：jump_to_chord 已随 0.21.10 跳转样式统一移除
assert.doesNotMatch(zh, /capabilities\.action\.jump_to_chord/);
assert.doesNotMatch(en, /capabilities\.action\.jump_to_chord/);

// 组块矩阵契约：组块 / 组级悬浮批量开关 / 姿态摘要 / 风险列由 JS 渲染
assert.match(capsSource, /cap-group-block/);
assert.match(capsSource, /cap-group-toggle/);
assert.match(capsSource, /cap-posture/);
assert.match(capsSource, /cap-risk/);
assert.match(capsSource, /cap-no-match/);
// context 门禁动作芯片走 context.trigger.* 翻译（不再显示原始 snake_case key）
assert.match(capsSource, /context\.trigger\./);

// spec-frontend §6.1：不用 style.display 切换显隐
assert.doesNotMatch(capsSource, /style\.display/);

console.log("capabilities tab tests passed");
