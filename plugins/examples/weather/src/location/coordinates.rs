//! 彩云请求坐标派生：原始地点保持 WGS84 不动，请求前按区域证据派生目标坐标系。
//!
//! 彩云 v2.x 规则（官方 FAQ）：中国大陆请求 GCJ-02，大陆以外（含港澳台）WGS84。
//! 区域判断只接受解析器行政元信息（ISO 国家码或和风国家+明确省级行政区）
//! 或用户明确声明（`in_mainland_china`）；地点名称、
//! 时区、系统语言、IP、粗略经纬度矩形都不能当国界，证据不足时拒绝猜测。
//! WGS84→GCJ-02 使用公开偏移算法（eviltransform 系实现的通用公式），测试对
//! 该项目发布的上海/深圳/北京基准向量以 5 位小数精度断言。
//!
//! 响应中的 `location` 数组（纬度在前）仅作诊断，不回写已保存地点，避免重复
//! 转换累积误差。

use crate::model::ResolvedLocation;
use crate::protocol::PluginError;

/// 派生出的彩云请求坐标及其坐标系。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RequestCoordinates {
    pub latitude: f64,
    pub longitude: f64,
    pub coordinate_system: &'static str,
}

/// 从解析器候选的 ISO 国家码推导"是否中国大陆"行政证据。
/// CN → 大陆；HK/MO/TW（与 CN 不同的 ISO 条目）→ 非大陆；其他两位码 → 非大陆；
/// 缺失（和风候选无 country_code、手工坐标）→ None，证据不足。
pub fn mainland_from_country_code(country_code: Option<&str>) -> Option<bool> {
    let code = country_code?.trim().to_ascii_uppercase();
    if code.len() != 2 || !code.bytes().all(|b| b.is_ascii_alphabetic()) {
        return None;
    }
    Some(code == "CN")
}

/// 和风 GeoAPI 的 country + adm1 是行政层级证据，不能只凭 country="中国"
/// 判为大陆。仅接受完整省级白名单，港澳台单独处理；模糊/缺失数据保持待确认。
pub fn mainland_from_qweather_admin(country: &str, province: &str) -> Option<bool> {
    if !matches!(country.trim(), "中国" | "中华人民共和国")
        && !country.trim().eq_ignore_ascii_case("China")
    {
        return None;
    }
    match province.trim() {
        "香港" | "香港特别行政区" | "澳门" | "澳门特别行政区" | "台湾" | "台湾省" => {
            Some(false)
        }
        "北京"
        | "北京市"
        | "天津"
        | "天津市"
        | "上海"
        | "上海市"
        | "重庆"
        | "重庆市"
        | "河北"
        | "河北省"
        | "山西"
        | "山西省"
        | "辽宁"
        | "辽宁省"
        | "吉林"
        | "吉林省"
        | "黑龙江"
        | "黑龙江省"
        | "江苏"
        | "江苏省"
        | "浙江"
        | "浙江省"
        | "安徽"
        | "安徽省"
        | "福建"
        | "福建省"
        | "江西"
        | "江西省"
        | "山东"
        | "山东省"
        | "河南"
        | "河南省"
        | "湖北"
        | "湖北省"
        | "湖南"
        | "湖南省"
        | "广东"
        | "广东省"
        | "海南"
        | "海南省"
        | "四川"
        | "四川省"
        | "贵州"
        | "贵州省"
        | "云南"
        | "云南省"
        | "陕西"
        | "陕西省"
        | "甘肃"
        | "甘肃省"
        | "青海"
        | "青海省"
        | "内蒙古"
        | "内蒙古自治区"
        | "广西"
        | "广西壮族自治区"
        | "西藏"
        | "西藏自治区"
        | "宁夏"
        | "宁夏回族自治区"
        | "新疆"
        | "新疆维吾尔自治区" => Some(true),
        _ => None,
    }
}

/// 彩云请求坐标：大陆声明 → WGS84 派生 GCJ-02；非大陆 → 原值直传；
/// 未确认 → 报错并给出可操作入口（提示与该地点绑定，不做全局假设）。
/// 派生只读取原始地点，不修改、不累积转换误差；重复调用结果一致。
pub fn caiyun_request_coordinates(
    location: &ResolvedLocation,
) -> Result<RequestCoordinates, PluginError> {
    let region = location.in_mainland_china.or_else(|| {
        (location.source == "qweather")
            .then(|| mainland_from_qweather_admin(&location.country, &location.administrative_area))
            .flatten()
    });
    match region {
        Some(false) => Ok(RequestCoordinates {
            latitude: location.latitude,
            longitude: location.longitude,
            coordinate_system: "wgs84",
        }),
        Some(true) => {
            let (latitude, longitude) = wgs84_to_gcj02(location.latitude, location.longitude);
            Ok(RequestCoordinates {
                latitude,
                longitude,
                coordinate_system: "gcj02",
            })
        }
        None => Err(PluginError {
            code: "invalid_arguments".into(),
            message: "该地点的坐标区域无法自动确定。请在 设置 → 插件 → 天气 → 更换地点 中选择“中国大陆”或“中国大陆以外”，然后点击“保存地点”；或在 location 中提供 in_mainland_china 声明。".into(),
        }),
    }
}

