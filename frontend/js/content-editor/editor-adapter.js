/**
 * EditorAdapter（0.23.1；本次修复改为 canonical buffer 单一真源）——Source/MD 双视图唯一操作面。
 *
 * ## 真源与职责
 *
 * - **会话正文唯一真源 = `CanonicalBuffer`**（canonical-buffer.js）。
 *   任何视图切换、保存、草稿持久化都只读它；**不再**从引擎取全文。
 * - Source 视图：textarea 是缓冲区的直接编辑面（逐字符等价）。
 * - MD 视图：缓冲区是文档的投影。用户编辑后由 `MarkdownIrEngine.takeSourcePatch`
 *   产出**最小块级 patch**，只改写变更窗口对应的源文区间；未编辑区间逐字节保留。
 *   正文**延迟物化**（在 getText / 切换 / 保存时才算），避免每次按键重算整篇。
 * - **安全降级**：风险门拒绝的结构或块对齐验证失败 → MD 仍可**预览**，
 *   但 `readOnly`，禁止富文本编辑（不产出任何 patch），并由 `onNotice` 明确提示原因。
 *
 * ## content_revision
 *
 * 由缓冲区持有并单调自增（任何用户可感知变更 +1）；MD 编辑在事务到达时先
 * `markChanged()`（正文延迟物化），保证提交协议与草稿 revision 水位单调。
 *
 * 不跨 IPC 暴露 DOM Range / ProseMirror position / 通用 offset。
 */

import {SourceEngine, dictationGapPrefix} from "./engines/source-engine.js";
import {MarkdownIrEngine} from "./engines/markdown-engine.js";
import {evaluateMarkdownGate} from "./engines/markdown-gate.js";
import {CanonicalBuffer} from "./canonical-buffer.js";

export class EditorAdapter {
    /** 当前视图："source" | "markdown" | null（未载入） */
    view = null;

    /** 当前引擎实例 */
    engine = null;

    /** 会话正文唯一真源（UTF-8 源文缓冲区） */
    buffer = new CanonicalBuffer();

    /** 已发布/权威基线正文（提交成功或外部同步后前移）——dirty 判定基准 */
    checkpoint = "";

    /** 来源的 Markdown 视图策略（快照下发："disabled"|"available"|"preferred"） */
    markdownPolicy = "available";

    /** 风险门结果（进入 MD 前按当前正文实时重估） */
    gate = null;

    /** 本轮听写追加锚点（0.23.6 §5.7）：{engineKind, fromChar, extended}；无听写为 null */
    _dictationRun = null;

    /** 生命周期/退出落盘临界区：冻结所有正文交互，避免验证后又有新输入。 */
    _interactionLocked = false;

    /**
     * @param {object} deps
     * @param {HTMLTextAreaElement} deps.sourceEl
     * @param {HTMLElement} deps.mdContainerEl
     * @param {HTMLElement|null} deps.mdToolbarEl
     * @param {object} callbacks
     * @param {() => void} [callbacks.onContentChanged] - 用户可感知变更
     * @param {(view: "source"|"markdown") => void} [callbacks.onViewChanged]
     * @param {(reasonKey: string) => void} [callbacks.onNotice] - 风险门/降级提示
     * @param {() => void} [callbacks.onMdModeChanged] - MD 可编辑性变化（只读降级）
     * @param {object} [engineFactories] - 引擎工厂（测试注入 fake 用）
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

    /** 单调内容版本（由缓冲区持有） */
    get revision() {
        return this.buffer.revision;
    }

    /** Canonical checkpoint captured from the authoritative source. */
    get checkpointText() {
        return this.checkpoint;
    }

    /**
     * 载入会话初始内容：完整 reset 后按策略 + 风险门决定初始视图。
     * preferred + 门通过 + 尺寸正常 → 默认 MD；其余默认 Source。
     */
    loadInitial({body, markdownPolicy}) {
        this.reset();
        // canonical source is an opaque JavaScript string.  Markdown may use a
        // normalized projection internally, but the source/checkpoint must not
        // silently change CRLF, mixed EOL, or the EOF newline.
        const text = typeof body === "string" ? body : "";
        this.buffer.load(text);
        this.checkpoint = text;
        this.markdownPolicy = markdownPolicy ?? "available";
        this.gate = evaluateMarkdownGate(text);

        const autoEnter = this.markdownPolicy === "preferred"
            && this.gate.allowed && this.gate.autoEnter;
        this._enterView(autoEnter ? "markdown" : "source", {initialText: text, initial: true});

        if (!this.gate.allowed) {
            // 弱内容/超限：不自动进入 MD，但允许用户手动进入只读预览
            this._callbacks.onNotice?.(
                this.gate.reason === "large" ? "editor.gate.large" : "editor.gate.readonly",
            );
        } else if (this.gate.sizeWarn && this.markdownPolicy === "preferred") {
            this._callbacks.onNotice?.("editor.gate.slow");
        }
        if (this.isMdReadOnly()) {
            this._callbacks.onNotice?.("editor.md.readOnly");
        }
    }

