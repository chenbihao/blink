import {describe, test} from "node:test";
import assert from "node:assert/strict";
import {canRetryGlobalHotkey, configuredGlobalHotkey, explainGlobalHotkeyRejection, globalHotkeyStatusTextKey, normalizeRecordedGlobalHotkey} from "./global-hotkey.js";

// ── 0.26.18 共享测试向量 ─────────────────────────────────────────────────────
// 与 Rust 侧 crates/blink-infra/src/platform/hotkey/mod.rs 的 test_vectors 同键集：
// IPC 层 is_supported_global_hotkey 与注册层 vk_for_key/vk_to_key 消费同一份向量。
// 扩展键集时两处必须同步（JS 无法 import Rust 模块，靠注释互指锁定一致性）。
const SUPPORTED_COMBO_KEYS = [
    // 字母 / 数字 / 空格
    "a", "z", "0", "9", " ",
    // OEM 标点（VK_OEM_1..8 区段，US 布局未加 Shift 字符）
    "`", "-", "=", "[", "]", "\\", ";", "'", ",", ".", "/", "~",
    // 方向键 / 导航键
    "up", "down", "left", "right", "pageup", "pagedown", "home", "end",
    // 功能键
    "f1", "f11",
];
const REJECTED_KEYS = [
    "tab", "enter", "escape", "esc", "capslock", "backspace", "printscreen", "delete",
    "insert", "f12", "f13", "arrowup", "spacebar", "", "aa",
];
const BARE_KEYS = Array.from({length: 11}, (_, i) => `f${i + 1}`);

describe("全局快捷键录制白名单", () => {
    test("单独 F1–F11 全部可用，F12 和裸输入键拒绝", () => {
        for (const key of BARE_KEYS) {
            assert.deepEqual(normalizeRecordedGlobalHotkey({modifiers: [], key}), {modifiers: [], key});
        }
        for (const key of ["F12", "F13", "F01", "a", "1", " ", "Enter", "Tab", "Escape", "lalt", ""]) {
            assert.equal(normalizeRecordedGlobalHotkey({modifiers: [], key}), null, key);
        }
    });
    test("0.26.18 扩展键集与共享向量一致（OEM 标点/方向键/导航键）", () => {
        for (const key of SUPPORTED_COMBO_KEYS) {
            const combo = normalizeRecordedGlobalHotkey({modifiers: ["ctrl"], key: key.length === 1 ? key.toUpperCase() : key});
            assert.deepEqual(combo, {modifiers: ["ctrl"], key}, `ctrl+${key} 应可用`);
            // normalize 与 explain 对同一输入判定不可分叉
            assert.equal(explainGlobalHotkeyRejection({modifiers: ["ctrl"], key}), null, key);
        }
        for (const key of REJECTED_KEYS) {
            assert.equal(normalizeRecordedGlobalHotkey({modifiers: ["ctrl"], key}), null, `ctrl+${key} 应拒绝`);
            const expectedCode = key === "" ? "malformed" : "unsupported_key";
            assert.equal(explainGlobalHotkeyRejection({modifiers: ["ctrl"], key}).code, expectedCode, key);
        }
        // 录制产出的大写形态归一命中
        assert.deepEqual(normalizeRecordedGlobalHotkey({modifiers: ["ctrl"], key: "Up"}), {modifiers: ["ctrl"], key: "up"});
        assert.deepEqual(normalizeRecordedGlobalHotkey({modifiers: ["alt"], key: "`"}), {modifiers: ["alt"], key: "`"});
        // 纯 Shift 组合仍拒绝
        assert.equal(normalizeRecordedGlobalHotkey({modifiers: ["shift"], key: "up"}), null);
        assert.equal(normalizeRecordedGlobalHotkey({modifiers: ["shift"], key: "`"}), null);
    });
    test("原组合规则保留，左右别名规范化，未知修饰键不被静默丢弃", () => {
        assert.deepEqual(normalizeRecordedGlobalHotkey({modifiers: ["ralt", "lctrl", "ctrl"], key: "F11"}), {modifiers: ["ctrl", "alt"], key: "f11"});
        assert.deepEqual(normalizeRecordedGlobalHotkey({modifiers: ["win"], key: " "}), {modifiers: ["meta"], key: " "});
        assert.deepEqual(normalizeRecordedGlobalHotkey({modifiers: ["ctrl", "shift"], key: "A"}), {modifiers: ["ctrl", "shift"], key: "a"});
        for (const record of [{modifiers: ["shift"], key: "F1"}, {modifiers: ["ctrl"], key: "F12"}, {modifiers: ["ctrl", "fn"], key: "a"}, {key: "F1"}, null]) {
            assert.equal(normalizeRecordedGlobalHotkey(record), null);
        }
    });
    test("explainGlobalHotkeyRejection 返回带具体原因的结构化拒绝原因", () => {
        // 不支持的按键：携带键名（录制到的形态，未强制小写）
        assert.deepEqual(explainGlobalHotkeyRejection({modifiers: ["ctrl"], key: "Tab"}), {code: "unsupported_key", key: "Tab"});
        assert.deepEqual(explainGlobalHotkeyRejection({modifiers: ["ctrl"], key: "F12"}), {code: "unsupported_key", key: "F12"});
        // 不支持的修饰键：携带修饰键名
        assert.deepEqual(explainGlobalHotkeyRejection({modifiers: ["ctrl", "fn"], key: "a"}), {code: "unsupported_modifier", modifier: "fn"});
        // 缺少 Ctrl/Alt/Win：裸键非 F1–F11 / 纯 Shift 组合
        assert.deepEqual(explainGlobalHotkeyRejection({modifiers: [], key: "up"}), {code: "modifier_required"});
        assert.deepEqual(explainGlobalHotkeyRejection({modifiers: ["shift"], key: "`"}), {code: "modifier_required"});
        // 结构非法
        assert.deepEqual(explainGlobalHotkeyRejection(null), {code: "malformed"});
        assert.deepEqual(explainGlobalHotkeyRejection({modifiers: ["ctrl"], key: ""}), {code: "malformed"});
        // 有效组合返回 null
        assert.equal(explainGlobalHotkeyRejection({modifiers: ["ctrl"], key: "pageup"}), null);
        assert.equal(explainGlobalHotkeyRejection({modifiers: [], key: "f5"}), null);
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
