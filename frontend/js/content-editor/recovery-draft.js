/**
 * RecoveryDraft —— 与 Ctrl+S **分离**的实时恢复草稿控制器。
 *
 * ## 为什么需要它
 *
 * 修复前工作正文只存在前端内存里（`EditorSession` 只在显式保存时才从适配器
 * 取正文），崩溃 / 异常退出 / 系统强制结束都会丢掉用户正在写的内容。
 * 本次修复新增独立的恢复草稿通道：
 *
 * - 输入后 **300–750ms 防抖**自动落盘（默认 500ms）；
 * - 切换视图、隐藏、关闭、失焦等关键边界**强制 flush**；
 * - 用**单调 revision + 内容摘要**做水位，旧异步写入不得覆盖新正文；
 * - 后端原子持久化（`infra::utils::fs::atomic_write_bytes`），崩溃后可恢复；
 * - **只调用草稿专用 IPC**，绝不触发文件 / 剪贴板 / 便签等保存目标副作用
 *   （Ctrl+S 与草稿是两条独立链路）。
 *
 * ## 生命周期（清晰、可测）
 *
 * ```
 * 会话绑定 → bind()          → 可选 restore()（崩溃恢复）
 * 用户输入 → schedule()      → 防抖后 flush()
 * 关键边界 → flush()         → 立即落盘
 * 落盘必须确认 → flushVerified()  → 强制结束会话/退出确认前的验证式落盘
 * 提交成功 → discardCurrent()     → 仅清理当前会话最新水位对应的草稿
 * 用户明确放弃 → discardCurrent() → 用户确认后清理草稿
 * 冻结的旧候选 → discardCandidate(wall) → 恢复冲突"保留当前/复制"后精确清理
 * 来源失效/异常关闭 → orphan() → 保留草稿，交给恢复候选列表
 * 异常退出 → （无 discard）  → 草稿留在磁盘，下次 bind() 可恢复
 * ```
 */

/** 默认防抖（毫秒）；phase 要求的 300–750ms 区间内 */
export const DRAFT_DEBOUNCE_MS = 500;
export const DRAFT_MIN_DEBOUNCE_MS = 300;
export const DRAFT_MAX_DEBOUNCE_MS = 750;

/**
 * 草稿键由后端随 EditorSessionSnapshot 下发。只有 sticky 的稳定键允许在
 * 旧快照兼容路径中从来源推导；临时来源没有后端键时宁可不落盘，也不猜
 * `empty`/`selection`/`clipboard`/Capability 的身份。
 * @param {object|null} source - `SourceDescriptor` 的 camelCase 投影
 * @returns {string|null}
 */
export function draftKeyForSource(source, snapshot = null) {
    if (snapshot?.draftKey) return String(snapshot.draftKey);
    if (source?.draftKey) return String(source.draftKey);
    if (!source || typeof source !== "object") return null;
    switch (source.kind) {
        case "sticky":
            return source.stickyId ? `sticky:${source.stickyId}` : null;
        default:
            return null;
    }
}

/**
 * Read-fence identity check shared by the editor orchestration and tests.
 * Session/source identity changes drop a late response entirely; content
 * changes are handled separately as an explicit recovery conflict.
 */
export function recoverySourceMatches(frozen, current) {
    return !!current
        && current.sessionRef === frozen?.sessionRef
        && current.generation === frozen?.generation
        && current.key === frozen?.key
        && current.sourceInstanceId === frozen?.sourceInstanceId;
}

/** Full read fence, including the body/revision observed before disk I/O. */
export function recoveryFenceMatches(frozen, current) {
    return current?.sessionActive === true
        && recoverySourceMatches(frozen, current)
        && current.sourceRevision === frozen?.sourceRevision
        && current.adapterRevision === frozen?.adapterRevision
        && current.body === frozen?.body
        && current.bodyHash === frozen?.bodyHash;
}

export class RecoveryDraft {
    /** 已成功落盘的水位（revision + 内容摘要） */
    _persistedRevision = 0;
    _persistedHash = null;

