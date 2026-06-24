//! 插件协议(JSONL,见 production/0.2-core-plugin-design.md §3.2)。
//!
//! newline-delimited JSON,每行一个完整 JSON。本切片实现 `query`→`response`(单行)与
//! core→插件单向 `cancel`(查询超时发送,插件可忽略);流式(stream/delta/done)/ attachments 暂不实现。
//!
//! bin crate 无 lib target,示例插件目前各持一份本 struct 的副本(后续抽 SDK crate)。

use serde::{Deserialize, Serialize};

/// core → 插件请求。`type` 标签区分 query/cancel。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum PluginRequest {
    /// 查询。
    Query {
        id: String,
        query: String,
        #[serde(default)]
        context: PluginQueryContext,
    },
    /// 取消(core→插件,best-effort:查询超时发送,插件可忽略)。
    Cancel { id: String },
}

impl PluginRequest {
    /// 构造一条 query 请求。
    pub fn query(id: impl Into<String>, query: impl Into<String>) -> Self {
        PluginRequest::Query {
            id: id.into(),
            query: query.into(),
            context: PluginQueryContext::default(),
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

/// 插件 → core 响应(一行 = 一个完整 JSON)。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginResponse {
    pub id: String,
    #[serde(default)]
    pub items: Vec<PluginItem>,
    #[serde(default)]
    pub error: Option<PluginErrorPayload>,
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
}
