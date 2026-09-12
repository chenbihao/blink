/**
 * EditorSession（0.23.1，前端侧会话状态机，框架无关可测）。
 *
 * 职责：持有 session_ref + generation + 来源信息；驱动 Adapter 载入/复位；
 * commit/end 的 IPC 编排与代际防护——请求发出前捕获会话身份，回调返回时
 * 校验身份未变才落状态（旧异步结果不能污染新会话，§6.2 验收）。
 *
 * 正交状态不压缩成单一 phase（§3.2）：save 由 saving 表达，内容版本由
 * Adapter.revision 表达；lifecycle 以 sessionRef 是否为空判定。
 */

import {normalizeError} from "../shared/tauri.js";

/** 提交意图标识计数（module 级：跨会话单调，叠加时间戳防窗口重载后重复） */
let _mutationSeq = 0;

/**
 * 生成提交意图标识（0.23.6 二次 Review）：每次显式提交唯一。后端幂等只对
 * "同一 mutation 的精确重放"（相同 id）早退——同 revision、同正文但意图
 * 不同的新提交（切换目标/保存副本/覆盖/重新复制）必须真实执行。
 * @returns {string}
 */
function newMutationId() {
    _mutationSeq += 1;
    const rand = typeof crypto !== "undefined" && crypto.randomUUID
        ? crypto.randomUUID()
        : `${Date.now().toString(36)}${(_mutationSeq % 0xffff).toString(36)}`;
    return `mut-${rand}`;
}

export class EditorSession {
    /** 当前会话引用（后端 opaque）；null = 无活动会话 */
    sessionRef = null;

    /** 会话代际 */
    generation = 0;

    /** 后端生成的恢复草稿键/来源实例，前端不从来源描述符猜测 */
    draftKey = null;
    sourceInstanceId = null;

    /** 来源描述符（快照下发） */
    source = null;

    /** 便签来源 revision（原位保存冲突基线） */
    sourceRevision = null;

    /** 主保存目标（后端 CommitTarget，0.23.2；"保存到…"成功后切换） */
    target = null;

    /** commit 进行中（save 切片） */
    saving = false;

    /** 已观察到的最高后端 generation；reset 后保留，用于拒绝迟到快照复活旧会话。 */
    _latestGeneration = 0;

    /**
     * 同会话 mutation 串行队列（0.23.6 §5.7）：commit 与 end 排队执行，
     * 保存中发起的关闭/放弃/生命周期关闭会等待当前提交得到确定结果后再
     * end——后端不再出现"end 清槽后 commit 副作用迟到写入"的交错。
     */
    _mutationTail = Promise.resolve();

    /**
     * @param {object} deps
     * @param {*} deps.api - shared/api.js（测试可注入 fake）
     * @param {*} deps.adapter - EditorAdapter 实例
     * @param {object} callbacks
     * @param {(title: string) => void} [callbacks.onTitle]
     * @param {(message: string) => void} [callbacks.onStatus] - 状态行文案（已翻译）
     * @param {(message: string) => void} [callbacks.onError] - 错误提示（已翻译）
     * @param {() => void} [callbacks.onSessionCleared] - 会话清空（外部 ended）
     */
    constructor({api, adapter}, callbacks = {}) {
        this.api = api;
        this.adapter = adapter;
        this._callbacks = callbacks;
    }

    get isActive() {
        return this.sessionRef !== null;
    }

    /** 拉取当前会话快照（init 主动拉取路径）。无会话时不动作。 */
    async activate() {
        const snap = await this.api.getEditorSession();
        if (snap) this.applySnapshot(snap);
        return snap;
    }

    /**
     * 应用快照。同 sessionRef 的重复 bound（窗口重新激活）不重置正文。
     * @param {*} snap - 后端 EditorSessionSnapshot（camelCase）
     */
    applySnapshot(snap) {
        if (!snap || !snap.sessionRef) return;
        if (snap.sessionRef === this.sessionRef) return;

        const generation = Number(snap.generation ?? 0);
        if (!Number.isFinite(generation) || generation <= this._latestGeneration) return;

        this.sessionRef = snap.sessionRef;
        this.generation = generation;
        this._latestGeneration = generation;
        this.draftKey = typeof snap.draftKey === "string" ? snap.draftKey : null;
        this.sourceInstanceId = typeof snap.sourceInstanceId === "string"
            ? snap.sourceInstanceId
            : null;
        this.source = snap.source ?? null;
        this.sourceRevision = snap.sourceRevision ?? null;
        this.target = snap.commitTarget ?? null;

        this.adapter.loadInitial({
            body: snap.body ?? "",
            markdownPolicy: snap.markdownPolicy ?? "available",
        });
        this._callbacks.onTitle?.(snap.title || "");
        this._callbacks.onStatus?.("");
        this._callbacks.onTargetChanged?.(this.target);
        this._callbacks.onSnapshotApplied?.();
        this.adapter.focus();
    }

