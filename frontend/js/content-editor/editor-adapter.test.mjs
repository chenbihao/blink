/**
 * EditorAdapter 测试（0.23.1 基线 + 本次修复的 canonical buffer 语义）。
 *
 * 覆盖：
 * - canonical buffer 是会话正文唯一真源（MD 不再用整篇序列化当正文）；
 * - 反复切换视图逐字节一致；
 * - MD 局部编辑只改变更窗口，未编辑区间逐字节保留；
 * - 无法保证无损修改的结构安全降级为只读预览（不产出任何正文修改）；
 * - 危险结构不能进入可编辑 MD；
 * - reset 后正文/undo（dispose）/检查点/revision 全清。
 */

import {test} from "node:test";
import assert from "node:assert/strict";
import {EditorAdapter} from "./editor-adapter.js";

/**
 * fake 引擎：记录构造参数与生命周期，可控行为。
 * @param {object} [opts]
 * @param {boolean} [opts.mdAlignFail] - 模拟块对齐验证失败（MD 只读降级）
 */
function makeFakeEngines(opts = {}) {
    const created = [];
    class FakeEngine {
        static nextId = 1;
        constructor(options) {
            this.id = FakeEngine.nextId++;
            this.kind = this.constructor.kindName;
            this.initialText = options.initialText ?? options.initialMarkdown;
            this._text = this.initialText;
            this.edited = false;
            this.normalized = false;
            this.readOnly = options.editable === false;
            this.readOnlyReason = this.readOnly ? "forced" : null;
            this.disposed = false;
            this.appended = [];
            this._pending = null;
            this._onChange = options.onChange;
            created.push(this);
        }
        getText() {
            return this._text;
        }
        getSelectionText() {
            return "";
        }
        focus() {}
        hasPendingSourcePatch() {
            return !!this._pending;
        }
        takeSourcePatch() {
            if (!this._pending) return null;
            const result = this._pending;
            this._pending = null;
            if (result.exact === false) {
                this.readOnly = true;
                this.readOnlyReason = "patch-rejected";
                return {exact: false, reason: "patch-verification"};
            }
            this._text = result.text;
            return {text: result.text, exact: result.exact !== false};
        }
        /** 模拟 Source 视图用户输入（textarea 即真源） */
        setText(text, {edited = true, normalized = false} = {}) {
            this._text = text;
            this.edited = edited;
            this.normalized = normalized;
            if (edited) this._onChange?.();
        }
        /** 模拟 MD 视图用户编辑：正文以块级 patch 形式延迟物化 */
        setPendingSource(text, {exact = true} = {}) {
            this._pending = {text, exact};
            this.edited = true;
            this._onChange?.();
        }
        replaceAll(text) {
            if (this.readOnly) return false;
            this.setText(text, {edited: true});
            return true;
        }
        loadContent(text) {
            this._text = text;
            this.edited = false;
        }
        appendText(text) {
            if (this.readOnly) return false;
            this.appended.push(text);
            this._text += text;
            this._onChange?.();
            return true;
        }
        appendParagraph(text) {
            if (this.readOnly) return false;
            this.appended.push(text);
            this._text += `\n\n${text}`;
            this._onChange?.();
            return true;
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
        constructor(options) {
            super(options);
            if (opts.mdAlignFail) {
                this.readOnly = true;
                this.readOnlyReason = "align";
            }
        }
    }
    return {
        created,
        factory: {
            source: (o) => new FakeSource(o),
            markdown: (o) => new FakeMarkdown(o),
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

// ── 基线：视图决策与检查点 ──────────────────────────────────────────────────

test("adapter: preferred 来源 + 门通过 → 默认 MD；未编辑时正文逐字符等于原文", () => {
    const {factory} = makeFakeEngines();
    const {adapter} = makeAdapter(factory);
    const original = "# 标题\n\n正文 *内容*（原始星号列表标记）";

    adapter.loadInitial({body: original, markdownPolicy: "preferred"});
    assert.equal(adapter.view, "markdown");

    assert.equal(adapter.getText(), original);
    assert.equal(adapter.isDirty(), false);
    assert.equal(adapter.isMdEditable(), true);
});

test("adapter: available 来源默认 Source；手动切 MD 再切回还原原文", () => {
    const {factory, created} = makeFakeEngines();
    const {adapter} = makeAdapter(factory);
    const original = "正文";

    adapter.loadInitial({body: original, markdownPolicy: "available"});
    assert.equal(adapter.view, "source");

    assert.equal(adapter.switchView("markdown"), true);
    assert.equal(adapter.view, "markdown");
    const mdEngine = created.at(-1);
    assert.equal(mdEngine.initialText, original);

    assert.equal(adapter.switchView("source"), true);
    const srcEngine = created.at(-1);
    assert.equal(srcEngine.initialText, original);
    assert.equal(adapter.isDirty(), false);
});

// ── 核心：视图切换不改变正文 ────────────────────────────────────────────────

test("adapter: 未修改时反复切换 Source/MD，正文逐字节一致", () => {
    const {factory} = makeFakeEngines();
    const {adapter} = makeAdapter(factory);
    // 含会被 Tiptap 规范化重写的结构：`*` 列表符号、Setext 标题、`__` 强调
    const original = "* 甲\n* 乙\n\nTitle\n=====\n\n__强调__ 段落\n\n尾部\n\n";

    adapter.loadInitial({body: original, markdownPolicy: "available"});
    for (let i = 0; i < 5; i += 1) {
        assert.equal(adapter.switchView("markdown"), true);
        assert.equal(adapter.getText(), original, `第 ${i + 1} 轮切换后正文必须逐字节一致`);
        assert.equal(adapter.isDirty(), false, "未编辑时必须 clean");
        assert.equal(adapter.switchView("source"), true);
        assert.equal(adapter.getText(), original, `第 ${i + 1} 轮切回后正文必须逐字节一致`);
    }
    assert.equal(adapter.revision, 0, "纯切换不得推进内容版本");
});

test("adapter: MD 局部编辑只改变更窗口，且正文真源不是整篇序列化", () => {
    const {factory, created} = makeFakeEngines();
    const {adapter} = makeAdapter(factory);
    const original = "* 甲\n* 乙\n\nTitle\n=====\n\n正文";
    adapter.loadInitial({body: original, markdownPolicy: "preferred"});
    assert.equal(adapter.view, "markdown");

    const md = created.at(-1);
    // 模拟块级 patch：只有末块被替换，前部（含 `*` 与 Setext）逐字节保留
    md.setPendingSource("* 甲\n* 乙\n\nTitle\n=====\n\n正文改");

    // 廉价 dirty：有待物化变更即为 dirty，不需要整篇重算
    assert.equal(adapter.isDirty(), true);

    // 毒化 getText：若适配器仍从序列化取正文，这里会暴露
    md.getText = () => "POISON-整篇序列化";
    assert.equal(adapter.getText(), "* 甲\n* 乙\n\nTitle\n=====\n\n正文改");
    assert.equal(adapter.isDirty(), true, "内容已变，与基线不同");

    adapter.switchView("source");
    assert.equal(created.at(-1).initialText, "* 甲\n* 乙\n\nTitle\n=====\n\n正文改");
    assert.equal(adapter.revision > 0, true);
});

test("adapter: Markdown patch 复核失败时 canonical 正文逐字符不变并进入只读", () => {
    const {factory, created} = makeFakeEngines();
    const {adapter, notices} = makeAdapter(factory);
    adapter.loadInitial({body: "正文", markdownPolicy: "preferred"});

    created.at(-1).setPendingSource("被整篇重写后的文本\n", {exact: false});
    assert.equal(adapter.getText(), "正文", "exact:false 不能进入 canonical buffer");
    assert.ok(notices.includes("editor.md.patchRejected"), "必须说明本次 Markdown 修改未应用");
    assert.equal(adapter.isMdReadOnly(), true, "失败后的投影必须进入只读");
    adapter.switchView("source");
    assert.equal(created.at(-1).initialText, "正文", "切回 Source 仍必须是原始文本");
    assert.equal(adapter.isDirty(), false, "被拒绝的 patch 不得制造正文 dirty");
});

test("adapter: canonical source 保留 CRLF、混合换行与 EOF newline", () => {
    const {factory, created} = makeFakeEngines();
    const {adapter} = makeAdapter(factory);
    const original = "第一行\r\n\r\n第二行\n第三行\r\n";
    adapter.loadInitial({body: original, markdownPolicy: "available"});
    assert.equal(adapter.getText(), original);
    adapter.switchView("markdown");
    adapter.switchView("source");
    assert.equal(created.at(-1).initialText, original);
    assert.equal(adapter.getText(), original);
});

// ── 安全降级：无法保证无损修改 ──────────────────────────────────────────────

test("adapter: 风险门拒绝的结构进入只读 MD 预览，正文不变", () => {
    const {factory, created} = makeFakeEngines();
    const {adapter, notices} = makeAdapter(factory);
    const withTable = "| a | b |\n|---|---|\n| 1 | 2 |\n\n表格后的段落";

    adapter.loadInitial({body: withTable, markdownPolicy: "preferred"});
    // preferred 但门拒绝 → 默认仍是 Source（不自动进入），并提示原因
    assert.equal(adapter.view, "source");
    assert.ok(notices.includes("editor.gate.readonly"));

    // 手动切换：允许预览，但禁止富文本编辑
    assert.equal(adapter.switchView("markdown"), true);
    assert.equal(adapter.view, "markdown");
    assert.equal(adapter.isMdReadOnly(), true);
    assert.equal(adapter.isMdEditable(), false);
    assert.equal(created.at(-1).readOnly, true);
    assert.equal(adapter.getText(), withTable, "只读预览绝不改写正文");
    assert.equal(adapter.isDirty(), false);
});

test("adapter: 块对齐验证失败（align）→ MD 只读且拒绝一切正文修改", () => {
    const {factory} = makeFakeEngines({mdAlignFail: true});
    const {adapter} = makeAdapter(factory);
    const original = "复杂结构正文";
    adapter.loadInitial({body: original, markdownPolicy: "available"});
    assert.equal(adapter.switchView("markdown"), true);
    assert.equal(adapter.mdReadOnlyReason(), "align");
    assert.equal(adapter.isMdEditable(), false);

    // 只读预览下所有正文变更入口一律拒绝
    assert.equal(adapter.appendDictation("听写内容", {newParagraph: true}), false);
    assert.equal(adapter.replaceRange({kind: "markdown", from: 0, to: 1, text: "复"}, "新"), false);
    assert.equal(adapter.freezeRange("selection", "复杂"), null);
    assert.equal(adapter.getText(), original);
});

test("adapter: 门结果按当前正文重估——Source 中新编辑出表格后切 MD 变为只读", () => {
    const {factory, created} = makeFakeEngines();
    const {adapter, notices} = makeAdapter(factory);
    adapter.loadInitial({body: "干净正文", markdownPolicy: "available"});
    assert.equal(adapter.switchView("markdown"), true);
    assert.equal(adapter.isMdEditable(), true, "干净内容可编辑");

    // 切回 Source，编辑出 GFM 表格
    adapter.switchView("source");
    created.at(-1).setText("| a | b |\n|---|---|\n| 1 | 2 |");

    // 载入时的 gate 已过期：切换必须按当前正文重新评估
    assert.equal(adapter.switchView("markdown"), true);
    assert.equal(adapter.isMdEditable(), false, "含表格的正文不得进入可编辑 MD");
    assert.equal(adapter.isMdReadOnly(), true);
    assert.ok(notices.includes("editor.gate.readonly"));
});

test("adapter: markdownPolicy=disabled 时拒绝进入 MD", () => {
    const {factory} = makeFakeEngines();
    const {adapter, notices} = makeAdapter(factory);
    adapter.loadInitial({body: "正文", markdownPolicy: "disabled"});
    assert.equal(adapter.switchView("markdown"), false);
    assert.equal(adapter.view, "source");
    assert.ok(notices.includes("editor.gate.rejected"));
});

test("adapter: 各类危险结构统一安全降级为只读 MD（绝不静默改写）", () => {
    const samples = [
        "| a | b |\n|---|---|\n| 1 | 2 |\n\n正文",
        "<div>html</div>\n\n正文",
        "脚注[^1]\n\n[^1]: 定义\n\n正文",
        "$$\nE = mc^2\n$$\n\n正文",
        "正文 `` 含反引号 `` 结束",
    ];
    for (const src of samples) {
        const {factory} = makeFakeEngines();
        const {adapter} = makeAdapter(factory);
        adapter.loadInitial({body: src, markdownPolicy: "available"});
        assert.equal(adapter.switchView("markdown"), true, "应允许进入只读预览");
        assert.equal(adapter.view, "markdown");
        assert.equal(adapter.isMdEditable(), false, "危险结构不得进入可编辑 MD");
        assert.equal(adapter.getText(), src, "只读预览绝不改写正文");
        // 只读下所有写入入口一律拒绝
        assert.equal(adapter.appendDictation("听写", {newParagraph: false}), false);
        assert.equal(adapter.freezeRange("selection", "正文"), null);
    }
});

test("adapter: 超尺寸（性能门）不进入 MD，连只读预览也不进入", () => {
    const {factory, created} = makeFakeEngines();
    const {adapter, notices} = makeAdapter(factory);
    const huge = "字".repeat(140 * 1024); // > 128KB：整篇解析会卡死窗口

    adapter.loadInitial({body: huge, markdownPolicy: "available"});
    assert.equal(adapter.mdReachable(), false);
    assert.equal(adapter.switchView("markdown"), false, "超尺寸必须拒绝进入 MD");
    assert.equal(adapter.view, "source");
    assert.ok(notices.includes("editor.gate.large"));
    assert.equal(created.filter((e) => e.kind === "markdown").length, 0, "不得创建 MD 引擎");

    // preferred 来源同样不自动进入
    const second = makeFakeEngines();
    const {adapter: adapter2} = makeAdapter(second.factory);
    adapter2.loadInitial({body: huge, markdownPolicy: "preferred"});
    assert.equal(adapter2.view, "source");
    assert.equal(adapter2.mdReachable(), false);
});

// ── 检查点 / revision / reset ──────────────────────────────────────────────

test("adapter: 提交成功后检查点前移", () => {
    const {factory, created} = makeFakeEngines();
    const {adapter} = makeAdapter(factory);
    adapter.loadInitial({body: "v1", markdownPolicy: "available"});

    created.at(-1).setText("v2");
    assert.equal(adapter.isDirty(), true);

    adapter.setCheckpoint("v2");
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
    assert.equal(adapter.buffer.text, "");
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

// ── 外部同步 ────────────────────────────────────────────────────────────────

test("adapter: syncFromExternal 同时前移缓冲区与检查点并推进版本", () => {
    const {factory} = makeFakeEngines();
    const {adapter} = makeAdapter(factory);
    adapter.loadInitial({body: "旧内容", markdownPolicy: "available"});
    const before = adapter.revision;

    adapter.syncFromExternal("便签新内容");
    assert.equal(adapter.getText(), "便签新内容");
    assert.equal(adapter.isDirty(), false, "外部同步后应为 clean");
    // 内容确实变了，版本必须前进：否则下一次显式提交会被提交协议判为 StaleRevision
    assert.ok(adapter.revision > before, "程序化内容替换也必须推进内容版本");
});

test("adapter: 生命周期临界区锁定时拒绝切换与所有正文写入", () => {
    const {factory} = makeFakeEngines();
    const {adapter} = makeAdapter(factory);
    adapter.loadInitial({body: "原文", markdownPolicy: "available"});
    adapter.setInteractionLocked(true);

    assert.equal(adapter.sourceEl.readOnly, true);
    assert.equal(adapter.switchView("markdown"), false);
    assert.equal(adapter.replaceAll("覆盖"), false);
    assert.equal(adapter.appendDictation("追加"), false);
    assert.equal(adapter.restoreDraft("恢复"), false);
    assert.equal(adapter.syncFromExternal("外部"), false);
    assert.equal(adapter.getText(), "原文");

    adapter.setInteractionLocked(false);
    assert.equal(adapter.sourceEl.readOnly, false);
    assert.equal(adapter.replaceAll("解锁后"), true);
    assert.equal(adapter.getText(), "解锁后");
});

test("adapter: CRLF 载入保持原始换行（canonical 真源口径）", () => {
    const {factory} = makeFakeEngines();
    const {adapter} = makeAdapter(factory);
    adapter.loadInitial({body: "第一行\r\n第二行\r\n", markdownPolicy: "available"});
    assert.equal(adapter.getText(), "第一行\r\n第二行\r\n");
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
        readOnly: false,
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
        readOnly: false,
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
