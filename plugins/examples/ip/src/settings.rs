use crate::{model::Group, providers::Provider};
use serde_json::Value;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Settings {
    pub ipv6: bool,
    pub enhanced: bool,
    pub show_location: bool,
    pub fallback: bool,
    pub groups: Vec<Group>,
    pub domestic: Vec<Provider>,
    pub international: Vec<Provider>,
    pub ipv6_sources: Vec<Provider>,
}

impl Settings {
    pub fn enhanced_enabled(settings: &Value) -> bool {
        settings["enhanced_network"].as_bool().unwrap_or(false)
    }
    pub fn same_detection(&self, other: &Self) -> bool {
        self.fallback == other.fallback
            && self.groups == other.groups
            && self.domestic == other.domestic
            && self.international == other.international
            && self.ipv6_sources == other.ipv6_sources
    }
    pub fn parse(settings: &Value, arguments: &Value) -> Self {
        let boolean = |key: &str, default| settings[key].as_bool().unwrap_or(default);
        // 保留 geo_provider="none" 的离线语义，旧配置不隐式增加联网。
        let groups = match settings["geo_provider"].as_str().unwrap_or("ip-api.com") {
            "none" => vec![],
            "domestic" => vec![Group::Domestic],
            "international" => vec![Group::International],
            _ => vec![Group::Domestic, Group::International],
        };
        Self {
            ipv6: arguments["include_ipv6"]
                .as_bool()
                .unwrap_or_else(|| boolean("use_ipv6", false)),
            enhanced: Self::enhanced_enabled(settings),
            show_location: boolean("show_location", true),
            fallback: boolean("allow_fallback", true),
            groups,
            domestic: order(
                &settings["domestic_order"],
                &[Provider::Ipip, Provider::Pubyun, Provider::Oray],
            ),
            international: order(
                &settings["international_order"],
                &[Provider::Ifconfig, Provider::Ipify],
            ),
            // Old regional IPv6 orders no longer control detection. A new unified
            // key makes test-ipv6 primary even for saved pre-unification defaults.
            ipv6_sources: order(
                &settings["ipv6_order"],
                &[Provider::TestIpv6, Provider::IpifyV6],
            ),
        }
    }

    pub fn providers(&self, group: Group) -> Vec<Provider> {
        let mut result = match group {
            Group::Domestic => self.domestic.clone(),
            Group::International => self.international.clone(),
            Group::Ipv6 => self.ipv6_sources.clone(),
        };
        if !self.fallback {
            result.truncate(1);
        }
        result
    }

    pub fn detection_groups(&self) -> Vec<Group> {
        let mut groups = self.groups.clone();
        if self.ipv6 && !groups.is_empty() {
            groups.push(Group::Ipv6);
        }
        groups
    }
}

fn order(value: &Value, allowed: &[Provider]) -> Vec<Provider> {
    let tokens: Vec<&str> = match value {
        Value::Array(values) => values.iter().filter_map(Value::as_str).collect(),
        Value::String(text) => text.split(',').map(str::trim).collect(),
        _ => Vec::new(),
    };
    let mut result = Vec::new();
    for token in tokens {
        if let Some(provider) = allowed.iter().find(|p| p.id() == token) {
            if !result.contains(provider) {
                result.push(*provider);
            }
        }
    }
    // 排序不是删除；缺失的有效源排在末尾，国内/国际源不能串组。
    for provider in allowed {
        if !result.contains(provider) {
            result.push(*provider);
        }
    }
    result
}
