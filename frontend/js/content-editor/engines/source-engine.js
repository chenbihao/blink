/**
 * SourceEngine（0.23.1）——textarea 背后的原文精确编辑引擎。
 *
 * Adapter 契约（phase 文档 §3.4）：文本快照、selection 文本、opaque range
 * handle、单事务替换、focus、change/selection 订阅、dispose。Source 视图
 * 不受 MD/Diff/AI 阈值限制，是任意文本的保真出口。
 */

/**
 * 首段段落分隔前缀（0.23.3 §3.6：首段按需补一个段落分隔）。
 * 纯函数导出供测试。空文本不补；已有空行不补；单个换行补一个换行；
 * 其余补空行（\n\n）。
 * @param {string} currentText
 * @returns {string}
 */
export function dictationGapPrefix(currentText) {
    if (!currentText) return "";
    if (currentText.endsWith("\n\n")) return "";
    if (currentText.endsWith("\n")) return "\n";
    return "\n\n";
}

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

    /**
     * 文末追加（0.23.3 听写追尾，单事务）。聚焦时走 execCommand 并入原生
     * undo 栈；未聚焦时直写并手动通知——**不抢焦点**（失焦期间听写继续）。
     * 追加后恢复用户此前 selection；选区原在文末时自然跟随到新文末。
     * @param {string} text
     */
    appendText(text) {
        if (!text) return;
        const el = this.el;
        const prevStart = el.selectionStart;
        const prevEnd = el.selectionEnd;
        const prevLen = el.value.length;
        const atEnd = prevStart >= prevLen && prevEnd >= prevLen;

        if (document.activeElement === el) {
            el.setSelectionRange(prevLen, prevLen);
            let ok = false;
            try {
                ok = document.execCommand("insertText", false, text);
            } catch {
                ok = false;
            }
            if (!ok) {
                el.value += text;
                this._onChange?.();
            }
            if (!atEnd) el.setSelectionRange(prevStart, prevEnd);
            return;
        }

        // 未聚焦：直写（value 赋值会把 caret 移到末尾，需恢复）
        el.value = el.value + text;
        this._onChange?.();
        if (!atEnd) el.setSelectionRange(prevStart, prevEnd);
    }

    /**
     * 文末新起一段追加（0.23.3 首段语义）：按需补段落分隔后追加。
     * @param {string} text
     */
    appendParagraph(text) {
        if (!text) return;
        this.appendText(dictationGapPrefix(this.el.value) + text);
    }

    /**
     * 在当前视图中选中给定文本并滚动到可见（"定位到本次听写"）。
     * @param {string} text
     * @returns {boolean} 是否找到并选中
     */
    locateText(text) {
        if (!text) return false;
        const idx = this.el.value.indexOf(text);
        if (idx < 0) return false;
        this.el.focus();
        this.el.setSelectionRange(idx, idx + text.length);
        // 近似滚动使选区进入视口（textarea 不会自动滚动到 setSelectionRange 处）
        const line = this.el.value.substring(0, idx).split("\n").length - 1;
        const lineHeight = parseFloat(getComputedStyle(this.el).lineHeight) || 20;
        const targetTop = line * lineHeight;
        const viewTop = this.el.scrollTop;
        if (targetTop < viewTop || targetTop > viewTop + this.el.clientHeight - lineHeight) {
            this.el.scrollTop = Math.max(0, targetTop - this.el.clientHeight / 2);
        }
        return true;
    }

    /**
     * 冻结当前选区为 opaque range handle（0.23.4 §3.4 整理请求）。
     * handle 携带 {kind, start, end} 锚点 + 冻结文本；Source 视图恒 blockSafe。
     * @returns {{kind: string, start: number, end: number, text: string, blockSafe: boolean}|null}
     */
    createSelectionRangeHandle() {
        const {selectionStart, selectionEnd} = this.el;
        if (selectionStart == null || selectionEnd <= selectionStart) return null;
        return {
            kind: this.kind,
            start: selectionStart,
            end: selectionEnd,
            text: this.el.value.slice(selectionStart, selectionEnd),
            blockSafe: true,
        };
    }

    /**
     * 在当前正文中定位给定文本并冻结为 range handle（本次听写整理）。
     * @param {string} text
     * @returns {{kind: string, start: number, end: number, text: string, blockSafe: boolean}|null}
     */
    createTextRangeHandle(text) {
        if (!text) return null;
        const idx = this.el.value.indexOf(text);
        if (idx < 0) return null;
        return {kind: this.kind, start: idx, end: idx + text.length, text, blockSafe: true};
    }

    /**
     * 单事务替换 handle 范围（0.23.4 §3.7 确认应用路径）。
     * Engine 先复核冻结文本仍在锚点处（§3.4 range handle 校验），再走
     * execCommand 并入原生 undo 栈——一次 Ctrl+Z 恢复；校验失败返回 false，
     * 不产生任何修改。
     * @param {{start: number, end: number, text: string}} handle
     * @param {string} newText
     * @returns {boolean}
     */
    replaceRange(handle, newText) {
        if (!handle || typeof newText !== "string") return false;
        const {start, end, text: expected} = handle;
        const current = this.el.value;
        if (
            !Number.isInteger(start) || !Number.isInteger(end)
            || start < 0 || end > current.length || start >= end
            || current.slice(start, end) !== expected
        ) {
            return false;
        }
        this.el.focus();
        this.el.setSelectionRange(start, end);
        let ok = false;
        try {
            ok = document.execCommand("insertText", false, newText);
        } catch {
            ok = false;
        }
        if (!ok) {
            // 降级直写：手动通知（input 事件不会触发）
            this.el.value = current.slice(0, start) + newText + current.slice(end);
            this._onChange?.();
        }
        return true;
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
