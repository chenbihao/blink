//! Chord 模式底层能力（0.8.5 §六）。
//!
//! **交互模型**：主窗 visible + Alt hold（状态驱动，非定时器）。前端 `keyboard.js`
//! 检测 `body.alt-active` 下拉增强菜单 + 拦截 Alt+字母 → `invoke("trigger_chord")`。
//! 后端只提供注册表 + 触发分派，**不碰 LL hook**（hook 的 tap/hold 状态机天然支持，
//! 见 phases §6.2 自洽性证明）。
//!
//! **四域约束**（0.8.4）：Chord 动作是 Execution 域消费者。`ChordContext.snapshot`
//! 预留 0.9 AI function calling（§6.10）；0.8.5 stub 动作直接 `execute(app)`，
//! 真实动作（截图/划词/剪贴板）自行采集所需 Awareness（如划词调 selection 模块），
//! 参数注入必须显式（`ExecArg::UserExplicit`），不能无脑抽 snapshot 重蹈 0.8.4 修掉的 bug。

use std::sync::Arc;

/// Chord 触发后的窗口形态（决定前端如何切主窗形态）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChordSurface {
    /// 全屏截图覆盖（Alt+A）
    Screenshot,
    /// 主窗缩成悬浮小球（Alt+Q）
    MiniBall,
    /// 主窗切面板形态（Alt+C 剪贴板历史 / 未来 AI 面板）
    Panel,
    /// 不变形，直接执行后 hide
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
    /// 显示名（stub 用 &'static str；#13 i18n 化时改 LocalizableText）。
    fn label(&self) -> &'static str;
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

    /// 列出所有动作元数据（供前端增强菜单渲染）。已 disable 的跳过。
    pub fn list(&self, disabled: &[String]) -> Vec<serde_json::Value> {
        self.actions
            .iter()
            .filter(|a| !disabled.iter().any(|d| d == a.id()))
            .map(|a| {
                serde_json::json!({
                    "id": a.id(),
                    "key": a.key().to_string(),
                    "label": a.label(),
                    "surface": a.surface().as_str(),
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

// ── stub 动作（0.8.5 骨架占位，#10/#11/#12 替换为真实实现）──────────────────────

struct StubAction {
    id: &'static str,
    key: char,
    label: &'static str,
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
    fn label(&self) -> &'static str {
        self.label
    }
    fn surface(&self) -> ChordSurface {
        self.surface
    }
    async fn execute(&self, _app: &tauri::AppHandle) -> Result<(), String> {
        tracing::info!(id = self.id, "chord stub action（待 #10/#11/#12 实现）");
        Ok(())
    }
}

/// 构建默认 ChordRegistry（注册第一批 stub 动作）。
/// 真实动作在 #10（截图）/ #11（划词）/ #12（剪贴板）落地时替换 stub。
pub fn build_default_registry() -> ChordRegistry {
    let mut reg = ChordRegistry::new();
    reg.register(Arc::new(StubAction {
        id: "screenshot",
        key: 'a',
        label: "区域截图",
        surface: ChordSurface::Screenshot,
    }));
    reg.register(Arc::new(StubAction {
        id: "selection",
        key: 'q',
        label: "智能划词",
        surface: ChordSurface::MiniBall,
    }));
    reg.register(Arc::new(StubAction {
        id: "clipboard_history",
        key: 'c',
        label: "剪贴板历史",
        surface: ChordSurface::Panel,
    }));
    reg
}
