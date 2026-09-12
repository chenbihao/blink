/**
 * 编辑器生命周期收口（0.23.6 恢复链路修复）。
 *
 * 从 main.js 装配层抽出的两个可测控制器：
 *
 * 1. `EditorLifecycle.endSession` —— 会话结束的草稿安全协议：
 *    - `retainDraft`（来源失效强制关闭）路径：dirty 正文必须先经
 *      `flushVerified()` 确认可靠落盘，`markOrphaned()` 显式转存成功，
 *      才允许发起 `session.end()`；任何一步失败都返回 `{ok:false}`，
 *      编辑器保持可编辑、自动保存保持绑定（不静默关闭、不谎称已保留草稿）；
 *    - 只有 `session.end()` 成功（或后端明确 stale_session）后才解除
 *      草稿绑定（`draft.orphan()`）——end 失败时正文与草稿身份都不丢；
 *    - `clearDraft` / clean 收尾走 `discardCurrent()` / `discardIfBodyEquals()`，
 *      与候选清理（discardCandidate）严格分离。
 *
 * 2. `EditorExit.handleRequest` —— 用户退出确认（请求-应答协议）：
 *    - 无会话或 clean 快速放行；
 *    - dirty 时一次汇总确认；用户确认后必须 `await flushVerified()`，
 *      落盘失败/过期/等待期间继续编辑/会话变化 → `confirmed:false`
 *      阻止退出并提示（后端结束进程前正文已可靠落盘）。
 */

/**
 * 会话结束 + 草稿收尾编排。
 *
 * @param {object} deps
 * @param {*} deps.session - EditorSession（isActive / isDirty 经 adapter / end）
 * @param {*} deps.adapter - EditorAdapter（isDirty / getText）
 * @param {*} deps.draft - RecoveryDraft（flushVerified / markOrphaned / orphan /
 *   discardCurrent / discardIfBodyEquals）
 */
export class EditorLifecycle {
    constructor({session, adapter, draft, setEditingLocked}) {
        this._session = session;
        this._adapter = adapter;
        this._draft = draft;
        this._setEditingLocked = setEditingLocked
            ?? ((locked) => this._adapter.setInteractionLocked?.(locked));
    }

    /**
     * 结束会话。只有显式放弃或成功提交才可请求清理（clearDraft）；
     * 来源失效路径传 `retainDraft`，clean 会话也只有在磁盘草稿等于
     * 权威正文时才清理。
     *
     * 返回 `{ok:false}` 时调用方必须保持窗口可编辑（自动保存未解绑）。
     * @param {"saved"|"abandoned"} reason
     * @param {{clearDraft?: boolean, retainDraft?: boolean}} [opts]
     * @returns {Promise<{ok: boolean, stale?: boolean, error?: {code: string, message?: string}}>}
     */
    async endSession(reason = "abandoned", {clearDraft = false, retainDraft = false} = {}) {
        const wasClean = this._session.isActive && !this._adapter.isDirty();
        const authoritativeBody = this._session.isActive ? this._adapter.getText() : null;

        let locked = false;
        const frozen = this._session.isActive
            ? {sessionRef: this._session.sessionRef, generation: this._session.generation}
            : null;
        if (retainDraft && this._session.isActive && !wasClean) {
            // 从落盘验证开始冻结输入，直到 orphan + end 全部完成；否则用户可能
            // 在两个 await 之间继续输入，导致已验证旧正文后关闭窗口。
            this._setEditingLocked(true);
            locked = true;
            const flushed = await this._draft.flushVerified();
            if (!flushed.ok) {
                this._setEditingLocked(false);
                return {
                    ok: false,
                    error: {
                        code: "draft_flush_failed",
                        message: flushed.reason ?? "",
                        detail: flushed.error,
                    },
                };
            }
            if (!this._sameSession(frozen)) {
                this._setEditingLocked(false);
                return {ok: false, error: {code: "draft_flush_failed", message: "stale_session"}};
            }
            // 落盘成功后显式转存为 orphan 候选；失败即中止（不得声称已保留）。
            const marked = await this._draft.markOrphaned();
            if (!marked.marked || !this._sameSession(frozen)) {
                this._setEditingLocked(false);
                return {ok: false, error: {code: "draft_orphan_failed"}};
            }
        }

        const result = await this._session.end(reason);
        if (!result.ok) {
            // end 失败：会话身份保留、自动保存仍绑定当前会话、草稿身份不丢。
            if (locked) this._setEditingLocked(false);
            return result;
        }
        if (retainDraft) {
            // 只有 end 成功（含 stale_session 复位）才解除草稿绑定。
            this._draft.orphan();
        }
        if (clearDraft) {
            await this._draft.discardCurrent();
        } else if (!retainDraft && wasClean && authoritativeBody != null) {
            await this._draft.discardIfBodyEquals(authoritativeBody);
        }
        if (locked) this._setEditingLocked(false);
        return result;
    }

