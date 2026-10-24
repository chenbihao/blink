import assert from "node:assert/strict";

// ── 桩（outgoing.js → i18n → tauri.js 顶层访问 window；动态 import 后置）────
globalThis.window = globalThis.window || {};
globalThis.window.__TAURI__ = {
    core: {invoke: async () => ({})},
    event: {listen: async () => ({unlisten: () => {}})},
};

const {buildOutgoingMessage, attachmentSummaryText} = await import("./outgoing.js");

const REF = "rref_aaaabbbbccccddddeeeeffff00001111";

// ── 纯文本发送：负载 = 可见正文，无附件 ──

const textOnly = buildOutgoingMessage("总结这个文件", []);
assert.deepEqual(textOnly, {text: "总结这个文件", attachments: []});

// ── 文本 + 类型化附件：可见正文保持干净，附件作为结构化元数据 ──

const withAttach = buildOutgoingMessage("总结这个文件", [
    {kind: "text_file", ref: REF, display: "笔记.txt"},
]);
assert.equal(withAttach.text, "总结这个文件");
assert.deepEqual(withAttach.attachments, [{kind: "text_file", ref: REF, display: "笔记.txt"}]);
assert.ok(!withAttach.text.includes("rref_"), "可见正文不得含 ref token");

// ── 纯附件发送（空正文）：text 为按 kind 分组的人类可读摘要 ──

const attachOnly = buildOutgoingMessage("", [
    {kind: "audio", ref: REF, display: "录音.wav"},
    {kind: "audio", ref: `rref_${"b".repeat(32)}`, display: "会议记录.wav"},
    {kind: "text_file", ref: `rref_${"c".repeat(32)}`, display: "笔记.txt"},
]);
assert.equal(attachOnly.text, "（音频附件：录音.wav、会议记录.wav；文件附件：笔记.txt）");
assert.equal(attachOnly.attachments.length, 3);
assert.ok(!attachOnly.text.includes("rref_"), "摘要不得含 ref token");
assert.ok(!attachOnly.text.includes("audio_ref"), "摘要不得含技术块");

// ── 摘要构造器：独立附件卡片场景下判定"正文是否即摘要"（避免文件名渲染两遍）──

assert.equal(
    attachmentSummaryText([{name: "录音.wav"}, "会议记录.wav"]),
    "（音频附件：录音.wav、会议记录.wav）",
    "对象（旧徽标缺 kind 按 audio）与字符串两种入参应等价",
);
assert.equal(attachmentSummaryText([]), "");
assert.equal(
    attachmentSummaryText(attachOnly.attachments),
    attachOnly.text,
    "构造摘要应与 buildOutgoingMessage 生成的摘要一致",
);

// ── 图片附件摘要分组（0.27.10）──

const imageSummary = buildOutgoingMessage("", [
    {kind: "image", ref: `rref_${"i".repeat(32)}`, display: "截图.png"},
    {kind: "text_file", ref: `rref_${"j".repeat(32)}`, display: "笔记.txt"},
]);
assert.equal(imageSummary.text, "（图片附件：截图.png；文件附件：笔记.txt）", "图片与文本分组列举");
assert.ok(!imageSummary.text.includes("rref_"));

// ── 空内容：返回 null（composer 不触发发送）──

assert.equal(buildOutgoingMessage("", []), null);
assert.equal(buildOutgoingMessage("   ", null), null);

// ── 附件卫生：非 rref_ 形态被剔除、控制字符被清洗、超长被截断 ──

const sanitized = buildOutgoingMessage("看附件", [
    {kind: "text_file", ref: "bearer-token", display: "x.txt"},
    {kind: "text_file", ref: REF, display: "a\nb.txt"},
    {kind: "text_file", ref: `rref_${"c".repeat(64)}`, display: "y".repeat(300)},
]);
assert.equal(sanitized.attachments.length, 2, "非 rref_ 形态的附件应被剔除");
assert.deepEqual(
    sanitized.attachments.map((a) => a.display),
    ["ab.txt", "y".repeat(160)],
    "display 应去控制字符并截断",
);

// ── 超过单轮上限的附件被截断（与后端 MAX_CHAT_ATTACHMENTS 对齐）──

const many = buildOutgoingMessage("批量", Array.from({length: 12}, (_, i) => ({
    kind: "text_file",
    ref: `rref_${String(i).padStart(32, "0")}`,
    display: `a${i}.txt`,
})));
assert.equal(many.attachments.length, 8);

// ── kind 缺省回退 text_file（防御旧调用形态）──

const noKind = buildOutgoingMessage("x", [{ref: REF, display: "a.txt"}]);
assert.equal(noKind.attachments[0].kind, "text_file");

console.log("Chat outgoing message tests passed");
