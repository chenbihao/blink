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

use crate::domain::event_names::EventNames;
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
    /// 触发语义。`None`（未设置）时由动作的 `default_semantic()` 兜底——
    /// 这是 0.11 review W3 的修复：此前用 `ChordSemantic`（默认 Tap）无法区分
    /// "用户显式设 Tap" 与 "未设置"，导致 voice_input 重绑 key 后从 Hold 静默降级 Tap。
    /// 改成 `Option<ChordSemantic>` 后，`None` 明确表示"未设置，走 default"。
    ///
    /// **向后兼容**：老配置 `"semantic": "tap"` 反序列化为 `Some(Tap)`；
    /// 缺失字段为 `None`（走 default）。
    #[serde(default)]
    pub semantic: Option<ChordSemantic>,
}

impl Default for ChordBinding {
    fn default() -> Self {
        Self {
            key: String::new(),
            modifiers: default_alt_modifiers(),
            semantic: None,
        }
    }
}

fn default_alt_modifiers() -> Vec<String> {
    vec!["alt".to_string()]
}

/// 所有 chord 动作的键位绑定集合（0.10.7）。
///
/// 每个字段对应一个 chord action id。新增 chord 动作时加字段 + serde default 兜底。
/// `key` 为空字符串表示用动作的 `default_key()` 兜底；`semantic` 为 `None`
/// 表示用动作的 `default_semantic()` 兜底。
///
/// **0.11 review W3 修复**：`semantic` 从 `ChordSemantic` 改为 `Option<ChordSemantic>`，
/// 解决了"无法区分用户显式设 Tap 与未设置"的歧义——voice_input 重绑 key 后不再
/// 静默从 Hold 降级 Tap。`None` 明确表示"未设置，走 default"。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ChordBindings {
    #[serde(default)]
    pub voice_input: ChordBinding,
    #[serde(default)]
    pub screenshot: ChordBinding,
    #[serde(default)]
    pub clipboard_history: ChordBinding,
    /// 0.12.1: AI 对话窗口（默认 Alt+Q）。老配置缺字段时 serde default 兜底。
    #[serde(default)]
    pub chat: ChordBinding,
    /// 0.16.9: 编辑当前内容（默认 Alt+E）。老配置缺字段时 serde default 兜底。
    #[serde(default)]
    pub edit: ChordBinding,
    /// 0.16.9: 钉为便签（默认 Alt+S）。老配置缺字段时 serde default 兜底。
    #[serde(default)]
    pub sticky: ChordBinding,
}

