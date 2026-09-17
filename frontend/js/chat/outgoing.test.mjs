import assert from "node:assert/strict";

import {buildOutgoingMessage, attachmentSummaryText} from "./outgoing.js";

const REF = "rref_aaaabbbbccccddddeeeeffff00001111";

// ── 纯文本发送：负载 = 可见正文，无附件 ──

const textOnly = buildOutgoingMessage("转写这个附件", []);
assert.deepEqual(textOnly, {text: "转写这个附件", attachments: []});

// ── 文本 + 附件：可见正文保持干净，附件作为结构化元数据 ──

const withAttach = buildOutgoingMessage("转写这个附件", [
    {audioRef: REF, displayName: "录音.wav"},
]);
assert.equal(withAttach.text, "转写这个附件");
assert.deepEqual(withAttach.attachments, [{audioRef: REF, displayName: "录音.wav"}]);
assert.ok(!withAttach.text.includes("rref_"), "可见正文不得含 ref token");

// ── 纯附件发送（空正文）：text 为人类可读摘要，标题/气泡不出现技术提示 ──

const attachOnly = buildOutgoingMessage("", [
    {audioRef: REF, displayName: "录音.wav"},
    {audioRef: `rref_${"b".repeat(32)}`, displayName: "会议记录.wav"},
]);
assert.equal(attachOnly.text, "（音频附件：录音.wav、会议记录.wav）");
assert.equal(attachOnly.attachments.length, 2);
assert.ok(!attachOnly.text.includes("rref_"), "摘要不得含 ref token");
assert.ok(!attachOnly.text.includes("audio_ref"), "摘要不得含技术块");

// ── 摘要构造器：独立附件卡片场景下判定"正文是否即摘要"（避免文件名渲染两遍）──

assert.equal(
    attachmentSummaryText([{displayName: "录音.wav"}, "会议记录.wav"]),
    "（音频附件：录音.wav、会议记录.wav）",
    "对象与字符串两种入参应等价",
);
assert.equal(attachmentSummaryText([]), "");
assert.equal(
    attachmentSummaryText(attachOnly.attachments),
    attachOnly.text,
    "构造摘要应与 buildOutgoingMessage 生成的摘要一致",
);

// ── 空内容：返回 null（composer 不触发发送）──

assert.equal(buildOutgoingMessage("", []), null);
assert.equal(buildOutgoingMessage("   ", null), null);

// ── 附件卫生：非 rref_ 形态被剔除、控制字符被清洗、超长被截断 ──

const sanitized = buildOutgoingMessage("看附件", [
    {audioRef: "bearer-token", displayName: "x.wav"},
    {audioRef: REF, displayName: "a\nb.wav"},
    {audioRef: `rref_${"c".repeat(64)}`, displayName: "y".repeat(300)},
]);
assert.equal(sanitized.attachments.length, 2, "非 rref_ 形态的附件应被剔除");
assert.deepEqual(
    sanitized.attachments.map((a) => a.displayName),
    ["ab.wav", "y".repeat(160)],
    "display name 应去控制字符并截断",
);

// ── 超过单轮上限的附件被截断（与后端 MAX_CHAT_ATTACHMENTS 对齐）──

const many = buildOutgoingMessage("批量", Array.from({length: 12}, (_, i) => ({
    audioRef: `rref_${String(i).padStart(32, "0")}`,
    displayName: `a${i}.wav`,
})));
assert.equal(many.attachments.length, 8);

console.log("Chat outgoing message tests passed");
