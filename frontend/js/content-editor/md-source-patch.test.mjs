/**
 * md-source-patch 单测：源码感知的块级 patch。
 *
 * 分两部分：
 * 1. **纯逻辑**（始终运行）：用可注入的 parse/serialize 假体，断言块扫描、
 *    对齐验证与最小窗口 patch 的语义——未编辑区间必须逐字节保留；
 * 2. **真实 Tiptap 集成**（依赖存在时运行，缺失时显式跳过）：直接使用与生产
 *    同版本的 `MarkdownManager`（`target/editor-markdown-build`），证明
 *    「MD 局部编辑不改未编辑区间」在真实序列化器上成立。
 */

import {test} from "node:test";
import assert from "node:assert/strict";
import {
    buildBlockMap,
    diffTopNodes,
    isEmptyParagraphNode,
    jsonEqual,
    planSourcePatch,
    scanTopLevelBlocks,
} from "./md-source-patch.js";
import {CanonicalBuffer} from "./canonical-buffer.js";
import {evaluateMarkdownGate} from "./engines/markdown-gate.js";

// ── 假体：模拟 Tiptap 的规范化重写（`*`→`-`、`__`→`**`、Setext→ATX）─────────

function normalizeBlock(blockText) {
    return blockText
        .split("\n")
        .map((line) => line
            .replace(/^(\s*)[-+*](\s)/, "$1-$2")
            .replace(/__([^_\n]+)__/g, "**$1**")
            .replace(/[ \t]+$/, ""))
        .join("\n")
        .replace(/^([^\n]+)\n=+$/, "# $1")
        .replace(/^([^\n]+)\n-+$/, "## $1");
}

const fakeParseBlock = (text) => ({type: "block", text: normalizeBlock(text)});
const fakeSerializeNode = (node) => node.text;
const fakeDoc = (blockTexts) => ({
    type: "doc",
    content: blockTexts.map((t) => fakeParseBlock(t)),
});

/** 用 CanonicalBuffer 应用 patch（顺带验证 patch 合法性契约） */
function apply(text, patches) {
    const buf = new CanonicalBuffer(text);
    const ok = buf.applyPatches(patches);
    return {ok, text: buf.text};
}

// ── 块扫描不变量 ────────────────────────────────────────────────────────────

test("scan: 块区间连续覆盖全文，尾部空白归入最后一块", () => {
    const samples = [
        "a",
        "a\n",
        "a\n\n",
        "a\n\n\n\nb",
        "* a\n* b\n\nTitle\n=====\n\n正文",
        "```js\nlet a = 1;\n\n// 空行在围栏内\n```\n\n尾部",
        "> 引用\n> 第二行\n\n正文",
        "1. 甲\n2. 乙\n\n- [ ] 任务\n\n---\n\n结语",
        "\n\n\n前导空行",
        "全部\n是\n空白\n\n\n",
    ];
    for (const src of samples) {
        const blocks = scanTopLevelBlocks(src);
        if (src.trim() === "") {
            assert.equal(blocks.length, 0, `${JSON.stringify(src)} 全空白应无块`);
            continue;
        }
        assert.ok(blocks.length > 0, `${JSON.stringify(src)} 应有块`);
        assert.equal(blocks[0].start, 0, "首块从 0 开始");
        assert.equal(blocks.at(-1).end, src.length, "末块覆盖到文末");
        for (let i = 1; i < blocks.length; i += 1) {
            assert.equal(blocks[i].start, blocks[i - 1].end, "块区间必须首尾相接");
        }
        for (const b of blocks) {
            assert.ok(b.contentStart >= b.start && b.contentEnd <= b.end && b.contentEnd > b.contentStart);
            assert.equal(b.text, src.slice(b.contentStart, b.contentEnd));
            assert.notEqual(b.text.trim(), "", "内容块不得是空白块");
        }
    }
});

test("scan: Setext 下划线与段落在同一块；围栏内空行不切块", () => {
    const setext = scanTopLevelBlocks("Title\n=====\n\n正文");
    assert.equal(setext.length, 2);
    assert.equal(setext[0].text, "Title\n=====");

    const setextH2 = scanTopLevelBlocks("副标题\n---\n\n正文");
    assert.equal(setextH2.length, 2, "`---` 在段落后应视为 Setext 下划线而非分隔线");
    assert.equal(setextH2[0].text, "副标题\n---");

    const fenced = scanTopLevelBlocks("```\na\n\nb\n```\n\n尾部");
    assert.equal(fenced.length, 2, "围栏内的空行不得切成两块");
});

