/**
 * EditorVoiceController（0.23.3）——编辑器连续听写的前端状态机。
 *
 * 职责（phase 文档 §3.6 / §6.4）：
 * - 由编辑器按钮显式开始/结束；启动时冻结 session_ref + generation；
 * - confirmed segment 按 epoch + seq 去重追尾：重复 seq 忽略，缺号拉
 *   snapshot 补齐，仍缺号则跳号继续（记 onGapLost）；
 * - preview 只进浮窗装饰层，本控制器不把它写进正文；
 * - 追加经 Adapter 单事务（首段补段落分隔），不抢焦点、不移动前文
 *   selection；追加计入正文/dirty/undo（confirmed 是正文内容）；
 * - 结束后保留本次追加文本（segments），供"定位/整理本次听写"；
 * - 视图切换/会话结束由 main.js 决定停听写；控制器自身无窗口知识。
 *
 * phase 与 §3.2 voice 切片对应：idle/starting/recording/paused/stopping。
 */

import {normalizeError} from "../shared/tauri.js";
import {EVENTS} from "../shared/event-names.js";

export class EditorVoiceController {
    /** @type {"idle"|"starting"|"recording"|"paused"|"stopping"} */
    phase = "idle";

    /** 当前听写 epoch（start_editor_voice 返回） */
    epoch = 0;

    /** 已处理的最大 seq（0 = 尚无段；事件与快照统一按此去重） */
    lastSeq = 0;

    /** 冻结的编辑器会话身份 */
    sessionRef = null;
    generation = 0;

    /** 本次听写已追加的段文本（定位/整理入口用） */
    segments = [];

    /** 本轮听写冻结范围（Adapter 签发的 opaque handle；结束/清空/失效为 null） */
    runHandle = null;

    /**
     * @param {object} deps
     * @param {*} deps.api - shared/api.js 子集（测试注入 fake）：
     *   { startEditorVoice, stopEditorVoice, getEditorVoiceSnapshot }
     * @param {*} deps.adapter - EditorAdapter 实例（appendDictation/locateText）
     * @param {() => {isActive: boolean, sessionRef: string|null, generation: number}} deps.getSession
     * @param {*} deps.listen - tauri listen（测试注入 fake）
     * @param {object} callbacks
     * @param {(phase: string) => void} [callbacks.onPhaseChanged]
     * @param {() => void} [callbacks.onSegmentAppended]
     * @param {() => void} [callbacks.onGapLost] - 缺号且快照无法补齐
     * @param {{count: number}} [callbacks.onEnded]
     * @param {(message: string) => void} [callbacks.onError] - 已翻译文案
     * @param {(code: string, message: string) => string} [callbacks.describeError] - 错误码 → 文案
     */
    constructor({api, adapter, getSession, listen}, callbacks = {}) {
        this._api = api;
        this._adapter = adapter;
        this._getSession = getSession;
        this._listen = listen;
        this._callbacks = callbacks;
        this._unlisteners = null;
    }

    get isBusy() {
        return this.phase !== "idle";
    }

    /** 本次听写拼接文本（"定位到本次听写"搜索串） */
    get joinedText() {
        return this.segments.join("");
    }

