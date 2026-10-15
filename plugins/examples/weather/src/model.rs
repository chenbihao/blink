//! 供应商无关的天气数值模型；格式化仅在出口发生。
//!
//! 条件字段的多轨约定：
//! - `condition_code` 是 WMO 数值代码（Open-Meteo 返回），展示经 `conditions::wmo_description`。
//! - `condition*` 系列保留供应商原始文本与代码（和风 text+字符串代码、彩云 skycon 字符串
//!   代码经 `conditions::skycon_description` 转描述），不映射 WMO 表；两轨缺失即未知，
//!   不互相伪造。`condition`（全天主导，彩云）/ `day_condition`+`night_condition`
//!   （分段，和风）按供应商语义使用，不互相冒充。
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct ResolvedLocation {
    pub name: String,
    pub display_name: String,
    pub latitude: f64,
    pub longitude: f64,
    pub coordinate_system: &'static str,
    pub administrative_area: String,
    pub country: String,
    pub country_code: Option<String>,
    pub administrative_areas: Vec<String>,
    pub timezone: Option<String>,
    pub kind: &'static str,
    pub source: &'static str,
    pub source_id: Option<String>,
    pub resolved_at: String,
    /// 是否位于中国大陆（彩云 v2.x 请求坐标规则：大陆 GCJ-02，其余 WGS84）。
    /// Some 来自解析器行政证据（Open-Meteo ISO 国家码）或用户明确声明；
    /// None = 证据不足，彩云执行边界必须拒绝猜测并给出可操作的确认入口。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub in_mainland_china: Option<bool>,
}

impl ResolvedLocation {
    /// 国内按国家 → 省 → 市 → 区县排列；保留供应商原文，不猜测缺失层级。
    /// 从结构化字段生成，兼容已经保存的旧 display_name。
    pub fn display_label(&self) -> String {
        let domestic = self
            .country_code
            .as_deref()
            .is_some_and(|code| code.eq_ignore_ascii_case("CN"))
            || matches!(self.country.trim(), "中国" | "中华人民共和国")
            || self.country.trim().eq_ignore_ascii_case("China");
        if !domestic {
            return self.display_name.clone();
        }
        let areas: Vec<&str> = if self.administrative_areas.is_empty() {
            vec![self.administrative_area.as_str()]
        } else {
            self.administrative_areas
                .iter()
                .map(String::as_str)
                .collect()
        };
        let mut parts: Vec<&str> = Vec::new();
        for part in std::iter::once(self.country.as_str())
            .chain(areas)
            .chain(std::iter::once(self.name.as_str()))
            .map(str::trim)
            .filter(|part| !part.is_empty())
        {
            if !parts
                .iter()
                .any(|existing| crate::location::same_place_name(existing, part))
            {
                parts.push(part);
            }
        }
        parts.join(" · ")
    }
}

/// 供应商原始天气条件（和风）；Open-Meteo 走 WMO 数值代码，两者不混用。
#[derive(Debug, Clone, Serialize)]
pub struct ProviderCondition {
    pub text: String,
    pub code: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct CurrentWeather {
    pub temperature_c: Option<f64>,
    pub condition_code: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub condition: Option<ProviderCondition>,
    pub wind_speed_kmh: Option<f64>,
    pub valid_at: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DailyWeather {
    pub date: String,
    pub condition_code: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub condition: Option<ProviderCondition>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub day_condition: Option<ProviderCondition>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub night_condition: Option<ProviderCondition>,
    pub temperature_max_c: Option<f64>,
    pub temperature_min_c: Option<f64>,
    pub precipitation_probability_percent: Option<f64>,
    pub wind_max_kmh: Option<f64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct WeatherSnapshot {
    pub location: ResolvedLocation,
    pub provider: &'static str,
    pub fetched_at: String,
    pub current: Option<CurrentWeather>,
    pub daily: Vec<DailyWeather>,
    pub warnings: Vec<String>,
    pub attributions: Vec<String>,
}

impl WeatherSnapshot {
    pub fn status(&self) -> &'static str {
        if self.warnings.is_empty() {
            "ok"
        } else {
            "partial"
        }
    }
}
