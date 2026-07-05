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

/// Chord 动作契约（0.8.6 §8.1.1：`ChordAction: Action` supertrait）。
///
/// 在 `Action` trait 基础上扩展 Chord 特有属性（触发键 / 窗口形态 / 显示名）。
/// `execute` 统一走 `Action::execute`，返回 `ActionOutcome`（Emit / Nop 等）。
/// 实现方注册到 [`ChordRegistry`]，前端按 `key` 触发。
#[async_trait::async_trait]
pub trait ChordAction: crate::domain::execution::Action {
    /// 触发字母（小写，如 `'a'`）。前端 Alt+此字母 → trigger_chord。
    fn key(&self) -> char;
    /// 显示名（走 `LocalizableText`——registry 声明 zh/en，list() 按 language 解析）。
    fn label(&self) -> &LocalizableText;
    /// 触发后的窗口形态。
    fn surface(&self) -> ChordSurface;
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

    /// 按字母键查找已注册动作的 id（无关 disabled 状态）。
    ///
    /// 供 command 层做 disabled 门禁——先查 id → 再对比 DB 里的 disabled 列表。
    /// registry 本身不持 disabled 状态，保持"注册/分派"单一职责。
    pub fn action_id_for_key(&self, key: &str) -> Option<&str> {
        let lower = key.to_lowercase();
        self.actions
            .iter()
            .find(|a| a.key().to_string() == lower)
            .map(|a| a.id())
    }

