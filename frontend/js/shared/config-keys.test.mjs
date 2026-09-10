/**
 * config-keys.js — chord toggles payload wire contract 测试。
 *
 * 验证：
 * 1. payload 字段名是 camelCase（chordEnabled / chordHintVisible），
 *    与后端 ChordTogglesUpdate #[serde(rename_all = "camelCase")] 对齐。
 * 2. 布尔值正确传递。
 * 3. 非布尔输入安全归一化为布尔。
 */

import {describe, test} from "node:test";
import assert from "node:assert";

// tauri.js 模块加载时会给 window.alert/confirm/prompt 打补丁，先 mock window
globalThis.window = globalThis;

const {buildChordTogglesPayload} = await import("./config-keys.js");

describe("buildChordTogglesPayload — wire contract", () => {
    test("字段名是 camelCase（chordEnabled / chordHintVisible）", () => {
        const p = buildChordTogglesPayload(true, false);
        assert.ok("chordEnabled" in p, "必须有 chordEnabled 字段");
        assert.ok("chordHintVisible" in p, "必须有 chordHintVisible 字段");
        assert.ok(!("chord_enabled" in p), "不得有 snake_case chord_enabled");
        assert.ok(!("chord_hint_visible" in p), "不得有 snake_case chord_hint_visible");
    });

    test("布尔值正确传递", () => {
        assert.deepStrictEqual(buildChordTogglesPayload(true, true), {chordEnabled: true, chordHintVisible: true});
        assert.deepStrictEqual(buildChordTogglesPayload(false, false), {chordEnabled: false, chordHintVisible: false});
        assert.deepStrictEqual(buildChordTogglesPayload(true, false), {chordEnabled: true, chordHintVisible: false});
    });

    test("非布尔输入安全归一化为布尔（=== true 语义）", () => {
        assert.deepStrictEqual(buildChordTogglesPayload(1, 0), {chordEnabled: false, chordHintVisible: false});
        assert.deepStrictEqual(buildChordTogglesPayload(null, undefined), {
            chordEnabled: false,
            chordHintVisible: false
        });
        assert.deepStrictEqual(buildChordTogglesPayload("yes", ""), {chordEnabled: false, chordHintVisible: false});
        // 只有真正的 boolean true 才是 true
        assert.deepStrictEqual(buildChordTogglesPayload(true, true), {chordEnabled: true, chordHintVisible: true});
    });
});
