import test from "node:test";
import assert from "node:assert/strict";

import {formatLocalLogTimestamp, formatLogLine} from "./local-engine-log-format.js";

test("日志时间显示为本地精简格式", () => {
    const fakeDate = {
        getTime: () => 1,
        getMonth: () => 7,
        getDate: () => 29,
        getHours: () => 10,
        getMinutes: () => 48,
        getSeconds: () => 54,
        getMilliseconds: () => 6,
    };
    assert.equal(
        formatLocalLogTimestamp("2026-08-29T02:48:54.006Z", () => fakeDate),
        "08-29 10:48:54.006",
    );
});

test("无效时间保留原文，便于诊断 wire 数据", () => {
    assert.equal(formatLocalLogTimestamp("not-a-date"), "not-a-date");
});

test("复制日志与界面使用同一时间格式", () => {
    const result = formatLogLine({
        timestamp: "2026-08-29T10:48:54.006+08:00",
        level: "info",
        text: "模型加载完成",
    });
    assert.match(result, /^\[\d{2}-\d{2} \d{2}:\d{2}:\d{2}\.\d{3}\] \[info\] 模型加载完成$/);
});
