//! Chord 模式底层能力（0.8.5 §六 / 0.10.7 键位配置化）。
//!
//! **交互模型**：主窗 visible + Alt hold（状态驱动，非定时器）。前端 `keyboard.js`
//! 检测 `body.chord-visible` 显示 Ghost overlay 层的提示（§6.5.6）+ 拦截 Alt+字母 →
//! `invoke("trigger_chord")`。后端只提供注册表 + 触发分派，**不碰 LL hook**（hook 的
//! tap/hold 状态机天然支持，见 phases §6.2 自洽性证明）。
//!
//! **0.10.7 键位配置化**：`ChordAction::key()` → `default_key()`，实际生效键由
//! `ChordConfig.bindings` 覆盖。`ChordRegistry` 的 list / trigger 等方法接收
//! `&ChordBindings` 合并默认键与用户配置。voice_input 的 `default_semantic = Hold`，
//! PR2 起原生 hotkey hook 读 chord 配置决定是否 hold 触发。
//!
//! **四域约束**（0.8.4）：Chord 动作是 Execution 域消费者。真实动作（截图/剪贴板）
//! 自行采集所需 Awareness，参数注入必须显式
//! （`ExecArg::UserExplicit`），不能无脑抽 snapshot 重蹈 0.8.4 修掉的 bug。
//! Alt+Space 语音输入是 display-only 条目——触发走 native hotkey hold 状态机，
//! 不经 trigger_chord，execute() 仅作防御性 Nop。
//!
//! **i18n**（0.8.5.1 §6.6）：`label` 走 `LocalizableText`——registry 侧声明 zh/en
//! 双语，`list()` 按当前 UI 语言解析成字符串给前端；ChordAction 实现方（本 mod 的
//! VoiceInputAction / ClipboardHistoryAction）用 `LocalizableText::Localized` 双语字面量。

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tauri::Emitter;

use crate::domain::plugin::LocalizableText;

// ── 0.10.7：chord 键位配置类型（域层定义，app/config 引用）────────────────────

/// chord 键位语义（0.10.7）。
///
/// - `Tap`：按下即触发（截图 / 剪贴板），前端 keydown / hook 吞键直达。
/// - `Hold`：长按触发（语音录音），走 hotkey hook 的 tap/hold 状态机。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChordSemantic {
    Tap,
    Hold,
}

impl Default for ChordSemantic {
    fn default() -> Self {
        ChordSemantic::Tap
    }
}

/// 单个 chord 动作的键位绑定（0.10.7）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChordBinding {
    /// 主键（与 hotkey 配置同命名：`" "` / `"a"` / `"c"` 等）。
    /// 空字符串表示用动作的 `default_key()` 兜底。
    #[serde(default)]
    pub key: String,
    /// 修饰键列表（当前只有 `["alt"]`，预留扩展）。
    #[serde(default = "default_alt_modifiers")]
    pub modifiers: Vec<String>,
    /// 触发语义。未设置时由动作的 `default_semantic()` 兜底。
    #[serde(default)]
    pub semantic: ChordSemantic,
}

impl Default for ChordBinding {
    fn default() -> Self {
        Self {
            key: String::new(),
            modifiers: default_alt_modifiers(),
            semantic: ChordSemantic::default(),
        }
    }
}

fn default_alt_modifiers() -> Vec<String> {
    vec!["alt".to_string()]
}

/// 所有 chord 动作的键位绑定集合（0.10.7）。
///
/// 每个字段对应一个 chord action id。新增 chord 动作时加字段 + serde default 兜底。
/// `key` 为空字符串时表示用动作的 `default_key()` 兜底；`semantic` 默认 `Tap`
/// 时由动作的 `default_semantic()` 兜底（注意：无法区分"用户显式设 Tap"与"未设置"，
/// 因此 hold 类动作如 voice_input 必须在 default_semantic 返回 Hold，且用户若要改回
/// Tap 需显式设置——当前不暴露此 UI）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ChordBindings {
    #[serde(default)]
    pub voice_input: ChordBinding,
    #[serde(default)]
    pub screenshot: ChordBinding,
    #[serde(default)]
    pub clipboard_history: ChordBinding,
}

impl ChordBindings {
    /// 按 chord action id 取对应 binding 的不可变引用。
    /// 未注册的 id 返回 None。
    pub fn get(&self, id: &str) -> Option<&ChordBinding> {
        match id {
            "voice_input" => Some(&self.voice_input),
            "screenshot" => Some(&self.screenshot),
            "clipboard_history" => Some(&self.clipboard_history),
            _ => None,
        }
    }