    /** 注册后端事件监听（segment/status）。幂等。 */
    async bind() {
        if (this._unlisteners) return;
        this._unlisteners = [
            await this._listen(EVENTS.EDITOR_VOICE_SEGMENT, (event) => this.handleSegment(event.payload)),
            await this._listen(EVENTS.EDITOR_VOICE_STATUS, (event) => this.handleStatus(event.payload)),
        ];
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

    /** 麦克风按钮语义：idle → 开始；其余 → 结束。 */
    toggle() {
        if (this.phase === "idle") return this.start();
        return this.stop();
    }

    /** 开始听写：校验会话 → 冻结身份 → 启动 Editor VoiceSession。 */
    async start() {
        if (this.phase !== "idle") return;
        const session = this._getSession();
        if (!session || !session.isActive) {
            this._callbacks.onError?.(this._describe("stale_session", ""));
            return;
        }
        this.phase = "starting";
        this._callbacks.onPhaseChanged?.(this.phase);
        try {
            const res = await this._api.startEditorVoice(session.sessionRef, session.generation);
            this.sessionRef = session.sessionRef;
            this.generation = session.generation;
            this.epoch = res?.epoch ?? 0;
            this.lastSeq = 0;
            this.segments = [];
            this.runHandle = null;
            this._setPhase("recording");
            // 记录本轮真实追加锚点（0.23.6 §5.7）：定位/整理只作用于
            // 锚点之后的范围，正文前部相同文本不会被误命中。
            this._adapter.beginDictationRun?.();
        } catch (e) {
            this._setPhase("idle");
            const err = normalizeError(e);
            console.warn(`[editor-voice] start 失败 [${err.code}]: ${err.message}`);
            this._callbacks.onError?.(this._describe(err.code, err.message));
        }
    }

    /** 结束听写：confirmed 保留、preview 丢弃（后端收尾，ended 状态回落）。 */
    async stop() {
        if (this.phase === "idle" || this.phase === "stopping") return;
        this._setPhase("stopping");
        try {
            await this._api.stopEditorVoice();
            // command 在后端收尾完成后才返回；若 ended 事件丢失，防御性回落，
            // 避免预热窗口复用后永久卡在 stopping。
            if (this.phase === "stopping") this._finish();
        } catch (e) {
            const err = normalizeError(e);
            console.warn(`[editor-voice] stop 失败 [${err.code}]: ${err.message}`);
            this._setPhase("idle");
            this._callbacks.onError?.(this._describe(err.code || "voice_failed", err.message));
        }
    }

    /** 窗口重新聚焦/恢复时的补齐入口（§3.6 恢复或重新聚焦拉 snapshot）。 */
    async resyncIfActive() {
        if (this.phase === "recording" || this.phase === "paused") {
            await this._resync();
        }
    }

    /**
     * 编辑器会话事件联动（blink://editor-session-changed 的 ended）：
     * 会话已结束则停听写并清本地状态（confirmed 已在正文中，不回撤）。
     */
    handleSessionEnded() {
        if (this.phase !== "idle") {
            // 防御性释放：正常路径 main.js 已先 stop，此处兜底回收麦克风
            this.stop().catch(() => {});
        }
        this._setPhase("idle");
        // 即使听写已正常 ended（phase 已是 idle），结束 EditorSession 也必须
        // 清掉定位/整理用的段缓存，禁止跨会话残留。
        this._clearRun();
    }

    // ── 后端事件 ──────────────────────────────────────────────────────────

    /**
     * confirmed 段事件：身份/epoch 过滤 → seq 去重 → 缺号补齐 → 追加。
     * async：缺号路径需等待快照补齐（测试可 await；生产 fire-and-forget）。
     * @param {{sessionRef: string, generation: number, epoch: number, seq: number, text: string}} p
     */
    async handleSegment(p) {
        if (!p || this.phase === "idle" || this.phase === "stopping") return;
        if (p.sessionRef !== this.sessionRef || p.generation !== this.generation) return;
        if (p.epoch !== this.epoch) return;
        if (typeof p.seq !== "number" || p.seq <= this.lastSeq) return; // 重复/回退
        if (p.seq > this.lastSeq + 1) {
            // 缺号：先拉快照补齐（追完再消费当前事件）；真实缺口由 _resync
            // 显式上报后从最早可用段继续，此处只在快照不可用时兜底上报。
            const {gapReported} = await this._resync();
            if (p.seq <= this.lastSeq) return; // 快照已覆盖当前段
            if (p.seq > this.lastSeq + 1 && !gapReported) {
                console.warn(`[editor-voice] 缺号无法补齐（snapshot 不可用），跳号 ${this.lastSeq + 1}-${p.seq - 1}`);
                this._callbacks.onGapLost?.();
            }
        }
        this._appendSegment(p.seq, p.text);
    }

    /**
     * 状态事件：phase 投影 + ended 回落。
     * @param {{sessionRef: string, generation: number, epoch: number, phase: string,
     *          preview?: string|null, message?: string|null}} p
     */
    handleStatus(p) {
        if (!p) return;
        if (p.sessionRef && p.sessionRef !== this.sessionRef) return;
        if (p.generation && p.generation !== this.generation) return;
        if (p.epoch && this.epoch && p.epoch !== this.epoch) return;

        switch (p.phase) {
            case "recording":
                if (this.phase === "starting" || this.phase === "paused") this._setPhase("recording");
                break;
            case "paused":
                this._setPhase("paused");
                break;
            case "finalizing":
                if (this.phase !== "idle") this._setPhase("stopping");
                break;
            case "error":
                this._callbacks.onError?.(
                    this._describe(p.code || "voice_failed", p.message || ""),
                );
                break;
            case "ended":
                // stopped→ended（正常）或 starting→ended（秒错）：confirmed 保留
                this._finish();
                break;
            default:
                break;
        }
    }

    // ── 内部 ──────────────────────────────────────────────────────────────

    /**
     * 拉取 snapshot 补 lastSeq 之后的段（epoch 失配/无状态 → 忽略）。
     *
     * 缺口显式判定（0.23.6 §5.7）：snapshot 首段 seq 不紧接 `lastSeq + 1`
     * 时，所需区间已被淘汰——显式触发一次 `onGapLost`，再从最早可用段继续，
     * 禁止静默丢段；首段紧接 lastSeq（已越过淘汰区间）时即使历史
     * `truncated > 0` 也不误报。
     * @returns {Promise<{appended: number, gapReported: boolean}>}
     */
    async _resync() {
        try {
            const snap = await this._api.getEditorVoiceSnapshot(this.epoch, this.lastSeq);
            if (!snap || snap.epoch !== this.epoch || !Array.isArray(snap.segments)) {
                return {appended: 0, gapReported: false};
            }
            let gapReported = false;
            const first = snap.segments[0];
            if (first && first.seq > this.lastSeq + 1) {
                console.warn(
                    `[editor-voice] 听写段 ${this.lastSeq + 1}-${first.seq - 1} 已被快照淘汰，无法补齐`,
                );
                this._callbacks.onGapLost?.();
                gapReported = true;
            }
            let appended = 0;
            for (const seg of snap.segments) {
                if (seg.seq > this.lastSeq) {
                    this._appendSegment(seg.seq, seg.text);
                    appended += 1;
                }
            }
            return {appended, gapReported};
        } catch (e) {
            const err = normalizeError(e);
            console.warn(`[editor-voice] snapshot 补齐失败 [${err.code}]: ${err.message}`);
            return {appended: 0, gapReported: false};
        }
    }

    /** 追加一段到正文（Adapter 单事务；首段补段落分隔） */
    _appendSegment(seq, text) {
        if (typeof text !== "string" || text.length === 0) {
            this.lastSeq = Math.max(this.lastSeq, seq);
            return;
        }
        const isFirst = this.segments.length === 0;
        const written = this._adapter.appendDictation(text, {newParagraph: isFirst});
        if (written === false) {
            // 只读 MD 预览等不可编辑场景：不写正文、不吞段——停止听写并提示
            this.lastSeq = Math.max(this.lastSeq, seq);
            this._callbacks.onError?.(this._callbacks.describeError?.("readonly") ?? "");
            void this.stop();
            return;
        }
        this.segments.push(text);
        this.lastSeq = Math.max(this.lastSeq, seq);
        this._callbacks.onSegmentAppended?.();
    }

    _finish() {
        const count = this.segments.length;
        // 听写一结束即冻结本轮真实范围：此后用户编辑使 Engine 复核拒绝应用，
        // 但范围本身不再依赖点击时的全文 indexOf 猜测（0.23.6 §5.7）。
        this.runHandle = count > 0 ? (this._adapter.freezeDictationRun?.() ?? null) : null;
        this._setPhase("idle");
        if (count > 0) {
            this._callbacks.onEnded?.({count});
        } else {
            this._clearRun();
        }
    }

    /** 用户关闭 chips：丢弃本次听写范围缓存（不回撤正文）。 */
    clearRunResult() {
        this.segments = [];
        this.runHandle = null;
    }

    /**
     * 本轮范围失效（视图切换后引擎已更换，无跨引擎 anchor 迁移，§3.3）：
     * 清空段缓存与冻结范围，chips 随之隐藏。
     */
    invalidateRun() {
        this.segments = [];
        this.runHandle = null;
    }

    _setPhase(phase) {
        if (this.phase === phase) return;
        this.phase = phase;
        this._callbacks.onPhaseChanged?.(phase);
    }

    _clearRun() {
        this.sessionRef = null;
        this.generation = 0;
        this.epoch = 0;
        this.lastSeq = 0;
        this.segments = [];
        this.runHandle = null;
    }

    _describe(code, message) {
        if (this._callbacks.describeError) return this._callbacks.describeError(code, message);
        return message || code;
    }
}
