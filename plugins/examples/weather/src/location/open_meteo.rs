//! Open-Meteo geocoding URL construction and response parsing.

use std::collections::HashSet;

use crate::model::ResolvedLocation;
use crate::protocol::PluginError;

use super::LocationQuery;

const API_URL: &str = "https://geocoding-api.open-meteo.com/v1/search";
const REQUEST_COUNT: usize = 100;
const MAX_CANDIDATES: usize = 20;

/// Build a bounded Open-Meteo search URL. The API count stays at 100 so provider-side truncation
/// does not hide matches before local filtering and de-duplication.
pub fn search_url(query: &LocationQuery) -> String {
    build_search_url(query, true)
}

/// Build a retry URL without the upstream admin1 qualifier. Local filtering still applies the
/// original `LocationQuery.admin` against every returned administrative level.
pub fn unqualified_search_url(query: &LocationQuery) -> String {
    build_search_url(query, false)
}

fn build_search_url(query: &LocationQuery, include_admin: bool) -> String {
    let name = match query.admin.as_deref() {
        Some(admin) if include_admin => format!("{},{}", query.query, admin),
        None => query.query.clone(),
        Some(_) => query.query.clone(),
    };
    let mut url = format!(
        "{API_URL}?name={}&count={REQUEST_COUNT}&language=zh&format=json",
        urlencoding::encode(&name)
    );
    if let Some(country_code) = query.country_code.as_deref() {
        url.push_str("&countryCode=");
        url.push_str(&urlencoding::encode(country_code));
    }
    url
}

/// Report whether Open-Meteo may have truncated the provider result set at its requested cap.
///
/// This checks the raw array before local country/admin/type filtering and result limiting, so a
/// single surviving candidate cannot be treated as unique when the API returned its full count.
/// Malformed responses are conservatively considered possibly truncated.
pub fn possibly_truncated(body: &str) -> bool {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(body) else {
        return true;
    };
    let Some(object) = value.as_object() else {
        return true;
    };

    match object.get("results") {
        None | Some(serde_json::Value::Null) => false,
        Some(serde_json::Value::Array(results)) => results.len() >= REQUEST_COUNT,
        Some(_) => true,
    }
}

/// Parse, validate, filter, and de-duplicate Open-Meteo geocoding candidates.
///
/// The returned candidate count never exceeds the validated query limit. Callers that need to
/// assess ambiguity before auto-resolving should request a limit large enough to see alternatives.
pub fn parse_candidates(
    body: &str,
    query: &LocationQuery,
) -> Result<Vec<ResolvedLocation>, PluginError> {
    let response: GeocodingResponse = serde_json::from_str(body).map_err(|_| invalid_response())?;
    if response
        .error
        .as_ref()
        .is_some_and(|value| value == &serde_json::Value::Bool(true))
    {
        return Err(invalid_response());
    }

    let return_limit = query.limit.min(MAX_CANDIDATES);
    let mut candidates = Vec::new();
    let mut seen = HashSet::new();

    for raw in response.results.unwrap_or_default() {
        let Some(candidate) = convert_candidate(raw) else {
            continue;
        };

        if let Some(country_code) = query.country_code.as_deref() {
            if !candidate
                .country_code
                .as_deref()
                .is_some_and(|actual| same_text(actual, country_code))
            {
                continue;
            }
        }
        if let Some(admin) = query.admin.as_deref() {
            if !candidate
                .administrative_areas
                .iter()
                .any(|area| super::same_place_name(area, admin))
                && !super::same_place_name(&candidate.administrative_area, admin)
            {
                continue;
            }
        }

        let dedupe_key = candidate
            .source_id
            .as_deref()
            .map(|id| format!("id:{id}"))
            .unwrap_or_else(|| {
                format!(
                    "fallback:{}:{}:{}:{}",
                    candidate.name.to_ascii_lowercase(),
                    candidate.country_code.as_deref().unwrap_or_default(),
                    candidate.latitude.to_bits(),
                    candidate.longitude.to_bits()
                )
            });
        if !seen.insert(dedupe_key) {
            continue;
        }

        candidates.push(candidate);
        if candidates.len() == return_limit {
            break;
        }
    }

    Ok(candidates)
}

#[derive(serde::Deserialize)]
struct GeocodingResponse {
    #[serde(default)]
    results: Option<Vec<GeocodingCandidate>>,
    #[serde(default)]
    error: Option<serde_json::Value>,
}

