/**
 * 共享 Tiptap Markdown 编辑器封装（0.23.1）。
 *
 * 无 bundler 铁则下的 vendor IIFE（`window.BlinkTiptap`，esbuild 预打包，
 * 版本真源 frontend/vendor/VERSIONS.md，见 xtask/scripts/bundle-tiptap.js）。
 * 本模块收敛实例创建、Markdown parse/serialize 与 EOF 换行约定，
 * 供内容编辑器 MarkdownIrEngine 与便签等复用。
 */

/** vendor bundle 版本（与 xtask/scripts/bundle-tiptap.js 的 TIPTAP_VERSION 同步） */
export const TIPTAP_BUNDLE_VERSION = "3.29.2";

/** Tiptap bundle 是否已加载 */
export function isTiptapAvailable() {
    return typeof window !== "undefined" && !!window.BlinkTiptap;
}

/**
 * 创建 Markdown IR 编辑器实例。
 *
 * @param {object} options
 * @param {HTMLElement} options.element - 挂载容器
 * @param {string} options.initialMarkdown - 初始 Markdown 文本
 * @param {boolean} [options.editable=true]
 * @param {() => void} [options.onUpdate] - 用户可感知的内容变更（含粘贴）
 * @param {() => void} [options.onSelectionUpdate]
 * @param {() => void} [options.onTransaction] - 每次事务（工具栏状态刷新用）
 * @param {Function} [options.handlePaste] - 粘贴拦截（view, event）=> boolean
 * @returns {Editor} Tiptap Editor 实例
 */
export function createMarkdownEditor(options) {
    const {Editor, StarterKit, Markdown, TaskList, TaskItem} = window.BlinkTiptap;
    const extensions = [StarterKit, Markdown];
    // TaskList/TaskItem 在 bundle 中可能按需存在，缺省时降级
    if (TaskList) extensions.push(TaskList);
    if (TaskItem) extensions.push(TaskItem);

    const editor = new Editor({
        element: options.element,
        extensions,
        content: options.initialMarkdown,
        contentType: "markdown",
        editable: options.editable ?? true,
        editorProps: {
            attributes: {
                class: "content-editor-tiptap",
                spellcheck: "false",
            },
            handlePaste: options.handlePaste,
        },
    });

    if (options.onUpdate) editor.on("update", options.onUpdate);
    if (options.onSelectionUpdate) editor.on("selectionUpdate", options.onSelectionUpdate);
    if (options.onTransaction) editor.on("transaction", options.onTransaction);
    return editor;
}

/** Markdown 文本 → ProseMirror JSON（解析失败时抛错，由调用方降级） */
export function parseMarkdown(editor, markdown) {
    return editor.markdown.parse(markdown);
}

/** EOF 换行约定（§3.10 冻结）：serialize 不输出文末换行，由保存/文本路径统一补一个 */
export function ensureEofNewline(text) {
    return text.endsWith("\n") ? text : `${text}\n`;
}

/** \r\n → \n 归一化（载入侧） */
export function normalizeEol(text) {
    return (text ?? "").replace(/\r\n/g, "\n");
}

/**
 * 序列化为 Markdown 文本（含 EOF 换行约定）。
 * 序列化失败时抛错，由调用方决定降级路径。
 */
export function serializeMarkdown(editor) {
    return ensureEofNewline(editor.getMarkdown());
}
