/**
 * EditorTransformController（0.23.4）——编辑器 AI 整理的前端状态机。
 *
 * 职责（phase 文档 §3.7 / §6.5）：
 * - 两个显式入口：整理选中内容（当前选区）/ 整理本次听写（听写拼接文本定位）；
 * - 启动冻结：session 身份 + scope + Engine range handle + content_revision，
 *   经后端全局单活跃协调（`ai_already_active` 携带活跃窗口）；
 * - 候选只来自完成事件：按身份 + requestId 过滤，未经确认不触碰正文；
 * - stale 判定：请求后任何正文变化（revision ≠ 冻结值）→ 只允许复制或
 *   重新整理，应用按钮禁用；
 * - 确认应用：Adapter 单事务替换（Engine 复核冻结文本），一次 Ctrl+Z 恢复；
 *   MD 块边界不安全（blockSafe=false）时仅允许复制（§3.7）；
 * - 视图切换/新请求/会话结束/窗口关闭取消运行请求并丢弃候选（由 main.js 接线）；
 * - 候选卡为非模态卡片，DOM 注入本模块（纯逻辑可直接测试）。
 *
 * phase 与 §3.2 transform 切片对应：idle / running / candidate（stale 为
 * candidate 的正交标志，不单独成态）。
 */

import {normalizeError} from "../shared/tauri.js";
import {EVENTS} from "../shared/event-names.js";
import {t} from "../i18n/index.js";
import {computeDiff, diffToHtml, hasChanges} from "./diff.js";

export class EditorTransformController {
    /** @type {"idle"|"running"|"candidate"} */
    phase = "idle";

    /** 运行中请求 id（start 返回后回填；null = 尚未确认启动） */
    runRequestId = null;

    /** 运行中请求的冻结信息（完成事件回流时构建候选） */
    pendingRun = null;

    /** 待确认候选 */
    candidate = null;

    /** AI 是否可用（未配置时隐藏整理入口，§3.8） */
    aiAvailable = true;

    /**
     * @param {object} deps
     * @param {*} deps.api - shared/api.js 子集（测试注入 fake）：
     *   { startEditorTransform, cancelEditorTransform }
     * @param {*} deps.adapter - EditorAdapter 实例（freezeRange/replaceRange/revision）
     * @param {() => {isActive: boolean, sessionRef: string|null, generation: number}} deps.getSession
     * @param {*} deps.listen - tauri listen（测试注入 fake）
     * @param {(text: string) => Promise<*>} deps.copyToClipboard
     * @param {object} [deps.el] - 候选卡 DOM（缺省 = 无头模式，仅状态）
     * @param {HTMLElement|null} [deps.el.card] [deps.el.title] [deps.el.staleStrip]
     * @param {HTMLElement|null} [deps.el.body] [deps.el.applyBtn] [deps.el.copyBtn]
     * @param {HTMLElement|null} [deps.el.discardBtn] [deps.el.closeBtn]
     * @param {object} callbacks
     * @param {(phase: string) => void} [callbacks.onPhaseChanged]
     * @param {(message: string) => void} [callbacks.onStatus] - 状态行文案（已翻译）
     * @param {(message: string) => void} [callbacks.onError] - 已翻译错误文案
     */
    constructor({api, adapter, getSession, listen, copyToClipboard, el = {}}, callbacks = {}) {
        this._api = api;
        this._adapter = adapter;
        this._getSession = getSession;
        this._listen = listen;
        this._copyToClipboard = copyToClipboard;
        this._el = el;
        this._callbacks = callbacks;
        this._unlisteners = null;
    }

    get isBusy() {
        return this.phase === "running";
    }

    /** 注册后端完成/失败事件。幂等。 */
    async bind() {
        if (this._unlisteners) return;
        this._unlisteners = [
            await this._listen(EVENTS.EDITOR_TRANSFORM_COMPLETED, (e) => this.handleCompleted(e.payload)),
            await this._listen(EVENTS.EDITOR_TRANSFORM_FAILED, (e) => this.handleFailed(e.payload)),
        ];
        this._bindCardActions();
    }

