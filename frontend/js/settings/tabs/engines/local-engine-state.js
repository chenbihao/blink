/**
 * 纯状态 reducer —— 本地引擎运行时前端状态层（0.22.5 H3，0.22.6 收敛）。
 *
 * 按 engine_id 保存：
 * - catalog item（描述符投影）
 * - status snapshot（观测快照，含后端推导的 available）
 * - storage snapshot（存储概览）
 * - logs（结构化日志，bounded）
 * - pending UI action（用户触发的操作 kind + operation_id）
 * - models / preferences / pending model actions（0.22.6）
 *
 * ## 合并规则（铁则）
 *
 * ### epoch / revision
 * - 比较 engine_id。
 * - service_epoch **不同**：接受新 epoch，并清空旧 epoch 的 revision 门和旧日志。
 *   **不能比较 service_epoch 大小，只比较是否相同。**
 * - 同 epoch：只接受 revision 更大的状态（revision 是字符串化 u64，
 *   数值比较，禁止字典序）。
 *
 * ### 日志去重
 * - 日志按 `source + seq` 去重（source = operation_id 或 instance_id）。
 * - 当前 instance 变化后，旧 instance 的迟到日志**不得**进入当前实时日志区。
 *
 * ### operation
 * - operation action/result 必须绑定 operation_id。
 * - 迟到 completion（旧 operation_id）不覆盖新 operation；
 *   install_stage 事件同理（applyInstallStage 校验 operation_id）。
 *
 * @module local-engine-state
 */

// ── 常量 ─────────────────────────────────────────────────────────────────────

/** 实时日志最大行数（bounded，防 DOM 洪泛）。 */
export const MAX_LOG_LINES = 500;

// ── 类型 ──────────────────────────────────────────────────────────────────────

/**
 * 单引擎状态条目。
 * @typedef {Object} EngineStateEntry
 * @property {Object|null} catalog - catalog item DTO
 * @property {Object|null} status - EngineStatusDto
 * @property {Object|null} storage - EngineStorageDto
 * @property {LogEntry[]} logs - bounded 实时日志数组
 * @property {string|null} currentInstanceId - 当前 instance id（用于隔离日志）
 * @property {PendingAction|null} pendingAction - 用户触发的待完成操作
 * @property {boolean} logAutoExpand - 一次性标志：要求 renderer 自动展开日志区
 */

/**
 * 待完成 UI 操作。
 * @typedef {Object} PendingAction
 * @property {string} kind - 操作种类（install/start/stop/repair/cleanup）
 * @property {string} operationId - 操作实例 id
 * @property {number} timestamp - 触发时间戳
 */

/**
 * 日志条目。
 * @typedef {Object} LogEntry
 * @property {string} source - 日志来源标识（operation_id 或 instance_id）
 * @property {string} sourceKind - "operation" | "instance"
 * @property {string} seq
 * @property {string} timestamp
 * @property {string} level
 * @property {string} text
 */

// ── 初始状态 ──────────────────────────────────────────────────────────────────

/**
 * 创建单个引擎的初始状态条目。
 * @returns {EngineStateEntry}
 */
export function createInitialEntry() {
    return {
        catalog: null,
        status: null,
        storage: null,
        logs: [],
        currentInstanceId: null,
        pendingAction: null,
        // 0.22.6: 模型列表（ModelCatalogItemDto 数组）
        models: null,
        // 0.22.6: preferences DTO（EnginePreferencesDto）
        preferences: null,
        // 0.22.6: 模型级 pending action（按 model_id 索引）
        // Map<model_id, {kind, operationId, timestamp}>
        pendingModelActions: null,
        // 0.22.6: 一次性标志——renderer 展开后由 DOM 侧记录 opId 防重复
        logAutoExpand: false,
    };
}

/**
 * 创建空的 reducer 状态（Map<engine_id, EngineStateEntry>）。
 * @returns {Map<string, EngineStateEntry>}
 */
export function createInitialState() {
    return new Map();
}

// ── 辅助 ──────────────────────────────────────────────────────────────────────

/**
 * 替换 Map 中某个引擎的条目，返回新 Map（不可变更新）。
 * @param {Map<string, EngineStateEntry>} state
 * @param {string} engineId
 * @param {EngineStateEntry} newEntry
 * @returns {Map<string, EngineStateEntry>}
 */
function setEntry(state, engineId, newEntry) {
    const newMap = new Map(state);
    newMap.set(engineId, newEntry);
    return newMap;
}

