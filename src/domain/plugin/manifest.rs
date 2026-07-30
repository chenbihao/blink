//! 插件 manifest(JSON,见 phases/0.2-core-plugin-design.md §3.3)。
//!
//! 本切片解析必要字段;permissions/resources/icon 等未列字段由 serde 默认忽略
//! (不建字段)。permissions 自用阶段完全不解析(§3.6)。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// core 当前支持的 manifest schema_version 上限(§3.7 B4:超出范围拒绝加载)。
pub const SUPPORTED_SCHEMA_VERSION: u32 = 1;

/// 插件清单。
#[derive(Debug, Clone, Deserialize)]
pub struct PluginManifest {
    pub schema_version: u32,
    pub id: String,
    #[allow(dead_code)] // 设置页展示用(list_plugins 走 resolve)
    pub name: LocalizableText,
    #[allow(dead_code)]
    pub version: String,
    #[serde(default)]
    #[allow(dead_code)] // 设置页展示用(list_plugins 走 resolve)
    pub description: LocalizableText,
    #[serde(default)]
    #[allow(dead_code)] // builtin 信任来源标记,本切片只加载 builtin
    pub builtin: bool,
    /// 首次装机是否默认启用（缺省 true，保持向后兼容）。
    ///
    /// 用于"需配置才能用"的插件（如翻译需 API 密钥）——首装写默认 `PluginConfig`
    /// 时以此值覆盖 `enabled`，避免用户装完就撞到无法工作的插件。
    ///
    /// 只影响首次装机路径（`init_configs` 里 DB 无该插件记录时）。老用户 DB 已
    /// 存在的 `plugin:{id}` 记录不会被本字段回退——用户之前的开关状态保留。
    #[serde(default = "default_true")]
    pub default_enabled: bool,
    pub runtime: PluginRuntime,
    #[serde(default, deserialize_with = "deserialize_triggers_lenient")]
    pub triggers: Vec<PluginTrigger>,
    #[serde(default)]
    pub capabilities: Vec<String>,
    /// 配置项元数据声明(驱动设置页 UI 渲染)。缺失则该插件用裸 JSON 编辑(降级)。
    #[serde(default)]
    pub settings_schema: Vec<SettingField>,
    /// 插件声明的 tool 列表(0.9.3)——AI 路由可调用的能力。
    ///
    /// 每个 tool 对应一份 `ActionSchema` + `DangerClass`，启动时注册进
    /// `ActionRegistry`，与 builtin 动作并列供 AI tool-call 消费。
    /// 缺失或空 = 该插件不参与 AI tool-call（老插件向后兼容）。
    #[serde(default)]
    pub tools: Vec<ToolDef>,
}

/// 插件声明的 tool 定义(0.9.3)——对齐 `ActionSchema` + `DangerClass`。
///
/// **0.11.1 改进 3a**：新增 5 个元信息字段（`result_type` / `hint` / `examples` /
/// `sensitive` / `progress_hint`）+ `setting_bindings`（3b 参数动态注入用）。
/// 全部 serde default，老 manifest 零迁移。
///
/// manifest 示例:
/// ```jsonc
/// "tools": [{
///   "name": "translate",
///   "description": "翻译文本到目标语言",
///   "parameters": { "type": "object", "properties": { "text": { "type": "string" } } },
///   "danger_class": "Safe",
///   "result_type": "text",
///   "progress_hint": "翻译文本",
///   "setting_bindings": { "target_lang": "target_lang" }
/// }]
/// ```
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ToolDef {
    /// tool 唯一标识——全局唯一(ActionRegistry 按此查找)，冲突则 warn + 跳过。
    pub name: String,
    /// 人类可读描述，直接送入 LLM。
    #[serde(default)]
    pub description: String,
    /// JSON Schema Object（draft-07），对齐 OpenAI function calling / MCP tool schema。
    #[serde(default = "default_empty_object_schema")]
    pub parameters: serde_json::Value,
    /// 危险等级——Safe 可直接执行，Dangerous 需人机确认。默认 Safe。
    #[serde(default)]
    pub danger_class: DangerClassDef,

    // ── 0.11.1 改进 3a：工具元信息增强 ──────────────────────────────────────
    /// 返回类型声明（0.11.1 §2.3a）——帮 AI 预期结果形态 + 帮 lane 选投影路径。
    /// 缺失（老插件）为 None，消费方按现有行为兜底（插件走 Items 投影）。
    #[serde(default)]
    pub result_type: Option<ToolResultType>,
    /// 给 AI 的额外提示（0.11.1 §2.3a）——自动拼入 system prompt 工具描述段。
    /// 插件作者一句话告诉 AI 这个工具的用法窍门，如"返回多个 IP，公网 IP 通常最有价值"。
    #[serde(default)]
    pub hint: Option<String>,
    /// 示例调用（0.11.1 §2.3a）——帮弱模型理解参数用法。
    /// 0.11 默认不拼入 system prompt（省 token），0.12 本地模型模式注入。
    #[serde(default)]
    pub examples: Option<Vec<serde_json::Value>>,
    /// 声明敏感（0.11.1 §2.3a）——读隐私数据（应用列表/剪贴板历史等）。
    /// 0.11 不做权限强制，0.12 MCP server 暴露时需用户显式授权。default false。
    #[serde(default)]
    pub sensitive: bool,
    /// 占位文案提示词（0.11.1 §2.3a / §3.2）——拼成 `AI 正在{progress_hint}…`。
    /// 缺失时回退到 description 前 8 字 + `…`。3 个 builtin 插件均填此字段。
    #[serde(default)]
    pub progress_hint: Option<String>,
    /// 参数→插件 setting 的绑定映射（0.11.1 §2.3b 参数动态注入配置用）。
    ///
    /// key = parameters 中的参数名，value = settings 中的 setting key。
    /// 投影时若对应 setting 已配置（非空），则：
    /// - 从 `required` 移除该参数（required → optional）
    /// - 注入 `"default": <setting_value>`
    /// - description 自动追加"（默认: {value}）"
    ///
    /// **投影层改动**：不动 manifest 的 `parameters` 原文，只在 `build_capability_tools`
    /// 时通过 `inject_plugin_settings` 生成新的 ActionSchema。运行时 setting 变更
    /// 下次构建 tools 时自动生效（每次 AI 请求都重建）。
    #[serde(default)]
    pub setting_bindings: Option<std::collections::HashMap<String, String>>,

    /// manifest 投影规则（0.14 §三 Cap 协议分层）——轨道 A 的配置。
    ///
    /// 告诉投影引擎"怎么把插件返回的纯 data 投影成 CapabilityResult"。
    /// 缺失时（老插件 / 轨道 B）走旧路径——插件直接返回 `PluginItem` 列表。
    ///
    /// manifest 示例:
    /// ```jsonc
    /// "projection": {
    ///   "result_shape": "text",
    ///   "pointer": "$",
    ///   "desc": "译文",
    ///   "item_actions": [{"type": "copy"}]
    /// }
    /// ```
    #[serde(default)]
    pub projection: Option<crate::domain::capability::ProjectionRule>,
}

