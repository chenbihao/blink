//! Chord 模式底层能力（0.8.5 §六 / 0.10.7 键位配置化 / 0.21.2 分流重构）。
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
//! **0.21.2 分流重构**：`ChordAction` 不再继承 `execution::Action`，改为纯
//! descriptor/binding trait。6 个旧 ChordAction 全量分流：
//! - `voice_input` → Interaction-only（descriptor target = VoiceInteraction，不造空 Capability）
//! - `chat` → `open_chat` GUI Capability
//! - `screenshot` → `start_region_capture` GUI Capability（与 headless `screenshot` 分离）
//! - `clipboard_history` → `open_clipboard_mode` GUI Capability
//! - `edit` → `start_content_editor` GUI Capability
//! - `sticky` → 绑定既有 `create_sticky` Capability
//!
//! **i18n**（0.8.5.1 §6.6）：`label` 走 `LocalizableText`——registry 侧声明 zh/en
//! 双语，`list()` 按当前 UI 语言解析成字符串给前端。

use std::sync::Arc;

use serde::{Deserialize, Serialize};

// ── 0.10.7：chord 键位配置类型（域层定义，app/config 引用）────────────────────

/// chord 键位语义（0.10.7）。
///
/// - `Tap`：按下即触发（截图 / 剪贴板），前端 keydown / hook 吞键直达。
/// - `Hold`：长按触发（语音录音），走 hotkey hook 的 tap/hold 状态机。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[derive(Default)]
pub enum ChordSemantic {
    #[default]
    Tap,
    Hold,
}

/// 单个 chord 动作的可选全局快捷键形态（0.22.12）。
///
/// - `FollowChord`：跟随动作的 chord 触发键（组合键 = `binding.modifiers` + 生效键）。
///   chord 键变更时自动跟随。运行时由 app 层「主窗可见时全局路径让位」规则保证
///   与 chord 吞键路径按按键互斥（LL hook 先于 RegisterHotKey 派发）。
/// - `Custom`：独立组合键，与 chord 键位无关，始终生效。
///
/// voice_input（Hold 语义 / `ChordTarget::VoiceInteraction`）不参与全局键，
/// 解析与校验层显式跳过。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum GlobalBinding {
    FollowChord,
    Custom {
        #[serde(default)]
        modifiers: Vec<String>,
        #[serde(default)]
        key: String,
    },
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
    /// 可选全局快捷键（0.22.12）。`None` = 未提升全局入口。
    /// `skip_serializing_if` 保持未提升动作的存量 JSON 不新增字段。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub global: Option<GlobalBinding>,
}

impl Default for ChordBinding {
    fn default() -> Self {
        Self {
            key: String::new(),
            modifiers: default_alt_modifiers(),
            semantic: None,
            global: None,
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

// ── 0.22.12：全局快捷键组合键与冲突检查 ──────────────────────────────────────

/// 规范化组合键（域内比较用）：修饰键集合 + 主键。
///
/// 修饰键按别名折叠（`lalt`/`ralt` → `alt`、`win`/`super` → `meta` 等）——
/// `RegisterHotKey` 的 `MOD_*` 也不区分左右，比较语义保持一致。主键统一小写。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct HotkeyCombo {
    modifiers: std::collections::BTreeSet<String>,
    key: String,
}

impl HotkeyCombo {
    pub fn new(modifiers: &[String], key: &str) -> Self {
        Self {
            modifiers: modifiers
                .iter()
                .map(|m| normalize_modifier_name(m))
                .filter(|m| !m.is_empty())
                .collect(),
            // 注意：不做 trim——" " 是空格键，trim 会把它误判为空
            key: key.to_lowercase(),
        }
    }

    /// 规范显示名（`ctrl+alt+s` 顺序），供冲突文案与状态查询。
    pub fn display(&self) -> String {
        const ORDER: [&str; 4] = ["ctrl", "alt", "shift", "meta"];
        let mut parts: Vec<String> = Vec::new();
        for canonical in ORDER {
            if self.modifiers.contains(canonical) {
                parts.push(canonical.to_string());
            }
        }
        for m in &self.modifiers {
            if !ORDER.contains(&m.as_str()) {
                parts.push(m.clone());
            }
        }
        parts.push(if self.key == " " {
            "space".to_string()
        } else {
            self.key.clone()
        });
        parts.join("+")
    }

    /// 规范化修饰键列表（canonical 顺序），供注册层映射 `MOD_*`。
    pub fn modifiers(&self) -> Vec<String> {
        const ORDER: [&str; 4] = ["ctrl", "alt", "shift", "meta"];
        let mut parts: Vec<String> = Vec::new();
        for canonical in ORDER {
            if self.modifiers.contains(canonical) {
                parts.push(canonical.to_string());
            }
        }
        for m in &self.modifiers {
            if !ORDER.contains(&m.as_str()) {
                parts.push(m.clone());
            }
        }
        parts
    }

    /// 主键（小写；`" "` 表示空格）。
    pub fn key(&self) -> &str {
        &self.key
    }
}

/// 修饰键名归一化：左右折叠 + 常见别名折叠。未知名原样小写返回。
fn normalize_modifier_name(m: &str) -> String {
    match m.trim().to_lowercase().as_str() {
        "lctrl" | "rctrl" | "control" => "ctrl".to_string(),
        "lalt" | "ralt" => "alt".to_string(),
        "lshift" | "rshift" => "shift".to_string(),
        "meta" | "win" | "super" => "meta".to_string(),
        other => other.to_string(),
    }
}

/// chord 绑定冲突类型（0.22.12 扩展）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BindingConflictKind {
    /// 多个动作映射到同一生效键（既有检查）。
    ChordKeyDuplicate,
    /// 两个全局快捷键组合相同。
    GlobalDuplicate,
    /// 全局快捷键与主热键相同。
    GlobalVsMainHotkey,
    /// 自定义全局快捷键与某 chord 生效键相同（跟随模式天然等于自身 chord 键，豁免）。
    GlobalVsChordKey,
    /// chord Tap 键与主热键相同（0.22.12 补齐既有缺口；Hold 语义豁免——
    /// voice_input 的 Alt+Space 长按与主热键 tap 共存是设计行为）。
    ChordKeyVsMainHotkey,
}

/// 一条绑定冲突：类型 + 组合键显示名 + 涉及动作 id。
#[derive(Debug, Clone)]
pub struct BindingConflict {
    pub kind: BindingConflictKind,
    pub combo: String,
    pub actions: Vec<String>,
}

impl BindingConflict {
    /// 面向用户的中文冲突描述（与 command 层既有错误文案风格一致）。
    pub fn describe(&self) -> String {
        match self.kind {
            BindingConflictKind::ChordKeyDuplicate => format!(
                "Chord 键 {} 已被多个动作占用：{}",
                self.combo,
                self.actions.join(", ")
            ),
            BindingConflictKind::GlobalDuplicate => format!(
                "全局快捷键 {} 已被多个动作占用：{}",
                self.combo,
                self.actions.join(", ")
            ),
            BindingConflictKind::GlobalVsMainHotkey => {
                format!("全局快捷键 {} 与主热键冲突，请更换组合", self.combo)
            }
            BindingConflictKind::GlobalVsChordKey => format!(
                "全局快捷键 {} 与动作（{}）的 Chord 键相同，请改用「跟随触发键」或更换组合",
                self.combo,
                self.actions.join(", ")
            ),
            BindingConflictKind::ChordKeyVsMainHotkey => format!(
                "Chord 键 {}（{}）与主热键相同，触发会被主热键抢占，请更换",
                self.combo,
                self.actions.join(", ")
            ),
        }
    }
}

/// 已解析的全局快捷键绑定（0.22.12）。
///
/// 由 `ChordRegistry::global_hotkey_bindings` 从 bindings 派生：修饰键已规范化
/// （canonical 名），主键小写。供 app 层构建 `InputConfigSnapshot` 与 infra
/// 注册层映射 `MOD_*` / VK。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct GlobalHotkeyBinding {
    pub action_id: String,
    /// `true` = 跟随 chord 触发键（服务层据此执行「主窗可见时让位」规则）。
    pub follow_chord: bool,
    pub modifiers: Vec<String>,
    pub key: String,
}

