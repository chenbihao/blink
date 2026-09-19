/**
 * MarkdownIrEngine.replaceRange 事务逻辑回归（真实 Tiptap，0.23.16 修复）。
 *
 * 背景见 docs/reports/20260919-editor-transform-apply-investigation.md：
 * 多行整理稿在段内行内 range 上 `replaceWith` 段落数组会产生幽灵空段落，
 * 且 AI 输出 `\n\n` 生成的真实空段落在 Markdown 源文中不可表达——两者叠加
 * 使延迟物化的整篇复核必败 → 正文被回滚 + 转只读（体感"应用了但段落没变"）。
 *
 * 本文件用生产同源 schema + MarkdownManager 在 headless EditorState 上直接
 * 驱动 `planRangeReplacement`（replaceRange 的纯逻辑核心），并复刻
 * `takeSourcePatch` 的物化复核链路（planSourcePatch + 整篇 parse 比对），
 * 覆盖"应用 → 物化"全链路——此前 MD 引擎单测全是假引擎，正是漏网原因。
 * 依赖缺失（target/editor-markdown-build 未 npm install）时整体显式跳过。
 */

globalThis.window = globalThis;

const {test} = await import("node:test");
const assert = (await import("node:assert/strict")).default;
const {join} = await import("node:path");
const {pathToFileURL} = await import("node:url");
const {MarkdownIrEngine} = await import("./markdown-engine.js");
const {buildBlockMap, jsonEqual, planSourcePatch} = await import("../md-source-patch.js");
const {CanonicalBuffer} = await import("../canonical-buffer.js");

let harness = null;
let harnessErr = null;
try {
    const engineMod = await import("../../../../testdata/editor/markdown/lib/engine.mjs");
    engineMod.ensureDeps();
    const {DEPS_DIR} = engineMod;
    const importFromDeps = (...segments) =>
        import(pathToFileURL(join(DEPS_DIR, "node_modules", ...segments)).href);
    const [manager, schema, pmModel, pmState] = await Promise.all([
        engineMod.createManager(),
        engineMod.createSchema(),
        importFromDeps("@tiptap", "pm", "dist", "model", "index.js"),
        importFromDeps("@tiptap", "pm", "dist", "state", "index.js"),
    ]);
    harness = {manager, schema, Node: pmModel.Node, EditorState: pmState.EditorState};
} catch (e) {
    harnessErr = e;
}

const realTiptapTest = harnessErr ? test.skip : test;

if (harnessErr) {
    console.log(`[markdown-engine] 跳过真实 Tiptap 用例：${harnessErr.message.split("\n")[0]}`);
}