/// 工具返回类型声明（0.11.1 §2.3a）。
///
/// 帮 AI 预期结果形态 + 帮 lane 选投影路径。serde `lowercase`，
/// 与 manifest 的 `"result_type": "text"` 字符串对齐。
#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ToolResultType {
    /// 单文本结果（如 translate 译文）。
    Text,
    /// 结构化列表（如 get_ip 返回多个 IP、search_apps 返回应用列表）。
    Items,
    /// 仅执行完成无数据返回（如 lock 锁屏）。
    Done,
}

/// `ToolDef` 的 Default——仅供测试构造便利（`..Default::default()`）。
/// 实际解析走 serde，name 缺失会由 `from_path` 的 JSON 结构保证必填。
impl Default for ToolDef {
    fn default() -> Self {
        ToolDef {
            name: String::new(),
            description: String::new(),
            parameters: default_empty_object_schema(),
            danger_class: DangerClassDef::default(),
            result_type: None,
            hint: None,
            examples: None,
            sensitive: false,
            progress_hint: None,
            setting_bindings: None,
            projection: None,
        }
    }
}

/// manifest 侧 danger_class 声明——映射到 `domain::execution::DangerClass`。
///
/// 用独立 enum 而非直接引用 execution 模块，保持 manifest 解析层零业务依赖。
#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "PascalCase")]
pub enum DangerClassDef {
    #[default]
    Safe,
    Dangerous,
}

fn default_empty_object_schema() -> serde_json::Value {
    serde_json::json!({ "type": "object", "properties": {} })
}

/// 插件运行时类型。
#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum RuntimeType {
    /// 原生可执行文件（直接 spawn）
    Process,
    /// Python 脚本（python xxx.py）
    Python,
    /// Node.js 脚本（node xxx.js）
    Node,
    /// PowerShell 脚本（powershell -File xxx.ps1）
    Powershell,
}

impl Default for RuntimeType {
    fn default() -> Self {
        RuntimeType::Process
    }
}

/// 进程拉起参数。
#[derive(Debug, Clone, Deserialize)]
pub struct PluginRuntime {
    /// 可执行路径(相对 manifest 所在目录,或绝对路径)。
    pub exec: String,
    /// 运行时类型，默认 Process
    #[serde(default)]
    pub r#type: RuntimeType,
    #[serde(default)]
    #[allow(dead_code)]
    pub protocol: Option<String>,
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    #[serde(default)]
    #[allow(dead_code)] // 并发池 §3.7 B3 后续做
    pub concurrency: Option<u32>,
    /// 最短参数长度(字符数,0=不限)。小于该长度的查询直接返回空(不发进程请求)。
    /// 用于天气/翻译等需要完整输入才有意义的插件,避免 IME 中间态/短词浪费网络。
    #[serde(default)]
    pub min_arg_length: Option<usize>,
    /// 防抖间隔(毫秒,0=不防抖)。连续输入停止该时间后才触发插件查询。
    /// 网络类插件(翻译/天气)建议 300-800ms,避免每次按键都发 HTTP 请求。
    /// 本地插件保持 0(默认),每键触发即时反馈。
    #[serde(default)]
    pub debounce_ms: Option<u64>,
    /// 空参数引导文案（0.8.1）。语义："我需要用户输入才能工作，空参数时请显示这条静态提示"。
    ///
    /// 与 `min_arg_length` 正交：
    /// - `min_arg_length` 管的是"带参但太短"——降级到 Generic 搜索，插件让位。
    /// - `empty_arg_hint` 管的是"完全没参数"——插件明确表达"我要参数"，框架合成
    ///   静态占位 item 直接展示，**根本不发起进程/子任务调用**。
    ///
    /// 典型受益：翻译/搜索/查词类。天气/IP 这类"空参数有意义"的插件保持不填此字段。
    /// 前端渲染无差异——就是一条普通 PluginItem，`action=none` 不可执行。
    #[serde(default)]
    pub empty_arg_hint: Option<LocalizableText>,
}

/// 触发器。0.8.2 §3.2.3 加 `Context` 变体。
///
/// **serde 容错**：`triggers: Vec<PluginTrigger>` 字段用自定义反序列化——单条 trigger
/// 解析失败（未知 tag / 未知 when 值 / surface 越界等）仅 `warn!` 后跳过该条，其他
/// trigger 保留。避免 0.x 阶段新增 tag 时旧版 blink 加载新 manifest 直接崩掉整个插件。
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum PluginTrigger {
    Keyword {
        keyword: String,
        #[serde(default = "default_exclusive")]
        #[allow(dead_code)] // 独占语义留给 RuleRouter(§4.3),本切片不消费
        exclusive: bool,
    },
    /// 正则触发(本切片定义不实现)。
    #[allow(dead_code)]
    Regex {
        pattern: String,
        #[serde(default = "default_exclusive")]
        exclusive: bool,
    },
    /// Context 触发（0.8.2 §3.2.3）。**弱意图信号，永不 Takeover**——
    /// manifest 侧 `surface` 只允许 `Priority`；填 `Inline` 时 `RuleRouter` warn+降级。
    Context {
        when: ManifestContextWhen,
        #[serde(default)]
        surface: ManifestSurfaceHint,
    },
}

