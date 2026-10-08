/**
 * MarkdownIrEngine（0.23.1；本次修复引入源码感知 patch）——Tiptap/ProseMirror 所见即所得引擎。
 *
 * ## 与 CanonicalBuffer 的关系（本次修复的核心）
 *
 * 修复前：本引擎的 `getText()`（= `editor.getMarkdown()` 整篇序列化）就是
 * MD 视图下的"正文"，于是**一次局部编辑会把整篇文档按编辑器规范重写**
 * （`*`→`-`、`__`→`**`、Setext→ATX、空行收敛），未编辑区间被静默改变。
 *
 * 现在：会话正文的唯一真源是 `CanonicalBuffer`（见 canonical-buffer.js），
 * 本引擎只是它的**投影**：
 * - 载入时建立「源文内容块 ↔ 文档顶层节点」映射并做对齐验证
 *   （md-source-patch.js::buildBlockMap）；
 * - 用户编辑后由 `takeSourcePatch(baseSource)` 产出**最小块级 patch**，
 *   只改写变更窗口对应的源文区间；
 * - 复核失败时走**序列化兜底**（0.25.18）：canonical 前进到整篇序列化
 *   结果（内容保全、格式可能规范化、保持可编辑），**绝不回滚用户输入**；
 *   仅当序列化器自身抛错才退回「回滚 + 只读」（最后防线）；
 * - 载入对齐验证不通过（罕见结构 / 块丢失）→ `readOnly = true`：仍可预览，
 *   但**禁止富文本编辑**，也不会产出任何 patch。
 *
 * `getText()`（整篇序列化）保留仅用于诊断与规范化比对，
 * **不再作为会话正文真源**（EditorAdapter 一律读缓冲区）。
 */

import {
    createMarkdownEditor,
    normalizeEol,
    parseMarkdown,
    serializeMarkdown,
} from "../../shared/tiptap-editor.js";
import {createMdToolbar, bindMdToolbar, updateToolbarStates} from "../../shared/md-toolbar.js";
import {
    buildBlockMap,
    jsonEqual,
    normalizeNodeJson,
    planSourcePatch,
} from "../md-source-patch.js";
import {CanonicalBuffer} from "../canonical-buffer.js";

export class MarkdownIrEngine {
    kind = "markdown";

    /** 用户是否发生过编辑（程序化载入/还原不计入） */
    edited = false;

    /** 载入内容是否被 Tiptap 规范化重写（序列化形态 ≠ 原文） */
    normalized = false;

    /**
     * 只读预览：对齐验证失败或显式 `editable=false`。
     * 只读时禁止一切正文修改（用户输入 + 程序化追加/替换），
     * 也不产出 patch——"无法保证无损就不修改"。
     */
    readOnly = false;

    /** 只读原因：`"align"`（块对齐验证失败）/ `"forced"`（调用方禁止编辑） */
    readOnlyReason = null;