    /**
     * 编辑器会话事件入口（blink://editor-session-changed）。
     * bound 且是新区 → 拉快照应用；ended 且匹配当前会话 → 防御性清空
     * （正常由本窗口发起；带 sessionRef 匹配，迟到的旧事件不误清新会话）。
     */
    handleSessionEvent(payload) {
        if (!payload) return;
        if (payload.kind === "bound" && payload.sessionRef) {
            if (payload.sessionRef === this.sessionRef) return;
            this.activate().catch((e) => {
                const err = normalizeError(e);
                console.error(`[editor-session] 拉取快照失败 [${err.code}]: ${err.message}`);
            });
            return;
        }
        if (payload.kind === "ended") {
            if (payload.sessionRef && payload.sessionRef !== this.sessionRef) return;
            if (this.sessionRef) {
                this._resetLocal();
            }
        }
    }

    /**
     * 提交正文（保存目标由后端按主目标分派；可传单次覆盖，§3.5）。
     * 经 mutation 队列串行（④）；代际防护：调用时捕获会话身份，入队等待
     * 期间会话已切换则丢弃本次操作，执行后返回的身份校验兜底迟到结果。
     * @param {{kind: string, path?: string}|null} [targetOverride] - "保存到…"/"另存为副本…"/覆盖冲突文件
     * @returns {Promise<{ok: boolean, stale?: boolean, error?: {code, message}}>}
     */
    commit(targetOverride = null) {
        const captured = this.isActive
            ? {sessionRef: this.sessionRef, generation: this.generation}
            : null;
        return this._serializeMutation(() => this._commit(targetOverride, captured));
    }

    /** commit 实体（队列内执行，同时至多一个）。 */
    async _commit(targetOverride, capturedAtEnqueue) {
        if (this.saving || !this.isActive) return {ok: false};
        // 入队等待期间会话已切换：本次 commit 属于旧会话，不得携带旧意图
        //（如目标覆盖）作用于新会话。
        if (capturedAtEnqueue
            && (capturedAtEnqueue.sessionRef !== this.sessionRef
                || capturedAtEnqueue.generation !== this.generation)) {
            return {ok: false, stale: true};
        }
        this.saving = true;

        const request = {
            sessionRef: this.sessionRef,
            generation: this.generation,
            revision: this.adapter.revision,
            body: this.adapter.getText(),
            mutationId: newMutationId(),
        };
        if (targetOverride) request.target = targetOverride;
        const captured = request;

        try {
            const outcome = await this.api.commitEditorSession(captured);
            if (captured.sessionRef !== this.sessionRef) {
                return {ok: true, stale: true};
            }
            this.adapter.setCheckpoint(captured.body);
            if (outcome?.sourceRevision != null) {
                this.sourceRevision = outcome.sourceRevision;
            }
            if (outcome?.commitTarget) {
                this.target = outcome.commitTarget;
                this._callbacks.onTargetChanged?.(this.target);
            }
            return {ok: true};
        } catch (e) {
            const err = normalizeError(e);
            return {ok: false, error: err};
        } finally {
            this.saving = false;
        }
    }

    /**
     * 结束会话（保存后结束或明确放弃）。经 mutation 队列串行（④）：
     * 保存进行中时等待其确定结果后才发起 end，"放弃修改"之后不会再有
     * 该会话的便签/文件/剪贴板迟到写入。后端确认成功后才清本地状态；
     * IPC 失败时保留正文与会话身份，避免窗口隐藏后留下无法重新打开的
     * 后端活跃槽。
     * @param {"saved"|"abandoned"} reason
     */
    end(reason = "abandoned") {
        const captured = this.isActive
            ? {sessionRef: this.sessionRef, generation: this.generation}
            : null;
        return this._serializeMutation(() => this._end(reason, captured));
    }

