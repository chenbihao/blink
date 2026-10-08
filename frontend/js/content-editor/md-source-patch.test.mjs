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

test("jsonEqual: undefined 值 key 视为不存在（parse 脏 key vs toJSON 口径，0.25.18）", () => {
    // @tiptap/markdown 解析 codeBlock 内 text node 产出 `marks: undefined`（脏 key），
    // ProseMirror `toJSON()` 产出不含该 key。两者必须判等，否则任何含代码块的
    // 文档都会在对齐验证中误判"块不匹配"而锁定只读。
    assert.ok(jsonEqual(
        {type: "text", text: "code", marks: undefined},
        {type: "text", text: "code"},
    ), "undefined 值 key 不得参与键数比较");
    assert.ok(!jsonEqual(
        {type: "text", text: "code", marks: []},
        {type: "text", text: "code"},
    ), "undefined 之外的值（含空数组）照常参与比较");
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

// ── 空段落参与变更的编辑形态（0.25.18 回归：此前 6 分支拼 gap 丢/多空段落）──

/** 空段落形态专用假体：paragraph 节点，文本取 content[0].text */
const shapeDoc = (nodes) => ({type: "doc", content: nodes});
const shapePara = (t) => (t === null ? {type: "paragraph", content: []} : {
    type: "paragraph",
    content: [{type: "text", text: t}],
});
const shapeParse = (t) => shapePara(t);
const shapeSerialize = (n) => n.content?.[0]?.text ?? "";

/**
 * 形态回归骨架：src 载入对齐 → before/after 节点序列 → patch → 断言
 * 「apply 后的源文重新对齐出 after 的块结构」与「未编辑前缀逐字节保留」。
 * 空段落在真实 Tiptap 下的映射（parse 探测锚定）：块间 E 空段 ↔ 2(E+1) 换行、
 * 文档首/尾 E 空段 ↔ 2E 换行——假体直接按该映射构造对齐两侧。
 */
function shapeCase(label, src, before, after, {expect} = {}) {
    const map = buildBlockMap(src, shapeDoc(before), shapeParse);
    assert.equal(map.ok, true, `${label}：源文与 before 节点必须可对齐`);
    const patches = planSourcePatch({
        sourceText: src,
        blocks: map.blocks,
        beforeNodes: before,
        afterNodes: after,
        serializeNode: shapeSerialize,
    });
    const {ok, text} = apply(src, patches);
    assert.equal(ok, true, `${label}：patch 必须可应用`);
    if (expect !== undefined) {
        assert.deepEqual(text, expect, `${label}：候选源文`);
    }
    return text;
}

test("shape: 尾部多空行打字（倒数第 2 空行）不丢尾部空段落", () => {
    // 用户实测形态：文末多个空行处打字其一。旧实现 postEmpties 计数循环
    // 以 `seenAfter < blockCount - bi1` 为界，窗口在文末时恒为 0 次迭代，
    // 尾部空段落被静默吞掉 → 整篇复核必败误锁只读。
    shapeCase(
        "尾部2空行打字第1个",
        "A\n\n\n\n",
        [shapePara("A"), shapePara(null), shapePara(null)],
        [shapePara("A"), shapePara("x"), shapePara(null)],
        {expect: "A\n\nx\n\n"},
    );
});

test("shape: 块间空行打字（A-e-B 后打字尾空行）不注入幽灵空段落", () => {
    // 旧实现 preEmpties 回扫以 `seen < bi0` 为界，跨过了紧邻内容块 B 把
    // A/B 之间的空段落也计入 → 候选源文凭空多出空段落 → 复核必败。
    shapeCase(
        "A-e-B-e 打字末空行",
        "A\n\n\n\nB\n\n",
        [shapePara("A"), shapePara(null), shapePara("B"), shapePara(null)],
        [shapePara("A"), shapePara(null), shapePara("B"), shapePara("x")],
        {expect: "A\n\n\n\nB\n\nx"},
    );
});

test("shape: 删除段落文本留下空段落（唯一段/末段/中段/首段）不丢空段落", () => {
    // 旧实现整体删除分支「吸掉左侧 gap」把存活空段落一并吸掉。
    shapeCase(
        "删唯一段文本",
        "B",
        [shapePara("B")],
        [shapePara(null)],
        {expect: "\n\n"},
    );
    shapeCase(
        "删末段文本",
        "A\n\nB",
        [shapePara("A"), shapePara("B")],
        [shapePara("A"), shapePara(null)],
        {expect: "A\n\n"},
    );
    shapeCase(
        "删中段文本",
        "A\n\nB\n\nC",
        [shapePara("A"), shapePara("B"), shapePara("C")],
        [shapePara("A"), shapePara(null), shapePara("C")],
        {expect: "A\n\n\n\nC"},
    );
    shapeCase(
        "删首段文本",
        "A\n\nB",
        [shapePara("A"), shapePara("B")],
        [shapePara(null), shapePara("B")],
        {expect: "\n\nB"},
    );
});

test("shape: 全空行文档中部空行打字保留前导空段落", () => {
    shapeCase(
        "全空行打字",
        "\n\n\n\n",
        [shapePara(null), shapePara(null)],
        [shapePara(null), shapePara("x")],
        {expect: "\n\nx"},
    );
});

test("shape: 内容块删除后跟空段落（节点级删除留空段）", () => {
    shapeCase(
        "删末节点留空段",
        "A\n\nB",
        [shapePara("A"), shapePara("B")],
        [shapePara("A"), shapePara(null)],
        {expect: "A\n\n"},
    );
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
        // 与生产 _serializeNodeText 同口径：不做空白剃除（尾随空格是用户
        // 内容，0.25.18），首尾换行剥除由 nodeText 统一负责
        return typeof raw === "string" ? raw : "";
    };
    return {parseBlock, serializeNode};
}

realTiptapTest("real-tiptap: 生产快照口径（doc.toJSON vs 单块 parse）下含代码块文档对齐通过", async () => {
    // 生产 MarkdownIrEngine 的对齐比较是「单块源文 manager.parse 产物」对
    // 「ProseMirror 文档节点 toJSON() 快照」。此前测试两侧都用 manager.parse
    // 口径，掩盖了 codeBlock 内 text node 的 `marks: undefined` 脏 key 差异
    // （任何含代码块文档载入即锁只读，0.25.18）。本用例复刻生产口径锁回归。
    const {join} = await import("node:path");
    const {pathToFileURL} = await import("node:url");
    const manager = await engineMod.createManager();
    const schema = await engineMod.createSchema();
    const {Node} = await import(pathToFileURL(
        join(engineMod.DEPS_DIR, "node_modules", "@tiptap", "pm", "dist", "model", "index.js"),
    ).href);
    const {parseBlock} = makeRealAdapters(manager);

    // 与生产 _topSnapshot 相同口径：文档顶层节点 toJSON 后再比较
    const topSnapshot = (md) => {
        const doc = Node.fromJSON(schema, manager.parse(md));
        const out = [];
        for (let i = 0; i < doc.childCount; i += 1) out.push(doc.child(i).toJSON());
        return out;
    };

    const samples = [
        "```js\nconst a = 1;\n```\n",
        "# 标题\n\n说明文字。\n\n```js\nconst a = 1;\n```\n\n结尾段落。\n",
        "- item\n\n```py\nprint(1)\n```\n\n```js\nx();\n```\n\n尾段。",
        "```js\nconst a = 1;\n\nconst b = 2;\n```\n",
        "说明：\n\n```html\n<div>样例</div>\n```\n\n完。",
    ];
    for (const src of samples) {
        const docJson = {type: "doc", content: topSnapshot(src)};
        const map = buildBlockMap(src, docJson, parseBlock);
        assert.equal(map.ok, true, `含代码块文档在生产快照口径下必须可对齐: ${JSON.stringify(src)}`);
    }
});

realTiptapTest("real-tiptap: 生产口径下含尾部空行的文档编辑后复核通过（toJSON 空段落无 content key，0.25.18）", async () => {
    // 复刻用户实测：单代码块 + 尾部空行（文档含空段落）。toJSON 对空段落
    // 省略 content，parse 产物显式 content: []——整篇复核两侧口径不一致曾使
    // 任何编辑都复核失败并误转只读。normalizeNodeJson 统一口径后必须通过。
    const {normalizeNodeJson} = await import("./md-source-patch.js");
    const {join} = await import("node:path");
    const {pathToFileURL} = await import("node:url");
    const manager = await engineMod.createManager();
    const schema = await engineMod.createSchema();
    const {Node} = await import(pathToFileURL(
        join(engineMod.DEPS_DIR, "node_modules", "@tiptap", "pm", "dist", "model", "index.js"),
    ).href);
    const {EditorState} = await import(pathToFileURL(
        join(engineMod.DEPS_DIR, "node_modules", "@tiptap", "pm", "dist", "state", "index.js"),
    ).href);
    const {parseBlock, serializeNode} = makeRealAdapters(manager);
    const {CanonicalBuffer} = await import("./canonical-buffer.js");

    const topSnapshot = (mdOrState) => {
        const doc = typeof mdOrState === "string"
            ? Node.fromJSON(schema, manager.parse(mdOrState))
            : mdOrState.doc;
        const out = [];
        for (let i = 0; i < doc.childCount; i += 1) out.push(normalizeNodeJson(doc.child(i).toJSON()));
        return out;
    };

    const samples = [
        "```python\ndef f():\n    return 1\n```\n\n",
        "前言\n\n```\ncode\n```\n\n\n尾段。\n\n",
        "```js\nconst a = 1;\n```\n",
    ];
    for (const src of samples) {
        const state = EditorState.create({schema, doc: Node.fromJSON(schema, manager.parse(src))});
        const before = topSnapshot(src);
        const map = buildBlockMap(src, {type: "doc", content: before}, parseBlock);
        assert.equal(map.ok, true, `含空段落文档应可对齐: ${JSON.stringify(src)}`);

        // 在文档末尾追加一个字（模拟用户打字触发物化）
        const afterState = state.apply(state.tr.insertText("X", state.doc.content.size - 1));
        const after = topSnapshot(afterState);
        const patches = planSourcePatch({
            sourceText: src, blocks: map.blocks, beforeNodes: before, afterNodes: after, serializeNode,
        });
        const buf = new CanonicalBuffer(src);
        const applied = patches.length > 0 ? buf.applyPatches(patches) : false;
        const candidate = applied ? buf.text : src;
        const exact = jsonEqual(
            normalizeNodeJson(manager.parse(candidate)),
            {type: "doc", content: after},
        );
        assert.equal(exact, true, `编辑后整篇复核必须通过: ${JSON.stringify(src)}`);
    }
});

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

// ── 0.25.18 回归：空段落参与变更的编辑形态必须整篇复核通过 ─────────────────
//
// 复刻用户实测（0.25.18 后仍复现）：MD 模式在文末空行处打字——Enter 产出
// 空段落（纯空段插入，复核通过），随后打字把空段落变为内容段落，旧实现的
// 特例分支丢失/多算相邻空段落 → parse(candidate) ≠ 当前文档 → 回滚误锁只读。
// 以下用真实 parse/serialize 全链路验证：patch 候选源文必须解析回编辑后文档。

realTiptapTest("real-tiptap: 文末空行打字（多轮 Enter+打字序列）复核通过", async () => {
    const {normalizeNodeJson} = await import("./md-source-patch.js");
    const manager = await engineMod.createManager();
    const {parseBlock, serializeNode} = makeRealAdapters(manager);
    const {CanonicalBuffer} = await import("./canonical-buffer.js");

    // (源文, 编辑后节点构造器) 对：after 直接复刻 parse(src) 的节点序列再施加
    // 目标变换，保证两侧都是真实 Tiptap 口径
    const cases = [
        // 尾部 2 空行，打字第 1 个（存活 1 个尾部空段）
        ["A\n\n\n\n", (nodes) => {nodes[1] = {type: "paragraph", content: [{type: "text", text: "x"}]}; return nodes;}],
        // 尾部 2 空行，打字最后 1 个
        ["A\n\n\n\n", (nodes) => {nodes[2] = {type: "paragraph", content: [{type: "text", text: "x"}]}; return nodes;}],
        // 块间空行 + 尾空行，打字尾空行（preEmpties 越界计数回归）
        ["A\n\n\n\nB\n\n", (nodes) => {nodes[3] = {type: "paragraph", content: [{type: "text", text: "x"}]}; return nodes;}],
        // 前导空行打字
        ["\n\nA", (nodes) => {nodes[0] = {type: "paragraph", content: [{type: "text", text: "x"}]}; return nodes;}],
        // 全空行文档打字其一
        ["\n\n\n\n", (nodes) => {nodes[1] = {type: "paragraph", content: [{type: "text", text: "x"}]}; return nodes;}],
        // 代码块 + 尾空行打字（用户 0.25.18 实测文档形态）
        ["```\ncode\n```\n\n\n\n", (nodes) => {nodes[1] = {type: "paragraph", content: [{type: "text", text: "x"}]}; return nodes;}],
    ];
    for (const [src, mutate] of cases) {
        const before = manager.parse(src).content.map((n) => normalizeNodeJson(n));
        const map = buildBlockMap(src, {type: "doc", content: before}, parseBlock);
        assert.equal(map.ok, true, `${JSON.stringify(src)} 载入对齐`);
        const after = mutate(before.map((n) => structuredClone(n))).map((n) => normalizeNodeJson(n));
        const patches = planSourcePatch({
            sourceText: src, blocks: map.blocks, beforeNodes: before, afterNodes: after, serializeNode,
        });
        const buf = new CanonicalBuffer(src);
        const applied = patches.length > 0 ? buf.applyPatches(patches) : false;
        const candidate = applied ? buf.text : src;
        assert.equal(
            jsonEqual(normalizeNodeJson(manager.parse(candidate)), {type: "doc", content: after}),
            true,
            `${JSON.stringify(src)} 打字后整篇复核必须通过，candidate=${JSON.stringify(candidate)}`,
        );
    }
});

realTiptapTest("real-tiptap: 段落尾随空格是用户内容，tier1 无损物化（0.25.18）", async () => {
    // 用户实测形态（blocks=0 nodes=22 误锁只读的构成之一）：序列化链路
    // 此前对单节点序列化结果做 `\s+$` 剃除，段落尾随空格被静默丢弃 →
    // parse-back ≠ 当前文档 → 复核必败。@tiptap/markdown 的 parser 逐字
    // 保留尾随空格，序列化口径不剃空白后 tier1 必须直接无损通过。
    const {normalizeNodeJson} = await import("./md-source-patch.js");
    const manager = await engineMod.createManager();
    const {parseBlock, serializeNode} = makeRealAdapters(manager);
    const {CanonicalBuffer} = await import("./canonical-buffer.js");

    const cases = [
        // 空文档打出含尾随空格段的多块正文（用户 22 节点场景的最小化）
        ["", [
            {type: "paragraph", content: [{type: "text", text: "第一段"}]},
            {type: "paragraph", content: [{type: "text", text: "abc "}]},
            {type: "paragraph", content: [{type: "text", text: "第三段"}]},
        ]],
        // 已有内容文档编辑出尾随空格
        ["前言\n\n正文", [
            {type: "paragraph", content: [{type: "text", text: "前言"}]},
            {type: "paragraph", content: [{type: "text", text: "正文 "}]},
        ]],
    ];
    for (const [src, afterRaw] of cases) {
        const before = src === ""
            ? []
            : manager.parse(src).content.map((n) => normalizeNodeJson(n));
        const map = buildBlockMap(src, {type: "doc", content: before}, parseBlock);
        assert.equal(map.ok, true, `${JSON.stringify(src)} 载入对齐`);
        const after = afterRaw.map((n) => normalizeNodeJson(structuredClone(n)));
        const patches = planSourcePatch({
            sourceText: src, blocks: map.blocks, beforeNodes: before, afterNodes: after, serializeNode,
        });
        const buf = new CanonicalBuffer(src);
        const applied = patches.length > 0 ? buf.applyPatches(patches) : false;
        const candidate = applied ? buf.text : src;
        assert.equal(
            jsonEqual(normalizeNodeJson(manager.parse(candidate)), {type: "doc", content: after}),
            true,
            `${JSON.stringify(src)} 含尾随空格必须 tier1 无损物化，candidate=${JSON.stringify(candidate)}`,
        );
        assert.ok(candidate.includes("abc ") || candidate.includes("正文 "),
            "尾随空格必须保留在候选源文中");
    }
});

realTiptapTest("real-tiptap: 删除段落文本留下空段落（唯一/末/中/首段）复核通过", async () => {
    const {normalizeNodeJson} = await import("./md-source-patch.js");
    const manager = await engineMod.createManager();
    const {parseBlock, serializeNode} = makeRealAdapters(manager);
    const {CanonicalBuffer} = await import("./canonical-buffer.js");

    const emptyOut = (idx) => (nodes) => {
        nodes[idx] = {type: "paragraph", content: []};
        return nodes;
    };
    const cases = [
        ["B", emptyOut(0)],
        ["A\n\nB", emptyOut(1)],
        ["A\n\nB\n\nC", emptyOut(1)],
        ["A\n\nB", emptyOut(0)],
        ["A\n\n\n\nB", emptyOut(2)],
    ];
    for (const [src, mutate] of cases) {
        const before = manager.parse(src).content.map((n) => normalizeNodeJson(n));
        const map = buildBlockMap(src, {type: "doc", content: before}, parseBlock);
        assert.equal(map.ok, true, `${JSON.stringify(src)} 载入对齐`);
        const after = mutate(before.map((n) => structuredClone(n))).map((n) => normalizeNodeJson(n));
        const patches = planSourcePatch({
            sourceText: src, blocks: map.blocks, beforeNodes: before, afterNodes: after, serializeNode,
        });
        const buf = new CanonicalBuffer(src);
        const applied = patches.length > 0 ? buf.applyPatches(patches) : false;
        const candidate = applied ? buf.text : src;
        assert.equal(
            jsonEqual(normalizeNodeJson(manager.parse(candidate)), {type: "doc", content: after}),
            true,
            `${JSON.stringify(src)} 删文本留空段后整篇复核必须通过，candidate=${JSON.stringify(candidate)}`,
        );
    }
});

realTiptapTest("real-tiptap: 随机形态压力——规划器候选必须不劣于整篇序列化的表达力", async () => {
    // 随机文档（多种块型 + 随机空行 gap）× 随机编辑（空段落转变/删文本/
    // 增删块/改文本）。断言：凡「整篇序列化可 round-trip」的编辑后文档，
    // 规划器候选源文必须复核通过——即规划器不得比序列化器更弱。
    // 已知豁免（真不可表达，安全网正确兜底）：纯空白文本段落（nbsp/空格
    // 段序列化为空串、parse 又把 nbsp 转普通空格）、同标记列表间的空段落
    // （CommonMark lazy continuation 语义），这两类整篇序列化自身也不
    // round-trip，不计入断言。
    const {normalizeNodeJson} = await import("./md-source-patch.js");
    const manager = await engineMod.createManager();
    const {parseBlock, serializeNode} = makeRealAdapters(manager);
    const {CanonicalBuffer} = await import("./canonical-buffer.js");

    let seed = 20261009;
    const rnd = () => (seed = (seed * 1103515245 + 12345) & 0x7fffffff) / 0x7fffffff;
    const pick = (arr) => arr[Math.floor(rnd() * arr.length)];
    const ri = (lo, hi) => lo + Math.floor(rnd() * (hi - lo + 1));

    const blockTexts = [
        () => `段落${ri(1, 99)}文字`,
        () => `# 标题${ri(1, 9)}`,
        () => "```rust\nfn main() {\n    let x = 1;\n}\n```",
        () => "```\nplain code\n```",
        () => `- 列表项${ri(1, 9)}\n- 另一项`,
        () => "> 引用内容",
        () => "---",
    ];
    const randomSource = () => {
        const parts = [];
        const lead = pick([0, 2, 4]);
        if (lead) parts.push("\n".repeat(lead));
        const nBlocks = ri(1, 4);
        for (let i = 0; i < nBlocks; i += 1) {
            parts.push(pick(blockTexts)());
            if (i < nBlocks - 1) parts.push("\n".repeat(pick([2, 2, 4, 6])));
        }
        const tail = pick([0, 1, 2, 4]);
        if (tail) parts.push("\n".repeat(tail));
        return parts.join("");
    };

    let checked = 0;
    for (let i = 0; i < 400; i += 1) {
        const src = randomSource();
        const before = manager.parse(src).content.map((n) => normalizeNodeJson(n));
        const map = buildBlockMap(src, {type: "doc", content: before}, parseBlock);
        if (!map.ok) continue;

        let after = before.map((n) => structuredClone(n));
        const idx = ri(0, after.length - 1);
        const edit = ri(0, 4);
        if (edit === 0) { // 空段落 → 内容（或内容段追加）
            if (!after[idx].content) after[idx].content = [];
            if (after[idx].content.length === 0) after[idx].content.push({type: "text", text: "新"});
            else if (after[idx].content[0]?.text != null) after[idx].content[0].text += "字";
            else continue;
        } else if (edit === 1 && after[idx].type === "paragraph") { // 内容 → 空段落
            after[idx] = {type: "paragraph", content: []};
        } else if (edit === 2) { // 删节点
            after.splice(idx, 1);
            if (after.length === 0) continue;
        } else if (edit === 3) { // 插入内容段
            after.splice(idx, 0, {type: "paragraph", content: [{type: "text", text: "插入"}]});
        } else { // 改文本
            const s = JSON.stringify(after);
            const hit = s.match(/"text":"([^"]+)"/);
            if (!hit) continue;
            after = JSON.parse(s.replace(`"${hit[1]}"`, `"${hit[1]}改"`));
        }

        // 整篇序列化可 round-trip 才是规划器必须达成的表达力基准
        let serRound = false;
        try {
            serRound = jsonEqual(
                normalizeNodeJson(manager.parse(manager.serialize({type: "doc", content: after}))),
                {type: "doc", content: after},
            );
        } catch {serRound = false;}
        if (!serRound) continue;
        checked += 1;

        const patches = planSourcePatch({
            sourceText: src, blocks: map.blocks, beforeNodes: before, afterNodes: after, serializeNode,
        });
        const buf = new CanonicalBuffer(src);
        const applied = patches.length > 0 ? buf.applyPatches(patches) : false;
        const candidate = applied ? buf.text : src;
        assert.equal(
            jsonEqual(normalizeNodeJson(manager.parse(candidate)), {type: "doc", content: after}),
            true,
            `随机形态 ${i}（edit=${edit}）规划器候选必须复核通过`
            + `：src=${JSON.stringify(src)} candidate=${JSON.stringify(candidate)}`,
        );
    }
    assert.ok(checked > 200, `随机形态覆盖不足：仅 ${checked} 例达到序列化基准`);
});
