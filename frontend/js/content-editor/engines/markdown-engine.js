/**
 * MarkdownIrEngine（0.23.1）——Tiptap/ProseMirror 所见即所得编辑引擎。
 *
 * 与 SourceEngine 共同满足 Adapter 契约（§3.4）。MD 侧特殊职责：
 * - **edited 标志**：首次用户编辑前置位。未编辑时 Adapter 用会话级检查点
 *   逐字符还原原文（Tiptap 规范化重写不回写正文）。
 * - **suppress 抑制**：程序化 setContent（markdown parse 路径）可能仍触发
 *   update 事件（便签侧实证），载入/还原期间屏蔽，防止假 edited。
 * - **normalized 检测**：载入后立即序列化一次，与原文比对得出"已按编辑器
 *   规范重写"标志（§3.10：首次 MD 编辑保存时提示）。
 */

import {
    createMarkdownEditor,
    normalizeEol,
    parseMarkdown,
    serializeMarkdown,
} from "../../shared/tiptap-editor.js";
import {createMdToolbar, bindMdToolbar, updateToolbarStates} from "../../shared/md-toolbar.js";

export class MarkdownIrEngine {
    kind = "markdown";

    /** 用户是否发生过编辑（程序化载入/还原不计入） */
    edited = false;

    /** 载入内容是否被 Tiptap 规范化重写（序列化形态 ≠ 原文） */
    normalized = false;

    /**
     * @param {object} options
     * @param {HTMLElement} options.element - Tiptap 挂载容器
     * @param {HTMLElement|null} options.toolbarMount - MD 工具栏挂载点（null 则不装配）
     * @param {string} options.initialMarkdown - 初始 Markdown（应为载入侧 \r\n 归一化后的文本）
     * @param {boolean} [options.editable=true]
     * @param {() => void} [options.onChange] - 用户可感知变更
     * @param {() => void} [options.onSelectionChange]
     * @throws Tiptap bundle 未加载或初始化失败时抛错（调用方降级 Source）
     */
    constructor({element, toolbarMount, initialMarkdown, editable = true, onChange, onSelectionChange}) {
        if (!window.BlinkTiptap) {
            throw new Error("BlinkTiptap bundle 未加载");
        }
        this.el = element;
        this._onChange = onChange ?? null;
        this._onSelectionChange = onSelectionChange ?? null;
        this._suppress = false;

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

        // 载入后立即序列化比对——规范化检测（一次性成本，见 0.23.0 perf 报告）
        try {
            this.normalized = normalizeEol(serializeMarkdown(this.editor)) !== normalizeEol(initialMarkdown);
        } catch {
            this.normalized = false;
        }

        // MD 工具栏（与便签共用 components/md-toolbar）；引擎实例级监听随引擎创建
        if (toolbarMount) {
            toolbarMount.innerHTML = "";
            this.toolbar = createMdToolbar("md-toolbar-inner");
            toolbarMount.appendChild(this.toolbar);
            bindMdToolbar(this.toolbar, this.editor, {editorEl: element});
            this.editor.on("selectionUpdate", () => updateToolbarStates(this.toolbar, this.editor));
            this.editor.on("transaction", () => updateToolbarStates(this.toolbar, this.editor));
        }

        this.el.hidden = false;
    }

    /**
     * 全文快照。未编辑时返回 undefined 语义由 Adapter 决定（用检查点还原），
     * 编辑后返回序列化文本（含 EOF 换行约定）。
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
     * 程序化全文替换（单事务）。0.23.4 AI 候选确认应用走此路径；
     * 置 edited=true（内容确已改变）。
     */
    replaceAll(markdown) {
        const json = parseMarkdown(this.editor, markdown);
        this._suppress = true;
        try {
            this.editor.chain().clearContent().insertContent(json).run();
        } finally {
            this._suppress = false;
        }
        this.edited = true;
        this._onChange?.();
    }

    /**
     * 文末追加文本（0.23.3 听写追尾，单事务）。末尾节点是段落时在段内
     * 追加；否则（空文档/围栏代码块等收尾）插入新段落。恢复用户此前
     * selection、不抢焦点、不滚动。onUpdate 置 edited 并通知 revision。
     * @param {string} text
     */
    appendText(text) {
        if (!text) return;
        this._insertAtDocEnd(text, {newParagraph: false});
    }