    /** 解绑事件监听（测试/卸载用）。 */
    unbind() {
        for (const off of this._unlisteners ?? []) {
            try {
                off();
            } catch {
                /* 忽略 */
            }
        }
        this._unlisteners = null;
    }

    // ── 入口 ──────────────────────────────────────────────────────────────

    /**
     * 发起整理。scope="selection" 冻结当前选区；scope="dictation" 定位拼接文本。
     * 已有运行请求时先取消（§3.7 新请求取消旧请求）；已有候选直接丢弃。
     * @param {"selection"|"dictation"} scope
     * @param {{text?: string}} [opts] - dictation 的范围文本（本次听写拼接）
     */
    async start(scope, {text = ""} = {}) {
        const session = this._getSession();
        if (!session || !session.isActive) {
            this._callbacks.onError?.(t("editor.transform.err.stale_session"));
            return;
        }
        if (this.isBusy) await this.cancel({silent: true});
        if (this.candidate) this._discardCandidate({silent: true});

        const frozen = this._adapter.freezeRange(scope, text);
        if (!frozen || !frozen.text.trim()) {
            this._callbacks.onError?.(t("editor.transform.err.empty"));
            return;
        }

        this.phase = "running";
        this._callbacks.onPhaseChanged?.(this.phase);
        this.pendingRun = {
            scope,
            sourceText: frozen.text,
            handle: frozen.handle,
            frozenRevision: this._adapter.revision,
        };
        this._callbacks.onStatus?.(t("editor.transform.running"));
        this._renderCard();

        try {
            const res = await this._api.startEditorTransform({
                sessionRef: session.sessionRef,
                generation: session.generation,
                scope,
                text: frozen.text,
                revision: this._adapter.revision,
                rangeHandle: JSON.stringify(frozen.handle),
            });
            this.runRequestId = res?.requestId ?? null;
        } catch (e) {
            const err = normalizeError(e);
            console.warn(`[editor-transform] start 失败 [${err.code}]: ${err.message}`);
            this.phase = "idle";
            this.runRequestId = null;
            this.pendingRun = null;
            this._callbacks.onPhaseChanged?.(this.phase);
            this._callbacks.onError?.(this._describe(err.code, err.detail));
            this._renderCard();
        }
    }

    /**
     * 取消运行请求并丢弃候选（视图切换/会话结束/窗口关闭/新请求）。
     * @param {{silent?: boolean}} [opts] - silent 时只清状态不提示
     */
    async cancel({silent = false} = {}) {
        const hadRunning = this.phase === "running" && this.runRequestId != null;
        const requestId = this.runRequestId;
        this.phase = "idle";
        this.runRequestId = null;
        this.pendingRun = null;
        if (this.candidate) this._discardCandidate({silent: true});

        if (hadRunning) {
            try {
                await this._api.cancelEditorTransform(requestId);
            } catch (e) {
                const err = normalizeError(e);
                console.warn(`[editor-transform] cancel 失败 [${err.code}]: ${err.message}`);
            }
            if (!silent) this._callbacks.onStatus?.(t("editor.transform.cancelled"));
        }
        this._callbacks.onPhaseChanged?.(this.phase);
        this._renderCard();
    }

    // ── 后端事件 ──────────────────────────────────────────────────────────

    /**
     * 完成事件：身份 + requestId 过滤 → 构建候选 → 渲染候选卡。
     * 冻结 revision 与当前 revision 比对得出初始 stale（请求期间被编辑）。
     *
     * 竞态防护：start IPC 响应尚未返回（runRequestId == null）时事件先到，
     * 直接采纳其 requestId——此窗口内至多只有一个未决请求（旧请求已在
     * start 入口取消），不会误吞完成事件导致 running 卡死。
     * @param {{sessionRef: string, generation: number, requestId: number, scope: string,
     *          revisedText: string, revision: number, rangeHandle: string}} p
     */
    handleCompleted(p) {
        if (!p || this.phase !== "running" || !this.pendingRun) return;
        const session = this._getSession();
        if (!session || p.sessionRef !== session.sessionRef || p.generation !== session.generation) return;
        if (this.runRequestId != null && p.requestId !== this.runRequestId) return;
        this.runRequestId = p.requestId;

        this.phase = "candidate";
        this.runRequestId = null;
        const stale = p.revision !== this._adapter.revision;
        this.candidate = {
            requestId: p.requestId,
            scope: this.pendingRun.scope,
            sourceText: this.pendingRun.sourceText,
            handle: this.pendingRun.handle,
            revisedText: typeof p.revisedText === "string" ? p.revisedText : "",
            frozenRevision: p.revision,
            stale,
        };
        this.pendingRun = null;
        this._callbacks.onPhaseChanged?.(this.phase);
        this._callbacks.onStatus?.(t("editor.transform.candidateReady"));
        this._renderCard();
    }

