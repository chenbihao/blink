//! Chord 模式底层能力（0.8.5 §六）。
//!
//! **交互模型**：主窗 visible + Alt hold（状态驱动，非定时器）。前端 `keyboard.js`
//! 检测 `body.chord-visible` 显示 Ghost overlay 层的提示（§6.5.6）+ 拦截 Alt+字母 →
//! `invoke("trigger_chord")`。后端只提供注册表 + 触发分派，**不碰 LL hook**（hook 的
//! tap/hold 状态机天然支持，见 phases §6.2 自洽性证明）。
//!
//! **四域约束**（0.8.4）：Chord 动作是 Execution 域消费者。真实动作（截图/划词/剪贴板）
//! 自行采集所需 Awareness（如划词调 selection 模块），参数注入必须显式
//! （`ExecArg::UserExplicit`），不能无脑抽 snapshot 重蹈 0.8.4 修掉的 bug。
//!
//! **i18n**（0.8.5.1 §6.6）：`label` 走 `LocalizableText`——registry 侧声明 zh/en
//! 双语，`list()` 按当前 UI 语言解析成字符串给前端；ChordAction 实现方（本 mod 的
//! StubAction / ClipboardHistoryAction）用 `LocalizableText::Localized` 双语字面量。

use std::sync::Arc;

use tauri::Emitter;

use crate::domain::plugin::LocalizableText;

/// Chord 触发后的窗口形态（决定前端如何切主窗形态）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChordSurface {
    /// 全屏截图覆盖（Alt+A）
    Screenshot,
    /// 主窗缩成悬浮小球（Alt+Q）
    MiniBall,
    /// 主窗切面板形态（预留：未来 AI 面板等真需要独占屏幕的动作）
    /// **0.8.5 §6.4 之后无当前消费者**——Alt+C 剪贴板已改走 `Default` + fill-query。
    /// 保留变体以维持 surface 扩展位；trigger_chord command 层暂无分派。
    #[allow(dead_code)]
    Panel,
    /// 不变形，直接执行后按 execute 内部决定 UI 反馈（如 emit fill-query）
    Default,
}

impl ChordSurface {
    /// 序列化给前端的字符串标签。
    pub fn as_str(&self) -> &'static str {
        match self {
            ChordSurface::Screenshot => "screenshot",
            ChordSurface::MiniBall => "mini_ball",
            ChordSurface::Panel => "panel",
            ChordSurface::Default => "default",
        }
    }
}

/// Chord 动作契约。实现方注册到 [`ChordRegistry`]，前端按 `key` 触发。
#[async_trait::async_trait]
pub trait ChordAction: Send + Sync {
    /// 唯一 id（disable 列表存储项，如 `"screenshot"`）。
    fn id(&self) -> &'static str;
    /// 触发字母（小写，如 `'a'`）。前端 Alt+此字母 → trigger_chord。
    fn key(&self) -> char;
    /// 显示名（走 `LocalizableText`——registry 声明 zh/en，list() 按 language 解析）。
    fn label(&self) -> &LocalizableText;
    /// 触发后的窗口形态。
    fn surface(&self) -> ChordSurface;
    /// 执行动作。stub 阶段只 log；真实动作（#10/#11/#12）在此实现副作用。
    async fn execute(&self, app: &tauri::AppHandle) -> Result<(), String>;
}

/// Chord 动作注册表。
pub struct ChordRegistry {
    actions: Vec<Arc<dyn ChordAction>>,
}

impl ChordRegistry {
    pub fn new() -> Self {
        Self { actions: Vec::new() }
    }

    /// 注册一个动作。
    pub fn register(&mut self, action: Arc<dyn ChordAction>) {
        tracing::debug!(id = action.id(), key = action.key().to_string(), "chord action registered");
        self.actions.push(action);
    }

    /// 列出所有动作元数据（供前端 Ghost overlay 提示层渲染）。
    ///
    /// - `disabled`:被 disable 的 action id 列表,命中即跳过
    /// - `language`:当前 UI 语言（`AppConfig.language`）,用于解析 `LocalizableText` 到字符串
    pub fn list(&self, disabled: &[String], language: &str) -> Vec<serde_json::Value> {
        self.actions
            .iter()
            .filter(|a| !disabled.iter().any(|d| d == a.id()))
            .map(|a| {
                serde_json::json!({
                    "id": a.id(),
                    "key": a.key().to_string(),
                    "label": a.label().resolve(language),
                    "surface": a.surface().as_str(),
                })
            })
            .collect()
    }

    /// 列出所有动作 + 各自 enabled 状态（供设置页展示所有可开关的 Chord）。
    /// 与 `list` 的区别:不过滤 disabled,而是把 disabled 状态作为 `enabled` 字段返回。
    pub fn list_all(&self, disabled: &[String], language: &str) -> Vec<serde_json::Value> {
        self.actions
            .iter()
            .map(|a| {
                let is_enabled = !disabled.iter().any(|d| d == a.id());
                serde_json::json!({
                    "id": a.id(),
                    "key": a.key().to_string(),
                    "label": a.label().resolve(language),
                    "surface": a.surface().as_str(),
                    "enabled": is_enabled,
                })
            })
            .collect()
    }