    /**
     * 文末新起一段追加（0.23.3 首段语义）：末段非空段落时插入新段落，
     * 其余情况与 appendText 等价（空段落被填充/非段落收尾本就需新段）。
     * @param {string} text
     */
    appendParagraph(text) {
        if (!text) return;
        this._insertAtDocEnd(text, {newParagraph: true});
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
     * @param {string} text
     * @returns {boolean} 是否找到并选中
     */
    locateText(text) {
        if (!text) return false;
        const {state} = this.editor;
        let full = "";
        /** @type {Array<{start: number, pos: number}>} text node 起点映射 */
        const spans = [];
        state.doc.descendants((node, pos) => {
            if (node.isText && node.text) {
                spans.push({start: full.length, pos});
                full += node.text;
            }
            return true;
        });
        const idx = full.indexOf(text);
        if (idx < 0) return false;
        const posAt = (ci) => {
            for (let i = spans.length - 1; i >= 0; i--) {
                if (spans[i].start <= ci) return spans[i].pos + (ci - spans[i].start);
            }
            return null;
        };
        const from = posAt(idx);
        if (from == null) return false;
        const to = posAt(idx + text.length) ?? from + text.length;
        this.editor.chain().setTextSelection({from, to}).scrollIntoView().run();
        return true;
    }

    /** 程序化载入/还原内容（不置 edited，抑制假更新；edited/normalized 复位） */
    loadContent(markdown) {
        const json = parseMarkdown(this.editor, markdown);
        this._suppress = true;
        try {
            this.editor.commands.setContent(json, false);
        } finally {
            this._suppress = false;
        }
        // 同步/还原后回到 clean 基线：未编辑、规范化标志按新内容重算
        this.edited = false;
        try {
            this.normalized = normalizeEol(serializeMarkdown(this.editor)) !== normalizeEol(markdown);
        } catch {
            this.normalized = false;
        }
    }

    /**
     * 冻结当前选区为 opaque range handle（0.23.4 §3.4）。
     * 跨文本块范围 blockSafe=false（§3.7：Markdown 块边界不安全时只允许复制）。
     * @returns {{kind: string, from: number, to: number, text: string, blockSafe: boolean}|null}
     */
    createSelectionRangeHandle() {
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
     * 在当前文档文本中定位给定文本并冻结为 range handle（本次听写整理）。
     * 定位口径与 locateText 一致（text node 拼接，不含块分隔符）。
     * @param {string} text
     * @returns {{kind: string, from: number, to: number, text: string, blockSafe: boolean}|null}
     */
    createTextRangeHandle(text) {
        if (!text) return null;
        const {state} = this.editor;
        let full = "";
        /** @type {Array<{start: number, pos: number}>} */
        const spans = [];
        state.doc.descendants((node, pos) => {
            if (node.isText && node.text) {
                spans.push({start: full.length, pos});
                full += node.text;
            }
            return true;
        });
        const idx = full.indexOf(text);
        if (idx < 0) return null;
        const posAt = (ci) => {
            for (let i = spans.length - 1; i >= 0; i--) {
                if (spans[i].start <= ci) return spans[i].pos + (ci - spans[i].start);
            }
            return null;
        };
        const from = posAt(idx);
        if (from == null) return null;
        const to = posAt(idx + text.length) ?? from + text.length;
        return {
            kind: this.kind,
            from,
            to,
            text,
            blockSafe: MarkdownIrEngine._blockSafe(state, from, to),
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
     * 单事务替换 handle 范围（0.23.4 §3.7 确认应用路径）。
     * 仅接受 blockSafe 范围；先复核冻结文本，再一次事务完成替换——单行输出
     * 走 insertText（保持段落），多行输出按行拆段落（保持 Markdown 块结构）。
     * 校验失败返回 false，不产生任何修改。
     * @param {{from: number, to: number, text: string, blockSafe: boolean}} handle
     * @param {string} newText
     * @returns {boolean}
     */
    replaceRange(handle, newText) {
        if (!handle || typeof newText !== "string" || !handle.blockSafe) return false;
        const {from, to, text: expected} = handle;
        const {state} = this.editor;
        if (!Number.isInteger(from) || !Number.isInteger(to)
            || from < 0 || to > state.doc.content.size || from >= to) {
            return false;
        }
        if (state.doc.textBetween(from, to, "\n") !== expected) return false;

        const schema = state.schema;
        const lines = newText.split("\n");
        this.editor.chain().command(({tr}) => {
            if (lines.length === 1) {
                tr.insertText(lines[0], from, to);
            } else {
                const nodes = lines.map((line) =>
                    schema.nodes.paragraph.create(null, line ? schema.text(line) : null));
                tr.replaceWith(from, to, nodes);
            }
            return true;
        }).run();
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
