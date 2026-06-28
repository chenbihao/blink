//! 插件协议(JSONL,见 production-design/phases/0.2-core-plugin-design.md §3.2)。
//!
//! newline-delimited JSON,每行一个完整 JSON。本切片实现 `query`→`response`(单行)与
//! core→插件单向 `cancel`(查询超时发送,插件可忽略);流式(stream/delta/done)/ attachments 暂不实现。
//!
//! bin crate 无 lib target,示例插件目前各持一份本 struct 的副本(后续抽 SDK crate)。

use serde::{Deserialize, Serialize};

/// core → 插件请求。`type` 标签区分 query/cancel/http_response。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum PluginRequest {
    /// 查询。
    #[serde(rename = "query")]
    Query {
        id: String,
        query: String,
        #[serde(default)]
        context: PluginQueryContext,
        /// 该插件的 PluginConfig.settings（0.5.1 透传,见 0.5 设计 §2.4「settings 透传协议」）。
        /// 采用 query 内联:每次查询携带,天然热更新。老插件忽略此字段;无配置时为 None。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        settings: Option<serde_json::Value>,
    },
    /// 取消(core→插件,best-effort:查询超时发送,插件可忽略)。
    #[serde(rename = "cancel")]
    Cancel { id: String },
    /// HTTP 响应(core→插件):插件之前发起的 http_request 的结果。
    #[serde(rename = "http_response")]
    HttpResponse {
        id: String,
        status: u16,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        body: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
}

impl PluginRequest {
    /// 构造一条 query 请求。
    #[allow(dead_code)] // 便利函数，未来可能直接使用
    pub fn query(id: impl Into<String>, query: impl Into<String>) -> Self {
        PluginRequest::Query {
            id: id.into(),
            query: query.into(),
            context: PluginQueryContext::default(),
            settings: None,
        }
    }
}

/// 查询上下文(随请求传给插件;包含环境上下文供插件决策)。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PluginQueryContext {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lang: Option<String>,
    /// 前台应用进程名（如 "code.exe"）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub foreground_app: Option<String>,
    /// 前台窗口标题
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_title: Option<String>,
    /// 剪贴板文本（截断 200 字符）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clipboard_text: Option<String>,
}

impl PluginQueryContext {
    /// 从 ContextSnapshot 转换为插件协议格式。
    pub fn from_snapshot(snapshot: &crate::context::ContextSnapshot) -> Self {
        let (foreground_app, window_title) = match &snapshot.foreground_app {
            Some(app) => (
                Some(app.process_name.clone()),
                Some(app.window_title.clone()),
            ),
            None => (None, None),
        };
        PluginQueryContext {
            lang: None, // 后续加配置项预留
            foreground_app,
            window_title,
            clipboard_text: snapshot.clipboard_text.clone(),
        }
    }
}

/// 插件 → core 的上行消息(一行 = 一个完整 JSON)。
/// 包含普通查询响应和插件发起的 HTTP 请求。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum PluginUpstreamMessage {
    /// 查询结果响应(插件→core)。
    #[serde(rename = "response")]
    Response(PluginResponse),
    /// HTTP 请求(插件→core):请求 core 代为发起 HTTP 请求。
    #[serde(rename = "http_request")]
    HttpRequest(HttpRequest),
}

/// 插件 → core 响应(一行 = 一个完整 JSON)。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginResponse {
    pub id: String,
    #[serde(default)]
    pub items: Vec<PluginItem>,
    #[serde(default)]
    pub error: Option<PluginErrorPayload>,
}

/// 插件发起的 HTTP 请求(通过 core 代理)。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HttpRequest {
    pub id: String,
    pub method: String,
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    #[serde(default = "default_http_timeout")]
    pub timeout_ms: u64,
}

fn default_http_timeout() -> u64 {
    10000
}

/// 插件返回的错误。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginErrorPayload {
    pub code: String,
    pub message: String,
}

/// 插件结果项。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginItem {
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subtitle: Option<String>,
    #[serde(default = "default_score")]
    pub score: f32,
    pub action: PluginAction,
}

fn default_score() -> f32 {
    0.5
}

