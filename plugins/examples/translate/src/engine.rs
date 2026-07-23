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
/// **build_request**：构造单次翻译的 HTTP 请求（含签名）。配置缺失（如 API key 为空）返回 None。
/// **parse_response**：从单次翻译的 HTTP 响应 body 解析译文。失败返回 None（触发降级）。
///
/// ## 批量翻译能力（可选）
///
/// 引擎可声明是否支持原生批量翻译（`supports_batch`）。支持时由调用方优先走
/// `build_batch_request` / `parse_batch_response`,引擎自己保证顺序与输入一一对应;
/// 不支持时,调用方降级到 tag 拼接单次请求(`build_tagged_batch` / `parse_tagged_batch`)。
///
/// **新增引擎默认 `supports_batch() == false`,无需实装批量方法,向后兼容。**
pub trait TranslateEngine {
    fn build_request(
        &self,
        text: &str,
        target_lang: &str,
        settings: &Value,
    ) -> Option<EngineRequest>;
    fn parse_response(&self, body: &str) -> Option<String>;

    /// 是否支持原生批量翻译。默认 false。
    /// true 时 `try_translate_batch` 优先走 `build_batch_request`。
    fn supports_batch(&self) -> bool {
        false
    }

    /// 是否支持 tag 拼接批量（用私用区字符分隔多行，一次请求翻译后按 tag 拆回）。
    /// 默认 true。签名包含 text 内容的引擎（如有道）应返回 false，
    /// 因为私用区字符参与签名会导致签名校验失败。
    fn supports_tag_batch(&self) -> bool {
        true
    }

    /// 原生批量请求构造。`texts` 非空,`supports_batch() == true` 时才会被调用。
    /// 返回 None 视为实装不完整,调用方降级到 tag 拼接。
    fn build_batch_request(
        &self,
        _texts: &[String],
        _target_lang: &str,
        _settings: &Value,
    ) -> Option<EngineRequest> {
        None
    }

    /// 原生批量响应解析。返回 Vec 长度必须 == `expected`,且顺序与输入一一对应,
    /// 否则视为失败,调用方降级到 tag 拼接或单行并发。
    fn parse_batch_response(&self, _body: &str, _expected: usize) -> Option<Vec<String>> {
        None
    }
}
