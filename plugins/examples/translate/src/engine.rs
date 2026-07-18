//! 翻译引擎 trait + 请求结构。

use serde_json::Value;

/// 引擎构造的 HTTP 请求（通过 core 代理发送）。
pub struct EngineRequest {
    pub method: String,
    pub url: String,
    pub body: Option<String>,
    pub timeout_ms: u64,
    pub headers: Vec<(String, String)>,
}

/// 翻译引擎 trait——5 个引擎各实现它。
///
/// **build_request**：构造 HTTP 请求参数（含签名）。配置缺失（如 API key 为空）返回 None。
/// **parse_response**：从 HTTP 响应 body 解析译文。失败返回 None（触发降级）。
pub trait TranslateEngine {
    fn build_request(&self, text: &str, target_lang: &str, settings: &Value) -> Option<EngineRequest>;
    fn parse_response(&self, body: &str) -> Option<String>;
}