    /**
     * 失败/取消事件：身份 + requestId 过滤 → 回落 idle 并提示。
     * 与 handleCompleted 同样的早到事件防护（runRequestId 未回填时采纳）。
     * @param {{sessionRef: string, generation: number, requestId: number, code: string}} p
     */
    handleFailed(p) {
        if (!p) return;
        const session = this._getSession();
        if (!session || p.sessionRef !== session.sessionRef || p.generation !== session.generation) return;
        // 运行中才消费（迟到失败不影响新请求）；candidate 态收到 cancelled
        // 说明请求已被本地取消，本地状态已清理，忽略。
        if (this.phase !== "running" || !this.pendingRun) return;
        if (this.runRequestId != null && p.requestId !== this.runRequestId) return;

        const code = p.code ?? "provider";
        const alreadyIdle = code === "cancelled";
        this.phase = "idle";
        this.runRequestId = null;
        this.pendingRun = null;
        this._callbacks.onPhaseChanged?.(this.phase);
        if (!alreadyIdle) {
            this._callbacks.onError?.(this._describe(code, ""));
        }
        this._renderCard();
    }

    // ── 候选操作 ──────────────────────────────────────────────────────────

    /**
     * 正文变化通知（adapter revision 自增后由 main.js 调用）：
     * 存在候选且尚未 stale → 置 stale 并刷新候选卡（§3.7）。
     */
    notifyContentChanged() {
        if (this.phase === "candidate" && this.candidate && !this.candidate.stale
            && this._adapter.revision !== this.candidate.frozenRevision) {
            this.candidate.stale = true;
            this._callbacks.onStatus?.(t("editor.transform.stale"));
            this._renderCard();
        }
    }

    /**
     * 确认应用：stale 拒绝；Adapter 单事务替换（Engine 复核冻结文本）；
     * 成功后丢弃候选。MD 块边界不安全的候选在候选卡上即禁用应用。
     * @returns {Promise<boolean>} 是否已应用
     */
    async apply() {
        if (!this.candidate || this.candidate.stale) return false;
        const ok = this._adapter.replaceRange(this.candidate.handle, this.candidate.revisedText);
        if (!ok) {
            // Engine 复核失败（正文实际内容与冻结不符）：候选作废
            this._callbacks.onError?.(t("editor.transform.err.applyFailed"));
            this._discardCandidate({silent: true});
            this.phase = "idle";
            this._callbacks.onPhaseChanged?.(this.phase);
            this._renderCard();
            return false;
        }
        this._discardCandidate({silent: true});
        this.phase = "idle";
        this._callbacks.onPhaseChanged?.(this.phase);
        this._callbacks.onStatus?.(t("editor.transform.applied"));
        this._renderCard();
        return true;
    }

    /** 复制整理结果（stale 时仍可用，§3.7）。 */
    async copy() {
        if (!this.candidate) return;
        try {
            await this._copyToClipboard(this.candidate.revisedText);
            this._callbacks.onStatus?.(t("editor.transform.copied"));
        } catch (e) {
            console.error("[editor-transform] 复制失败:", e);
            this._callbacks.onError?.(t("editor.transform.err.copyFailed"));
        }
    }

    /** 放弃候选（用户显式）。 */
    async discard() {
        if (!this.candidate) return;
        this._discardCandidate({silent: false});
    }

    // ── 内部 ──────────────────────────────────────────────────────────────

    _discardCandidate({silent}) {
        this.candidate = null;
        if (this.phase === "candidate") this.phase = "idle";
        if (!silent) this._callbacks.onStatus?.(t("editor.transform.discarded"));
        this._renderCard();
    }