    /**
     * @param {object} options
     * @param {HTMLElement} options.element - Tiptap 挂载容器
     * @param {HTMLElement|null} options.toolbarMount - MD 工具栏挂载点（null/只读时不装配）
     * @param {string} options.initialMarkdown - 初始 Markdown（canonical 原文）
     * @param {boolean} [options.editable=true]
     * @param {() => void} [options.onChange] - 用户可感知变更
     * @param {() => void} [options.onSelectionChange]
     * @param {object} [options.deps] - 解析/序列化注入（测试用）
     * @param {(md: string) => any} [options.deps.parseDoc]
     * @param {(json: any) => string} [options.deps.serializeDoc]
     * @throws Tiptap bundle 未加载或初始化失败时抛错（调用方降级 Source）
     */
    constructor({
        element,
        toolbarMount,
        initialMarkdown,
        editable = true,
        onChange,
        onSelectionChange,
        deps = {},
    }) {
        if (!window.BlinkTiptap) {
            throw new Error("BlinkTiptap bundle 未加载");
        }
        this.el = element;
        this._onChange = onChange ?? null;
        this._onSelectionChange = onSelectionChange ?? null;
        this._suppress = false;
        this._editable = editable;

        this.editor = createMarkdownEditor({
            element,
            initialMarkdown,
            editable,
            onUpdate: () => {
                if (this._suppress) return;
                this.edited = true;
                this._onChange?.();
            },
            onSelectionUpdate: () => this._onSelectionChange?.(),
            onTransaction: () => this._onSelectionChange?.(),
        });

        // 解析/序列化适配：默认走生产 Tiptap MarkdownManager（editor.markdown）
        this._parseDoc = deps.parseDoc ?? ((md) => this.editor.markdown.parse(md));
        this._serializeDoc = deps.serializeDoc ?? ((json) => this.editor.markdown.serialize(json));

        // 载入后立即序列化比对——规范化检测（一次性成本，见 0.23.0 perf 报告）
        try {
            this.normalized = normalizeEol(serializeMarkdown(this.editor)) !== normalizeEol(initialMarkdown);
        } catch {
            this.normalized = false;
        }

        // 建立块映射与只读判定（必须在任何用户交互之前）
        const map = this._rebuildDocumentState(initialMarkdown);

        // MD 工具栏：只读预览下不装配（编辑工具无意义）
        if (toolbarMount && !this.readOnly) {
            toolbarMount.innerHTML = "";
            this.toolbar = createMdToolbar("md-toolbar-inner");
            toolbarMount.appendChild(this.toolbar);
            bindMdToolbar(this.toolbar, this.editor, {editorEl: element});
            this.editor.on("selectionUpdate", () => updateToolbarStates(this.toolbar, this.editor));
            this.editor.on("transaction", () => updateToolbarStates(this.toolbar, this.editor));
        }

        if (this.readOnly && editable) {
            // 对齐验证失败：禁止富文本编辑（保留渲染预览）
            try {
                this.editor.setEditable(false);
            } catch (e) {
                console.error("[markdown-engine] 切换只读失败:", e);
            }
        }
        void map;

        this.el.hidden = false;
    }

    // ── 源码感知 patch ──────────────────────────────────────────────────────

    /**
     * 自上次同步以来文档顶层节点是否发生了变化（廉价：未改动子树命中缓存）。
     * 供 Adapter 做 O(1) 级 dirty 判定，避免每次按键都物化整篇源文。
     */
    hasPendingSourcePatch() {
        if (this.readOnly) return false;
        return !jsonEqual(this._nodeSnapshot, this._topSnapshot());
    }

    /**
     * 产出针对 canonical 源文的最小块级 patch（延迟物化）。
     *
     * @param {string} baseSource - 当前 canonical 源文
     * @returns {{text: string, exact: true}
     *          |{text: string, exact: false, normalized: true, reason: string}
     *          |{exact: false, reason: string}|null}
     *   - `null`：无可应用变更（未编辑 / 只读 / 源文已被外部改变）
     *   - `exact:true`：块级无损（未编辑区间逐字节保留）
     *   - `normalized:true`：**序列化兜底**（0.25.18）——块级复核失败但内容
     *     保全：canonical 前进到整篇序列化结果（格式可能被规范化），投影
     *     重建为同一文本的解析形态，**保持可编辑**，绝不回滚用户输入
     *   - 其余 `exact:false`：序列化器自身异常，无法产出任何文本——维持
     *     回滚 + 只读（最后防线，正常不可达）
     */
    takeSourcePatch(baseSource) {
        if (this.readOnly) return null;
        if (typeof baseSource !== "string") return null;

        if (baseSource !== this._baseSource) {
            // 外部已改变源文（不应发生）：保守重建映射，不产出 patch
            this._rebuildDocumentState(baseSource);
            return null;
        }

        const after = this._topSnapshot();
        if (jsonEqual(this._nodeSnapshot, after)) return null;

        const patches = planSourcePatch({
            sourceText: baseSource,
            blocks: this._sourceMap?.blocks ?? [],
            beforeNodes: this._nodeSnapshot,
            afterNodes: after,
            serializeNode: (node) => this._serializeNodeText(node),
        });

        const buffer = new CanonicalBuffer(baseSource);
        const applied = patches.length > 0 ? buffer.applyPatches(patches) : false;
        const candidate = applied ? buffer.text : baseSource;

        // 整篇复核：新源文必须解析回当前文档——否则块级无损不成立。
        // parse 产物过 normalizeNodeJson 与 toJSON 快照口径对齐（空 content 差异）。
        let exact = false;
        try {
            exact = jsonEqual(normalizeNodeJson(this._parseDoc(candidate)), {type: "doc", content: after});
        } catch {
            exact = false;
        }

        if (!exact) {
            return this._materializeByFallback(after);
        }

        this._baseSource = candidate;
        this._nodeSnapshot = after;
        this._sourceMap = buildBlockMap(
            candidate,
            {type: "doc", content: after},
            (text) => this._parseBlockNode(text),
        );
        this.edited = false;
        return {text: candidate, exact: true};
    }