    /**
     * 切换视图。
     *
     * - `disabled` 策略：拒绝进入 MD；
     * - 风险门拒绝的结构 / 块对齐验证失败：**仍进入 MD 预览，但只读**
     *   （安全降级：允许预览、禁止富文本编辑，并给出明确原因）；
     * - 切换不再取引擎文本：正文来自 canonical buffer，因此**未编辑时逐字节不变**。
     *
     * @param {"source"|"markdown"} target
     * @returns {boolean} 是否完成了视图切换
     */
    switchView(target) {
        if (this._interactionLocked) return false;
        if (target === this.view || !this.view) return true;

        if (target === "markdown") {
            if (this.markdownPolicy === "disabled") {
                this._callbacks.onNotice?.("editor.gate.rejected");
                return false;
            }
            // 先物化 Source/前一次 MD 的编辑，保证门与块映射基于最新正文
            const current = this.getText();
            this.gate = evaluateMarkdownGate(current);
            if (this.gate.reason === "large") {
                // 尺寸门是**性能门**（不是无损门）：超限连只读预览也不进入，
                // 否则整篇解析会把窗口卡死（perf 报告：128KB ≈ 0.5s、512KB ≈ 7.5s）。
                this._callbacks.onNotice?.("editor.gate.large");
                return false;
            }
            this._enterView("markdown", {initialText: current});
            if (!this.gate.allowed) {
                // 结构门是**无损门**：安全降级——允许预览、禁止富文本编辑，并说明原因
                this._callbacks.onNotice?.("editor.gate.readonly");
            } else if (this.isMdReadOnly()) {
                this._callbacks.onNotice?.("editor.md.readOnly");
            }
            return true;
        }

        const outgoingText = this.getText();
        this._enterView("source", {initialText: outgoingText});
        return true;
    }

    /**
     * MD 视图是否可达。
     * 结构不可无损编辑时仍可只读预览；策略禁用与超尺寸（性能门）时不可达。
     */
    mdReachable() {
        if (this.markdownPolicy === "disabled") return false;
        if (this.gate && !this.gate.allowed && this.gate.reason === "large") return false;
        return true;
    }

    /**
     * 当前正文（canonical 真源）。
     * MD 视图下先物化延迟的块级 patch，再返回缓冲区文本。
     */
    getText() {
        this._materializeMd();
        return this.buffer.text;
    }

    /**
     * dirty 判定。
     * 廉价路径：MD 有待物化的文档变更即为 dirty（不做整篇重算）；
     * 否则比较缓冲区与权威基线。
     */
    isDirty() {
        if (this.view === "markdown" && this.engine?.hasPendingSourcePatch?.()) return true;
        return this.buffer.text !== this.checkpoint;
    }

    /** MD 载入是否被规范化重写（提示"已按编辑器规范重写"用） */
    isNormalizedMd() {
        return this.view === "markdown" && !!this.engine?.normalized;
    }

    /** 当前是否处于只读 MD 预览（无法保证无损修改的结构） */
    isMdReadOnly() {
        return this.view === "markdown" && !!this.engine?.readOnly;
    }

    /** 当前 MD 视图是否可编辑（富文本编辑可用） */
    isMdEditable() {
        return this.view === "markdown" && !!this.engine
            && !this.engine.readOnly && !this._interactionLocked;
    }

    /** 冻结/恢复正文交互，用于验证式落盘后的生命周期临界区。 */
    setInteractionLocked(locked) {
        this._interactionLocked = locked === true;
        if (this.sourceEl) this.sourceEl.readOnly = this._interactionLocked;
        if (this.engine?.kind === "markdown" && this.engine.editor?.setEditable) {
            this.engine.editor.setEditable(!this._interactionLocked && !this.engine.readOnly);
        }
        if (this.mdToolbarEl) {
            this.mdToolbarEl.inert = this._interactionLocked;
            this.mdToolbarEl.setAttribute?.("aria-disabled", String(this._interactionLocked));
        }
        this._callbacks.onMdModeChanged?.();
    }

    /** MD 只读原因 key（供 UI 提示）：`"align"` / `"forced"` / `null` */
    mdReadOnlyReason() {
        return this.view === "markdown" ? (this.engine?.readOnlyReason ?? null) : null;
    }

    /**
     * 是否属于"大文档 MD"（32–128KB 区间）：可进但卡顿，
     * 高频草稿保存不做整篇物化（只在关键边界 flush）。
     */
    isHeavyMd() {
        return this.view === "markdown" && !!this.gate?.sizeWarn;
    }