/**
 * 比较两个 revision 字符串（wire 上是字符串防 JS 精度丢失，值本身是 u64）。
 *
 * 铁则：不能用普通字符串字典序——字典序下 "9" > "10"。
 * 全部走 BigInt 数值比较；解析失败（非法输入）fail closed 视为不大于。
 * @param {string} a
 * @param {string} b
 * @returns {boolean} a > b
 */
function revisionGreaterThan(a, b) {
    try {
        return BigInt(a) > BigInt(b);
    } catch {
        return false;
    }
}

// ── Reducer actions ───────────────────────────────────────────────────────────

/**
 * 设置 catalog（全量替换）。
 * @param {Map<string, EngineStateEntry>} state
 * @param {Object[]} catalog - EngineCatalogItem 数组
 * @returns {Map<string, EngineStateEntry>}
 */
export function setCatalog(state, catalog) {
    const newMap = new Map(state);
    for (const item of catalog) {
        const existing = newMap.get(item.engine_id) || createInitialEntry();
        newMap.set(item.engine_id, {...existing, catalog: item});
    }
    return newMap;
}

/**
 * 合并状态快照。
 *
 * 规则：
 * - engine_id 不同 → 忽略。
 * - service_epoch 不同 → 接受新 epoch，清空旧 revision 门和旧 instance 日志。
 *   **不能比较 service_epoch 大小，只比较是否相同。**
 * - 同 epoch → 只接受 revision 更大的状态。
 *
 * @param {Map<string, EngineStateEntry>} state
 * @param {Object} statusDto - EngineStatusDto
 * @returns {Map<string, EngineStateEntry>}
 */
export function mergeStatus(state, statusDto) {
    const engineId = statusDto.engine_id;
    if (!engineId) return state;

    const entry = state.get(engineId) || createInitialEntry();

    // 首次状态
    if (!entry.status) {
        return setEntry(state, engineId, {...entry, status: statusDto});
    }

    const oldStatus = entry.status;

    // epoch 不同 → 接受新 epoch，清空旧 revision 门和旧 epoch 日志。
    // 不能比较 service_epoch 大小，只比较是否相同。
    if (oldStatus.service_epoch !== statusDto.service_epoch) {
        const newEntry = {
            ...entry,
            status: statusDto,
            // 已绑定运行实例时清空旧日志，不能混入当前流
            logs: entry.currentInstanceId != null ? [] : entry.logs,
        };
        return setEntry(state, engineId, newEntry);
    }

    // 同 epoch → 只接受 revision 更大的状态
    if (!revisionGreaterThan(statusDto.revision, oldStatus.revision)) {
        // 旧或相同 revision → 丢弃（防慢查询覆盖较新 event）
        return state;
    }

    // 同 epoch + 更大 revision → 接受
    // 检测后端推送的新 operation → 设置自动展开标志（DOM 侧按 opId 幂等）
    let logAutoExpand = entry.logAutoExpand;
    const newOp = statusDto.status?.operation;
    if (newOp && newOp.operation_id
        && newOp.kind !== "idle"
        && !["completed", "cancelled", "failed"].includes(newOp.stage)) {
        logAutoExpand = true;
    }

    const newEntry = {
        ...entry,
        status: statusDto,
        logAutoExpand,
    };

    return setEntry(state, engineId, newEntry);
}

/**
 * 应用安装阶段事件（blink://local-engine-install-stage）。
 *
 * 迟到事件拒绝：payload 的 operation_id 必须与当前状态里的 operation_id
 * 一致才应用；不匹配（旧操作残留/异引擎串扰）直接丢弃，不改写 stage。
 *
 * @param {Map<string, EngineStateEntry>} state
 * @param {Object} payload - { engine_id, operation_id, stage }
 * @returns {Map<string, EngineStateEntry>}
 */
export function applyInstallStage(state, payload) {
    const engineId = payload?.engine_id;
    const stage = payload?.stage;
    if (!engineId || !stage) return state;

    const entry = state.get(engineId);
    if (!entry || !entry.status) return state;

    const currentOp = entry.status.status?.operation;
    if (!currentOp || currentOp.kind === "idle") return state;
    if (currentOp.operation_id && payload.operation_id !== currentOp.operation_id) {
        return state;
    }

    const newStatus = {
        ...entry.status,
        status: {
            ...entry.status.status,
            operation: {...currentOp, stage},
        },
    };
    return setEntry(state, engineId, {...entry, status: newStatus});
}