/// manifest 侧 Context 触发条件（0.8.2 §3.2.3）。
///
/// 字符串形态：`snake_case`。对应 `domain::context::trigger::ContextTrigger`
/// （由 `RuleRouter` 完成映射；本模块只做 manifest 解析）。
#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ManifestContextWhen {
    ClipboardIsUrl,
    ClipboardIsFilePath,
    SelectionNonEmpty,
    /// 翻译插件专用：文本（selection 优先，缺则 clipboard）值得翻译。
    TextIsNonTargetLang,
}

/// manifest 侧 surface 声明。0.8.2 只允许 `Priority`；`Inline` 保留 enum 但
/// `RuleRouter` 收到时 warn+降级。0.8.3 Chord / "搜索选区"插件真需要 Inline 时放开。
#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ManifestSurfaceHint {
    Priority,
    Inline,
}

impl Default for ManifestSurfaceHint {
    fn default() -> Self {
        ManifestSurfaceHint::Priority
    }
}

fn default_exclusive() -> bool {
    true
}

fn default_true() -> bool {
    true
}

/// 容错反序列化 triggers（0.8.2 §3.2.3）。
///
/// 逐条尝试解析：`serde_json::from_value::<PluginTrigger>(...)` 失败 → warn 后跳过，
/// 保留其余 trigger。避免 0.x 阶段新增 `type` / `when` 值时旧版 blink 直接崩掉整个 manifest。
fn deserialize_triggers_lenient<'de, D>(deserializer: D) -> Result<Vec<PluginTrigger>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    // 先当 raw Vec<Value>，再逐条尝试转 PluginTrigger
    let raw: Vec<serde_json::Value> = Vec::deserialize(deserializer)?;
    let mut out = Vec::with_capacity(raw.len());
    for (idx, item) in raw.into_iter().enumerate() {
        // 抽 type 字段做日志锚点，避免 %item 把整段 JSON(可能几 K)塞进日志
        let raw_type = item
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or("<missing>")
            .to_string();
        match serde_json::from_value::<PluginTrigger>(item) {
            Ok(t) => out.push(t),
            Err(e) => {
                tracing::warn!(
                    index = idx,
                    raw_type = %raw_type,
                    error = %e,
                    "triggers[{}] 解析失败，跳过该条（未知 type/when/surface 或字段缺失）",
                    idx,
                );
            }
        }
    }
    Ok(out)
}

// ── 配置 schema(0.5.1,驱动设置页 UI)──────────────────────────────────────────

/// 可本地化文本:既接受纯字符串,也接受多语言对象(serde untagged)。
/// resolve(lang) 按传入语言取值(回退 zh → 首个);lang 由调用方从 AppConfig.language 传入,
/// Rust 类型与前端均无需随 manifest 数据形态变化而改动。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum LocalizableText {
    Plain(String),
    Localized(std::collections::HashMap<String, String>),
}

/// 缺省值 Plain(""):manifest 的 description 有 #[serde(default)],缺省时填空串。
impl Default for LocalizableText {
    fn default() -> Self {
        LocalizableText::Plain(String::new())
    }
}

impl LocalizableText {
    /// 解析为当前展示文本:Localized 按 lang 取,回退 zh,再回退首个;Plain 原样返回。
    pub fn resolve(&self, lang: &str) -> String {
        match self {
            LocalizableText::Plain(s) => s.clone(),
            LocalizableText::Localized(map) => map
                .get(lang)
                .or_else(|| map.get("zh"))
                .or_else(|| map.values().next())
                .cloned()
                .unwrap_or_default(),
        }
    }
}

/// 配置项值类型(决定前端渲染成什么控件)。
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SettingType {
    Boolean,
    String,
    Number,
    Enum,
    /// select 是 enum 的别名(翻译等插件使用)。
    Select,
    /// 可拖动排序列表(存储为 JSON 数组)。
    #[serde(rename = "sortable_list")]
    SortableList,
}

impl SettingType {
    /// 类型缺省默认值(schema 未给 default 时用)。
    pub fn default_value(&self) -> serde_json::Value {
        match self {
            SettingType::Boolean => serde_json::Value::Bool(false),
            SettingType::String => serde_json::Value::String(String::new()),
            SettingType::Number => serde_json::json!(0),
            SettingType::Enum | SettingType::Select => serde_json::Value::Null,
            SettingType::SortableList => serde_json::json!([]),
        }
    }
}

/// enum 选项。
#[derive(Debug, Clone, Deserialize)]
pub struct SettingOption {
    pub value: serde_json::Value,
    pub label: LocalizableText,
}

/// 单个配置项的元数据声明。
#[derive(Debug, Clone, Deserialize)]
pub struct SettingField {
    /// 配置键(对应 settings JSON 字段名)。
    pub key: String,
    /// 值类型。
    #[serde(rename = "type")]
    pub kind: SettingType,
    /// 展示标题。
    pub title: LocalizableText,
    /// 描述(可选)。
    #[serde(default)]
    pub description: Option<LocalizableText>,
    /// 默认值(缺失按类型推断)。
    #[serde(default)]
    pub default: Option<serde_json::Value>,
    /// enum/select 可选项。兼容两种格式:
    /// - 对象数组: `[{"value":"x","label":"X"}]`
    /// - 扁平字符串数组: `["x","y","z"]`（value=label=字符串本身）
    #[serde(default, deserialize_with = "deserialize_options")]
    pub options: Vec<SettingOption>,
    /// number 范围约束(UI 用,可选)。
    #[serde(default)]
    pub min: Option<f64>,
    #[serde(default)]
    pub max: Option<f64>,
    /// 分组(可选)。支持两种格式:
    /// - 字符串: `"group": "有道智云"`
    /// - 对象: `"group": { "title": "有道智云", "description": "..." }`
    #[serde(default, deserialize_with = "deserialize_group")]
    pub group: Option<GroupConfig>,
}