    /** 检查点前移（提交成功 / 外部同步后由 EditorSession 调用） */
    setCheckpoint(text) {
        this.checkpoint = text;
    }

    /** 程序化全文替换（外部同步 / AI 全文应用）。只读预览下拒绝。 */
    replaceAll(text) {
        if (!this.engine || this._interactionLocked) return false;
        const next = typeof text === "string" ? text : "";
        const ok = this.engine.replaceAll ? this.engine.replaceAll(next) : false;
        if (!ok) return false;
        this.buffer.load(next);
        this.buffer.markChanged();
        return true;
    }

    /**
     * 听写追尾（0.23.3 §3.6）：confirmed segment 追加到最新文末。
     * 只读 MD 预览下拒绝（返回 false，调用方提示并停止听写）。
     * @param {string} text
     * @param {{newParagraph?: boolean}} [opts]
     * @returns {boolean} 是否已写入
     */
    appendDictation(text, {newParagraph = false} = {}) {
        if (!this.engine || !text || this.engine.readOnly || this._interactionLocked) return false;
        if (newParagraph && this.engine.appendParagraph) {
            const run = this._dictationRun;
            if (run && !run.extended && run.engineKind === "source" && this.engine.kind === "source") {
                run.fromChar += dictationGapPrefix(this.engine.getText()).length;
            }
            if (run) run.extended = true;
            return this.engine.appendParagraph(text) !== false;
        }
        if (this._dictationRun) this._dictationRun.extended = true;
        return this.engine.appendText(text) !== false;
    }

    // ── 本轮听写范围（0.23.6 §5.7：真实追加锚点，替代全文 indexOf 猜测）────

    /**
     * 记录本轮听写追加锚点（听写开始时调用）。锚点 = 当前引擎的文末字符偏移。
     */
    beginDictationRun() {
        if (!this.engine || this._interactionLocked) {
            this._dictationRun = null;
            return;
        }
        this._dictationRun = {
            engineKind: this.engine.kind,
            fromChar: this.engine.tailCharLength?.() ?? 0,
            extended: false,
        };
    }

    /**
     * 冻结本轮听写范围为 opaque range handle（听写结束时调用）。
     */
    freezeDictationRun() {
        const run = this._dictationRun;
        if (!run || !this.engine || this.engine.kind !== run.engineKind) return null;
        const handle = this.engine.createTailRangeHandle?.(run.fromChar) ?? null;
        if (!handle || !handle.text) return null;
        return {handle, text: handle.text, blockSafe: handle.blockSafe ?? false};
    }

    /**
     * 选中并滚动到冻结的本轮听写范围（"定位到本次听写"）。
     */
    locateDictationRun(frozen) {
        if (!frozen?.handle || !this.engine) return false;
        if (this.engine.kind !== frozen.handle.kind) return false;
        return this.engine.locateRange?.(frozen.handle) ?? false;
    }

    /**
     * 在当前视图中选中给定文本并滚动到可见。
     */
    locateText(text) {
        if (!this.engine || !text) return false;
        return this.engine.locateText?.(text) ?? false;
    }

    // ── 0.23.4 AI 整理范围（§3.4 冻结三元组的 Engine 侧）────────────────────

    /**
     * 冻结整理范围为 opaque range handle。只读 MD 预览下返回 null
     * （整理结果无法确认替换，不签发 handle）。
     */
    freezeRange(scope, text) {
        if (!this.engine || this.engine.readOnly || this._interactionLocked) return null;
        const frozen = scope === "selection"
            ? this.engine.createSelectionRangeHandle?.() ?? null
            : this.engine.createTextRangeHandle?.(text) ?? null;
        if (!frozen || !frozen.text) return null;
        return {handle: frozen, text: frozen.text, blockSafe: frozen.blockSafe ?? false};
    }

    /**
     * 单事务替换冻结范围（§3.7 确认应用路径）。
     */
    replaceRange(handle, newText) {
        if (!this.engine?.replaceRange || this._interactionLocked) return false;
        return this.engine.replaceRange(handle, newText);
    }

    /** 外部同步内容（便签变更 reload）：替换正文且前移检查点，不计用户编辑 */
    syncFromExternal(text) {
        if (this._interactionLocked) return false;
        const next = typeof text === "string" ? text : "";
        if (!this.engine || !this.view) {
            this.buffer.load(next);
        } else {
            if (this.view === "source") {
                this.engine.replaceAll(next);
            } else {
                this.engine.loadContent(next);
            }
            this.buffer.load(next);
        }
        this.checkpoint = next;
        // 程序化内容替换同样必须推进内容版本：否则下一次显式提交会被提交协议
        // 按「同 revision、不同正文」判定为 StaleRevision（正文已变但版本未动）。
        this.buffer.markChanged();
        this._callbacks.onMdModeChanged?.();
        return true;
    }