/**
 * 追加日志条目。
 *
 * 规则：
 * - 按 `engine_id + instance_id + seq` 去重。
 * - 旧 instance 的迟到日志**不得**进入当前实时日志区。
 * - bounded 最大行数 MAX_LOG_LINES。
 * - disposed 后不再处理事件（由 controller 在外部控制，reducer 自身是纯函数）。
 *
 * @param {Map<string, EngineStateEntry>} state
 * @param {Object} logDto - EngineLogDto
 * @returns {Map<string, EngineStateEntry>}
 */
export function appendLog(state, logDto) {
    const engineId = logDto.engine_id;
    if (!engineId) return state;

    const entry = state.get(engineId) || createInitialEntry();

    // 0.22.6: operation 日志（operation_id 非 null）不按 instance_id 过滤——
    // 安装/修复/删除操作可能在没有运行实例时产生日志。
    // 只对 instance 日志（operation_id 为 null/undefined）做 instance 过滤。
    const isOperationLog = logDto.operation_id != null;
    if (!isOperationLog && entry.currentInstanceId && logDto.instance_id !== entry.currentInstanceId) {
        return state;
    }

    // 去重：engine_id + instance_id + seq + source
    // 0.22.6: operation 日志用 operation_id 做来源标识，instance 日志用 instance_id
    const source = isOperationLog ? logDto.operation_id : logDto.instance_id;
    const dedupKey = `${source}:${logDto.seq}`;
    const existingKeys = new Set(entry.logs.map((l) => `${l.source}:${l.seq}`));
    if (existingKeys.has(dedupKey)) {
        return state;
    }

    const logEntry = {
        source,
        // 0.22.6: 标记日志来源——'operation' 或 'instance'
        sourceKind: isOperationLog ? "operation" : "instance",
        seq: logDto.seq,
        timestamp: logDto.timestamp,
        level: logDto.level,
        text: logDto.text,
    };

    const newLogs = [...entry.logs, logEntry];
    // bounded：超过最大行数时截断（保留最新的）
    const boundedLogs = newLogs.length > MAX_LOG_LINES
        ? newLogs.slice(newLogs.length - MAX_LOG_LINES)
        : newLogs;

    const newEntry = {
        ...entry,
        logs: boundedLogs,
        // 更新 currentInstanceId（首次或 instance 切换）——仅 instance 日志驱动
        currentInstanceId: isOperationLog
            ? entry.currentInstanceId
            : (entry.currentInstanceId || logDto.instance_id),
    };

    return setEntry(state, engineId, newEntry);
}

/**
 * 批量设置日志历史（替换，不追加）。
 * 用于初始化 pull 时加载历史日志。
 * @param {Map<string, EngineStateEntry>} state
 * @param {string} engineId
 * @param {Object[]} logDtos - EngineLogDto 数组
 * @returns {Map<string, EngineStateEntry>}
 */
export function setLogHistory(state, engineId, logDtos) {
    if (!engineId) return state;
    const entry = state.get(engineId) || createInitialEntry();

    // 从历史日志中提取 instance_id（取最新一条的 instance）
    let latestInstance = entry.currentInstanceId;
    for (let i = logDtos.length - 1; i >= 0; i--) {
        if (logDtos[i].instance_id) {
            latestInstance = logDtos[i].instance_id;
            break;
        }
    }

    const logs = logDtos.map((dto) => {
        const isOp = dto.operation_id != null;
        return {
            source: isOp ? dto.operation_id : dto.instance_id,
            sourceKind: isOp ? "operation" : "instance",
            seq: dto.seq,
            timestamp: dto.timestamp,
            level: dto.level,
            text: dto.text,
        };
    });

    // 安装/修复等 operation 日志只存在于实时事件流（后端历史只存引擎
    // 进程日志）——pull 替换时必须保留，否则窗口 focus 触发的 refreshStatus
    // 会把正在下载的安装日志整个清空。
    const existingOpLogs = entry.logs.filter((l) => l.sourceKind === "operation");
    const mergedLogs = existingOpLogs.length > 0
        ? [...logs, ...existingOpLogs]
        : logs;

    // bounded
    const boundedLogs = mergedLogs.length > MAX_LOG_LINES
        ? mergedLogs.slice(mergedLogs.length - MAX_LOG_LINES)
        : mergedLogs;

    const newEntry = {
        ...entry,
        logs: boundedLogs,
        currentInstanceId: latestInstance || entry.currentInstanceId,
    };

    return setEntry(state, engineId, newEntry);
}

/**
 * 设置存储概览。
 * @param {Map<string, EngineStateEntry>} state
 * @param {Object} storageDto - EngineStorageDto
 * @returns {Map<string, EngineStateEntry>}
 */