    /**
     * 序列化兜底（0.25.18 安全模型第二层）：内容保全优先于字节保真。
     *
     * 块级 patch 复核失败有两类根因：(a) patch 规划缺陷（内容本身可无损
     * 表达）；(b) 序列化器对当前文档自身不可往返——段落尾随空格被丢弃、
     * 代码块内容含围栏字符、空引用段、纯空白文本段等（实测探测锚定）。
     * 旧模型把两类一律「回滚用户输入 + 永久只读」，(b) 类用户实测一次丢
     * 22 段正文。兜底层用整篇序列化接管：
     *
     * - 序列化结果若能解析回当前文档（a 类）：直接接受为新 canonical，
     *   投影不动、零感知——未编辑区间的格式可能被规范化重写（`*`→`-`
     *   等），由调用方提示；
     * - 序列化结果解析不回当前文档（b 类）：仍接受文本（内容完整、格式
     *   尽力），投影重建为该文本的解析形态（与用户所见通常逐字符相同，
     *   差异是不可表达的空白/结构细节），保持可编辑；
     * - 序列化自身抛错：最后防线，维持回滚 + 只读。
     */
    _materializeByFallback(after) {
        let fallback = null;
        let fallbackDoc = null;
        try {
            fallback = serializeMarkdown(this.editor);
            fallbackDoc = this._parseDoc(fallback);
        } catch (error) {
            console.warn(
                "[markdown-engine] patch 复核失败且序列化兜底异常 → 回滚并转只读"
                + ` blocks=${this._sourceMap?.blocks.length} nodes=${after.length}`,
                error,
            );
            this._restoreCanonicalProjection(this._baseSource, "patch-rejected");
            return {exact: false, reason: "patch-verification"};
        }

        const reason = jsonEqual(normalizeNodeJson(fallbackDoc), {type: "doc", content: after})
            ? "serialize-exact"           // (a) patch 规划缺陷，序列化本身无损
            : "serialize-normalized";     // (b) 状态不可表达，格式规范化
        console.warn(
            "[markdown-engine] patch 整篇复核失败 → 序列化兜底（内容保全，格式可能规范化）"
            + ` reason=${reason} blocks=${this._sourceMap?.blocks.length} nodes=${after.length}`,
        );

        if (reason === "serialize-exact") {
            // 文档即 fallback 的解析形态：只前进 canonical 与映射，不动投影
            this._baseSource = fallback;
            this._nodeSnapshot = after;
            this._sourceMap = buildBlockMap(
                fallback,
                {type: "doc", content: after},
                (text) => this._parseBlockNode(text),
            );
            this.edited = false;
            this.normalized = true;
            return {text: fallback, exact: false, normalized: true, reason};
        }

        // (b)：投影重建到规范化形态（suppress：程序化载入不计用户编辑）
        this._suppress = true;
        try {
            this.editor.commands.setContent(fallbackDoc, false);
        } finally {
            this._suppress = false;
        }
        this.edited = false;
        this._rebuildDocumentState(fallback);
        this.readOnlyReason = this.readOnly ? reason : null;
        this.normalized = true;
        return {text: fallback, exact: false, normalized: true, reason};
    }