if (harness) {
    const {manager, schema, Node, EditorState} = harness;

    const parseBlock = (blockText) => {
        const json = manager.parse(blockText);
        const content = json?.content;
        if (!Array.isArray(content) || content.length !== 1) return null;
        return content[0];
    };
    // 与生产 MarkdownIrEngine._serializeNodeText 相同口径
    const serializeNode = (node) => {
        const raw = manager.serialize({type: "doc", content: [node]});
        return typeof raw === "string" ? raw.replace(/^\n+/, "").replace(/\s+$/, "") : "";
    };

    const makeState = (md) => {
        const doc = Node.fromJSON(schema, manager.parse(md));
        return EditorState.create({schema, doc});
    };

    /** 顶层节点 JSON 快照（= 生产 _topSnapshot 的产物） */
    const topJson = (doc) => {
        const out = [];
        for (let i = 0; i < doc.childCount; i += 1) out.push(doc.child(i).toJSON());
        return out;
    };

    const paraTexts = (doc) =>
        topJson(doc).map((n) => n.content?.[0]?.text ?? "");

    /** 第 index 个顶层块的行内文本范围（blockSafe） */
    const childRange = (doc, index) => {
        let pos = 0;
        for (let i = 0; i < index; i += 1) pos += doc.child(i).nodeSize;
        const node = doc.child(index);
        return {from: pos + 1, to: pos + 1 + node.content.size};
    };

    /** 与 replaceRange 相同的事务执行（headless） */
    const applyReplacement = (state, from, to, newText) => {
        const plan = MarkdownIrEngine.planRangeReplacement(state, from, to, newText);
        const tr = state.tr;
        if (plan.kind === "inline") {
            tr.insertText(plan.text, from, to);
        } else {
            tr.replaceWith(plan.blockFrom, plan.blockTo, plan.nodes);
        }
        return tr.doc;
    };

    /** 复刻 MarkdownIrEngine.takeSourcePatch 的常规物化路径与整篇复核 */
    const materialize = (baseSource, docAfter) => {
        const beforeNodes = topJson(Node.fromJSON(schema, manager.parse(baseSource)));
        const map = buildBlockMap(baseSource, {type: "doc", content: beforeNodes}, parseBlock);
        assert.equal(map.ok, true, "初始源文必须可对齐");
        const afterNodes = topJson(docAfter);
        const patches = planSourcePatch({
            sourceText: baseSource,
            blocks: map.blocks,
            beforeNodes,
            afterNodes,
            serializeNode,
        });
        const buffer = new CanonicalBuffer(baseSource);
        const applied = patches.length > 0 ? buffer.applyPatches(patches) : false;
        const candidate = applied ? buffer.text : baseSource;
        let exact = false;
        try {
            exact = jsonEqual(manager.parse(candidate), {type: "doc", content: afterNodes});
        } catch {
            exact = false;
        }
        return {exact, candidate};
    };

    realTiptapTest("多行整理稿：块级替换无幽灵/空段落，物化 exact（S1 回归）", () => {
        const src = "今天讨论了三件事，首先是发布节奏，其次是权限回收，最后是复盘安排。";
        const state = makeState(src);
        assert.equal(state.doc.childCount, 1);
        const {from, to} = childRange(state.doc, 0);
        assert.equal(MarkdownIrEngine._blockSafe(state, from, to), true);

        const docAfter = applyReplacement(state, from, to, "会议纪要。\n\n决议：下周发布。");
        assert.deepEqual(topJson(docAfter), [
            {type: "paragraph", content: [{type: "text", text: "会议纪要。"}]},
            {type: "paragraph", content: [{type: "text", text: "决议：下周发布。"}]},
        ], "\\n\\n 只分隔相邻段落，不产生空段落节点");

        const m = materialize(src, docAfter);
        assert.equal(m.exact, true, "整篇复核必须通过（否则回滚+只读）");
        assert.equal(m.candidate, "会议纪要。\n\n决议：下周发布。");
    });

    realTiptapTest("整理稿首尾/连续空行不生成节点，物化仍 exact", () => {
        const src = "原始听写内容";
        const state = makeState(src);
        const {from, to} = childRange(state.doc, 0);

        const docAfter = applyReplacement(state, from, to, "\nA\n\n\nB\n\n");
        assert.deepEqual(paraTexts(docAfter), ["A", "B"], "空行全部跳过");
        const m = materialize(src, docAfter);
        assert.equal(m.exact, true);
        assert.equal(m.candidate, "A\n\nB");
    });

    realTiptapTest("段内子范围多行替换：范围外前后缀回填首末段（选区整理）", () => {
        const src = "hello world foo";
        const state = makeState(src);
        const from = 1 + "hello ".length;
        const to = from + "world".length;
        assert.equal(MarkdownIrEngine._blockSafe(state, from, to), true);

        const docAfter = applyReplacement(state, from, to, "W1\nW2");
        assert.deepEqual(paraTexts(docAfter), ["hello W1", "W2 foo"],
            "只替换冻结文本，同段前后文字保留");
        const m = materialize(src, docAfter);
        assert.equal(m.exact, true);
        assert.equal(m.candidate, "hello W1\n\nW2 foo");
    });

    realTiptapTest("多行替换只影响目标块：heading 等未编辑区间逐字节保留（S4 回归）", () => {
        const src = "# 标题\n\n听写正文段落";
        const state = makeState(src);
        const {from, to} = childRange(state.doc, 1);
        assert.equal(MarkdownIrEngine._blockSafe(state, from, to), true);

        const docAfter = applyReplacement(state, from, to, "第一行整理稿。\n\n第二行整理稿。");
        const m = materialize(src, docAfter);
        assert.equal(m.exact, true);
        assert.equal(m.candidate, "# 标题\n\n第一行整理稿。\n\n第二行整理稿。",
            "未编辑的 heading 块逐字节保留");
    });

    realTiptapTest("单行/全空行整理稿：走行内替换路径，不动块结构", () => {
        const state = makeState("a\n\nb");
        const single = MarkdownIrEngine.planRangeReplacement(state, 1, 2, "整理稿");
        assert.equal(single.kind, "inline");
        assert.equal(single.text, "整理稿");

        // AI 全空行输出 = 删除范围文本（不插入空段落——空段落在源文不可表达）
        const blank = MarkdownIrEngine.planRangeReplacement(state, 1, 2, "\n\n");
        assert.equal(blank.kind, "inline");
        assert.equal(blank.text, "");
    });

    realTiptapTest("跨段范围 blockSafe=false：块边界安全判定的真实 resolve 口径", () => {
        const state = makeState("甲\n\n乙");
        // from 在 para 甲文末、to 落在 para 乙内部：跨块
        assert.equal(MarkdownIrEngine._blockSafe(state, 2, 4), false);
        assert.equal(MarkdownIrEngine._blockSafe(state, 1, 2), true, "单段内为 true");
    });
}