impl ChordBindings {
    /// 按 chord action id 取对应 binding 的不可变引用。
    /// 未注册的 id 返回 None。
    pub fn get(&self, id: &str) -> Option<&ChordBinding> {
        match id {
            "voice_input" => Some(&self.voice_input),
            "screenshot" => Some(&self.screenshot),
            "clipboard_history" => Some(&self.clipboard_history),
            "chat" => Some(&self.chat),
            "edit" => Some(&self.edit),
            "sticky" => Some(&self.sticky),
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
            "chat" => Some(&mut self.chat),
            "edit" => Some(&mut self.edit),
            "sticky" => Some(&mut self.sticky),
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

    /// 解析某动作的生效语义：`binding.semantic` 为 `Some` 用 binding，否则用 default。
    ///
    /// **0.11 review W3**：此前用 `binding.key 非空` 作为"用户改过"的代理判定，
    /// 用户改了 semantic 但没改 key 时会走 default。改成 `Option<ChordSemantic>` 后，
    /// `semantic` 字段本身就是"是否设置"的权威信号——语义干净、无歧义。
    ///
    /// **0.11.7 修复**：`voice_input` 强制走 default（Hold），忽略 binding.semantic。
    /// 语义上 voice_input 的键位与语义都由 hotkey 配置决定（前端 UI 已锁 keyLocked），
    /// 但历史 DB 或前端旧代码（`chord.js` 曾硬编码 `semantic: "tap"`）可能残留脏值。
    /// 若走 unwrap_or(default) 分支，脏值 `Some(Tap)` 会让 voice_input 被错误收进 tap_keys，
    /// 导致 LL hook 吞掉 Alt+Space keydown → 主窗唤起失效。此处按 id 特判兜底，
    /// 与 `default_key()` 的锁定策略保持一致（key 也不接受 binding 覆盖）。
    pub fn effective_semantic(&self, id: &str, default_semantic: ChordSemantic) -> ChordSemantic {
        // voice_input 的语义硬锁：忽略任何 binding.semantic，永远走 default_semantic()。
        if id == "voice_input" {
            return default_semantic;
        }
        match self.get(id) {
            Some(b) => b.semantic.unwrap_or(default_semantic),
            None => default_semantic,
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
    /// 是否需要输入框文本作为入参（0.16.2）。
    ///
    /// `false`（默认）：仅空 query 时可触发，无入参。
    /// `true`：非空 query 时也可触发，输入框文本通过 `trigger_chord` 的 `input_text`
    /// 参数传入 `ActionContext.arguments["input"]`。
    ///
    /// 前端 `getTapKeys()` 据此动态过滤：空 query 返回全部 tap 键，
    /// 非空 query 只返回 `requires_input=true` 的键。
    fn requires_input(&self) -> bool {
        false
    }
    /// 空文本时是否在提示条隐藏此动作（0.16.11）。
    ///
    /// `false`（默认）：空文本时正常显示提示。
    /// `true`：空文本时 overlay 不显示，但触发仍可用（保留 awareness 选区等兜底场景）。
    /// 仅影响提示可见性，不影响 `getTapKeys()` 触发集合。
    ///
    /// 典型：contextual 动作（edit/sticky）覆盖为 true——它们对"当前内容"做动作，
    /// 空文本时提示价值低；idle 入口（screenshot/clipboard）和明确入口（chat）保持 false。
    fn hint_hidden_when_empty(&self) -> bool {
        false
    }
    /// 显示名（走 `LocalizableText`--registry 声明 zh/en，list() 按 language 解析）。
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
                    "requires_input": a.requires_input(),
                    "hint_hidden_when_empty": a.hint_hidden_when_empty(),
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
    pub fn action_id_for_key(&self, key: &str, bindings: &ChordBindings) -> Option<&str> {
        let lower = key.to_lowercase();
        self.actions
            .iter()
            .find(|a| bindings.effective_key(a.id(), a.default_key()) == lower)
            .map(|a| a.id())
    }

    /// 派生 native exclusive chord 可吞的键集合。
    ///
    /// 只含 enabled、Tap 语义、`requires_input=false`（空 query 可 native 触发）的键。
    /// 供 app 层 `refresh_input_config` 构建 `InputConfigSnapshot.exclusive_tap_keys`。
    pub fn exclusive_tap_keys(
        &self,
        bindings: &ChordBindings,
        disabled: &[String],
    ) -> std::collections::HashSet<String> {
        self.actions
            .iter()
            .filter(|a| !disabled.iter().any(|d| d == a.id()))
            .filter(|a| {
                bindings.effective_semantic(a.id(), a.default_semantic()) == ChordSemantic::Tap
            })
            .filter(|a| !a.requires_input())
            .map(|a| {
                bindings
                    .effective_key(a.id(), a.default_key())
                    .to_lowercase()
            })
            .collect()
    }

    /// 按字母键触发对应动作，返回动作的 surface（供 command 层决定显示哪个窗口）。
    /// 键未注册 → Err（前端会 log，不弹窗）。
    ///
    /// 0.8.6 重构：统一走 `Action::execute` 返回 `ActionOutcome`，
    /// registry 层按 outcome 分派副作用（Emit → emit 事件）。
    ///
    /// 0.10.7：键位由 binding 覆盖，匹配时用 effective_key。
    ///
    /// 0.16.2：增加 `input` 参数。`requires_input=true` 的 action 通过
    /// `ActionContext.arguments["input"]` 拿到输入框文本；其他 action 忽略。
    pub async fn trigger(
        &self,
        key: &str,
        bindings: &ChordBindings,
        env: &dyn crate::domain::event::DomainEnv,
        input: Option<&str>,
        origin_ref: Option<&str>,
    ) -> Result<ChordSurface, String> {
        let lower = key.to_lowercase();
        let action = self
            .actions
            .iter()
            .find(|a| bindings.effective_key(a.id(), a.default_key()) == lower)
            .ok_or_else(|| format!("未注册的 chord 键: {lower}"))?;
        let surface = action.surface();
        tracing::info!(id = action.id(), key = %lower, surface = ?surface, has_input = input.is_some(), "chord trigger");

        // 0.16.2：requires_input 的 action 把 input 塞进 arguments；其他 action 传空。
        // 0.16.13：origin_ref 也塞进 arguments（chord-E 编辑已有项时继承 hit_count）
        let mut arguments = if action.requires_input() {
            serde_json::json!({ "input": input.unwrap_or("") })
        } else {
            serde_json::json!({})
        };
        if let Some(ref_id) = origin_ref {
            arguments["origin_ref"] = serde_json::Value::String(ref_id.to_string());
        }
        let cx = crate::domain::execution::ActionContext::from_arguments(env, arguments);
        let outcome = action.execute(&cx).await.map_err(|e| e.to_string())?;
        // 按 outcome 分派副作用
        match outcome {
            crate::domain::execution::ActionOutcome::Copy { text, .. } => {
                // Chord 动作的 Copy：当前无 Chord action 产出此变体（预留 0.9）。
                // 真正的剪贴板写入由 command 层或 SearchService::search 的 Copy 路径处理。
                tracing::debug!(len = text.len(), "chord action Copy outcome（当前未消费）");
            }
            crate::domain::execution::ActionOutcome::Emit { event, payload } => {
                env.emit(&event, payload).map_err(|e| e.to_string())?;
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
/// **execute 防御**：前端 `chord.getTapKeys()` 只含 semantic=tap 的键，
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

/// 构建默认 ChordRegistry（注册顺序即提示条展示顺序）。
/// - Alt+Space 语音输入（display-only，触发走 native hotkey hold，此条目仅用于提示条显示）
/// - Alt+Q AI 对话（0.12.1：独立对话窗口）
/// - Alt+A 区域截图（0.8.7：ScreenshotAction 真实实现）
/// - Alt+C 剪贴板历史（0.8.5 §6.4）
pub fn build_default_registry() -> ChordRegistry {
    let mut reg = ChordRegistry::new();
    reg.register(Arc::new(VoiceInputAction {
        label: bilingual("语音输入", "Voice input"),
    }));
    reg.register(Arc::new(ChatAction {
        label: bilingual("AI 对话", "AI chat"),
    }));
    reg.register(Arc::new(ScreenshotAction {
        label: bilingual("区域截图", "Screenshot"),
    }));
    reg.register(Arc::new(ClipboardHistoryAction {
        label: bilingual("剪贴板历史", "Clipboard history"),
    }));
    reg.register(Arc::new(EditAction {
        label: bilingual("编辑窗口", "Edit Window"),
    }));
    reg.register(Arc::new(StickyAction {
        label: bilingual("钉为便签", "Sticky"),
    }));
    reg
}

/// Alt+Q AI 对话窗口（0.12.1）。
///
/// Chord 只提供快捷入口；对话窗口、AgentProvider 和流式消息各自走独立模块，
/// 不把 `AgentBuilder` 能力泄露进主窗口 `AIProvider` 路径。
///
/// **可用性门禁**：command 层按 `AIConfig.enabled` 同时约束列表可见性和触发入口；
/// AI 总开关关闭时不会显示、也不能通过直接 IPC 绕过触发。
struct ChatAction {
    label: LocalizableText,
}

#[async_trait::async_trait]
impl crate::domain::execution::Action for ChatAction {
    fn id(&self) -> &str {
        "chat"
    }

    fn title(&self) -> &LocalizableText {
        &self.label
    }

    fn subtitle(&self) -> &LocalizableText {
        &self.label
    }

    fn schema(&self) -> crate::domain::execution::ActionSchema {
        crate::domain::execution::ActionSchema::empty(
            "chat",
            "Open the independent AI chat window (Alt+Q chord). Optional input_text to prefill.",
        )
    }

    fn danger_class(&self) -> crate::domain::execution::DangerClass {
        crate::domain::execution::DangerClass::Safe
    }

    async fn execute(
        &self,
        cx: &crate::domain::execution::ActionContext<'_>,
    ) -> Result<crate::domain::execution::ActionOutcome, crate::domain::execution::ExecError> {
        // 0.16.2：读 input 参数（requires_input 的 chord 把输入框文本带来）。
        // 仅填充到 chat 输入框，不自动发送--用户可检查/修改后手动回车。
        let initial_text: Option<&str> = cx
            .arguments
            .get("input")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty());
        // 看门狗按 PID 判前台；chat 与主窗同进程，不能指望失焦自动隐藏主窗。
        // 先确认 chat 已创建并聚焦，再隐藏主窗；创建失败时保留主窗，避免用户失去入口。
        cx.env
            .show_chat_window(initial_text)
            .map_err(crate::domain::execution::ExecError::Runtime)?;
        cx.env.hide_main_window("chat_chord");
        Ok(crate::domain::execution::ActionOutcome::Nop)
    }
}

#[async_trait::async_trait]
impl ChordAction for ChatAction {
    fn default_key(&self) -> char {
        'q'
    }

    /// 0.16.2：chat 需要输入框文本作为入参--非空 query 时 Alt+Q 仍可触发，
    /// 把文本带入 chat 窗口输入框（仅填充不发送）。
    fn requires_input(&self) -> bool {
        true
    }

    fn label(&self) -> &LocalizableText {
        &self.label
    }

    fn surface(&self) -> ChordSurface {
        ChordSurface::Default
    }
}

/// Alt+A 区域截图（0.8.7 §九）。
///
/// **执行时序**（防"截到自己"竞态）：
/// 1. 隐藏主窗 → sleep 80ms 让 DWM 合成完成
/// 2. `begin_session` 截取虚拟屏幕存进 SESSION（此刻桌面已无 blink 窗口）
/// 3. `show_screenshot_overlay(meta)` 建 overlay + SetWindowPos 按物理像素定位
/// 4. overlay 前端拉 `blink-screenshot://capture` → 只读 SESSION → 拖选 → 前端合成 PNG → `screenshot_copy/save/pin` 落地（0.11.7 改）
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
        // ⚠️ 临时打桩日志（0.19.14 性能排查用），收尾时清理
        let t0 = std::time::Instant::now();

        // 0.15.7：记录前台窗口 HWND（长截图 PostMessage 滚轮用）——必须在 hide_for_screenshot 之前
        crate::infra::platform::screenshot::record_fg_hwnd();
        let t_record = t0.elapsed();

        // 1. 隐藏主窗——走 cloak 路径（无 Win11 fade 动画，瞬间从桌面消失）
        cx.env.hide_for_screenshot();
        let t_hide = t0.elapsed();

        // 2. 等 DWM 完成一次不含主窗的新合成（DwmFlush + IsVisible 轮询，通常 <20ms）
        cx.env.wait_frame_after_hide().await;
        let t_wait = t0.elapsed();

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
                cx.env.unhide_after_screenshot();
                crate::domain::execution::ExecError::Runtime(e)
            })?;
        let t_capture = t0.elapsed();

        // 4. 撤销 cloak（主窗保持 hidden 状态，只是解除 DWM 雾化标志）——放在建 overlay
        //    之前：万一建 overlay 失败也不会残留 cloak；主窗不 show 用户看不到差别
        cx.env.unhide_after_screenshot();
        let t_unhide = t0.elapsed();

        // 5. 建 overlay + 按 meta 精确定位（物理像素）
        cx.env.show_screenshot_overlay(&meta).map_err(|e| {
            crate::infra::platform::screenshot::end_session();
            crate::domain::execution::ExecError::Runtime(e)
        })?;
        let t_overlay = t0.elapsed();

        tracing::info!(
            total_ms = t_overlay.as_millis() as u64,
            record_ms = t_record.as_millis() as u64,
            hide_ms = (t_hide - t_record).as_millis() as u64,
            wait_frame_ms = (t_wait - t_hide).as_millis() as u64,
            capture_ms = (t_capture - t_wait).as_millis() as u64,
            unhide_ms = (t_unhide - t_capture).as_millis() as u64,
            overlay_ms = (t_overlay - t_unhide).as_millis() as u64,
            vw = meta.virtual_x, vh = meta.virtual_y,
            w = meta.width, h = meta.height,
            "screenshot overlay 已就绪（分步计时）"
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
        cx.env.invoke_main_window();
        // 返回 Emit outcome，前端进入剪贴板独占模式（bypass SearchService pipeline）
        Ok(crate::domain::execution::ActionOutcome::Emit {
            event: EventNames::CHORD_ENTER_MODE.to_string(),
            payload: serde_json::json!({ "mode": "clipboard" }),
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

/// Alt+E 编辑当前内容（0.16.9）。
///
/// 从 `requires_input` 获得输入文本（前端 contextual 解析后传入），
/// 打交通用编辑器。无内容时打开空白编辑器。
///
/// **上下文解析**（前端侧）：
/// 1. active item 的文本 payload
/// 2. 非空 query
/// 3. 空闲态 Awareness 选区
/// 4. 空白
///
/// 前端解析后把内容作为 `inputText` 传入 `trigger_chord`，
/// 后端通过 `ActionContext.arguments["input"]` 读到。
struct EditAction {
    label: LocalizableText,
}

#[async_trait::async_trait]
impl crate::domain::execution::Action for EditAction {
    fn id(&self) -> &str {
        "edit"
    }
    fn title(&self) -> &LocalizableText {
        &self.label
    }
    fn subtitle(&self) -> &LocalizableText {
        &self.label
    }
    fn schema(&self) -> crate::domain::execution::ActionSchema {
        crate::domain::execution::ActionSchema::empty(
            "edit",
            "Open the content editor with the current text (Alt+E chord). Optional input_text to prefill.",
        )
    }
    fn danger_class(&self) -> crate::domain::execution::DangerClass {
        crate::domain::execution::DangerClass::Safe
    }
    async fn execute(
        &self,
        cx: &crate::domain::execution::ActionContext<'_>,
    ) -> Result<crate::domain::execution::ActionOutcome, crate::domain::execution::ExecError> {
        let input: &str = cx
            .arguments
            .get("input")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let origin_ref: Option<&str> = cx
            .arguments
            .get("origin_ref")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty());
        cx.env
            .show_content_editor(
                input,
                Some("编辑内容"),
                "chord",
                origin_ref,
                "clipboard_new",
            )
            .map_err(crate::domain::execution::ExecError::Runtime)?;
        cx.env.hide_main_window("edit_chord");
        Ok(crate::domain::execution::ActionOutcome::Nop)
    }
}

#[async_trait::async_trait]
impl ChordAction for EditAction {
    fn default_key(&self) -> char {
        'e'
    }
    /// 非空 query 时仍可触发——前端解析后把内容传入。
    fn requires_input(&self) -> bool {
        true
    }
    /// 空文本时提示隐藏——edit 是 contextual 动作，空文本时主用途不明确（依赖
    /// awareness 选区兜底），提示价值低。触发仍保留（Alt+E 打开空白编辑器）。
    fn hint_hidden_when_empty(&self) -> bool {
        true
    }
    fn label(&self) -> &LocalizableText {
        &self.label
    }
    fn surface(&self) -> ChordSurface {
        ChordSurface::Default
    }
}

/// Alt+S 将当前内容创建为便签（0.16.9）。
///
/// 与 EditAction 同源获取输入文本，但不进编辑器——直接创建便签并显示桌面窗口。
/// 无内容时创建空白便签。
struct StickyAction {
    label: LocalizableText,
}

#[async_trait::async_trait]
impl crate::domain::execution::Action for StickyAction {
    fn id(&self) -> &str {
        "sticky"
    }
    fn title(&self) -> &LocalizableText {
        &self.label
    }
    fn subtitle(&self) -> &LocalizableText {
        &self.label
    }
    fn schema(&self) -> crate::domain::execution::ActionSchema {
        crate::domain::execution::ActionSchema::empty(
            "sticky",
            "Create a sticky note from the current text (Alt+S chord). Optional input_text to prefill.",
        )
    }
    fn danger_class(&self) -> crate::domain::execution::DangerClass {
        crate::domain::execution::DangerClass::Safe
    }
    async fn execute(
        &self,
        cx: &crate::domain::execution::ActionContext<'_>,
    ) -> Result<crate::domain::execution::ActionOutcome, crate::domain::execution::ExecError> {
        let input: &str = cx
            .arguments
            .get("input")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        cx.env
            .create_sticky_and_show(input, None, None, None, None)
            .await
            .map_err(crate::domain::execution::ExecError::Runtime)?;
        cx.env.hide_main_window("sticky_chord");
        Ok(crate::domain::execution::ActionOutcome::Nop)
    }
}

#[async_trait::async_trait]
impl ChordAction for StickyAction {
    fn default_key(&self) -> char {
        's'
    }
    fn requires_input(&self) -> bool {
        true
    }
    /// 空文本时提示隐藏——sticky 是 contextual 动作，空文本时主用途不明确（依赖
    /// awareness 选区兜底），提示价值低。触发仍保留（Alt+S 创建空白便签）。
    fn hint_hidden_when_empty(&self) -> bool {
        true
    }
    fn label(&self) -> &LocalizableText {
        &self.label
    }
    fn surface(&self) -> ChordSurface {
        ChordSurface::Default
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_chat_binding_uses_alt_q_tap() {
        let bindings = ChordBindings::default();
        assert_eq!(bindings.effective_key("chat", 'q'), "q");
        assert_eq!(
            bindings.effective_semantic("chat", ChordSemantic::Tap),
            ChordSemantic::Tap
        );
        assert_eq!(bindings.chat.modifiers, vec!["alt"]);
    }

    #[test]
    fn chat_binding_roundtrip_and_override_are_backward_compatible() {
        let legacy: ChordBindings = serde_json::from_value(serde_json::json!({
            "voice_input": {},
            "screenshot": {},
            "clipboard_history": {}
        }))
        .unwrap();
        assert_eq!(legacy.effective_key("chat", 'q'), "q");

        let mut bindings = legacy;
        bindings.get_mut("chat").unwrap().key = "x".into();
        assert_eq!(bindings.effective_key("chat", 'q'), "x");
    }

    #[test]
    fn default_registry_exposes_chat_on_q() {
        let registry = build_default_registry();
        let bindings = ChordBindings::default();
        assert_eq!(registry.action_id_for_key("q", &bindings), Some("chat"));

        let listed = registry.list(&[], &bindings, "zh");
        let chat = listed.iter().find(|item| item["id"] == "chat").unwrap();
        assert_eq!(chat["key"], "q");
        assert_eq!(chat["semantic"], "tap");
        assert_eq!(chat["label"], "AI 对话");
        assert_eq!(chat["surface"], "default");
    }

    #[test]
    fn edit_action_registered_on_e() {
        let registry = build_default_registry();
        let bindings = ChordBindings::default();
        assert_eq!(registry.action_id_for_key("e", &bindings), Some("edit"));
        let listed = registry.list(&[], &bindings, "zh");
        let edit = listed.iter().find(|item| item["id"] == "edit").unwrap();
        assert_eq!(edit["key"], "e");
        assert_eq!(edit["requires_input"], true);
        assert_eq!(edit["label"], "编辑窗口");
    }

    #[test]
    fn sticky_action_registered_on_s() {
        let registry = build_default_registry();
        let bindings = ChordBindings::default();
        assert_eq!(registry.action_id_for_key("s", &bindings), Some("sticky"));
        let listed = registry.list(&[], &bindings, "en");
        let sticky = listed.iter().find(|item| item["id"] == "sticky").unwrap();
        assert_eq!(sticky["key"], "s");
        assert_eq!(sticky["requires_input"], true);
        assert_eq!(sticky["label"], "Sticky");
    }

    #[test]
    fn edit_sticky_bindings_support_override() {
        let mut bindings = ChordBindings::default();
        // 默认走 default_key
        assert_eq!(bindings.effective_key("edit", 'e'), "e");
        assert_eq!(bindings.effective_key("sticky", 's'), "s");

        // 改绑后走 binding.key
        bindings.get_mut("edit").unwrap().key = "x".into();
        bindings.get_mut("sticky").unwrap().key = "z".into();
        assert_eq!(bindings.effective_key("edit", 'e'), "x");
        assert_eq!(bindings.effective_key("sticky", 's'), "z");
    }

    #[test]
    fn edit_sticky_bindings_backward_compatible() {
        // 老配置缺 edit/sticky 字段时 serde default 兜底
        let legacy: ChordBindings = serde_json::from_value(serde_json::json!({
            "voice_input": {},
            "screenshot": {},
            "clipboard_history": {},
            "chat": {}
        }))
        .unwrap();
        assert_eq!(legacy.effective_key("edit", 'e'), "e");
        assert_eq!(legacy.effective_key("sticky", 's'), "s");
    }
}