    _bindCardActions() {
        const {applyBtn, copyBtn, discardBtn, closeBtn} = this._el;
        applyBtn?.addEventListener("click", () => void this.apply());
        copyBtn?.addEventListener("click", () => void this.copy());
        discardBtn?.addEventListener("click", () => void this.discard());
        closeBtn?.addEventListener("click", () => void this.discard());
    }

    /** 候选卡投影（无头模式下为空操作）。 */
    _renderCard() {
        const {card, title, staleStrip, body, applyBtn, copyBtn} = this._el;
        if (!card) return;
        const show = !!this.candidate || this.phase === "running";
        card.classList.toggle("hidden", !show);
        if (!show) return;

        if (this.phase === "running") {
            if (title) title.textContent = t("editor.transform.cardTitleRunning");
            if (staleStrip) staleStrip.classList.add("hidden");
            if (body) body.innerHTML = `<span class="diff-running">${t("editor.transform.running")}</span>`;
            if (applyBtn) applyBtn.disabled = true;
            if (copyBtn) copyBtn.disabled = true;
            return;
        }

        const cand = this.candidate;
        if (title) {
            title.textContent = t(cand.scope === "dictation"
                ? "editor.transform.cardTitleDictation"
                : "editor.transform.cardTitleSelection");
        }
        if (staleStrip) staleStrip.classList.toggle("hidden", !cand.stale);

        const segments = computeDiff(cand.sourceText, cand.revisedText);
        if (body) {
            if (segments && hasChanges(segments)) {
                body.innerHTML = diffToHtml(segments, escapeHtml);
            } else if (segments) {
                // 无可见改动（AI 原样返回）
                body.innerHTML = `<span class="diff-plain">${escapeHtml(cand.revisedText)}</span>`;
            } else {
                // 超门/熔断：仅复制展示（§3.10）
                body.innerHTML = `<span class="diff-plain">${escapeHtml(cand.revisedText)}</span>`;
            }
        }

        // 应用可用性：非 stale + 块安全（MD 跨块范围仅复制，§3.7）
        const applyable = !cand.stale && cand.handle?.blockSafe !== false;
        if (applyBtn) {
            applyBtn.disabled = !applyable;
            applyBtn.title = cand.stale
                ? t("editor.transform.stale")
                : (!cand.handle?.blockSafe ? t("editor.transform.copyOnlyHint") : "");
        }
        if (copyBtn) copyBtn.disabled = false;
    }

    _describe(code, detail) {
        switch (code) {
            case "ai_already_active": {
                // EditorError detail: { kind, activeWindow }（§3.10 携带活跃窗口）
                const win = detail?.activeWindow ?? null;
                return t("editor.transform.err.ai_already_active", {window: windowLabel(win)});
            }
            case "unsupported":
                // Unsupported.detail 是后端给的可行动中文说明（输入超限/上下文不足等）
                return (typeof detail?.detail === "string" && detail.detail)
                    ? detail.detail
                    : t("editor.transform.err.generic", {code});
            case "stale_session":
                return t("editor.transform.err.stale_session");
            case "cancelled":
                return t("editor.transform.err.cancelled");
            case "length":
            case "truncated":
                return t("editor.transform.err.truncated");
            case "content_filter":
                return t("editor.transform.err.contentFilter");
            case "empty":
                return t("editor.transform.err.emptyOutput");
            case "timeout":
                return t("editor.transform.err.timeout");
            case "network":
                return t("editor.transform.err.network");
            case "not_configured":
                return t("editor.transform.err.notConfigured");
            default:
                return t("editor.transform.err.generic", {code});
        }
    }
}

/** 活跃窗口标识 → 展示名（§3.10 AiAlreadyActive 携带当前活跃窗口）。 */
function windowLabel(targetWindow) {
    switch (targetWindow) {
        case "main": return t("editor.transform.window.main");
        case "chat": return t("editor.transform.window.chat");
        case "editor": return t("editor.transform.window.editor");
        default: return targetWindow ?? "";
    }
}

/** HTML 转义（候选卡正文渲染用）。 */
function escapeHtml(s) {
    return s.replace(/[&<>"']/g, (c) => ({
        "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;",
    }[c]));
}
