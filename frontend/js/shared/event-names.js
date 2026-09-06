/**
 * 事件名常量清单。
 *
 * 所有 blink:// 事件名的 single source of truth（前端侧）。
 * 后端对应文件：src/app/event_names.rs（手动同步，后续可考虑 codegen）。
 *
 * 使用方式：
 *   import { EVENTS } from './event-names.js';
 *   listen(EVENTS.RESULTS, (event) => { ... });
 *   TAU.event.emit(EVENTS.CONFIG_CHANGED, { key: 'app.hotkey' });
 */
export const EVENTS = Object.freeze({
    // ── 窗口生命周期 ──
    SHOWN: 'blink://shown',
    HIDDEN: 'blink://hidden',

    // ── 输入状态 ──
    /** 后端输入 UI 状态变化。payload: { revision, altDown, windowVisible, exclusiveChordActive }。 */
    INPUT_STATE_CHANGED: 'blink://input-state-changed',
    /** 快捷键 recorder 已完成后端 armed。payload: { requestId }。 */
    HOTKEY_RECORDING_READY: 'blink://hotkey-recording-ready',

    // ── 搜索 ──
    RESULTS: 'blink://results',

    // ── Chord ──
    CHORD_FILL_QUERY: 'blink://chord-fill-query',
    /** Chord 触发后要求前端进入独占模式。payload: { mode: "clipboard" }。 */
    CHORD_ENTER_MODE: 'blink://chord-enter-mode',
    /** 0.22.12：chord 全局快捷键注册状态。payload: [{ actionId, followChord, modifiers, key, registered, reason? }]。 */
    GLOBAL_HOTKEY_STATUS: 'blink://global-hotkey-status',

    // ── Chat ──
    CHAT_STREAM: 'blink://chat-stream',
    CHAT_CONFIRM_ACTION: 'blink://chat-confirm-action',
    CHAT_SKILL_ACTIVATED: 'blink://chat-skill-activated',
    CHAT_CONTEXT_STATUS: 'blink://chat-context-status',
    CHAT_TITLE_UPDATED: 'blink://chat-title-updated',
    CHAT_PREFILL: 'blink://chat-prefill',
    CHAT_LOAD_CONVERSATION: 'blink://chat-load-conversation',

    // ── 语音 ──
    VOICE_RECORDING_START: 'blink://voice-recording-start',
    VOICE_RECORDING_END: 'blink://voice-recording-end',
    VOICE_LEVEL: 'blink://voice-level',
    VOICE_PARTIAL: 'blink://voice-partial',
    VOICE_STATUS: 'blink://voice-status',
    VOICE_ERROR: 'blink://voice-error',

    // ── 配置 ──
    CONFIG_CHANGED: 'blink://config-changed',

    // ── 上下文感知 ──
    AWARENESS_UPDATED: 'blink://awareness-updated',
    CONTEXT_MENU_ACTION: 'blink://context-menu-action',

    // ── 音频测试 ──
    AUDIO_TEST_LEVEL: 'blink://audio-test-level',

    // ── 本地引擎（0.22.3）──
    /**
     * 通用引擎状态快照。payload: EngineStatusDto
     *
     * shape:
     * { engine_id, service_epoch, revision,
     *   status: { desired, operation: { kind, operation_id, stage, cancellable },
     *             environment, process: { state, pid?, reason? },
     *             service, model, backend, last_error? } }
     *
     * 前端去重：engine_id + service_epoch + revision。
     * operation.kind wire 值：idle/installing/updating/repairing/migrating/rolling_back/cleaning
     * operation.stage wire 值：pending/preparing/downloading/verifying/promoting/switching/validating/completed/cancelled/failed
     * i18n key：local_engine.operation.{kind} / local_engine.operation.stage.{stage}
     */
    LOCAL_ENGINE_STATUS: 'blink://local-engine-status',

    /**
     * 通用引擎日志条目。payload: EngineLogDto
     *
     * 运行时日志（instance_id 隔离）:
     * { engine_id, instance_id, operation_id: null, seq, timestamp, level, text }
     *
     * 安装日志（operation_id 隔离）:
     * { engine_id, instance_id: "", operation_id: "op-xxx", seq, timestamp, level, text }
     *
     * 前端去重：engine_id + (instance_id 或 operation_id) + seq。
     * seq 为字符串（避免 JS u64 精度丢失）。
     * 安装日志通过 operation_id 过滤；运行时日志通过 instance_id 过滤。
     */
    LOCAL_ENGINE_LOG: 'blink://local-engine-log',

    /**
     * 安装阶段变更事件（0.22.6 H4）。
     *
     * 当 InstallTransaction 内部阶段切换时实时广播：
     * preparing → downloading → verifying → promoting → switching → validating → completed
     *
     * payload:
     * { engine_id, operation_id, stage }
     *
     * stage 值与 LOCAL_ENGINE_STATUS 的 operation.stage 一致（snake_case）。
     * 前端可据此实时显示安装进度，不等 LOCAL_ENGINE_STATUS 的 revision 变化。
     */
    LOCAL_ENGINE_INSTALL_STAGE: 'blink://local-engine-install-stage',

    /**
     * 安装下载字节进度事件（0.22.14）。
     *
     * 引擎安装下载期间实时广播字节进度（后端节流 ≥200ms/条），供前端
     * 显示百分比与剩余时间估计。与 LOCAL_ENGINE_INSTALL_STAGE 分开：
     * 进度高频、阶段低频，语义不同不混用事件；前端用 stage 事件维护
     * 阶段文案，用本事件只更新进度数值。
     *
     * payload:
     * { engine_id, operation_id, downloaded, total }
     *
     * downloaded/total 均为当前下载文件（单文件）内计数——多文件安装
     * （如 PP-OCR 的 ORT zip + 3 个模型）逐文件重置。total 为 null 表示
     * 总大小未知，前端退化为已下载字节数展示。
     */
    LOCAL_ENGINE_INSTALL_PROGRESS: 'blink://local-engine-install-progress',

    // ── 便签（0.16.7）──
    STICKY_CREATED: 'blink://sticky-created',
    STICKY_DELETED: 'blink://sticky-deleted',
    STICKY_VISIBILITY_CHANGED: 'blink://sticky-visibility-changed',
    STICKY_APPEARANCE_CHANGED: 'blink://sticky-appearance-changed',
    STICKY_CONTENT_CHANGED: 'blink://sticky-content-changed',
    STICKY_TRASHED: 'blink://sticky-trashed',
    STICKY_RESTORED: 'blink://sticky-restored',

    // ── 截图（0.18.x）──
    /** 截图控件吸附 hints 流式推送 */
    SCREENSHOT_CONTROL_HINTS: 'blink://screenshot-control-hints',
});