/// 配置项分组。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GroupConfig {
    /// 分组标题。
    pub title: String,
    /// 分组描述(可选,如申请地址)。
    #[serde(default)]
    pub description: Option<String>,
}

/// 反序列化 group：兼容字符串 `"group": "xxx"` 和对象 `"group": { "title": "xxx", "description": "..." }`。
fn deserialize_group<'de, D>(deserializer: D) -> Result<Option<GroupConfig>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de;

    struct GroupVisitor;

    impl<'de> de::Visitor<'de> for GroupVisitor {
        type Value = Option<GroupConfig>;

        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("group 字符串或对象")
        }

        fn visit_none<E>(self) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            Ok(None)
        }

        fn visit_some<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
        where
            D: serde::Deserializer<'de>,
        {
            deserializer.deserialize_any(GroupVisitor)
        }

        fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            Ok(Some(GroupConfig {
                title: value.to_string(),
                description: None,
            }))
        }

        fn visit_map<M>(self, map: M) -> Result<Self::Value, M::Error>
        where
            M: de::MapAccess<'de>,
        {
            let map: serde_json::Map<String, serde_json::Value> =
                de::Deserialize::deserialize(de::value::MapAccessDeserializer::new(map))?;
            let title = map
                .get("title")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let description = map
                .get("description")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            Ok(Some(GroupConfig { title, description }))
        }
    }

    deserializer.deserialize_option(GroupVisitor)
}

/// 反序列化 options：兼容 `["a","b"]` 和 `[{"value":"a","label":"A"}]`。
fn deserialize_options<'de, D>(deserializer: D) -> Result<Vec<SettingOption>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de;

    struct OptionsVisitor;

    impl<'de> de::Visitor<'de> for OptionsVisitor {
        type Value = Vec<SettingOption>;

        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("option 对象数组或字符串数组")
        }

        fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
        where
            A: de::SeqAccess<'de>,
        {
            let mut options = Vec::new();
            while let Some(val) = seq.next_element::<serde_json::Value>()? {
                match val {
                    // 扁平字符串 → SettingOption { value=字符串, label=Plain(字符串) }
                    serde_json::Value::String(s) => {
                        options.push(SettingOption {
                            value: serde_json::Value::String(s.clone()),
                            label: LocalizableText::Plain(s),
                        });
                    }
                    // 对象 → 直接反序列化为 SettingOption
                    serde_json::Value::Object(_) => {
                        let opt: SettingOption =
                            serde_json::from_value(val).map_err(de::Error::custom)?;
                        options.push(opt);
                    }
                    other => {
                        return Err(de::Error::custom(format!(
                            "options 元素类型不支持: {other}"
                        )));
                    }
                }
            }
            Ok(options)
        }
    }

    deserializer.deserialize_seq(OptionsVisitor)
}

impl PluginManifest {
    /// 从 manifest.json 路径解析。
    pub fn from_path(path: &Path) -> Result<Self, String> {
        let raw = std::fs::read_to_string(path).map_err(|e| format!("读取失败: {e}"))?;
        let manifest: PluginManifest =
            serde_json::from_str(&raw).map_err(|e| format!("解析失败: {e}"))?;
        if manifest.schema_version > SUPPORTED_SCHEMA_VERSION {
            return Err(format!(
                "schema_version {} 超出支持上限 {}",
                manifest.schema_version, SUPPORTED_SCHEMA_VERSION
            ));
        }
        Ok(manifest)
    }

    /// 是否提供 query 召回能力。
    pub fn supports_query(&self) -> bool {
        self.capabilities.iter().any(|c| c == "query")
    }

    /// 解析 exec 为绝对路径(相对路径基于 manifest 所在目录,不依赖 cwd)。
    ///
    /// 对于 Rust 内置插件（type=Process, exec 以 ./bin/ 开头），Dev 模式下直接指向 target/debug/，
    /// 避免创建 Junction 符号链接导致 IDE 索引爆炸。Release 模式下保持相对路径不变。
    pub fn exec_path(&self, manifest_dir: &Path) -> PathBuf {
        let exec = &self.runtime.exec;

        // Rust 内置插件特殊处理：exec 以 ./bin/ 开头 且 是 Process 类型
        // Dev 模式下直接指向 target/debug/{exe_name}，无需 Junction
        #[cfg(debug_assertions)]
        if matches!(self.runtime.r#type, RuntimeType::Process) && exec.starts_with("./bin/") {
            // 提取 exe 文件名："./bin/blink-plugin-echo.exe" -> "blink-plugin-echo.exe"
            let exe_name = exec.trim_start_matches("./bin/");
            // CARGO_MANIFEST_DIR 是 Blink 根目录
            if let Ok(root) = std::env::var("CARGO_MANIFEST_DIR") {
                let target_path = PathBuf::from(root).join("target/debug").join(exe_name);
                if target_path.exists() {
                    return target_path;
                }
            }
            // fallback: manifest 上溯三级到项目根 -> target/debug
            let fallback = manifest_dir
                .parent()
                .and_then(|p| p.parent())
                .and_then(|p| p.parent())
                .map(|root| root.join("target/debug").join(exe_name));
            if let Some(path) = fallback {
                if path.exists() {
                    return path;
                }
            }
        }

        // 普通情况：相对路径基于 manifest 所在目录
        let exec_path = PathBuf::from(exec);
        let joined = if exec_path.is_absolute() {
            exec_path
        } else {
            manifest_dir.join(exec_path)
        };
        // Windows verbatim 前缀 `\\?\` 需要剥掉：Tauri 在 release 下 `resource_dir()`
        // 返回的路径以 `\\?\` 开头，join 后一路带着这个前缀传给 `CreateProcessW`，
        // Windows 会以 ERROR_PATH_NOT_FOUND (os error 3) 拒绝拉起进程。
        // dev 模式路径不带前缀，此函数对普通路径是幂等的。
        strip_windows_verbatim_prefix(&joined)
    }

