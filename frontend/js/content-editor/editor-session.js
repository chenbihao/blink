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

export class EditorSession {
    /** 当前会话引用（后端 opaque）；null = 无活动会话 */
    sessionRef = null;

    /** 会话代际 */
    generation = 0;

    /** 来源描述符（快照下发） */
    source = null;

    /** 便签来源 revision（原位保存冲突基线） */
    sourceRevision = null;

    /** commit 进行中（save 切片） */
    saving = false;

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

        this.sessionRef = snap.sessionRef;
        this.generation = snap.generation ?? 0;
        this.source = snap.source ?? null;
        this.sourceRevision = snap.sourceRevision ?? null;

        this.adapter.loadInitial({
            body: snap.body ?? "",
            markdownPolicy: snap.markdownPolicy ?? "available",
        });
        this._callbacks.onTitle?.(snap.title || "");
        this._callbacks.onStatus?.("");
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
                this._callbacks.onSessionCleared?.();
            }
        }
    }

    /**
     * 提交正文（保存目标由后端按来源分派）。
     * 代际防护：await 前捕获会话身份，返回时身份已变则不落任何状态。
     * @returns {Promise<{ok: boolean, stale?: boolean, error?: {code, message}}>}
     */
    async commit() {
        if (this.saving || !this.isActive) return {ok: false};
        this.saving = true;

        const captured = {
            sessionRef: this.sessionRef,
            generation: this.generation,
            revision: this.adapter.revision,
            body: this.adapter.getText(),
        };

        try {
            const outcome = await this.api.commitEditorSession(captured);
            if (captured.sessionRef !== this.sessionRef) {
                return {ok: true, stale: true};
            }
            this.adapter.setCheckpoint(captured.body);
            if (outcome?.sourceRevision != null) {
                this.sourceRevision = outcome.sourceRevision;
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
     * 结束会话（保存后结束或明确放弃）。先本地清空再通知后端——
     * 即使 IPC 失败，窗口侧也回到无会话态，后端会话由下次 open 覆盖。
     * @param {"saved"|"abandoned"} reason
     */
    async end(reason = "abandoned") {
        if (!this.isActive) return;
        const request = {
            sessionRef: this.sessionRef,
            generation: this.generation,
            reason,
        };
        this._resetLocal();
        try {
            await this.api.endEditorSession(request);
        } catch (e) {
            const err = normalizeError(e);
            console.warn(`[editor-session] end 失败 [${err.code}]: ${err.message}`);
        }
    }

    /** 本地完整清空：状态 + Adapter（正文/undo/selection/监听/检查点） */
    _resetLocal() {
        this.sessionRef = null;
        this.generation = 0;
        this.source = null;
        this.sourceRevision = null;
        this.adapter.reset();
        this._callbacks.onStatus?.("");
    }

    /**
     * 外部内容同步（便签在会话外被改且本地 clean）：替换正文并前移检查点。
     */
    syncExternalContent(text) {
        if (!this.isActive) return;
        this.adapter.syncFromExternal(text);
        this._callbacks.onStatus?.("");
    }
}