test("scan: 分隔线独立成块（不与前段落粘连）", () => {
    const blocks = scanTopLevelBlocks("a\n\n***\n\nb");
    assert.deepEqual(blocks.map((b) => b.text), ["a", "***", "b"]);
});

// ── 对齐验证 ────────────────────────────────────────────────────────────────

test("buildBlockMap: 内容块与内容节点一一对应时通过", () => {
    const src = "* 甲\n* 乙\n\nTitle\n=====\n\n正文";
    const doc = fakeDoc(["- 甲\n- 乙", "# Title", "正文"]);
    const map = buildBlockMap(src, doc, fakeParseBlock);
    assert.equal(map.ok, true);
    assert.equal(map.blocks.length, 3);
});

test("buildBlockMap: 块数与节点数不符 → 拒绝（触发只读降级）", () => {
    const src = "a\n\nb";
    const doc = fakeDoc(["a"]);
    assert.equal(buildBlockMap(src, doc, fakeParseBlock).ok, false);
});

test("buildBlockMap: 内容形态不符 → 拒绝", () => {
    const src = "a\n\nb";
    const doc = fakeDoc(["a", "被改过的内容"]);
    const map = buildBlockMap(src, doc, fakeParseBlock);
    assert.equal(map.ok, false);
    assert.equal(map.reason, "align");
});

test("buildBlockMap: 空段落节点允许出现在任意位置（不占源文块）", () => {
    const src = "a\n\nb";
    const doc = {
        type: "doc",
        content: [
            {type: "paragraph", content: [{type: "text", text: "a"}]},
            {type: "paragraph", content: []},
            {type: "paragraph", content: [{type: "text", text: "b"}]},
        ],
    };
    const parse = (t) => ({type: "paragraph", content: [{type: "text", text: t}]});
    assert.equal(buildBlockMap(src, doc, parse).ok, true);
});

// ── 最小变更窗口 ────────────────────────────────────────────────────────────

test("diffTopNodes: 取最小变更窗口（公共前后缀不重叠）", () => {
    const a = [{t: "1"}, {t: "2"}, {t: "3"}, {t: "4"}];
    const b = [{t: "1"}, {t: "2x"}, {t: "3"}, {t: "4"}];
    assert.deepEqual(diffTopNodes(a, b), {
        beforeStart: 1, beforeEnd: 2, afterStart: 1, afterEnd: 2,
    });

    const insert = [{t: "1"}, {t: "X"}, {t: "2"}, {t: "3"}, {t: "4"}];
    assert.deepEqual(diffTopNodes(a, insert), {
        beforeStart: 1, beforeEnd: 1, afterStart: 1, afterEnd: 2,
    });

    const allNew = [{t: "9"}];
    assert.deepEqual(diffTopNodes(a, allNew), {
        beforeStart: 0, beforeEnd: 4, afterStart: 0, afterEnd: 1,
    });

    assert.ok(jsonEqual({a: [1, {b: 2}]}, {a: [1, {b: 2}]}));
    assert.ok(!jsonEqual({a: [1, {b: 2}]}, {a: [1, {b: 3}]}));
});

// ── 核心：局部编辑不改未编辑区间 ────────────────────────────────────────────

test("patch: MD 局部编辑只改写变更块——列表符号与 Setext 逐字节保留", () => {
    const src = "* 甲\n* 乙\n\nTitle\n=====\n\n正文";
    const doc = fakeDoc(["- 甲\n- 乙", "# Title", "正文"]);
    const map = buildBlockMap(src, doc, fakeParseBlock);
    assert.equal(map.ok, true);

    const before = doc.content;
    const after = [...before.slice(0, 2), {type: "block", text: "正文改"}];
    const patches = planSourcePatch({
        sourceText: src,
        blocks: map.blocks,
        beforeNodes: before,
        afterNodes: after,
        serializeNode: fakeSerializeNode,
    });
    assert.equal(patches.length, 1);

    const {ok, text} = apply(src, patches);
    assert.equal(ok, true);
    // 关键断言：未编辑区间逐字节不变（`*` 列表符号未被规范化成 `-`，Setext 未被改成 ATX）
    assert.equal(text, "* 甲\n* 乙\n\nTitle\n=====\n\n正文改");
    assert.ok(text.startsWith("* 甲\n* 乙\n\nTitle\n====="), "未编辑前缀必须逐字节一致");
});

