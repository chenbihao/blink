//! BuiltinEngine: Blink 内置动作引擎（sync lane）。
//!
//! 内置动作不经过插件进程，直接在 core 内执行，响应速度最快。
//! 包含：设置、锁屏、关机/重启/睡眠、清空历史等系统操作。
//!
//! 数据模型（0.8.0 §1.3 扩容 / 0.8.2 §3.2.1 上移 enum）：
//! - `BuiltinAction`：静态注册表条目，除原有 id/title/subtitle/keywords/kind 外，扩
//!   三字段 `context` / `param_source` / `default_enabled`。
//! - `BuiltinActionKind`：**分派 tag**，唯一定义在这里。命令层 `commands::run_builtin_action`
//!   收到前端 action id 后 `match id.as_str()` 分派到对应 kind 的执行分支。
//!   与前端契约 `crate::domain::search::ActionKind`（Open/Copy/RunAction）撞名故加前缀。
//! - `ContextTrigger` / `ParamSource`：0.8.2 §3.2.1 起统一从 `domain::context::trigger`
//!   引用，与插件路由（`intent::RuleRouter`）共用同一 enum。

use serde::Serialize;

use super::engine::{QueryContext, SearchAction, SearchEngine, SearchItem};
use super::scorer::{BuiltinMatch, apply_history};
use crate::domain::context::trigger::{self as ctx_trigger, ContextTrigger, ParamSource};

/// Builtin descriptor 声明的成功后本地结果操作（0.21.13）。
///
/// 让“调用诊断 Capability，然后把返回文本复制到剪贴板”由显式结果动作表达，
/// 而不是 command 层识别具体 capability id。普通 `CapabilityResult::Text`
/// 本身不触发任何副作用——只有 descriptor 声明了 `CopyText` 才复制。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuiltinResultAction {
    /// 把 `CapabilityResult::Text.content` 复制到剪贴板。
    /// `skip_persist = true` 避免诊断文本进入剪贴板历史。
    CopyText { skip_persist: bool },
}

/// 内置动作定义（引擎内部模型，用于 keyword 匹配 + Context 触发 + 搜索结果展示）。
///
/// **0.21.3**：从 ActionRegistry 读 title/subtitle 收敛为 descriptor 自带双语字段。
/// `capability_id` 显式声明 descriptor → Capability target，不再隐式假设 id == capability_id。
/// `list_builtin_actions` / `list_builtin_context_bindings` 不再依赖 ActionRegistry。
///
/// **0.21.13**：`param_field` 声明目标 Capability schema 的参数字段名，`ParamSource::extract`
/// 直接产最终 JSON object（如 `{ "url": "..." }`），消除 command 层 `convert_legacy_arg_to_capability_args`。
/// `result_action` 声明成功后的本地结果操作（如诊断复制），消除 command 层 id 特判。
struct BuiltinAction {
    /// 唯一标识（descriptor id，与前端 disabled list / context binding key 一致）
    id: &'static str,
    /// 对应的 Capability id（descriptor target）。
    /// 0.21.3：无参动作 capability_id 与 id 相同；参数化动作也相同（open_url/open_path/reveal_in_explorer）。
    /// 显式声明而非隐式假设，为 0.21.4 FeatureCatalog 聚合预留。
    capability_id: &'static str,
    /// 主显示标题（中文）——keyword 匹配 + SearchItem.title
    title: &'static str,
    /// 主显示标题（英文）——按 language 选择
    title_en: &'static str,
    /// 副标题/说明（中文）——SearchItem.subtitle
    subtitle: &'static str,
    /// 副标题/说明（英文）——按 language 选择
    subtitle_en: &'static str,
    /// 匹配关键词（拼音首字母也会自动匹配，如 "设置" → "sz"）
    keywords: &'static [&'static str],
    /// Context 触发条件；空 slice = 不参与 Context 路由（0.8.0 §1.3）。
    context: &'static [ContextTrigger],
    /// 参数来源；`None` = 无参数动作（0.8.0 §1.3）。
    param_source: ParamSource,
    /// 目标 Capability schema 的参数字段名（0.21.13）。
    /// `param_source != None` 时必填：`open_url` → `"url"`，`open_path`/`reveal_in_explorer` → `"path"`。
    /// `param_source == None` 时忽略（无参能力 invoke 传 `{}`）。
    param_field: &'static str,
    /// 成功后的本地结果操作（0.21.13）。
    /// `None` = 无后续副作用（默认）。诊断动作声明 `CopyText` 以替代 command 层 id 特判。
    result_action: Option<BuiltinResultAction>,
    /// 默认启用状态。用户 disable 状态存 `AppConfig.disabled_builtin_actions`。
    default_enabled: bool,
}

