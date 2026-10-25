import assert from "node:assert/strict";
import {test} from "node:test";

// ── 契约锚点：后端 ThinkingCapability 的 serde 投影是 snake_case ────────────
// （`can_disable` / `can_enable_default` / `default_effort`，与 status 载荷
// 其余字段一致）。曾因前端读驼峰导致「关/开·默认/始终开启」永不渲染。

const {thinkingDropdownOptions} = await import("./thinking-options.js");

function efforts(cap, current = "") {
    return thinkingDropdownOptions(cap, current)
        .rows.filter((r) => r.type === "option")
        .map((r) => r.effort);
}

test("levels capability renders none/default_on/options from snake_case fields", () => {
    const cap = {
        shape: "levels",
        levels: ["low", "high", "max"],
        can_disable: true,
        can_enable_default: true,
        default_effort: "high",
        unknown: false,
    };
    const opts = thinkingDropdownOptions(cap, "high").rows.filter((r) => r.type === "option");
    assert.deepEqual(
        opts.map((o) => o.effort),
        ["", "none", "default_on", "low", "high", "max"],
        "可关 + 可开·默认 时应包含「关」与「开」"
    );
    const high = opts.find((o) => o.effort === "high");
    assert.equal(high.sub, "目录默认档", "default_effort 命中的档位标注目录默认档");
    assert.ok(high.selected, "当前档位选中");
});

test("cannot-disable levels capability omits the none option", () => {
    const cap = {
        shape: "levels",
        levels: ["low", "high"],
        can_disable: false,
        can_enable_default: false,
        default_effort: null,
        unknown: false,
    };
    const list = efforts(cap);
    assert.ok(!list.includes("none"), "不可关模型不出现「关」");
    assert.ok(!list.includes("default_on"), "不可开·默认时不出现「开」");
    assert.deepEqual(list, ["", "low", "high"]);
});

test("unknown capability keeps full dropdown and maps default_on onto 默认", () => {
    const cap = {shape: "unknown", levels: [], can_disable: true, unknown: true};
    const result = thinkingDropdownOptions(cap, "default_on");
    assert.ok(result.showCustomInput, "未知能力保留自定义输入框");
    const def = result.rows.find((r) => r.type === "option" && r.effort === "");
    assert.ok(def.selected, "default_on 对下拉型与「默认」同为 omit——选中归并");
    const none = result.rows.find((r) => r.type === "option" && r.effort === "none");
    assert.ok(none && !none.selected);
});

test("null capability falls back to full dropdown", () => {
    const result = thinkingDropdownOptions(null, "");
    assert.ok(result.showCustomInput);
    assert.ok(efforts(null).includes("none"));
});
