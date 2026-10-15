//! 地点搜索的纯逻辑：参数规范化与安全的自动选择判定。

use crate::model::ResolvedLocation;
use crate::protocol::PluginError;

pub mod coordinates;
pub mod open_meteo;
pub mod qweather;

const MAX_QUERY_CHARS: usize = 200;
const MAX_LIMIT: usize = 20;

#[derive(Clone)]
pub struct LocationQuery {
    pub query: String,
    pub country_code: Option<String>,
    pub admin: Option<String>,
    pub limit: usize,
}

/// Parse a user location query and validate all explicit geographic constraints.
///
/// `admin/name` is accepted as a concise form for an Open-Meteo admin1 filter.
pub fn parse_query(
    query: &str,
    country_code: Option<&str>,
    admin: Option<&str>,
    limit: usize,
) -> Result<LocationQuery, PluginError> {
    if !(1..=MAX_LIMIT).contains(&limit) {
        return Err(invalid_arguments("地点候选数量必须在 1 到 20 之间"));
    }

    let query = query.trim();
    let explicit_admin = match admin {
        Some(value) => {
            let value = value.trim();
            if value.is_empty() || value.chars().count() > MAX_QUERY_CHARS || value.contains(',') {
                return Err(invalid_arguments("行政区限定无效"));
            }
            Some(value)
        }
        None => None,
    };

    let (name, inline_admin) = if query.contains('/') {
        let Some((inline_admin, name)) = query.split_once('/') else {
            unreachable!("contains('/') guarantees split_once succeeds");
        };
        if name.contains('/') {
            return Err(invalid_arguments("地点限定格式应为“行政区/地点”"));
        }
        let inline_admin = inline_admin.trim();
        if inline_admin.is_empty()
            || inline_admin.chars().count() > MAX_QUERY_CHARS
            || inline_admin.contains(',')
        {
            return Err(invalid_arguments("行政区限定无效"));
        }
        (name.trim(), Some(inline_admin))
    } else {
        (query, None)
    };

    let name_len = name.chars().count();
    if name.contains(',') {
        return Err(invalid_arguments("地点名称不能包含用于分隔行政区的逗号"));
    }
    if !(2..=MAX_QUERY_CHARS).contains(&name_len) {
        return Err(invalid_arguments("地点名称长度必须在 2 到 200 个字符之间"));
    }

    if let (Some(inline), Some(explicit)) = (inline_admin, explicit_admin) {
        if !same_place_name(inline, explicit) {
            return Err(invalid_arguments("地点中的行政区限定与 admin 参数不一致"));
        }
    }

    let country_code = match country_code {
        Some(value) if value.len() == 2 && value.bytes().all(|b| b.is_ascii_alphabetic()) => {
            Some(value.to_ascii_uppercase())
        }
        Some(_) => return Err(invalid_arguments("国家代码必须是两位 ASCII 字母")),
        None => None,
    };

    Ok(LocationQuery {
        query: name.to_owned(),
        country_code,
        admin: inline_admin.or(explicit_admin).map(str::to_owned),
        limit,
    })
}

/// Automatically choose only one uniquely returned candidate whose name exactly matches.
///
/// Open-Meteo's prefix search is useful for suggestions, but a prefix result is not a
/// sufficient basis for fetching weather without the user choosing a location.
pub fn can_auto_resolve(query: &LocationQuery, candidates: &[ResolvedLocation]) -> bool {
    // A one-item result limit can truncate an ambiguous provider response, so it cannot
    // establish uniqueness even if the returned candidate happens to be an exact match.
    if query.limit < 2 || candidates.len() != 1 {
        return false;
    }

    let candidate = &candidates[0];
    if !same_text(&candidate.name, &query.query) {
        return false;
    }
    if let Some(country_code) = query.country_code.as_deref() {
        if !candidate
            .country_code
            .as_deref()
            .is_some_and(|actual| same_text(actual, country_code))
        {
            return false;
        }
    }
    if let Some(admin) = query.admin.as_deref() {
        if !candidate
            .administrative_areas
            .iter()
            .any(|area| same_place_name(area, admin))
            && !same_place_name(&candidate.administrative_area, admin)
        {
            return false;
        }
    }
    true
}

/// Compare place names after conservative normalization: case and whitespace are ignored,
/// and one common Chinese administrative suffix may be omitted. This does not do prefix or
/// transliteration matching.
pub fn same_place_name(left: &str, right: &str) -> bool {
    fn normalize(value: &str) -> String {
        let mut normalized = value
            .chars()
            .filter(|character| !character.is_whitespace())
            .flat_map(char::to_lowercase)
            .collect::<String>();

        // These are complete administrative names, not simple suffix variants.
        if !normalized.ends_with("自治区") && !normalized.ends_with("自治县") {
            if matches!(normalized.chars().last(), Some('省' | '市' | '区' | '县')) {
                normalized.pop();
            }
        }
        normalized
    }

    let left = normalize(left);
    let right = normalize(right);
    !left.is_empty() && left == right
}

