/**
 * 模型选择生命周期状态机（0.22.9 Handoff 09，纯 reducer）。
 *
 * 后端 `set_local_stt_selection` 已升级为跨 runtime 切换事务
 * （stop old → commit selected → start target → 失败回滚）。事务没有
 * 独立的 operation kind（复用 start/stop 内部路径），因此"切换中"的
 * 权威来源是**前端自身在途的 invoke**——本模块为其提供显式状态机，
 * 与 EngineStateEntry 并存（entry.selection 字段）。
 *
 * ## 状态（phase）
 *
 * - `switching`       用户已发起切换，事务在途（stop/commit/start 任一阶段）
 * - `rolled_back`     目标启动失败，已恢复旧模型（后端 code=switch_rolled_back）
 * - `rollback_failed` 目标失败且回滚也失败：selected=旧模型、active=None
 *                     （后端 code=switch_rollback_failed，双错误）
 * - 其余 Target 失败（未达回滚）不进入本状态机，走既有 transientError。
 *
 * selected model / active model / active implementation 不是本模块的状态——
 * 它们的真源分别是 models[].is_selected、models[].is_active 与
 * status.status.active_implementation（后端权威投影），本模块只表达
 * "用户动作在途/刚失败"这一层瞬时信息，绝不覆盖真源。
 *
 * ## 竞态防护（铁则）
 *
 * - **requestId 单调递增**：每次 begin 生成新 id；resolve 只接受与当前
 *   selection 匹配的 requestId——旧请求的迟到结果被丢弃，不得覆盖新状态。
 * - **新 begin 覆盖旧 phase**：用户重新发起切换时，旧失败态被整体替换。
 * - **状态事件不清除 switching**：LOCAL_ENGINE_STATUS / 模型列表刷新在
 *   事务期间到达（stop/commit/start 各阶段推送）时不改写 selection——
 *   事务期间 selected/active 处于中间态，任何"看起来对了"的快照都可能是
 *   事务的中间帧。
 * - **reconcile 兜底**：仅当命令结果丢失（IPC 异常断开）时，允许在
 *   收到"selected === active === target"的一致快照后收敛 switching——
 *   单一方向（switching → idle），失败态不由 reconcile 猜测清除。
 * - 失败态（rolled_back/rollback_failed）持续可见，直到下一次用户动作
 *   （begin 或 clear）——错误不能被无关状态刷新淹没。
 *
 * @module local-engine-selection
 */

// ── 常量 ─────────────────────────────────────────────────────────────────────

/**
 * selection.phase 闭合集合（fail-closed：未知 phase 视为不存在）。
 * @readonly
 */
export const SELECTION_PHASES = Object.freeze([
    "switching",
    "rolled_back",
    "rollback_failed",
]);

/**
 * 后端 switch 事务错误码 → 前端 phase 映射（0.22.9 Handoff 08 wire 契约）。
 * @readonly
 */
export const SWITCH_ERROR_PHASE = Object.freeze({
    switch_rolled_back: "rolled_back",
    switch_rollback_failed: "rollback_failed",
});

// ── requestId（模块级单调计数器）─────────────────────────────────────────────

let requestCounter = 0;

/**
 * 生成单调递增的切换请求 id。
 * 计数器保证同进程内严格递增（Date.now 在快速连点时可能重复）。
 * @returns {string}
 */
export function createSelectionRequestId() {
    requestCounter += 1;
    return `switch-${Date.now()}-${requestCounter}`;
}

// ── reducer ──────────────────────────────────────────────────────────────────

/**
 * 发起模型切换：进入 switching（覆盖旧的失败态）。
 *
 * @param {Map<string, Object>} state - engine_id → EngineStateEntry
 * @param {string} engineId
 * @param {{modelId: string, requestId: string}} request
 * @returns {Map<string, Object>}
 */
export function beginModelSwitch(state, engineId, request) {
    if (!engineId || !request?.modelId || !request?.requestId) return state;
    const entry = state.get(engineId) || createEmptyEntry();
    const selection = {
        phase: "switching",
        targetModelId: request.modelId,
        requestId: request.requestId,
        startedAt: Date.now(),
        error: null,
        detail: null,
    };
    return setEntry(state, engineId, {...entry, selection});
}

