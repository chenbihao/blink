/**
 * SourceEngine（0.23.1）——textarea 背后的原文精确编辑引擎。
 *
 * Adapter 契约（phase 文档 §3.4）：文本快照、selection 文本、opaque range
 * handle、单事务替换、focus、change/selection 订阅、dispose。Source 视图
 * 不受 MD/Diff/AI 阈值限制，是任意文本的保真出口。
 */

/** textarea input 事件 → 用户可感知变更 */
export class SourceEngine {
    /** 引擎种类标识（range handle 前缀共用） */
    kind = "source";

    /**
     * @param {object} options
     * @param {HTMLTextAreaElement} options.element
     * @param {string} options.initialText
     * @param {() => void} [options.onChange]
     * @param {() => void} [options.onSelectionChange]
     */
    constructor({element, initialText, onChange, onSelectionChange}) {
        this.el = element;
        this.el.value = initialText;
        this.el.hidden = false;

        this._onChange = onChange ?? null;
        this._onSelectionChange = onSelectionChange ?? null;
        this._inputListener = () => this._onChange?.();
        this._selectListener = () => this._onSelectionChange?.();

        this.el.addEventListener("input", this._inputListener);
        // keyup/click 覆盖键盘与鼠标两种 selection 变化路径
        this.el.addEventListener("keyup", this._selectListener);
        this.el.addEventListener("click", this._selectListener);
    }

    /** 全文快照（UTF-8 真源，逐字符） */
    getText() {
        return this.el.value;
    }

    /** 当前选中文本；无选区返回空串 */
    getSelectionText() {
        const {selectionStart, selectionEnd} = this.el;
        if (selectionStart == null || selectionEnd == null) return "";
        return this.el.value.slice(selectionStart, selectionEnd);
    }

    /**
     * 单事务全文替换。优先走 execCommand("insertText")——WebView2 (Chromium)
     * 下它并入原生 undo 栈，一次 Ctrl+Z 恢复；execCommand 不可用时降级直写。
     * @param {string} text
     */
    replaceAll(text) {
        this.el.focus();
        this.el.setSelectionRange(0, this.el.value.length);
        let ok = false;
        try {
            ok = document.execCommand("insertText", false, text);
        } catch {
            ok = false;
        }
        if (!ok) this.el.value = text;
    }

    focus() {
        this.el.focus();
    }

    /** 完整清理：解绑监听、清空内容（reset 验收要求监听为空） */
    dispose() {
        this.el.removeEventListener("input", this._inputListener);
        this.el.removeEventListener("keyup", this._selectListener);
        this.el.removeEventListener("click", this._selectListener);
        this.el.value = "";
        this.el.hidden = true;
        this._onChange = null;
        this._onSelectionChange = null;
    }
}
