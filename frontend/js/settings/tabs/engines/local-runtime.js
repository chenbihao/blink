/**
 * 通用本地引擎运行时 controller（0.22.5 H3）。
 *
 * 提供 mount()/dispose() 生命周期管理，协调 catalog/status/logs 的初始 pull
 * 与事件订阅，避免竞态。
 *
 * ## 初始 pull 与事件订阅竞态
 *
 * 推荐流程：
 * 1. 先注册 LOCAL_ENGINE_STATUS / LOCAL_ENGINE_LOG listener。
 * 2. 缓冲初始化期间事件（buffering 不依赖 mounted 标志）。
 * 3. 再拉 catalog/status/logs。
 * 4. 合并缓冲事件。
 * 5. 最后渲染。
 *
 * 避免"先 pull 后 listen"丢失中间状态，也避免慢查询覆盖较新的 event。
 *
 * ## mount/dispose 竞态防护
 *
 * - **mount generation**：每次 mount 递增 generation，dispose 后旧 generation 的
 *   异步回调全部失效（防止迟到 mount 覆盖已 dispose 的 controller）。
 * - **buffer 不依赖 mounted**：buffering 期间事件先入缓冲，不因 mounted=false 丢弃。
 * - **registerListeners 失败回滚**：如果 listener 注册失败，清理已注册的 listener。
 * - **dispose 期间 mount generation 失效**：dispose 后 mount generation 递增，
 *   旧 generation 的 mount 完成时发现 generation 不匹配，不激活。
 * - **refreshStatus buffer+generation 防护**：refreshStatus 期间也启用 buffering，
 *   且检查 generation 防止迟到刷新覆盖新状态。
 *
 * @module local-runtime
 */

import {invoke, listen, normalizeError} from "../../../shared/tauri.js";
import {EVENTS} from "../../../shared/event-names.js";
import {
    createInitialState,
    setCatalog,
    mergeStatus,
    applyInstallStage,
    appendLog,
    setLogHistory,
    setStorage,
    setPendingAction,
    setPendingModelAction,
    getPendingModelAction,
    clearLogs,
    getEngineIds,
    setModels,
    setPreferences,
    bindRealOperationId,
} from "./local-engine-state.js";

// ── 命令清单（与后端 commands/local_engine.rs 逐一核对）─────────────────────────

const COMMANDS = Object.freeze({
    // H3: 引擎生命周期
    GET_CATALOG: "get_local_engine_catalog",
    GET_STATUS: "get_local_engine_status",
    GET_LOGS: "get_local_engine_logs",
    GET_STORAGE: "get_local_engine_storage",
    INSTALL: "install_local_engine",
    START: "start_local_engine",
    STOP: "stop_local_engine",
    REPAIR: "repair_local_engine",
    CLEANUP: "cleanup_local_engine",
    CANCEL: "cancel_local_engine_operation",
    STOP_ORPHAN: "stop_orphan_engine",
    // H4: 偏好与诊断
    GET_PREFERENCES: "get_local_engine_preferences",
    SET_PREFERENCES: "set_local_engine_preferences",
    GET_FOUNDATION: "get_runtime_foundation_status",
    GET_DIAGNOSTICS: "get_engine_diagnostics",
    OPEN_ENGINE_FOLDER: "open_engine_folder",
    OPEN_RUNTIME_FOLDER: "open_runtime_folder",
    // H5: 模型生命周期
    LIST_MODELS: "list_engine_models",
    INSTALL_MODEL: "install_engine_model",
    DELETE_MODEL: "delete_engine_model",
    REPAIR_MODEL: "repair_engine_model",
    CANCEL_MODEL_OP: "cancel_model_operation",
});

// ── controller ─────────────────────────────────────────────────────────────────

/**
 * 创建本地引擎运行时 controller。
 *
 * @param {{onStateChange?: (state: Map) => void, onError?: (err: Object) => void}} callbacks
 * @returns {LocalEngineController}
 */