/**
 * 解决切换请求（命令 promise 落定后调用，恰好一次）。
 *
 * - requestId 不匹配（旧请求迟到）→ 原样返回，不覆盖。
 * - ok → 清除 selection（active/selected 真源由后续状态刷新投影）。
 * - SWITCH_ERROR_PHASE 命中 → 进入对应失败态并保留用户可读错误。
 * - 其他错误（Target 失败等）→ 清除 selection，错误由 transientError 展示。
 *
 * @param {Map<string, Object>} state
 * @param {string} engineId
 * @param {string} requestId - 发起时的请求 id
 * @param {{ok: boolean, errorCode?: string, error?: Object}} result
 * @returns {Map<string, Object>}
 */
export function resolveModelSwitch(state, engineId, requestId, result) {
    if (!engineId) return state;
    const entry = state.get(engineId);
    const selection = entry?.selection;
    if (!selection) return state;
    if (selection.requestId !== requestId) return state; // 旧请求迟到 → 丢弃

    if (result?.ok) {
        const next = {...entry};
        delete next.selection;
        return setEntry(state, engineId, next);
    }

    const phase = SWITCH_ERROR_PHASE[result?.errorCode];
    if (!phase) {
        // Target 失败等其他错误：不进 selection 状态机，交给 transientError
        const next = {...entry};
        delete next.selection;
        return setEntry(state, engineId, next);
    }

    return setEntry(state, engineId, {
        ...entry,
        selection: {
            ...selection,
            phase,
            error: result?.error || null,
            detail: result?.detail || null,
        },
    });
}

/**
 * 清除 selection（用户下一次动作前主动清理失败态时调用）。
 * @param {Map<string, Object>} state
 * @param {string} engineId
 * @returns {Map<string, Object>}
 */
export function clearSelection(state, engineId) {
    if (!engineId) return state;
    const entry = state.get(engineId);
    if (!entry?.selection) return state;
    const next = {...entry};
    delete next.selection;
    return setEntry(state, engineId, next);
}

/**
 * reconcile 兜底（单一方向：switching → idle）。
 *
 * 仅当当前 phase 为 switching 且模型列表给出完全一致的快照
 * （is_selected 与 is_active 都指向 target）时收敛——说明事务已完整
 * 落定而命令结果丢失。失败态不由本函数清除（不能猜测回滚是否完成）。
 *
 * @param {Map<string, Object>} state
 * @param {string} engineId
 * @returns {Map<string, Object>}
 */
export function reconcileSelection(state, engineId) {
    if (!engineId) return state;
    const entry = state.get(engineId);
    const selection = entry?.selection;
    if (!selection || selection.phase !== "switching") return state;

    const models = Array.isArray(entry?.models) ? entry.models : [];
    const target = models.find((m) => m.model_id === selection.targetModelId);
    if (!target || !target.is_selected || !target.is_active) return state;

    const next = {...entry};
    delete next.selection;
    return setEntry(state, engineId, next);
}

// ── 查询（纯读取）────────────────────────────────────────────────────────────

/**
 * 读取 selection（不存在的 phase 值 fail-closed 为 null）。
 * @param {Object|null} entry - EngineStateEntry
 * @returns {{phase: string, targetModelId: string, requestId: string,
 *            error?: Object, detail?: Object}|null}
 */
export function getSelection(entry) {
    const selection = entry?.selection;
    if (!selection || !SELECTION_PHASES.includes(selection.phase)) return null;
    return selection;
}

/**
 * 是否有切换事务在途。
 * @param {Object|null} entry
 * @returns {boolean}
 */
export function isSwitching(entry) {
    return getSelection(entry)?.phase === "switching";
}

/**
 * selection 是否处于可见失败态。
 * @param {Object|null} entry
 * @returns {boolean}
 */
export function hasSelectionFailure(entry) {
    return ["rolled_back", "rollback_failed"].includes(getSelection(entry)?.phase);
}

// ── 内部辅助 ─────────────────────────────────────────────────────────────────

function createEmptyEntry() {
    return {};
}

function setEntry(state, engineId, newEntry) {
    const newMap = new Map(state);
    newMap.set(engineId, newEntry);
    return newMap;
}