export function setStorage(state, storageDto) {
    const engineId = storageDto.engine_id;
    if (!engineId) return state;
    const entry = state.get(engineId) || createInitialEntry();
    return setEntry(state, engineId, {...entry, storage: storageDto});
}

/**
 * 设置 pending UI action。
 *
 * operation action/result 必须绑定 operation_id。
 * 迟到 completion（旧 operation_id）不覆盖新 operation。
 *
 * 当新的 operation 开始时（action 非 null 且 operationId 与之前不同），
 * 设置 `logAutoExpand` 标志——renderer 消费时自动展开日志区。
 * 只对预期产生有用日志的操作展开（install/repair/start/stop/cleanup）。
 *
 * @param {Map<string, EngineStateEntry>} state
 * @param {string} engineId
 * @param {{kind: string, operationId: string}|null} action
 * @returns {Map<string, EngineStateEntry>}
 */
export function setPendingAction(state, engineId, action) {
    if (!engineId) return state;
    const entry = state.get(engineId) || createInitialEntry();

    const pendingAction = action ? {
        kind: action.kind,
        operationId: action.operationId,
        timestamp: Date.now(),
    } : null;

    // 新 operation 出现 → 设置自动展开标志
    // install/repair/start/stop/cleanup 预期产生有用日志
    // cancel 和 null（清除）不展开
    let logAutoExpand = entry.logAutoExpand;
    if (action && action.operationId
        && ["install", "repair", "start", "stop", "cleanup"].includes(action.kind)) {
        logAutoExpand = true;
    } else if (action === null) {
        // 清除操作 → 重置标志（防止操作完成后仍保持展开状态）
        logAutoExpand = false;
    }

    return setEntry(state, engineId, {...entry, pendingAction, logAutoExpand});
}

/**
 * 清空 UI 日志缓冲（不影响后端日志）。
 * @param {Map<string, EngineStateEntry>} state
 * @param {string} engineId
 * @returns {Map<string, EngineStateEntry>}
 */
export function clearLogs(state, engineId) {
    if (!engineId) return state;
    const entry = state.get(engineId);
    if (!entry) return state;
    return setEntry(state, engineId, {...entry, logs: []});
}

// ── 0.22.6: 模型列表与 preferences reducer ───────────────────────────────────

/**
 * 设置引擎模型列表（全量替换）。
 * @param {Map<string, EngineStateEntry>} state
 * @param {string} engineId
 * @param {Object[]} models - ModelCatalogItemDto 数组
 * @returns {Map<string, EngineStateEntry>}
 */
export function setModels(state, engineId, models) {
    if (!engineId) return state;
    const entry = state.get(engineId) || createInitialEntry();
    return setEntry(state, engineId, {...entry, models: models || []});
}

/**
 * 设置引擎 preferences DTO。
 * @param {Map<string, EngineStateEntry>} state
 * @param {string} engineId
 * @param {Object|null} preferences - EnginePreferencesDto
 * @returns {Map<string, EngineStateEntry>}
 */
export function setPreferences(state, engineId, preferences) {
    if (!engineId) return state;
    const entry = state.get(engineId) || createInitialEntry();
    return setEntry(state, engineId, {...entry, preferences});
}

/**
 * 设置模型级 pending action（按 model_id 索引）。
 *
 * 模型安装/修复操作开始时设置 `logAutoExpand` 标志。
 *
 * @param {Map<string, EngineStateEntry>} state
 * @param {string} engineId
 * @param {string} modelId
 * @param {{kind: string, operationId: string}|null} action
 * @returns {Map<string, EngineStateEntry>}
 */
export function setPendingModelAction(state, engineId, modelId, action) {
    if (!engineId || !modelId) return state;
    const entry = state.get(engineId) || createInitialEntry();
    const actions = new Map(entry.pendingModelActions || []);

    let logAutoExpand = entry.logAutoExpand;

    if (action === null) {
        actions.delete(modelId);
    } else {
        actions.set(modelId, {
            kind: action.kind,
            operationId: action.operationId,
            timestamp: Date.now(),
        });
        // 模型安装/修复操作开始时自动展开日志
        if (action.operationId
            && ["install", "repair"].includes(action.kind)) {
            logAutoExpand = true;
        }
    }

    return setEntry(state, engineId, {...entry, pendingModelActions: actions, logAutoExpand});
}

/**
 * 获取模型级 pending action。
 * @param {EngineStateEntry} entry
 * @param {string} modelId
 * @returns {{kind: string, operationId: string, timestamp: number}|null}
 */
