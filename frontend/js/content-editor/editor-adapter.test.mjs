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

test("adapter: 2M Source envelope 可原样载入并完成 reset", () => {
    const {factory, created} = makeFakeEngines();
    const {adapter} = makeAdapter(factory);
    const body = "中".repeat(2_000_000);

    adapter.loadInitial({body, markdownPolicy: "available"});
    assert.equal(adapter.view, "source");
    assert.equal(adapter.getText(), body);
    adapter.reset();
    assert.ok(created.every((engine) => engine.disposed));
    assert.equal(adapter.getText(), "");
});

test("adapter: 100 次会话复用后每个引擎都销毁且无状态残留", () => {
    const {factory, created} = makeFakeEngines();
    const {adapter} = makeAdapter(factory);

    for (let i = 0; i < 100; i++) {
        adapter.loadInitial({body: `session-${i}`, markdownPolicy: "available"});
        assert.equal(adapter.switchView("markdown"), true);
        adapter.reset();
    }

    assert.equal(adapter.view, null);
    assert.equal(adapter.revision, 0);
    assert.equal(adapter.checkpoint, "");
    assert.ok(created.every((engine) => engine.disposed));
});

// ── 0.23.6 §5.7：本轮听写真实追加范围（去 indexOf 猜测）─────────────────

const {SourceEngine} = await import("./engines/source-engine.js");

globalThis.getComputedStyle = () => ({lineHeight: "20px"});
// appendText 判断 activeElement 走"未聚焦直写"路径（headless 无 document）
globalThis.document = {activeElement: null, execCommand: () => false};


/** 最小 textarea 假体（无头环境） */
function fakeTextarea(initial) {
    return {
        value: initial,
        hidden: false,
        selectionStart: 0,
        selectionEnd: 0,
        listeners: {},
        addEventListener(type, fn) {
            this.listeners[type] = fn;
        },
        removeEventListener() {},
        focus() {},
        setSelectionRange(s, e) {
            this.selectionStart = s;
            this.selectionEnd = e;
        },
        scrollTop: 0,
        clientHeight: 100,
    };
}

test("adapter: 前部相同文本时听写范围仍命中文末本轮追加（核心回归）", () => {
    const el = fakeTextarea("DICT");
    const adapter = new EditorAdapter(
        {sourceEl: el, mdContainerEl: {}, mdToolbarEl: null},
        {},
        {
            source: (opts) => new SourceEngine(opts),
            markdown: () => {
                throw new Error("unused");
            },
        },
    );
    adapter.loadInitial({body: "DICT", markdownPolicy: "available"});

    // 听写开始记录锚点（此时全文只有前部那个 "DICT"）
    adapter.beginDictationRun();
    // 追加一段与前部完全相同的文本
    adapter.appendDictation("DICT", {newParagraph: true});
    assert.equal(adapter.getText(), "DICT\n\nDICT");

    const run = adapter.freezeDictationRun();
    assert.ok(run, "本轮范围有效");
    assert.equal(run.text, "DICT", "范围文本不含首段分隔符，且为追加的这段");
    assert.deepEqual(
        {start: run.handle.start, end: run.handle.end},
        {start: 6, end: 10},
        "锚点跳过段落分隔，命中尾部而非前部相同文本",
    );

    // 确认替换只改尾部本轮范围
    assert.equal(adapter.replaceRange(run.handle, "整理稿"), true);
    assert.equal(adapter.getText(), "DICT\n\n整理稿", "前部 'DICT' 未被误替换");
});

test("adapter: locateDictationRun 消费冻结 handle；听写后继续输入只选本轮（核心回归）", () => {
    const el = fakeTextarea("前文");
    const fakeMd = {
        kind: "markdown",
        getText: () => "正文",
        edited: false,
        normalized: false,
        focus() {},
        dispose() {},
    };
    const adapter = new EditorAdapter(
        {sourceEl: el, mdContainerEl: {}, mdToolbarEl: null},
        {},
        {
            source: (opts) => new SourceEngine(opts),
            markdown: () => fakeMd,
        },
    );
    adapter.loadInitial({body: "前文", markdownPolicy: "available"});

    adapter.beginDictationRun();
    adapter.appendDictation("听写内容", {newParagraph: true});

    // 听写结束冻结范围；此后用户继续输入
    const run = adapter.freezeDictationRun();
    assert.ok(run, "结束即冻结本轮范围");
    el.value = adapter.getText() + "，后续手输";

    assert.equal(adapter.locateDictationRun(run), true);
    assert.deepEqual(
        {start: el.selectionStart, end: el.selectionEnd},
        {start: 4, end: 8},
        "定位只选本轮听写，右边界为冻结时的文末（不含后续手输）",
    );

    // 范围内文本被编辑过：定位失效，不产生错误选区
    const tampered = {handle: {...run.handle, text: "被改过"}, text: "被改过", blockSafe: true};
    assert.equal(adapter.locateDictationRun(tampered), false);

    // 视图切换：冻结 handle 随旧引擎作废（无跨引擎迁移，§3.3）
    assert.equal(adapter.switchView("markdown"), true);
    assert.equal(adapter.freezeDictationRun(), null);
    assert.equal(adapter.locateDictationRun(run), false);
});

test("adapter: locateDictationRun 按 Engine kind 分派 locateRange（MD 契约）", () => {
    const located = [];
    const fakeMd = {
        kind: "markdown",
        getText: () => "正文",
        edited: false,
        normalized: false,
        focus() {},
        dispose() {},
        locateRange(handle) {
            located.push(handle);
            return true;
        },
    };
    const adapter = new EditorAdapter(
        {sourceEl: {}, mdContainerEl: {}, mdToolbarEl: null},
        {},
        {
            source: () => {
                throw new Error("unused");
            },
            markdown: () => fakeMd,
        },
    );
    adapter.loadInitial({body: "正文", markdownPolicy: "preferred"});
    assert.equal(adapter.view, "markdown");

    const mdRun = {
        handle: {kind: "markdown", from: 0, to: 2, text: "正文"},
        text: "正文",
        blockSafe: true,
    };
    assert.equal(adapter.locateDictationRun(mdRun), true, "kind 匹配时分派 Engine.locateRange");
    assert.deepEqual(located, [mdRun.handle]);

    // 异 kind handle（旧引擎的 Source handle）拒绝
    const sourceRun = {handle: {kind: "source", start: 0, end: 2, text: "正文"}, text: "正文", blockSafe: true};
    assert.equal(adapter.locateDictationRun(sourceRun), false, "异 kind 不得进入当前 Engine");
    assert.equal(adapter.locateDictationRun(null), false);
});
