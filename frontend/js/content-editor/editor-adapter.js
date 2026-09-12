/**
 * EditorAdapter（0.23.1）——Source/MD 双视图的唯一操作面。
 *
 * 职责（phase 文档 §3.3/§3.4）：
 * - 拥有当前 Engine（同一时刻只有一个），对外暴露引擎语义子集；
 * - **会话级原文检查点**：未发生 MD 编辑时切回 Source / 保存正文逐字符等于
 *   原文（Tiptap 规范化重写不落正文）；
 * - **视图切换**：MD 风险门拦截危险结构；切换即离开旧引擎（无跨引擎
 *   anchor 迁移），正文经检查点/序列化衔接；
 * - **content_revision**：单调计数，任何用户可感知变更 +1。
 *
 * 不暴露 DOM Range / ProseMirror position / 通用 offset。
 */

import {SourceEngine} from "./engines/source-engine.js";
import {MarkdownIrEngine} from "./engines/markdown-engine.js";
import {evaluateMarkdownGate} from "./engines/markdown-gate.js";
import {normalizeEol} from "../shared/tiptap-editor.js";

export class EditorAdapter {
    /** 当前视图："source" | "markdown" | null（未载入） */
    view = null;

    /** 当前引擎实例 */
    engine = null;

    /** 会话级原文检查点（§3.3）——未编辑 MD 时正文的逐字符真值 */
    checkpoint = "";

    /** 来源的 Markdown 视图策略（快照下发："disabled"|"available"|"preferred"） */
    markdownPolicy = "available";

    /** 风险门结果（loadInitial 时计算并缓存） */
    gate = null;

    /** 单调内容版本 */
    revision = 0;

    /**
     * @param {object} deps
     * @param {HTMLTextAreaElement} deps.sourceEl
     * @param {HTMLElement} deps.mdContainerEl
     * @param {HTMLElement|null} deps.mdToolbarEl
     * @param {object} callbacks
     * @param {() => void} [callbacks.onContentChanged] - 用户可感知变更（revision 自增）
     * @param {(view: "source"|"markdown") => void} [callbacks.onViewChanged]
     * @param {(reasonKey: string) => void} [callbacks.onNotice] - 风险门/降级提示
     * @param {object} [engineFactories] - 引擎工厂（测试注入 fake 用）
     * @param {(opts: object) => object} [engineFactories.source]
     * @param {(opts: object) => object} [engineFactories.markdown]
     */
    constructor({sourceEl, mdContainerEl, mdToolbarEl}, callbacks = {}, engineFactories = {}) {
        this.sourceEl = sourceEl;
        this.mdContainerEl = mdContainerEl;
        this.mdToolbarEl = mdToolbarEl;
        this._callbacks = callbacks;
        this._factories = {
            source: engineFactories.source
                ?? ((opts) => new SourceEngine(opts)),
            markdown: engineFactories.markdown
                ?? ((opts) => new MarkdownIrEngine(opts)),
        };
    }

    /**
     * 载入会话初始内容：完整 reset 后按策略 + 风险门决定初始视图。
     * preferred + 门通过 + 尺寸正常 → 默认 MD；其余默认 Source。
     */
    loadInitial({body, markdownPolicy}) {
        this.reset();
        const text = normalizeEol(body);
        this.checkpoint = text;
        this.markdownPolicy = markdownPolicy ?? "available";
        this.gate = evaluateMarkdownGate(text);

        const autoEnter = this.markdownPolicy === "preferred"
            && this.gate.allowed && this.gate.autoEnter;
        this._enterView(autoEnter ? "markdown" : "source", {initialText: text, initial: true});

        // 提示（弱内容检测只提示，不自动改变视图）
        if (!this.gate.allowed) {
            this._callbacks.onNotice?.(
                this.gate.reason === "large" ? "editor.gate.large" : "editor.gate.rejected",
            );
        } else if (this.gate.sizeWarn && this.markdownPolicy === "preferred") {
            this._callbacks.onNotice?.("editor.gate.slow");
        }
    }

    /**
     * 切换视图。MD 被风险门/策略拒绝时返回 false 并提示，不切换。
     * 切换会放弃当前引擎（无 anchor 迁移）；切换视图前调用方应先处理
     * 运行中的异步候选（0.23.4 起生效）。
     *
     * 门结果按**当前正文**实时重估：用户在 Source 中新编辑出表格/公式等
     * 危险结构时，切 MD 必须被拦截（载入时的 gate 已过期）。
     * @param {"source"|"markdown"} target
     */
    switchView(target) {
        if (target === this.view || !this.view) return true;
        if (target === "markdown") {
            if (this.markdownPolicy === "disabled") {
                this._callbacks.onNotice?.("editor.gate.rejected");
                return false;
            }
            this.gate = evaluateMarkdownGate(this.getText());
            if (!this.gate.allowed) {
                this._callbacks.onNotice?.(
                    this.gate.reason === "large" ? "editor.gate.large" : "editor.gate.rejected",
                );
                return false;
            }
        }
        const outgoingText = this.getText();
        this._enterView(target, {initialText: outgoingText});
        return true;
    }

    /**
     * 当前正文。MD 未编辑时返回检查点（逐字符原文）；MD 已编辑返回序列化
     * 文本（EOF 换行约定）；Source 返回 textarea 实时值。
     */
    getText() {
        if (!this.engine) return this.checkpoint;
        if (this.view === "markdown" && !this.engine.edited) return this.checkpoint;
        return this.engine.getText();
    }

    /** dirty 判定：当前正文 ≠ 检查点。未编辑的 MD 视图恒为 clean。 */
    isDirty() {
        if (!this.engine || !this.view) return false;
        return this.getText() !== this.checkpoint;
    }