test("patch: 中间块插入/删除时两侧块内容逐字节保留", () => {
    const src = "* 甲\n* 乙\n\nTitle\n=====\n\n尾部段落";
    const doc = fakeDoc(["- 甲\n- 乙", "# Title", "尾部段落"]);
    const map = buildBlockMap(src, doc, fakeParseBlock);
    const before = doc.content;

    // 在块 1 与块 2 之间插入一个新块（窗口 = 纯插入）
    const inserted = [
        before[0],
        before[1],
        {type: "block", text: "插入段"},
        before[2],
    ];
    const insertPatches = planSourcePatch({
        sourceText: src,
        blocks: map.blocks,
        beforeNodes: before,
        afterNodes: inserted,
        serializeNode: fakeSerializeNode,
    });
    const insertedText = apply(src, insertPatches).text;
    assert.equal(insertedText, "* 甲\n* 乙\n\nTitle\n=====\n\n插入段\n\n尾部段落");

    // 删除块 1
    const removed = [before[0], before[2]];
    const removePatches = planSourcePatch({
        sourceText: src,
        blocks: map.blocks,
        beforeNodes: before,
        afterNodes: removed,
        serializeNode: fakeSerializeNode,
    });
    const removedText = apply(src, removePatches).text;
    assert.equal(removedText, "* 甲\n* 乙\n\n尾部段落");
});

test("patch: 编辑末块不改动前部空行与 CRLF 归一化后的字节", () => {
    // 连续空行 + 尾部空行：gap 必须原样保留
    const src = "前部\n\n\n\n中部\n\n尾部\n\n";
    const blocks = scanTopLevelBlocks(src);
    // 该源文在真实 Tiptap 下会产出空段落节点（连续空行），此处模拟同样的节点结构
    const parse = (t) => ({type: "paragraph", content: [{type: "text", text: t.trim()}]});
    const before = [
        {type: "paragraph", content: [{type: "text", text: "前部"}]},
        {type: "paragraph", content: []},
        {type: "paragraph", content: [{type: "text", text: "中部"}]},
        {type: "paragraph", content: [{type: "text", text: "尾部"}]},
        {type: "paragraph", content: []},
    ];
    const after = [...before];
    after[3] = {type: "paragraph", content: [{type: "text", text: "尾部改"}]};

    const patches = planSourcePatch({
        sourceText: src,
        blocks,
        beforeNodes: before,
        afterNodes: after,
        serializeNode: (n) => (n.content?.[0]?.text ?? ""),
    });
    const {ok, text} = apply(src, patches);
    assert.equal(ok, true);
    assert.equal(text, "前部\n\n\n\n中部\n\n尾部改\n\n", "除末块内容外全部逐字节保留（含尾部空行）");
    assert.equal(text.slice(0, src.indexOf("尾部")), src.slice(0, src.indexOf("尾部")));
});

test("patch: 无变化时不产生任何 patch", () => {
    const src = "a\n\nb";
    const doc = fakeDoc(["a", "b"]);
    const map = buildBlockMap(src, doc, fakeParseBlock);
    const patches = planSourcePatch({
        sourceText: src,
        blocks: map.blocks,
        beforeNodes: doc.content,
        afterNodes: doc.content,
        serializeNode: fakeSerializeNode,
    });
    assert.deepEqual(patches, []);
});

test("patch: 纯空段落增删按 gap 长度调整（1 个空段落 ↔ 2 个换行）", () => {
    const src = "a\n\nb";
    const blocks = scanTopLevelBlocks(src);
    const parse = (t) => ({type: "paragraph", content: [{type: "text", text: t}]});
    const empty = {type: "paragraph", content: []};
    const a = {type: "paragraph", content: [{type: "text", text: "a"}]};
    const b = {type: "paragraph", content: [{type: "text", text: "b"}]};
    void parse;

    // 新增一个空段落（窗口内无内容块）
    const grow = planSourcePatch({
        sourceText: src,
        blocks,
        beforeNodes: [a, b],
        afterNodes: [a, empty, b],
        serializeNode: (n) => (n.content?.[0]?.text ?? ""),
    });
    assert.deepEqual(apply(src, grow).text, "a\n\n\n\nb");

    // 删除一个空段落（源文含 4 个连续换行）
    const withEmpty = "a\n\n\n\nb";
    const shrink = planSourcePatch({
        sourceText: withEmpty,
        blocks: scanTopLevelBlocks(withEmpty),
        beforeNodes: [a, empty, b],
        afterNodes: [a, b],
        serializeNode: (n) => (n.content?.[0]?.text ?? ""),
    });
    assert.deepEqual(apply(withEmpty, shrink).text, "a\n\nb");
});

