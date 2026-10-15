/// 将 Open-Meteo / WMO 天气代码映射为简体中文描述。
///
/// 调用方应先区分天气代码缺失与数值 `0`；缺失值不能默认成晴天代码 `0`。
pub fn wmo_description(code: u32) -> &'static str {
    match code {
        0 => "晴",
        1 => "大部晴朗",
        2 => "局部多云",
        3 => "阴",
        45 => "雾",
        48 => "雾凇",
        51 => "小毛毛雨",
        53 => "中毛毛雨",
        55 => "大毛毛雨",
        56 => "小冻毛毛雨",
        57 => "大冻毛毛雨",
        61 => "小雨",
        63 => "中雨",
        65 => "大雨",
        66 => "小冻雨",
        67 => "大冻雨",
        71 => "小雪",
        73 => "中雪",
        75 => "大雪",
        77 => "雪粒",
        80 => "小阵雨",
        81 => "中阵雨",
        82 => "强阵雨",
        85 => "小阵雪",
        86 => "大阵雪",
        95 => "雷暴",
        96 => "雷暴伴小冰雹",
        97 => "强雷暴",
        99 => "雷暴伴大冰雹",
        _ => "未知",
    }
}

#[cfg(test)]
mod tests {
    use super::wmo_description;

    #[test]
    fn maps_all_documented_wmo_codes_to_chinese_descriptions() {
        let cases = [
            (0, "晴"),
            (1, "大部晴朗"),
            (2, "局部多云"),
            (3, "阴"),
            (45, "雾"),
            (48, "雾凇"),
            (51, "小毛毛雨"),
            (53, "中毛毛雨"),
            (55, "大毛毛雨"),
            (56, "小冻毛毛雨"),
            (57, "大冻毛毛雨"),
            (61, "小雨"),
            (63, "中雨"),
            (65, "大雨"),
            (66, "小冻雨"),
            (67, "大冻雨"),
            (71, "小雪"),
            (73, "中雪"),
            (75, "大雪"),
            (77, "雪粒"),
            (80, "小阵雨"),
            (81, "中阵雨"),
            (82, "强阵雨"),
            (85, "小阵雪"),
            (86, "大阵雪"),
            (95, "雷暴"),
            (96, "雷暴伴小冰雹"),
            (97, "强雷暴"),
            (99, "雷暴伴大冰雹"),
        ];

        for (code, expected) in cases {
            assert_eq!(wmo_description(code), expected, "WMO code {code}");
        }
    }

    #[test]
    fn distinguishes_clear_cloudy_and_overcast_conditions() {
        assert_eq!(wmo_description(1), "大部晴朗");
        assert_eq!(wmo_description(2), "局部多云");
        assert_eq!(wmo_description(3), "阴");
        assert_ne!(wmo_description(1), wmo_description(2));
        assert_ne!(wmo_description(2), wmo_description(3));
    }

    #[test]
    fn distinguishes_hail_thunderstorms_from_freezing_rain() {
        assert_eq!(wmo_description(66), "小冻雨");
        assert_eq!(wmo_description(67), "大冻雨");
        assert_eq!(wmo_description(96), "雷暴伴小冰雹");
        assert_eq!(wmo_description(99), "雷暴伴大冰雹");
        assert_ne!(wmo_description(96), wmo_description(66));
        assert_ne!(wmo_description(99), wmo_description(67));
    }

    #[test]
    fn unknown_codes_stay_unknown_instead_of_becoming_clear_sky() {
        for code in [4, 49, 68, 94, 98, 100, u32::MAX] {
            assert_eq!(wmo_description(code), "未知", "unexpected code {code}");
        }
        assert_eq!(wmo_description(0), "晴");
    }
}
