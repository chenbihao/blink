/**
 * 编辑器预设菜单纯函数测试（0.27.6）
 *
 * 覆盖：菜单项投影（互译展开三项/空库兜底/自定义直通）、动作标注组合。
 * DOM 弹层与 invoke 链路属 GUI/集成层，不在 node --test 自动化范围。
 */

import {test} from "node:test";
import assert from "node:assert/strict";

globalThis.window = globalThis;

const {composeActionLabel, presetMenuItems, TRANSLATE_PRESET_ID} =
    await import("./preset-menu.js");

test("presetMenuItems 互译内置展开三项（自动/译中/译英）", () => {
    const presets = [
        {id: "builtin.editor.tidy", name: "整理", builtin: true},
        {id: TRANSLATE_PRESET_ID, name: "中英互译", builtin: true},
        {id: "custom-1", name: "自定义", builtin: false},
    ];
    const items = presetMenuItems(presets);
    assert.equal(items.length, 5);
    assert.deepEqual(items[0], {preset: "builtin.editor.tidy", label: items[0].label, translateTarget: null});
    // 互译三项共享同一 preset id，目标分别为 auto(null)/zh/en
    const translateItems = items.filter((item) => item.preset === TRANSLATE_PRESET_ID);
    assert.deepEqual(
        translateItems.map((item) => item.translateTarget),
        [null, "zh", "en"],
    );
    // 互译项 label 含方向文案（区分三项）
    assert.notDeepEqual(translateItems.map((item) => item.label), []);
    // 自定义条目直通
    const custom = items.find((item) => item.preset === "custom-1");
    assert.equal(custom.translateTarget, null);
});

test("presetMenuItems 空库回落默认整理（与后端 None 语义一致）", () => {
    const items = presetMenuItems([]);
    assert.equal(items.length, 1);
    assert.equal(items[0].preset, "builtin.editor.tidy");
    assert.equal(items[0].translateTarget, null);
    assert.equal(presetMenuItems(null).length, 1);
});

test("composeActionLabel 预设名 + 互译方向组合", () => {
    assert.equal(composeActionLabel("整理", null), "整理");
    assert.equal(composeActionLabel(null, "zh"), null, "无预设名 → 回落旧标题");
    assert.equal(composeActionLabel(null, null), null);
    // 互译方向后缀（i18n 文案，非空即可）
    const toZh = composeActionLabel("中英互译", "zh");
    const toEn = composeActionLabel("中英互译", "en");
    assert.ok(toZh && toZh.includes("中英互译"));
    assert.ok(toEn && toEn !== toZh, "中英两个方向标注不同");
});