export function getPendingModelAction(entry, modelId) {
    if (!entry || !entry.pendingModelActions) return null;
    return entry.pendingModelActions.get(modelId) || null;
}

/**
 * 返回 UI 应展示的模型安装状态。前端已发起但后端列表尚未刷新的操作优先。
 */
export function getEffectiveModelInstallState(entry, model) {
    const pending = getPendingModelAction(entry, model?.model_id);
    const pendingState = {
        install: "downloading",
        repair: "repairing",
        delete: "deleting",
    }[pending?.kind];
    return pendingState || model?.install_state || "not_installed";
}

// ── 查询函数（纯读取） ─────────────────────────────────────────────────────────

/**
 * 获取单个引擎状态条目。
 * @param {Map<string, EngineStateEntry>} state
 * @param {string} engineId
 * @returns {EngineStateEntry|null}
 */
export function getEntry(state, engineId) {
    return state.get(engineId) || null;
}

/**
 * 获取所有引擎 id 列表。
 * @param {Map<string, EngineStateEntry>} state
 * @returns {string[]}
 */
export function getEngineIds(state) {
    return Array.from(state.keys());
}

/**
 * 判断引擎是否"就绪可用"。
 *
 * 可用性业务规则（desired/service/model 推导）由后端推导并投影为
 * `status.available`（0.22.6 协议）——前端只消费，不复制推导规则。
 *
 * @param {EngineStateEntry} entry
 * @returns {boolean}
 */
export function isEngineReady(entry) {
    if (!entry || !entry.status) return false;
    const s = entry.status.status;
    if (!s) return false;
    return s.available === true;
}

/**
 * 判断引擎是否有活跃操作（非 idle 且未结束）。
 * @param {EngineStateEntry} entry
 * @returns {boolean}
 */
export function hasActiveOperation(entry) {
    if (!entry || !entry.status) return false;
    const op = entry.status.status?.operation;
    if (!op) return false;
    if (op.kind === "idle") return false;
    // 已结束的阶段不算活跃
    return !["completed", "cancelled", "failed"].includes(op.stage);
}

/**
 * 判断 operation 是否可取消。
 * @param {EngineStateEntry} entry
 * @returns {boolean}
 */
export function isOperationCancellable(entry) {
    if (!hasActiveOperation(entry)) return false;
    return entry.status.status.operation.cancellable === true;
}

/**
 * 获取环境的可行动作。
 *
 * 按钮启用逻辑：
 * - Missing → install
 * - Broken/NeedsRebuild → repair
 * - Ready + stopped → start
 * - running/starting → stop；但 operation gate 冲突时禁用
 * - cancellable operation → cancel
 *
 * @param {EngineStateEntry} entry
 * @returns {string|null} action kind 或 null
 */
export function getPrimaryAction(entry) {
    if (!entry || !entry.status) return null;
    const s = entry.status.status;
    if (!s) return null;

    // 有可取消的活跃操作 → cancel 优先
    if (isOperationCancellable(entry)) {
        return "cancel";
    }

    // operation 活跃但不可取消 → 无可行动作（等待完成）
    if (hasActiveOperation(entry)) {
        return null;
    }

    // 按环境状态
    switch (s.environment) {
        case "missing":
            return "install";
        case "broken":
        case "needs_rebuild":
            return "repair";
        case "ready": {
            // ready + stopped → start
            // running/starting/stopping → stop
            //
            // ProcessStateDto wire shape（显式 DTO，不暴露 serde enum）：
            //   {state: "stopped"}
            //   {state: "starting"}
            //   {state: "running", pid: 1234}
            //   {state: "stopping"}
            //   {state: "exited", reason: "..."}
            const p = s.process;
            if (!p || typeof p !== "object") return "start";
            const ps = p.state;
            if (ps === "stopped" || ps === "exited") return "start";
            if (ps === "starting" || ps === "running" || ps === "stopping") return "stop";
            // 未知 state → fail closed，默认 start（安全）
            return "start";
        }
        default:
            return null;
    }
}

/**
 * 判断某个 action 是否被 operation gate 阻止。
 * 如果有活跃操作，则除 cancel 外的其他 action 被禁用。
 * @param {EngineStateEntry} entry
 * @param {string} actionKind
 * @returns {boolean}
 */
export function isActionBlocked(entry, actionKind) {
    if (actionKind === "cancel") return !isOperationCancellable(entry);
    if (actionKind === "log" || actionKind === "cleanup") return false; // 日志和清理可在任何时候查看
    return hasActiveOperation(entry);
}
