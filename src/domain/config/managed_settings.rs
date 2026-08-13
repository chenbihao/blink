//! AI 可管理设置的稳定白名单协议（0.19.8）。
//!
//! 这里只定义公开 id、元数据和纯值校验；不暴露底层 KV key 或完整配置分片。

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManagedSettingId {
    Theme,
    WindowOpacity,
    SearchHistoryEnabled,
    SearchHistoryDays,
    SearchMaxResults,
    SearchPageSize,
    AutosuggestEnabled,
    ClipboardEnabled,
    ClipboardRetentionDays,
    ClipboardMaxItems,
    ClipboardDisplayCount,
    ClipboardDisplayPages,
    ClipboardCandidateLimit,
}

impl ManagedSettingId {
    pub const ALL: [Self; 13] = [
        Self::Theme,
        Self::WindowOpacity,
        Self::SearchHistoryEnabled,
        Self::SearchHistoryDays,
        Self::SearchMaxResults,
        Self::SearchPageSize,
        Self::AutosuggestEnabled,
        Self::ClipboardEnabled,
        Self::ClipboardRetentionDays,
        Self::ClipboardMaxItems,
        Self::ClipboardDisplayCount,
        Self::ClipboardDisplayPages,
        Self::ClipboardCandidateLimit,
    ];

    pub fn parse(id: &str) -> Option<Self> {
        Some(match id {
            "appearance.theme" => Self::Theme,
            "appearance.window_opacity" => Self::WindowOpacity,
            "search.history_enabled" => Self::SearchHistoryEnabled,
            "search.history_days" => Self::SearchHistoryDays,
            "search.max_results" => Self::SearchMaxResults,
            "search.page_size" => Self::SearchPageSize,
            "suggestion.autosuggest_enabled" => Self::AutosuggestEnabled,
            "clipboard.enabled" => Self::ClipboardEnabled,
            "clipboard.retention_days" => Self::ClipboardRetentionDays,
            "clipboard.max_items" => Self::ClipboardMaxItems,
            "clipboard.display_count" => Self::ClipboardDisplayCount,
            "clipboard.display_pages" => Self::ClipboardDisplayPages,
            "clipboard.candidate_limit" => Self::ClipboardCandidateLimit,
            _ => return None,
        })
    }