    /** 文档顶层节点 JSON 快照（WeakMap 缓存：未改动子树仅一次引用比较）。
     *  产物过 normalizeNodeJson：与 parse 产物口径对齐（空 content / 脏 key）。 */
    _topSnapshot() {
        const doc = this.editor.state.doc;
        const out = [];
        for (let i = 0; i < doc.childCount; i += 1) {
            const node = doc.child(i);
            let json = this._jsonCache.get(node);
            if (json === undefined) {
                json = normalizeNodeJson(node.toJSON());
                this._jsonCache.set(node, json);
            }
            out.push(json);
        }
        return out;
    }

    /** 以给定源文重建映射与快照；同步刷新 `readOnly` */
    _rebuildDocumentState(source) {
        this._baseSource = typeof source === "string" ? source : "";
        this._jsonCache = new WeakMap();
        this._nodeSnapshot = this._topSnapshot();
        const map = buildBlockMap(
            this._baseSource,
            {type: "doc", content: this._nodeSnapshot},
            (text) => this._parseBlockNode(text),
        );
        this._sourceMap = map;
        this.readOnly = !map.ok || !this._editable;
        this.readOnlyReason = !map.ok ? "align" : (this._editable ? null : "forced");
        if (!map.ok) {
            // 只读降级必须可诊断：块切分 vs 文档节点的对齐数据直接进控制台
            const nodes = this._nodeSnapshot;
            console.warn(
                `[markdown-engine] 块对齐失败 → MD 只读预览 mismatchAt=${map.mismatchAt}`
                + ` blocks=${map.blocks.length} nodes=${nodes.length}`
                + ` blockTexts=${JSON.stringify(map.blocks.map((b) => b.text.slice(0, 60)))}`,
            );
        }
        return map;
    }

    /** 单块解析：解析结果必须是恰好一个顶层节点，否则视为不可对齐 */
    _parseBlockNode(blockText) {
        try {
            const json = this._parseDoc(blockText);
            const content = json?.content;
            if (!Array.isArray(content) || content.length !== 1) return null;
            return normalizeNodeJson(content[0]);
        } catch {
            return null;
        }
    }

    /**
     * 单节点序列化（`&nbsp;` 等内部表示由序列化器决定）。
     * 不做空白剃除——尾随空格是用户内容（parser 逐字保留），首尾换行的
     * 剥除由 md-source-patch.nodeText 统一负责（0.25.18）。
     */
    _serializeNodeText(node) {
        try {
            const raw = this._serializeDoc({type: "doc", content: [node]});
            return typeof raw === "string" ? raw : "";
        } catch {
            return "";
        }
    }

    /**
     * Restore the rendered document to canonical source after a rejected
     * source patch.  This deliberately does not touch the caller's buffer.
     */
    _restoreCanonicalProjection(source, reason) {
        try {
            const json = this._parseDoc(source);
            this._suppress = true;
            try {
                this.editor.commands.setContent(json, false);
            } finally {
                this._suppress = false;
            }
            this.edited = false;
            this._rebuildDocumentState(source);
            this.readOnly = true;
            this.readOnlyReason = reason;
            this.editor.setEditable(false);
            if (this.toolbar) this.toolbar.hidden = true;
        } catch (error) {
            // A source that was previously aligned should parse here.  If a
            // vendor/parser regression still prevents reconstruction, retain
            // the safety invariant: remain read-only and never emit source text.
            this.readOnly = true;
            this.readOnlyReason = reason;
            console.warn("[markdown-engine] canonical projection restore failed", error);
        }
    }

    // ── 引擎契约（§3.4）────────────────────────────────────────────────────

    /**
     * 整篇序列化快照。
     * **不作为会话正文真源**（真源是 CanonicalBuffer）——仅用于诊断与
     * 序列化形态比对。
     */
    getText() {
        return serializeMarkdown(this.editor);
    }

    /** 当前选中文本（ProseMirror from/to 区间） */
    getSelectionText() {
        const {state} = this.editor;
        const {from, to, empty} = state.selection;
        if (empty) return "";
        return state.doc.textBetween(from, to, "\n");
    }

