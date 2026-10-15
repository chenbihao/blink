//! 供应商无关的天气数值模型；格式化仅在出口发生。
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
}

#[derive(Debug, Serialize)]
pub struct CurrentWeather {
    pub temperature_c: Option<f64>,
    pub condition_code: Option<u32>,
    pub wind_speed_kmh: Option<f64>,
    pub valid_at: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct DailyWeather {
    pub date: String,
    pub condition_code: Option<u32>,
    pub temperature_max_c: Option<f64>,
    pub temperature_min_c: Option<f64>,
    pub precipitation_probability_percent: Option<f64>,
    pub wind_max_kmh: Option<f64>,
}

#[derive(Debug, Serialize)]
pub struct WeatherSnapshot {
    pub location: ResolvedLocation,
    pub provider: &'static str,
    pub fetched_at: String,
    pub current: Option<CurrentWeather>,
    pub daily: Vec<DailyWeather>,
    pub warnings: Vec<String>,
    pub attributions: Vec<&'static str>,
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