/// 插件结果项的动作(结构化 tagged,避免字符串与 payload 的隐式约定)。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum PluginAction {
    /// 复制文本到剪贴板。
    Copy { text: String },
    /// 打开路径(应用/文件/URL)。空 path = 纯展示项。
    Open { path: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_serializes_with_type_tag() {
        let req = PluginRequest::query("req_1", "hello");
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("\"type\":\"query\""));
        assert!(json.contains("\"id\":\"req_1\""));
        assert!(json.contains("\"query\":\"hello\""));
    }

    #[test]
    fn response_parses_items() {
        let json = r#"{"id":"req_1","items":[{"title":"本机 IP","subtitle":"复制","score":0.9,"action":{"type":"copy","text":"192.168.1.5"}}]}"#;
        let resp: PluginResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.id, "req_1");
        assert_eq!(resp.items.len(), 1);
        assert_eq!(resp.items[0].title, "本机 IP");
        assert!(matches!(&resp.items[0].action, PluginAction::Copy { text } if text == "192.168.1.5"));
        assert!(resp.error.is_none());
    }

    #[test]
    fn response_defaults_score_and_empty_items() {
        let json = r#"{"id":"x"}"#;
        let resp: PluginResponse = serde_json::from_str(json).unwrap();
        assert!(resp.items.is_empty());

        let json2 = r#"{"id":"y","items":[{"title":"t","action":{"type":"open","path":""}}]}"#;
        let resp2: PluginResponse = serde_json::from_str(json2).unwrap();
        assert_eq!(resp2.items[0].score, 0.5); // default
    }

    #[test]
    fn cancel_request_serializes_with_type_tag() {
        let req = PluginRequest::Cancel { id: "req_x".into() };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("\"type\":\"cancel\""));
        assert!(json.contains("\"id\":\"req_x\""));
    }

    #[test]
    fn query_with_settings_serializes_field() {
        let settings = serde_json::json!({"use_ipv6": true});
        let req = PluginRequest::Query {
            id: "r1".into(),
            query: "ip".into(),
            context: PluginQueryContext::default(),
            settings: Some(settings),
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("\"settings\""));
        assert!(json.contains("\"use_ipv6\""));
    }

    #[test]
    fn legacy_request_without_settings_parses() {
        // 老插件/老 core 发的请求无 settings 字段 → serde default 补 None,向后兼容。
        let json = r#"{"type":"query","id":"r1","query":"ip","context":{}}"#;
        let req: PluginRequest = serde_json::from_str(json).unwrap();
        match req {
            PluginRequest::Query { settings, .. } => assert!(settings.is_none()),
            _ => panic!("应是 Query"),
        }
    }

    #[test]
    fn wrapped_response_serializes_with_type_tag() {
        // 新协议：PluginUpstreamMessage::Response 包装响应
        let resp = PluginResponse {
            id: "req_1".into(),
            items: vec![PluginItem {
                title: "本机 IP".into(),
                subtitle: None,
                score: 0.9,
                action: PluginAction::Copy { text: "192.168.1.5".into() },
            }],
            error: None,
        };
        let wrapped = PluginUpstreamMessage::Response(resp);
        let json = serde_json::to_string(&wrapped).unwrap();
        assert!(json.contains("\"type\":\"response\""));
        assert!(json.contains("\"id\":\"req_1\""));
    }

    #[test]
    fn http_request_serializes_with_type_tag() {
        let req = HttpRequest {
            id: "http_1".into(),
            method: "GET".into(),
            url: "https://api.example.com".into(),
            body: None,
            timeout_ms: 10000,
        };
        let wrapped = PluginUpstreamMessage::HttpRequest(req);
        let json = serde_json::to_string(&wrapped).unwrap();
        assert!(json.contains("\"type\":\"http_request\""));
        assert!(json.contains("\"method\":\"GET\""));
        assert!(json.contains("\"url\":\"https://api.example.com\""));
    }

    #[test]
    fn http_response_serializes_with_type_tag() {
        let resp = PluginRequest::HttpResponse {
            id: "http_1".into(),
            status: 200,
            body: Some("ok".into()),
            error: None,
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"type\":\"http_response\""));
        assert!(json.contains("\"status\":200"));
        assert!(json.contains("\"body\":\"ok\""));
    }
}