    /**
     * 程序化全文替换（单事务）。仅用于「外部同步」这类源文整体变更路径
     * （调用方同时会把 CanonicalBuffer 设为同一文本）；只读预览下拒绝。
     */
    replaceAll(markdown) {
        if (this.readOnly) return false;
        const json = parseMarkdown(this.editor, markdown);
        this._suppress = true;
        try {
            this.editor.chain().clearContent().insertContent(json).run();
        } finally {
            this._suppress = false;
        }
        this.edited = true;
        this._rebuildDocumentState(markdown);
        this._onChange?.();
        return true;
    }

    /**
     * 文末追加文本（0.23.3 听写追尾，单事务）。只读预览下拒绝。
     * @param {string} text
     * @returns {boolean} 是否已写入
     */
    appendText(text) {
        if (this.readOnly || !text) return false;
        this._insertAtDocEnd(text, {newParagraph: false});
        return true;
    }

    /**
     * 文末新起一段追加（0.23.3 首段语义）。只读预览下拒绝。
     * @param {string} text
     * @returns {boolean} 是否已写入
     */
    appendParagraph(text) {
        if (this.readOnly || !text) return false;
        this._insertAtDocEnd(text, {newParagraph: true});
        return true;
    }

    /** appendText / appendParagraph 共用实现（单事务 + selection 恢复） */
    _insertAtDocEnd(text, {newParagraph}) {
        const {state, schema} = this.editor;
        const lastNode = state.doc.lastChild;
        const lastIsParagraph = !!lastNode && lastNode.type === schema.nodes.paragraph;
        const lastEmpty = lastIsParagraph && lastNode.content.size === 0;
        // 段内追加：doc 收尾 token 占 1，插到 docEnd-1；新段落：插到 docEnd
        const intoParagraph = lastIsParagraph && (!newParagraph || lastEmpty);
        const insertPos = intoParagraph ? state.doc.content.size - 1 : state.doc.content.size;
        const prevFrom = state.selection.from;
        const prevTo = state.selection.to;
        // 光标原在文末时让它自然跟随追加内容（打字语义）；其余恢复原选区，
        // 追尾不移动前文 selection（§6.4）
        const docEndPos = state.doc.content.size - 1;
        const atEnd = prevFrom >= docEndPos && prevTo >= docEndPos;

        let chain = this.editor.chain().command(({tr}) => {
            if (intoParagraph) {
                tr.insertText(text, insertPos);
            } else {
                const para = schema.nodes.paragraph.create(null, schema.text(text));
                tr.insert(insertPos, para);
            }
            return true;
        });
        if (!atEnd) {
            chain = chain.setTextSelection({from: prevFrom, to: prevTo});
        }
        chain.run();
        // 程序化插入是真实编辑：onUpdate 触发 edited=true + onChange（不 suppress）
    }

    /**
     * 在当前文档中选中给定文本并滚动到可见（"定位到本次听写"）。
     * 跨 text node 拼接全文后按字符下标映射回 doc position。
     */
    locateText(text) {
        if (!text) return false;
        const {full, spans} = this._concatText();
        const idx = full.indexOf(text);
        if (idx < 0) return false;
        const from = MarkdownIrEngine._posAt(spans, idx);
        if (from == null) return false;
        const to = MarkdownIrEngine._posAt(spans, idx + text.length) ?? from + text.length;
        this.editor.chain().setTextSelection({from, to}).scrollIntoView().run();
        return true;
    }

    /** 文档 text node 拼接全文 + 各 text node 起点的字符偏移映射。 */
    _concatText() {
        let full = "";
        /** @type {Array<{start: number, pos: number}>} text node 起点映射 */
        const spans = [];
        this.editor.state.doc.descendants((node, pos) => {
            if (node.isText && node.text) {
                spans.push({start: full.length, pos});
                full += node.text;
            }
            return true;
        });
        return {full, spans};
    }

    /** 当前文档拼接文本的字符长度（听写追加锚点，0.23.6 §5.7）。 */
    tailCharLength() {
        return this._concatText().full.length;
    }