    /// 按字母键触发对应动作，返回动作的 surface（供 command 层决定显示哪个窗口）。
    /// 键未注册 → Err（前端会 log，不弹窗）。
    pub async fn trigger(&self, key: &str, app: &tauri::AppHandle) -> Result<ChordSurface, String> {
        let lower = key.to_lowercase();
        let action = self
            .actions
            .iter()
            .find(|a| a.key().to_string() == lower)
            .ok_or_else(|| format!("未注册的 chord 键: {lower}"))?;
        let surface = action.surface();
        tracing::info!(id = action.id(), key = %lower, surface = ?surface, "chord trigger");
        action.execute(app).await?;
        Ok(surface)
    }
}

impl Default for ChordRegistry {
    fn default() -> Self {
        Self::new()
    }
}

// ── stub 动作（0.8.5 骨架占位，#10 截图落地时替换）─────────────────────────────

struct StubAction {
    id: &'static str,
    key: char,
    label: LocalizableText,
    surface: ChordSurface,
}

#[async_trait::async_trait]
impl ChordAction for StubAction {
    fn id(&self) -> &'static str {
        self.id
    }
    fn key(&self) -> char {
        self.key
    }
    fn label(&self) -> &LocalizableText {
        &self.label
    }
    fn surface(&self) -> ChordSurface {
        self.surface
    }
    async fn execute(&self, _app: &tauri::AppHandle) -> Result<(), String> {
        tracing::info!(id = self.id, "chord stub action（待 #10 实现）");
        Ok(())
    }
}

/// LocalizableText 便捷构造:zh/en 双语。
fn bilingual(zh: &str, en: &str) -> LocalizableText {
    let mut map = std::collections::HashMap::new();
    map.insert("zh".to_string(), zh.to_string());
    map.insert("en".to_string(), en.to_string());
    LocalizableText::Localized(map)
}

/// 构建默认 ChordRegistry（注册第一批动作）。
/// - Alt+A 截图（Screenshot surface，#10 落地时替换 stub）
/// - Alt+Q 智能划词（MiniBall surface，已在 #11 落地为真实实现——registry 只声明，
///   真正的悬浮球逻辑在 commands::trigger_chord 消费 surface 后走 window::show_chord_ball）
/// - Alt+C 剪贴板历史（0.8.5 §6.4 起走 fill-query：把"剪贴板 " 填进主窗搜索框，
///   由 ClipboardEngine 展开 results，激活时 Copy + 回写 hit）
pub fn build_default_registry() -> ChordRegistry {
    let mut reg = ChordRegistry::new();
    reg.register(Arc::new(StubAction {
        id: "screenshot",
        key: 'a',
        label: bilingual("区域截图", "Screenshot"),
        surface: ChordSurface::Screenshot,
    }));
    reg.register(Arc::new(StubAction {
        id: "selection",
        key: 'q',
        label: bilingual("智能划词", "Smart selection"),
        surface: ChordSurface::MiniBall,
    }));
    reg.register(Arc::new(ClipboardHistoryAction {
        label: bilingual("剪贴板历史", "Clipboard history"),
    }));
    reg
}

/// Alt+C 剪贴板历史（0.8.5 §6.4 定位反思后重构）。
///
/// **策略**：Chord 只提供快捷键直达能力，不新造独占 UI。execute 里：
/// 1. `window::invoke(app)` — 主窗 show + 焦点
/// 2. `emit "blink://chord-fill-query"` payload = `"剪贴板 "` — 前端填搜索框 + dispatch input
/// 3. 后续走 ClipboardEngine 常规召回链，激活时 SearchAction::Copy + record_clipboard_hit
///
/// surface = Default —— 不切窗口形态、不 emit chord-panel（Panel 变体已 deprecated）。
struct ClipboardHistoryAction {
    label: LocalizableText,
}

#[async_trait::async_trait]
impl ChordAction for ClipboardHistoryAction {
    fn id(&self) -> &'static str {
        "clipboard_history"
    }
    fn key(&self) -> char {
        'c'
    }
    fn label(&self) -> &LocalizableText {
        &self.label
    }
    fn surface(&self) -> ChordSurface {
        ChordSurface::Default
    }
    async fn execute(&self, app: &tauri::AppHandle) -> Result<(), String> {
        // 主窗 show + 焦点（同步）
        crate::infra::platform::window::invoke(app);
        // 前端 lifecycle listen 后填搜索框 + dispatch input → ClipboardEngine 召回
        app.emit("blink://chord-fill-query", "剪贴板 ")
            .map_err(|e| e.to_string())?;
        Ok(())
    }
}
