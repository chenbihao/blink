//! 语音转写 UI 纯状态模块测试（0.22.15 WP5）。
//!
//! 运行：node --test frontend/js/main-window/voice-transcript-state.test.mjs

import {describe, test} from "node:test";
import assert from "node:assert/strict";

import {
    applyFinal,
    applyPartial,
    beginRecording,
    createVoiceTranscriptState,
    endRecording,
    getFinalQuery,
    hasResult,
} from "./voice-transcript-state.js";

describe("begin → preview-only → non-empty final → end", () => {
    test("preview-only 然后非空 final，最终 query = final", () => {
        let s = createVoiceTranscriptState();
        s = beginRecording(s, "旧query");
        assert.equal(s.epoch, 1);
        assert.equal(s.baseQuery, "旧query");

        s = applyPartial(s, {confirmed: "", preview: "你好", epoch: 1});
        assert.equal(s.preview, "你好");

        s = applyFinal(s, {text: "你好世界", epoch: 1});
        assert.ok(s.finalDelivered);
        assert.equal(s.finalText, "你好世界");

        s = endRecording(s);
        assert.equal(getFinalQuery(s), "你好世界");
        assert.ok(hasResult(s));
    });
});

describe("begin → non-empty preview → empty partial → end", () => {
    test("空 partial 不清空已有 preview", () => {
        let s = createVoiceTranscriptState();
        s = beginRecording(s, "");

        s = applyPartial(s, {confirmed: "", preview: "你好", epoch: 1});
        assert.equal(s.preview, "你好");

        // 空 partial 是 no-op
        s = applyPartial(s, {confirmed: "", preview: "", epoch: 1});
        assert.equal(s.preview, "你好", "空 partial 不应清空 preview");

        // 继续收到新 preview
        s = applyPartial(s, {confirmed: "", preview: "你好世界", epoch: 1});
        assert.equal(s.preview, "你好世界");
    });
});

describe("begin → preview → empty final/error → fallback", () => {
    test("空 final 不清空 confirmed/preview", () => {
        let s = createVoiceTranscriptState();
        s = beginRecording(s, "");

        s = applyPartial(s, {confirmed: "", preview: "你好", epoch: 1});
        s = applyFinal(s, {text: "", epoch: 1});
        assert.ok(!s.finalDelivered, "空 final 不应标记为已交付");
        assert.equal(s.preview, "你好", "空 final 不应清空 preview");

        // getFinalQuery 应回退到 preview
        assert.equal(getFinalQuery(s), "你好");
    });

    test("终稿为空但有 confirmed → 用 confirmed", () => {
        let s = createVoiceTranscriptState();
        s = beginRecording(s, "");

        s = applyPartial(s, {confirmed: "你好。", preview: "世界", epoch: 1});
        s = applyFinal(s, {text: "", epoch: 1});
        assert.equal(getFinalQuery(s), "你好。");
    });
});

describe("begin epoch 2 后收到 epoch 1 partial/final/end", () => {
    test("旧 epoch 的 partial 被丢弃", () => {
        let s = createVoiceTranscriptState();
        s = beginRecording(s, ""); // epoch 1
        s = applyPartial(s, {confirmed: "你好", preview: "", epoch: 1});

        // 新录音
        s = beginRecording(s, ""); // epoch 2
        assert.equal(s.confirmed, "", "新录音 confirmed 应清空");

        // 旧 epoch 1 的 partial 到达
        s = applyPartial(s, {confirmed: "旧", preview: "旧preview", epoch: 1});
        assert.equal(s.confirmed, "", "旧 epoch partial 不应写入");
        assert.equal(s.preview, "", "旧 epoch preview 不应写入");
    });

    test("旧 epoch 的 final 被丢弃", () => {
        let s = createVoiceTranscriptState();
        s = beginRecording(s, ""); // epoch 1
        s = beginRecording(s, ""); // epoch 2

        // 旧 epoch 1 的 final 到达
        s = applyFinal(s, {text: "旧录音", epoch: 1});
        assert.ok(!s.finalDelivered, "旧 epoch final 不应标记");
        assert.equal(s.finalText, null);
    });
});

describe("confirmed + preview 多句增长", () => {
    test("confirmed 只追加，不回退", () => {
        let s = createVoiceTranscriptState();
        s = beginRecording(s, "");

        s = applyPartial(s, {confirmed: "你好。", preview: "今天", epoch: 1});
        assert.equal(s.confirmed, "你好。");

        s = applyPartial(s, {confirmed: "你好。今天天气不错。", preview: "", epoch: 1});
        assert.equal(s.confirmed, "你好。今天天气不错。");

        // 乱序：更短的 confirmed 不应覆盖更长的
        s = applyPartial(s, {confirmed: "你好。", preview: "", epoch: 1});
        assert.equal(s.confirmed, "你好。今天天气不错。", "confirmed 不应回退");
    });
});

describe("final 重复到达时幂等", () => {
    test("第二次 final 不覆盖第一次", () => {
        let s = createVoiceTranscriptState();
        s = beginRecording(s, "");

        s = applyFinal(s, {text: "你好世界", epoch: 1});
        assert.equal(s.finalText, "你好世界");

        // 重复 final
        s = applyFinal(s, {text: "其他文本", epoch: 1});
        assert.equal(s.finalText, "你好世界", "重复 final 不覆盖");
    });
});

describe("原 query 非空时空录音不丢原 query", () => {
    test("空录音恢复 baseQuery", () => {
        let s = createVoiceTranscriptState();
        s = beginRecording(s, "用户之前输入的文本");

        // 没有任何 partial/final，直接 end
        s = endRecording(s);
        assert.equal(getFinalQuery(s), "用户之前输入的文本");
        assert.ok(!hasResult(s));
    });
});

describe("无 epoch 字段的兼容路径", () => {
    test("缺少 epoch 字段时不拒绝事件（兼容旧格式）", () => {
        let s = createVoiceTranscriptState();
        s = beginRecording(s, "");

        // 不带 epoch 的 partial
        s = applyPartial(s, {confirmed: "", preview: "你好"});
        assert.equal(s.preview, "你好");
    });
});

describe("终稿交付后 partial 被丢弃", () => {
    test("final 后的 partial 不影响状态", () => {
        let s = createVoiceTranscriptState();
        s = beginRecording(s, "");

        s = applyPartial(s, {confirmed: "", preview: "你好", epoch: 1});
        s = applyFinal(s, {text: "你好世界", epoch: 1});

        // 终稿后的 partial
        s = applyPartial(s, {confirmed: "其他", preview: "其他", epoch: 1});
        assert.equal(s.confirmed, "", "终稿后 partial 不应写入 confirmed");
        assert.equal(s.preview, "你好", "终稿后 partial 不应覆盖 preview");
    });
});