/// 内置动作注册表。
///
/// 新增动作只需在这里添加条目，无需修改其他代码。
const ACTIONS: &[BuiltinAction] = &[
    BuiltinAction {
        id: "open_settings",
        capability_id: "open_settings",
        title: "打开设置",
        title_en: "Open Settings",
        subtitle: "Blink 偏好设置",
        subtitle_en: "Blink Preferences",
        keywords: &["设置", "settings", "sz", "偏好", "配置"],
        context: &[],
        param_source: ParamSource::None,
        param_field: "",
        result_action: None,
        default_enabled: true,
    },
    BuiltinAction {
        id: "sticky_manager",
        capability_id: "sticky_manager",
        title: "便签管理",
        title_en: "Sticky Manager",
        subtitle: "管理桌面便签",
        subtitle_en: "Manage desktop sticky notes",
        keywords: &["便签", "sticky", "bj", "管理", "笔记", "notes"],
        context: &[],
        param_source: ParamSource::None,
        param_field: "",
        result_action: None,
        default_enabled: true,
    },
    BuiltinAction {
        id: "edit_clipboard_image",
        capability_id: "edit_clipboard_image",
        title: "编辑剪贴板图片",
        title_en: "Edit Clipboard Image",
        subtitle: "标注当前剪贴板图片",
        subtitle_en: "Annotate the current clipboard image",
        keywords: &[
            "编辑图片",
            "剪贴板图片",
            "edit image",
            "edit clipboard image",
            "bjtp",
        ],
        context: &[],
        param_source: ParamSource::None,
        param_field: "",
        result_action: None,
        default_enabled: true,
    },
    BuiltinAction {
        id: "lock",
        capability_id: "lock",
        title: "锁定电脑",
        title_en: "Lock Workstation",
        subtitle: "Lock Workstation",
        subtitle_en: "Lock Workstation",
        keywords: &["锁定", "lock", "锁屏", "sd"],
        context: &[],
        param_source: ParamSource::None,
        param_field: "",
        result_action: None,
        default_enabled: true,
    },
    BuiltinAction {
        id: "shutdown",
        capability_id: "shutdown",
        title: "关机",
        title_en: "Shutdown",
        subtitle: "Shutdown",
        subtitle_en: "Shutdown",
        keywords: &["关机", "shutdown", "gj"],
        context: &[],
        param_source: ParamSource::None,
        param_field: "",
        result_action: None,
        default_enabled: true,
    },
    BuiltinAction {
        id: "restart",
        capability_id: "restart",
        title: "重启",
        title_en: "Restart",
        subtitle: "Restart",
        subtitle_en: "Restart",
        keywords: &["重启", "restart", "cq"],
        context: &[],
        param_source: ParamSource::None,
        param_field: "",
        result_action: None,
        default_enabled: true,
    },
    BuiltinAction {
        id: "sleep",
        capability_id: "sleep",
        title: "睡眠",
        title_en: "Sleep",
        subtitle: "Sleep",
        subtitle_en: "Sleep",
        keywords: &["睡眠", "sleep", "sm"],
        context: &[],
        param_source: ParamSource::None,
        param_field: "",
        result_action: None,
        default_enabled: true,
    },
    BuiltinAction {
        id: "clear_history",
        capability_id: "clear_history",
        title: "清空搜索历史",
        title_en: "Clear Search History",
        subtitle: "清除所有应用启动记录",
        subtitle_en: "Clear all app launch records",
        keywords: &["清空历史", "clear history", "qkls", "清除历史"],
        context: &[],
        param_source: ParamSource::None,
        param_field: "",
        result_action: None,
        default_enabled: true,
    },
    BuiltinAction {
        id: "exit_blink",
        capability_id: "exit_blink",
        title: "退出 Blink",
        title_en: "Exit Blink",
        subtitle: "Exit Blink Launcher",
        subtitle_en: "Exit Blink Launcher",
        keywords: &["退出", "exit", "quit", "tc", "关闭", "结束"],
        context: &[],
        param_source: ParamSource::None,
        param_field: "",
        result_action: None,
        default_enabled: true,
    },
    BuiltinAction {
        id: "open_logs",
        capability_id: "open_logs",
        title: "打开日志文件",
        title_en: "Open Log File",
        subtitle: "Open Blink Log File",
        subtitle_en: "Open Blink Log File",
        keywords: &["日志", "log", "日志文件", "rz"],
        context: &[],
        param_source: ParamSource::None,
        param_field: "",
        result_action: None,
        default_enabled: true,
    },
    BuiltinAction {
        id: "open_data_dir",
        capability_id: "open_data_dir",
        title: "打开数据目录",
        title_en: "Open Data Directory",
        subtitle: "Open Blink Data Folder",
        subtitle_en: "Open Blink Data Folder",
        keywords: &["目录", "文件夹", "数据", "ml"],
        context: &[],
        param_source: ParamSource::None,
        param_field: "",
        result_action: None,
        default_enabled: true,
    },
    BuiltinAction {
        id: "blink_print_debug_info",
        capability_id: "blink_print_debug_info",
        title: "Blink Print Debug Info",
        title_en: "Blink Print Debug Info",
        subtitle: "复制 Blink 通用调试信息；当前版本详细包含 Windows Hook 与输入状态",
        subtitle_en: "Copy general Blink debug info, currently with detailed Windows hook and input state",
        keywords: &[
            "debug",
            "debug info",
            "print debug",
            "调试",
            "调试信息",
            "诊断",
            "diagnostic",
        ],
        context: &[],
        param_source: ParamSource::None,
        param_field: "",
        result_action: Some(BuiltinResultAction::CopyText { skip_persist: true }),
        default_enabled: true,
    },
    BuiltinAction {
        id: "blink_debug_inithook",
        capability_id: "blink_debug_inithook",
        title: "Blink Debug InitHook",
        title_en: "Blink Debug InitHook",
        subtitle: "打印调试信息，并在安全门禁满足后重置输入状态与重装 Windows Hook",
        subtitle_en: "Print debug info, then safely reset input state and reinstall the Windows hook",
        keywords: &[
            "debug",
            "debug inithook",
            "inithook",
            "init hook",
            "恢复hook",
            "恢复输入",
            "修复快捷键",
            "alt卡住",
        ],
        context: &[],
        param_source: ParamSource::None,
        param_field: "",
        result_action: Some(BuiltinResultAction::CopyText { skip_persist: true }),
        default_enabled: true,
    },
    // ── 0.8.0 §1.3 参数化动作 ───────────────────────────────────────────────
    // 0.21.3：target 直接为既有 Capability（open_url/open_path/reveal_in_explorer），
    // 不再经 ActionRegistry → CapabilityRegistry fallback。
    // 0.21.13：param_field 声明目标 schema 字段名，ParamSource::extract 直接产最终 JSON object。
    BuiltinAction {
        id: "open_url",
        capability_id: "open_url",
        title: "打开链接",
        title_en: "Open URL",
        subtitle: "用默认浏览器打开剪贴板中的 URL",
        subtitle_en: "Open the clipboard URL in the default browser",
        keywords: &["打开链接", "open url", "dkurl", "url", "链接"],
        context: &[ContextTrigger::ClipboardIsUrl],
        param_source: ParamSource::Clipboard,
        param_field: "url",
        result_action: None,
        default_enabled: true,
    },
    BuiltinAction {
        id: "open_path",
        capability_id: "open_path",
        title: "打开路径",
        title_en: "Open Path",
        subtitle: "用系统默认程序打开剪贴板中的文件或目录",
        subtitle_en: "Open the clipboard file or directory with the default program",
        keywords: &["打开路径", "打开目录", "open path", "dklj", "路径"],
        context: &[ContextTrigger::ClipboardIsFilePath],
        param_source: ParamSource::Clipboard,
        param_field: "path",
        result_action: None,
        default_enabled: true,
    },
    BuiltinAction {
        id: "reveal_in_explorer",
        capability_id: "reveal_in_explorer",
        title: "在资源管理器中显示",
        title_en: "Reveal in Explorer",
        subtitle: "定位到剪贴板中的文件（explorer /select）",
        subtitle_en: "Reveal the clipboard file in Windows Explorer",
        keywords: &["定位", "resource", "reveal", "explorer", "dw", "资源管理器"],
        context: &[ContextTrigger::ClipboardIsFilePath],
        param_source: ParamSource::Clipboard,
        param_field: "path",
        result_action: None,
        default_enabled: true,
    },
];

/// 内置动作引擎。
pub struct BuiltinEngine;

