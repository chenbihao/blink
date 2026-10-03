import {test} from "node:test";
import assert from "node:assert/strict";
import {readFile} from "node:fs/promises";
import {zh} from "./zh.js";
import {en} from "./en.js";

// 检查源码而非仅检查导入对象：重复属性在模块求值后已被覆盖。
for (const [language, dictionary] of Object.entries({zh, en})) {
    test(`${language} 字典的每个 key 只定义一次`, async () => {
        const source = await readFile(new URL(`./${language}.js`, import.meta.url), "utf8");
        const keys = new Map();
        for (const match of source.matchAll(/^[ \t]*("(?:\\.|[^"\\])*")[ \t]*:/gm)) {
            const key = JSON.parse(match[1]);
            const line = source.slice(0, match.index).split("\n").length;
            assert.equal(keys.has(key), false, `${key} 重复定义：第 ${keys.get(key)}、${line} 行`);
            keys.set(key, line);
        }
        assert.deepEqual([...keys.keys()].sort(), Object.keys(dictionary).sort(), "源码检查必须覆盖全部词条");
    });
}

test("中英字典 key 对齐", () => {
    assert.deepEqual(Object.keys(zh).sort(), Object.keys(en).sort());
});

test("中英词条的插值参数对齐", () => {
    const placeholders = value => [...new Set([...value.matchAll(/\{(\w+)\}/g)].map(match => match[1]))].sort();
    for (const key of Object.keys(zh)) {
        assert.deepEqual(placeholders(zh[key]), placeholders(en[key]), key);
    }
});
