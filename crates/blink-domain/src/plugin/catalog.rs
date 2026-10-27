//! 官方插件目录（0.28.5 §五.5）——catalog.json 的解析与静态校验。
//!
//! 信任模型：目录端点固定为官方发布源（HTTPS，见 app 层 plugin_center），
//! 目录内容即官方分发意图；条目资产的真实性由下载侧 sha256 核验兜底
//! （与 manifest/dataset.json 的逐文件校验双保险）。本模块只做纯解析与
//! 校验，不发网络请求——拉取/缓存在 app 层，下载在 infra net 之上。
//!
//! 校验纪律（phase 验收"不可信/损坏/错误架构/不兼容包拒绝"）：
//! - `format_version` 高于本宿主支持 → 拒绝整个目录（提示升级，不降级猜）；
//! - 条目 URL 强制 HTTPS、sha256 必须 64 位十六进制、字节数 > 0；
//! - plugin_id/dataset_id/版本字符集与安装状态机同一套约束；
//! - 重复 plugin_id / 重复 (plugin_id, dataset_id) 拒绝——目录是构建产物，
//!   重复即构建错误，不允许"后者覆盖前者"的静默仲裁。

use serde::{Deserialize, Serialize};

use super::contract::validate_plugin_id;
use super::install::compare_versions;

/// catalog.json 的格式版本上限（0.28.5 首发 = 1）。
pub const SUPPORTED_CATALOG_FORMAT_VERSION: u32 = 1;

/// 目录静态校验错误（不涉及网络/下载）。
#[derive(Debug, thiserror::Error)]
pub enum CatalogError {
    #[error("目录 JSON 非法: {0}")]
    InvalidJson(String),
    #[error("目录格式版本 {found} 高于本宿主支持（最高 {supported}），请升级应用")]
    FormatTooNew { found: u32, supported: u32 },
    #[error("目录条目非法（{plugin}）: {reason}")]
    BadEntry { plugin: String, reason: String },
}

/// 资产描述（zip 下载源 + 完整性声明）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CatalogAsset {
    /// 下载 URL（必须 HTTPS）。
    pub url: String,
    /// 归档 sha256（64 位十六进制，统一小写比较）。
    pub sha256: String,
    /// 归档字节数（下载侧超量即中止——声明总量同时是进度分母与写入上限）。
    pub bytes: u64,
}

/// 插件目录条目。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CatalogPluginEntry {
    pub plugin_id: String,
    pub version: String,
    /// 宿主最低版本（"0" = 不限制）。下载前预检；包内 manifest 仍是最终真源。
    #[serde(default = "default_host_min")]
    pub host_min_version: String,
    /// 适用平台标签（空 = 任意平台）。
    #[serde(default)]
    pub platforms: Vec<String>,
    pub asset: CatalogAsset,
    // ── 展示元数据（0.28.12）：全部可选，旧目录缺省 = 前端按约定回退 ──
    /// 展示名称（双语）。缺省回退 plugin_id；旧目录无此字段为 None。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<super::manifest::LocalizableText>,
    /// 展示描述（双语）。缺省不显示描述行。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<super::manifest::LocalizableText>,
    /// 是否声明独立页面。旧目录为 None = 未知——前端不显示成"无页面"。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub has_pages: Option<bool>,
    /// 发布通道（0.28.12）。缺省 stable；非法值整个目录拒绝（构建产物不猜意图）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel: Option<super::manifest::PluginChannel>,
    /// 发布更新时间（RFC3339）。仅在有可靠发布时间时填写——本机 pack 不把
    /// 文件修改时间当发布时间，缺省 None。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
}

fn default_host_min() -> String {
    "0".to_string()
}

/// 数据包（词库）目录条目。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CatalogDatasetEntry {
    /// 归属插件 id（挂载命名空间，与 dataset.json 声明一致）。
    pub plugin_id: String,
    pub dataset_id: String,
    pub version: String,
    pub asset: CatalogAsset,
}

/// 官方插件目录。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PluginCatalog {
    pub format_version: u32,
    /// 构建时间戳（RFC3339 字符串，仅展示，不参与判定）。
    #[serde(default)]
    pub generated_at: String,
    #[serde(default)]
    pub plugins: Vec<CatalogPluginEntry>,
    #[serde(default)]
    pub datasets: Vec<CatalogDatasetEntry>,
}