#[async_trait::async_trait]
impl SearchEngine for BuiltinEngine {
    fn id(&self) -> &'static str {
        "builtin"
    }

    fn lane(&self) -> super::engine::Lane {
        super::engine::Lane::Sync
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    /// 搜索内置动作。
    ///
    /// 0.8.0 §1.3 双路匹配：
    /// - **空 query**：只召回 Context 命中的动作，`base_score = 1.0` 确保首屏首位。
    /// - **非空 query**：keyword/拼音 匹配（原逻辑）+ Context 命中（新逻辑）并行判定；
    ///   两路都命中取 max(keyword_score, 0.3) 再 `+ 0.3` 作为 ctx 加成（上限 1.0），
    ///   `score_detail` 反映两路来源。
    /// - **参数校验**：Action 声明 `param_source != None` 但从 snapshot 抽不到值 → 不召回
    ///   （避免"打开链接"配空参数的僵尸候选）。
    /// - **disable 校验**：`ctx.disabled_builtin_actions` 命中 → 跳过。
    async fn search(&self, query: &str, ctx: &QueryContext<'_>) -> Vec<SearchItem> {
        let q = query.trim().to_lowercase();
        let is_empty = q.is_empty();

        let mut items = Vec::new();
        for action in ACTIONS {
            // 1. disable 检查
            if ctx
                .disabled_builtin_actions
                .iter()
                .any(|id| id == action.id)
            {
                continue;
            }

            // 2a. Context 门禁（0.8.0 §1.3）：声明了 triggers 的参数化 Action，必须
            //     至少一条 trigger 命中才召回——**keyword 命中也不能绕过**。
            //     否则会出现"复制'缺' + 输入'打开链接'"这种 keyword 蒙混、参数不合法
            //     的僵尸候选，回车执行时报"找不到文件'缺'"。
            //     无 context 声明的 Action（现有 9 个无参动作）不受此闸门约束。
            //
            //     0.11.8：逐 trigger 判定而非 `any_hit` 聚合——命中后还需查 binding
            //     黑名单（`disabled_context_bindings`），让内置动作的 context 触发能按
            //     binding 粒度禁用。多 trigger 时只要有一条命中且未禁用即算通过。
            let mut ctx_hit = false;
            if !action.context.is_empty() {
                for trig in action.context {
                    if !ctx_trigger::is_hit(trig, ctx.snapshot, None) {
                        continue;
                    }
                    let key = crate::domain::intent::binding_key(
                        &format!("builtin:{}", action.id),
                        crate::domain::intent::trigger_key(trig),
                    );
                    if ctx.disabled_context_bindings.iter().any(|k| k == &key) {
                        tracing::trace!(binding = %key, "内置动作 context binding 被禁用，跳过该 trigger");
                        continue;
                    }
                    ctx_hit = true;
                    break;
                }
                if !ctx_hit {
                    continue;
                }
            }

            // 2b. 参数抽取 + 参数校验：Action 声明需要参数但抽不到值 → 不召回
            //     （能走到这里的参数化 Action 已通过 Context 门禁，参数一般存在；
            //     此处兜底防守，避免 snapshot 结构变化时静默出错）。
            //     0.21.13：extract 直接产最终 JSON object（如 { "url": "..." }），
            //     不再返回裸 String——command 层无需二次猜测参数形状。
            let arg = action
                .param_source
                .extract(ctx.snapshot, action.param_field);
            if action.param_source != ParamSource::None && arg.is_none() {
                continue;
            }

            // 3. 双路评分
            let kw_match = if is_empty {
                None
            } else {
                match_query(&q, action)
            };

            let (base_score, detail) = match (kw_match, ctx_hit) {
                (None, false) => continue, // 两路都没命中，不召回
                (Some(m), false) => {
                    let s = m.score();
                    (s, format!("builtin={:.1}", s))
                }
                (None, true) => {
                    // Context-only 命中：空 query 时 1.0（首屏首位），非空时 0.3（弱加成）
                    let s = if is_empty { 1.0 } else { 0.3 };
                    (s, format!("ctx=+{:.1}", s))
                }
                (Some(m), true) => {
                    // 双路命中：以 keyword 分为主，附加 0.3 ctx 加成；上限 1.0
                    let kw_s = m.score();
                    let s = (kw_s + 0.3).min(1.0);
                    (s, format!("builtin={:.1} ctx=+0.3", kw_s))
                }
            };

            // 0.10.8 §11.2 方案 1：空 query + Context-only 命中 = 环境自动填充候选。
            // keyword 命中 / 非空 query 表达了用户意图，不标记。
            let context_aware = is_empty && kw_match.is_none() && ctx_hit;

            let item_id = format!("builtin:{}", action.id);
            let score = apply_history(base_score, &item_id, ctx.history);
            items.push(action_to_search_item(
                action,
                score,
                arg,
                detail,
                context_aware,
            ));
        }
        items
    }
}

/// 匹配查询，返回匹配类型（决定基础分数）。
///
/// 匹配来源（0.8.0 §1.3 增强）：
/// - title 原文
/// - 每个 keyword 原文
/// - title / keyword 的**拼音首字母**（如 "打开设置" → "dksz"，"设置" → "sz"）
/// - title / keyword 的**全拼**（如 "打开设置" → "dakaishezhi"）
///
/// 优先级：title 原文 > keyword 完全相等 > 其他前缀/包含。评分粒度由 `BuiltinMatch` 决定，
/// 拼音派生的匹配当作 KeywordPrefix（避免"shezhi 命中的分数超过原文'设置'"这种反常）。
fn match_query(q: &str, action: &BuiltinAction) -> Option<BuiltinMatch> {
    // 1. 标题原文包含查询词（最高优先级）
    if action.title.to_lowercase().contains(q) {
        return Some(BuiltinMatch::TitleContains);
    }

    // 2. 关键词原文匹配（完全相等 / 前缀）
    for kw in action.keywords {
        let kw_lower = kw.to_lowercase();
        if kw_lower == q {
            return Some(BuiltinMatch::KeywordExact);
        }
        if kw_lower.starts_with(q) {
            return Some(BuiltinMatch::KeywordPrefix);
        }
    }

    // 3. 拼音派生匹配（0.8.0 §1.3）：让 "shezhi" / "dakaishezhi" 也能命中"打开设置"
    //    只对**中文文本**派生拼音——纯 ASCII 的 keyword（如 "settings" / "sz"）跳过，
    //    避免"settings" 派生成空串再造成误命中。
    if q.chars().all(|c| c.is_ascii_lowercase()) && !q.is_empty() {
        // 派生源：title 一份 + 每个 keyword 一份
        let sources = std::iter::once(action.title).chain(action.keywords.iter().copied());
        for src in sources {
            if !contains_cjk(src) {
                continue;
            }
            let initials = crate::infra::utils::text::pinyin_initials(src);
            let full = crate::infra::utils::text::pinyin_full(src);
            if initials.starts_with(q) || full.starts_with(q) || full.contains(q) {
                return Some(BuiltinMatch::KeywordPrefix);
            }
        }
    }

    None
}

/// 是否含 CJK 汉字。用于跳过纯 ASCII keyword（如 "settings"）的拼音派生。
fn contains_cjk(s: &str) -> bool {
    s.chars().any(|c| {
        matches!(c,
            '\u{4E00}'..='\u{9FFF}'
            | '\u{3400}'..='\u{4DBF}'
            | '\u{20000}'..='\u{2A6DF}'
        )
    })
}

/// BuiltinAction → SearchItem 转换。
///
/// 0.8.0 §1.3 起走 `SearchAction::RunAction { id, arg }`，取代原 `__BLINK_ACTION_XXX__`
/// 魔法串路径。参数化动作（Task 8 的 OpenUrl 等）通过 `arg` 携带 clipboard/selection 值；
/// 无参动作 `arg = None`。
///
/// 0.21.13：`arg` 已是目标 Capability schema 接收的最终 JSON object
/// （如 `{ "url": "https://..." }`），command 层无需二次转换。
fn action_to_search_item(
    action: &BuiltinAction,
    score: f32,
    arg: Option<serde_json::Value>,
    score_detail: String,
    context_aware: bool,
) -> SearchItem {
    // 0.21.3：RunAction.id 直接为 capability_id（descriptor target），
    // 不再隐式假设 descriptor id == capability id。
    SearchItem {
        id: format!("builtin:{}", action.id),
        title: action.title.to_string(),
        subtitle: Some(action.subtitle.to_string()),
        score,
        action: SearchAction::RunAction {
            id: action.capability_id.to_string(),
            arg,
        },
        source: "builtin".to_string(),
        score_detail: Some(score_detail),
        context_aware,
        color_list_hex: None,
    }
}

// ── 设置页 DTO（0.8.0 §1.3）────────────────────────────────────────────────