    /** 串行队列：保证写入顺序，旧写入不会与新写入交错 */
    _tail = Promise.resolve();

    /** 当前会话身份（bind 时冻结；end/清空后置 null） */
    _identity = null;

    /** 当前草稿键（end 后仍保留一份用于清理） */
    _key = null;

    /** 后端生成的来源实例身份 */
    _sourceInstanceId = null;

    /** 最近读到/写入的草稿身份，clear 必须携带完整水位 */
    _loadedDraft = null;

    /** 防止旧磁盘响应在新 bind 后污染 loaded identity */
    _bindEpoch = 0;

    _timer = null;

    /** 大文档 MD：不做高频物化，只等关键边界 flush */
    _deferred = false;

    /**
     * @param {object} deps
     * @param {*} deps.api - `{saveEditorDraft, loadEditorDraft, clearEditorDraft,
     *   orphanEditorDraft?, archiveEditorDraft?}`
     * @param {() => {sessionRef: string, generation: number}|null} deps.getIdentity
     * @param {() => string} deps.getText - 取 canonical 正文（会触发 MD 延迟物化）
     * @param {() => number} deps.getRevision - 取单调内容版本
     * @param {() => string} deps.getCheckpoint - 取来源 checkpoint，而非工作正文
     * @param {() => number|null} deps.getSourceRevision - 取持久来源 revision
     * @param {() => boolean} deps.isDirty - 正文是否与基线不同（clean 不必落草稿）
     * @param {(text: string) => string} deps.hashOf - 内容摘要
     * @param {object} [deps.timers] - `{setTimeout, clearTimeout}`（测试注入）
     * @param {number} [deps.debounceMs]
     * @param {(error: {code: string, message: string}) => void} [deps.onError]
     */
    constructor({
        api,
        getIdentity,
        getText,
        getRevision,
        getCheckpoint = () => "",
        getSourceRevision = () => null,
        isDirty,
        hashOf,
        timers = {},
        debounceMs = DRAFT_DEBOUNCE_MS,
        onError,
    }) {
        this._api = api;
        this._getIdentity = getIdentity;
        this._getText = getText;
        this._getRevision = getRevision;
        this._getCheckpoint = getCheckpoint;
        this._getSourceRevision = getSourceRevision;
        this._isDirty = isDirty;
        this._hashOf = hashOf;
        this._setTimeout = timers.setTimeout ?? ((fn, ms) => setTimeout(fn, ms));
        this._clearTimeout = timers.clearTimeout ?? ((id) => clearTimeout(id));
        this._debounceMs = Math.min(
            Math.max(debounceMs, DRAFT_MIN_DEBOUNCE_MS),
            DRAFT_MAX_DEBOUNCE_MS,
        );
        this._onError = onError ?? null;
    }

    /** 是否有未落盘的排期 */
    get pending() {
        return this._timer !== null || this._deferred;
    }

    /** 当前草稿键（诊断/测试用） */
    get key() {
        return this._key;
    }

    /**
     * 会话绑定：冻结身份与键，重置水位（新会话的 revision 从自己的序列开始）。
     * @param {{sessionRef: string, generation: number, key: string|null,
     *          sourceInstanceId?: string}} identity
     */
    bind(identity) {
        this.cancelTimer();
        this._bindEpoch += 1;
        this._identity = identity?.sessionRef ? {...identity} : null;
        this._key = identity?.key ?? null;
        this._sourceInstanceId = identity?.sourceInstanceId ?? null;
        this._loadedDraft = null;
        this._persistedRevision = 0;
        this._persistedHash = null;
        this._deferred = false;
    }

    /** 会话结束/清空：停止排期（草稿本身由 discard 决定去留） */
    unbind() {
        this.cancelTimer();
        this._bindEpoch += 1;
        this._identity = null;
        this._deferred = false;
    }

    cancelTimer() {
        if (this._timer !== null) {
            this._clearTimeout(this._timer);
            this._timer = null;
        }
    }

