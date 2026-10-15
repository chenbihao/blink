//! 和风 GeoAPI 城市搜索：URL 构造与响应解析。
//!
//! 响应/参数依据 2026-10 官方文档：`GET /geo/v2/city/lookup`，名称参数 location、
//! 行政限定 adm、国家限定 range（ISO 3166 代码）、数量 number（1-20）；响应候选
//! 无 country_code 字段（country 是本地化名称），来源声明在 refer.sources/license。

use std::collections::HashSet;

use crate::model::ResolvedLocation;
use crate::protocol::PluginError;

use super::LocationQuery;

const MAX_CANDIDATES: usize = 20;

/// 请求候选数：与查询 limit 对齐（上限 20）。达到该数量时上游可能还有更多结果。
fn request_count(query: &LocationQuery) -> usize {
    query.limit.min(MAX_CANDIDATES)
}

/// Build the QWeather city lookup URL. `credentials` carries the normalized API host root.
pub fn search_url(query: &LocationQuery, host: &str) -> String {
    let mut url = format!(
        "{host}/geo/v2/city/lookup?location={}&number={}&lang=zh",
        urlencoding::encode(&query.query),
        request_count(query),
    );
    if let Some(admin) = query.admin.as_deref() {
        url.push_str("&adm=");
        url.push_str(&urlencoding::encode(admin));
    }
    if let Some(country_code) = query.country_code.as_deref() {
        url.push_str("&range=");
        url.push_str(&urlencoding::encode(&country_code.to_ascii_lowercase()));
    }
    url
}

/// 上游是否可能截断了结果：和风 number 上限 20，返回数达到请求值即视为可能截断。
/// 解析失败时保守认为可能截断（不能据此确认唯一候选）。
pub fn possibly_truncated(body: &str, query: &LocationQuery) -> bool {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(body) else {
        return true;
    };
    let Some(object) = value.as_object() else {
        return true;
    };
    match object.get("location") {
        None | Some(serde_json::Value::Null) => false,
        Some(serde_json::Value::Array(locations)) => locations.len() >= request_count(query),
        Some(_) => true,
    }
}