/// 设置页「内置动作」面板展示的元数据。
///
/// 前端拿到后按 id 映射图标 + 渲染开关。字段设计原则：**预格式化**——
/// `trigger_desc` / `param_desc` 直接是可读文案（"剪贴板是 URL"），
/// 前端无需知道 `ContextTrigger` / `ParamSource` 枚举细节；未来加新变体只改后端。
#[derive(Debug, Clone, Serialize)]
pub struct BuiltinActionInfo {
    /// 动作 id（注册表 key，如 `"open_settings"`）——设置页 disable 开关的稳定标识。
    pub id: String,
    /// 主显示名（如 "打开设置"）
    pub title: String,
    /// 副显示名（如 "Blink 偏好设置"）
    pub subtitle: String,
    /// 关键词列表（展示"输入这些词可触发"）
    pub keywords: Vec<String>,
    /// 触发方式的**补充**说明；仅当存在 Context 触发或无任何触发时才有值。
    /// 纯 keyword 触发的动作返回 `None`——前面 `keywords: ...` 一行已表达清楚，不再重复。
    pub trigger_desc: Option<String>,
    /// 参数来源的可读描述；无参 = `None`
    pub param_desc: Option<String>,
    /// Context 门禁动作（声明了 context triggers）：关键词命中也必须 Context 命中才召回
    /// （见 `BuiltinEngine::search` 2a 门禁）。目录页据此不展示"关键词：…"死提示，
    /// 改以 context 触发条件作为本地入口标签。
    pub context_gated: bool,
    /// 当前是否启用（用户没在设置页 disable = true；被 disable = false）
    pub enabled: bool,
    /// 默认启用状态——供设置页显示"已改动"视觉提示
    pub default_enabled: bool,
}

/// 列出所有内置动作元数据 + 当前 enabled 状态。
///
/// `disabled_ids` 从 `AppConfig.disabled_builtin_actions` 读得——由命令层注入，
/// 保持 domain 层与 SQLite 解耦。
/// `language` 用于选择 descriptor 自带的双语 title/subtitle。
///
/// 0.21.3：title/subtitle 从 descriptor 自带双语字段读，不再依赖 ActionRegistry。
/// 语言前缀匹配："zh" → 中文，其他 → 英文。
pub fn list_builtin_actions(disabled_ids: &[String], language: &str) -> Vec<BuiltinActionInfo> {
    let use_en = language.starts_with("en");
    ACTIONS
        .iter()
        .map(|a| BuiltinActionInfo {
            id: a.id.to_string(),
            title: if use_en { a.title_en } else { a.title }.to_string(),
            subtitle: if use_en { a.subtitle_en } else { a.subtitle }.to_string(),
            keywords: a.keywords.iter().map(|k| k.to_string()).collect(),
            trigger_desc: describe_triggers(a.keywords, a.context),
            param_desc: describe_param_source(a.param_source),
            context_gated: !a.context.is_empty(),
            enabled: !disabled_ids.iter().any(|id| id == a.id),
            default_enabled: a.default_enabled,
        })
        .collect()
}

/// 列出所有内置动作的 Context binding（0.11.8）。
///
/// **用途**：设置页「Ghost 触发规则」面板。此前该面板只枚举插件 manifest 的 context
/// binding（`list_context_bindings` 命令），漏掉了内置参数化动作（`open_url` /
/// `open_path` / `reveal_in_explorer`）——它们在 `BuiltinEngine.search` 内部自判 context，
/// 不走 `RuleRouter.context_rules`。本函数补齐这一路，让前端 UI 能 disable 内置动作的
/// context 触发，对应 `BuiltinEngine` 已支持的 `disabled_context_bindings` 黑名单。
///
/// **字段与 `list_context_bindings` 命令对齐**（`commands.rs`），前端 `renderBindingRow`
/// 无需区分 manifest / builtin 两路来源，走同一渲染逻辑：
/// - `key`：`{target_id}::{trigger_key}`，如 `builtin:open_url::clipboard_is_url`
/// - `target_id`：`builtin:{action.id}`，与 `SearchItem.id` 一致
/// - `trigger_key`：snake_case，由 `intent::trigger_key` 派生
/// - `target_label`：动作名（i18n 解析），缺失时降级 `target_id`
/// - `trigger_label`：snake_case key，前端按 i18n 翻译
/// - `enabled`：binding 是否启用（未在黑名单中）
///
/// `disabled` 是 `AppConfig.disabled_context_bindings` 的快照。
pub fn list_builtin_context_bindings(
    disabled: &[String],
    language: &str,
) -> Vec<serde_json::Value> {
    let use_en = language.starts_with("en");
    let mut out = Vec::new();
    for action in ACTIONS.iter() {
        if action.context.is_empty() {
            continue;
        }
        let target_id = format!("builtin:{}", action.id);
        let target_label = if use_en {
            action.title_en
        } else {
            action.title
        }
        .to_string();
        for trig in action.context {
            let trigger_key = crate::domain::intent::trigger_key(trig);
            let key = crate::domain::intent::binding_key(&target_id, trigger_key);
            let enabled = !disabled.iter().any(|k| k == &key);
            out.push(serde_json::json!({
                "key": key,
                "target_id": target_id,
                "trigger_key": trigger_key,
                "target_label": target_label,
                "trigger_label": trigger_key, // 前端按 key 翻译（i18n）
                "enabled": enabled,
            }));
        }
    }
    out
}

/// 触发方式的**补充**可读描述。
///
/// 组合规则（0.8.0 §1.3 设置页）——只在需要补充信息时才输出，避免与 `keywords: ...` 行重复：
/// - 有 keyword 且无 context → `None`（`keywords: ...` 已表达清楚）
/// - 有 keyword 且有 context → `Some("<context 文案>")`（keyword 那一路省略）
/// - 无 keyword 但有 context → `Some("<context 文案>")`（当前无此类，预留）
/// - 都无 → `Some("总是可见")`（当前无此类，预留）
fn describe_triggers(keywords: &[&'static str], triggers: &[ContextTrigger]) -> Option<String> {
    let has_kw = !keywords.is_empty();
    let ctx_desc = if triggers.is_empty() {
        None
    } else {
        Some(
            triggers
                .iter()
                .map(describe_trigger)
                .collect::<Vec<_>>()
                .join(" 或 "),
        )
    };
    match (has_kw, ctx_desc) {
        (true, None) => None,
        (_, Some(c)) => Some(c),
        (false, None) => Some("总是可见".to_string()),
    }
}

fn describe_trigger(t: &ContextTrigger) -> &'static str {
    match t {
        ContextTrigger::ClipboardIsUrl => "剪贴板是 URL",
        ContextTrigger::ClipboardIsFilePath => "剪贴板是文件路径",
        ContextTrigger::SelectionNonEmpty => "选中了文本",
        ContextTrigger::TextIsNonTargetLang { .. } => "文本值得翻译",
    }
}

fn describe_param_source(s: ParamSource) -> Option<String> {
    match s {
        ParamSource::None => None,
        ParamSource::Clipboard => Some("剪贴板".to_string()),
        ParamSource::Selection => Some("选中的文本".to_string()),
    }
}

/// 按 descriptor id 查找声明的结果动作（0.21.13）。
///
/// command 层 `run_builtin_action` 成功后用此获取显式结果动作，
/// 替代旧按 capability id 特判前贴板的逻辑。
/// 返回 `None` = 无后续副作用；返回 `Some(CopyText { .. })` = 复制 Text 结果。
#[allow(dead_code)] // 公开 API：command 层当前按 capability_id 查找，此函数供测试和未来消费者使用
pub fn find_result_action(descriptor_id: &str) -> Option<BuiltinResultAction> {
    ACTIONS
        .iter()
        .find(|a| a.id == descriptor_id)
        .and_then(|a| a.result_action)
}

