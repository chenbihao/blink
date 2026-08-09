//! 事件名常量清单。
//!
//! 所有 `blink://*` 事件名的 single source of truth。
//! 后端 emit / 前端 listen 都从此处取常量，消除字面量散落、拼错无编译期保护的问题。
//!
//! 放在 domain 层（而非 app 层）：domain 子模块（search / chord / ai 等）需要引用这些常量，
//! 而 domain 不能反向依赖 app，所以常量必须定义在 domain。
//!
//! 前端对应文件：`frontend/js/shared/event-names.js`（手动同步，后续可考虑 codegen）。

/// 所有 `blink://*` 事件名常量。
///
/// 使用方式：`app.emit(EventNames::RESULTS, payload)` 替代 `app.emit("blink://results", payload)`。
pub struct EventNames;

impl EventNames {
    // ── 窗口生命周期 ──
    pub const SHOWN: &str = "blink://shown";
    pub const HIDDEN: &str = "blink://hidden";

    // ── 输入状态 ──
    /// 后端输入 UI 状态变化。payload: `InputUiState { revision, altDown, windowVisible, exclusiveChordActive }`。
    /// 前端以 `revision` 去重/拒绝旧状态，投影 `alt-active` / `chord-visible`。
    pub const INPUT_STATE_CHANGED: &str = "blink://input-state-changed";

    // ── 搜索 ──
    pub const RESULTS: &str = "blink://results";

    // ── Chord ──
    pub const CHORD_FILL_QUERY: &str = "blink://chord-fill-query";

    // ── Chat ──
    pub const CHAT_STREAM: &str = "blink://chat-stream";
    pub const CHAT_CONFIRM_ACTION: &str = "blink://chat-confirm-action";
    pub const CHAT_SKILL_ACTIVATED: &str = "blink://chat-skill-activated";
    pub const CHAT_CONTEXT_STATUS: &str = "blink://chat-context-status";
    pub const CHAT_TITLE_UPDATED: &str = "blink://chat-title-updated";
    /// 0.16.2：chord Alt+Q 带文本触发时，把初始文本推给 chat 窗口前端填充输入框。
    pub const CHAT_PREFILL: &str = "blink://chat-prefill";
    /// 0.17.6a: promote 临时对话后，通知 chat 窗口切换到该 conversation。payload: conversation_id
    pub const CHAT_LOAD_CONVERSATION: &str = "blink://chat-load-conversation";

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

    // ── 便签（0.16.7-0.16.10）──
    /// 便签被创建。payload: `{ stickyId }`
    pub const STICKY_CREATED: &str = "blink://sticky-created";
    /// 便签被删除。payload: `{ stickyId }`
    pub const STICKY_DELETED: &str = "blink://sticky-deleted";
    /// 便签可见性变化。payload: `{ stickyId, visible }`
    pub const STICKY_VISIBILITY_CHANGED: &str = "blink://sticky-visibility-changed";
    /// 便签外观变化（颜色）。payload: `{ stickyId, color }`
    pub const STICKY_APPEARANCE_CHANGED: &str = "blink://sticky-appearance-changed";
    /// 便签内容变化。payload: `{ stickyId, source, updatedAt }`；source 为
    /// `content-editor | sticky | capability`。
    pub const STICKY_CONTENT_CHANGED: &str = "blink://sticky-content-changed";
    /// 便签被移入回收站（0.17.7）。payload: `{ stickyId }`
    pub const STICKY_TRASHED: &str = "blink://sticky-trashed";
    /// 便签从回收站恢复（0.17.7）。payload: `{ stickyId }`
    pub const STICKY_RESTORED: &str = "blink://sticky-restored";

    // ── 截图（0.18.x）──
    /// 截图控件吸附 hints 流式推送（0.18.x）。
    /// payload: `ControlHintsEvent { generation, kind: "batch"|"done", depth, hints, ... }`
    pub const SCREENSHOT_CONTROL_HINTS: &str = "blink://screenshot-control-hints";
}
