/**
 * EditorAdapter 检查点与视图决策测试（0.23.1，注入 fake 引擎，无 DOM）。
 *
 * 覆盖验收（phase 文档 §6.2）：
 * - Source/MD 共用 Adapter；切换视图不改变正文语义；
 * - 未编辑 MD 时正文逐字符等于原文（检查点机制）；
 * - 危险结构（风险门拒绝）不能进入 MD 视图；
 * - reset 后正文/undo/监听（dispose）为空。
 */

import {test} from "node:test";
import assert from "node:assert/strict";
import {EditorAdapter} from "./editor-adapter.js";

/** fake 引擎：记录构造参数与生命周期，可控行为 */
function makeFakeEngines() {
    const created = [];
    class FakeEngine {
        static nextId = 1;
        constructor(opts) {
            this.id = FakeEngine.nextId++;
            this.kind = this.constructor.kindName;
            // SourceEngine 用 initialText，MarkdownIrEngine 用 initialMarkdown
            this.initialText = opts.initialText ?? opts.initialMarkdown;
            this._text = this.initialText;
            this.edited = false;
            this.normalized = false;
            this.disposed = false;
            this._onChange = opts.onChange;
            created.push(this);
        }
        getText() {
            return this._text;
        }
        getSelectionText() {
            return "";
        }
        focus() {}
        /** 模拟序列化重写（如 * → -）：写入带规范化痕迹的文本 */
        setText(text, {edited = true, normalized = false} = {}) {
            this._text = text;
            this.edited = edited;
            this.normalized = normalized;
            if (edited) this._onChange?.();
        }
        replaceAll(text) {
            this.setText(text, {edited: true});
        }
        loadContent(text) {
            this._text = text;
            this.edited = false;
        }
        dispose() {
            this.disposed = true;
        }
    }
    class FakeSource extends FakeEngine {
        static kindName = "source";
    }
    class FakeMarkdown extends FakeEngine {
        static kindName = "markdown";
    }
    return {
        created,
        factory: {
            source: (opts) => new FakeSource(opts),
            markdown: (opts) => new FakeMarkdown(opts),
        },
        classes: {FakeSource, FakeMarkdown},
    };
}

function makeAdapter(factory) {
    const notices = [];
    const viewChanges = [];
    const adapter = new EditorAdapter(
        {sourceEl: {}, mdContainerEl: {}, mdToolbarEl: null},
        {
            onNotice: (k) => notices.push(k),
            onViewChanged: (v) => viewChanges.push(v),
        },
        factory,
    );
    return {adapter, notices, viewChanges};
}

test("adapter: preferred 来源 + 门通过 → 默认 MD；未编辑时正文逐字符等于原文", () => {
    const {factory} = makeFakeEngines();
    const {adapter} = makeAdapter(factory);
    const original = "# 标题\n\n正文 *内容*（原始星号列表标记）";

    adapter.loadInitial({body: original, markdownPolicy: "preferred"});
    assert.equal(adapter.view, "markdown");

    // 未编辑：getText 返回检查点原文（Tiptap 规范化不回写正文）
    assert.equal(adapter.getText(), original);
    assert.equal(adapter.isDirty(), false);
});

test("adapter: available 来源默认 Source；手动切 MD 再切回还原原文", () => {
    const {factory, created} = makeFakeEngines();
    const {adapter} = makeAdapter(factory);
    const original = "正文";

    adapter.loadInitial({body: original, markdownPolicy: "available"});
    assert.equal(adapter.view, "source");

    assert.equal(adapter.switchView("markdown"), true);
    assert.equal(adapter.view, "markdown");
    // 切视图正文衔接：MD 引擎收到的初始文本是切出时的正文
    const mdEngine = created.at(-1);
    assert.equal(mdEngine.initialText, original);

    // 未编辑 → 切回 Source 必须逐字符还原
    assert.equal(adapter.switchView("source"), true);
    const srcEngine = created.at(-1);
    assert.equal(srcEngine.initialText, original);
    assert.equal(adapter.isDirty(), false);
});

