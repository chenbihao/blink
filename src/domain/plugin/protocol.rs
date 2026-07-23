//! 插件协议(JSONL,见 production-design/phases/0.2-core-plugin-design.md §3.2)。
//!
//! newline-delimited JSON,每行一个完整 JSON。本切片实现:
//! - `query`→`response`(单行查询)
//! - core→插件单向 `cancel`(查询超时发送,插件可忽略)
//! - `tool_call`→`tool_result`(0.9.3 AI tool-call 执行)
//! - `http_request`→`http_response`(插件发起 HTTP 请求,core 代为执行)
//!
//! 流式(stream/delta/done)/ attachments 暂不实现。
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
    /// tool-call 执行请求(core→插件,0.9.3)。
    ///
    /// AI 路由产出 tool_call → `ActionRegistry` 查到 `PluginActionAdapter` →
    /// adapter 通过 `PluginHandle::execute_tool()` 发此消息到插件子进程。
    /// 插件执行后返回 `ToolResult`。
    #[serde(rename = "tool_call")]
    ToolCall {
        id: String,
        /// 对应 manifest.tools[].name
        tool_name: String,
        /// AI 产出的参数(JSON Object)
        arguments: serde_json::Value,
        /// 该插件的 PluginConfig.settings（与 Query 同规则透传）
        #[serde(default, skip_serializing_if = "Option::is_none")]
        settings: Option<serde_json::Value>,
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
///
/// 0.8.6 §8.2.2：`clipboard_text` 已移除——插件直接读 Awareness 违反四域架构。
/// 插件想要环境信息须走 Suggestion 域 → 用户 Tab 采纳 → `ExecArg::UserExplicit` 注入。
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
}

impl PluginQueryContext {
    /// 从 ContextSnapshot 转换为插件协议格式。
    pub fn from_snapshot(snapshot: &crate::infra::platform::context::ContextSnapshot) -> Self {
        let (foreground_app, window_title) = match &snapshot.foreground_app {
            Some(app) => (
                Some(app.process_name.clone()),
                Some(app.window_title.clone()),
            ),
            None => (None, None),
        };
        PluginQueryContext {
            lang: None,
            foreground_app,
            window_title,
        }
    }
}

/// 插件 → core 的上行消息(一行 = 一个完整 JSON)。
/// 包含普通查询响应、插件发起的 HTTP 请求、tool-call 结果。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum PluginUpstreamMessage {
    /// 查询结果响应(插件→core)。
    #[serde(rename = "response")]
    Response(PluginResponse),
    /// HTTP 请求(插件→core):请求 core 代为发起 HTTP 请求。
    #[serde(rename = "http_request")]
    HttpRequest(HttpRequest),
    /// tool-call 执行结果(插件→core,0.9.3)。
    #[serde(rename = "tool_result")]
    ToolResult(ToolResultPayload),
}

/// 插件返回的 tool-call 执行结果(0.9.3)。
///
/// **与 `PluginResponse` 统一格式**——`items` 复用 `PluginItem`，
/// 插件的 `handle_query` 和 `handle_tool_call` 可共用结果构造逻辑。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResultPayload {
    /// 关联的请求 id(与 `PluginRequest::ToolCall.id` 对应)。
    pub id: String,
    /// 结构化结果项——与 `PluginResponse.items` 同格式。
    /// 第一项的 `title` 作为 AI 回答展示；`action` 决定回车行为。
    #[serde(default)]
    pub items: Vec<PluginItem>,
    /// 执行错误(成功时为 None)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<PluginErrorPayload>,
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
    /// HTTP 请求头（0.11.6 翻译插件 tencent 引擎需要 Authorization 等自定义头）。
    /// 向后兼容：老插件不填 → 空数组 → body 有时默认 Content-Type: application/json。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub headers: Vec<(String, String)>,
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
///
/// **0.11.0 改进 1**：新增 `payload: Option<Value>` 字段——结构化数据给 AI 读，
/// 与 `title`（前端展示用）分离。与 `CapabilityResult::Items` 的 `ItemResult.payload`
/// 设计对齐："前端展示用 title/subtitle，AI 读 payload"，两套消费分离。
///
/// 向后兼容：老插件不填 payload 时为 `None`，消费方从 `action` 提取兜底
/// （Copy→`{text}`，Open→`{path}`）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginItem {
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subtitle: Option<String>,
    #[serde(default = "default_score")]
    pub score: f32,
    pub action: PluginAction,
    /// 结构化 payload——给 AI 读（任意 JSON）。0.11.0 新增，向后兼容。
    ///
    /// **约定**：缺失时（老插件）由消费方从 `action` 提取兜底。
    /// 插件作者应在 tool-call 路径下主动填此字段，让 AI 拿到结构化数据
    /// 而非从展示文本反推。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload: Option<serde_json::Value>,
}