    /// 按 chord action id 取可变引用（设置页改键用）。
    #[allow(dead_code)]
    pub fn get_mut(&mut self, id: &str) -> Option<&mut ChordBinding> {
        match id {
            "voice_input" => Some(&mut self.voice_input),
            "screenshot" => Some(&mut self.screenshot),
            "clipboard_history" => Some(&mut self.clipboard_history),
            _ => None,
        }
    }

    /// 解析某动作的生效键：binding.key 非空用 binding，否则用 default_key。
    /// 未注册 id 返回 None。
    pub fn effective_key(&self, id: &str, default_key: char) -> String {
        match self.get(id) {
            Some(b) if !b.key.is_empty() => b.key.clone(),
            _ => default_key.to_string(),
        }
    }

    /// 解析某动作的生效语义：binding 显式覆盖（当前实现：voice_input 默认 Hold，
    /// 其余默认 Tap；若 binding.semantic 被用户显式设置则以 binding 为准）。
    /// 由于 `ChordSemantic::default() == Tap`，无法区分"未设置"与"显式 Tap"，
    /// 因此 hold 类动作（voice_input）的 default 必须在调用方处理——本方法
    /// 直接返回 binding.semantic，调用方在 binding 为默认值时用 default_semantic 兜底。
    pub fn effective_semantic(&self, id: &str, default_semantic: ChordSemantic) -> ChordSemantic {
        // 简单策略：binding 未被用户改过（key 为空且 modifiers 为默认 alt）时用 default。
        // 这避免 voice_input 被错误降级为 Tap。设置页保存时若用户显式改 semantic，
        // binding.key 一般也会被一起设置（改键必然产生非空 key）。
        match self.get(id) {
            Some(b) if !b.key.is_empty() => b.semantic,
            _ => default_semantic,
        }
    }
}

// ── Chord 触发后的窗口形态 ─────────────────────────────────────────────────────

/// Chord 触发后的窗口形态（决定前端如何切主窗形态）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChordSurface {
    /// 全屏截图覆盖（Alt+A）
    Screenshot,
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
            ChordSurface::Panel => "panel",
            ChordSurface::Default => "default",
        }
    }
}

