//! 插件契约（0.28.1 §五）——从 manifest 冻结的运行时身份/能力快照。
//!
//! 契约是**发布前的校验产物**：加载/安装/更新任何插件集合前，先把 manifest
//! 冻结成 `PluginContract`，用 `ContractSet::validate` 校验身份、tool id 与
//! 关键词的全局唯一性，通过后才允许构造 `PluginHandle`、注册 Capability、
//! 注入路由规则。校验失败整个集合拒绝发布（半成品不进入执行目录）。
//!
//! 校验只看**最终形态**：tool id 按 `plugin_tool_id` 的点转下划线映射后的
//! 结果比对（`a.b`+`c` 与 `a`+`b_c` 都映射到 `a_b_c`，必须拒绝后者），
//! 不比较 manifest 原文——保持已发布 tool id 稳定的同时拦住映射碰撞。

use std::collections::{HashMap, HashSet};

use serde::Serialize;

use super::manifest::PluginManifest;
use super::plugin_tool_id;

/// plugin_id 允许的字符集：ASCII 字母/数字/点/下划线/连字符。
const ID_CHARSET: &str = "ASCII 字母/数字/`.`/`_`/`-`";
/// plugin_id 最大长度。
const ID_MAX_LEN: usize = 64;
/// 随包插件的保留命名空间前缀。非随包来源声明此前缀的 id 在安装时拒绝
/// （来源信任归 install.rs；本模块只暴露常量）。
pub const BUILTIN_NAMESPACE: &str = "builtin.";

/// 契约校验错误（thiserror，跨 install/publish 边界保留结构化字段）。
#[derive(Debug, thiserror::Error, PartialEq)]
pub enum ContractError {
    #[error("插件 id 非法: {id:?}（允许 {charset}，长度 1..={max}，不得以 . 开头/结尾）")]
    InvalidId {
        id: String,
        charset: &'static str,
        max: usize,
    },
    #[error("插件 {plugin} 需要 query/tool 进程能力但未声明 runtime.exec")]
    MissingExec { plugin: String },
    #[error("插件 id 重复: {id}（{first} 与 {second}）")]
    DuplicatePlugin {
        id: String,
        first: String,
        second: String,
    },
    #[error("tool id 碰撞: {id}（{first} 与 {second} 点转下划线后映射相同）")]
    ToolIdCollision {
        id: String,
        first: String,
        second: String,
    },
    #[error("tool id 已被既有能力占用: {id}（插件 {plugin}）")]
    ToolIdTaken { id: String, plugin: String },
    #[error("触发关键词重复: {keyword:?}（{first} 与 {second} 同时声明，新插件不得抢占既有入口）")]
    KeywordCollision {
        keyword: String,
        first: String,
        second: String,
    },
    #[error("插件 {plugin} 的页面 id 重复: {id}")]
    DuplicatePage { plugin: String, id: String },
    #[error("插件 {plugin} 的数据包 id 非法: {id:?}（允许 [a-z0-9-]，长度 1..=64）")]
    InvalidDatasetId { plugin: String, id: String },
    #[error("插件 {plugin} 的版本号不是安全目录名: {version:?}")]
    InvalidVersion { plugin: String, version: String },
}

/// 单个插件的冻结契约。
#[derive(Debug, Clone, Serialize)]
pub struct PluginContract {
    pub plugin_id: String,
    pub version: String,
    pub builtin: bool,
    pub protocol_version: u32,
    /// manifest.tools 经 `plugin_tool_id` 映射后的最终 capability id。
    pub tool_ids: Vec<String>,
    /// Keyword 触发词（小写化）——用于集合内关键词抢占校验。
    pub keywords: Vec<String>,
    /// 页面逻辑 id。
    pub page_ids: Vec<String>,
    /// 依赖的数据包 id。
    pub datasets: Vec<String>,
    /// 是否提供 query 召回。
    pub has_query: bool,
    /// 是否需要插件进程。
    pub needs_process: bool,
}

impl PluginContract {
    /// 从 manifest 冻结契约。**不**做集合级校验（那是 `ContractSet::validate` 的职责），
    /// 只做本插件自身合法性：id 字符集、进程能力必须声明 exec。
    pub fn from_manifest(m: &PluginManifest) -> Result<Self, ContractError> {
        Self::from_manifest_checked(m, m.builtin)
    }

