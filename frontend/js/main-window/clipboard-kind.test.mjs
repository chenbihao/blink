//! clipboard-kind 纯逻辑测试（0.25.23）。
//!
//! 运行：node --test frontend/js/main-window/clipboard-kind.test.mjs

import {describe, test} from "node:test";
import assert from "node:assert/strict";

import {KIND_ORDER, isValidKind, nextKind} from "./clipboard-kind.js";

describe("KIND_ORDER — 循环顺序", () => {
    test("全部/文本/图片/文件按顺序循环", () => {
        assert.deepEqual(KIND_ORDER, ["all", "text", "image", "file"]);
    });
});

describe("isValidKind — 合法性", () => {
    test("四个合法值", () => {
        assert.equal(isValidKind("all"), true);
        assert.equal(isValidKind("text"), true);
        assert.equal(isValidKind("image"), true);
        assert.equal(isValidKind("file"), true);
    });

    test("非法值（audio/空/undefined）", () => {
        assert.equal(isValidKind("audio"), false);
        assert.equal(isValidKind(""), false);
        assert.equal(isValidKind(undefined), false);
    });
});

describe("nextKind — Tab/Shift+Tab 循环", () => {
    test("Tab 前进：all → text → image → file → all（回绕）", () => {
        assert.equal(nextKind("all"), "text");
        assert.equal(nextKind("text"), "image");
        assert.equal(nextKind("image"), "file");
        assert.equal(nextKind("file"), "all");
    });

    test("Shift+Tab 后退：all → file → image → text → all（回绕）", () => {
        assert.equal(nextKind("all", true), "file");
        assert.equal(nextKind("file", true), "image");
        assert.equal(nextKind("image", true), "text");
        assert.equal(nextKind("text", true), "all");
    });

    test("非法当前值按 all 起步，不抛错", () => {
        assert.equal(nextKind("garbage"), "text");
        assert.equal(nextKind("garbage", true), "file");
        assert.equal(nextKind(undefined), "text");
    });
});