#[derive(serde::Deserialize)]
struct GeocodingCandidate {
    #[serde(default)]
    id: Option<u64>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    latitude: Option<f64>,
    #[serde(default)]
    longitude: Option<f64>,
    #[serde(default)]
    feature_code: Option<String>,
    #[serde(default)]
    admin1: Option<String>,
    #[serde(default)]
    admin2: Option<String>,
    #[serde(default)]
    admin3: Option<String>,
    #[serde(default)]
    admin4: Option<String>,
    #[serde(default)]
    country: Option<String>,
    #[serde(default)]
    country_code: Option<String>,
    #[serde(default)]
    timezone: Option<String>,
}

fn convert_candidate(raw: GeocodingCandidate) -> Option<ResolvedLocation> {
    let name = nonempty(raw.name)?;
    let latitude = raw
        .latitude
        .filter(|value| value.is_finite() && (-90.0..=90.0).contains(value))?;
    let longitude = raw
        .longitude
        .filter(|value| value.is_finite() && (-180.0..=180.0).contains(value))?;
    let kind = kind_for_feature(raw.feature_code.as_deref()?)?;
    let country = nonempty(raw.country).unwrap_or_default();
    let country_code = nonempty(raw.country_code).map(|value| value.to_ascii_uppercase());
    let timezone = nonempty(raw.timezone);

    let mut administrative_areas = Vec::new();
    for area in [raw.admin1, raw.admin2, raw.admin3, raw.admin4]
        .into_iter()
        .filter_map(nonempty)
    {
        if !administrative_areas
            .iter()
            .any(|existing| existing == &area)
        {
            administrative_areas.push(area);
        }
    }
    let administrative_area = administrative_areas.first().cloned().unwrap_or_default();

    let mut display_parts = Vec::new();
    for part in std::iter::once(name.as_str())
        .chain(administrative_areas.iter().map(String::as_str))
        .chain(std::iter::once(country.as_str()))
        .filter(|part| !part.is_empty())
    {
        if !display_parts.iter().any(|existing| *existing == part) {
            display_parts.push(part);
        }
    }
    let display_name = display_parts.join(" · ");

    let mut location = ResolvedLocation {
        name,
        display_name,
        latitude,
        longitude,
        coordinate_system: "wgs84",
        administrative_area,
        country,
        country_code: country_code.clone(),
        // "是否中国大陆"的行政证据来自解析器返回的 ISO 国家码（CN=大陆，
        // HK/MO/TW/其他=非大陆）；缺失即证据不足，交给用户确认。
        in_mainland_china: super::coordinates::mainland_from_country_code(country_code.as_deref()),
        administrative_areas,
        timezone,
        kind,
        source: "open_meteo",
        source_id: raw.id.map(|id| format!("open_meteo:{id}")),
        resolved_at: chrono::Utc::now().to_rfc3339(),
    };
    location.display_name = location.display_label();
    Some(location)
}

fn kind_for_feature(feature_code: &str) -> Option<&'static str> {
    match feature_code {
        "PPL" | "PPLC" | "PPLG" | "PPLS" | "PPLA" => Some("city_center"),
        "PPLA2" | "PPLA3" | "PPLA4" => Some("county_center"),
        "ADM1" => Some("administrative_center"),
        "ADM2" | "ADM3" | "ADM4" => Some("county_center"),
        _ => None,
    }
}