const SEMI_MAJOR_AXIS: f64 = 6378245.0;
const ECCENTRICITY_SQUARED: f64 = 0.00669342162296594323;

/// WGS84 → GCJ-02 公开偏移算法。远超中国范围的坐标原样返回（算法内置的
/// 适用域守卫，防止对明显矛盾声明套用荒谬偏移）；该矩形只约束转换函数本身，
/// 不用于区域判定——是否调用转换由 `in_mainland_china` 证据决定。
pub fn wgs84_to_gcj02(wgs_latitude: f64, wgs_longitude: f64) -> (f64, f64) {
    if out_of_china_bounds(wgs_latitude, wgs_longitude) {
        return (wgs_latitude, wgs_longitude);
    }
    let (transformed_latitude, transformed_longitude) =
        transform(wgs_longitude - 105.0, wgs_latitude - 35.0);
    let radian_latitude = wgs_latitude.to_radians();
    let mut magic = radian_latitude.sin();
    magic = 1.0 - ECCENTRICITY_SQUARED * magic * magic;
    let sqrt_magic = magic.sqrt();
    let latitude_delta = (transformed_latitude * 180.0)
        / ((SEMI_MAJOR_AXIS * (1.0 - ECCENTRICITY_SQUARED)) / (magic * sqrt_magic)
            * std::f64::consts::PI);
    let longitude_delta = (transformed_longitude * 180.0)
        / (SEMI_MAJOR_AXIS / sqrt_magic * radian_latitude.cos() * std::f64::consts::PI);
    (
        wgs_latitude + latitude_delta,
        wgs_longitude + longitude_delta,
    )
}

/// 算法适用域（大陆及其近海），与 eviltransform 系实现一致；不是国界。
fn out_of_china_bounds(latitude: f64, longitude: f64) -> bool {
    !(longitude > 72.004 && longitude < 137.8347 && latitude > 0.8293 && latitude < 55.8271)
}

