use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CoreToPlugin {
    Query {
        id: String,
        query: String,
        settings: Option<Value>,
        budget_ms: Option<u64>,
    },
    ToolCall {
        id: String,
        tool_name: String,
        #[serde(default)]
        arguments: Value,
        settings: Option<Value>,
        budget_ms: Option<u64>,
    },
    HttpResponse {
        id: String,
        status: u16,
        body: Option<String>,
        error: Option<String>,
    },
    Cancel {
        id: String,
    },
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PluginToCore {
    Response {
        id: String,
        items: Vec<PluginItem>,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<PluginError>,
    },
    RawResult {
        id: String,
        data: Value,
        error: Option<PluginError>,
    },
    HttpRequest {
        id: String,
        method: &'static str,
        url: &'static str,
        timeout_ms: u64,
        headers: Vec<(String, String)>,
    },
}

#[derive(Debug, Serialize)]
pub struct PluginError {
    pub code: &'static str,
    pub message: String,
}

#[derive(Debug, Serialize)]
pub struct PluginItem {
    pub title: String,
    pub subtitle: String,
    pub score: f32,
    pub action: PluginAction,
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum PluginAction {
    Copy { text: String },
    None,
}
