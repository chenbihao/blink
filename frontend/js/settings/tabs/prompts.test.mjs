/**
 * 提示词库 Tab 纯函数测试（0.27.5）
 *
 * 覆盖：作用域过滤、上移/下移重排 payload、错误 code → 文案映射。
 * DOM 渲染与 invoke 链路属 GUI/集成层，不在 node --test 自动化范围。
 */

import {test} from "node:test";
import assert from "node:assert/strict";

// tauri.js 模块加载期需要 window（Tauri bridge 探测）；纯函数测试前先补全局
globalThis.window = globalThis;

const {reorderedIdsAfterMove, visibleEntries} = await import("./prompts.js");

/** 与后端 PromptEntryView 形状一致的 fixture（camelCase wire 格式）。 */
function entry(id, scope, extra = {}) {
    return {
        id,
        name: id,
        content: "body",
        overrideContent: null,
        effectiveContent: "body",
        scope,
        builtin: false,
        hidden: false,
        sortOrder: 0,
        revision: 1,
        ...extra,
    };
}

test("visibleEntries 按 filter 过滤作用域", () => {
    const list = [
        entry("a", "chat"),
        entry("b", "editor"),
        entry("c", "editor"),
    ];
    assert.deepEqual(visibleEntries(list, "all").map((e) => e.id), ["a", "b", "c"]);
    assert.deepEqual(visibleEntries(list, "chat").map((e) => e.id), ["a"]);
    assert.deepEqual(visibleEntries(list, "editor").map((e) => e.id), ["b", "c"]);
});

test("visibleEntries 不改变原数组（all 时也是拷贝）", () => {
    const list = [entry("a", "chat")];
    const copy = visibleEntries(list, "all");
    assert.notEqual(copy, list);
    copy.push(entry("x", "chat"));
    assert.equal(list.length, 1);
});

test("reorderedIdsAfterMove 交换相邻项并返回全量顺序", () => {
    const list = [entry("a", "chat"), entry("b", "chat"), entry("c", "chat")];
    assert.deepEqual(reorderedIdsAfterMove(list, "b", "up"), ["b", "a", "c"]);
    assert.deepEqual(reorderedIdsAfterMove(list, "b", "down"), ["a", "c", "b"]);
    // 边界：首项上移 / 末项下移 → null（无变化，不发请求）
    assert.equal(reorderedIdsAfterMove(list, "a", "up"), null);
    assert.equal(reorderedIdsAfterMove(list, "c", "down"), null);
    // 未知 id → null
    assert.equal(reorderedIdsAfterMove(list, "zz", "up"), null);
});

test("reorderedIdsAfterMove 不修改原数组顺序", () => {
    const list = [entry("a", "chat"), entry("b", "chat")];
    reorderedIdsAfterMove(list, "b", "up");
    assert.deepEqual(list.map((e) => e.id), ["a", "b"], "原数组顺序不变");
});