    /// 查询超时(毫秒),缺省 3000。
    pub fn timeout_ms(&self) -> u64 {
        self.runtime.timeout_ms.unwrap_or(3000)
    }

    /// 从 settings_schema 生成默认 settings JSON {key: default}。无 schema 返回 null。
    pub fn default_settings(&self) -> serde_json::Value {
        if self.settings_schema.is_empty() {
            return serde_json::Value::Null;
        }
        let mut map = serde_json::Map::new();
        for field in &self.settings_schema {
            let val = field
                .default
                .clone()
                .unwrap_or_else(|| field.kind.default_value());
            map.insert(field.key.clone(), val);
        }
        serde_json::Value::Object(map)
    }
}

/// 剥掉 Windows verbatim/extended-length 前缀 `\\?\`。
///
/// 背景：Tauri release 下 `resource_dir()` 返回的路径带 `\\?\` 前缀（如
/// `\\?\D:\DevTools\Blink\...`），join 后仍保留。`CreateProcessW` 不接受此前缀
/// 作为 `lpApplicationName`，会返回 ERROR_PATH_NOT_FOUND (os error 3)。
///
/// UNC 形式的 `\\?\UNC\server\share\...` 转成 `\\server\share\...`。
/// 非 Windows 或普通路径原样返回（幂等）。
fn strip_windows_verbatim_prefix(p: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        // 用字符串处理更稳：PathBuf 组件 API 在遇到 `\\?\` 时行为随版本变化。
        if let Some(s) = p.to_str() {
            if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
                return PathBuf::from(format!(r"\\{rest}"));
            }
            if let Some(rest) = s.strip_prefix(r"\\?\") {
                return PathBuf::from(rest);
            }
        }
    }
    p.to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{
        "schema_version": 1,
        "id": "builtin.echo",
        "name": "Echo",
        "version": "0.1.0",
        "builtin": true,
        "runtime": { "type": "process", "protocol": "jsonl", "exec": "./bin/echo.exe", "timeout_ms": 1000 },
        "triggers": [{ "type": "keyword", "keyword": "echo", "exclusive": true }],
        "capabilities": ["query"],
        "permissions": ["clipboard"],
        "homepage": "https://example.com"
    }"#;

    #[test]
    fn parses_required_fields_ignores_unknown() {
        let m: PluginManifest = serde_json::from_str(SAMPLE).unwrap();
        assert_eq!(m.id, "builtin.echo");
        assert!(m.builtin);
        assert!(m.supports_query());
        assert_eq!(m.timeout_ms(), 1000);
        assert_eq!(m.triggers.len(), 1);
        // permissions / homepage 未建字段,被 serde 忽略,不报错
    }

    #[test]
    fn keyword_trigger_default_exclusive() {
        let json = r#"{"type":"keyword","keyword":"ip"}"#;
        let t: PluginTrigger = serde_json::from_str(json).unwrap();
        assert!(matches!(
            t,
            PluginTrigger::Keyword {
                exclusive: true,
                ..
            }
        ));
    }

    #[test]
    fn missing_timeout_defaults_3000() {
        let json =
            r#"{"schema_version":1,"id":"x","name":"X","version":"0","runtime":{"exec":"x.exe"}}"#;
        let m: PluginManifest = serde_json::from_str(json).unwrap();
        assert_eq!(m.timeout_ms(), 3000);
    }

    #[test]
    fn settings_schema_parses_plain_and_localized() {
        let json = r#"{
            "schema_version": 1, "id": "x", "name": "X", "version": "0",
            "runtime": {"exec": "x.exe"},
            "settings_schema": [
                {"key":"use_ipv6","type":"boolean","title":"查询 IPv6","default":false},
                {"key":"geo","type":"enum","title":{"zh":"定位","en":"Geo"},"default":"ip-api.com",
                 "options":[{"value":"ip-api.com","label":"推荐"},{"value":"none","label":"关闭"}]}
            ]
        }"#;
        let m: PluginManifest = serde_json::from_str(json).unwrap();
        assert_eq!(m.settings_schema.len(), 2);
        // Plain title
        assert_eq!(m.settings_schema[0].title.resolve("zh"), "查询 IPv6");
        // Localized title → 按 lang 取,zh 回退首个
        assert_eq!(m.settings_schema[1].title.resolve("zh"), "定位");
        // 传 en → 取 en 值(验证 i18n 按 locale 取值已接通)
        assert_eq!(m.settings_schema[1].title.resolve("en"), "Geo");
        // 默认 settings 由 schema 生成
        let defaults = m.default_settings();
        assert_eq!(defaults["use_ipv6"], false);
        assert_eq!(defaults["geo"], "ip-api.com");
    }

    #[test]
    fn no_schema_defaults_null() {
        let json =
            r#"{"schema_version":1,"id":"x","name":"X","version":"0","runtime":{"exec":"x.exe"}}"#;
        let m: PluginManifest = serde_json::from_str(json).unwrap();
        assert!(m.settings_schema.is_empty());
        assert!(m.default_settings().is_null());
    }

    #[test]
    fn select_type_with_flat_options() {
        // 模拟 translate 插件的 manifest 格式
        let json = r#"{
            "schema_version": 1, "id": "builtin.translate", "name": "翻译", "version": "0.1.0",
            "runtime": {"type": "python", "exec": "./main.py"},
            "settings_schema": [
                {"key":"default_engine","type":"select","title":"默认翻译引擎",
                 "options":["youdao","baidu","deepl"],"default":"youdao"},
                {"key":"target_lang","type":"select","title":"目标语言",
                 "options":["auto","zh","en","ja","ko"],"default":"zh"}
            ]
        }"#;
        let m: PluginManifest = serde_json::from_str(json).unwrap();
        assert_eq!(m.settings_schema.len(), 2);
        assert_eq!(m.settings_schema[0].options.len(), 3);
        assert_eq!(m.settings_schema[0].options[0].value, "youdao");
        assert_eq!(
            m.settings_schema[0].options[0].label.resolve("zh"),
            "youdao"
        );
        let defaults = m.default_settings();
        assert_eq!(defaults["default_engine"], "youdao");
        assert_eq!(defaults["target_lang"], "zh");
    }

    #[cfg(windows)]
    #[test]
    fn exec_path_strips_verbatim_prefix() {
        // 模拟 release 下 Tauri resource_dir 带 \\?\ 前缀的场景。
        // exec 用 .exe 后缀而不是 ./bin/... 是为了避开 debug 特化分支(会尝试
        // 从 target/debug 取,与本测试无关);走通用相对路径 + join(manifest_dir) 分支。
        let json = r#"{"schema_version":1,"id":"x","name":"X","version":"0",
            "runtime":{"type":"process","exec":"blink-plugin-x.exe"}}"#;
        let m: PluginManifest = serde_json::from_str(json).unwrap();
        let dir = PathBuf::from(r"\\?\D:\DevTools\Blink\plugins\builtin\x");
        let got = m.exec_path(&dir);
        assert!(
            !got.to_string_lossy().starts_with(r"\\?\"),
            "exec_path 结果不应带 \\\\?\\ 前缀，实际={}",
            got.display(),
        );
    }

    #[test]
    fn context_trigger_parses() {
        let json = r#"{
            "schema_version": 1, "id": "translate", "name": "T", "version": "0",
            "runtime": {"exec": "x.exe"},
            "triggers": [
                {"type": "keyword", "keyword": "翻译"},
                {"type": "context", "when": "text_is_non_target_lang", "surface": "priority"}
            ]
        }"#;
        let m: PluginManifest = serde_json::from_str(json).unwrap();
        assert_eq!(m.triggers.len(), 2);
        assert!(matches!(
            m.triggers[1],
            PluginTrigger::Context {
                when: ManifestContextWhen::TextIsNonTargetLang,
                surface: ManifestSurfaceHint::Priority,
            }
        ));
    }

    #[test]
    fn context_trigger_surface_defaults_to_priority() {
        let json = r#"{
            "schema_version": 1, "id": "x", "name": "X", "version": "0",
            "runtime": {"exec": "x.exe"},
            "triggers": [
                {"type": "context", "when": "clipboard_is_url"}
            ]
        }"#;
        let m: PluginManifest = serde_json::from_str(json).unwrap();
        assert!(matches!(
            m.triggers[0],
            PluginTrigger::Context {
                when: ManifestContextWhen::ClipboardIsUrl,
                surface: ManifestSurfaceHint::Priority, // default
            }
        ));
    }

    #[test]
    fn default_enabled_defaults_to_true() {
        // 缺省不写 default_enabled 字段 → true（保持向后兼容，旧 manifest 首装即启用）
        let json = r#"{
            "schema_version": 1, "id": "x", "name": "X", "version": "0",
            "runtime": {"exec": "x.exe"}
        }"#;
        let m: PluginManifest = serde_json::from_str(json).unwrap();
        assert!(m.default_enabled);
    }

    #[test]
    fn default_enabled_explicit_false() {
        // 显式声明 false → 首装默认关（需配置才能用的插件用这个）
        let json = r#"{
            "schema_version": 1, "id": "x", "name": "X", "version": "0",
            "default_enabled": false,
            "runtime": {"exec": "x.exe"}
        }"#;
        let m: PluginManifest = serde_json::from_str(json).unwrap();
        assert!(!m.default_enabled);
    }

    #[test]
    fn triggers_lenient_skips_bad_entry() {
        // 一条未知 type 的 trigger 混在中间：只跳过它，keyword 保留
        let json = r#"{
            "schema_version": 1, "id": "x", "name": "X", "version": "0",
            "runtime": {"exec": "x.exe"},
            "triggers": [
                {"type": "keyword", "keyword": "a"},
                {"type": "future_unknown_type", "foo": "bar"},
                {"type": "keyword", "keyword": "b"}
            ]
        }"#;
        let m: PluginManifest = serde_json::from_str(json).unwrap();
        assert_eq!(m.triggers.len(), 2);
        assert!(matches!(&m.triggers[0], PluginTrigger::Keyword { keyword, .. } if keyword == "a"));
        assert!(matches!(&m.triggers[1], PluginTrigger::Keyword { keyword, .. } if keyword == "b"));
    }

    #[test]
    fn triggers_lenient_skips_bad_when_value() {
        // context trigger 的 when 是未知值 → 跳过该条,其他保留
        let json = r#"{
            "schema_version": 1, "id": "x", "name": "X", "version": "0",
            "runtime": {"exec": "x.exe"},
            "triggers": [
                {"type": "keyword", "keyword": "ok"},
                {"type": "context", "when": "some_future_condition"}
            ]
        }"#;
        let m: PluginManifest = serde_json::from_str(json).unwrap();
        assert_eq!(m.triggers.len(), 1);
        assert!(
            matches!(&m.triggers[0], PluginTrigger::Keyword { keyword, .. } if keyword == "ok")
        );
    }

    #[cfg(windows)]
    #[test]
    fn strip_verbatim_prefix_variants() {
        // 普通路径:不变
        assert_eq!(
            strip_windows_verbatim_prefix(Path::new(r"D:\a\b")),
            PathBuf::from(r"D:\a\b"),
        );
        // Verbatim 磁盘路径:去前缀
        assert_eq!(
            strip_windows_verbatim_prefix(Path::new(r"\\?\D:\a\b")),
            PathBuf::from(r"D:\a\b"),
        );
        // Verbatim UNC:转成普通 UNC
        assert_eq!(
            strip_windows_verbatim_prefix(Path::new(r"\\?\UNC\server\share\a")),
            PathBuf::from(r"\\server\share\a"),
        );
    }

    // ── 0.9.3 tools 字段 ─────────────────────────────────────────────────

    #[test]
    fn tools_field_defaults_to_empty() {
        // 老 manifest 无 tools 字段 → 空 vec，向后兼容
        let json =
            r#"{"schema_version":1,"id":"x","name":"X","version":"0","runtime":{"exec":"x.exe"}}"#;
        let m: PluginManifest = serde_json::from_str(json).unwrap();
        assert!(m.tools.is_empty());
    }

    #[test]
    fn tools_parses_single_tool() {
        let json = r#"{
            "schema_version": 1, "id": "translate", "name": "翻译", "version": "0",
            "runtime": {"exec": "main.py", "type": "python"},
            "tools": [{
                "name": "translate",
                "description": "翻译文本到目标语言",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "text": { "type": "string", "description": "要翻译的文本" }
                    },
                    "required": ["text"]
                },
                "danger_class": "Safe"
            }]
        }"#;
        let m: PluginManifest = serde_json::from_str(json).unwrap();
        assert_eq!(m.tools.len(), 1);
        let t = &m.tools[0];
        assert_eq!(t.name, "translate");
        assert_eq!(t.description, "翻译文本到目标语言");
        assert_eq!(t.parameters["properties"]["text"]["type"], "string");
        assert_eq!(t.danger_class, DangerClassDef::Safe);
    }

    #[test]
    fn tools_defaults_danger_class_to_safe() {
        let json = r#"{
            "schema_version": 1, "id": "x", "name": "X", "version": "0",
            "runtime": {"exec": "x.exe"},
            "tools": [{ "name": "foo", "description": "bar" }]
        }"#;
        let m: PluginManifest = serde_json::from_str(json).unwrap();
        assert_eq!(m.tools[0].danger_class, DangerClassDef::Safe);
    }

    #[test]
    fn tools_defaults_parameters_to_empty_object() {
        let json = r#"{
            "schema_version": 1, "id": "x", "name": "X", "version": "0",
            "runtime": {"exec": "x.exe"},
            "tools": [{ "name": "foo" }]
        }"#;
        let m: PluginManifest = serde_json::from_str(json).unwrap();
        assert_eq!(m.tools[0].parameters["type"], "object");
        assert!(
            m.tools[0].parameters["properties"]
                .as_object()
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn tools_danger_class_dangerous_parses() {
        let json = r#"{
            "schema_version": 1, "id": "x", "name": "X", "version": "0",
            "runtime": {"exec": "x.exe"},
            "tools": [{ "name": "delete_file", "danger_class": "Dangerous" }]
        }"#;
        let m: PluginManifest = serde_json::from_str(json).unwrap();
        assert_eq!(m.tools[0].danger_class, DangerClassDef::Dangerous);
    }

    #[test]
    fn tools_multiple_tools() {
        let json = r#"{
            "schema_version": 1, "id": "x", "name": "X", "version": "0",
            "runtime": {"exec": "x.exe"},
            "tools": [
                { "name": "translate", "description": "翻译" },
                { "name": "detect_lang", "description": "检测语言" }
            ]
        }"#;
        let m: PluginManifest = serde_json::from_str(json).unwrap();
        assert_eq!(m.tools.len(), 2);
        assert_eq!(m.tools[0].name, "translate");
        assert_eq!(m.tools[1].name, "detect_lang");
    }

    // ── 0.11.1 改进 3a：工具元信息新字段 ──────────────────────────────────

    #[test]
    fn tools_new_fields_default_when_missing() {
        // 老 manifest 无新字段 → 全部 default（None / false），向后兼容
        let json = r#"{
            "schema_version": 1, "id": "x", "name": "X", "version": "0",
            "runtime": {"exec": "x.exe"},
            "tools": [{ "name": "foo", "description": "bar", "danger_class": "Safe" }]
        }"#;
        let m: PluginManifest = serde_json::from_str(json).unwrap();
        let t = &m.tools[0];
        assert!(t.result_type.is_none());
        assert!(t.hint.is_none());
        assert!(t.examples.is_none());
        assert!(!t.sensitive);
        assert!(t.progress_hint.is_none());
        assert!(t.setting_bindings.is_none());
    }

    #[test]
    fn tools_result_type_parses_text_items_done() {
        for (json_val, expected) in [
            ("\"text\"", ToolResultType::Text),
            ("\"items\"", ToolResultType::Items),
            ("\"done\"", ToolResultType::Done),
        ] {
            let json = format!(
                r#"{{"schema_version":1,"id":"x","name":"X","version":"0",
                "runtime":{{"exec":"x.exe"}},
                "tools":[{{"name":"t","result_type":{json_val}}}]}}"#
            );
            let m: PluginManifest = serde_json::from_str(&json).unwrap();
            assert_eq!(
                m.tools[0].result_type,
                Some(expected),
                "result_type {json_val} 应解析为 {expected:?}"
            );
        }
    }

    #[test]
    fn tools_progress_hint_and_hint_parse() {
        let json = r#"{
            "schema_version": 1, "id": "x", "name": "X", "version": "0",
            "runtime": {"exec": "x.exe"},
            "tools": [{
                "name": "get_weather",
                "description": "查天气",
                "progress_hint": "查询天气",
                "hint": "返回结构化数据"
            }]
        }"#;
        let m: PluginManifest = serde_json::from_str(json).unwrap();
        assert_eq!(m.tools[0].progress_hint.as_deref(), Some("查询天气"));
        assert_eq!(m.tools[0].hint.as_deref(), Some("返回结构化数据"));
    }

    #[test]
    fn tools_sensitive_defaults_false_parses_true() {
        let json = r#"{
            "schema_version": 1, "id": "x", "name": "X", "version": "0",
            "runtime": {"exec": "x.exe"},
            "tools": [{ "name": "search_apps", "sensitive": true }]
        }"#;
        let m: PluginManifest = serde_json::from_str(json).unwrap();
        assert!(m.tools[0].sensitive);
    }

    #[test]
    fn tools_examples_parse_array() {
        let json = r#"{
            "schema_version": 1, "id": "x", "name": "X", "version": "0",
            "runtime": {"exec": "x.exe"},
            "tools": [{
                "name": "get_weather",
                "examples": [{"city": "北京"}, {"city": "Tokyo"}]
            }]
        }"#;
        let m: PluginManifest = serde_json::from_str(json).unwrap();
        let examples = m.tools[0].examples.as_ref().unwrap();
        assert_eq!(examples.len(), 2);
        assert_eq!(examples[0]["city"], "北京");
    }

    #[test]
    fn tools_setting_bindings_parse_map() {
        let json = r#"{
            "schema_version": 1, "id": "x", "name": "X", "version": "0",
            "runtime": {"exec": "x.exe"},
            "tools": [{
                "name": "get_weather",
                "setting_bindings": {"city": "default_city", "unit": "temperature_unit"}
            }]
        }"#;
        let m: PluginManifest = serde_json::from_str(json).unwrap();
        let bindings = m.tools[0].setting_bindings.as_ref().unwrap();
        assert_eq!(bindings.len(), 2);
        assert_eq!(bindings.get("city").unwrap(), "default_city");
        assert_eq!(bindings.get("unit").unwrap(), "temperature_unit");
    }

    #[test]
    fn tools_new_fields_serialize_roundtrip() {
        // 新字段 round-trip 稳定——translate manifest 已含 result_type=text。
        // PluginManifest 只 Deserialize，但 ToolDef 有 Serialize derive，
        // 对 ToolDef 做 round-trip 验证新字段序列化稳定。
        let json = r#"{
            "schema_version": 1, "id": "x", "name": "X", "version": "0",
            "runtime": {"exec": "x.exe"},
            "tools": [{
                "name": "translate",
                "description": "翻译",
                "danger_class": "Safe",
                "result_type": "text",
                "progress_hint": "翻译文本",
                "setting_bindings": {"target_lang": "target_lang"}
            }]
        }"#;
        let m: PluginManifest = serde_json::from_str(json).unwrap();
        let s = serde_json::to_string(&m.tools[0]).unwrap();
        let t2: ToolDef = serde_json::from_str(&s).unwrap();
        assert_eq!(t2.result_type, Some(ToolResultType::Text));
        assert_eq!(t2.progress_hint.as_deref(), Some("翻译文本"));
        assert_eq!(
            t2.setting_bindings.as_ref().unwrap().get("target_lang"),
            Some(&"target_lang".to_string())
        );
    }

    // ── 0.14: ToolDef.projection 投影规则 ────────────────────────────────

    #[test]
    fn tools_projection_defaults_to_none() {
        // 老 manifest 无 projection 字段 → None，向后兼容
        let json = r#"{
            "schema_version": 1, "id": "x", "name": "X", "version": "0",
            "runtime": {"exec": "x.exe"},
            "tools": [{ "name": "foo", "description": "bar", "danger_class": "Safe" }]
        }"#;
        let m: PluginManifest = serde_json::from_str(json).unwrap();
        assert!(m.tools[0].projection.is_none());
    }

    #[test]
    fn tools_projection_text_shape_parses() {
        // 翻译插件 manifest 投影配置示例
        let json = r#"{
            "schema_version": 1, "id": "translate", "name": "翻译", "version": "0",
            "runtime": {"exec": "main.py", "type": "python"},
            "tools": [{
                "name": "translate",
                "description": "翻译文本",
                "projection": {
                    "result_shape": "text",
                    "pointer": "$",
                    "desc": "译文",
                    "item_actions": [{"type": "copy"}]
                }
            }]
        }"#;
        let m: PluginManifest = serde_json::from_str(json).unwrap();
        let proj = m.tools[0].projection.as_ref().unwrap();
        assert!(proj.result_shape.is_some());
        assert_eq!(proj.pointer.as_deref(), Some("$"));
        assert_eq!(proj.desc.as_deref(), Some("译文"));
        assert_eq!(proj.item_actions.len(), 1);
    }

    #[test]
    fn tools_projection_items_shape_parses() {
        // IP 插件 manifest 投影配置示例
        let json = r#"{
            "schema_version": 1, "id": "ip", "name": "IP", "version": "0",
            "runtime": {"exec": "main.py", "type": "python"},
            "tools": [{
                "name": "get_ip",
                "description": "查询 IP",
                "projection": {
                    "result_shape": "items",
                    "items_pointer": "$",
                    "item_pointer": "$.ip",
                    "item_desc_pointer": "$.type",
                    "item_actions": [{"type": "copy"}]
                }
            }]
        }"#;
        let m: PluginManifest = serde_json::from_str(json).unwrap();
        let proj = m.tools[0].projection.as_ref().unwrap();
        assert_eq!(proj.items_pointer.as_deref(), Some("$"));
        assert_eq!(proj.item_pointer.as_deref(), Some("$.ip"));
        assert_eq!(proj.item_desc_pointer.as_deref(), Some("$.type"));
    }

    #[test]
    fn tools_projection_serializes_roundtrip() {
        let json = r#"{
            "schema_version": 1, "id": "x", "name": "X", "version": "0",
            "runtime": {"exec": "x.exe"},
            "tools": [{
                "name": "translate",
                "projection": {
                    "result_shape": "text",
                    "desc": "译文"
                }
            }]
        }"#;
        let m: PluginManifest = serde_json::from_str(json).unwrap();
        let s = serde_json::to_string(&m.tools[0]).unwrap();
        let t2: ToolDef = serde_json::from_str(&s).unwrap();
        assert!(t2.projection.is_some());
        assert_eq!(
            t2.projection.as_ref().unwrap().desc.as_deref(),
            Some("译文")
        );
    }
}