/// Chord 动作契约（0.8.6 §8.1.1：`ChordAction: Action` supertrait / 0.10.7 键位配置化）。
///
/// 在 `Action` trait 基础上扩展 Chord 特有属性（默认触发键 / 语义 / 窗口形态 / 显示名）。
/// `execute` 统一走 `Action::execute`，返回 `ActionOutcome`（Emit / Nop 等）。
/// 实现方注册到 [`ChordRegistry`]，前端按生效键（binding 覆盖 default）触发。
#[async_trait::async_trait]
pub trait ChordAction: crate::domain::execution::Action {
    /// 默认触发字母（小写，如 `'a'`）。用户可在设置页通过 binding 覆盖。
    fn default_key(&self) -> char;
    /// 默认触发语义。`Tap` = 按下即触发，`Hold` = 长按触发（走 hotkey 状态机）。
    fn default_semantic(&self) -> ChordSemantic {
        ChordSemantic::Tap
    }
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
        Self {
            actions: Vec::new(),
        }
    }

    /// 注册一个动作。
    pub fn register(&mut self, action: Arc<dyn ChordAction>) {
        tracing::debug!(
            id = action.id(),
            default_key = action.default_key().to_string(),
            semantic = ?action.default_semantic(),
            "chord action registered"
        );
        self.actions.push(action);
    }

    /// 列出所有动作元数据（供前端 Ghost overlay 提示层渲染）。
    ///
    /// - `disabled`:被 disable 的 action id 列表,命中即跳过
    /// - `bindings`:键位绑定（0.10.7），覆盖各动作的 default_key
    /// - `language`:当前 UI 语言（`AppConfig.language`）,用于解析 `LocalizableText` 到字符串
    pub fn list(
        &self,
        disabled: &[String],
        bindings: &ChordBindings,
        language: &str,
    ) -> Vec<serde_json::Value> {
        self.actions
            .iter()
            .filter(|a| !disabled.iter().any(|d| d == a.id()))
            .map(|a| {
                let key = bindings.effective_key(a.id(), a.default_key());
                let semantic = bindings.effective_semantic(a.id(), a.default_semantic());
                serde_json::json!({
                    "id": a.id(),
                    "key": key,
                    "semantic": match semantic {
                        ChordSemantic::Tap => "tap",
                        ChordSemantic::Hold => "hold",
                    },
                    "label": a.label().resolve(language),
                    "surface": a.surface().as_str(),
                })
            })
            .collect()
    }

    /// 列出所有动作 + 各自 enabled 状态（供设置页展示所有可开关的 Chord）。
    /// 与 `list` 的区别:不过滤 disabled,而是把 disabled 状态作为 `enabled` 字段返回。
    pub fn list_all(
        &self,
        disabled: &[String],
        bindings: &ChordBindings,
        language: &str,
    ) -> Vec<serde_json::Value> {
        self.actions
            .iter()
            .map(|a| {
                let is_enabled = !disabled.iter().any(|d| d == a.id());
                let key = bindings.effective_key(a.id(), a.default_key());
                let semantic = bindings.effective_semantic(a.id(), a.default_semantic());
                serde_json::json!({
                    "id": a.id(),
                    "key": key,
                    "semantic": match semantic {
                        ChordSemantic::Tap => "tap",
                        ChordSemantic::Hold => "hold",
                    },
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
    ///
    /// 0.10.7：键位由 binding 覆盖，匹配时用 effective_key。
    pub fn action_id_for_key(&self, key: &str, bindings: &ChordBindings) -> Option<&str> {
        let lower = key.to_lowercase();
        self.actions
            .iter()
            .find(|a| bindings.effective_key(a.id(), a.default_key()) == lower)
            .map(|a| a.id())
    }

    /// 按字母键触发对应动作，返回动作的 surface（供 command 层决定显示哪个窗口）。
    /// 键未注册 → Err（前端会 log，不弹窗）。
    ///
    /// 0.8.6 重构：统一走 `Action::execute` 返回 `ActionOutcome`，
    /// registry 层按 outcome 分派副作用（Emit → emit 事件）。
    ///
    /// 0.10.7：键位由 binding 覆盖，匹配时用 effective_key。
    pub async fn trigger(
        &self,
        key: &str,
        bindings: &ChordBindings,
        app: &tauri::AppHandle,
    ) -> Result<ChordSurface, String> {
        let lower = key.to_lowercase();
        let action = self
            .actions
            .iter()
            .find(|a| bindings.effective_key(a.id(), a.default_key()) == lower)
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
            // 0.11.0 改进 1: Items 变体——Chord action 当前不产此变体（仅 PluginActionAdapter 产）。
            // 加此分支满足 match 穷尽性；若未来 Chord action 产 Items，需在此分派。
            crate::domain::execution::ActionOutcome::Items { .. } => {
                tracing::debug!("chord action Items outcome（当前未消费）");
            }
        }
        Ok(surface)
    }
}

impl Default for ChordRegistry {
    fn default() -> Self {
        Self::new()
    }
}

// ── Alt+Space 语音输入（display-only chord 条目）─────────────────────────────

/// Alt+Space 语音输入（display-only chord 条目，0.10.7 升级为 semantic=hold）。
///
/// **特殊性**：触发**不走 chord tap 路径**——Alt+Space 由 native hotkey hook 的
/// hold 状态机处理（`HotkeyEvent::Hold` → `VoiceService::start_recording`）。
/// 此条目仅用于在 chord 提示条中显示「Alt+Space 语音输入」，让 hold-to-talk
/// 这个隐藏交互变得可发现。
///
/// **0.10.7**：`default_semantic = Hold`，PR2 起原生 hotkey hook 读 chord 配置
/// 决定是否 hold 触发；chord 总开关 / disabled 列表也会门禁 hold 路径。
///
/// **execute 防御**：前端 `keyboard.js` 的 `CHORD_KEYS` 只含 semantic=tap 的键，
/// Alt+Space 不会走 `onChordTrigger → trigger_chord`。若因任何原因被调用（如未来改动），
/// execute 返回 Nop——真正录音由 hotkey 层已在 hold 时启动。
///
/// **可见性门禁**：`list_chord_actions` / `list_all_chord_actions` command 层
/// 按 `SttConfig.enabled` 过滤——STT 未启用时不返回此条目（提示条不显示、
/// 设置页不显示），语音的可见性自然绑定到语音总开关。
struct VoiceInputAction {
    label: LocalizableText,
}

#[async_trait::async_trait]
impl crate::domain::execution::Action for VoiceInputAction {
    fn id(&self) -> &str {
        "voice_input"
    }
    fn title(&self) -> &LocalizableText {
        &self.label
    }
    fn subtitle(&self) -> &LocalizableText {
        &self.label
    }
    /// 0.9.0 §3.3 铁则:Chord 动作显式覆盖 schema
    fn schema(&self) -> crate::domain::execution::ActionSchema {
        crate::domain::execution::ActionSchema::empty(
            "voice_input",
            "Voice input via hold-to-talk on Alt+Space. Display-only chord entry — trigger is handled by the native hotkey hook, not trigger_chord.",
        )
    }
    /// 语音录音不改系统状态（音频采集 + STT），注入是 G2 的职责——Safe
    fn danger_class(&self) -> crate::domain::execution::DangerClass {
        crate::domain::execution::DangerClass::Safe
    }
    async fn execute(
        &self,
        _cx: &crate::domain::execution::ActionContext<'_>,
    ) -> Result<crate::domain::execution::ActionOutcome, crate::domain::execution::ExecError> {
        tracing::warn!(
            "voice_input chord execute 被调用（不应发生：Alt+Space 由 hotkey hook 处理）"
        );
        Ok(crate::domain::execution::ActionOutcome::Nop)
    }
}

#[async_trait::async_trait]
impl ChordAction for VoiceInputAction {
    fn default_key(&self) -> char {
        ' '
    }
    fn default_semantic(&self) -> ChordSemantic {
        ChordSemantic::Hold
    }
    fn label(&self) -> &LocalizableText {
        &self.label
    }
    fn surface(&self) -> ChordSurface {
        ChordSurface::Default
    }
}

/// LocalizableText 便捷构造:zh/en 双语。
fn bilingual(zh: &str, en: &str) -> LocalizableText {
    let mut map = std::collections::HashMap::new();
    map.insert("zh".to_string(), zh.to_string());
    map.insert("en".to_string(), en.to_string());
    LocalizableText::Localized(map)
}

/// 构建默认 ChordRegistry（注册第一批动作）。**注册顺序即提示条展示顺序**。
/// - Alt+Space 语音输入（display-only，触发走 native hotkey hold，此条目仅用于提示条显示）
/// - Alt+A 区域截图（0.8.7：ScreenshotAction 真实实现）
/// - Alt+C 剪贴板历史（0.8.5 §6.4）
pub fn build_default_registry() -> ChordRegistry {
    let mut reg = ChordRegistry::new();
    reg.register(Arc::new(VoiceInputAction {
        label: bilingual("语音输入", "Voice input"),
    }));
    reg.register(Arc::new(ScreenshotAction {
        label: bilingual("区域截图", "Screenshot"),
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
    /// 0.9.0 §3.3 铁则:Chord 动作显式覆盖 schema
    fn schema(&self) -> crate::domain::execution::ActionSchema {
        crate::domain::execution::ActionSchema::empty(
            "screenshot",
            "Capture a region of the screen (Alt+A chord). Interactive—no arguments.",
        )
    }
    /// 截图只读屏幕像素,不改文件系统——Safe
    fn danger_class(&self) -> crate::domain::execution::DangerClass {
        crate::domain::execution::DangerClass::Safe
    }
    async fn execute(
        &self,
        cx: &crate::domain::execution::ActionContext<'_>,
    ) -> Result<crate::domain::execution::ActionOutcome, crate::domain::execution::ExecError> {
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
            .map_err(|e| {
                crate::domain::execution::ExecError::Runtime(format!("截屏 task 崩溃: {e}"))
            })?
            .map_err(|e| {
                // 截屏失败也要撤销 cloak,避免主窗永远隐形
                crate::infra::platform::window::unhide_after_screenshot(cx.app_handle);
                crate::domain::execution::ExecError::Runtime(e)
            })?;

        // 4. 撤销 cloak（主窗保持 hidden 状态，只是解除 DWM 雾化标志）——放在建 overlay
        //    之前：万一建 overlay 失败也不会残留 cloak；主窗不 show 用户看不到差别
        crate::infra::platform::window::unhide_after_screenshot(cx.app_handle);

        // 5. 建 overlay + 按 meta 精确定位（物理像素）
        crate::infra::platform::window::show_screenshot_overlay(cx.app_handle, meta).map_err(
            |e| {
                crate::infra::platform::screenshot::end_session();
                crate::domain::execution::ExecError::Runtime(e)
            },
        )?;
        tracing::info!(
            total_ms = t0.elapsed().as_millis() as u64,
            "screenshot overlay 已就绪"
        );
        Ok(crate::domain::execution::ActionOutcome::Nop)
    }
}

#[async_trait::async_trait]
impl ChordAction for ScreenshotAction {
    fn default_key(&self) -> char {
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
    /// 0.9.0 §3.3 铁则:Chord 动作显式覆盖 schema
    fn schema(&self) -> crate::domain::execution::ActionSchema {
        crate::domain::execution::ActionSchema::empty(
            "clipboard_history",
            "Open the clipboard history browser (Alt+C chord). No arguments—fills the main search box.",
        )
    }
    /// 剪贴板浏览只读——Safe
    fn danger_class(&self) -> crate::domain::execution::DangerClass {
        crate::domain::execution::DangerClass::Safe
    }
    async fn execute(
        &self,
        cx: &crate::domain::execution::ActionContext<'_>,
    ) -> Result<crate::domain::execution::ActionOutcome, crate::domain::execution::ExecError> {
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
    fn default_key(&self) -> char {
        'c'
    }
    fn label(&self) -> &LocalizableText {
        &self.label
    }
    fn surface(&self) -> ChordSurface {
        ChordSurface::Default
    }
}
