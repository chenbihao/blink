import {describe, test} from "node:test";
import assert from "node:assert/strict";
import {canRetryGlobalHotkey, configuredGlobalHotkey, globalHotkeyStatusTextKey, normalizeRecordedGlobalHotkey} from "./global-hotkey.js";

describe("全局快捷键录制白名单", () => {
    test("单独 F1–F11 全部可用，F12 和裸输入键拒绝", () => {
        for (let n = 1; n <= 11; n++) {
            assert.deepEqual(normalizeRecordedGlobalHotkey({modifiers: [], key: `F${n}`}), {modifiers: [], key: `f${n}`});
        }
        for (const key of ["F12", "F13", "F01", "a", "1", " ", "Enter", "Tab", "Escape", "lalt", ""]) {
            assert.equal(normalizeRecordedGlobalHotkey({modifiers: [], key}), null, key);
        }
    });
    test("原组合规则保留，左右别名规范化，未知修饰键不被静默丢弃", () => {
        assert.deepEqual(normalizeRecordedGlobalHotkey({modifiers: ["ralt", "lctrl", "ctrl"], key: "F11"}), {modifiers: ["ctrl", "alt"], key: "f11"});
        assert.deepEqual(normalizeRecordedGlobalHotkey({modifiers: ["win"], key: " "}), {modifiers: ["meta"], key: " "});
        assert.deepEqual(normalizeRecordedGlobalHotkey({modifiers: ["ctrl", "shift"], key: "A"}), {modifiers: ["ctrl", "shift"], key: "a"});
        for (const record of [{modifiers: ["shift"], key: "F1"}, {modifiers: ["ctrl"], key: "F12"}, {modifiers: ["ctrl", "fn"], key: "a"}, {key: "F1"}, null]) {
            assert.equal(normalizeRecordedGlobalHotkey(record), null);
        }
    });
});

describe("注册状态与欢迎页键位", () => {
    test("占用和系统错误可重试，无效键与已成功注册不可重试", () => {
        for (const reason of ["occupied", "error"]) assert.equal(canRetryGlobalHotkey({registered: false, reason}), true);
        for (const status of [null, {registered: false, reason: "invalid"}, {registered: true, reason: "occupied"}]) assert.equal(canRetryGlobalHotkey(status), false);
        assert.equal(globalHotkeyStatusTextKey({registered: true}), "chord.global.status.active");
        assert.equal(globalHotkeyStatusTextKey(null), "chord.global.status.pending");
        assert.equal(globalHotkeyStatusTextKey({registered: false, reason: "occupied"}), "chord.global.status.occupied");
    });
    test("自定义 F1 显示为全局键，不被 Chord Alt+A 代替", () => {
        const binding = {key: "a", modifiers: ["alt"], global: {mode: "custom", modifiers: [], key: "f1"}};
        assert.deepEqual(configuredGlobalHotkey(binding, "q"), {modifiers: [], key: "f1"});
        assert.deepEqual(configuredGlobalHotkey({global: {mode: "follow_chord"}}, "q"), {modifiers: ["alt"], key: "q"});
        assert.equal(configuredGlobalHotkey({key: "a"}, "q"), null);
    });
});