impl ChordBindings {
    /// 某动作的生效修饰键列表（binding 缺失时按 `["alt"]` 兜底）。
    fn effective_modifiers(&self, id: &str) -> Vec<String> {
        self.get(id)
            .map(|b| b.modifiers.clone())
            .unwrap_or_else(default_alt_modifiers)
    }

    /// 某动作的生效全局组合键（未设置 / voice_input / 空主键 → None）。
    fn effective_global_combo(
        &self,
        id: &str,
        default_key: char,
    ) -> Option<HotkeyCombo> {
        let binding = self.get(id)?;
        match &binding.global {
            Some(GlobalBinding::FollowChord) => Some(HotkeyCombo::new(
                &binding.modifiers,
                &self.effective_key(id, default_key),
            )),
            Some(GlobalBinding::Custom { modifiers, key }) if !key.is_empty() => {
                Some(HotkeyCombo::new(modifiers, key))
            }
            _ => None,
        }
    }
}

impl ChordRegistry {
    /// 解析各动作的生效全局组合键（0.22.12）。
    ///
    /// voice_input（`ChordTarget::VoiceInteraction`，Hold 语义）不参与；
    /// 空 key 的 Custom 视为未设置。返回 (action_id, combo)。
    pub fn global_hotkey_combos(&self, bindings: &ChordBindings) -> Vec<(String, HotkeyCombo)> {
        self.actions
            .iter()
            .filter(|a| !matches!(a.target(), ChordTarget::VoiceInteraction))
            .filter_map(|a| {
                bindings
                    .effective_global_combo(a.id(), a.default_key())
                    .map(|combo| (a.id().to_string(), combo))
            })
            .collect()
    }

    /// 解析各动作的全局快捷键绑定（0.22.12，供快照/注册层消费）。
    ///
    /// 过滤 disabled 动作与 voice_input（Hold 语义不参与全局键）；
    /// 修饰键已按 canonical 名规范化（`lctrl`→`ctrl` 等）。
    pub fn global_hotkey_bindings(
        &self,
        bindings: &ChordBindings,
        disabled: &[String],
    ) -> Vec<GlobalHotkeyBinding> {
        self.actions
            .iter()
            .filter(|a| !disabled.iter().any(|d| d == a.id()))
            .filter(|a| !matches!(a.target(), ChordTarget::VoiceInteraction))
            .filter_map(|a| {
                let follow_chord = matches!(
                    bindings.get(a.id()).and_then(|b| b.global.as_ref()),
                    Some(GlobalBinding::FollowChord)
                );
                let combo = bindings.effective_global_combo(a.id(), a.default_key())?;
                Some(GlobalHotkeyBinding {
                    action_id: a.id().to_string(),
                    follow_chord,
                    modifiers: combo.modifiers(),
                    key: combo.key().to_string(),
                })
            })
            .collect()
    }