impl PluginCatalog {
    pub fn plugin(&self, plugin_id: &str) -> Option<&CatalogPluginEntry> {
        self.plugins.iter().find(|e| e.plugin_id == plugin_id)
    }

    pub fn dataset(&self, plugin_id: &str, dataset_id: &str) -> Option<&CatalogDatasetEntry> {
        self.datasets
            .iter()
            .find(|e| e.plugin_id == plugin_id && e.dataset_id == dataset_id)
    }
}

/// 目录条目兼容性预检（下载前快速拒绝；安装期 manifest 校验仍独立执行）。
#[derive(Debug, Clone, PartialEq)]
pub enum CatalogCompat {
    Ok,
    HostIncompatible { min: String },
    PlatformMismatch { declared: Vec<String> },
}

pub fn plugin_entry_compat(
    entry: &CatalogPluginEntry,
    host_version: &str,
    platform: &str,
) -> CatalogCompat {
    if compare_versions(host_version, &entry.host_min_version) == std::cmp::Ordering::Less {
        return CatalogCompat::HostIncompatible {
            min: entry.host_min_version.clone(),
        };
    }
    if !entry.platforms.is_empty() && !entry.platforms.iter().any(|p| p == platform) {
        return CatalogCompat::PlatformMismatch {
            declared: entry.platforms.clone(),
        };
    }
    CatalogCompat::Ok
}

/// 解析并静态校验目录文本。
pub fn parse_catalog(raw: &str) -> Result<PluginCatalog, CatalogError> {
    let catalog: PluginCatalog =
        serde_json::from_str(raw).map_err(|e| CatalogError::InvalidJson(e.to_string()))?;
    if catalog.format_version > SUPPORTED_CATALOG_FORMAT_VERSION {
        return Err(CatalogError::FormatTooNew {
            found: catalog.format_version,
            supported: SUPPORTED_CATALOG_FORMAT_VERSION,
        });
    }

    let mut seen_plugins = std::collections::HashSet::new();
    for entry in &catalog.plugins {
        let ctx = |reason: &str| CatalogError::BadEntry {
            plugin: entry.plugin_id.clone(),
            reason: reason.to_string(),
        };
        validate_plugin_id(&entry.plugin_id)
            .map_err(|_| ctx("plugin_id 非法"))?;
        if !seen_plugins.insert(entry.plugin_id.clone()) {
            return Err(CatalogError::BadEntry {
                plugin: entry.plugin_id.clone(),
                reason: "plugin_id 重复".into(),
            });
        }
        if !valid_version(&entry.version) {
            return Err(ctx("version 字符集非法"));
        }
        if !valid_version(&entry.host_min_version) {
            return Err(ctx("host_min_version 字符集非法"));
        }
        if entry.platforms.iter().any(|p| p.is_empty()) {
            return Err(ctx("platforms 含空标签"));
        }
        // 展示字段（0.28.12）：updated_at 只做长度/字符集约束（RFC3339 由 pack 侧
        // 保证）；name/description/channel 的结构由 serde 类型保证——非法 channel
        // 值在反序列化阶段即整个目录拒绝。
        if let Some(ts) = &entry.updated_at
            && (ts.is_empty() || ts.len() > 64 || !ts.chars().all(|c| c.is_ascii_graphic()))
        {
            return Err(ctx("updated_at 字符集非法"));
        }
        validate_asset(&entry.asset).map_err(|r| ctx(&r))?;
    }

    let mut seen_datasets = std::collections::HashSet::new();
    for entry in &catalog.datasets {
        let ctx = |reason: &str| CatalogError::BadEntry {
            plugin: format!("{}/{}", entry.plugin_id, entry.dataset_id),
            reason: reason.to_string(),
        };
        validate_plugin_id(&entry.plugin_id).map_err(|_| ctx("plugin_id 非法"))?;
        if !valid_dataset_id(&entry.dataset_id) {
            return Err(ctx("dataset_id 字符集非法（[a-z0-9-]）"));
        }
        if !valid_version(&entry.version) {
            return Err(ctx("version 字符集非法"));
        }
        if !seen_datasets.insert((entry.plugin_id.clone(), entry.dataset_id.clone())) {
            return Err(CatalogError::BadEntry {
                plugin: format!("{}/{}", entry.plugin_id, entry.dataset_id),
                reason: "dataset 条目重复".into(),
            });
        }
        validate_asset(&entry.asset).map_err(|r| ctx(&r))?;
    }

    Ok(catalog)
}

