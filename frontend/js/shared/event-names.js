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

    // ── Python 环境 / FunASR / 音频测试 ──
    PYTHON_ENV_PROGRESS: 'blink://python-env-progress',
    FUNASR_SERVER_LOG: 'blink://funasr-server-log',
    FUNASR_SERVER_STATUS: 'blink://funasr-server-status',
    AUDIO_TEST_LEVEL: 'blink://audio-test-level',

    // ── 本地引擎（0.22.3）──
    /** 通用引擎状态快照。payload: { engine_id, service_epoch, revision, snapshot } */
    LOCAL_ENGINE_STATUS: 'blink://local-engine-status',
    /** 通用引擎日志条目。payload: { engine_id, instance_id, seq, timestamp, level, text } */
    LOCAL_ENGINE_LOG: 'blink://local-engine-log',

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