/// Parse, validate, filter, and de-duplicate QWeather city lookup candidates.
///
/// GeoAPI 候选没有 country_code，用户传入的国家限定只经 `range` 交上游过滤，
/// 本地不做二次剔除（无法验证的过滤宁可少做，不能静默丢候选）；也正因缺少本地
/// 证据，`can_auto_resolve` 不会在国家限定下自动确认和风候选。
pub fn parse_candidates(
    body: &str,
    query: &LocationQuery,
) -> Result<Vec<ResolvedLocation>, PluginError> {
    let response: LookupResponse = serde_json::from_str(body).map_err(|_| invalid_response())?;
    match response.code.as_deref() {
        None | Some("200") => {}
        Some("401") => {
            return Err(PluginError {
                code: "authentication_failed".into(),
                message: "和风鉴权失败，请检查 API Host 与 API Key".into(),
            });
        }
        Some("402") => {
            return Err(PluginError {
                code: "quota_exceeded".into(),
                message: "和风账号余额或额度不足".into(),
            });
        }
        Some("429") => {
            return Err(PluginError {
                code: "rate_limited".into(),
                message: "和风请求过于频繁，请稍后重试".into(),
            });
        }
        Some(_) => return Err(invalid_response()),
    }

    let return_limit = query.limit.min(MAX_CANDIDATES);
    let mut candidates = Vec::new();
    let mut seen = HashSet::new();
    for raw in response.location.unwrap_or_default() {
        let Some(candidate) = convert_candidate(raw) else {
            continue;
        };
        // 行政区限定在本地按层级文本匹配（和风 adm 参数之外的双保险）。
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
            .clone()
            .unwrap_or_else(|| format!("fallback:{}", candidate.display_name));
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

/// GeoAPI 响应中的来源声明（refer.sources/license），进入搜索结果包供展示。
pub fn parse_attributions(body: &str) -> Vec<String> {
    serde_json::from_str::<LookupResponse>(body)
        .ok()
        .and_then(|response| response.refer)
        .map(|refer| {
            refer
                .sources
                .into_iter()
                .chain(refer.license)
                .filter(|s| !s.trim().is_empty())
                .collect()
        })
        .unwrap_or_default()
}

#[derive(serde::Deserialize)]
struct LookupResponse {
    #[serde(default)]
    code: Option<String>,
    #[serde(default)]
    location: Option<Vec<LookupCandidate>>,
    #[serde(default)]
    refer: Option<Refer>,
}

#[derive(serde::Deserialize)]
struct Refer {
    #[serde(default)]
    sources: Vec<String>,
    #[serde(default)]
    license: Vec<String>,
}

#[derive(serde::Deserialize)]
struct LookupCandidate {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    lat: Option<String>,
    #[serde(default)]
    lon: Option<String>,
    #[serde(default)]
    adm1: Option<String>,
    #[serde(default)]
    adm2: Option<String>,
    #[serde(default)]
    country: Option<String>,
    #[serde(default)]
    tz: Option<String>,
    #[serde(default)]
    r#type: Option<String>,
}

fn convert_candidate(raw: LookupCandidate) -> Option<ResolvedLocation> {
    let name = nonempty(raw.name)?;
    let latitude = raw
        .lat
        .and_then(|value| value.trim().parse::<f64>().ok())
        .filter(|value| value.is_finite() && (-90.0..=90.0).contains(value))?;
    let longitude = raw
        .lon
        .and_then(|value| value.trim().parse::<f64>().ok())
        .filter(|value| value.is_finite() && (-180.0..=180.0).contains(value))?;
    // GeoAPI 的 type 不能区分城市与区县中心，未知类型直接丢弃，不凭名字或
    // rank 冒充已确认的地点类型。
    let kind = kind_for_type(raw.r#type.as_deref()?)?;
    let country = nonempty(raw.country).unwrap_or_default();
    let timezone = nonempty(raw.tz);

    let mut administrative_areas = Vec::new();
    for area in [raw.adm1, raw.adm2].into_iter().filter_map(nonempty) {
        if !administrative_areas
            .iter()
            .any(|existing| existing == &area)
        {
            administrative_areas.push(area);
        }
    }
    let administrative_area = administrative_areas.first().cloned().unwrap_or_default();

    let mut display_parts = vec![name.clone()];
    for part in administrative_areas
        .iter()
        .chain(std::iter::once(&country))
        .filter(|part| !part.is_empty())
    {
        if !display_parts.iter().any(|existing| existing == part) {
            display_parts.push(part.clone());
        }
    }
    let display_name = display_parts.join(" · ");

    Some(ResolvedLocation {
        name,
        display_name,
        latitude,
        longitude,
        coordinate_system: "wgs84",
        administrative_area,
        country,
        // 和风不提供国家代码；宁缺勿伪造，不从国家名或时区反推。
        country_code: None,
        administrative_areas,
        timezone,
        kind,
        source: "qweather",
        source_id: raw
            .id
            .filter(|id| !id.trim().is_empty())
            .map(|id| format!("qweather:{id}")),
        resolved_at: chrono::Utc::now().to_rfc3339(),
    })
}

fn kind_for_type(location_type: &str) -> Option<&'static str> {
    match location_type {
        "city" => Some("city_center"),
        "town" => Some("district_center"),
        _ => None,
    }
}

fn nonempty(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

fn invalid_response() -> PluginError {
    PluginError {
        code: "invalid_response".into(),
        message: "和风地点搜索返回了无效响应".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::location::parse_query;

    fn query(limit: usize) -> LocationQuery {
        parse_query("朝阳", Some("CN"), Some("辽宁"), limit).unwrap()
    }

    fn response() -> String {
        r#"{
          "code": "200",
          "location": [
            {"name": "朝阳", "id": "101070301", "lat": "41.57382", "lon": "120.45078",
             "adm1": "辽宁省", "adm2": "朝阳市", "country": "中国",
             "tz": "Asia/Shanghai", "type": "city", "rank": "10"},
            {"name": "朝阳", "id": "101070301", "lat": "41.57382", "lon": "120.45078",
             "adm1": "辽宁省", "adm2": "朝阳市", "country": "中国", "type": "city"},
            {"name": "朝阳镇", "id": "101070302", "lat": "42.0", "lon": "121.0",
             "adm1": "辽宁省", "adm2": "沈阳市", "country": "中国", "type": "city"},
            {"name": "朝阳", "id": "101060407", "lat": "39.92189", "lon": "116.44093",
             "adm1": "北京市", "adm2": "朝阳区", "country": "中国", "type": "city"},
            {"name": "朝阳县", "id": "999", "lat": "40.0", "lon": "120.0",
             "adm1": "辽宁省", "country": "中国", "type": "unknown-kind"}
          ],
          "refer": {"sources": ["https://developer.qweather.com/attribution.html"], "license": ["QWeather Developers License"]}
        }"#
            .to_owned()
    }

    #[test]
    fn search_url_encodes_name_admin_range_and_number() {
        let url = search_url(&query(5), "https://abc123.re.qweatherapi.com");
        assert!(url.starts_with("https://abc123.re.qweatherapi.com/geo/v2/city/lookup?"));
        let encoded_name = url
            .split("location=")
            .nth(1)
            .unwrap()
            .split('&')
            .next()
            .unwrap();
        assert_eq!(urlencoding::decode(encoded_name).unwrap(), "朝阳");
        assert!(url.contains("number=5"));
        assert!(url.contains("lang=zh"));
        assert!(url.contains("range=cn"));
        let encoded_adm = url.split("adm=").nth(1).unwrap().split('&').next().unwrap();
        assert_eq!(urlencoding::decode(encoded_adm).unwrap(), "辽宁");
    }

    #[test]
    fn candidates_are_deduplicated_filtered_and_namespaced() {
        let candidates = parse_candidates(&response(), &query(20)).unwrap();
        // 去重同一 Location ID；未知 type 丢弃；adm1=北京的候选被本地辽宁限定过滤。
        assert_eq!(candidates.len(), 2);
        let first = &candidates[0];
        assert_eq!(first.name, "朝阳");
        assert_eq!(first.source, "qweather");
        assert_eq!(first.source_id.as_deref(), Some("qweather:101070301"));
        assert_eq!(first.country_code, None);
        assert_eq!(
            first.administrative_areas,
            vec!["辽宁省".to_owned(), "朝阳市".to_owned()]
        );
        assert_eq!(first.kind, "city_center");
        assert_eq!(first.timezone.as_deref(), Some("Asia/Shanghai"));
        assert!(!first.resolved_at.is_empty());
    }

    #[test]
    fn geoapi_error_codes_map_to_stable_plugin_errors() {
        for (code, expected) in [
            ("401", "authentication_failed"),
            ("402", "quota_exceeded"),
            ("429", "rate_limited"),
            ("500", "invalid_response"),
        ] {
            let body = format!(r#"{{"code":"{code}"}}"#);
            assert_eq!(
                parse_candidates(&body, &query(5)).unwrap_err().code,
                expected
            );
        }
    }

    #[test]
    fn no_results_is_empty_and_malformed_json_is_an_error() {
        assert!(
            parse_candidates(r#"{"code":"200","location":[]}"#, &query(5))
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            parse_candidates("not json", &query(5)).unwrap_err().code,
            "invalid_response"
        );
    }

    #[test]
    fn truncation_uses_requested_count() {
        let body = r#"{"code":"200","location":[
            {"name":"a","lat":"1","lon":"2","type":"city"},
            {"name":"b","lat":"1","lon":"2","type":"city"}
        ]}"#;
        assert!(possibly_truncated(body, &query(2)));
        assert!(!possibly_truncated(body, &query(20)));
        assert!(!possibly_truncated(r#"{"code":"200"}"#, &query(5)));
        assert!(possibly_truncated("not json", &query(5)));
    }

    #[test]
    fn refer_attributions_are_extracted_for_display() {
        let attributions = parse_attributions(&response());
        assert_eq!(
            attributions,
            vec![
                "https://developer.qweather.com/attribution.html".to_owned(),
                "QWeather Developers License".to_owned()
            ]
        );
        assert!(parse_attributions(r#"{"code":"200"}"#).is_empty());
    }
}