    /// 按字母键触发对应动作，返回动作的 surface（供 command 层决定显示哪个窗口）。
    /// 键未注册 → Err（前端会 log，不弹窗）。
    ///
    /// 0.8.6 重构：统一走 `Action::execute` 返回 `ActionOutcome`，
    /// registry 层按 outcome 分派副作用（Emit → emit 事件）。
    pub async fn trigger(&self, key: &str, app: &tauri::AppHandle) -> Result<ChordSurface, String> {
        let lower = key.to_lowercase();
        let action = self
            .actions
            .iter()
            .find(|a| a.key().to_string() == lower)
            .ok_or_else(|| format!("未注册的 chord 键: {lower}"))?;
        let surface = action.surface();
        tracing::info!(id = action.id(), key = %lower, surface = ?surface, "chord trigger");

        let cx = crate::domain::execution::ActionContext::new(app, None);
        let outcome = action.execute(&cx).await.map_err(|e| e.to_string())?;
        // 按 outcome 分派副作用
        match outcome {
            crate::domain::execution::ActionOutcome::Copy { text, .. } => {
                // Chord 动作的 Copy：当前无 Chord action 产出此变体（预留 0.9）。
                // 真正的剪贴板写入由 command 层或 SearchService::search 的 Copy 路径处理。
                tracing::debug!(len = text.len(), "chord action Copy outcome（当前未消费）");
            }
            crate::domain::execution::ActionOutcome::Emit { event, payload } => {
                app.emit(&event, payload).map_err(|e| e.to_string())?;
            }
            crate::domain::execution::ActionOutcome::Open { path } => {
                if let Err(e) = open::that(&path) {
                    tracing::error!(error = %e, %path, "chord action 打开路径失败");
                }
            }
            crate::domain::execution::ActionOutcome::Nop => {}
        }
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
impl crate::domain::execution::Action for StubAction {
    fn id(&self) -> &str {
        self.id
    }
    fn title(&self) -> &LocalizableText {
        &self.label
    }
    fn subtitle(&self) -> &LocalizableText {
        &self.label // stub: subtitle 同 title
    }
    async fn execute(&self, _cx: &crate::domain::execution::ActionContext<'_>) -> Result<crate::domain::execution::ActionOutcome, crate::domain::execution::ExecError> {
        tracing::info!(id = self.id, "chord stub action（待 #10 实现）");
        Ok(crate::domain::execution::ActionOutcome::Nop)
    }
}

#[async_trait::async_trait]
impl ChordAction for StubAction {
    fn key(&self) -> char {
        self.key
    }
    fn label(&self) -> &LocalizableText {
        &self.label
    }
    fn surface(&self) -> ChordSurface {
        self.surface
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
/// - Alt+A 区域截图（0.8.7：ScreenshotAction 真实实现）
/// - Alt+Q 划词翻译（MiniBall surface）
/// - Alt+C 剪贴板历史（0.8.5 §6.4）
pub fn build_default_registry() -> ChordRegistry {
    let mut reg = ChordRegistry::new();
    reg.register(Arc::new(ScreenshotAction {
        label: bilingual("区域截图", "Screenshot"),
    }));
    reg.register(Arc::new(StubAction {
        id: "selection",
        key: 'q',
        label: bilingual("划词翻译", "Selection translate"),
        surface: ChordSurface::MiniBall,
    }));
    reg.register(Arc::new(ClipboardHistoryAction {
        label: bilingual("剪贴板历史", "Clipboard history"),
    }));
    reg
}

/// Alt+A 区域截图（0.8.7 §九）。
///
/// **执行时序**（防"截到自己"竞态）：
/// 1. 隐藏主窗 → sleep 80ms 让 DWM 合成完成
/// 2. `begin_session` 截取虚拟屏幕存进 SESSION（此刻桌面已无 blink 窗口）
/// 3. `show_screenshot_overlay(meta)` 建 overlay + SetWindowPos 按物理像素定位
/// 4. overlay 前端拉 `blink-screenshot://capture` → 只读 SESSION → 拖选 → capture_region 落地
struct ScreenshotAction {
    label: LocalizableText,
}

#[async_trait::async_trait]
impl crate::domain::execution::Action for ScreenshotAction {
    fn id(&self) -> &str {
        "screenshot"
    }
    fn title(&self) -> &LocalizableText {
        &self.label
    }
    fn subtitle(&self) -> &LocalizableText {
        &self.label
    }
    async fn execute(&self, cx: &crate::domain::execution::ActionContext<'_>) -> Result<crate::domain::execution::ActionOutcome, crate::domain::execution::ExecError> {
        let t0 = std::time::Instant::now();

        // 1. 隐藏主窗——走 cloak 路径（无 Win11 fade 动画，瞬间从桌面消失）
        crate::infra::platform::window::hide_for_screenshot(cx.app_handle);

        // 2. 等 DWM 完成一次不含主窗的新合成（DwmFlush + IsVisible 轮询，通常 <20ms）
        let app_handle = cx.app_handle.clone();
        tokio::task::spawn_blocking(move || {
            crate::infra::platform::window::wait_frame_after_hide(&app_handle);
        })
        .await
        .ok();

        // 3. 截屏存 SESSION（此刻桌面上没有 blink，BitBlt 不会拍到自己）。
        //    Win32 阻塞调用（BitBlt + GetDIBits 合计 ~50-100ms 全屏）—— 走 spawn_blocking
        //    避免挤占 tokio worker。
        let meta = tokio::task::spawn_blocking(crate::infra::platform::screenshot::begin_session)
            .await
            .map_err(|e| crate::domain::execution::ExecError::Runtime(format!("截屏 task 崩溃: {e}")))?
            .map_err(|e| {
                // 截屏失败也要撤销 cloak,避免主窗永远隐形
                crate::infra::platform::window::unhide_after_screenshot(cx.app_handle);
                crate::domain::execution::ExecError::Runtime(e)
            })?;

        // 4. 撤销 cloak（主窗保持 hidden 状态，只是解除 DWM 雾化标志）——放在建 overlay
        //    之前：万一建 overlay 失败也不会残留 cloak；主窗不 show 用户看不到差别
        crate::infra::platform::window::unhide_after_screenshot(cx.app_handle);

        // 5. 建 overlay + 按 meta 精确定位（物理像素）
        crate::infra::platform::window::show_screenshot_overlay(cx.app_handle, meta)
            .map_err(|e| {
                crate::infra::platform::screenshot::end_session();
                crate::domain::execution::ExecError::Runtime(e)
            })?;
        tracing::info!(total_ms = t0.elapsed().as_millis() as u64, "screenshot overlay 已就绪");
        Ok(crate::domain::execution::ActionOutcome::Nop)
    }
}

#[async_trait::async_trait]
impl ChordAction for ScreenshotAction {
    fn key(&self) -> char {
        'a'
    }
    fn label(&self) -> &LocalizableText {
        &self.label
    }
    fn surface(&self) -> ChordSurface {
        ChordSurface::Screenshot
    }
}

/// Alt+C 剪贴板历史（0.8.5 §6.4 定位反思后重构）。
///
/// **策略**：Chord 只提供快捷键直达能力，不新造独占 UI。execute 里：
/// 1. `window::invoke(app)` — 主窗 show + 焦点
/// 2. 返回 `ActionOutcome::Emit { event: "blink://chord-fill-query" }` — 前端填搜索框 + dispatch input
/// 3. 后续走 ClipboardEngine 常规召回链，激活时 SearchAction::Copy + record_clipboard_hit
///
/// surface = Default —— 不切窗口形态、不 emit chord-panel（Panel 变体已 deprecated）。
struct ClipboardHistoryAction {
    label: LocalizableText,
}

#[async_trait::async_trait]
impl crate::domain::execution::Action for ClipboardHistoryAction {
    fn id(&self) -> &str {
        "clipboard_history"
    }
    fn title(&self) -> &LocalizableText {
        &self.label
    }
    fn subtitle(&self) -> &LocalizableText {
        &self.label
    }
    async fn execute(&self, cx: &crate::domain::execution::ActionContext<'_>) -> Result<crate::domain::execution::ActionOutcome, crate::domain::execution::ExecError> {
        // 主窗 show + 焦点（同步）
        crate::infra::platform::window::invoke(cx.app_handle);
        // 返回 Emit outcome，由 ChordRegistry::trigger 负责实际 emit
        Ok(crate::domain::execution::ActionOutcome::Emit {
            event: "blink://chord-fill-query".to_string(),
            payload: serde_json::Value::String("剪贴板 ".to_string()),
        })
    }
}

#[async_trait::async_trait]
impl ChordAction for ClipboardHistoryAction {
    fn key(&self) -> char {
        'c'
    }
    fn label(&self) -> &LocalizableText {
        &self.label
    }
    fn surface(&self) -> ChordSurface {
        ChordSurface::Default
    }
}
