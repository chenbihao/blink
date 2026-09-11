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