    /**
     * 内容变化后调用：排期防抖落盘。
     * @param {{defer?: boolean}} [opts] - `defer=true` 时不排期（大文档 MD），
     *   仅记录"下一个关键边界需要 flush"
     * @returns {boolean} 是否已排期/已标记
     */
    schedule({defer = false} = {}) {
        if (!this._identity || !this._key) return false;
        if (defer) {
            this._deferred = true;
            return true;
        }
        this._deferred = false;
        // 真防抖：每次输入都重置窗口（持续输入时不中途落盘，停顿后才写）
        this.cancelTimer();
        this._timer = this._setTimeout(() => {
            this._timer = null;
            void this.flush();
        }, this._debounceMs);
        return true;
    }

    /**
     * 立即落盘（关键边界：切换视图 / 隐藏 / 关闭 / 失焦 / 会话结束前）。
     * 经串行队列执行，写入前复核会话身份，旧写入不得覆盖新正文。
     * @returns {Promise<{saved: boolean, stale?: boolean, error?: object}>}
     */
    flush() {
        this.cancelTimer();
        this._deferred = false;
        if (!this._identity || !this._key) return Promise.resolve({saved: false});
        if (!this._isDirty()) return Promise.resolve({saved: false});

        const text = this._getText();
        const revision = this._getRevision();
        const hash = this._hashOf(text);
        // The draft records the checkpoint from which the user edited.  Using
        // the current working body here would make a changed sticky appear
        // safe after a crash and would permit an overwrite on restore.
        const baseDigest = this._hashOf(this._getCheckpoint());
        const baseRevision = this._getSourceRevision();

        // 无新内容（水位已覆盖同一 revision + 同一摘要）→ 不重复写盘
        if (revision <= this._persistedRevision && hash === this._persistedHash) {
            return Promise.resolve({saved: false});
        }

        const identity = {...this._identity, key: this._key};
        const run = () => this._write(identity, text, revision, hash, baseDigest, baseRevision);
        const task = this._tail.then(run, run);
        this._tail = task.then(() => {}, () => {});
        return task;
    }

    /** 实际写入（串行队列内执行） */
    async _write(identity, text, revision, hash, baseDigest, baseRevision) {
        if (!this._isCurrent(identity)) return {saved: false, stale: true};
        try {
            const result = await this._api.saveEditorDraft({
                key: identity.key,
                sessionRef: identity.sessionRef,
                generation: identity.generation,
                revision,
                hash,
                body: text,
                baseDigest,
                baseRevision,
                sourceInstanceId: identity.sourceInstanceId ?? this._sourceInstanceId ?? "",
                schemaVersion: 1,
            });
            if (!this._isCurrent(identity)) return {saved: true, stale: true};
            const stored = Number(result?.storedRevision ?? revision);
            if (stored >= this._persistedRevision) {
                this._persistedRevision = stored;
                this._persistedHash = hash;
                this._loadedDraft = {
                    key: identity.key,
                    sessionRef: identity.sessionRef,
                    generation: identity.generation,
                    revision: stored,
                    hash,
                    body: text,
                };
            }
            return {saved: true};
        } catch (e) {
            const err = normalizeDraftError(e);
            this._onError?.(err);
            return {saved: false, error: err};
        }
    }

    /**
     * 读取并分类草稿。返回候选不等于自动应用；只有 schema/base 与当前
     * canonical checkpoint 一致的结果才允许调用方自动恢复。
     * 空字符串是合法工作正文，不能作为“无草稿”哨兵。
     * 返回值携带 `identity`（本次读到的完整版本墙），供调用方冻结后交给
     * `discardCandidate` 做精确清理——执行时不得重新读取后续状态。
     * @param {{key: string, authoritativeBody: string, authoritativeDigest?: string,
     *          authoritativeRevision?: number|null}} opts
     * @returns {Promise<{status: "restore"|"conflict"|"same", text: string,
     *          revision: number, hash: string, trusted: boolean, untrusted?: boolean,
     *          orphaned?: boolean, identity: {key: string, sessionRef: string,
     *          generation: number, revision: number, hash: string}}|null>}
     */
    restore(options) {
        const run = () => this._restore(options);
        const task = this._tail.then(run, run);
        this._tail = task.then(() => {}, () => {});
        return task;
    }