    _sameSession(frozen) {
        return !!frozen && this._session.isActive
            && this._session.sessionRef === frozen.sessionRef
            && this._session.generation === frozen.generation;
    }
}

/**
 * 用户退出确认控制器（§3.5 请求-应答）。
 *
 * @param {object} deps
 * @param {{resolveEditorExit: (req: {requestId: string, confirmed: boolean}) => Promise<*>}} deps.api
 * @param {() => {isActive: boolean, sessionRef: string|null, generation: number}|null} deps.getSession
 * @param {() => boolean} deps.isDirty
 * @param {() => Promise<{ok: boolean}>} deps.flushVerified - 验证式落盘当前正文
 * @param {() => Promise<"ok"|"cancel">} deps.showDialog - 退出确认对话框（可注入）
 * @param {(message: string) => void} [deps.onStatus]
 * @param {(key: string, params?: object) => string} [deps.t]
 */
export class EditorExit {
    constructor({api, getSession, isDirty, flushVerified, showDialog, setEditingLocked, onStatus, t}) {
        this._api = api;
        this._getSession = getSession;
        this._isDirty = isDirty;
        this._flushVerified = flushVerified;
        this._showDialog = showDialog;
        this._setEditingLocked = setEditingLocked ?? (() => {});
        this._onStatus = onStatus ?? (() => {});
        this._t = t ?? ((key, params) => key);
        this._dialogOpen = false;
    }

    /**
     * 处理一次退出确认请求。无会话/clean 直接放行；dirty 时展示一次汇总
     * 确认，确认后验证式落盘——任何失败都以 `confirmed:false` 应答，
     * 阻止退出（超时兜底在后端，迟到应答被后端忽略）。
     * @param {{requestId?: string}} payload
     */
    async handleRequest(payload) {
        const requestId = payload?.requestId;
        if (!requestId) return;
        const session = this._getSession();
        if (!session?.isActive || !this._isDirty()) {
            await this._resolve(requestId, true);
            return;
        }
        if (this._dialogOpen) return; // 已有一次确认在展示，忽略重复请求
        this._dialogOpen = true;
        const frozen = {sessionRef: session.sessionRef, generation: session.generation};
        let confirmed = false;
        let locked = false;
        try {
            const choice = await this._showDialog();
            confirmed = choice === "ok";
            if (confirmed) {
                // 等待期间会话被替换：旧确认不得批准新会话的退出。
                const now = this._getSession();
                const sameSession = now?.isActive
                    && now.sessionRef === frozen.sessionRef
                    && now.generation === frozen.generation;
                if (!sameSession) {
                    confirmed = false;
                } else {
                    // 退出会结束进程：必须真正等到最新正文可靠落盘。
                    this._setEditingLocked(true);
                    locked = true;
                    const verified = await this._flushVerified();
                    if (!verified.ok) {
                        confirmed = false;
                        this._onStatus(this._t("editor.draft.exitFlushFailed"));
                    } else {
                        const afterFlush = this._getSession();
                        confirmed = afterFlush?.isActive === true
                            && afterFlush.sessionRef === frozen.sessionRef
                            && afterFlush.generation === frozen.generation;
                        if (!confirmed) {
                            this._onStatus(this._t("editor.draft.exitFlushFailed"));
                        }
                    }
                }
            }
        } finally {
            this._dialogOpen = false;
        }
        const accepted = await this._resolve(requestId, confirmed);
        // 后端已接受 confirmed=true 时会立即退出进程，保持锁定可彻底关闭
        // “验证成功到进程退出”之间的最后输入缝隙。拒绝/失败则恢复编辑。
        if (locked && (!confirmed || !accepted)) this._setEditingLocked(false);
    }

    async _resolve(requestId, confirmed) {
        try {
            return await this._api.resolveEditorExit({requestId, confirmed}) === true;
        } catch (e) {
            // 应答失败只记日志：后端超时会自行放弃本次退出，不丢正文。
            console.error(`[editor-exit] 退出确认应答失败: ${e instanceof Error ? e.message : String(e)}`);
            return false;
        }
    }
}