    /** MD 载入是否被规范化重写（提示"已按编辑器规范重写"用） */
    isNormalizedMd() {
        return this.view === "markdown" && !!this.engine?.normalized;
    }

    /** 检查点前移（提交成功 / 外部同步后由 EditorSession 调用） */
    setCheckpoint(text) {
        this.checkpoint = text;
    }

    /** 单事务全文替换当前视图（0.23.4 AI 应用与外部同步共用） */
    replaceAll(text) {
        if (!this.engine) return;
        this.engine.replaceAll(text);
        this.revision += 1;
    }

    /**
     * 听写追尾（0.23.3 §3.6）：confirmed segment 追加到最新文末。
     * newParagraph=true 时先补段落分隔（首段语义）；revision 由引擎
     * onChange 回调自增（追加是真实编辑，计入正文/dirty/undo）。
     * @param {string} text
     * @param {{newParagraph?: boolean}} [opts]
     */
    appendDictation(text, {newParagraph = false} = {}) {
        if (!this.engine || !text) return;
        if (newParagraph && this.engine.appendParagraph) {
            this.engine.appendParagraph(text);
        } else {
            this.engine.appendText(text);
        }
    }

    /**
     * 在当前视图中选中给定文本并滚动到可见（"定位到本次听写"）。
     * @param {string} text
     * @returns {boolean} 是否找到
     */
    locateText(text) {
        if (!this.engine || !text) return false;
        return this.engine.locateText?.(text) ?? false;
    }

    // ── 0.23.4 AI 整理范围（§3.4 冻结三元组的 Engine 侧）────────────────────

    /**
     * 冻结整理范围为 opaque range handle。
     * scope="selection" 取当前选区；scope="dictation" 在全文定位给定文本。
     * handle 由当前 Engine 签发（含冻结文本与块安全标志），仅当前 Engine
     * 可解析与校验；视图切换后旧 handle 随引擎丢弃。
     * @param {"selection"|"dictation"} scope
     * @param {string} [text] - dictation 范围文本（本次听写拼接）
     * @returns {{handle: object, text: string, blockSafe: boolean}|null} 范围不存在/为空返回 null
     */
    freezeRange(scope, text) {
        if (!this.engine) return null;
        const frozen = scope === "selection"
            ? this.engine.createSelectionRangeHandle?.() ?? null
            : this.engine.createTextRangeHandle?.(text) ?? null;
        if (!frozen || !frozen.text) return null;
        return {handle: frozen, text: frozen.text, blockSafe: frozen.blockSafe ?? false};
    }

    /**
     * 单事务替换冻结范围（§3.7 确认应用路径）。Engine 复核冻结文本通过后
     * 替换；revision 由引擎 onChange 回调自增（替换是真实编辑，计入 undo）。
     * @param {object} handle - freezeRange 返回的 handle
     * @param {string} newText
     * @returns {boolean} 校验失败或引擎不支持返回 false（不产生修改）
     */
    replaceRange(handle, newText) {
        if (!this.engine?.replaceRange) return false;
        return this.engine.replaceRange(handle, newText);
    }

    /** 外部同步内容（便签变更 reload）：替换正文且前移检查点，不计用户编辑 */
    syncFromExternal(text) {
        const next = normalizeEol(text);
        if (!this.engine || !this.view) {
            this.checkpoint = next;
            return;
        }
        if (this.view === "source") {
            this.engine.replaceAll(next);
        } else {
            this.engine.loadContent(next);
        }
        this.checkpoint = next;
    }

    getSelectionText() {
        return this.engine?.getSelectionText() ?? "";
    }

    focus() {
        this.engine?.focus();
    }

    /**
     * 完整 reset（§3.1/§6.2）：正文、undo、selection、引擎监听、检查点、
     * 门结果、revision 全部清空。引擎实例销毁保证 Tiptap 侧无残留。
     */
    reset() {
        if (this.engine) {
            this.engine.dispose();
            this.engine = null;
        }
        this.view = null;
        this.checkpoint = "";
        this.gate = null;
        this.revision = 0;
    }

    // ── 内部 ────────────────────────────────────────────────────────────────

    _bumpRevision() {
        this.revision += 1;
        this._callbacks.onContentChanged?.();
    }

    _enterView(target, {initialText}) {
        const engineCallbacks = () => ({
            onChange: () => this._bumpRevision(),
            // 选区变化通知（0.23.4：整理选中入口的可见性跟随选区）
            onSelectionChange: () => this._callbacks.onSelectionChanged?.(),
        });
        if (this.engine) {
            this.engine.dispose();
            this.engine = null;
        }
        this.view = target;

        if (target === "source") {
            this.mdContainerEl.hidden = true;
            if (this.mdToolbarEl) this.mdToolbarEl.hidden = true;
            this.engine = this._factories.source({
                element: this.sourceEl,
                initialText,
                ...engineCallbacks(),
            });
        } else {
            // Tiptap 不可用或初始化失败 → 降级 Source（保留文本，提示）
            try {
                if (this.mdToolbarEl) this.mdToolbarEl.hidden = false;
                this.engine = this._factories.markdown({
                    element: this.mdContainerEl,
                    toolbarMount: this.mdToolbarEl,
                    initialMarkdown: initialText,
                    ...engineCallbacks(),
                });
                this.sourceEl.hidden = true;
            } catch (e) {
                console.error("[editor-adapter] MD 引擎创建失败，降级 Source:", e);
                this.view = "source";
                if (this.mdToolbarEl) this.mdToolbarEl.hidden = true;
                this.engine = this._factories.source({
                    element: this.sourceEl,
                    initialText,
                    ...engineCallbacks(),
                });
                this._callbacks.onNotice?.("editor.gate.fallback");
            }
        }
        this._callbacks.onViewChanged?.(this.view);
    }
}