    async _restore({key, authoritativeBody, authoritativeDigest, authoritativeRevision = null}) {
        if (!key) return null;
        const bindEpoch = this._bindEpoch;
        let draft;
        try {
            draft = await this._api.loadEditorDraft(key);
        } catch (e) {
            this._onError?.(normalizeDraftError(e));
            return null;
        }
        if (!draft || draft.key !== key) return null;
        if (bindEpoch !== this._bindEpoch || this._key !== key) return null;
        if (typeof draft.body !== "string") return null;
        const hash = this._hashOf(draft.body);
        const revision = Number(draft.revision ?? 0);
        const identity = {
            key,
            sessionRef: String(draft.sessionRef ?? ""),
            generation: Number(draft.generation ?? 0),
            revision,
            hash,
        };
        this._loadedDraft = {...identity, body: draft.body};
        if (draft.body === authoritativeBody) return {
            status: "same",
            text: draft.body,
            revision,
            hash,
            trusted: true,
            identity,
        };
        const baseDigest = typeof draft.baseDigest === "string" ? draft.baseDigest : "";
        const orphaned = draft.orphaned === true;
        const trusted = !orphaned && Number(draft.schemaVersion ?? 0) === 1 && baseDigest.length > 0;
        const baselineMatches = trusted
            && baseDigest === (authoritativeDigest ?? this._hashOf(authoritativeBody))
            && ((draft.baseRevision == null && authoritativeRevision == null)
                || (draft.baseRevision != null
                    && authoritativeRevision != null
                    && Number(draft.baseRevision) === Number(authoritativeRevision)));
        const status = baselineMatches ? "restore" : "conflict";
        // 冲突候选先迁移到独立 orphan 键，再交给 UI 等待用户决定。
        // 这样弹窗停留期间当前会话的自动保存仍可安全写原键，不会覆盖候选。
        if (status === "conflict" && typeof this._api.archiveEditorDraft === "function") {
            try {
                const archivedKey = await this._api.archiveEditorDraft({
                    key: identity.key,
                    sessionRef: identity.sessionRef,
                    generation: identity.generation,
                    expectedRevision: identity.revision,
                    expectedHash: identity.hash,
                });
                if (typeof archivedKey === "string" && archivedKey.length > 0) {
                    identity.key = archivedKey;
                }
            } catch (e) {
                this._onError?.(normalizeDraftError(e));
            }
        }
        return {
            status,
            text: draft.body,
            revision,
            hash,
            trusted: baselineMatches,
            untrusted: !trusted,
            orphaned,
            identity,
        };
    }

    /**
     * 精确清理一个冻结的候选草稿（恢复冲突弹窗的"保留当前/复制候选"、
     * "same" 探测命中）。只携带冻结时捕获的完整版本墙
     * （key + sessionRef + generation + revision + hash），**绝不**读取执行
     * 时刻的 `_loadedDraft` / 水位——弹窗停留期间自动保存写入的新草稿
     * 不允许被旧候选的清理误删（后端 clear_if_matches 身份不匹配时拒绝）。
     * 不取消当前排期、不改当前水位：这是对"别的草稿"的操作。
     * @param {{key: string, sessionRef: string, generation: number,
     *          revision: number, hash: string}} candidate - 冻结的候选身份
     * @returns {Promise<{cleared: boolean}>}
     */
    discardCandidate(candidate) {
        const wall = normalizeCandidateWall(candidate);
        if (!wall) return Promise.resolve({cleared: false});
        const run = async () => {
            try {
                const cleared = await this._api.clearEditorDraft({
                    key: wall.key,
                    sessionRef: wall.sessionRef,
                    generation: wall.generation,
                    expectedRevision: wall.revision,
                    expectedHash: wall.hash,
                });
                return {cleared: cleared === true};
            } catch (e) {
                this._onError?.(normalizeDraftError(e));
                return {cleared: false};
            }
        };
        const task = this._tail.then(run, run);
        this._tail = task.then(() => {}, () => {});
        return task;
    }