    /// 同 [`PluginContract::from_manifest`]，但允许调用方覆写 builtin 信任标记——
    /// 安装来源判定（install.rs）用：随包目录加载的 manifest 视为 builtin，
    /// 用户目录安装的即使 manifest 自称 builtin 也按非 builtin 校验（不授保留命名空间）。
    pub fn from_manifest_checked(m: &PluginManifest, trusted_builtin: bool) -> Result<Self, ContractError> {
        validate_plugin_id(&m.id)?;
        if !valid_package_version(&m.version) {
            return Err(ContractError::InvalidVersion { plugin: m.id.clone(), version: m.version.clone() });
        }
        let needs_process = m.needs_process();
        if needs_process && m.runtime.exec.trim().is_empty() {
            return Err(ContractError::MissingExec {
                plugin: m.id.clone(),
            });
        }
        let tool_ids = m
            .tools
            .iter()
            .map(|t| plugin_tool_id(&m.id, &t.name))
            .collect();
        let keywords = m
            .triggers
            .iter()
            .filter_map(|t| match t {
                super::PluginTrigger::Keyword { keyword, .. } => {
                    Some(keyword.trim().to_lowercase())
                }
                _ => None,
            })
            .collect();
        let mut page_ids: Vec<String> = Vec::with_capacity(m.pages.len());
        for page in &m.pages {
            if page_ids.iter().any(|id| id == &page.id) {
                return Err(ContractError::DuplicatePage {
                    plugin: m.id.clone(),
                    id: page.id.clone(),
                });
            }
            page_ids.push(page.id.clone());
        }
        // 0.28.4：数据包 id 字符集校验——readiness/安装路径用 `datasets/<pid>/<did>`
        // 拼接，id 含路径分隔符/点会逃逸挂载目录（与 dataset.json 侧同一字符集）。
        for def in &m.datasets {
            let valid = !def.id.is_empty()
                && def.id.len() <= 64
                && def
                    .id
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
            if !valid {
                return Err(ContractError::InvalidDatasetId {
                    plugin: m.id.clone(),
                    id: def.id.clone(),
                });
            }
        }
        Ok(PluginContract {
            plugin_id: m.id.clone(),
            version: m.version.clone(),
            builtin: trusted_builtin,
            protocol_version: m.protocol_version,
            tool_ids,
            keywords,
            page_ids,
            datasets: m.datasets.iter().map(|d| d.id.clone()).collect(),
            has_query: m.supports_query(),
            needs_process,
        })
    }
}

/// plugin_id 合法性：非空、字符集、长度、不以 `.` 开头/结尾（空段）。
pub fn validate_plugin_id(id: &str) -> Result<(), ContractError> {
    let valid = !id.is_empty()
        && id.len() <= ID_MAX_LEN
        && !id.starts_with('.')
        && !id.ends_with('.')
        && id.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-');
    if valid {
        Ok(())
    } else {
        Err(ContractError::InvalidId {
            id: id.to_string(),
            charset: ID_CHARSET,
            max: ID_MAX_LEN,
        })
    }
}

/// 包版本参与目录拼接，必须是单个普通路径段，不能解释为当前/父目录。
pub(super) fn valid_package_version(version: &str) -> bool {
    !version.is_empty() && version.len() <= 64
        && version.as_bytes()[0].is_ascii_alphanumeric()
        && !version.ends_with('.')
        && version.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
}

/// 非随包来源不得占用 `builtin.` 保留命名空间。
pub fn validate_namespace(id: &str, trusted_builtin: bool) -> Result<(), ContractError> {
    if !trusted_builtin && id.starts_with(BUILTIN_NAMESPACE) {
        return Err(ContractError::InvalidId {
            id: id.to_string(),
            charset: ID_CHARSET,
            max: ID_MAX_LEN,
        });
    }
    Ok(())
}