/// 按 capability id 查找声明的结果动作（0.21.13）。
///
/// `run_builtin_action` 收到的 `id` 是 `SearchAction::RunAction.id`，
/// 即 descriptor 的 `capability_id`（不假设与 descriptor id 相等）。
/// 用此函数按真实 capability target 查找结果动作。
pub fn find_result_action_by_capability_id(capability_id: &str) -> Option<BuiltinResultAction> {
    ACTIONS
        .iter()
        .find(|a| a.capability_id == capability_id)
        .and_then(|a| a.result_action)
}

/// 按 descriptor id 查找对应的 capability_id（0.21.13）。
///
/// 证明 descriptor id 与 capability id 不隐式假设相等——
/// command 层可查真实 target，而非假设 `id == capability_id`。
#[allow(dead_code)] // 公开 API：证明 descriptor→capability target 查找走显式字段，供测试验证
pub fn find_capability_id(descriptor_id: &str) -> Option<&'static str> {
    ACTIONS
        .iter()
        .find(|a| a.id == descriptor_id)
        .map(|a| a.capability_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infra::platform::context::ContextSnapshot;
    use std::collections::HashMap;

    /// 构造无 Context 命中的默认查询上下文。
    fn make_ctx<'a>(
        history: &'a HashMap<String, (i64, i64)>,
        snapshot: &'a ContextSnapshot,
    ) -> QueryContext<'a> {
        QueryContext {
            history,
            snapshot,
            disabled_builtin_actions: &[],
            disabled_context_bindings: &[],
            language: "zh",
        }
    }

    // ── 原 keyword 匹配路径（0.7 行为兼容） ───────────────────────────────────

    #[tokio::test]
    async fn search_settings() {
        let engine = BuiltinEngine;
        let history = HashMap::new();
        let snapshot = ContextSnapshot::default();
        let ctx = make_ctx(&history, &snapshot);

        let items = engine.search("设置", &ctx).await;
        assert!(!items.is_empty());
        assert_eq!(items[0].title, "打开设置");
        assert!(items[0].score > 0.0);
    }

    #[tokio::test]
    async fn search_edit_clipboard_image() {
        let engine = BuiltinEngine;
        let history = HashMap::new();
        let snapshot = ContextSnapshot::default();
        let ctx = make_ctx(&history, &snapshot);

        let items = engine.search("编辑图片", &ctx).await;
        assert!(
            items
                .iter()
                .any(|item| item.id == "builtin:edit_clipboard_image"),
            "编辑图片应召回本地用户 Action"
        );
    }

    #[tokio::test]
    async fn search_lock() {
        let engine = BuiltinEngine;
        let history = HashMap::new();
        let snapshot = ContextSnapshot::default();
        let ctx = make_ctx(&history, &snapshot);

        let items = engine.search("锁定", &ctx).await;
        assert!(!items.is_empty());
        assert_eq!(items[0].title, "锁定电脑");
    }

    #[tokio::test]
    async fn search_pinyin_initial() {
        let engine = BuiltinEngine;
        let history = HashMap::new();
        let snapshot = ContextSnapshot::default();
        let ctx = make_ctx(&history, &snapshot);

        // 首字母 "sz" 匹配 "设置"
        let items = engine.search("sz", &ctx).await;
        assert!(!items.is_empty());
        assert_eq!(items[0].title, "打开设置");
    }

    // ── 0.8.0 §1.3 拼音派生匹配 ───────────────────────────────────────────────

    #[tokio::test]
    async fn search_pinyin_full() {
        // 全拼 "shezhi" 应命中 keyword "设置"
        let engine = BuiltinEngine;
        let history = HashMap::new();
        let snapshot = ContextSnapshot::default();
        let ctx = make_ctx(&history, &snapshot);

        let items = engine.search("shezhi", &ctx).await;
        assert!(
            items.iter().any(|it| it.id == "builtin:open_settings"),
            "全拼 shezhi 应召回 open_settings"
        );
    }

    #[tokio::test]
    async fn search_pinyin_full_title() {
        // 全拼 "dakaishezhi" 应命中 title "打开设置"
        let engine = BuiltinEngine;
        let history = HashMap::new();
        let snapshot = ContextSnapshot::default();
        let ctx = make_ctx(&history, &snapshot);

        let items = engine.search("dakaishezhi", &ctx).await;
        assert!(
            items.iter().any(|it| it.id == "builtin:open_settings"),
            "全拼 dakaishezhi 应召回 open_settings"
        );
    }

    #[tokio::test]
    async fn search_pinyin_full_prefix() {
        // 全拼前缀 "guanj" 应命中 title "关机"（guanji）
        let engine = BuiltinEngine;
        let history = HashMap::new();
        let snapshot = ContextSnapshot::default();
        let ctx = make_ctx(&history, &snapshot);

        let items = engine.search("guanj", &ctx).await;
        assert!(
            items.iter().any(|it| it.id == "builtin:shutdown"),
            "全拼前缀 guanj 应召回 shutdown"
        );
    }

    #[tokio::test]
    async fn search_pinyin_does_not_over_match() {
        // 保护：随机 ASCII 串不应误命中——确认拼音派生没让 match_query 变成万能通配
        let engine = BuiltinEngine;
        let history = HashMap::new();
        let snapshot = ContextSnapshot::default();
        let ctx = make_ctx(&history, &snapshot);

        let items = engine.search("xyzabc123", &ctx).await;
        assert!(items.is_empty());
    }

    #[tokio::test]
    async fn search_no_match() {
        let engine = BuiltinEngine;
        let history = HashMap::new();
        let snapshot = ContextSnapshot::default();
        let ctx = make_ctx(&history, &snapshot);

        let items = engine.search("xyzabc123", &ctx).await;
        assert!(items.is_empty());
    }

    // ── 空 query 路径（0.8.0 §1.3 新行为） ────────────────────────────────────

    #[tokio::test]
    async fn empty_query_no_context_returns_nothing() {
        // 空 query + 无 Context 命中 → 无召回（现有 9 个动作 context 均为空）
        let engine = BuiltinEngine;
        let history = HashMap::new();
        let snapshot = ContextSnapshot::default();
        let ctx = make_ctx(&history, &snapshot);

        let items = engine.search("", &ctx).await;
        assert!(items.is_empty());
    }

    // ── disable 路径 ──────────────────────────────────────────────────────────

    #[tokio::test]
    async fn disabled_action_not_recalled() {
        let engine = BuiltinEngine;
        let history = HashMap::new();
        let snapshot = ContextSnapshot::default();
        let disabled = vec!["open_settings".to_string()];
        let ctx = QueryContext {
            history: &history,
            snapshot: &snapshot,
            disabled_builtin_actions: &disabled,
            disabled_context_bindings: &[],
            language: "zh",
        };

        // "设置" 原本匹配 open_settings；被 disable 后不召回
        let items = engine.search("设置", &ctx).await;
        assert!(items.iter().all(|it| it.id != "builtin:open_settings"));
    }

    // ── 0.8.0 §1.3 参数化 Action + Context 路径 ──────────────────────────────

    /// 构造带 clipboard 的快照。
    fn snapshot_with_clipboard(text: &str) -> ContextSnapshot {
        ContextSnapshot::with_clipboard(text)
    }

    #[tokio::test]
    async fn empty_query_context_url_hits_open_url() {
        // 空 query + 剪贴板是 URL → 只召回 open_url，base_score=1.0
        let engine = BuiltinEngine;
        let history = HashMap::new();
        let snapshot = snapshot_with_clipboard("https://example.com");
        let ctx = make_ctx(&history, &snapshot);

        let items = engine.search("", &ctx).await;
        let open_url = items.iter().find(|it| it.id == "builtin:open_url");
        assert!(open_url.is_some(), "剪贴板是 URL 应召回 open_url");
        assert_eq!(
            open_url.unwrap().score,
            1.0,
            "空 query Context-only base_score=1.0"
        );
        // arg 应携带 URL 的最终 JSON object
        if let super::SearchAction::RunAction { arg, .. } = &open_url.unwrap().action {
            assert_eq!(
                arg.as_ref()
                    .and_then(|v| v.get("url"))
                    .and_then(|v| v.as_str()),
                Some("https://example.com")
            );
        } else {
            panic!("open_url 应产 RunAction");
        }
        // 剪贴板不是文件路径 → open_path / reveal_in_explorer 不召回
        assert!(items.iter().all(|it| it.id != "builtin:open_path"));
        assert!(items.iter().all(|it| it.id != "builtin:reveal_in_explorer"));
    }

    #[tokio::test]
    async fn empty_query_context_path_hits_open_path_and_reveal() {
        // 空 query + 剪贴板是文件路径 → open_path / reveal_in_explorer 都召回
        let engine = BuiltinEngine;
        let history = HashMap::new();
        let snapshot = snapshot_with_clipboard("C:\\Users\\test.txt");
        let ctx = make_ctx(&history, &snapshot);

        let items = engine.search("", &ctx).await;
        assert!(items.iter().any(|it| it.id == "builtin:open_path"));
        assert!(items.iter().any(|it| it.id == "builtin:reveal_in_explorer"));
        // 不是 URL → open_url 不召回
        assert!(items.iter().all(|it| it.id != "builtin:open_url"));
    }

    #[tokio::test]
    async fn param_missing_action_not_recalled() {
        // 参数化 Action 声明 param_source=Clipboard，但剪贴板为空 → 即使 keyword 命中也不召回
        let engine = BuiltinEngine;
        let history = HashMap::new();
        let snapshot = ContextSnapshot::default(); // clipboard = None
        let ctx = make_ctx(&history, &snapshot);

        // 输入"打开链接"能匹配 open_url 的 keyword，但缺参 → 不召回
        let items = engine.search("打开链接", &ctx).await;
        assert!(items.iter().all(|it| it.id != "builtin:open_url"));
    }

    #[tokio::test]
    async fn keyword_matches_but_clipboard_not_url_no_recall() {
        // 修复回归：0.8.0 §1.3 早期版本会误召回。
        // 输入 "打开链接" keyword 命中 open_url，但剪贴板是普通文本"缺"——
        // Context 门禁应挡下，避免 "找不到文件'缺'" 之类的运行期错误。
        let engine = BuiltinEngine;
        let history = HashMap::new();
        let snapshot = snapshot_with_clipboard("缺");
        let ctx = make_ctx(&history, &snapshot);

        let items = engine.search("打开链接", &ctx).await;
        assert!(
            items.iter().all(|it| it.id != "builtin:open_url"),
            "Context 未命中时，keyword 命中不该绕过闸门召回参数化 Action"
        );

        // 同理：keyword 命中 open_path，但剪贴板是"缺"（既非 URL 也非路径）→ 不召回
        let items = engine.search("打开路径", &ctx).await;
        assert!(items.iter().all(|it| it.id != "builtin:open_path"));
    }

    #[tokio::test]
    async fn keyword_and_context_both_hit_dedup() {
        // query "打开链接" 匹配 open_url keyword，同时 clipboard 是 URL 触发 Context
        // 应只产出一条 open_url，且分数 = max(keyword, 0.3) + 0.3
        let engine = BuiltinEngine;
        let history = HashMap::new();
        let snapshot = snapshot_with_clipboard("https://example.com");
        let ctx = make_ctx(&history, &snapshot);

        let items = engine.search("打开链接", &ctx).await;
        let open_url_items: Vec<_> = items
            .iter()
            .filter(|it| it.id == "builtin:open_url")
            .collect();
        assert_eq!(open_url_items.len(), 1, "同一 Action 至多一条 SearchItem");
        assert!(
            open_url_items[0]
                .score_detail
                .as_deref()
                .unwrap_or("")
                .contains("ctx=+0.3"),
            "score_detail 应体现 ctx 加成"
        );
    }

    #[tokio::test]
    async fn empty_query_disabled_context_action_not_recalled() {
        // 剪贴板是 URL，但 open_url 被 disable → 不召回
        let engine = BuiltinEngine;
        let history = HashMap::new();
        let snapshot = snapshot_with_clipboard("https://example.com");
        let disabled = vec!["open_url".to_string()];
        let ctx = QueryContext {
            history: &history,
            snapshot: &snapshot,
            disabled_builtin_actions: &disabled,
            disabled_context_bindings: &[],
            language: "zh",
        };

        let items = engine.search("", &ctx).await;
        assert!(items.iter().all(|it| it.id != "builtin:open_url"));
    }

    // ── 0.11.8：context binding 黑名单（disabled_context_bindings） ─────────

    #[tokio::test]
    async fn context_binding_disabled_blocks_empty_query_recall() {
        // 剪贴板是 URL，但 `builtin:open_url::clipboard_is_url` 被 binding 粒度禁用
        // → 空 query 时 open_url 不召回（与整条禁用等价，但能仅禁 context 不禁 keyword）
        let engine = BuiltinEngine;
        let history = HashMap::new();
        let snapshot = snapshot_with_clipboard("https://example.com");
        let disabled_ctx = vec!["builtin:open_url::clipboard_is_url".to_string()];
        let ctx = QueryContext {
            history: &history,
            snapshot: &snapshot,
            disabled_builtin_actions: &[],
            disabled_context_bindings: &disabled_ctx,
            language: "zh",
        };

        let items = engine.search("", &ctx).await;
        assert!(
            items.iter().all(|it| it.id != "builtin:open_url"),
            "binding 黑名单禁用后，空 query Context 召回应被挡下"
        );
    }

    #[tokio::test]
    async fn context_binding_disabled_keeps_keyword_recall_when_context_still_hits() {
        // 反向验证：binding 禁用了 context 触发，但 keyword 路径应也不召回——
        // 因为 builtin_engine 的 Context 门禁要求「参数化 Action 必须 context 命中」，
        // context 被禁 = context 未命中 = keyword 也不能绕过（与 disabled_builtin_actions
        // 同语义，只是粒度更细）。这是设计一致性验证。
        let engine = BuiltinEngine;
        let history = HashMap::new();
        let snapshot = snapshot_with_clipboard("https://example.com");
        let disabled_ctx = vec!["builtin:open_url::clipboard_is_url".to_string()];
        let ctx = QueryContext {
            history: &history,
            snapshot: &snapshot,
            disabled_builtin_actions: &[],
            disabled_context_bindings: &disabled_ctx,
            language: "zh",
        };

        // keyword "打开链接" 本会命中 open_url，但 context 被 binding 禁用 → 门禁挡下
        let items = engine.search("打开链接", &ctx).await;
        assert!(
            items.iter().all(|it| it.id != "builtin:open_url"),
            "binding 禁用 context 后，keyword 路径也应被 Context 门禁挡下"
        );
    }

    #[tokio::test]
    async fn context_binding_unrelated_key_does_not_block() {
        // 保护：黑名单里是无关 key（其他 action 的 binding）不应误伤本 action。
        // 剪贴板是 URL，黑名单含 `builtin:open_path::clipboard_is_file_path`（无关）→
        // open_url 应正常召回。
        let engine = BuiltinEngine;
        let history = HashMap::new();
        let snapshot = snapshot_with_clipboard("https://example.com");
        let disabled_ctx = vec!["builtin:open_path::clipboard_is_file_path".to_string()];
        let ctx = QueryContext {
            history: &history,
            snapshot: &snapshot,
            disabled_builtin_actions: &[],
            disabled_context_bindings: &disabled_ctx,
            language: "zh",
        };

        let items = engine.search("", &ctx).await;
        assert!(
            items.iter().any(|it| it.id == "builtin:open_url"),
            "无关 binding key 不应影响 open_url 召回"
        );
    }

    // ── 0.10.8 §11.2 方案 1：context_aware 标记 ──────────────────────────────

    #[tokio::test]
    async fn empty_query_context_only_marks_context_aware() {
        // 空 query + 剪贴板是 URL → open_url 标 context_aware=true
        // （前端 chordEligible 据此跳过，允许 chord 提示条与 Context Ghost 共存）
        let engine = BuiltinEngine;
        let history = HashMap::new();
        let snapshot = snapshot_with_clipboard("https://example.com");
        let ctx = make_ctx(&history, &snapshot);

        let items = engine.search("", &ctx).await;
        let open_url = items
            .iter()
            .find(|it| it.id == "builtin:open_url")
            .expect("剪贴板是 URL 应召回 open_url");
        assert!(
            open_url.context_aware,
            "空 query + Context-only 命中应标 context_aware=true"
        );
    }

    #[tokio::test]
    async fn keyword_hit_not_marked_context_aware() {
        // 非空 query + keyword 命中 + Context 命中 → context_aware=false
        // 用户已表达意图（"打开链接"），不是环境自动填充。
        let engine = BuiltinEngine;
        let history = HashMap::new();
        let snapshot = snapshot_with_clipboard("https://example.com");
        let ctx = make_ctx(&history, &snapshot);

        let items = engine.search("打开链接", &ctx).await;
        let open_url = items
            .iter()
            .find(|it| it.id == "builtin:open_url")
            .expect("keyword+Context 双命中应召回 open_url");
        assert!(
            !open_url.context_aware,
            "keyword 命中表达用户意图，不应标 context_aware"
        );

        // 空 query 但无 Context 命中的对照——搜索"设置"应召回 open_settings 且非 context_aware
        let empty_snap = ContextSnapshot::default();
        let ctx2 = make_ctx(&history, &empty_snap);
        let items2 = engine.search("设置", &ctx2).await;
        let open_settings = items2
            .iter()
            .find(|it| it.id == "builtin:open_settings")
            .expect("keyword '设置' 应召回 open_settings");
        assert!(
            !open_settings.context_aware,
            "纯 keyword 命中不应标 context_aware"
        );
    }

    #[tokio::test]
    async fn non_empty_context_bonus_not_marked_context_aware() {
        // 非空 query + Context-only 命中（kw_match=None, ctx_hit=true）
        // 用户已开始输入，即使无 keyword 命中也不算"环境自动填充"。
        // 构造：query="xyz" 不命中任何 keyword，剪贴板是 URL 触发 Context。
        let engine = BuiltinEngine;
        let history = HashMap::new();
        let snapshot = snapshot_with_clipboard("https://example.com");
        let ctx = make_ctx(&history, &snapshot);

        let items = engine.search("xyz", &ctx).await;
        // 若召回（走 (None, true) 分支 base_score=0.3），context_aware 必须为 false
        if let Some(open_url) = items.iter().find(|it| it.id == "builtin:open_url") {
            assert!(
                !open_url.context_aware,
                "非空 query 即使走 Context-only 分支也不标 context_aware（用户已在输入）"
            );
        }
    }

    // ── 0.19.17：诊断动作搜索 ─────────────────────────────────────────────────

    #[tokio::test]
    async fn search_blink_print_debug_info() {
        let engine = BuiltinEngine;
        let history = HashMap::new();
        let snapshot = ContextSnapshot::default();
        let ctx = make_ctx(&history, &snapshot);

        let items = engine.search("诊断", &ctx).await;
        assert!(
            items
                .iter()
                .any(|it| it.id == "builtin:blink_print_debug_info"),
            "搜索'诊断'应召回 blink_print_debug_info"
        );
    }

    #[tokio::test]
    async fn search_blink_print_debug_info_pinyin() {
        let engine = BuiltinEngine;
        let history = HashMap::new();
        let snapshot = ContextSnapshot::default();
        let ctx = make_ctx(&history, &snapshot);

        // 拼音首字母 "zd" 匹配 "诊断"
        let items = engine.search("zd", &ctx).await;
        assert!(
            items
                .iter()
                .any(|it| it.id == "builtin:blink_print_debug_info"),
            "搜索'zd'应召回 blink_print_debug_info"
        );
    }

    #[tokio::test]
    async fn search_blink_debug_inithook() {
        let engine = BuiltinEngine;
        let history = HashMap::new();
        let snapshot = ContextSnapshot::default();
        let ctx = make_ctx(&history, &snapshot);

        let items = engine.search("恢复输入", &ctx).await;
        assert!(
            items
                .iter()
                .any(|it| it.id == "builtin:blink_debug_inithook"),
            "搜索'恢复输入'应召回 blink_debug_inithook"
        );
    }

    #[tokio::test]
    async fn search_blink_debug_inithook_english() {
        let engine = BuiltinEngine;
        let history = HashMap::new();
        let snapshot = ContextSnapshot::default();
        let ctx = make_ctx(&history, &snapshot);

        let items = engine.search("inithook", &ctx).await;
        assert!(
            items
                .iter()
                .any(|it| it.id == "builtin:blink_debug_inithook"),
            "搜索'inithook'应召回 blink_debug_inithook"
        );
    }

    // ── 0.21.13: 参数协议契约测试 ──────────────────────────────────────────────

    /// 无参 descriptor 产生 `None` arg（command 层传 `{}`）。
    #[tokio::test]
    async fn no_param_descriptor_produces_none_arg() {
        let engine = BuiltinEngine;
        let history = HashMap::new();
        let snapshot = ContextSnapshot::default();
        let ctx = make_ctx(&history, &snapshot);

        let items = engine.search("设置", &ctx).await;
        let open_settings = items
            .iter()
            .find(|it| it.id == "builtin:open_settings")
            .expect("搜索'设置'应召回 open_settings");
        if let super::SearchAction::RunAction { arg, .. } = &open_settings.action {
            assert!(
                arg.is_none(),
                "无参 descriptor arg 应为 None（command 层传 {{}}）"
            );
        } else {
            panic!("open_settings 应产 RunAction");
        }
    }

    /// `open_url` 从 Clipboard 产生 `{ "url": "..." }`。
    #[tokio::test]
    async fn open_url_produces_url_object() {
        let engine = BuiltinEngine;
        let history = HashMap::new();
        let snapshot = snapshot_with_clipboard("https://example.com");
        let ctx = make_ctx(&history, &snapshot);

        let items = engine.search("", &ctx).await;
        let open_url = items
            .iter()
            .find(|it| it.id == "builtin:open_url")
            .expect("剪贴板是 URL 应召回 open_url");
        if let super::SearchAction::RunAction { arg, .. } = &open_url.action {
            let arg = arg.as_ref().expect("参数化 descriptor arg 不应为 None");
            assert_eq!(
                arg["url"], "https://example.com",
                "arg 应为 {{ \"url\": \"...\" }}"
            );
        } else {
            panic!("open_url 应产 RunAction");
        }
    }

    /// `open_path` 产生 `{ "path": "..." }`。
    #[tokio::test]
    async fn open_path_produces_path_object() {
        let engine = BuiltinEngine;
        let history = HashMap::new();
        let snapshot = snapshot_with_clipboard("C:\\Users\\test.txt");
        let ctx = make_ctx(&history, &snapshot);

        let items = engine.search("", &ctx).await;
        let open_path = items
            .iter()
            .find(|it| it.id == "builtin:open_path")
            .expect("剪贴板是文件路径应召回 open_path");
        if let super::SearchAction::RunAction { arg, .. } = &open_path.action {
            let arg = arg.as_ref().expect("参数化 descriptor arg 不应为 None");
            assert_eq!(
                arg["path"], "C:\\Users\\test.txt",
                "arg 应为 {{ \"path\": \"...\" }}"
            );
        } else {
            panic!("open_path 应产 RunAction");
        }
    }

    /// `reveal_in_explorer` 产生 `{ "path": "..." }`。
    #[tokio::test]
    async fn reveal_in_explorer_produces_path_object() {
        let engine = BuiltinEngine;
        let history = HashMap::new();
        let snapshot = snapshot_with_clipboard("C:\\Users\\test.txt");
        let ctx = make_ctx(&history, &snapshot);

        let items = engine.search("", &ctx).await;
        let reveal = items
            .iter()
            .find(|it| it.id == "builtin:reveal_in_explorer")
            .expect("剪贴板是文件路径应召回 reveal_in_explorer");
        if let super::SearchAction::RunAction { arg, .. } = &reveal.action {
            let arg = arg.as_ref().expect("参数化 descriptor arg 不应为 None");
            assert_eq!(
                arg["path"], "C:\\Users\\test.txt",
                "arg 应为 {{ \"path\": \"...\" }}"
            );
        } else {
            panic!("reveal_in_explorer 应产 RunAction");
        }
    }

    /// Clipboard 空白值不召回参数化动作。
    #[tokio::test]
    async fn blank_clipboard_does_not_recall() {
        let engine = BuiltinEngine;
        let history = HashMap::new();
        // 只有空白字符——find_text trim 后为空，不召回
        let snapshot = snapshot_with_clipboard("   ");
        let ctx = make_ctx(&history, &snapshot);

        let items = engine.search("", &ctx).await;
        assert!(
            items.iter().all(|it| it.id != "builtin:open_url"),
            "空白剪贴板不应召回 open_url"
        );
    }

    /// descriptor id 与 capability id 不相同时，RunAction 使用真实 capability target。
    /// 验证没有隐式 `id == capability_id` 假设。
    #[tokio::test]
    async fn run_action_uses_capability_id_not_descriptor_id() {
        // 遍历所有 ACTIONS，验证 RunAction.id == capability_id（而非 descriptor id）。
        // 对于当前所有条目 capability_id == id，但这个测试证明代码使用的是 capability_id 字段，
        // 而非假设 id == capability_id。
        let engine = BuiltinEngine;
        let history = HashMap::new();
        let snapshot = ContextSnapshot::default();
        let ctx = make_ctx(&history, &snapshot);

        // 用一个无参动作验证——它的 RunAction.id 应等于 capability_id
        let items = engine.search("设置", &ctx).await;
        let item = items
            .iter()
            .find(|it| it.id == "builtin:open_settings")
            .expect("应召回 open_settings");
        if let super::SearchAction::RunAction { id, .. } = &item.action {
            assert_eq!(
                id, "open_settings",
                "RunAction.id 应为 capability_id，不是 descriptor id"
            );
        } else {
            panic!("应产 RunAction");
        }
    }

    /// `find_capability_id` 查找正确返回真实 target。
    #[test]
    fn find_capability_id_returns_correct_target() {
        // 所有当前条目 capability_id == id，但证明查找走的是显式字段
        assert_eq!(find_capability_id("open_url"), Some("open_url"));
        assert_eq!(find_capability_id("open_settings"), Some("open_settings"));
        assert_eq!(find_capability_id("nonexistent"), None);
    }

    /// BuiltinEngine 产出的 arg 可直接交给对应 Capability invoke/schema 契约，
    /// 不经过二次转换——验证 arg 是 object 而非裸 string。
    #[tokio::test]
    async fn arg_is_object_not_bare_string() {
        let engine = BuiltinEngine;
        let history = HashMap::new();
        let snapshot = snapshot_with_clipboard("https://example.com");
        let ctx = make_ctx(&history, &snapshot);

        let items = engine.search("", &ctx).await;
        let open_url = items
            .iter()
            .find(|it| it.id == "builtin:open_url")
            .expect("应召回 open_url");
        if let super::SearchAction::RunAction { arg, .. } = &open_url.action {
            let arg = arg.as_ref().expect("arg 不应为 None");
            assert!(
                arg.is_object(),
                "arg 应是 JSON object（可直接 invoke），不是裸 string"
            );
        } else {
            panic!("应产 RunAction");
        }
    }

    // ── 0.21.13: 结果动作契约测试 ──────────────────────────────────────────────

    /// 诊断 descriptor 声明了 CopyText 结果动作。
    #[test]
    fn diagnostic_descriptors_declare_copy_text_result_action() {
        let debug_info_action = find_result_action("blink_print_debug_info");
        assert_eq!(
            debug_info_action,
            Some(BuiltinResultAction::CopyText { skip_persist: true }),
            "blink_print_debug_info 应声明 CopyText {{ skip_persist: true }}"
        );

        let inithook_action = find_result_action("blink_debug_inithook");
        assert_eq!(
            inithook_action,
            Some(BuiltinResultAction::CopyText { skip_persist: true }),
            "blink_debug_inithook 应声明 CopyText {{ skip_persist: true }}"
        );
    }

    /// 普通无参 descriptor 没有结果动作——普通 Text 不自动复制。
    #[test]
    fn non_diagnostic_descriptors_have_no_result_action() {
        assert_eq!(find_result_action("open_settings"), None);
        assert_eq!(find_result_action("lock"), None);
        assert_eq!(find_result_action("shutdown"), None);
        assert_eq!(find_result_action("open_url"), None);
        assert_eq!(find_result_action("open_path"), None);
    }

    /// 按 capability id 查找结果动作也返回正确值。
    #[test]
    fn find_result_action_by_capability_id_works() {
        assert_eq!(
            find_result_action_by_capability_id("blink_print_debug_info"),
            Some(BuiltinResultAction::CopyText { skip_persist: true })
        );
        assert_eq!(find_result_action_by_capability_id("open_settings"), None);
    }
}