    /** end 实体（队列内执行，同时至多一个）。 */
    async _end(reason, capturedAtEnqueue) {
        if (!this.isActive) return {ok: true};
        // 入队等待期间会话已切换：不得结束（清空）新会话。
        if (capturedAtEnqueue
            && (capturedAtEnqueue.sessionRef !== this.sessionRef
                || capturedAtEnqueue.generation !== this.generation)) {
            return {ok: true, stale: true};
        }
        const request = {
            sessionRef: this.sessionRef,
            generation: this.generation,
            reason,
        };
        try {
            await this.api.endEditorSession(request);
            if (request.sessionRef === this.sessionRef && request.generation === this.generation) {
                this._resetLocal();
            }
            return {ok: true};
        } catch (e) {
            const err = normalizeError(e);
            console.warn(`[editor-session] end 失败 [${err.code}]: ${err.message}`);
            // stale_session 表示后端已经不再拥有该会话，本地可安全复位。
            if (err.code === "stale_session"
                && request.sessionRef === this.sessionRef
                && request.generation === this.generation) {
                this._resetLocal();
                return {ok: true, stale: true};
            }
            return {ok: false, error: err};
        }
    }

    /** 排入 mutation 队列：前一个变更（含失败）完成后才执行下一个。 */
    _serializeMutation(run) {
        const result = this._mutationTail.then(run, run);
        this._mutationTail = result.then(() => {}, () => {});
        return result;
    }

    /** 本地完整清空：状态 + Adapter（正文/undo/selection/监听/检查点） */
    _resetLocal() {
        this.sessionRef = null;
        this.generation = 0;
        this.draftKey = null;
        this.sourceInstanceId = null;
        this.source = null;
        this.sourceRevision = null;
        this.target = null;
        this.adapter.reset();
        this._callbacks.onStatus?.("");
        this._callbacks.onSessionCleared?.();
    }

    /**
     * 外部内容同步（便签在会话外被改且本地 clean，或用户显式放弃重载）：
     * 替换正文并前移检查点；提供 revision 时一并前移冲突基线，
     * 后续保存不再误报冲突。
     */
    syncExternalContent(text, sourceRevision = null) {
        if (!this.isActive) return;
        this.adapter.syncFromExternal(text);
        if (sourceRevision != null) this.sourceRevision = sourceRevision;
        this._callbacks.onStatus?.("");
    }

    /**
     * 便签异步回载（0.23.6 双重版本墙，§5.7）。
     *
     * 发起时冻结 `session_ref + generation + sticky_id`；读取返回后重新校验
     * 会话身份——同一便签被重开（新 generation）或会话已切换时丢弃旧回流，
     * 禁止旧请求覆盖同一便签的新会话；非 force 路径在返回后重查 dirty，
     * 读取期间发生的任何用户编辑都丢弃旧回流。force（冲突后放弃修改的
     * 显式重载）跳过 dirty 重查，但同样受身份墙约束。
     *
     * @param {{stickyId: string, force?: boolean, getNote: (id: string) => Promise<*>}} opts
     * @returns {Promise<{applied: boolean, stale?: boolean, dirty?: boolean,
     *                     error?: {code: string, message: string}}>}
     */
    async reloadFromSticky({stickyId, force = false, getNote} = {}) {
        if (!this.isActive || !stickyId || typeof getNote !== "function") {
            return {applied: false};
        }
        const captured = {
            sessionRef: this.sessionRef,
            generation: this.generation,
            stickyId,
        };
        if (!force && this.adapter.isDirty()) return {applied: false, dirty: true};

        let note;
        try {
            note = await getNote(stickyId);
        } catch (e) {
            return {applied: false, error: normalizeError(e)};
        }
        if (!note) return {applied: false};

        const currentStickyId = this.source?.kind === "sticky" ? this.source.stickyId : null;
        const sameSession = captured.sessionRef === this.sessionRef
            && captured.generation === this.generation
            && captured.stickyId === currentStickyId;
        if (!sameSession) return {applied: false, stale: true};
        if (!force && this.adapter.isDirty()) return {applied: false, dirty: true};

        this.syncExternalContent(note.content || "", note.updatedAt ?? null);
        return {applied: true};
    }
}