fn nonempty(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

fn same_text(left: &str, right: &str) -> bool {
    left.trim().eq_ignore_ascii_case(right.trim())
}

fn invalid_response() -> PluginError {
    PluginError {
        code: "invalid_response".into(),
        message: "地点搜索服务返回了无效响应".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::location::{can_auto_resolve, parse_query};

    fn query(limit: usize) -> LocationQuery {
        parse_query("朝阳", Some("CN"), Some("辽宁"), limit).unwrap()
    }

    fn response() -> String {
        r#"{
          "results": [
            {"id": 10, "name": "朝阳", "latitude": 41.57, "longitude": 120.45,
             "feature_code": "PPLA2", "admin1": "辽宁", "admin2": "朝阳市",
             "country": "中国", "country_code": "CN", "timezone": "Asia/Shanghai"},
            {"id": 10, "name": "朝阳", "latitude": 41.57, "longitude": 120.45,
             "feature_code": "PPLA2", "admin1": "辽宁", "admin2": "朝阳市",
             "country": "中国", "country_code": "CN"},
            {"id": 11, "name": "朝阳区", "latitude": 39.92, "longitude": 116.44,
             "feature_code": "PPL", "admin1": "辽宁", "country": "中国", "country_code": "CN"},
            {"id": 12, "name": "朝阳", "latitude": 40.0, "longitude": 110.0,
             "feature_code": "PPL", "admin1": "吉林", "country": "中国", "country_code": "CN"},
            {"id": 13, "name": "Chaoyang", "latitude": 34.0, "longitude": -100.0,
             "feature_code": "PPL", "admin1": "Texas", "country": "United States", "country_code": "US"},
            {"id": 14, "name": "朝阳社区", "latitude": 41.0, "longitude": 120.0,
             "feature_code": "PPLX", "admin1": "辽宁", "country": "中国", "country_code": "CN"}
          ]
        }"#
            .to_owned()
    }

    #[test]
    fn search_url_uses_language_country_admin_and_ambiguity_count() {
        let url = search_url(&query(1));
        assert!(url.starts_with(API_URL));
        assert!(url.contains("count=100"));
        assert!(url.contains("language=zh"));
        assert!(url.contains("countryCode=CN"));
        let decoded_name = url
            .split("name=")
            .nth(1)
            .unwrap()
            .split('&')
            .next()
            .unwrap();
        assert_eq!(urlencoding::decode(decoded_name).unwrap(), "朝阳,辽宁");
    }

    #[test]
    fn unqualified_retry_keeps_country_and_omits_admin_suffix() {
        let query = parse_query("广州/天河", Some("CN"), None, 5).unwrap();
        let url = unqualified_search_url(&query);
        assert!(url.contains("count=100"));
        assert!(url.contains("language=zh"));
        assert!(url.contains("countryCode=CN"));
        let encoded_name = url
            .split("name=")
            .nth(1)
            .unwrap()
            .split('&')
            .next()
            .unwrap();
        assert_eq!(urlencoding::decode(encoded_name).unwrap(), "天河");
    }

    #[test]
    fn candidates_are_filtered_deduplicated_bounded_and_fully_mapped() {
        let candidates = parse_candidates(&response(), &query(20)).unwrap();
        assert_eq!(candidates.len(), 2);
        let city = &candidates[0];
        assert_eq!(city.name, "朝阳");
        assert_eq!(city.display_name, "中国 · 辽宁 · 朝阳市");
        assert_eq!(city.country_code.as_deref(), Some("CN"));
        assert_eq!(
            city.administrative_areas,
            vec!["辽宁".to_owned(), "朝阳市".to_owned()]
        );
        assert_eq!(city.kind, "county_center");
        assert_eq!(city.source_id.as_deref(), Some("open_meteo:10"));
        assert!(!city.resolved_at.is_empty());
        assert_eq!(candidates[1].kind, "city_center");
    }

    #[test]
    fn limit_one_returns_exactly_one_candidate() {
        let query = query(1);
        let candidates = parse_candidates(&response(), &query).unwrap();
        assert_eq!(candidates.len(), 1);
        assert!(!can_auto_resolve(&query, &candidates));
    }

    #[test]
    fn city_admin_matches_any_hierarchy_level_and_country_code_stays_exact() {
        let query = parse_query("广州/天河", Some("CN"), None, 5).unwrap();
        let body = r#"{"results":[
            {"id":21,"name":"天河区","latitude":23.12,"longitude":113.36,
             "feature_code":"PPL","admin1":"广东","admin2":"广州市","admin3":"天河区",
             "country":"中国","country_code":"CN"},
            {"id":22,"name":"天河区","latitude":23.12,"longitude":113.36,
             "feature_code":"PPL","admin1":"广东","admin2":"广州市","admin3":"天河区",
             "country":"中国","country_code":"CN市"}
        ]}"#;
        let candidates = parse_candidates(body, &query).unwrap();
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].name, "天河区");
        assert_eq!(candidates[0].administrative_area, "广东");
        assert_eq!(candidates[0].administrative_areas[1], "广州市");
    }

    #[test]
    fn no_results_is_empty_and_malformed_json_is_an_error() {
        assert!(
            parse_candidates(r#"{"results":[]}"#, &query(5))
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            parse_candidates("not json", &query(5)).unwrap_err().code,
            "invalid_response"
        );
    }

    fn body_with_raw_result_count(count: usize) -> String {
        format!(
            "{{\"results\":[{}]}}",
            (0..count).map(|_| "null").collect::<Vec<_>>().join(",")
        )
    }

    #[test]
    fn truncation_uses_the_raw_result_count() {
        assert!(!possibly_truncated("{}"));
        assert!(!possibly_truncated(r#"{"results":null}"#));
        assert!(!possibly_truncated(&body_with_raw_result_count(0)));
        assert!(!possibly_truncated(&body_with_raw_result_count(
            REQUEST_COUNT - 1
        )));
        assert!(possibly_truncated(&body_with_raw_result_count(
            REQUEST_COUNT
        )));
        assert!(possibly_truncated(&body_with_raw_result_count(
            REQUEST_COUNT + 1
        )));
    }

    #[test]
    fn malformed_truncation_probe_fails_closed() {
        assert!(possibly_truncated("not json"));
        assert!(possibly_truncated("[]"));
        assert!(possibly_truncated(r#"{"results":{}}"#));
    }
}