fn default_score() -> f32 {
    0.5
}

impl Default for PluginItem {
    /// 便利构造：`PluginItem { title, action, ..Default::default() }`
    /// 让新增字段（如 payload）时已有构造点零改动。
    fn default() -> Self {
        PluginItem {
            title: String::new(),
            subtitle: None,
            score: default_score(),
            action: PluginAction::None,
            payload: None,
        }
    }
}

/// 插件结果项的动作(结构化 tagged,避免字符串与 payload 的隐式约定)。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum PluginAction {
    /// 纯展示项，无操作。
    None,
    /// 复制文本到剪贴板。
    Copy { text: String },
    /// 打开路径(应用/文件/URL)。
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
        assert!(
            matches!(&resp.items[0].action, PluginAction::Copy { text } if text == "192.168.1.5")
        );
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

        let json3 = r#"{"id":"z","items":[{"title":"hint","action":{"type":"none"}}]}"#;
        let resp3: PluginResponse = serde_json::from_str(json3).unwrap();
        assert!(matches!(resp3.items[0].action, PluginAction::None));
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
                action: PluginAction::Copy {
                    text: "192.168.1.5".into(),
                },
                ..Default::default()
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
            headers: vec![],
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

    // ── 0.9.3 tool_call / tool_result ────────────────────────────────────

    #[test]
    fn tool_call_request_serializes_with_type_tag() {
        let req = PluginRequest::ToolCall {
            id: "tc_1".into(),
            tool_name: "translate".into(),
            arguments: serde_json::json!({ "text": "hello", "target_lang": "zh" }),
            settings: None,
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("\"type\":\"tool_call\""));
        assert!(json.contains("\"tool_name\":\"translate\""));
        assert!(json.contains("\"text\":\"hello\""));
        // settings=None 时 skip_serializing_if 生效
        assert!(!json.contains("\"settings\""));
    }

    #[test]
    fn tool_call_request_with_settings() {
        let req = PluginRequest::ToolCall {
            id: "tc_2".into(),
            tool_name: "translate".into(),
            arguments: serde_json::json!({ "text": "hi" }),
            settings: Some(serde_json::json!({ "engine": "deepl" })),
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("\"settings\""));
        assert!(json.contains("\"engine\":\"deepl\""));
    }

    #[test]
    fn tool_call_request_parses_from_json() {
        let json = r#"{"type":"tool_call","id":"tc_1","tool_name":"translate","arguments":{"text":"hello"}}"#;
        let req: PluginRequest = serde_json::from_str(json).unwrap();
        match req {
            PluginRequest::ToolCall {
                id,
                tool_name,
                arguments,
                settings,
            } => {
                assert_eq!(id, "tc_1");
                assert_eq!(tool_name, "translate");
                assert_eq!(arguments["text"], "hello");
                assert!(settings.is_none());
            }
            _ => panic!("应是 ToolCall"),
        }
    }

    #[test]
    fn tool_result_upstream_serializes_with_type_tag() {
        let msg = PluginUpstreamMessage::ToolResult(ToolResultPayload {
            id: "tc_1".into(),
            items: vec![PluginItem {
                title: "你好".into(),
                subtitle: Some("翻译自: hello".into()),
                score: 1.0,
                action: PluginAction::Copy {
                    text: "你好".into(),
                },
                payload: Some(serde_json::json!({ "translated": "你好", "source": "hello" })),
                ..Default::default()
            }],
            error: None,
        });
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("\"type\":\"tool_result\""));
        assert!(json.contains("\"你好\""));
        assert!(json.contains("\"翻译自: hello\""));
        assert!(!json.contains("\"error\""));
        // 0.11.0: payload 字段正确序列化
        assert!(json.contains("\"payload\""));
        assert!(json.contains("\"translated\""));
    }

    #[test]
    fn tool_result_with_error() {
        let msg = PluginUpstreamMessage::ToolResult(ToolResultPayload {
            id: "tc_1".into(),
            items: vec![],
            error: Some(PluginErrorPayload {
                code: "EXEC_FAILED".into(),
                message: "翻译服务不可用".into(),
            }),
        });
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("\"type\":\"tool_result\""));
        assert!(json.contains("\"EXEC_FAILED\""));
        assert!(json.contains("\"翻译服务不可用\""));
    }

    #[test]
    fn tool_result_parses_from_json() {
        let json = r#"{"type":"tool_result","id":"tc_1","items":[{"title":"你好","score":1.0,"action":{"type":"copy","text":"你好"}}]}"#;
        let msg: PluginUpstreamMessage = serde_json::from_str(json).unwrap();
        match msg {
            PluginUpstreamMessage::ToolResult(payload) => {
                assert_eq!(payload.id, "tc_1");
                assert_eq!(payload.items.len(), 1);
                assert_eq!(payload.items[0].title, "你好");
                assert!(payload.error.is_none());
            }
            _ => panic!("应是 ToolResult"),
        }
    }

    #[test]
    fn tool_result_empty_items_with_error() {
        // 空 items + error = 失败场景
        let json = r#"{"type":"tool_result","id":"tc_1","items":[],"error":{"code":"FAIL","message":"err"}}"#;
        let msg: PluginUpstreamMessage = serde_json::from_str(json).unwrap();
        match msg {
            PluginUpstreamMessage::ToolResult(payload) => {
                assert!(payload.items.is_empty());
                assert!(payload.error.is_some());
            }
            _ => panic!("应是 ToolResult"),
        }
    }

    // ── 0.11.0: PluginItem.payload 向后兼容 ────────────────────────────────

    #[test]
    fn legacy_plugin_item_without_payload_parses_as_none() {
        // 老插件/core 发的 PluginItem 无 payload 字段 → serde default 补 None,向后兼容。
        // 这是 0.11.0 改进 1 的核心契约:老插件不填 payload 仍能正常工作。
        let json =
            r#"{"title":"本机 IP","score":0.9,"action":{"type":"copy","text":"192.168.1.5"}}"#;
        let item: PluginItem = serde_json::from_str(json).unwrap();
        assert_eq!(item.title, "本机 IP");
        assert!(item.payload.is_none(), "老 JSON 无 payload 应解析为 None");
    }

    #[test]
    fn plugin_item_with_payload_round_trips() {
        // 新插件填 payload → 序列化/反序列化 round-trip 保持
        let item = PluginItem {
            title: "公网 IP: 1.2.3.4".into(),
            subtitle: Some("北京 | 按 Enter 复制".into()),
            score: 0.9,
            action: PluginAction::Copy {
                text: "1.2.3.4".into(),
            },
            payload: Some(serde_json::json!({
                "ip": "1.2.3.4",
                "type": "public",
                "city": "北京"
            })),
        };
        let json = serde_json::to_string(&item).unwrap();
        let restored: PluginItem = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.title, item.title);
        assert_eq!(restored.payload, item.payload);
        // payload 是结构化 JSON,AI 可按字段读
        assert_eq!(restored.payload.unwrap()["ip"], "1.2.3.4");
    }

    #[test]
    fn plugin_item_payload_skipped_when_none() {
        // payload=None 时 skip_serializing_if 生效,JSON 不含 payload 字段
        // —— 避免对老协议消费方产生干扰,且省 bytes
        let item = PluginItem {
            title: "bare".into(),
            score: 0.5,
            action: PluginAction::None,
            ..Default::default()
        };
        let json = serde_json::to_string(&item).unwrap();
        assert!(!json.contains("payload"), "payload=None 时不应序列化");
    }
}
