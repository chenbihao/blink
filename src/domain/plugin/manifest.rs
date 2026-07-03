//! 插件 manifest(JSON,见 production-design/phases/0.2-core-plugin-design.md §3.3)。
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
    pub runtime: PluginRuntime,
    #[serde(default)]
    pub triggers: Vec<PluginTrigger>,
    #[serde(default)]
    pub capabilities: Vec<String>,
    /// 配置项元数据声明(驱动设置页 UI 渲染)。缺失则该插件用裸 JSON 编辑(降级)。
    #[serde(default)]
    pub settings_schema: Vec<SettingField>,
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
}

/// 触发器。本切片只实现 keyword(精确/前缀);regex 先定义不实现。
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
}

fn default_exclusive() -> bool {
    true
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
                        let opt: SettingOption = serde_json::from_value(val)
                            .map_err(de::Error::custom)?;
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
        assert!(matches!(t, PluginTrigger::Keyword { exclusive: true, .. }));
    }

    #[test]
    fn missing_timeout_defaults_3000() {
        let json = r#"{"schema_version":1,"id":"x","name":"X","version":"0","runtime":{"exec":"x.exe"}}"#;
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
        let json = r#"{"schema_version":1,"id":"x","name":"X","version":"0","runtime":{"exec":"x.exe"}}"#;
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
        assert_eq!(m.settings_schema[0].options[0].label.resolve("zh"), "youdao");
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
            "exec_path 结果不应带 \\\\?\\ 前缀，实际={}", got.display(),
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
}
