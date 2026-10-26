/**
 * 配置同步 Tab 纯函数测试（0.28.6）
 *
 * 覆盖：同步报告计数、冲突值人读摘要、item key 短展示、时间格式化。
 * DOM 渲染与 invoke 链路属 GUI/集成层，不在 node --test 自动化范围。
 */

import {test} from "node:test";
import assert from "node:assert/strict";

// tauri.js 模块加载期需要 window（Tauri bridge 探测）；纯函数测试前先补全局
globalThis.window = globalThis;

const {countSummary, summarizeValue, shortItem, formatSyncTime} = await import("./sync.js");

test("countSummary 分组计数", () => {
    const summary = [
        {kind: "applied", item: "a", from: "remote"},
        {kind: "applied", item: "b", from: "remote"},
        {kind: "applied", item: "c", from: "local"},
        {kind: "overridden", item: "d", reason: "x"},
        {kind: "pending_plugin", plugin_id: "p"},
        {kind: "pending_secret", provider_id: "q"},
        {kind: "preserved_unknown", item: "e"},
    ];
    assert.deepEqual(countSummary(summary), {
        appliedRemote: 2,
        appliedLocal: 1,
        overridden: 1,
        pendingPlugins: 1,
        pendingSecrets: 1,
        preservedUnknown: 1,
    });
    assert.deepEqual(countSummary([]), {
        appliedRemote: 0,
        appliedLocal: 0,
        overridden: 0,
        pendingPlugins: 0,
        pendingSecrets: 0,
        preservedUnknown: 0,
    });
});

test("summarizeValue 长值截断、对象紧凑化、缺失显式空", () => {
    assert.equal(summarizeValue(null), "∅");
    assert.equal(summarizeValue(undefined), "∅");
    assert.equal(summarizeValue(""), '""');
    assert.equal(summarizeValue("dark"), "dark");
    // 超长字符串截断为 77 字符 + 省略号
    const long = "x".repeat(120);
    const out = summarizeValue(long);
    assert.equal(out.length, 78);
    assert.ok(out.endsWith("…"));
    // 对象紧凑 JSON
    assert.equal(summarizeValue({modifiers: ["alt"], key: " "}), '{"modifiers":["alt"],"key":" "}');
});

test("shortItem 去分片前缀", () => {
    assert.equal(shortItem("app.appearance#theme"), "theme");
    assert.equal(shortItem("app.chord#bindings.chat"), "bindings.chat");
    assert.equal(shortItem("plugin:builtin.translate#enabled"), "enabled");
    assert.equal(shortItem("prompt_library#entry/x"), "entry/x");
    assert.equal(shortItem("no-hash"), "no-hash");
});

test("formatSyncTime Unix 秒转本地时间文本", () => {
    assert.equal(formatSyncTime(0), "-");
    // 2026-10-26 00:00:00 UTC+8 → 本地时区构造按 Date 展开
    const text = formatSyncTime(1795958400);
    assert.match(text, /^\d{4}-\d{2}-\d{2} \d{2}:\d{2}$/);
});