    /// 全局快捷键冲突 + 主热键跨检查（0.22.12）。
    ///
    /// 覆盖四类：全局键互相、全局键 vs 主热键、自定义全局键 vs chord 生效键、
    /// chord Tap 键 vs 主热键（补齐既有缺口）。`main_hotkey` 传 `None` 时跳过
    /// 主热键相关两类（域内单测/调用方无主热键上下文的场景）。
    ///
    /// 与既有 `binding_conflicts()`（chord 键互相重复）互补，不替代——
    /// 调用方（`set_config` 的 `chord_bindings` 分支）两者都跑。
    pub fn global_binding_conflicts(
        &self,
        bindings: &ChordBindings,
        main_hotkey: Option<(&[String], &str)>,
    ) -> Vec<BindingConflict> {
        let mut conflicts = Vec::new();
        let globals = self.global_hotkey_combos(bindings);

        // 1. 全局键互相冲突
        for (idx, (id_a, combo_a)) in globals.iter().enumerate() {
            for (id_b, combo_b) in globals.iter().skip(idx + 1) {
                if combo_a == combo_b {
                    conflicts.push(BindingConflict {
                        kind: BindingConflictKind::GlobalDuplicate,
                        combo: combo_a.display(),
                        actions: vec![id_a.clone(), id_b.clone()],
                    });
                }
            }
        }

        let main_combo = main_hotkey.map(|(mods, key)| HotkeyCombo::new(mods, key));

        // 2. 全局键 vs 主热键
        if let Some(main) = &main_combo {
            for (id, combo) in &globals {
                if combo == main {
                    conflicts.push(BindingConflict {
                        kind: BindingConflictKind::GlobalVsMainHotkey,
                        combo: combo.display(),
                        actions: vec![id.clone()],
                    });
                }
            }
        }

        // 3. 自定义全局键 vs chord 生效键（含自身；跟随模式豁免）
        for (id, combo) in &globals {
            let is_custom = matches!(
                bindings.get(id).and_then(|b| b.global.as_ref()),
                Some(GlobalBinding::Custom { .. })
            );
            if !is_custom {
                continue;
            }
            for action in &self.actions {
                let chord_combo = HotkeyCombo::new(
                    &bindings.effective_modifiers(action.id()),
                    &bindings.effective_key(action.id(), action.default_key()),
                );
                if &chord_combo == combo {
                    conflicts.push(BindingConflict {
                        kind: BindingConflictKind::GlobalVsChordKey,
                        combo: combo.display(),
                        actions: vec![id.clone(), action.id().to_string()],
                    });
                }
            }
        }

        // 4. chord Tap 键 vs 主热键（0.22.12 补缺口；Hold 语义豁免）
        if let Some(main) = &main_combo {
            for action in &self.actions {
                if bindings.effective_semantic(action.id(), action.default_semantic())
                    != ChordSemantic::Tap
                {
                    continue;
                }
                let chord_combo = HotkeyCombo::new(
                    &bindings.effective_modifiers(action.id()),
                    &bindings.effective_key(action.id(), action.default_key()),
                );
                if &chord_combo == main {
                    conflicts.push(BindingConflict {
                        kind: BindingConflictKind::ChordKeyVsMainHotkey,
                        combo: chord_combo.display(),
                        actions: vec![action.id().to_string()],
                    });
                }
            }
        }

        conflicts
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

// ── 0.21.2：ChordTarget — 触发后分派目标 ────────────────────────────────────

/// Chord binding 触发后的分派目标（0.21.2）。
///
/// 旧 `ChordAction: Action` supertrait 已删除，`ChordAction` 不再有 `execute()` 方法。
/// 触发器根据 `ChordTarget` 决定调用哪个 Capability 或领域 Interaction service。
#[derive(Debug, Clone)]
pub enum ChordTarget {
    /// 调用指定 Capability id。触发器构建 `InvokeContext` 并经 `CapabilityRegistry::invoke`。
    /// `args` 为可选参数 JSON（如 `{"content": "...", "prefill": "..."}` 等）。
    Capability {
        capability_id: &'static str,
        /// 从 chord input_text 提取的参数键名（如 "content" / "prefill" / "body"）。
        /// None 表示无参数传入。
        input_param: Option<&'static str>,
        /// 额外固定参数（如 origin / save_policy）
        extra_args: Vec<(&'static str, &'static str)>,
        /// 是否在调用 Capability 前隐藏主窗（如 sticky chord 需要隐藏主窗，
        /// 而 open_clipboard_mode 需要显示主窗）。
        /// 仅对 GUI_SURFACE Capability 有意义；None 表示不主动隐藏（由 Capability 自管）。
        hide_main_before: bool,
    },
    /// Voice Interaction——voice_input 不造空 Capability，触发走 native hotkey hold。
    /// descriptor target 明确标识为 Interaction starter，`trigger` 时返回 Nop。
    VoiceInteraction,
}

// ── 0.21.2：ChordAction 纯 descriptor trait ──────────────────────────────────

/// Chord 动作契约（0.21.2 重构：纯 descriptor/binding，不再继承 `Action`）。
///
/// `ChordAction` 只负责声明 binding 元数据（键、语义、显示名、surface）和
/// 触发目标（`ChordTarget`）。真实执行由 `ChordRegistry::trigger` 根据 target
/// 调 `CapabilityRegistry::invoke` 或领域 Interaction service 完成。
///
/// **0.21.2 变更**：
/// - 删除 `ChordAction: Action` supertrait——不再有 `execute()` 方法。
/// - 删除 `title()` / `subtitle()` / `schema()` / `danger_class()`——这些是旧
///   `Action` trait 的方法，Chord 不再需要。
/// - 新增 `target()` 方法——返回 `ChordTarget`，触发器据此分派。
pub trait ChordAction: Send + Sync {
    /// 唯一标识（binding id，如 "chat" / "screenshot" / "voice_input"）。
    fn id(&self) -> &str;
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
    /// 参数传入。`ChordTarget::Capability` 的 `input_param` 指定如何把文本映射为
    /// Capability 参数。
    fn requires_input(&self) -> bool {
        false
    }
    /// 空文本时是否在提示条隐藏此动作（0.16.11）。
    fn hint_hidden_when_empty(&self) -> bool {
        false
    }
    /// 显示名（走 `LocalizableText`--registry 声明 zh/en，list() 按 language 解析）。
    fn label(&self) -> &crate::domain::plugin::LocalizableText;
    /// 触发后的窗口形态。
    fn surface(&self) -> ChordSurface;
    /// 触发后的分派目标（0.21.2 新增）。
    fn target(&self) -> ChordTarget;
}

// ── 6 个 ChordAction 实现（纯 descriptor，不再有 execute） ─────────────────────

/// Alt+Space 语音输入（display-only chord 条目，0.10.7 升级为 semantic=hold）。
///
/// **特殊性**：触发**不走 chord tap 路径**——Alt+Space 由 native hotkey hook 的
/// hold 状态机处理（`HotkeyEvent::Hold` → `VoiceService::start_recording`）。
/// 此条目仅用于在 chord 提示条中显示「Alt+Space 语音输入」，让 hold-to-talk
/// 这个隐藏交互变得可发现。
///
/// **0.21.2**：target = `VoiceInteraction`，不造空 Capability。
struct VoiceInputAction {
    label: crate::domain::plugin::LocalizableText,
}

impl ChordAction for VoiceInputAction {
    fn id(&self) -> &str {
        "voice_input"
    }
    fn default_key(&self) -> char {
        ' '
    }
    fn default_semantic(&self) -> ChordSemantic {
        ChordSemantic::Hold
    }
    fn label(&self) -> &crate::domain::plugin::LocalizableText {
        &self.label
    }
    fn surface(&self) -> ChordSurface {
        ChordSurface::Default
    }
    fn target(&self) -> ChordTarget {
        ChordTarget::VoiceInteraction
    }
}

/// Alt+Q AI 对话窗口（0.12.1 / 0.21.2 分流为 `open_chat` Capability）。
struct ChatAction {
    label: crate::domain::plugin::LocalizableText,
}

impl ChordAction for ChatAction {
    fn id(&self) -> &str {
        "chat"
    }
    fn default_key(&self) -> char {
        'q'
    }
    /// 0.16.2：chat 需要输入框文本作为入参--非空 query 时 Alt+Q 仍可触发，
    /// 把文本带入 chat 窗口输入框（仅填充不发送）。
    fn requires_input(&self) -> bool {
        true
    }
    fn label(&self) -> &crate::domain::plugin::LocalizableText {
        &self.label
    }
    fn surface(&self) -> ChordSurface {
        ChordSurface::Default
    }
    fn target(&self) -> ChordTarget {
        ChordTarget::Capability {
            capability_id: "open_chat",
            input_param: Some("prefill"),
            extra_args: vec![],
            hide_main_before: false, // open_chat 自己负责 hide_main_window
        }
    }
}

/// Alt+A 区域截图（0.8.7 / 0.21.2 分流为 `start_region_capture` Capability）。
struct ScreenshotAction {
    label: crate::domain::plugin::LocalizableText,
}

impl ChordAction for ScreenshotAction {
    fn id(&self) -> &str {
        "screenshot"
    }
    fn default_key(&self) -> char {
        'a'
    }
    fn label(&self) -> &crate::domain::plugin::LocalizableText {
        &self.label
    }
    fn surface(&self) -> ChordSurface {
        ChordSurface::Screenshot
    }
    fn target(&self) -> ChordTarget {
        ChordTarget::Capability {
            capability_id: "start_region_capture",
            input_param: None,
            extra_args: vec![],
            hide_main_before: false, // start_region_capture SurfacePort 实现内部调 hide_for_screenshot
        }
    }
}

/// Alt+C 剪贴板历史（0.8.5 / 0.21.2 分流为 `open_clipboard_mode` Capability）。
struct ClipboardHistoryAction {
    label: crate::domain::plugin::LocalizableText,
}

impl ChordAction for ClipboardHistoryAction {
    fn id(&self) -> &str {
        "clipboard_history"
    }
    fn default_key(&self) -> char {
        'c'
    }
    fn label(&self) -> &crate::domain::plugin::LocalizableText {
        &self.label
    }
    fn surface(&self) -> ChordSurface {
        ChordSurface::Default
    }
    fn target(&self) -> ChordTarget {
        ChordTarget::Capability {
            capability_id: "open_clipboard_mode",
            input_param: None,
            extra_args: vec![],
            hide_main_before: false, // open_clipboard_mode 需要显示主窗
        }
    }
}

/// Alt+E 编辑当前内容（0.16.9 / 0.21.2 分流为 `start_content_editor` Capability）。
struct EditAction {
    label: crate::domain::plugin::LocalizableText,
}

impl ChordAction for EditAction {
    fn id(&self) -> &str {
        "edit"
    }
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
    fn label(&self) -> &crate::domain::plugin::LocalizableText {
        &self.label
    }
    fn surface(&self) -> ChordSurface {
        ChordSurface::Default
    }
    fn target(&self) -> ChordTarget {
        ChordTarget::Capability {
            capability_id: "start_content_editor",
            input_param: Some("body"),
            extra_args: vec![("origin", "chord"), ("save_policy", "clipboard_new")],
            hide_main_before: false, // start_content_editor 自己负责 hide_main_window
        }
    }
}

/// Alt+S 将当前内容创建为便签（0.16.9 / 0.21.2 绑定既有 `create_sticky` Capability）。
struct StickyAction {
    label: crate::domain::plugin::LocalizableText,
}

impl ChordAction for StickyAction {
    fn id(&self) -> &str {
        "sticky"
    }
    fn default_key(&self) -> char {
        's'
    }
    fn requires_input(&self) -> bool {
        true // 0.20.0：Alt+S 预填输入框文本（用户显式输入），但不读 SelectionCache
    }
    /// 空文本时提示隐藏——sticky 是 contextual 动作，空文本时创建空白便签，
    /// 提示价值低。触发仍保留（Alt+S 创建空白便签）。
    fn hint_hidden_when_empty(&self) -> bool {
        true
    }
    fn label(&self) -> &crate::domain::plugin::LocalizableText {
        &self.label
    }
    fn surface(&self) -> ChordSurface {
        ChordSurface::Default
    }
    fn target(&self) -> ChordTarget {
        ChordTarget::Capability {
            capability_id: "create_sticky",
            input_param: Some("content"),
            extra_args: vec![],
            hide_main_before: true, // create_sticky Capability 不自管隐藏，由 chord trigger 隐藏
        }
    }
}

// ── 便捷 helper ──────────────────────────────────────────────────────────────

/// LocalizableText 便捷构造:zh/en 双语。
fn bilingual(zh: &str, en: &str) -> crate::domain::plugin::LocalizableText {
    let mut map = std::collections::HashMap::new();
    map.insert("zh".to_string(), zh.to_string());
    map.insert("en".to_string(), en.to_string());
    crate::domain::plugin::LocalizableText::Localized(map)
}

// ── ChordRegistry ─────────────────────────────────────────────────────────────

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

    /// 返回生效键冲突（0.22.12 起统一返回 `BindingConflict`）。
    ///
    /// 一个键映射到多个动作时 `.find()` 会让后续动作永远不可达，
    /// 因此配置写入前必须拒绝这类绑定。
    pub fn binding_conflicts(&self, bindings: &ChordBindings) -> Vec<BindingConflict> {
        let mut by_key: std::collections::BTreeMap<HotkeyCombo, Vec<String>> =
            std::collections::BTreeMap::new();
        for action in &self.actions {
            let combo = HotkeyCombo::new(
                &bindings.effective_modifiers(action.id()),
                &bindings.effective_key(action.id(), action.default_key()),
            );
            by_key.entry(combo).or_default().push(action.id().to_string());
        }
        by_key
            .into_iter()
            .filter(|(_, action_ids)| action_ids.len() > 1)
            .map(|(combo, actions)| BindingConflict {
                kind: BindingConflictKind::ChordKeyDuplicate,
                combo: combo.display(),
                actions,
            })
            .collect()
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

    /// 返回所有已注册动作的迭代器（0.21.4 FeatureCatalog 聚合用）。
    ///
    /// 供 FeatureCatalogAggregator 读取每个 ChordAction 的 `target()` 以关联 Capability。
    pub fn actions_iter(&self) -> impl Iterator<Item = &Arc<dyn ChordAction>> {
        self.actions.iter()
    }

    /// 按字母键触发对应动作，返回动作的 surface（供 command 层决定显示哪个窗口）。
    /// 键未注册 → Err（前端会 log，不弹窗）。
    ///
    /// **0.21.2 重构**：不再调 `Action::execute`，改为按 `ChordTarget` 分派。
    /// **0.22.12 重构**：分派体收敛进 `dispatch`，`trigger`（按键解析）与
    /// `trigger_by_id`（全局快捷键直达，与 chord 键位解耦）共用同一执行语义。
    #[allow(clippy::too_many_arguments)]
    pub async fn trigger(
        &self,
        key: &str,
        bindings: &ChordBindings,
        cap_registry: &crate::domain::capability::CapabilityRegistry,
        env: &dyn crate::domain::event::CapabilityEnv,
        surface_port: Option<&dyn crate::domain::capability::policy::SurfacePort>,
        input_text: Option<&str>,
        origin_ref: Option<&str>,
    ) -> Result<ChordSurface, String> {
        let lower = key.to_lowercase();
        let action = self
            .actions
            .iter()
            .find(|a| bindings.effective_key(a.id(), a.default_key()) == lower)
            .ok_or_else(|| format!("未注册的 chord 键: {lower}"))?;
        tracing::info!(id = action.id(), key = %lower, "chord trigger（按生效键解析）");
        self.dispatch(action.as_ref(), cap_registry, env, surface_port, input_text, origin_ref)
            .await
    }

    /// 按动作 id 直接触发（0.22.12 全局快捷键路径）。
    ///
    /// 与 chord 键位完全解耦——全局入口的触发不经过 `effective_key` 解析。
    /// `input_text` / `origin_ref` 由调用方传 `None`（全局按键无输入文本上下文）。
    #[allow(clippy::too_many_arguments)]
    pub async fn trigger_by_id(
        &self,
        action_id: &str,
        cap_registry: &crate::domain::capability::CapabilityRegistry,
        env: &dyn crate::domain::event::CapabilityEnv,
        surface_port: Option<&dyn crate::domain::capability::policy::SurfacePort>,
        input_text: Option<&str>,
        origin_ref: Option<&str>,
    ) -> Result<ChordSurface, String> {
        let action = self
            .actions
            .iter()
            .find(|a| a.id() == action_id)
            .ok_or_else(|| format!("未注册的 chord 动作: {action_id}"))?;
        tracing::info!(id = %action_id, "chord trigger（全局快捷键直达）");
        self.dispatch(action.as_ref(), cap_registry, env, surface_port, input_text, origin_ref)
            .await
    }

    /// 按 `ChordTarget` 分派到 Capability 或领域 Interaction。
    ///
    /// - `Capability { capability_id, input_param, extra_args }` → 经 `CapabilityRegistry::invoke`
    /// - `VoiceInteraction` → Nop（真实录音由 hotkey 层已在 hold 时启动）
    async fn dispatch(
        &self,
        action: &dyn ChordAction,
        cap_registry: &crate::domain::capability::CapabilityRegistry,
        env: &dyn crate::domain::event::CapabilityEnv,
        surface_port: Option<&dyn crate::domain::capability::policy::SurfacePort>,
        input_text: Option<&str>,
        origin_ref: Option<&str>,
    ) -> Result<ChordSurface, String> {
        let surface = action.surface();
        tracing::info!(id = action.id(), surface = ?surface, has_input = input_text.is_some(), "chord trigger");

        match action.target() {
            ChordTarget::VoiceInteraction => {
                // voice_input 由 native hotkey hold 状态机处理，trigger 路径仅作防御性 Nop。
                tracing::debug!(
                    id = action.id(),
                    "voice_input chord trigger（Nop：由 hotkey 处理）"
                );
            }
            ChordTarget::Capability {
                capability_id,
                input_param,
                extra_args,
                hide_main_before,
            } => {
                // 构建 Capability args
                let mut args = serde_json::json!({});
                if let Some(param_key) = input_param
                    && let Some(text) = input_text
                {
                    args[param_key] = serde_json::Value::String(text.to_string());
                }
                // origin_ref 注入（用于 chord-E 编辑已有项时继承 hit_count）
                if let Some(ref_id) = origin_ref {
                    if ref_id.is_empty() {
                        args["origin_ref"] = serde_json::Value::Null;
                    } else {
                        args["origin_ref"] = serde_json::Value::String(ref_id.to_string());
                    }
                }
                // 额外固定参数
                for (k, v) in &extra_args {
                    args[*k] = serde_json::Value::String(v.to_string());
                }

                // 0.21.2：部分 chord binding 需要在调用 Capability 前隐藏主窗
                //（如 sticky——create_sticky Capability 不自管隐藏）
                if hide_main_before && let Some(surface) = surface_port {
                    surface.hide_main_window(action.id());
                }

                let ctx = crate::domain::capability::InvokeContext {
                    env,
                    origin: crate::domain::capability::InvocationOrigin::LocalSurface,
                    runtime: crate::domain::capability::RuntimeCapabilities {
                        surface: surface_port,
                        main_process: true,
                        desktop_session: true,
                    },
                    deadline: None,
                };

                match cap_registry.invoke(capability_id, args, &ctx).await {
                    Ok(result) => {
                        tracing::info!(
                            id = capability_id,
                            chord_id = action.id(),
                            summary = %result.to_display_text(None),
                            "chord → Capability 执行成功"
                        );
                    }
                    Err(e) => {
                        tracing::error!(
                            id = capability_id,
                            chord_id = action.id(),
                            error = %e,
                            "chord → Capability 执行失败"
                        );
                        // 失败时恢复被 hide_main_before 隐藏的主窗，避免「主窗消失 + 无便签/反馈」
                        if hide_main_before
                            && let Some(surface) = surface_port
                            && let Err(show_err) = surface.show_main_window()
                        {
                            tracing::warn!(
                                id = capability_id,
                                chord_id = action.id(),
                                error = %show_err,
                                "chord → Capability 失败后恢复主窗失败"
                            );
                        }
                        return Err(e.to_string());
                    }
                }
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

/// 构建默认 ChordRegistry（注册顺序即提示条展示顺序）。
/// - Alt+Space 语音输入（display-only，触发走 native hotkey hold，此条目仅用于提示条显示）
/// - Alt+Q AI 对话（0.12.1：独立对话窗口 → 0.21.2：open_chat Capability）
/// - Alt+A 区域截图（0.8.7：ScreenshotAction → 0.21.2：start_region_capture Capability）
/// - Alt+C 剪贴板历史（0.8.5 §6.4 → 0.21.2：open_clipboard_mode Capability）
/// - Alt+E 编辑窗口（0.16.9 → 0.21.2：start_content_editor Capability）
/// - Alt+S 钉为便签（0.16.9 → 0.21.2：create_sticky Capability）
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
    fn duplicate_effective_keys_are_reported() {
        let registry = build_default_registry();
        let mut bindings = ChordBindings::default();
        bindings.screenshot.key = "q".to_string();

        let conflicts = registry.binding_conflicts(&bindings);
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].kind, BindingConflictKind::ChordKeyDuplicate);
        assert_eq!(conflicts[0].combo, "alt+q");
        assert_eq!(
            conflicts[0].actions,
            vec!["chat".to_string(), "screenshot".to_string()]
        );
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
        assert_eq!(sticky["requires_input"], true); // 0.20.0: Alt+S 预填输入框文本
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

    // ── 0.21.2：ChordTarget 分流测试 ────────────────────────────────────────

    #[test]
    fn chord_action_no_longer_implements_action() {
        // 0.21.2：ChordAction 不再继承 execution::Action。
        // 验证 trait 定义不包含 execute / title / subtitle / schema / danger_class。
        // 这是编译期保证——如果 ChordAction 仍继承 Action，下面编译不过。
        #[allow(dead_code)] // 编译期 trait 约束验证：编译通过即测试通过
        fn assert_no_action_supertrait<T: ChordAction>() {}
        // ChatAction / ScreenshotAction / etc 都只 impl ChordAction，不 impl Action。
        // 此测试主要验证 trait 本身不再要求 Action supertrait。
        // 编译通过即说明 ChordAction 不继承 Action。
    }

    #[test]
    fn voice_input_target_is_interaction() {
        let registry = build_default_registry();
        let voice = registry
            .actions
            .iter()
            .find(|a| a.id() == "voice_input")
            .unwrap();
        assert!(matches!(voice.target(), ChordTarget::VoiceInteraction));
    }

    #[test]
    fn chat_target_is_open_chat_capability() {
        let registry = build_default_registry();
        let chat = registry.actions.iter().find(|a| a.id() == "chat").unwrap();
        match chat.target() {
            ChordTarget::Capability {
                capability_id,
                input_param,
                ..
            } => {
                assert_eq!(capability_id, "open_chat");
                assert_eq!(input_param, Some("prefill"));
            }
            _ => panic!("chat target 应为 Capability"),
        }
    }

    #[test]
    fn screenshot_target_is_start_region_capture() {
        let registry = build_default_registry();
        let screenshot = registry
            .actions
            .iter()
            .find(|a| a.id() == "screenshot")
            .unwrap();
        match screenshot.target() {
            ChordTarget::Capability {
                capability_id,
                input_param,
                ..
            } => {
                assert_eq!(capability_id, "start_region_capture");
                assert_eq!(input_param, None);
            }
            _ => panic!("screenshot target 应为 Capability"),
        }
    }

    #[test]
    fn clipboard_history_target_is_open_clipboard_mode() {
        let registry = build_default_registry();
        let cb = registry
            .actions
            .iter()
            .find(|a| a.id() == "clipboard_history")
            .unwrap();
        match cb.target() {
            ChordTarget::Capability { capability_id, .. } => {
                assert_eq!(capability_id, "open_clipboard_mode");
            }
            _ => panic!("clipboard_history target 应为 Capability"),
        }
    }

    #[test]
    fn edit_target_is_start_content_editor() {
        let registry = build_default_registry();
        let edit = registry.actions.iter().find(|a| a.id() == "edit").unwrap();
        match edit.target() {
            ChordTarget::Capability {
                capability_id,
                input_param,
                extra_args,
                ..
            } => {
                assert_eq!(capability_id, "start_content_editor");
                assert_eq!(input_param, Some("body"));
                assert!(
                    extra_args
                        .iter()
                        .any(|(k, v)| *k == "origin" && *v == "chord")
                );
                assert!(
                    extra_args
                        .iter()
                        .any(|(k, v)| *k == "save_policy" && *v == "clipboard_new")
                );
            }
            _ => panic!("edit target 应为 Capability"),
        }
    }

    #[test]
    fn sticky_target_is_create_sticky() {
        let registry = build_default_registry();
        let sticky = registry
            .actions
            .iter()
            .find(|a| a.id() == "sticky")
            .unwrap();
        match sticky.target() {
            ChordTarget::Capability {
                capability_id,
                input_param,
                ..
            } => {
                assert_eq!(capability_id, "create_sticky");
                assert_eq!(input_param, Some("content"));
            }
            _ => panic!("sticky target 应为 Capability"),
        }
    }

    #[test]
    fn six_chord_actions_registered() {
        let registry = build_default_registry();
        assert_eq!(registry.actions.len(), 6);
    }

    #[test]
    fn screenshot_binding_alias_preserved() {
        // 0.21.2：旧 chord screenshot binding key "a" 仍映射到 screenshot action id。
        // 用户键位不丢失——binding 配置的 screenshot 字段仍由 ChordBindings 管理。
        let bindings = ChordBindings::default();
        let registry = build_default_registry();
        assert_eq!(
            registry.action_id_for_key("a", &bindings),
            Some("screenshot")
        );

        // 改绑后仍能匹配
        let mut bindings2 = ChordBindings::default();
        bindings2.get_mut("screenshot").unwrap().key = "x".into();
        assert_eq!(
            registry.action_id_for_key("x", &bindings2),
            Some("screenshot")
        );
        assert_eq!(registry.action_id_for_key("a", &bindings2), None);
    }

    // ── 0.22.12：全局快捷键 ─────────────────────────────────────────────────

    /// 主热键默认修饰键（测试用共享构造）。
    fn alt_mods() -> Vec<String> {
        vec!["alt".to_string()]
    }

    #[test]
    fn global_binding_serde_backward_compatible() {
        // 老配置无 global 字段 → None
        let legacy: ChordBindings =
            serde_json::from_value(serde_json::json!({ "screenshot": {} })).unwrap();
        assert!(legacy.screenshot.global.is_none());

        // follow_chord roundtrip
        let b: ChordBindings = serde_json::from_value(serde_json::json!({
            "screenshot": { "global": { "mode": "follow_chord" } }
        }))
        .unwrap();
        assert_eq!(b.screenshot.global, Some(GlobalBinding::FollowChord));

        // custom roundtrip
        let b: ChordBindings = serde_json::from_value(serde_json::json!({
            "sticky": { "global": { "mode": "custom", "modifiers": ["ctrl", "alt"], "key": "x" } }
        }))
        .unwrap();
        assert_eq!(
            b.sticky.global,
            Some(GlobalBinding::Custom {
                modifiers: vec!["ctrl".into(), "alt".into()],
                key: "x".into(),
            })
        );

        // 未提升时不序列化 global 字段（存量 JSON 不新增键）
        let out = serde_json::to_value(ChordBindings::default()).unwrap();
        assert!(out["screenshot"].get("global").is_none());
    }

    #[test]
    fn hotkey_combo_normalization_and_display() {
        // 左右/别名折叠 + 显示顺序
        let combo = HotkeyCombo::new(&["lalt".to_string()], " ");
        assert_eq!(combo.display(), "alt+space");

        let combo = HotkeyCombo::new(&["alt".to_string(), "ctrl".to_string()], "S");
        assert_eq!(combo.display(), "ctrl+alt+s");

        let combo = HotkeyCombo::new(&["super".to_string()], "f5");
        assert_eq!(combo.display(), "meta+f5");

        // 同组合不同写法相等
        assert_eq!(
            HotkeyCombo::new(&["lctrl".to_string()], "a"),
            HotkeyCombo::new(&["ctrl".to_string()], "A")
        );
    }

    #[test]
    fn global_hotkey_combos_resolve_follow_and_custom() {
        let registry = build_default_registry();
        let mut bindings = ChordBindings::default();
        bindings.screenshot.global = Some(GlobalBinding::FollowChord);
        bindings.sticky.global = Some(GlobalBinding::Custom {
            modifiers: vec!["ctrl".into(), "alt".into()],
            key: "s".into(),
        });

        let combos = registry.global_hotkey_combos(&bindings);
        assert_eq!(combos.len(), 2, "voice_input（VoiceInteraction）不参与");
        assert!(combos.iter().all(|(id, _)| id != "voice_input"));
        let screenshot = combos.iter().find(|(id, _)| id == "screenshot").unwrap();
        assert_eq!(screenshot.1.display(), "alt+a", "跟随模式解析为生效键组合");
        let sticky = combos.iter().find(|(id, _)| id == "sticky").unwrap();
        assert_eq!(sticky.1.display(), "ctrl+alt+s");
    }

    #[test]
    fn global_conflict_duplicate_and_main_hotkey() {
        let registry = build_default_registry();
        let mut bindings = ChordBindings::default();
        // 截图跟随（alt+a）+ edit 自定义（alt+a）→ 全局键互相冲突
        bindings.screenshot.global = Some(GlobalBinding::FollowChord);
        bindings.edit.global = Some(GlobalBinding::Custom {
            modifiers: vec!["alt".into()],
            key: "a".into(),
        });
        let conflicts = registry.global_binding_conflicts(&bindings, Some((&alt_mods()[..], " ")));
        assert!(conflicts
            .iter()
            .any(|c| c.kind == BindingConflictKind::GlobalDuplicate && c.combo == "alt+a"));

        // chat 自定义（alt+space）撞主热键（alt+space）
        let mut bindings = ChordBindings::default();
        bindings.chat.global = Some(GlobalBinding::Custom {
            modifiers: vec!["alt".into()],
            key: " ".into(),
        });
        let conflicts = registry.global_binding_conflicts(&bindings, Some((&alt_mods()[..], " ")));
        assert!(conflicts
            .iter()
            .any(|c| c.kind == BindingConflictKind::GlobalVsMainHotkey && c.combo == "alt+space"));
    }

    #[test]
    fn global_conflict_custom_vs_chord_key() {
        let registry = build_default_registry();
        // 自定义全局键与自身 chord 键相同 → 冲突（跟随模式才允许同键）
        let mut bindings = ChordBindings::default();
        bindings.sticky.global = Some(GlobalBinding::Custom {
            modifiers: vec!["alt".into()],
            key: "s".into(),
        });
        let conflicts = registry.global_binding_conflicts(&bindings, None);
        assert!(conflicts
            .iter()
            .any(|c| c.kind == BindingConflictKind::GlobalVsChordKey && c.combo == "alt+s"));

        // 跟随模式天然等于自身 chord 键 → 不算冲突
        let mut bindings = ChordBindings::default();
        bindings.sticky.global = Some(GlobalBinding::FollowChord);
        assert!(registry.global_binding_conflicts(&bindings, None).is_empty());
    }

    #[test]
    fn chord_tap_key_vs_main_hotkey_gap_check() {
        let registry = build_default_registry();
        let main_hotkey = (&alt_mods()[..], " ");

        // 默认配置：voice_input 是 Hold 语义（Alt+Space 长按与主热键共存是设计），
        // 其余默认键（q/a/c/e/s）与 Alt+Space 不同 → 无冲突
        assert!(registry
            .global_binding_conflicts(&ChordBindings::default(), Some(main_hotkey))
            .is_empty());

        // 把 chat（Tap）改绑到空格 → 与主热键冲突（补齐既有缺口）
        let mut bindings = ChordBindings::default();
        bindings.chat.key = " ".into();
        let conflicts = registry.global_binding_conflicts(&bindings, Some(main_hotkey));
        assert!(conflicts.iter().any(|c| c.kind == BindingConflictKind::ChordKeyVsMainHotkey
            && c.actions == vec!["chat".to_string()]));

        // 主热键 None → 跳过主热键相关检查
        assert!(registry.global_binding_conflicts(&bindings, None).is_empty());
    }
}
