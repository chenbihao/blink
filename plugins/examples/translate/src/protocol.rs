//! JSONL 协议消息定义（与主程序 protocol.rs 字段对齐）。

use serde::{Deserialize, Serialize};

/// core → 插件的消息。
#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
pub enum CoreToPlugin {
    #[serde(rename = "query")]
    Query {
        id: String,
        query: String,
        #[serde(default)]
        settings: Option<serde_json::Value>,
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
    Cancel {
        #[allow(dead_code)]
        id: String,
    },
    #[serde(rename = "tool_call")]
    ToolCall {
        id: String,
        tool_name: String,
        #[serde(default)]
        arguments: serde_json::Value,
        #[serde(default)]
        settings: Option<serde_json::Value>,
    },
}

/// 插件 → core 的上行消息。
#[derive(Debug, Serialize)]
#[serde(tag = "type")]
pub enum PluginToCore {
    #[serde(rename = "response")]
    Response(PluginResponse),
    #[serde(rename = "http_request")]
    HttpRequest(HttpRequest),
    #[serde(rename = "tool_result")]
    ToolResult(ToolResultPayload),
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

#[derive(Debug, Deserialize, Serialize)]
pub struct HttpResponse {
    pub id: String,
    pub status: u16,
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default)]
    pub error: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct PluginResponse {
    pub id: String,
    pub items: Vec<PluginItem>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<PluginError>,
}

#[derive(Debug, Serialize)]
pub struct ToolResultPayload {
    pub id: String,
    pub items: Vec<PluginItem>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<PluginError>,
}

#[derive(Debug, Serialize)]
pub struct PluginError {
    pub code: String,
    pub message: String,
}

/// 插件结果项（0.11.0 改进 1：加 payload 字段，主动填结构化数据给 AI）。
#[derive(Debug, Serialize, Default)]
pub struct PluginItem {
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subtitle: Option<String>,
    pub score: f32,
    pub action: PluginAction,
    /// 结构化数据（0.11.0）：译文项填 {text: 译文}，供 AI 直接读。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub payload: Option<serde_json::Value>,
}

#[derive(Debug, Serialize, Default)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum PluginAction {
    #[default]
    None,
    Copy {
        text: String,
    },
    Open {
        path: String,
    },
}