    /**
     * 清理当前会话已落盘水位对应的草稿（成功提交 / 明确放弃 / 结束清理）。
     * 无论会话是否仍然活动都按 bind 时冻结的键清理；排队的 flush 完成后
     * 使用同一身份的最新水位（提交成功清理必须覆盖最后一次自动保存）。
     */
    discardCurrent() {
        this.cancelTimer();
        this._deferred = false;
        const key = this._key;
        const identity = this._identity ? {
            sessionRef: this._identity.sessionRef,
            generation: this._identity.generation,
        } : null;
        const initialExpected = this._loadedDraft ?? (
            this._identity ? {
                sessionRef: this._identity.sessionRef,
                generation: this._identity.generation,
                revision: this._persistedRevision,
                hash: this._persistedHash,
            } : null
        );
        this._persistedRevision = 0;
        this._persistedHash = null;
        if (!key || (!identity && !initialExpected)) {
            return Promise.resolve({cleared: false});
        }

        const run = async () => {
            // A flush already queued before discard may finish while this task
            // waits in _tail.  Use that newer draft from the same identity;
            // never borrow state from a later session that reused the key.
            const loaded = this._loadedDraft;
            const expected = loaded
                && loaded.key === key
                && loaded.sessionRef === identity?.sessionRef
                && loaded.generation === identity?.generation
                ? loaded
                : initialExpected;
            if (!expected || !expected.hash || !Number.isFinite(expected.revision)) {
                return {cleared: false};
            }
            try {
                const cleared = await this._api.clearEditorDraft({
                    key,
                    sessionRef: expected.sessionRef ?? null,
                    generation: expected.generation ?? 0,
                    expectedRevision: expected.revision,
                    expectedHash: expected.hash,
                });
                return {cleared: cleared === true};
            } catch (e) {
                this._onError?.(normalizeDraftError(e));
                return {cleared: false};
            }
        };
        const task = this._tail.then(run, run);
        this._tail = task.then(() => {}, () => {});
        return task;
    }

    /** 来源失效/异常关闭：保留已落盘草稿，解除当前会话绑定。 */
    orphan() {
        this.cancelTimer();
        this._bindEpoch += 1;
        this._deferred = false;
        this._identity = null;
    }

    /**
     * 来源失效时把已落盘草稿显式标记为 orphan，供候选列表发现；
     * 后端仍会复核完整身份，失败时不会碰当前草稿。
     */
    markOrphaned() {
        const key = this._key;
        const expected = this._loadedDraft ?? (
            this._identity ? {
                sessionRef: this._identity.sessionRef,
                generation: this._identity.generation,
                revision: this._persistedRevision,
                hash: this._persistedHash,
            } : null
        );
        if (!key || !expected || !expected.hash || !Number.isFinite(expected.revision)
            || typeof this._api.orphanEditorDraft !== "function") {
            return Promise.resolve({marked: false});
        }
        const run = async () => {
            try {
                const marked = await this._api.orphanEditorDraft({
                    key,
                    sessionRef: expected.sessionRef ?? null,
                    generation: expected.generation ?? 0,
                    expectedRevision: expected.revision,
                    expectedHash: expected.hash,
                });
                return {marked: marked === true};
            } catch (e) {
                this._onError?.(normalizeDraftError(e));
                return {marked: false};
            }
        };
        const task = this._tail.then(run, run);
        this._tail = task.then(() => {}, () => {});
        return task;
    }