    /**
     * 冻结 [fromChar, 文末] 的本轮听写范围（0.23.6 §5.7）。
     * 锚点为听写开始时记录的拼接文本偏移，不做全文 indexOf 猜测。
     */
    createTailRangeHandle(fromChar) {
        const {full, spans} = this._concatText();
        const start = Math.min(Math.max(fromChar, 0), full.length);
        if (start >= full.length) return null;
        const from = MarkdownIrEngine._posAt(spans, start);
        if (from == null) return null;
        const to = MarkdownIrEngine._posAt(spans, full.length) ?? from + (full.length - start);
        return {
            kind: this.kind,
            from,
            to,
            text: full.slice(start),
            blockSafe: MarkdownIrEngine._blockSafe(this.editor.state, from, to),
        };
    }

    /**
     * 选中并滚动到冻结的听写范围（"定位到本次听写"）。
     * 右边界是冻结时的文末，听写结束后继续输入不会被一并选中；
     * 范围内文本已被编辑过时返回 false。
     */
    locateRange(handle) {
        if (!handle || handle.kind !== this.kind) return false;
        const {from, to, text: expected} = handle;
        const {state} = this.editor;
        if (!Number.isInteger(from) || !Number.isInteger(to)
            || from < 0 || to > state.doc.content.size || from >= to) {
            return false;
        }
        if (state.doc.textBetween(from, to, "\n") !== expected) return false;
        this.editor.chain().setTextSelection({from, to}).scrollIntoView().run();
        return true;
    }

    /** 字符偏移 → doc position（spans 逆序查找；越界返回 null）。 */
    static _posAt(spans, charIndex) {
        for (let i = spans.length - 1; i >= 0; i--) {
            if (spans[i].start <= charIndex) return spans[i].pos + (charIndex - spans[i].start);
        }
        return null;
    }

    /**
     * 程序化载入外部内容（便签同步等）。**允许在只读预览下执行**：
     * 外部同步不是"富文本编辑"，且调用方会把 CanonicalBuffer 设为同一文本。
     * edited/normalized 与块映射按新内容重算。
     */
    loadContent(markdown) {
        const json = parseMarkdown(this.editor, markdown);
        this._suppress = true;
        try {
            this.editor.commands.setContent(json, false);
        } finally {
            this._suppress = false;
        }
        this.edited = false;
        try {
            this.normalized = normalizeEol(serializeMarkdown(this.editor)) !== normalizeEol(markdown);
        } catch {
            this.normalized = false;
        }
        this._rebuildDocumentState(markdown);
    }

    /**
     * 冻结当前选区为 opaque range handle（0.23.4 §3.4）。
     * 只读预览下不签发 handle（整理结果无法确认替换）。
     */
    createSelectionRangeHandle() {
        if (this.readOnly) return null;
        const {state} = this.editor;
        const {from, to, empty} = state.selection;
        if (empty) return null;
        return {
            kind: this.kind,
            from,
            to,
            text: state.doc.textBetween(from, to, "\n"),
            blockSafe: MarkdownIrEngine._blockSafe(state, from, to),
        };
    }

    /**
     * 在当前文档文本中定位给定文本并冻结为 range handle（选区整理）。
     * 定位口径与 locateText 一致（text node 拼接，不含块分隔符）。
     */
    createTextRangeHandle(text) {
        if (this.readOnly || !text) return null;
        const {full, spans} = this._concatText();
        const idx = full.indexOf(text);
        if (idx < 0) return null;
        const from = MarkdownIrEngine._posAt(spans, idx);
        if (from == null) return null;
        const to = MarkdownIrEngine._posAt(spans, idx + text.length) ?? from + text.length;
        return {
            kind: this.kind,
            from,
            to,
            text,
            blockSafe: MarkdownIrEngine._blockSafe(this.editor.state, from, to),
        };
    }

    /** 块边界安全判定：range 两端同属一个文本块（§3.7 替换安全条件）。 */
    static _blockSafe(state, from, to) {
        try {
            const $from = state.doc.resolve(from);
            const $to = state.doc.resolve(to);
            return $from.sameParent($to) && $from.parent.isTextblock;
        } catch {
            return false;
        }
    }

