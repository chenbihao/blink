import assert from "node:assert/strict";

// 0.27.9 @ 附件面板纯函数测试（phase §5.9.4 / §11 测试映射）

// ── 桩（attachment-palette.js 顶层 import 链）───────────────────────────────

globalThis.window = globalThis.window || {};
globalThis.window.__TAURI__ = {
    core: {invoke: async () => ({})},
    event: {listen: async () => ({unlisten: () => {}})},
};
globalThis.document = {
    getElementById: () => null,
    addEventListener() {},
    removeEventListener() {},
};

const palette = await import("./attachment-palette.js");

// ── splitAtQuery：触发契约（§九：仅首非空白字符 @ 且单行触发）──────────────

assert.deepEqual(palette.splitAtQuery("@文件"), {query: "文件"});
assert.deepEqual(palette.splitAtQuery("  @ "), {query: ""});
assert.deepEqual(palette.splitAtQuery("@"), {query: ""});
assert.equal(palette.splitAtQuery("你好"), null, "非 @ 开头不触发");
assert.equal(palette.splitAtQuery("/cmd"), null, "斜杠命令不触发附件面板");
assert.equal(palette.splitAtQuery("@a\n@b"), null, "多行不触发");
assert.equal(palette.splitAtQuery("  你@好"), null, "@ 不在首位不触发");

// ── filterCandidateRows：query 命中标签或文件名 ─────────────────────────────

const rows = [
    {label: "选择文件…", names: []},
    {label: "笔记.md", names: ["笔记.md"]},
    {label: "3 个文件", names: ["a.txt", "b.log", "数据.csv"]},
    {label: "录音.wav", names: ["录音.wav"]},
];
assert.equal(palette.filterCandidateRows(rows, "").length, 4, "空 query 全量");
assert.equal(palette.filterCandidateRows(rows, "笔记").length, 1);
assert.equal(palette.filterCandidateRows(rows, "A").length, 2, "大小写不敏感——'A' 命中 'a.txt' 与 '录音.wav'");
assert.equal(palette.filterCandidateRows(rows, "a").length, 2, "子串命中 names（a.txt 与 .wav）");
assert.equal(palette.filterCandidateRows(rows, "数据").length, 1, "命中组内文件名");
assert.equal(palette.filterCandidateRows(rows, "录音").length, 1);
assert.equal(palette.filterCandidateRows(rows, "不存在").length, 0, "无命中返回空");

// ── entryAttachable：最近文件条目至少一个可附加才可整体采纳 ─────────────────

assert.equal(palette.entryAttachable({files: [{attachable: true}, {attachable: false}]}), true);
assert.equal(palette.entryAttachable({files: [{attachable: false}]}), false);
assert.equal(palette.entryAttachable({files: []}), false, "空文件组不可采纳");
assert.equal(palette.entryAttachable({}), false, "防御空条目");

// ── ATTACHMENT_ICONS：三种 kind 均有 sprite 内图标 ─────────────────────────

assert.equal(palette.ATTACHMENT_ICONS.audio, "audio-lines");
assert.equal(palette.ATTACHMENT_ICONS.text_file, "file-text");
assert.equal(palette.ATTACHMENT_ICONS.image, "image");

// ── attachmentDisplayName：label 缺失按 kind 兜底 ───────────────────────────

assert.equal(palette.attachmentDisplayName({label: "笔记.md"}, "text_file"), "笔记.md");
assert.equal(palette.attachmentDisplayName({label: null}, "text_file").length > 0, true, "兜底非空");
assert.equal(palette.attachmentDisplayName({display: "截图.png"}, "image"), "截图.png");

console.log("Chat attachment palette tests passed");
