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

/**
 * "仍在处理中"档位阈值（秒，0.23.7）。
 *
 * 真实请求常见 20s+，静态"正在整理"会让用户以为卡死。超过该阈值后等待态
 * 追加"仍在处理中"提示，并始终展示真实经过时间——**不展示百分比**：后端
 * 当前没有可用的阶段进度，虚假进度比没有进度更糟。
 */
export const STILL_WORKING_AFTER_SECONDS = 10;

/** 等待态文案 key（纯函数）：未超阈值用常态文案，超过后提示仍在处理。 */
export function runningLabelKey(seconds) {
    return seconds >= STILL_WORKING_AFTER_SECONDS
        ? "editor.transform.stillWorking"
        : "editor.transform.running";
}

/** 真实经过时间（秒，向下取整、非负；起点缺失时按 0）。 */
export function elapsedSeconds(startedAt, now) {
    if (startedAt == null || now == null) return 0;
    return Math.max(0, Math.floor((now - startedAt) / 1000));
}

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

    /** 本地 operation generation（0.23.6 §5.7：每次 start 递增） */
    _opSeq = 0;

    /**
     * 已取消/追认取消的请求 id：其迟到完成/失败事件不得污染新请求。
     * 有界 Map（id → 退役时 opSeq），超上限按插入序淘汰最旧——预热窗口
     * 长会话下不无界增长（0.23.6 二次 Review）。
     * @type {Map<number, number>}
     */
    _retiredRequestIds = new Map();

    /** retired 记录上限（完成事件迟于响应的窗口有限，32 足够宽裕） */
    static RETIRED_CAP = 32;

    /** 等待态计时器句柄（running 期间每秒刷新真实经过时间） */
    _runTimer = null;

    /** running 起始时刻（ms；离开 running 时清空） */
    _runStartedAt = null;

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
     * @param {() => number} [deps.now] - 时钟（测试注入假时钟）
     * @param {(fn: () => void, ms: number) => *} [deps.setTimer] - 计时器（测试注入）
     * @param {(id: *) => void} [deps.clearTimer]
     * @param {object} callbacks
     * @param {(phase: string) => void} [callbacks.onPhaseChanged]
     * @param {(message: string) => void} [callbacks.onStatus] - 状态行文案（已翻译）
     * @param {(message: string) => void} [callbacks.onError] - 已翻译错误文案
     */
    constructor(
        {
            api,
            adapter,
            getSession,
            listen,
            copyToClipboard,
            el = {},
            now = () => Date.now(),
            // UI 心跳不应单独维持进程存活（Node 测试/CLI 场景会因此挂死）；
            // 浏览器返回值是 number，无 unref，`?.` 直接忽略。
            setTimer = (fn, ms) => {
                const id = setInterval(fn, ms);
                id?.unref?.();
                return id;
            },
            clearTimer = (id) => clearInterval(id),
        },
        callbacks = {},
    ) {
        this._api = api;
        this._adapter = adapter;
        this._getSession = getSession;
        this._listen = listen;
        this._copyToClipboard = copyToClipboard;
        this._el = el;
        this._now = now;
        this._setTimer = setTimer;
        this._clearTimer = clearTimer;
        this._callbacks = callbacks;
        this._unlisteners = null;
    }

    get isBusy() {
        return this.phase === "running";
    }

    /** 等待态真实经过时间（秒） */
    get runningElapsedSeconds() {
        return elapsedSeconds(this._runStartedAt, this._now());
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
        this._stopTicker();
        for (const off of this._unlisteners ?? []) {
            try {
                off();
            } catch {
                /* 忽略 */
            }
        }
        this._unlisteners = null;
    }

    /** 语言切换后重绘卡片动态文案（等待态标签、取消/放弃按钮）。 */
    refreshCard() {
        this._renderCard();
    }

    // ── 入口 ──────────────────────────────────────────────────────────────

    /**
     * 发起整理。scope="selection" 冻结当前选区；scope="dictation" 定位拼接文本。
     * 已有运行请求时先取消（§3.7 新请求取消旧请求）；已有候选直接丢弃。
     * @param {"selection"|"dictation"} scope
     * @param {{text?: string, handle?: {handle: object, text: string, blockSafe: boolean}|null}} [opts]
     *   dictation 的范围文本（本次听写拼接）；handle = Adapter 已冻结的
     *   本轮听写范围（0.23.6 §5.7，提供时跳过 Engine 内 indexOf 定位）
     */
    async start(scope, {text = "", handle = null} = {}) {
        const session = this._getSession();
        if (!session || !session.isActive) {
            this._callbacks.onError?.(t("editor.transform.err.stale_session"));
            return;
        }
        if (this.isBusy) await this.cancel({silent: true});
        if (this.candidate) this._discardCandidate({silent: true});

        const frozen = handle ?? this._adapter.freezeRange(scope, text);
        if (!frozen || !frozen.text.trim()) {
            this._callbacks.onError?.(t("editor.transform.err.empty"));
            return;
        }

        // 本地 operation generation（0.23.6 §5.7）：start IPC 响应未返回期间
        // 发生的取消/新请求会推进 _opSeq 或离开 running；迟到响应据此不写回
        // 状态，并立即补发取消（追认），不让被弃请求占用全局 AI 单槽。
        const op = ++this._opSeq;
        // 新一代请求起步即清空运行槽：上一代遗留的 requestId 不得参与本代
        // 完成事件过滤（二次 Review：连续极早完成不得遗留旧 id）。
        this.runRequestId = null;
        this.pendingRun = {
            scope,
            sourceText: frozen.text,
            handle: frozen.handle,
            frozenRevision: this._adapter.revision,
        };
        this._callbacks.onStatus?.(t("editor.transform.running"));
        this._setPhase("running");

        try {
            const res = await this._api.startEditorTransform({
                sessionRef: session.sessionRef,
                generation: session.generation,
                scope,
                text: frozen.text,
                revision: this._adapter.revision,
                rangeHandle: JSON.stringify(frozen.handle),
            });
            if (op !== this._opSeq) {
                // 已被更新一代请求取代：追认取消，不写回任何状态。
                this._retireLateRequest(res?.requestId);
                return;
            }
            if (this.phase !== "running") {
                if (this.phase === "candidate" && this.candidate
                    && res?.requestId != null && this.candidate.requestId === res.requestId) {
                    // 完成事件早于 start 响应被采纳（§3.7 契约）：响应只确认
                    // 候选归属。不得把已完成 id 写回 runRequestId——否则放弃
                    // 候选后的下一请求会被旧 id 拒绝其早到完成事件，永久停在
                    // running（0.23.6 二次 Review）。
                    this._retiredRequestIds.delete(res.requestId);
                    return;
                }
                // 已被取消（idle）：迟到响应不得复活状态，追认取消。
                this._retireLateRequest(res?.requestId);
                return;
            }
            this.runRequestId = res?.requestId ?? null;
            if (this.runRequestId != null) this._retiredRequestIds.delete(this.runRequestId);
        } catch (e) {
            const err = normalizeError(e);
            console.warn(`[editor-transform] start 失败 [${err.code}]: ${err.message}`);
            if (op !== this._opSeq || this.phase !== "running") return; // 迟到失败不污染新状态
            this.runRequestId = null;
            this.pendingRun = null;
            this._setPhase("idle");
            this._callbacks.onError?.(this._describe(err.code, err.detail));
        }
    }

    /**
     * 取消运行请求并丢弃候选（视图切换/会话结束/窗口关闭/新请求）。
     * start 响应尚未返回（runRequestId == null）时只清本地状态——迟到响应
     * 到达后由 start 内的 op/phase 检查追认取消。
     * @param {{silent?: boolean}} [opts] - silent 时只清状态不提示
     */
    async cancel({silent = false} = {}) {
        const hadRunning = this.phase === "running";
        const requestId = this.runRequestId;
        this.runRequestId = null;
        this.pendingRun = null;
        if (this.candidate) this._discardCandidate({silent: true});
        this._setPhase("idle");

        if (hadRunning && requestId != null) {
            this._retire(requestId);
            try {
                await this._api.cancelEditorTransform(requestId);
            } catch (e) {
                const err = normalizeError(e);
                console.warn(`[editor-transform] cancel 失败 [${err.code}]: ${err.message}`);
            }
            if (!silent) this._callbacks.onStatus?.(t("editor.transform.cancelled"));
        }
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
        // 已取消请求的迟到完成事件：不污染当前/后续请求（0.23.6 §5.7）
        if (this._retiredRequestIds.has(p.requestId)) return;
        const session = this._getSession();
        if (!session || p.sessionRef !== session.sessionRef || p.generation !== session.generation) return;
        if (this.runRequestId != null && p.requestId !== this.runRequestId) return;
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
        this._setPhase("candidate");
        this._callbacks.onStatus?.(t("editor.transform.candidateReady"));
    }

    /**
     * 失败/取消事件：身份 + requestId 过滤 → 回落 idle 并提示。
     * 与 handleCompleted 同样的早到事件防护（runRequestId 未回填时采纳）。
     * @param {{sessionRef: string, generation: number, requestId: number, code: string}} p
     */
    handleFailed(p) {
        if (!p) return;
        // 已取消请求的迟到失败（含后端 confirmed 的 cancelled）：忽略（0.23.6 §5.7）
        if (this._retiredRequestIds.has(p.requestId)) return;
        const session = this._getSession();
        if (!session || p.sessionRef !== session.sessionRef || p.generation !== session.generation) return;
        // 运行中才消费（迟到失败不影响新请求）；candidate 态收到 cancelled
        // 说明请求已被本地取消，本地状态已清理，忽略。
        if (this.phase !== "running" || !this.pendingRun) return;
        if (this.runRequestId != null && p.requestId !== this.runRequestId) return;

        const code = p.code ?? "provider";
        const alreadyIdle = code === "cancelled";
        this.runRequestId = null;
        this.pendingRun = null;
        this._setPhase("idle");
        if (!alreadyIdle) {
            this._callbacks.onError?.(this._describe(code, ""));
        }
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
            return false;
        }
        this._discardCandidate({silent: true});
        this._callbacks.onStatus?.(t("editor.transform.applied"));
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

    /**
     * 放弃候选 / 取消运行中的请求（用户显式）。
     * 运行中时"放弃"即取消请求——候选卡上该按钮文案相应显示为"取消"，
     * 与关闭按钮同义，两个入口都保留（0.23.7 等待态要求保留取消/关闭入口）。
     */
    async discard() {
        if (this.phase === "running") {
            await this.cancel();
            return;
        }
        if (!this.candidate) return;
        this._discardCandidate({silent: false});
    }

    // ── 内部 ──────────────────────────────────────────────────────────────

    /**
     * 唯一 phase 写入口：同步等待态计时器 → 通知 phase 变化 → 重绘卡片。
     * running 期间启动 1s 心跳刷新真实经过时间；离开 running 立即停表，
     * 不留悬挂计时器（会话结束/取消/候选/失败都走这里）。
     * @param {"idle"|"running"|"candidate"} next
     */
    _setPhase(next) {
        this.phase = next;
        if (next === "running") {
            if (this._runStartedAt == null) this._runStartedAt = this._now();
            this._startTicker();
        } else {
            this._stopTicker();
            this._runStartedAt = null;
        }
        this._callbacks.onPhaseChanged?.(next);
        this._renderCard();
    }

    /** 启动等待态心跳（幂等：先停旧表）。 */
    _startTicker() {
        this._stopTicker();
        this._runTimer = this._setTimer(() => {
            if (this.phase !== "running") {
                this._stopTicker();
                return;
            }
            this._renderRunningText();
        }, 1000);
    }

    _stopTicker() {
        if (this._runTimer == null) return;
        this._clearTimer(this._runTimer);
        this._runTimer = null;
    }

    /** 只更新等待行的文案节点（避免整段 innerHTML 重建重启 spinner 动画）。 */
    _renderRunningText() {
        const body = this._el.body;
        if (!body?.querySelector) return;
        const seconds = this.runningElapsedSeconds;
        const label = body.querySelector(".editor-diff-running-label");
        if (label) label.textContent = t(runningLabelKey(seconds));
        const elapsed = body.querySelector(".editor-diff-running-elapsed");
        if (elapsed) elapsed.textContent = t("editor.transform.elapsed", {seconds});
    }

    /** 等待态行：spinner + 状态文案 + 真实经过时间（无百分比，§3.7）。 */
    _runningHtml() {
        const seconds = this.runningElapsedSeconds;
        return '<span class="editor-diff-running">'
            + '<span class="spinner spinner-sm" aria-hidden="true"></span>'
            + `<span class="editor-diff-running-label">${escapeHtml(t(runningLabelKey(seconds)))}</span>`
            + `<span class="editor-diff-running-elapsed">${escapeHtml(t("editor.transform.elapsed", {seconds}))}</span>`
            + "</span>";
    }

    /**
     * 退役一个请求 id：标记其迟到事件无效，并维持有界（超上限按插入序淘汰
     * 最旧）。0.23.6 二次 Review：预热窗口长会话下 retired 记录不得无界增长。
     * @param {number|null} requestId
     */
    _retire(requestId) {
        if (requestId == null) return;
        this._retiredRequestIds.set(requestId, this._opSeq);
        while (this._retiredRequestIds.size > EditorTransformController.RETIRED_CAP) {
            const oldest = this._retiredRequestIds.keys().next().value;
            this._retiredRequestIds.delete(oldest);
        }
    }

    /** 追认取消：迟到 start 响应携带的请求已不属于当前状态，标记退役并补发取消。 */
    _retireLateRequest(requestId) {
        if (requestId == null) return;
        this._retire(requestId);
        void this._api.cancelEditorTransform(requestId).catch((e) => {
            const err = normalizeError(e);
            console.warn(`[editor-transform] 追认取消失败 [${err.code}]: ${err.message}`);
        });
    }

    _discardCandidate({silent}) {
        const wasCandidate = this.phase === "candidate";
        this.candidate = null;
        if (!silent) this._callbacks.onStatus?.(t("editor.transform.discarded"));
        if (wasCandidate) {
            // 经唯一 phase 入口：通知下游（chips 可见性等）并重绘
            this._setPhase("idle");
        } else {
            this._renderCard();
        }
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
        const {card, title, staleStrip, body, applyBtn, copyBtn, discardBtn} = this._el;
        if (!card) return;
        const show = !!this.candidate || this.phase === "running";
        card.classList.toggle("hidden", !show);
        if (!show) return;

        if (this.phase === "running") {
            if (title) title.textContent = t("editor.transform.cardTitleRunning");
            if (staleStrip) staleStrip.classList.add("hidden");
            // 等待态：spinner + 状态文案 + 真实经过时间（超过阈值提示"仍在处理中"）
            if (body) body.innerHTML = this._runningHtml();
            // 尚不可用的应用/复制入口禁用；放弃按钮改为"取消"语义（等同关闭）
            if (applyBtn) {
                applyBtn.disabled = true;
                applyBtn.title = t("editor.transform.runningHint");
            }
            if (copyBtn) {
                copyBtn.disabled = true;
                copyBtn.title = t("editor.transform.runningHint");
            }
            if (discardBtn) discardBtn.textContent = t("editor.transform.cancel");
            return;
        }

        const cand = this.candidate;
        if (title) {
            title.textContent = t(cand.scope === "dictation"
                ? "editor.transform.cardTitleDictation"
                : "editor.transform.cardTitleSelection");
        }
        if (staleStrip) staleStrip.classList.toggle("hidden", !cand.stale);
        if (discardBtn) discardBtn.textContent = t("editor.transform.discard");

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