    pub fn id(self) -> &'static str {
        match self {
            Self::Theme => "appearance.theme",
            Self::WindowOpacity => "appearance.window_opacity",
            Self::SearchHistoryEnabled => "search.history_enabled",
            Self::SearchHistoryDays => "search.history_days",
            Self::SearchMaxResults => "search.max_results",
            Self::SearchPageSize => "search.page_size",
            Self::AutosuggestEnabled => "suggestion.autosuggest_enabled",
            Self::ClipboardEnabled => "clipboard.enabled",
            Self::ClipboardRetentionDays => "clipboard.retention_days",
            Self::ClipboardMaxItems => "clipboard.max_items",
            Self::ClipboardDisplayCount => "clipboard.display_count",
            Self::ClipboardDisplayPages => "clipboard.display_pages",
            Self::ClipboardCandidateLimit => "clipboard.candidate_limit",
        }
    }

    pub fn descriptor(self, current_value: Value) -> ManagedSetting {
        let (value_type, minimum, maximum, enum_values, description) = match self {
            Self::Theme => (
                "enum",
                None,
                None,
                Some(vec!["auto".into(), "light".into(), "dark".into()]),
                "界面主题；首版自然语言设置只允许跟随系统、浅色或深色",
            ),
            Self::WindowOpacity => (
                "number",
                Some(0.2),
                Some(1.0),
                None,
                "主窗口透明度，0.2 到 1.0",
            ),
            Self::SearchHistoryEnabled => ("boolean", None, None, None, "是否记录搜索历史"),
            Self::SearchHistoryDays => (
                "integer",
                Some(0.0),
                Some(365.0),
                None,
                "搜索历史保留天数，0 表示不按天数清理",
            ),
            Self::SearchMaxResults => (
                "integer",
                Some(1.0),
                Some(100.0),
                None,
                "一次搜索最多保留的候选结果数",
            ),
            Self::SearchPageSize => ("integer", Some(1.0), Some(20.0), None, "搜索结果每页条数"),
            Self::AutosuggestEnabled => ("boolean", None, None, None, "是否启用输入补全建议"),
            Self::ClipboardEnabled => ("boolean", None, None, None, "是否启用剪贴板历史监听"),
            Self::ClipboardRetentionDays => (
                "integer",
                Some(0.0),
                Some(3650.0),
                None,
                "剪贴板历史保留天数，0 表示永久",
            ),
            Self::ClipboardMaxItems => (
                "integer",
                Some(10.0),
                Some(5000.0),
                None,
                "剪贴板文本历史最大保留条数",
            ),
            Self::ClipboardDisplayCount => (
                "integer",
                Some(1.0),
                Some(200.0),
                None,
                "（已废弃，请使用 clipboard.display_pages）旧字段：单次展示条数",
            ),
            Self::ClipboardDisplayPages => (
                "integer",
                Some(1.0),
                Some(20.0),
                None,
                "剪贴板模式一次加载几页（每页条数由 search.page_size 控制）",
            ),
            Self::ClipboardCandidateLimit => (
                "integer",
                Some(50.0),
                Some(5000.0),
                None,
                "搜索候选池上限，控制 fuzzy 匹配时加载多少条元数据",
            ),
        };
        ManagedSetting {
            id: self.id().into(),
            value_type: value_type.into(),
            minimum,
            maximum,
            enum_values,
            current_value,
            description: description.into(),
            requires_restart: false,
        }
    }

    pub fn validate(self, value: &Value) -> Result<(), String> {
        match self {
            Self::Theme => match value.as_str() {
                Some("auto" | "light" | "dark") => Ok(()),
                _ => Err("主题只允许 auto、light 或 dark".into()),
            },
            Self::WindowOpacity => validate_number(value, 0.2, 1.0),
            Self::SearchHistoryEnabled | Self::AutosuggestEnabled | Self::ClipboardEnabled => {
                if value.is_boolean() {
                    Ok(())
                } else {
                    Err("值必须是 boolean".into())
                }
            }
            Self::SearchHistoryDays => validate_integer(value, 0, 365),
            Self::SearchMaxResults => validate_integer(value, 1, 100),
            Self::SearchPageSize => validate_integer(value, 1, 20),
            Self::ClipboardRetentionDays => validate_integer(value, 0, 3650),
            Self::ClipboardMaxItems => validate_integer(value, 10, 5000),
            Self::ClipboardDisplayCount => validate_integer(value, 1, 200),
            Self::ClipboardDisplayPages => validate_integer(value, 1, 20),
            Self::ClipboardCandidateLimit => validate_integer(value, 50, 5000),
        }
    }
}

fn validate_number(value: &Value, min: f64, max: f64) -> Result<(), String> {
    let number = value.as_f64().ok_or("值必须是 number")?;
    if number.is_finite() && (min..=max).contains(&number) {
        Ok(())
    } else {
        Err(format!("值必须在 {min} 到 {max} 之间"))
    }
}

fn validate_integer(value: &Value, min: u64, max: u64) -> Result<(), String> {
    let number = value.as_u64().ok_or("值必须是非负 integer")?;
    if (min..=max).contains(&number) {
        Ok(())
    } else {
        Err(format!("值必须在 {min} 到 {max} 之间"))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ManagedSetting {
    pub id: String,
    #[serde(rename = "type")]
    pub value_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub minimum: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub maximum: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enum_values: Option<Vec<String>>,
    pub current_value: Value,
    pub description: String,
    pub requires_restart: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ManagedSettingUpdate {
    pub setting_id: String,
    pub old_value: Value,
    pub new_value: Value,
    pub immediately_effective: bool,
    pub requires_restart: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn whitelist_ids_round_trip_and_are_unique() {
        let mut ids = std::collections::HashSet::new();
        for setting in ManagedSettingId::ALL {
            assert_eq!(ManagedSettingId::parse(setting.id()), Some(setting));
            assert!(ids.insert(setting.id()));
        }
    }

    #[test]
    fn rejects_unknown_and_out_of_range_values() {
        assert!(ManagedSettingId::parse("ai.provider.api_key").is_none());
        assert!(
            ManagedSettingId::WindowOpacity
                .validate(&json!(0.19))
                .is_err()
        );
        assert!(
            ManagedSettingId::SearchPageSize
                .validate(&json!(21))
                .is_err()
        );
        assert!(
            ManagedSettingId::Theme
                .validate(&json!("secret-theme"))
                .is_err()
        );
    }

    #[test]
    fn descriptor_exposes_public_metadata_only() {
        let item = ManagedSettingId::SearchMaxResults.descriptor(json!(50));
        let value = serde_json::to_value(item).unwrap();
        assert_eq!(value["id"], "search.max_results");
        assert_eq!(value["type"], "integer");
        assert!(value.get("key").is_none());
        assert!(value.get("secret").is_none());
    }
}