    /**
     * 规划 blockSafe 范围的替换（纯函数，headless 可测）。
     *
     * 两条引擎级约束（docs/reports/20260919 排查报告 §4/§5.1）：
     * - Markdown 源文不存在能解析回真实空段落节点的字节序列——AI 输出中的
     *   空行必须跳过，`\n\n` 分隔由相邻段落序列化时自然恢复，否则延迟物化
     *   的整篇复核必败（回滚 + 只读）；
     * - 多行替换必须扩为父 textblock 的块级替换：在段内行内 range 上
     *   `replaceWith` 段落数组会被 slice fitter 闭合出幽灵空段落。
     *
     * 范围外同段文字（blockSafe 只保证单块，不保证整块）回填进首/末新段，
     * 保证实际替换的恰好是冻结文本本身。
     *
     * @param {object} state - 替换前的 ProseMirror EditorState（提供 doc/schema）
     * @param {number} from @param {number} to - blockSafe 行内范围
     * @param {string} newText - AI 整理稿（可含 `\n` / `\n\n`）
     * @returns {{kind: "inline", text: string}
     *          |{kind: "block", blockFrom: number, blockTo: number, nodes: object[]}}
     */
    static planRangeReplacement(state, from, to, newText) {
        const lines = (typeof newText === "string" ? newText : "")
            .split("\n").filter((line) => line.trim() !== "");
        if (lines.length <= 1) {
            // 单行（或全空行 = 删除范围文本）：行内替换，不动块结构
            return {kind: "inline", text: lines[0] ?? ""};
        }
        const $from = state.doc.resolve(from);
        const $to = state.doc.resolve(to);
        const prefix = $from.parent.textBetween(0, $from.parentOffset, "\n");
        const suffix = $to.parent.textBetween($to.parentOffset, $to.parent.content.size, "\n");
        const nodes = lines.map((line, i) => {
            const text = (i === 0 ? prefix : "") + line + (i === lines.length - 1 ? suffix : "");
            return state.schema.nodes.paragraph.create(null, text ? state.schema.text(text) : null);
        });
        return {kind: "block", blockFrom: $from.before(), blockTo: $to.after(), nodes};
    }

    /**
     * 单事务替换 handle 范围（0.23.4 §3.7 确认应用路径）。
     * 仅接受 blockSafe 范围；先复核冻结文本，再一次事务完成替换。
     * 只读预览、校验失败或引擎不支持时返回 false（不产生修改）。
     */
    replaceRange(handle, newText) {
        if (this.readOnly) return false;
        if (!handle || typeof newText !== "string" || !handle.blockSafe) return false;
        const {from, to, text: expected} = handle;
        const {state} = this.editor;
        if (!Number.isInteger(from) || !Number.isInteger(to)
            || from < 0 || to > state.doc.content.size || from >= to) {
            return false;
        }
        if (state.doc.textBetween(from, to, "\n") !== expected) return false;

        let plan = null;
        try {
            plan = MarkdownIrEngine.planRangeReplacement(state, from, to, newText);
            this.editor.chain().command(({tr}) => {
                if (plan.kind === "inline") {
                    tr.insertText(plan.text, from, to);
                } else {
                    tr.replaceWith(plan.blockFrom, plan.blockTo, plan.nodes);
                }
                return true;
            }).run();
        } catch (e) {
            console.warn("[markdown-engine] replaceRange 事务失败:", e);
            return false;
        }
        // 焦点交还编辑器（selection 随事务映射），保证"一次 Ctrl+Z 恢复"可达
        this.editor.commands.focus();
        // 程序化替换是真实编辑：onUpdate 触发 edited=true + onChange（revision 自增）
        return true;
    }

    focus() {
        this.editor.commands.focus();
    }

    /** 完整清理：销毁实例（undo 历史随实例消亡）、清空挂载点 */
    dispose() {
        try {
            this.editor.destroy();
        } catch (e) {
            console.error("[markdown-engine] destroy 失败:", e);
        }
        this.el.hidden = true;
        this.el.innerHTML = "";
        if (this.toolbar) {
            this.toolbar.remove();
            this.toolbar = null;
        }
        this._onChange = null;
        this._onSelectionChange = null;
    }
}
