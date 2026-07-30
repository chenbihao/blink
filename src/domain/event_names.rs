//! 事件名常量清单（0.14.6 §3.3）。
//!
//! 所有 `blink://*` 事件名的 single source of truth。
//! 后端 emit / 前端 listen 都从此处取常量，消除字面量散落、拼错无编译期保护的问题。
//!
//! 放在 domain 层（而非 app 层）：domain 子模块（search / chord / ai 等）需要引用这些常量，
//! 而 domain 不能反向依赖 app，所以常量必须定义在 domain。
//!
//! 前端对应文件：`frontend/js/event-names.js`（手动同步，后续可考虑 codegen）。

/// 所有 `blink://*` 事件名常量。
///
/// 使用方式：`app.emit(EventNames::RESULTS, payload)` 替代 `app.emit("blink://results", payload)`。
pub struct EventNames;

impl EventNames {
    // ── 窗口生命周期 ──
    pub const SHOWN: &str = "blink://shown";
    pub const HIDDEN: &str = "blink://hidden";

    // ── 搜索 / AI ──
    pub const RESULTS: &str = "blink://results";
    pub const AI_STREAM: &str = "blink://ai-stream";
    pub const AI_CONFIRM_ACTION: &str = "blink://ai-confirm-action";

    // ── Chord ──
    pub const CHORD_FILL_QUERY: &str = "blink://chord-fill-query";

    // ── Chat ──
    pub const CHAT_STREAM: &str = "blink://chat-stream";
    pub const CHAT_CONFIRM_ACTION: &str = "blink://chat-confirm-action";
    pub const CHAT_SKILL_ACTIVATED: &str = "blink://chat-skill-activated";
    pub const CHAT_CONTEXT_STATUS: &str = "blink://chat-context-status";
    pub const CHAT_TITLE_UPDATED: &str = "blink://chat-title-updated";

    // ── 语音 ──
    pub const VOICE_RECORDING_START: &str = "blink://voice-recording-start";
    pub const VOICE_RECORDING_END: &str = "blink://voice-recording-end";
    pub const VOICE_LEVEL: &str = "blink://voice-level";
    pub const VOICE_PARTIAL: &str = "blink://voice-partial";
    pub const VOICE_STATUS: &str = "blink://voice-status";
    pub const VOICE_ERROR: &str = "blink://voice-error";

    // ── 配置 ──
    pub const CONFIG_CHANGED: &str = "blink://config-changed";

    // ── 上下文感知 ──
    pub const AWARENESS_UPDATED: &str = "blink://awareness-updated";
    pub const CONTEXT_MENU_ACTION: &str = "blink://context-menu-action";

    // ── Python 环境 / FunASR / 音频测试 ──
    pub const PYTHON_ENV_PROGRESS: &str = "blink://python-env-progress";
    pub const FUNASR_SERVER_LOG: &str = "blink://funasr-server-log";
    pub const FUNASR_SERVER_STATUS: &str = "blink://funasr-server-status";
    pub const AUDIO_TEST_LEVEL: &str = "blink://audio-test-level";
}