test("patch: 新块遵循邻近换行风格，且编辑块的 CRLF 行尾不被改成 LF", () => {
    const src = "前部\r\n\r\n尾部\r\n";
    const blocks = scanTopLevelBlocks(src);
    const before = [
        {type: "paragraph", content: [{type: "text", text: "前部"}]},
        {type: "paragraph", content: [{type: "text", text: "尾部"}]},
    ];
    const after = [
        before[0],
        {type: "paragraph", content: [{type: "text", text: "插入"}]},
        {type: "paragraph", content: [{type: "text", text: "尾部"}]},
    ];
    const patches = planSourcePatch({
        sourceText: src,
        blocks,
        beforeNodes: before,
        afterNodes: after,
        serializeNode: (node) => node.content[0].text,
    });
    const inserted = apply(src, patches).text;
    assert.equal(inserted, "前部\r\n\r\n插入\r\n\r\n尾部\r\n");

    const changed = planSourcePatch({
        sourceText: src,
        blocks,
        beforeNodes: before,
        afterNodes: [before[0], {type: "paragraph", content: [{type: "text", text: "尾部改"}]}],
        serializeNode: (node) => node.content[0].text,
    });
    assert.equal(apply(src, changed).text, "前部\r\n\r\n尾部改\r\n");
});

test("patch: 空文档插入首块（不产生伪造的前导空段落）", () => {
    const patches = planSourcePatch({
        sourceText: "",
        blocks: [],
        beforeNodes: [],
        afterNodes: [{type: "paragraph", content: [{type: "text", text: "首段"}]}],
        serializeNode: (n) => n.content[0].text,
    });
    assert.deepEqual(apply("", patches).text, "首段");

    // 全空白文档同理：空白不承载节点，插入首块时被首块内容替换
    const blank = "   \n\n";
    const blankPatches = planSourcePatch({
        sourceText: blank,
        blocks: [],
        beforeNodes: [],
        afterNodes: [{type: "paragraph", content: [{type: "text", text: "首段"}]}],
        serializeNode: (n) => n.content[0].text,
    });
    assert.deepEqual(apply(blank, blankPatches).text, "首段");
});

// ── 真实 Tiptap 集成（依赖缺失时显式跳过）───────────────────────────────────

let engineMod = null;
let engineErr = null;
try {
    engineMod = await import("../../../testdata/editor/markdown/lib/engine.mjs");
    engineMod.ensureDeps();
} catch (e) {
    engineErr = e;
}

const realTiptapTest = engineErr ? test.skip : test;

if (engineErr) {
    console.log(`[md-source-patch] 跳过真实 Tiptap 集成用例：${engineErr.message.split("\n")[0]}`);
}

/** 与生产 MarkdownIrEngine 相同的单块解析/序列化口径 */
function makeRealAdapters(manager) {
    const parseBlock = (blockText) => {
        const json = manager.parse(blockText);
        const content = json?.content;
        if (!Array.isArray(content) || content.length !== 1) return null;
        return content[0];
    };
    const serializeNode = (node) => {
        const raw = manager.serialize({type: "doc", content: [node]});
        return typeof raw === "string" ? raw.replace(/^\n+/, "").replace(/\s+$/, "") : "";
    };
    return {parseBlock, serializeNode};
}