test("adapter: MD 已编辑 → 切回 Source 使用序列化文本，dirty 正确", () => {
    const {factory, created} = makeFakeEngines();
    const {adapter} = makeAdapter(factory);
    const original = "原始";
    adapter.loadInitial({body: original, markdownPolicy: "preferred"});
    assert.equal(adapter.view, "markdown");

    const mdEngine = created.at(-1);
    mdEngine.setText("重写后的文本\n", {normalized: true});

    assert.equal(adapter.isDirty(), true);
    assert.equal(adapter.getText(), "重写后的文本\n");

    adapter.switchView("source");
    const srcEngine = created.at(-1);
    assert.equal(srcEngine.initialText, "重写后的文本\n");
    assert.equal(adapter.isNormalizedMd(), false);
});

test("adapter: 风险门拒绝的结构不能进入 MD 视图", () => {
    const {factory} = makeFakeEngines();
    const {adapter, notices} = makeAdapter(factory);
    const withTable = "| a | b |\n|---|---|\n| 1 | 2 |\n\n表格后的段落";

    adapter.loadInitial({body: withTable, markdownPolicy: "preferred"});
    // preferred 但门拒绝 → 默认 Source + 提示
    assert.equal(adapter.view, "source");
    assert.ok(notices.includes("editor.gate.rejected"));

    // 手动切换同样被拦截
    assert.equal(adapter.switchView("markdown"), false);
    assert.equal(adapter.view, "source");
});

test("adapter: 门结果按当前正文重估——Source 中新编辑出表格后切 MD 被拦", () => {
    const {factory, created} = makeFakeEngines();
    const {adapter, notices} = makeAdapter(factory);
    adapter.loadInitial({body: "干净正文", markdownPolicy: "available"});
    assert.equal(adapter.view, "source");
    assert.equal(adapter.switchView("markdown"), true); // 干净内容可进 MD

    // 切回 Source，编辑出 GFM 表格
    adapter.switchView("source");
    created.at(-1).setText("| a | b |\n|---|---|\n| 1 | 2 |");

    // 载入时的 gate 已过期：切换必须按当前正文重新评估并拒绝
    assert.equal(adapter.switchView("markdown"), false);
    assert.equal(adapter.view, "source");
    assert.ok(notices.includes("editor.gate.rejected"));
});

test("adapter: 提交成功后检查点前移", () => {
    const {factory, created} = makeFakeEngines();
    const {adapter} = makeAdapter(factory);
    adapter.loadInitial({body: "v1", markdownPolicy: "available"});

    const src = created.at(-1);
    src.setText("v2"); // 用户编辑
    assert.equal(adapter.isDirty(), true);

    adapter.setCheckpoint("v2"); // commit 成功后由 EditorSession 调用
    assert.equal(adapter.isDirty(), false);
});

test("adapter: reset 后引擎销毁、检查点与 revision 清空", () => {
    const {factory, created} = makeFakeEngines();
    const {adapter} = makeAdapter(factory);
    adapter.loadInitial({body: "内容", markdownPolicy: "available"});
    created.at(-1).setText("改动");
    assert.ok(adapter.revision > 0);

    adapter.reset();
    assert.equal(adapter.view, null);
    assert.equal(adapter.checkpoint, "");
    assert.equal(adapter.revision, 0);
    assert.ok(created.every((e) => e.disposed), "所有引擎都应被 dispose（监听为空）");
});

test("adapter: revision 单调递增", () => {
    const {factory, created} = makeFakeEngines();
    const {adapter} = makeAdapter(factory);
    adapter.loadInitial({body: "a", markdownPolicy: "available"});
    const before = adapter.revision;
    created.at(-1).setText("b");
    created.at(-1).setText("c");
    assert.ok(adapter.revision >= before + 2);
});
