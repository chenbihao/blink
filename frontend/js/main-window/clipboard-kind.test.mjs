//! clipboard-kind 纯逻辑测试（0.25.23）。
//!
//! 运行：node --test frontend/js/main-window/clipboard-kind.test.mjs

import {describe, test} from "node:test";
import assert from "node:assert/strict";

import {KIND_ORDER, isValidKind, nextKind} from "./clipboard-kind.js";

describe("KIND_ORDER — 循环顺序", () => {
    test("当前只含 全部/文本/图片（文件/音频待采集支持）", () => {
        assert.deepEqual(KIND_ORDER, ["all", "text", "image"]);
    });
});

describe("isValidKind — 合法性", () => {
    test("三个合法值", () => {
        assert.equal(isValidKind("all"), true);
        assert.equal(isValidKind("text"), true);
        assert.equal(isValidKind("image"), true);
    });

    test("非法值（file/audio/空/undefined）", () => {
        assert.equal(isValidKind("file"), false);
        assert.equal(isValidKind("audio"), false);
        assert.equal(isValidKind(""), false);
        assert.equal(isValidKind(undefined), false);
    });
});

describe("nextKind — Tab/Shift+Tab 循环", () => {
    test("Tab 前进：all → text → image → all（回绕）", () => {
        assert.equal(nextKind("all"), "text");
        assert.equal(nextKind("text"), "image");
        assert.equal(nextKind("image"), "all");
    });

    test("Shift+Tab 后退：all → image → text → all（回绕）", () => {
        assert.equal(nextKind("all", true), "image");
        assert.equal(nextKind("image", true), "text");
        assert.equal(nextKind("text", true), "all");
    });

    test("非法当前值按 all 起步，不抛错", () => {
        assert.equal(nextKind("garbage"), "text");
        assert.equal(nextKind("garbage", true), "image");
        assert.equal(nextKind(undefined), "text");
    });
});