realTiptapTest("real-tiptap: 规范化结构（列表符号/Setext/强调）仍可对齐验证", async () => {
    const manager = await engineMod.createManager();
    const {parseBlock} = makeRealAdapters(manager);
    const samples = [
        "* 甲\n* 乙\n\n正文",
        "Title\n=====\n\n正文",
        "1. 甲\n2. 乙\n\n> 引用\n\n`code`",
        "# 标题\n\n**粗体** 与 _斜体_ 与 ~~删除~~",
        "- [ ] 任务\n- [x] 完成",
        "```js\nlet a = 1;\n```\n\n段落",
    ];
    for (const src of samples) {
        const map = buildBlockMap(src, manager.parse(src), parseBlock);
        assert.equal(map.ok, true, `${JSON.stringify(src)} 应通过对齐验证`);
    }
});

realTiptapTest("real-tiptap: 危险结构至少被一道防线拒绝（风险门或块对齐验证）", async () => {
    const manager = await engineMod.createManager();
    const {parseBlock} = makeRealAdapters(manager);
    const samples = [
        "| a | b |\n|---|---|\n| 1 | 2 |\n\n正文",
        "<div>html</div>\n\n正文",
        "脚注[^1]\n\n[^1]: 定义\n\n正文",
        "$$\nE = mc^2\n$$\n\n正文",
        "正文 `` 含反引号 `` 结束",
    ];
    for (const src of samples) {
        const gated = !evaluateMarkdownGate(src).allowed;
        const map = buildBlockMap(src, manager.parse(src), parseBlock);
        assert.ok(
            gated || !map.ok,
            `${JSON.stringify(src)} 必须被风险门或块对齐验证拒绝（否则会静默改写）`,
        );
    }

    // 结构性丢失（整块内容消失）必须由块对齐验证兜住——风险门之外的第二道防线
    for (const src of [
        "| a | b |\n|---|---|\n| 1 | 2 |\n\n正文",
        "脚注[^1]\n\n[^1]: 定义\n\n正文",
    ]) {
        assert.equal(
            buildBlockMap(src, manager.parse(src), parseBlock).ok,
            false,
            `${JSON.stringify(src)} 的整块丢失必须被块对齐验证检出`,
        );
    }
});

realTiptapTest("real-tiptap: 局部编辑后未编辑区间逐字节保留", async () => {
    const manager = await engineMod.createManager();
    const {parseBlock, serializeNode} = makeRealAdapters(manager);

    const cases = [
        {src: "* 甲\n* 乙\n\nTitle\n=====\n\n正文段落", target: 2},
        {src: "前言\n\n__强调__ 段落\n\n结语", target: 2},
        {src: "1. 甲\n2. 乙\n\n~~~\ncode\n~~~\n\n尾部", target: 2},
    ];

    for (const {src, target} of cases) {
        const docBefore = manager.parse(src);
        const map = buildBlockMap(src, docBefore, parseBlock);
        assert.equal(map.ok, true, `${JSON.stringify(src)} 应可对齐`);

        const afterNodes = docBefore.content.map((n) => structuredClone(n));
        const node = afterNodes[target];
        const textNode = (node.content ?? []).find((c) => typeof c.text === "string");
        assert.ok(textNode, "目标块应含文本节点");
        textNode.text = `${textNode.text}X`;

        const patches = planSourcePatch({
            sourceText: src,
            blocks: map.blocks,
            beforeNodes: docBefore.content,
            afterNodes,
            serializeNode,
        });
        assert.ok(patches.length > 0, "应产生 patch");
        const {ok, text: patched} = apply(src, patches);
        assert.equal(ok, true);

        // 未编辑区间逐字节保留：编辑块之前的源文必须是原样的前缀
        const editStart = map.blocks[target].contentStart;
        assert.equal(
            patched.slice(0, editStart),
            src.slice(0, editStart),
            "编辑点之前的源文必须逐字节一致",
        );

        // 复核：新源文解析结果必须与编辑后的文档一致
        assert.ok(
            jsonEqual(manager.parse(patched), {type: "doc", content: afterNodes}),
            "patch 后源文必须解析回编辑后的文档结构",
        );
    }
});

realTiptapTest("real-tiptap: 空文档首段输入 → 源文即首段文本（不伪造空段落）", async () => {
    const manager = await engineMod.createManager();
    const {parseBlock, serializeNode} = makeRealAdapters(manager);
    const src = "";
    const map = buildBlockMap(src, manager.parse(src), parseBlock);
    assert.equal(map.ok, true, "空文档必须可编辑（preferred 来源默认 MD）");

    const afterNodes = [{type: "paragraph", content: [{type: "text", text: "第一段"}]}];
    const patches = planSourcePatch({
        sourceText: src,
        blocks: map.blocks,
        beforeNodes: [],
        afterNodes,
        serializeNode,
    });
    const {text} = apply(src, patches);
    assert.equal(text, "第一段");
    assert.ok(jsonEqual(manager.parse(text), {type: "doc", content: afterNodes}));
});