fn validate_asset(asset: &CatalogAsset) -> Result<(), String> {
    let url = reqwest::Url::parse(&asset.url).map_err(|_| "asset.url 非法".to_string())?;
    if url.scheme() != "https" {
        return Err("asset.url 必须为 HTTPS".into());
    }
    if asset.sha256.len() != 64 || !asset.sha256.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err("asset.sha256 必须为 64 位十六进制".into());
    }
    if asset.bytes == 0 {
        return Err("asset.bytes 必须大于 0".into());
    }
    Ok(())
}

/// 版本字符集（与 dataset.rs `valid_version_charset` 同一套：目录先行拒绝）。
fn valid_version(v: &str) -> bool {
    super::contract::valid_package_version(v)
}

/// dataset_id 字符集（与 dataset.rs 安装期校验同一套：[a-z0-9-]）。
fn valid_dataset_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::install::current_platform_tag;

    fn asset_json() -> String {
        r#""url":"https://github.com/o/r/releases/download/plugins-catalog/a.zip","sha256":"a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2","bytes":1234"#.into()
    }

    fn catalog_json(plugins: &str, datasets: &str) -> String {
        format!(
            r#"{{"format_version":1,"generated_at":"2026-10-26T00:00:00Z","plugins":[{plugins}],"datasets":[{datasets}]}}"#
        )
    }

    fn plugin_entry(id: &str) -> String {
        format!(
            r#"{{"plugin_id":"{id}","version":"1.0.0","host_min_version":"0.28.1","platforms":["windows-x86_64"],"asset":{{{}}}}}"#,
            asset_json()
        )
    }

    fn dataset_entry(pid: &str, did: &str) -> String {
        format!(
            r#"{{"plugin_id":"{pid}","dataset_id":"{did}","version":"1.0.0","asset":{{{}}}}}"#,
            asset_json()
        )
    }

    #[test]
    fn parse_catalog_accepts_valid_full_document() {
        let raw = catalog_json(
            &format!("{},{}", plugin_entry("builtin.translate"), plugin_entry("extra.demo")),
            &dataset_entry("builtin.dictionary", "dictionary-basic"),
        );
        let c = parse_catalog(&raw).unwrap();
        assert_eq!(c.format_version, 1);
        assert_eq!(c.plugins.len(), 2);
        assert_eq!(c.datasets.len(), 1);
        assert!(c.plugin("builtin.translate").is_some());
        assert!(c.dataset("builtin.dictionary", "dictionary-basic").is_some());
        assert!(c.dataset("builtin.dictionary", "nope").is_none());
    }

    // ── 0.28.12 展示元数据：新旧目录双向兼容 ─────────────────────────────

    #[test]
    fn display_metadata_parses_and_legacy_catalog_stays_compatible() {
        // 新目录：name/description 双语 + has_pages + channel + updated_at
        let entry = r#"{"plugin_id":"builtin.dictionary","version":"0.2.0",
            "host_min_version":"0.28.1","platforms":["windows-x86_64"],
            "asset":{ASSET},
            "name":{"zh":"离线词典","en":"Dictionary"},
            "description":{"zh":"离线查词","en":"Offline lookup"},
            "has_pages":true,"channel":"beta","updated_at":"2026-10-28T00:00:00Z"}"#
            .replace("ASSET", &asset_json());
        let c = parse_catalog(&catalog_json(&entry, "")).unwrap();
        let e = &c.plugins[0];
        assert_eq!(
            e.name.as_ref().unwrap().resolve("zh"),
            "离线词典"
        );
        assert_eq!(e.description.as_ref().unwrap().resolve("en"), "Offline lookup");
        assert_eq!(e.has_pages, Some(true));
        assert_eq!(e.channel, Some(super::super::manifest::PluginChannel::Beta));
        assert_eq!(e.updated_at.as_deref(), Some("2026-10-28T00:00:00Z"));

        // 旧目录（0.28.5 产物，无展示字段）：全部 None = 前端按约定回退
        let legacy = plugin_entry("extra.demo");
        let c = parse_catalog(&catalog_json(&legacy, "")).unwrap();
        let e = &c.plugins[0];
        assert!(e.name.is_none());
        assert!(e.description.is_none());
        assert_eq!(e.has_pages, None);
        assert_eq!(e.channel, None);
        assert!(e.updated_at.is_none());
    }

    #[test]
    fn display_metadata_bad_values_rejected() {
        // 未知 channel 值：整个目录拒绝（serde 类型即校验）
        let bad_channel = r#"{"plugin_id":"extra.demo","version":"1.0.0",
            "asset":{ASSET},"channel":"nightly"}"#
            .replace("ASSET", &asset_json());
        assert!(parse_catalog(&catalog_json(&bad_channel, "")).is_err());
        // updated_at 含空白（非 ascii_graphic）拒绝
        let bad_ts = r#"{"plugin_id":"extra.demo","version":"1.0.0",
            "asset":{ASSET},"updated_at":"2026-10-28 00:00"}"#
            .replace("ASSET", &asset_json());
        assert!(parse_catalog(&catalog_json(&bad_ts, "")).is_err());
    }

    #[test]
    fn parse_catalog_accepts_empty_lists() {
        let c = parse_catalog(&catalog_json("", "")).unwrap();
        assert!(c.plugins.is_empty() && c.datasets.is_empty());
    }

    #[test]
    fn format_too_new_rejected() {
        let raw = r#"{"format_version":2,"plugins":[],"datasets":[]}"#;
        assert!(matches!(
            parse_catalog(raw),
            Err(CatalogError::FormatTooNew { found: 2, .. })
        ));
    }

    #[test]
    fn http_url_and_bad_hash_rejected() {
        let base = plugin_entry("extra.demo");
        let http = base.replace("https://", "http://");
        assert!(parse_catalog(&catalog_json(&http, "")).is_err());
        let bad_hash = base.replace(
            "a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2",
            "abcd",
        );
        assert!(parse_catalog(&catalog_json(&bad_hash, "")).is_err());
    }

    #[test]
    fn zero_bytes_and_bad_ids_rejected() {
        let zero = plugin_entry("extra.demo").replace("\"bytes\":1234", "\"bytes\":0");
        assert!(parse_catalog(&catalog_json(&zero, "")).is_err());

        assert!(parse_catalog(&catalog_json(&plugin_entry("BAD id"), "")).is_err());
        assert!(
            parse_catalog(&catalog_json(
                "",
                &dataset_entry("builtin.dictionary", "Bad_ID")
            ))
            .is_err()
        );
    }

    #[test]
    fn duplicate_entries_rejected() {
        let dup = format!("{},{}", plugin_entry("extra.demo"), plugin_entry("extra.demo"));
        assert!(parse_catalog(&catalog_json(&dup, "")).is_err());
        let dup_ds = format!(
            "{},{}",
            dataset_entry("builtin.dictionary", "d"),
            dataset_entry("builtin.dictionary", "d")
        );
        assert!(parse_catalog(&catalog_json("", &dup_ds)).is_err());
    }

    #[test]
    fn compat_checks_host_and_platform() {
        let entry = parse_catalog(&catalog_json(&plugin_entry("extra.demo"), ""))
            .unwrap()
            .plugins
            .remove(0);
        assert_eq!(
            plugin_entry_compat(&entry, "0.28.5", current_platform_tag()),
            CatalogCompat::Ok
        );
        assert!(matches!(
            plugin_entry_compat(&entry, "0.27.0", current_platform_tag()),
            CatalogCompat::HostIncompatible { .. }
        ));
        assert!(matches!(
            plugin_entry_compat(&entry, "0.28.5", "linux-x86_64"),
            CatalogCompat::PlatformMismatch { .. }
        ));

        let mut any_platform = entry.clone();
        any_platform.platforms.clear();
        assert_eq!(
            plugin_entry_compat(&any_platform, "0.28.5", "linux-x86_64"),
            CatalogCompat::Ok
        );
    }
}