fn transform(x: f64, y: f64) -> (f64, f64) {
    let pi = std::f64::consts::PI;
    let mut latitude =
        -100.0 + 2.0 * x + 3.0 * y + 0.2 * y * y + 0.1 * x * y + 0.2 * x.abs().sqrt();
    latitude += (20.0 * (6.0 * x * pi).sin() + 20.0 * (2.0 * x * pi).sin()) * 2.0 / 3.0;
    latitude += (20.0 * (y * pi).sin() + 40.0 * (y / 3.0 * pi).sin()) * 2.0 / 3.0;
    latitude += (160.0 * (y / 12.0 * pi).sin() + 320.0 * (y * pi / 30.0).sin()) * 2.0 / 3.0;

    let mut longitude = 300.0 + x + 2.0 * y + 0.1 * x * x + 0.1 * x * y + 0.1 * x.abs().sqrt();
    longitude += (20.0 * (6.0 * x * pi).sin() + 20.0 * (2.0 * x * pi).sin()) * 2.0 / 3.0;
    longitude += (20.0 * (x * pi).sin() + 40.0 * (x / 3.0 * pi).sin()) * 2.0 / 3.0;
    longitude += (150.0 * (x / 12.0 * pi).sin() + 300.0 * (x / 30.0 * pi).sin()) * 2.0 / 3.0;
    (latitude, longitude)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn location(
        in_mainland_china: Option<bool>,
        latitude: f64,
        longitude: f64,
    ) -> ResolvedLocation {
        ResolvedLocation {
            name: "测试".into(),
            display_name: "测试".into(),
            latitude,
            longitude,
            coordinate_system: "wgs84",
            administrative_area: String::new(),
            country: String::new(),
            country_code: None,
            administrative_areas: vec![],
            timezone: None,
            kind: "manual",
            source: "user",
            source_id: None,
            resolved_at: "2026-10-15T00:00:00Z".into(),
            in_mainland_china,
        }
    }

    /// eviltransform 官方测试向量（go/transform_test.go，5 位小数断言）。
    /// 来源：https://github.com/googollee/eviltransform（MIT）。
    #[test]
    fn conversion_matches_published_reference_vectors() {
        // (wgs_lat, wgs_lng, expected_gcj_lat, expected_gcj_lng)
        let vectors = [
            (31.1774276, 121.5272106, 31.17530398364597, 121.531541859215), // 上海
            (22.543847, 113.912316, 22.540796131694766, 113.9171764808363), // 深圳
            (39.911954, 116.377817, 39.91334545536069, 116.38404722455657), // 北京
        ];
        for (wgs_lat, wgs_lng, gcj_lat, gcj_lng) in vectors {
            let (lat, lng) = wgs84_to_gcj02(wgs_lat, wgs_lng);
            assert!((lat - gcj_lat).abs() < 1e-6, "lat {lat} vs {gcj_lat}");
            assert!((lng - gcj_lng).abs() < 1e-6, "lng {lng} vs {gcj_lng}");
        }
    }

    #[test]
    fn mainland_request_coordinates_derive_gcj02_and_keep_the_original_untouched() {
        let original = location(Some(true), 31.1774276, 121.5272106);
        let first = caiyun_request_coordinates(&original).unwrap();
        assert_eq!(first.coordinate_system, "gcj02");
        assert!((first.latitude - 31.17530398364597).abs() < 1e-6);
        assert!((first.longitude - 121.531541859215).abs() < 1e-6);
        // 原始地点不被改写；重复派生结果一致，不累积误差
        assert_eq!(original.latitude, 31.1774276);
        assert_eq!(original.longitude, 121.5272106);
        let second = caiyun_request_coordinates(&original).unwrap();
        assert_eq!(first, second);
        // 换源（如改回 Open-Meteo 场景）读取的原值仍是最初保存的 WGS84
        assert_eq!(original.coordinate_system, "wgs84");
    }

    #[test]
    fn outside_mainland_passes_wgs84_through_untouched() {
        // 港澳台与海外：非大陆声明 → 原值直传（含边界附近坐标）
        for (lat, lng, label) in [
            (22.3193, 114.1694, "香港"),
            (51.5074, -0.1278, "伦敦"),
            (-33.8688, 151.2093, "悉尼"),
            (0.0, 0.0, "零值"),
            (-41.876, -171.0, "负值极值"),
        ] {
            let request = caiyun_request_coordinates(&location(Some(false), lat, lng)).unwrap();
            assert_eq!(request.coordinate_system, "wgs84", "{label}");
            assert_eq!(request.latitude, lat, "{label}");
            assert_eq!(request.longitude, lng, "{label}");
        }
    }

    #[test]
    fn missing_region_evidence_is_rejected_with_an_actionable_error() {
        let error = caiyun_request_coordinates(&location(None, 39.9, 116.4)).unwrap_err();
        assert_eq!(error.code, "invalid_arguments");
        assert!(error.message.contains("中国大陆"));
        assert!(error.message.contains("in_mainland_china"));
        // 和风候选（无国家码）与手工坐标同样证据不足
        assert_eq!(mainland_from_country_code(None), None);
    }

    #[test]
    fn country_code_evidence_distinguishes_mainland_from_special_regions() {
        assert_eq!(mainland_from_country_code(Some("CN")), Some(true));
        assert_eq!(mainland_from_country_code(Some("cn")), Some(true));
        // 港澳台是独立 ISO 条目，不能凭中文名并入大陆
        assert_eq!(mainland_from_country_code(Some("HK")), Some(false));
        assert_eq!(mainland_from_country_code(Some("MO")), Some(false));
        assert_eq!(mainland_from_country_code(Some("TW")), Some(false));
        assert_eq!(mainland_from_country_code(Some("US")), Some(false));
        assert_eq!(mainland_from_country_code(Some("JP")), Some(false));
        // 非两位字母不是可用证据
        assert_eq!(mainland_from_country_code(Some("CHN")), None);
        assert_eq!(mainland_from_country_code(Some("")), None);
        assert_eq!(mainland_from_country_code(Some("C1")), None);
    }

    #[test]
    fn qweather_region_uses_country_and_exact_provincial_metadata() {
        assert_eq!(mainland_from_qweather_admin("中国", "广东省"), Some(true));
        assert_eq!(mainland_from_qweather_admin("China", "北京市"), Some(true));
        for province in ["香港特别行政区", "澳门", "台湾省"] {
            assert_eq!(mainland_from_qweather_admin("中国", province), Some(false));
        }
        for (country, province) in [
            ("中国", ""),
            ("中国", "广州"),
            ("中国", "广东附近"),
            ("", "广东省"),
            ("日本", "广东省"),
        ] {
            assert_eq!(mainland_from_qweather_admin(country, province), None);
        }
        let mut saved = location(None, 23.13, 113.26);
        saved.source = "qweather";
        saved.country = "中国".into();
        saved.administrative_area = "广东省".into();
        assert_eq!(
            caiyun_request_coordinates(&saved)
                .unwrap()
                .coordinate_system,
            "gcj02"
        );
        assert_eq!(
            saved.in_mainland_china, None,
            "read-only recovery never rewrites stored metadata"
        );
        saved.in_mainland_china = Some(false);
        assert_eq!(
            caiyun_request_coordinates(&saved)
                .unwrap()
                .coordinate_system,
            "wgs84",
            "explicit choice takes priority"
        );
        saved.in_mainland_china = None;
        saved.source = "user";
        assert!(
            caiyun_request_coordinates(&saved).is_err(),
            "manual metadata cannot impersonate a resolver"
        );
    }

    #[test]
    fn far_away_misdeclared_coordinates_are_returned_unchanged_by_the_guard() {
        // 声明与坐标明显矛盾（东京）时算法守卫原样返回，不套用荒谬偏移
        let (lat, lng) = wgs84_to_gcj02(35.6762, 139.6503);
        assert_eq!((lat, lng), (35.6762, 139.6503));
    }
}