export function createLocalEngineController(callbacks = {}) {
    let state = createInitialState();
    let mounted = false;
    let disposed = false;
    let unlisteners = [];
    let eventBuffer = []; // 初始化期间缓冲事件
    let buffering = false;
    let activeActions = new Map(); // engine_id → Set<operation_id>（single-flight）

    // mount generation：每次 mount 递增，dispose 后旧 generation 的异步回调失效
    let mountGeneration = 0;

    /**
     * 通知状态变化。
     */
    // 高频日志事件（下载时每行一条）会触发整卡重渲染——合并为最多
    // ~12 次/秒，避免按钮 hover 闪烁、点击落空与 DOM 抖动。
    let notifyTimer = null;

    function notifyStateChange() {
        if (disposed) return;
        if (notifyTimer != null) return;
        notifyTimer = setTimeout(() => {
            notifyTimer = null;
            if (disposed) return;
            if (callbacks.onStateChange) {
                callbacks.onStateChange(new Map(state));
            }
        }, 80);
    }

    /**
     * 合并一条 EngineStatusDto 到状态。
     *
     * 0.22.6.1：合并后执行乐观 pending → 真实 operation_id 绑定——
     * synthetic id 只表达"请求已发出"，收到后端状态后绑定真实 id，
     * 使 install_stage 事件的 operation_id 校验能够命中。
     *
     * @param {Object} statusDto - EngineStatusDto
     */
    function applyStatusDto(statusDto) {
        state = mergeStatus(state, statusDto);
        const engineId = statusDto?.engine_id;
        if (!engineId) return;
        const entry = state.get(engineId);
        if (entry?.status) {
            state = bindRealOperationId(state, engineId, entry.status);
        }
    }

    /**
     * 处理状态事件。
     *
     * payload 是 EngineStatusDto：`{ engine_id, service_epoch, revision, status }`。
     * status query 与 LOCAL_ENGINE_STATUS event 使用同一 DTO shape。
     *
     * @param {Object} payload - EngineStatusDto
     */
    function handleStatusEvent(payload) {
        if (disposed) return;

        // 缓冲期间先存起来（不依赖 mounted 标志）
        if (buffering) {
            eventBuffer.push({type: "status", payload});
            return;
        }

        if (!mounted) return;

        applyStatusDto(payload);
        notifyStateChange();
    }

    /**
     * 处理日志事件。
     *
     * payload 是 EngineLogDto：`{ engine_id, instance_id, seq, timestamp, level, text }`。
     * 历史与实时事件使用同一 shape。
     *
     * @param {Object} payload - EngineLogDto
     */
    function handleLogEvent(payload) {
        if (disposed) return;

        if (buffering) {
            eventBuffer.push({type: "log", payload});
            return;
        }

        if (!mounted) return;

        state = appendLog(state, payload);
        notifyStateChange();
    }

    /**
     * 处理安装阶段变更事件（0.22.6 H4）。
     *
     * payload: { engine_id, operation_id, stage }
     *
     * 前端据此实时显示安装进度，不等 LOCAL_ENGINE_STATUS 的 revision 变化。
     * stage 投影到 EngineStatus.operation.stage——operation_id 不匹配的迟到
     * 事件由 reducer 拒绝（applyInstallStage）。
     *
     * @param {Object} payload - { engine_id, operation_id, stage }
     */
    function handleInstallStageEvent(payload) {
        if (disposed) return;

        // 缓冲期间先存起来
        if (buffering) {
            eventBuffer.push({type: "install_stage", payload});
            return;
        }

        if (!mounted) return;

        state = applyInstallStage(state, payload);
        notifyStateChange();
    }

    /**
     * 注册事件监听器。
     * 如果部分注册成功后失败，回滚已注册的 listener。
     */
    async function registerListeners() {
        const registered = [];

        try {
            const unlistenStatus = await listen(EVENTS.LOCAL_ENGINE_STATUS, (event) => {
                handleStatusEvent(event.payload);
            });
            registered.push(unlistenStatus);

            const unlistenLog = await listen(EVENTS.LOCAL_ENGINE_LOG, (event) => {
                handleLogEvent(event.payload);
            });
            registered.push(unlistenLog);

            // 0.22.6 H4: 安装阶段变更事件
            const unlistenInstallStage = await listen(EVENTS.LOCAL_ENGINE_INSTALL_STAGE, (event) => {
                handleInstallStageEvent(event.payload);
            });
            registered.push(unlistenInstallStage);

            unlisteners = registered;
        } catch (e) {
            // 回滚已注册的 listener
            for (const unlisten of registered) {
                if (typeof unlisten === "function") {
                    try {
                        unlisten();
                    } catch (rollbackErr) {
                        console.warn("[local-engine] rollback unlisten failed:", rollbackErr);
                    }
                }
            }
            throw e;
        }
    }

    /**
     * 合并缓冲的事件到状态。
     */
    function flushBuffer() {
        if (eventBuffer.length === 0) return;

        for (const buffered of eventBuffer) {
            if (buffered.type === "status") {
                // payload 本身就是 EngineStatusDto
                applyStatusDto(buffered.payload);
            } else if (buffered.type === "log") {
                state = appendLog(state, buffered.payload);
            } else if (buffered.type === "install_stage") {
                // 0.22.6 H4: 安装阶段变更（迟到事件由 reducer 拒绝）
                state = applyInstallStage(state, buffered.payload);
            }
        }
        eventBuffer = [];
    }

    /**
     * 拉取 catalog。
     */
    async function pullCatalog() {
        const catalog = await invoke(COMMANDS.GET_CATALOG);
        state = setCatalog(state, catalog);
    }

    /**
     * 拉取 status（全部引擎）。
     */
    async function pullStatus() {
        const statuses = await invoke(COMMANDS.GET_STATUS, {engineId: null});
        for (const dto of statuses) {
            applyStatusDto(dto);
        }
    }

    /**
     * 拉取日志历史（所有引擎）。
     */
    async function pullLogs() {
        const engineIds = getEngineIds(state);
        for (const engineId of engineIds) {
            try {
                const logs = await invoke(COMMANDS.GET_LOGS, {engineId, maxLines: 500});
                state = setLogHistory(state, engineId, logs);
            } catch (e) {
                console.warn(`[local-engine] pull logs for ${engineId} failed:`, e);
            }
        }
    }

    /**
     * 拉取存储概览（单个引擎）。
     */
    async function pullStorage(engineId) {
        try {
            const storage = await invoke(COMMANDS.GET_STORAGE, {engineId});
            state = setStorage(state, storage);
            notifyStateChange();
        } catch (e) {
            console.warn(`[local-engine] pull storage for ${engineId} failed:`, e);
        }
    }

    /**
     * 拉取所有引擎存储。
     */
    async function pullAllStorage() {
        const engineIds = getEngineIds(state);
        for (const engineId of engineIds) {
            await pullStorage(engineId);
        }
    }

    /**
     * 拉取所有引擎的模型列表与 preferences（0.22.6）。
     * 在 catalog pull 之后调用——需要 engine_id 列表。
     */
    async function pullModelsAndPreferences() {
        const engineIds = getEngineIds(state);
        for (const engineId of engineIds) {
            // 模型列表（只读查询，无副作用）
            try {
                const models = await invoke(COMMANDS.LIST_MODELS, {engineId});
                state = setModels(state, engineId, models);
            } catch (e) {
                console.warn(`[local-engine] pull models for ${engineId} failed:`, e);
            }
            // preferences
            try {
                const prefs = await invoke(COMMANDS.GET_PREFERENCES, {engineId});
                state = setPreferences(state, engineId, prefs);
            } catch (e) {
                console.warn(`[local-engine] pull preferences for ${engineId} failed:`, e);
            }
        }
    }

    // ── 公开 API ──────────────────────────────────────────────────────────────

    return {
        /**
         * 挂载 controller：注册监听 → 缓冲 → pull → 合并 → 渲染。
         *
         * 竞态防护：
         * - mount generation 递增，防止异步 mount 完成时 controller 已 dispose。
         * - 重复 mount 不创建重复监听。
         * - registerListeners 失败时回滚已注册 listener。
         */
        async mount() {
            if (disposed) throw new Error("controller已 disposed，不可 mount");
            if (mounted) return; // 幂等

            const gen = ++mountGeneration;

            // 1. 先注册 listener（buffering=true，事件入缓冲）
            buffering = true;
            try {
                await registerListeners();
            } catch (e) {
                // registerListeners 失败 → 回滚已清理，恢复 buffering
                buffering = false;
                eventBuffer = [];
                throw e;
            }

            // dispose 可能在 await 期间发生 → 检查 generation
            if (gen !== mountGeneration || disposed) {
                // 旧 generation 的 mount → 清理 listener 后返回
                for (const unlisten of unlisteners) {
                    if (typeof unlisten === "function") {
                        try {
                            unlisten();
                        } catch (e) {
                            // ignore
                        }
                    }
                }
                unlisteners = [];
                buffering = false;
                eventBuffer = [];
                return;
            }

            // 2. pull catalog → status → logs → models + preferences
            try {
                await pullCatalog();
                await pullStatus();
                await pullLogs();
                await pullModelsAndPreferences();
            } catch (e) {
                if (gen !== mountGeneration || disposed) {
                    // 旧 generation → 不激活
                    return;
                }
                if (callbacks.onError) callbacks.onError(normalizeError(e));
            }

            // 再次检查 generation（pull 期间可能 dispose）
            if (gen !== mountGeneration || disposed) {
                return;
            }

            // 3. 合并缓冲事件
            buffering = false;
            flushBuffer();

            // 4. 通知渲染
            mounted = true;
            notifyStateChange();

            // 5. 后台拉取 storage + 刷新 models（不阻塞初始渲染）
            pullAllStorage().catch((e) => {
                console.warn("[local-engine] background storage pull failed:", e);
            });
            // 后台再刷一次 models/preferences（初始 pull 可能在 status 之前）
            pullModelsAndPreferences().catch((e) => {
                console.warn("[local-engine] background models pull failed:", e);
            });
        },

        /**
         * 解除所有监听并清空状态。
         * dispose 后不得继续处理事件。
         *
         * 竞态防护：mount generation 递增，使进行中的 mount 完成时检测到
         * generation 不匹配而放弃激活。
         */
        dispose() {
            disposed = true;
            mounted = false;
            buffering = false;
            mountGeneration++; // 使进行中的 mount 失效
            eventBuffer = [];
            activeActions.clear();
            if (notifyTimer != null) {
                clearTimeout(notifyTimer);
                notifyTimer = null;
            }

            for (const unlisten of unlisteners) {
                if (typeof unlisten === "function") {
                    try {
                        unlisten();
                    } catch (e) {
                        console.warn("[local-engine] unlisten failed:", e);
                    }
                }
            }
            unlisteners = [];
            state = createInitialState();
        },

        /**
         * 是否已挂载。
         */
        isMounted() {
            return mounted && !disposed;
        },

        /**
         * 是否已 disposed。
         */
        isDisposed() {
            return disposed;
        },

        /**
         * 获取当前状态快照（不可变副本）。
         */
        getState() {
            return new Map(state);
        },

        /**
         * 设置状态变化回调（可替换）。
         */
        setOnStateChange(cb) {
            callbacks.onStateChange = cb;
        },

        // ── 操作 actions ──────────────────────────────────────────────────────

        /**
         * 安装引擎。
         * 返回 {engine_id, operation_id, end_state}——end_state 为
         * "completed"|"cancelled"，cancelled 是正常终态不是错误。
         * @param {string} engineId
         * @param {string|null} computePreference
         */
        async install(engineId, computePreference = null) {
            return this._executeAction(engineId, "install", async () => {
                return invoke(COMMANDS.INSTALL, {
                    engineId,
                    computePreference,
                });
            });
        },

        /**
         * 启动引擎。
         * @param {string} engineId
         * @param {string|null} computePreference
         */
        async start(engineId, computePreference = null) {
            return this._executeAction(engineId, "start", async () => {
                return invoke(COMMANDS.START, {
                    engineId,
                    computePreference,
                });
            });
        },

        /**
         * 停止引擎。
         * @param {string} engineId
         */
        async stop(engineId) {
            return this._executeAction(engineId, "stop", async () => {
                return invoke(COMMANDS.STOP, {engineId});
            });
        },

        /**
         * 停止孤儿引擎进程（0.22.6）。
         *
         * 当 lease 恢复扫描发现遗留进程时，用户可手动调用此命令终止孤儿进程。
         * 后端通过 PID + 创建时间 + 路径 + token 验证身份后终止。
         *
         * 此方法不走 _executeAction（不设置 pending action），
         * 因为孤儿停止是一次性诊断操作，不经过正常 install/start/stop 生命周期。
         *
         * @param {string} engineId
         * @returns {Promise<{engine_id: string, stopped: boolean, reason: string, detail?: string}>}
         */
        async stopOrphan(engineId) {
            if (!mounted || disposed) return;
            try {
                const result = await invoke(COMMANDS.STOP_ORPHAN, {engineId});
                // 停止后刷新状态（lease 可能已清除）
                await this.refreshStatus();
                return result;
            } catch (e) {
                const err = normalizeError(e);
                if (callbacks.onError) callbacks.onError(err);
                throw err;
            }
        },

        /**
         * 修复引擎。
         * 返回 {engine_id, operation_id, end_state}，同 install。
         * @param {string} engineId
         */
        async repair(engineId) {
            return this._executeAction(engineId, "repair", async () => {
                return invoke(COMMANDS.REPAIR, {engineId});
            });
        },

        /**
         * 清理引擎资产。
         * @param {string} engineId
         * @param {string[]} targetIds
         */
        async cleanup(engineId, targetIds) {
            return this._executeAction(engineId, "cleanup", async () => {
                return invoke(COMMANDS.CLEANUP, {
                    request: {
                        engine_id: engineId,
                        target_ids: targetIds,
                        operation_id: null,
                    },
                });
            });
        },

        /**
         * 取消操作。
         * @param {string} engineId
         * @param {string} operationId
         */
        async cancel(engineId, operationId) {
            try {
                const result = await invoke(COMMANDS.CANCEL, {engineId, operationId});
                // 取消后清除 pending action
                state = setPendingAction(state, engineId, null);
                notifyStateChange();
                return result;
            } catch (e) {
                const err = normalizeError(e);
                if (callbacks.onError) callbacks.onError(err);
                throw err;
            }
        },

        // ── 存储操作 ──────────────────────────────────────────────────────────

        /**
         * 刷新存储概览（单个引擎）。
         * 页面/tab 切回或窗口 focus 时调用。
         */
        async refreshStorage(engineId) {
            if (!mounted || disposed) return;
            return pullStorage(engineId);
        },

        /**
         * 刷新所有引擎存储。
         */
        async refreshAllStorage() {
            if (!mounted || disposed) return;
            return pullAllStorage();
        },

        /**
         * 清空 UI 日志缓冲（不影响后端日志）。
         * @param {string} engineId
         */
        clearLogBuffer(engineId) {
            state = clearLogs(state, engineId);
            notifyStateChange();
        },

        /**
         * 手动触发状态刷新（窗口 focus / tab 重新进入）。
         *
         * 竞态防护：
         * - 启用 buffering 防止 pull 期间事件与 pull 结果竞争。
         * - 使用 mount generation 防止迟到刷新覆盖 dispose 后的状态。
         */
        async refreshStatus() {
            if (!mounted || disposed) return;

            const gen = mountGeneration;
            buffering = true;
            try {
                await pullStatus();
                await pullLogs();
                await pullModelsAndPreferences();
            } catch (e) {
                if (callbacks.onError) callbacks.onError(normalizeError(e));
            }

            // 检查 generation（refresh 期间可能 dispose 或 re-mount）
            if (gen !== mountGeneration || disposed) {
                // 旧 generation 的刷新 → 丢弃缓冲，不通知
                buffering = false;
                eventBuffer = [];
                return;
            }

            buffering = false;
            flushBuffer();
            notifyStateChange();
        },

        // ── 0.22.6: 模型操作 ────────────────────────────────────────────────

        /**
         * 安装（下载）引擎模型。
         * @param {string} engineId
         * @param {string} modelId
         * @returns {Promise<Object>} ModelOperationResultDto
         */
        async installModel(engineId, modelId) {
            if (disposed) throw new Error("controller 已 disposed");
            const operationId = `install-model-${modelId}-${Date.now()}`;
            // 发起前存入 state，供取消按钮使用
            state = setPendingModelAction(state, engineId, modelId, {
                kind: "install",
                operationId,
            });
            notifyStateChange();
            return invoke(COMMANDS.INSTALL_MODEL, {
                request: {
                    engine_id: engineId,
                    model_id: modelId,
                    operation_id: operationId,
                },
            }).then((result) => {
                // 清除 pending model action
                state = setPendingModelAction(state, engineId, modelId, null);
                notifyStateChange();
                // 刷新模型列表
                this._refreshModels(engineId);
                return result;
            }).catch((e) => {
                state = setPendingModelAction(state, engineId, modelId, null);
                notifyStateChange();
                const err = normalizeError(e);
                if (callbacks.onError) callbacks.onError(err);
                throw err;
            });
        },

        /**
         * 修复引擎模型。
         * @param {string} engineId
         * @param {string} modelId
         * @returns {Promise<Object>} ModelOperationResultDto
         */
        async repairModel(engineId, modelId) {
            if (disposed) throw new Error("controller 已 disposed");
            const operationId = `repair-model-${modelId}-${Date.now()}`;
            // 发起前存入 state，供取消按钮使用
            state = setPendingModelAction(state, engineId, modelId, {
                kind: "repair",
                operationId,
            });
            notifyStateChange();
            return invoke(COMMANDS.REPAIR_MODEL, {
                request: {
                    engine_id: engineId,
                    model_id: modelId,
                    operation_id: operationId,
                },
            }).then((result) => {
                state = setPendingModelAction(state, engineId, modelId, null);
                notifyStateChange();
                this._refreshModels(engineId);
                return result;
            }).catch((e) => {
                state = setPendingModelAction(state, engineId, modelId, null);
                notifyStateChange();
                const err = normalizeError(e);
                if (callbacks.onError) callbacks.onError(err);
                throw err;
            });
        },

        /**
         * 删除引擎模型（结构化冲突由调用方处理）。
         * @param {string} engineId
         * @param {string} modelId
         * @returns {Promise<Object>} ModelOperationResultDto
         */
        async deleteModel(engineId, modelId) {
            if (disposed) throw new Error("controller 已 disposed");
            const operationId = `delete-model-${modelId}-${Date.now()}`;
            state = setPendingModelAction(state, engineId, modelId, {
                kind: "delete",
                operationId,
            });
            notifyStateChange();
            return invoke(COMMANDS.DELETE_MODEL, {
                request: {
                    engine_id: engineId,
                    model_id: modelId,
                    operation_id: operationId,
                },
            }).then((result) => {
                state = setPendingModelAction(state, engineId, modelId, null);
                notifyStateChange();
                this._refreshModels(engineId);
                return result;
            }).catch((e) => {
                state = setPendingModelAction(state, engineId, modelId, null);
                notifyStateChange();
                // 不做静默 fallback——结构化冲突由调用方展示
                const err = normalizeError(e);
                throw err;
            });
        },

        /**
         * 取消模型操作。
         * @param {string} engineId
         * @param {string} modelId
         * @param {string} operationId
         * @returns {Promise<Object>} ModelOperationResultDto
         */
        async cancelModelOperation(engineId, modelId, operationId) {
            if (disposed) throw new Error("controller 已 disposed");
            // 如果未传 operationId，尝试从 pending model action 中获取
            if (!operationId) {
                const entry = state.get(engineId);
                const pending = getPendingModelAction(entry, modelId);
                if (pending) {
                    operationId = pending.operationId;
                }
            }
            if (!operationId) {
                // 无 operationId 无法取消
                console.warn("[local-engine] cancelModelOperation: no operationId available");
                return;
            }
            return invoke(COMMANDS.CANCEL_MODEL_OP, {
                engineId,
                modelId,
                operationId,
            }).then((result) => {
                state = setPendingModelAction(state, engineId, modelId, null);
                this._refreshModels(engineId);
                return result;
            }).catch((e) => {
                const err = normalizeError(e);
                if (callbacks.onError) callbacks.onError(err);
                throw err;
            });
        },

        // ── 0.22.6: 底座与诊断 ──────────────────────────────────────────────

        /**
         * 拉取运行时底座状态（只读）。
         * @returns {Promise<Object>}
         */
        async getFoundationStatus() {
            if (disposed) throw new Error("controller 已 disposed");
            return invoke(COMMANDS.GET_FOUNDATION);
        },

        /**
         * 拉取引擎诊断信息（只读）。
         * @param {string} engineId
         * @returns {Promise<Object>}
         */
        async getDiagnostics(engineId) {
            if (disposed) throw new Error("controller 已 disposed");
            return invoke(COMMANDS.GET_DIAGNOSTICS, {engineId});
        },

        /**
         * 打开引擎数据目录。
         * @param {string} engineId
         */
        async openEngineFolder(engineId) {
            if (disposed) throw new Error("controller 已 disposed");
            return invoke(COMMANDS.OPEN_ENGINE_FOLDER, {engineId});
        },

        /**
         * 打开运行时根目录。
         */
        async openRuntimeFolder() {
            if (disposed) throw new Error("controller 已 disposed");
            return invoke(COMMANDS.OPEN_RUNTIME_FOLDER);
        },

        // ── 0.22.6: preferences 保存 ────────────────────────────────────────

        /**
         * 保存引擎偏好（闭合命令，失败回滚由调用方处理）。
         * @param {string} engineId
         * @param {Object} patch - EnginePreferencesPatchDto
         * @returns {Promise<Object>} EnginePreferencesDto
         */
        async savePreferences(engineId, patch) {
            if (disposed) throw new Error("controller 已 disposed");
            const result = await invoke(COMMANDS.SET_PREFERENCES, {engineId, patch});
            // 更新状态中的 preferences
            state = setPreferences(state, engineId, result);
            notifyStateChange();
            return result;
        },

        // ── 内部辅助 ──────────────────────────────────────────────────────────

        /**
         * 刷新单个引擎的模型列表。
         * @param {string} engineId
         */
        async _refreshModels(engineId) {
            try {
                const models = await invoke(COMMANDS.LIST_MODELS, {engineId});
                state = setModels(state, engineId, models);
                notifyStateChange();
            } catch (e) {
                console.warn(`[local-engine] refresh models for ${engineId} failed:`, e);
            }
        },

        // ── 内部辅助 ──────────────────────────────────────────────────────────

        /**
         * 执行操作 action（single-flight 防护）。
         * 返回 fn 的结构化结果（如 install 的 {engine_id, operation_id, end_state}）。
         *
         * 0.22.6.1 反馈闭环：
         * - 点击后立即 setPendingAction（乐观 pending → UI 立即显示忙碌）；
         * - 成功/失败都执行一次受 epoch/revision 防护的 refreshStatus——
         *   偶发丢失 completed status event 时也能退出 busy；失败时最终状态
         *   （如 rollback error）同样可见；
         * - 不让前端自己推导安装成功——以后端终态（status operation 终态 +
         *   command 返回的 end_state）为准。
         * @param {string} engineId
         * @param {string} actionKind
         * @param {() => Promise<*>} fn
         */
        async _executeAction(engineId, actionKind, fn) {
            if (disposed) throw new Error("controller 已 disposed");
            if (!mounted) throw new Error("controller 未 mount");

            // single-flight：同一引擎同一 action 不重复触发
            const key = `${engineId}:${actionKind}`;
            if (activeActions.has(key)) {
                return; // 已在进行中
            }
            activeActions.set(key, true);

            // 设置 pending action（乐观 pending：立即显示 pending/starting）
            const operationId = `${actionKind}-${Date.now()}`;
            state = setPendingAction(state, engineId, {
                kind: actionKind,
                operationId,
            });
            notifyStateChange();

            try {
                const result = await fn();
                // 成功（含 end_state=cancelled）后清除 pending action——
                // completed/cancelled 都必须退出忙碌状态
                state = setPendingAction(state, engineId, null);
                notifyStateChange();
                // 完成后受 epoch/revision 防护的状态刷新（兜底丢失的终态事件）
                this.refreshStatus().catch(() => {});
                return result;
            } catch (e) {
                const err = normalizeError(e);
                // 失败也清除 pending action（错误通过状态事件反馈）
                state = setPendingAction(state, engineId, null);
                notifyStateChange();
                // 失败后刷新最终状态，使 rollback error / last_error 可见
                this.refreshStatus().catch(() => {});
                if (callbacks.onError) callbacks.onError(err);
                throw err;
            } finally {
                activeActions.delete(key);
            }
        },
    };
}