    /**
     * 验证式落盘（强制结束会话 / 退出确认等"落盘失败就必须停下"的边界）。
     * 只有确认当前 dirty 正文可靠落盘（或本就 clean）才返回 `{ok: true}`：
     * - clean → 无可丢失正文；
     * - 水位已覆盖同一 revision + 摘要 → 本会话已写过且未被清理；
     * - 真实写入成功，且等待期间正文/revision 未再变化。
     * 写入报错、写入后身份过期（stale）、等待期间继续编辑、或写完水位未
     * 覆盖冻结正文，都返回 `{ok: false}`——调用方不得继续危险动作。
     * @returns {Promise<{ok: boolean, clean?: boolean, saved?: boolean,
     *          alreadyPersisted?: boolean, reason?: string, error?: object}>}
     */
    async flushVerified() {
        if (!this._identity || !this._key) return {ok: false, reason: "unbound"};
        if (!this._isDirty()) return {ok: true, clean: true};
        const text = this._getText();
        const revision = this._getRevision();
        const hash = this._hashOf(text);
        if (revision <= this._persistedRevision && hash === this._persistedHash) {
            return {ok: true, alreadyPersisted: true};
        }
        const result = await this.flush();
        if (result?.error) return {ok: false, reason: "error", error: result.error};
        if (result?.stale) return {ok: false, reason: "stale"};
        if (this._getRevision() !== revision || this._hashOf(this._getText()) !== hash) {
            // 等待落盘期间用户继续编辑：旧正文的落盘结果不得批准本次动作。
            return {ok: false, reason: "changed"};
        }
        if (this._persistedRevision >= revision && this._persistedHash === hash) {
            return {ok: true, saved: true};
        }
        return {ok: false, reason: "unconfirmed"};
    }

    /**
     * clean 会话结束时的安全清理：只有磁盘草稿正文与权威正文完全相同
     * 才允许 clear；找不到或不同则保留为恢复候选。
     */
    async discardIfBodyEquals(authoritativeBody) {
        if (!this._key) return {cleared: false};
        if (!this._loadedDraft) {
            try {
                const loaded = await this._api.loadEditorDraft(this._key);
                if (!loaded || loaded.key !== this._key || loaded.body !== authoritativeBody) {
                    return {cleared: false};
                }
                this._loadedDraft = {
                    key: this._key,
                    sessionRef: String(loaded.sessionRef ?? ""),
                    generation: Number(loaded.generation ?? 0),
                    revision: Number(loaded.revision ?? 0),
                    hash: this._hashOf(loaded.body),
                    body: loaded.body,
                };
            } catch (e) {
                this._onError?.(normalizeDraftError(e));
                return {cleared: false};
            }
        }
        if (this._loadedDraft.body !== authoritativeBody
            || this._loadedDraft.hash !== this._hashOf(authoritativeBody)) {
            return {cleared: false};
        }
        return this.discardCurrent();
    }

    /** 会话身份是否仍为冻结时的身份（旧会话的迟到写入据此丢弃） */
    _isCurrent(identity) {
        const now = this._getIdentity();
        if (!now || !this._identity) return false;
        return now.sessionRef === identity.sessionRef
            && now.generation === identity.generation
            && this._key === identity.key;
    }
}

/** 结构化错误归一（不依赖 tauri.js，保持模块可独立测试） */
function normalizeDraftError(e) {
    if (e && typeof e === "object" && typeof e.code === "string") {
        return {code: e.code, message: String(e.message ?? "")};
    }
    return {code: "unknown", message: e instanceof Error ? e.message : String(e ?? "")};
}

/**
 * 校验并归一候选草稿的完整版本墙。任何字段缺失/非有限数字 → null
 * （不允许按 key 粗暴清理，宁可不清）。
 */
function normalizeCandidateWall(candidate) {
    if (!candidate || typeof candidate !== "object") return null;
    const {key, sessionRef, generation, revision, hash} = candidate;
    if (typeof key !== "string" || key.length === 0) return null;
    if (typeof sessionRef !== "string" || sessionRef.length === 0) return null;
    if (!Number.isFinite(generation)) return null;
    if (!Number.isFinite(revision)) return null;
    if (typeof hash !== "string" || hash.length === 0) return null;
    return {key, sessionRef, generation: Number(generation), revision: Number(revision), hash};
}
