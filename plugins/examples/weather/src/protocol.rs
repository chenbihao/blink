//! 与宿主兼容的 JSONL 消息；不包含供应商或展示业务。
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
pub enum CoreToPlugin {
    #[serde(rename = "query")]
    Query {
        id: String,
        query: String,
        #[serde(default)]
        settings: Option<serde_json::Value>,
        #[serde(default)]
        budget_ms: Option<u64>,
    },
    #[serde(rename = "http_response")]
    HttpResponse {
        id: String,
        status: u16,
        #[serde(default)]
        body: Option<String>,
        #[serde(default)]
        error: Option<String>,
    },
    #[serde(rename = "cancel")]
    Cancel { id: String },
    #[serde(rename = "tool_call")]
    ToolCall {
        id: String,
        tool_name: String,
        #[serde(default)]
        arguments: serde_json::Value,
        #[serde(default)]
        settings: Option<serde_json::Value>,
        #[serde(default)]
        budget_ms: Option<u64>,
    },
}

#[derive(Debug, Serialize)]
#[serde(tag = "type")]
pub enum PluginToCore {
    #[serde(rename = "response")]
    Response(PluginResponse),
    #[serde(rename = "http_request")]
    HttpRequest(HttpRequest),
    #[serde(rename = "raw_result")]
    RawResult(RawToolResult),
}

#[derive(Debug, Serialize)]
pub struct RawToolResult {
    pub id: String,
    pub data: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<PluginError>,
}

#[derive(Debug, Serialize)]
pub struct HttpRequest {
    pub id: String,
    pub method: String,
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    pub timeout_ms: u64,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub headers: Vec<(String, String)>,
}

#[derive(Debug, Serialize)]
pub struct PluginResponse {
    pub id: String,
    pub items: Vec<PluginItem>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<PluginError>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PluginError {
    pub code: String,
    pub message: String,
}

#[derive(Debug, Serialize)]
pub struct PluginItem {
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subtitle: Option<String>,
    pub score: f32,
    pub action: PluginAction,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub payload: Option<serde_json::Value>,
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum PluginAction {
    #[serde(rename = "none")]
    None,
    Copy {
        text: String,
    },
    /// 在系统浏览器/资源管理器打开 URL 或路径（与宿主 PluginAction::Open 同形）。
    Open {
        path: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_without_optional_arguments_keeps_default_configuration() {
        let request: CoreToPlugin = serde_json::from_value(serde_json::json!({
            "type":"tool_call", "id":"tc_1", "tool_name":"get_weather",
            "settings":{"default_city":"广州"}
        }))
        .unwrap();
        match request {
            CoreToPlugin::ToolCall {
                arguments,
                settings,
                ..
            } => {
                assert!(arguments.is_null());
                assert_eq!(settings.unwrap()["default_city"], "广州");
            }
            _ => panic!("expected tool call"),
        }
    }

    #[test]
    fn raw_result_preserves_single_item_array() {
        let response = PluginToCore::RawResult(RawToolResult {
            id: "tc_1".into(),
            data: serde_json::json!([{"city":"广州","temp":"25°C","daily":[]}]),
            error: None,
        });
        let value = serde_json::to_value(response).unwrap();
        assert_eq!(value["type"], "raw_result");
        assert_eq!(value["data"].as_array().unwrap().len(), 1);
        assert_eq!(value["data"][0]["city"], "广州");
        assert!(value.get("error").is_none());
    }
}
