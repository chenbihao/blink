/**
 * 事件名常量清单（0.14.6 §3.3）。
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

    // ── 搜索 / AI ──
    RESULTS: 'blink://results',
    AI_STREAM: 'blink://ai-stream',
    AI_CONFIRM_ACTION: 'blink://ai-confirm-action',

    // ── Chord ──
    CHORD_FILL_QUERY: 'blink://chord-fill-query',

    // ── Chat ──
    CHAT_STREAM: 'blink://chat-stream',
    CHAT_CONFIRM_ACTION: 'blink://chat-confirm-action',
    CHAT_SKILL_ACTIVATED: 'blink://chat-skill-activated',
    CHAT_CONTEXT_STATUS: 'blink://chat-context-status',
    CHAT_TITLE_UPDATED: 'blink://chat-title-updated',

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
});