realTiptapTest("real-tiptap: 连续空行与尾部换行在局部编辑后逐字节保留", async () => {
    const manager = await engineMod.createManager();
    const {parseBlock, serializeNode} = makeRealAdapters(manager);
    const src = "前言\n\n\n\n正文\n\n";
    const docBefore = manager.parse(src);
    const map = buildBlockMap(src, docBefore, parseBlock);
    assert.equal(map.ok, true, "含连续空行/尾部换行的文档仍应可编辑（空段落不占源文块）");

    const afterNodes = docBefore.content.map((n) => structuredClone(n));
    let idx = -1;
    for (let i = afterNodes.length - 1; i >= 0; i -= 1) {
        if (!isEmptyParagraphNode(afterNodes[i])) {
            idx = i;
            break;
        }
    }
    assert.ok(idx >= 0, "应存在内容节点");
    afterNodes[idx].content[0].text = `${afterNodes[idx].content[0].text}！`;

    const patches = planSourcePatch({
        sourceText: src,
        blocks: map.blocks,
        beforeNodes: docBefore.content,
        afterNodes,
        serializeNode,
    });
    const {ok, text: patched} = apply(src, patches);
    assert.equal(ok, true);
    assert.ok(patched.startsWith("前言\n\n\n\n"), "连续空行不得被收敛");
    assert.ok(patched.endsWith("\n\n"), "尾部空行不得被吞");
    assert.equal(patched, "前言\n\n\n\n正文！\n\n");
    assert.ok(jsonEqual(manager.parse(patched), {type: "doc", content: afterNodes}));
});

realTiptapTest("real-tiptap: LF 行尾在多行文档中逐字节保留", async () => {
    const manager = await engineMod.createManager();
    const {parseBlock, serializeNode} = makeRealAdapters(manager);
    const src = "第一行\n第二行（软换行同一段）\n\n第二段\n\n- 列表项\n\n结语\n";
    const docBefore = manager.parse(src);
    const map = buildBlockMap(src, docBefore, parseBlock);
    assert.equal(map.ok, true);

    const afterNodes = docBefore.content.map((n) => structuredClone(n));
    const last = afterNodes.at(-1);
    const textNode = (last.content ?? []).find((c) => typeof c.text === "string");
    assert.ok(textNode, "最后一个块应为可编辑文本块");
    textNode.text = "结语改";

    const patches = planSourcePatch({
        sourceText: src,
        blocks: map.blocks,
        beforeNodes: docBefore.content,
        afterNodes,
        serializeNode,
    });
    const {ok, text: patched} = apply(src, patches);
    assert.equal(ok, true);
    // 未编辑区间（前两段 + 列表 + 尾部换行）逐字节保留，行尾统一为 LF
    assert.ok(patched.startsWith("第一行\n第二行（软换行同一段）\n\n第二段\n\n- 列表项\n\n"));
    assert.ok(patched.endsWith("\n"), "尾部换行不得被吞");
    assert.ok(!patched.includes("\r"), "不得引入 CR");
    assert.equal(patched, "第一行\n第二行（软换行同一段）\n\n第二段\n\n- 列表项\n\n结语改\n");
});

realTiptapTest("real-tiptap: 未编辑时反复扫描-对齐是幂等的（切换视图不改变正文）", async () => {
    const manager = await engineMod.createManager();
    const {parseBlock} = makeRealAdapters(manager);
    const src = "前言\n\n* 甲\n* 乙\n\nTitle\n=====\n\n尾部\n\n";
    for (let i = 0; i < 3; i += 1) {
        const map = buildBlockMap(src, manager.parse(src), parseBlock);
        const patches = planSourcePatch({
            sourceText: src,
            blocks: map.blocks,
            beforeNodes: manager.parse(src).content,
            afterNodes: manager.parse(src).content,
            serializeNode: (n) => manager.serialize({type: "doc", content: [n]}),
        });
        assert.deepEqual(patches, [], "未编辑时不得产生 patch");
    }
});
