/**
 * CanonicalBuffer 单测：会话正文唯一真源的 revision / hash / patch 语义。
 */

import {test} from "node:test";
import assert from "node:assert/strict";
import {CanonicalBuffer, fnv1a64, fnv1a64Hex} from "./canonical-buffer.js";

test("canonical: 程序化载入不计入 revision，用户编辑计入", () => {
    const buf = new CanonicalBuffer();
    assert.equal(buf.text, "");
    assert.equal(buf.revision, 0);

    buf.load("原文");
    assert.equal(buf.text, "原文");
    assert.equal(buf.revision, 0, "程序化载入不推进 revision");

    assert.equal(buf.setFromSource("原文改"), true);
    assert.equal(buf.revision, 1);

    // 同值写入不算变更
    assert.equal(buf.setFromSource("原文改"), false);
    assert.equal(buf.revision, 1);
});

test("canonical: revision 单调且每次块级 patch 只 +1", () => {
    const buf = new CanonicalBuffer("aaa\n\nbbb\n\nccc");
    buf.applyPatches([
        {start: 0, end: 3, text: "AAA"},
        {start: 10, end: 13, text: "CCC"},
    ]);
    assert.equal(buf.text, "AAA\n\nbbb\n\nCCC");
    assert.equal(buf.revision, 1, "一次 applyPatches 只推进一个 revision");
});

test("canonical: 非法 patch 不产生任何修改", () => {
    const buf = new CanonicalBuffer("abcdef");
    const snapshot = buf.text;

    assert.equal(buf.applyPatches([]), false);
    assert.equal(buf.applyPatches(null), false);
    assert.equal(buf.applyPatches([{start: 0, end: 99, text: "x"}]), false, "越界拒绝");
    assert.equal(buf.applyPatches([{start: -1, end: 2, text: "x"}]), false, "负下标拒绝");
    assert.equal(buf.applyPatches([{start: 2.5, end: 3, text: "x"}]), false, "非整数拒绝");
    assert.equal(
        buf.applyPatches([{start: 3, end: 5, text: "x"}, {start: 1, end: 2, text: "y"}]),
        false,
        "乱序拒绝",
    );
    assert.equal(
        buf.applyPatches([{start: 0, end: 3, text: "x"}, {start: 2, end: 4, text: "y"}]),
        false,
        "重叠拒绝",
    );
    assert.equal(buf.applyPatches([{start: 0, end: 2, text: 7}]), false, "非字符串替换体拒绝");

    assert.equal(buf.text, snapshot, "非法 patch 必须零副作用");
    assert.equal(buf.revision, 0);
});

test("canonical: 等值 patch 不推进 revision（回退到原文本不算新版本）", () => {
    const buf = new CanonicalBuffer("abc\n\ndef");
    assert.equal(buf.applyPatches([{start: 0, end: 3, text: "abc"}]), false);
    assert.equal(buf.revision, 0);
});

test("canonical: hash 与后端 body_digest 同算法（FNV-1a 64 / UTF-8 字节）", () => {
    // 手工推导的固定值：FNV-1a 64("a") = 0xaf63dc4c8601ec8c
    assert.equal(fnv1a64Hex("a"), "af63dc4c8601ec8c");
    // FNV-1a 64("") = 偏移基准
    assert.equal(fnv1a64Hex(""), "cbf29ce484222325");
    assert.equal(fnv1a64("你好").toString(16), fnv1a64Hex("你好").replace(/^0+/, ""));

    const buf = new CanonicalBuffer("你好，世界");
    const before = buf.hash;
    buf.setFromSource("你好，世界 ");
    assert.notEqual(buf.hash, before, "内容变化后摘要必须变化");
    assert.equal(buf.hash, fnv1a64("你好，世界 "));
});

test("canonical: reset 清空正文与版本", () => {
    const buf = new CanonicalBuffer("x");
    buf.setFromSource("y");
    buf.reset();
    assert.equal(buf.text, "");
    assert.equal(buf.length, 0);
    assert.equal(buf.revision, 0);
    assert.equal(buf.hash, fnv1a64(""));
});

test("canonical: 2M 字符 Source envelope 下 patch 仍精确", () => {
    const head = "中".repeat(1_999_990);
    const buf = new CanonicalBuffer(`${head}尾巴`);
    assert.equal(buf.applyPatches([{start: head.length, end: head.length + 2, text: "尾部"}]),
        true);
    assert.equal(buf.text, `${head}尾部`);
    assert.equal(buf.length, head.length + 2);
});