    /**
     * 恢复未保存草稿（崩溃恢复）：只替换**工作正文**，不动检查点——
     * 会话因此保持 dirty，等待用户显式保存或放弃（不静默改写任何保存目标）。
     * @param {string} text
     * @returns {boolean} 是否已恢复
     */
    restoreDraft(text) {
        if (!this.engine || !this.view || this._interactionLocked) return false;
        const next = typeof text === "string" ? text : "";
        if (this.view === "source") {
            this.engine.replaceAll(next);
        } else {
            this.engine.loadContent(next);
        }
        this.buffer.load(next);
        // A recovered body is a user-visible working edit even when it is the
        // empty string.  Advance the content revision so a later commit/draft
        // flush cannot be mistaken for the original clean session.
        this.buffer.markChanged();
        this._callbacks.onMdModeChanged?.();
        return true;
    }

    getSelectionText() {
        return this.engine?.getSelectionText() ?? "";
    }

    focus() {
        this.engine?.focus();
    }

    /**
     * 完整 reset（§3.1/§6.2）：正文、undo、selection、监听、检查点、
     * 门结果、revision、听写范围锚点全部清空。引擎实例销毁保证 Tiptap 侧无残留。
     */
    reset() {
        if (this.engine) {
            this.engine.dispose();
            this.engine = null;
        }
        this.view = null;
        this.buffer.reset();
        this.checkpoint = "";
        this.gate = null;
        this._dictationRun = null;
        this._interactionLocked = false;
        if (this.sourceEl) this.sourceEl.readOnly = false;
        if (this.mdToolbarEl) {
            this.mdToolbarEl.inert = false;
            this.mdToolbarEl.setAttribute?.("aria-disabled", "false");
        }
    }

    // ── 内部 ────────────────────────────────────────────────────────────────

    /**
     * MD 延迟物化：把文档变更以**最小块级 patch** 落到 canonical buffer。
     * - 无变更 / 只读预览 → 不动；
     * - 块级无损成立 → 只改变更窗口；
     * - 无法保证无损（罕见结构）→ 引擎撤销本次富文本投影并进入只读，
     *   canonical buffer 不发生任何改变。
     */
    _materializeMd() {
        if (this.view !== "markdown" || !this.engine) return;
        const result = this.engine.takeSourcePatch?.(this.buffer.text);
        if (!result) return;
        if (result.exact !== true || typeof result.text !== "string") {
            // `exact:false` is a rejection, never a candidate正文.  The engine
            // has already rebuilt its projection from canonical source and made
            // itself read-only; keep this buffer/checkpoint untouched.
            this._callbacks.onNotice?.("editor.md.patchRejected");
            this._callbacks.onMdModeChanged?.();
            return;
        }
        this.buffer.load(result.text);
        this._callbacks.onMdModeChanged?.();
    }

    /** 引擎变更回调：Source 同步文本，MD 只推进内容版本（正文延迟物化） */
    _handleEngineChange() {
        if (this.view === "source" && this.engine?.kind === "source") {
            const changed = this.buffer.setFromSource(this.engine.getText());
            if (!changed) return;
        } else {
            this.buffer.markChanged();
        }
        this._callbacks.onContentChanged?.();
    }

    _enterView(target, {initialText}) {
        // 听写锚点随旧引擎作废（§3.3 无跨引擎 anchor 迁移）；听写进行中时
        // 由 main.switchView 在切换后重新 beginDictationRun。
        this._dictationRun = null;
        const engineCallbacks = () => ({
            onChange: () => this._handleEngineChange(),
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
            // 门拒绝 → 只读预览（仍在 MD 视图渲染，但不允许富文本编辑）
            const editable = this.gate?.allowed !== false;
            try {
                if (this.mdToolbarEl) this.mdToolbarEl.hidden = !editable;
                this.engine = this._factories.markdown({
                    element: this.mdContainerEl,
                    toolbarMount: editable ? this.mdToolbarEl : null,
                    initialMarkdown: initialText,
                    editable,
                    ...engineCallbacks(),
                });
                this.sourceEl.hidden = true;
                if (this.engine.readOnly && this.mdToolbarEl) this.mdToolbarEl.hidden = true;
            } catch (e) {
                console.error("[editor-adapter] MD 引擎创建失败，降级 Source:", e);
                this.view = "source";
                if (this.mdToolbarEl) this.mdToolbarEl.hidden = true;
                this.mdContainerEl.hidden = true;
                this.engine = this._factories.source({
                    element: this.sourceEl,
                    initialText,
                    ...engineCallbacks(),
                });
                this._callbacks.onNotice?.("editor.gate.fallback");
            }
        }
        this.setInteractionLocked(this._interactionLocked);
        this._callbacks.onViewChanged?.(this.view);
    }
}