/// 集合级校验（0.28.1 §五.1：拒绝最终 tool id/关键词碰撞）。
///
/// - `contracts`：本次要一起发布的全部插件契约。
/// - `existing_tool_ids`：发布时已存在的 capability id（builtin Capability
///   经 inventory 注册 + 已注册的其他插件 tool）。发布集合不得与其碰撞。
/// - `existing_keywords`：已占用触发词 → 占用者（engine 规则/已启用插件）。
///   新插件不得抢占既有入口——冲突拒绝发布，不是先到先得静默覆盖。
///
/// 返回 Err 时**整个集合**不得发布；错误消息带双方身份便于诊断。
pub fn validate_contract_set(
    contracts: &[PluginContract],
    existing_tool_ids: &HashSet<String>,
    existing_keywords: &HashMap<String, String>,
) -> Result<(), ContractError> {
    // 1. plugin_id 唯一
    let mut seen_ids: HashMap<&str, &str> = HashMap::new();
    for c in contracts {
        if let Some(first) = seen_ids.get(c.plugin_id.as_str()) {
            return Err(ContractError::DuplicatePlugin {
                id: c.plugin_id.clone(),
                first: (*first).to_string(),
                second: c.plugin_id.clone(),
            });
        }
        seen_ids.insert(c.plugin_id.as_str(), &c.version);
    }

    // 2. tool id：集合内 + 与既有注册表
    let mut seen_tools: HashMap<&str, &str> = HashMap::new();
    for c in contracts {
        for tool_id in &c.tool_ids {
            if let Some(first_owner) = seen_tools.get(tool_id.as_str()) {
                return Err(ContractError::ToolIdCollision {
                    id: tool_id.clone(),
                    first: (*first_owner).to_string(),
                    second: c.plugin_id.clone(),
                });
            }
            if let Some(_owner) = existing_tool_ids.get(tool_id) {
                return Err(ContractError::ToolIdTaken {
                    id: tool_id.clone(),
                    plugin: c.plugin_id.clone(),
                });
            }
            seen_tools.insert(tool_id.as_str(), &c.plugin_id);
        }
    }

    // 3. 关键词：集合内互斥 + 不抢占既有入口
    let mut seen_kws: HashMap<&str, &str> = HashMap::new();
    for c in contracts {
        for kw in &c.keywords {
            if kw.is_empty() {
                continue;
            }
            if let Some(first) = seen_kws.get(kw.as_str()) {
                return Err(ContractError::KeywordCollision {
                    keyword: kw.clone(),
                    first: (*first).to_string(),
                    second: c.plugin_id.clone(),
                });
            }
            if let Some(owner) = existing_keywords.get(kw) {
                return Err(ContractError::KeywordCollision {
                    keyword: kw.clone(),
                    first: owner.clone(),
                    second: c.plugin_id.clone(),
                });
            }
            seen_kws.insert(kw.as_str(), &c.plugin_id);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::manifest::{
        DangerClassDef, HostCompat, PageDef, PluginManifest, PluginRuntime, ToolDef,
    };

    fn manifest_base(id: &str, exec: &str) -> PluginManifest {
        PluginManifest {
            schema_version: 1,
            id: id.to_string(),
            name: crate::plugin::LocalizableText::Plain("T".into()),
            version: "0.1.0".into(),
            host: None,
            protocol_version: 1,
            platforms: vec![],
            description: crate::plugin::LocalizableText::default(),
            builtin: false,
            default_enabled: true,
            runtime: PluginRuntime {
                exec: exec.into(),
                ..Default::default()
            },
            triggers: vec![],
            capabilities: vec![],
            settings_schema: vec![],
            tools: vec![],
            pages: vec![],
            datasets: vec![],
            args: None,
        }
    }

    fn manifest_with_tool(mut m: PluginManifest, tool: &str) -> PluginManifest {
        m.capabilities.push("query".into());
        m.tools.push(ToolDef {
            name: tool.into(),
            ..Default::default()
        });
        m
    }

    fn manifest_with_keyword(mut m: PluginManifest, kw: &str) -> PluginManifest {
        m.triggers.push(crate::plugin::PluginTrigger::Keyword {
            keyword: kw.into(),
            exclusive: true,
        });
        m
    }

    #[test]
    fn contract_from_v1_manifest_freezes_tool_ids() {
        let m = manifest_with_tool(manifest_base("a.b", "p.exe"), "lookup");
        let c = PluginContract::from_manifest(&m).unwrap();
        assert_eq!(c.tool_ids, vec!["a_b_lookup".to_string()]);
        assert!(c.needs_process && c.has_query);
    }

    #[test]
    fn package_versions_cannot_escape_the_version_directory() {
        for version in [".", "..", "...", "1.", "../outside", "C:/outside", "1/2", "1\\2", ""] {
            let mut manifest = manifest_base("extra.demo", "p.exe");
            manifest.version = version.into();
            assert!(matches!(PluginContract::from_manifest(&manifest), Err(ContractError::InvalidVersion { .. })));
        }
        assert!(valid_package_version("1.0.0-beta.1"));
    }

    #[test]
    fn pure_page_plugin_without_runtime_is_valid() {
        let mut m = manifest_base("extra.dict", "");
        m.pages.push(PageDef {
            id: "main".into(),
            entry: "index.html".into(),
        });
        let c = PluginContract::from_manifest(&m).unwrap();
        assert!(!c.needs_process);
        assert_eq!(c.page_ids, vec!["main".to_string()]);
    }

    #[test]
    fn process_plugin_missing_exec_rejected() {
        let m = manifest_with_tool(manifest_base("a.b", "  "), "t");
        assert!(matches!(
            PluginContract::from_manifest(&m),
            Err(ContractError::MissingExec { .. })
        ));
    }

    #[test]
    fn invalid_plugin_ids_rejected() {
        for bad in ["", ".a.b", "a.b.", "a b", "a/b", &"x".repeat(65)] {
            assert!(
                validate_plugin_id(bad).is_err(),
                "id {bad:?} 应被拒绝"
            );
        }
        assert!(validate_plugin_id("builtin.translate").is_ok());
        assert!(validate_plugin_id("extra.dict-v2").is_ok());
    }

    #[test]
    fn builtin_namespace_reserved_for_trusted_source() {
        assert!(validate_namespace("builtin.x", true).is_ok());
        assert!(validate_namespace("builtin.x", false).is_err());
        assert!(validate_namespace("extra.x", false).is_ok());
    }

    #[test]
    fn sanitized_tool_id_collision_detected() {
        // a.b + c 与 a + b_c 映射后同为 a_b_c
        let m1 = manifest_with_tool(manifest_base("a.b", "p1.exe"), "c");
        let m2 = manifest_with_tool(manifest_base("a", "p2.exe"), "b_c");
        let c1 = PluginContract::from_manifest(&m1).unwrap();
        let c2 = PluginContract::from_manifest(&m2).unwrap();
        let err = validate_contract_set(&[c1, c2], &HashSet::new(), &HashMap::new());
        assert!(matches!(
            err,
            Err(ContractError::ToolIdCollision { id, .. }) if id == "a_b_c"
        ));
    }

    #[test]
    fn tool_id_taken_by_existing_registry_rejected() {
        let m = manifest_with_tool(manifest_base("p", "p.exe"), "t");
        let c = PluginContract::from_manifest(&m).unwrap();
        let mut existing = HashSet::new();
        existing.insert("p_t".to_string());
        assert!(matches!(
            validate_contract_set(&[c], &existing, &HashMap::new()),
            Err(ContractError::ToolIdTaken { .. })
        ));
    }

    #[test]
    fn duplicate_plugin_id_and_keyword_rejected() {
        let m1 = manifest_with_keyword(manifest_base("p.one", "a.exe"), "查词");
        let m2 = manifest_with_keyword(manifest_base("p.one", "b.exe"), "翻译");
        let err = validate_contract_set(
            &[PluginContract::from_manifest(&m1).unwrap(), PluginContract::from_manifest(&m2).unwrap()],
            &HashSet::new(),
            &HashMap::new(),
        );
        assert!(matches!(err, Err(ContractError::DuplicatePlugin { .. })));

        let m3 = manifest_with_keyword(manifest_base("p.two", "c.exe"), "翻译");
        let err = validate_contract_set(
            &[PluginContract::from_manifest(&m2).unwrap(), PluginContract::from_manifest(&m3).unwrap()],
            &HashSet::new(),
            &HashMap::new(),
        );
        assert!(matches!(err, Err(ContractError::KeywordCollision { keyword, .. }) if keyword == "翻译"));

        // 既有入口被新插件抢占 → 拒绝
        let m4 = manifest_with_keyword(manifest_base("p.three", "d.exe"), "翻译");
        let mut existing_kw = HashMap::new();
        existing_kw.insert("翻译".to_string(), "builtin.translate".to_string());
        assert!(matches!(
            validate_contract_set(&[PluginContract::from_manifest(&m4).unwrap()], &HashSet::new(), &existing_kw),
            Err(ContractError::KeywordCollision { first, .. }) if first == "builtin.translate"
        ));
    }

    #[test]
    fn duplicate_page_id_within_plugin_rejected() {
        let mut m = manifest_base("extra.dict", "");
        m.pages.push(PageDef { id: "main".into(), entry: "a.html".into() });
        m.pages.push(PageDef { id: "main".into(), entry: "b.html".into() });
        assert!(matches!(
            PluginContract::from_manifest(&m),
            Err(ContractError::DuplicatePage { .. })
        ));
    }

    #[test]
    fn host_compat_and_dataset_fields_parse() {
        // 字段可解析（serde 层），契约透传 datasets
        let mut m = manifest_base("extra.dict", "");
        m.host = Some(HostCompat {
            min_version: Some("0.28.0".into()),
            max_version: None,
        });
        m.datasets.push(crate::plugin::manifest::DatasetDef {
            id: "ecdict-basic".into(),
            min_version: Some("1.0.0".into()),
            optional: false,
        });
        let c = PluginContract::from_manifest(&m).unwrap();
        assert_eq!(c.datasets, vec!["ecdict-basic".to_string()]);
    }

    #[test]
    fn danger_class_default_matches_legacy() {
        // 老 manifest 无 danger_class → Safe（serde default 不变）
        let m = manifest_with_tool(manifest_base("p", "p.exe"), "t");
        assert_eq!(m.tools[0].danger_class, DangerClassDef::Safe);
    }
}
