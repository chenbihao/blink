import assert from "node:assert/strict";
import test from "node:test";

import {
    findTapActionById,
    findTapActionByKey,
    isAvailableWithQuery,
    isClipboardModeSwitch,
} from "./chord-availability.js";

const actions = [
    {id: "screenshot", key: "a", semantic: "tap", requires_input: false, available_with_query: false},
    {id: "clipboard_history", key: "c", semantic: "tap", requires_input: false, available_with_query: true},
    {id: "edit", key: "x", semantic: "tap", requires_input: true, available_with_query: true},
];

test("clipboard 模式切换在非空 query 下仍可触发", () => {
    assert.equal(isAvailableWithQuery(actions[1]), true);
    assert.equal(findTapActionByKey(actions, "C", true)?.id, "clipboard_history");
    assert.equal(findTapActionById(actions, "clipboard_history", true)?.key, "c");
});

test("普通入口在非空 query 下继续让位", () => {
    assert.equal(findTapActionByKey(actions, "a", true), null);
    assert.equal(findTapActionByKey(actions, "a", false)?.id, "screenshot");
});

test("动作 id 语义不依赖默认键位", () => {
    assert.equal(findTapActionById(actions, "edit", true)?.key, "x");
});

test("clipboard action 采用主窗原地模式切换", () => {
    assert.equal(isClipboardModeSwitch(actions[1]), true);
    assert.equal(isClipboardModeSwitch(actions[2]), false);
});