fn same_text(left: &str, right: &str) -> bool {
    left.trim().eq_ignore_ascii_case(right.trim())
}

fn invalid_arguments(message: &str) -> PluginError {
    PluginError {
        code: "invalid_arguments".into(),
        message: message.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(name: &str) -> ResolvedLocation {
        ResolvedLocation {
            name: name.into(),
            display_name: format!("{name} · 辽宁 · 中国"),
            latitude: 41.8,
            longitude: 123.4,
            coordinate_system: "wgs84",
            administrative_area: "辽宁".into(),
            country: "中国".into(),
            country_code: Some("CN".into()),
            administrative_areas: vec!["辽宁".into()],
            timezone: Some("Asia/Shanghai".into()),
            kind: "city_center",
            source: "open_meteo",
            source_id: Some("open_meteo:1".into()),
            resolved_at: "2026-10-15T00:00:00Z".into(),
            in_mainland_china: Some(true),
        }
    }

    #[test]
    fn parses_inline_admin_and_normalizes_country_code() {
        let query = parse_query(" 辽宁 / 朝阳 ", Some("cn"), None, 5).unwrap();
        assert_eq!(query.query, "朝阳");
        assert_eq!(query.admin.as_deref(), Some("辽宁"));
        assert_eq!(query.country_code.as_deref(), Some("CN"));
        assert_eq!(query.limit, 5);
    }

    #[test]
    fn conflicting_or_malformed_admin_constraints_are_rejected() {
        assert_eq!(
            parse_query("辽宁/朝阳", None, Some("吉林"), 5)
                .err()
                .unwrap()
                .code,
            "invalid_arguments"
        );
        assert_eq!(
            parse_query("辽宁//朝阳", None, None, 5).err().unwrap().code,
            "invalid_arguments"
        );
    }

    #[test]
    fn validates_unicode_name_length_country_and_limit() {
        assert!(parse_query("北京", Some("CN"), None, 1).is_ok());
        assert_eq!(
            parse_query("京", None, None, 5).err().unwrap().code,
            "invalid_arguments"
        );
        assert_eq!(
            parse_query("北京", Some("中A"), None, 5)
                .err()
                .unwrap()
                .code,
            "invalid_arguments"
        );
        assert_eq!(
            parse_query("北京", None, None, 21).err().unwrap().code,
            "invalid_arguments"
        );
        assert_eq!(
            parse_query(&"市".repeat(201), None, None, 5)
                .err()
                .unwrap()
                .code,
            "invalid_arguments"
        );
    }

    #[test]
    fn only_a_unique_exact_name_with_matching_constraints_auto_resolves() {
        let query = parse_query("朝阳", Some("CN"), Some("辽宁"), 5).unwrap();
        assert!(can_auto_resolve(&query, &[candidate("朝阳")]));
        assert!(!can_auto_resolve(&query, &[candidate("朝阳区")]));
        let single_limit = parse_query("朝阳", Some("CN"), Some("辽宁"), 1).unwrap();
        assert!(!can_auto_resolve(&single_limit, &[candidate("朝阳")]));
        assert!(!can_auto_resolve(
            &query,
            &[candidate("朝阳"), candidate("朝阳")]
        ));

        let mut wrong_country = candidate("朝阳");
        wrong_country.country_code = Some("US".into());
        assert!(!can_auto_resolve(&query, &[wrong_country]));
        let mut wrong_admin = candidate("朝阳");
        wrong_admin.administrative_area = "吉林".into();
        wrong_admin.administrative_areas = vec!["吉林".into()];
        assert!(!can_auto_resolve(&query, &[wrong_admin]));
    }

    #[test]
    fn place_name_normalization_is_limited_to_case_whitespace_and_common_suffixes() {
        assert!(same_place_name("广州市", "广州"));
        assert!(same_place_name(" Tian He ", "tianhe"));
        assert!(!same_place_name("朝阳门", "朝阳"));
        assert!(!same_place_name("自治区", "自治"));
        assert!(!same_place_name(" ", ""));
    }

    #[test]
    fn automatic_resolution_accepts_a_matching_second_level_admin_and_suffix_variant() {
        let query = parse_query("广州/天河区", Some("CN"), None, 5).unwrap();
        let mut tianhe = candidate("天河区");
        tianhe.administrative_area = "广东".into();
        tianhe.administrative_areas = vec!["广东".into(), "广州市".into(), "天河区".into()];
        assert!(can_auto_resolve(&query, &[tianhe]));
    }
}
