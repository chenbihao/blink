//! 插件协议(JSONL,见 phases/0.2-core-plugin-design.md §3.2)。
//!
//! newline-delimited JSON,每行一个完整 JSON。本切片实现:
//! - `query`→`response`(单行查询)
//! - core→插件单向 `cancel`(查询超时发送,插件可忽略)
//! - `tool_call`→`raw_result`(0.14.3 AI tool-call 执行，轨道 A 纯数据)
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
    /// tool-call 执行请求(core→插件,0.9.3 引入，0.14.3 改为轨道 A 纯数据返回)。
    ///
    /// AI 路由产出 tool_call → `PluginCapabilityAdapter` →
    /// adapter 通过 `PluginHandle::execute_tool_raw()` 发此消息到插件子进程。
    /// 插件执行后返回 `RawResult`（轨道 A 纯 data）。
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
/// 包含普通查询响应、插件发起的 HTTP 请求、轨道 A 纯数据 tool 结果。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum PluginUpstreamMessage {
    /// 查询结果响应(插件→core)。
    #[serde(rename = "response")]
    Response(PluginResponse),
    /// HTTP 请求(插件→core):请求 core 代为发起 HTTP 请求。
    #[serde(rename = "http_request")]
    HttpRequest(HttpRequest),
    /// 轨道 A 纯数据 tool 结果(插件→core,0.14.3)。
    ///
    /// manifest 配了 `projection` 的插件走轨道 A:只返回纯 `data`，
    /// core 的 `PluginCapabilityAdapter` 用 `ProjectionRule` 投影成 `CapabilityResult`。
    #[serde(rename = "raw_result")]
    RawResult(RawToolResult),
}

/// 轨道 A 纯数据 tool 结果（0.14.3）——插件只吐纯 `data`，投影规则在 manifest。
///
/// manifest 配了 `projection` 的插件走轨道 A，返回本结构。
/// core 的 `PluginCapabilityAdapter` 收到后用 `ProjectionRule` 投影成 `CapabilityResult`。
///
/// **wire 格式**：`{"type":"raw_result","id":"...","data":<任意 JSON>}`
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RawToolResult {
    /// 关联的请求 id（与 `PluginRequest::ToolCall.id` 对应）。
    pub id: String,
    /// 纯数据，零展示逻辑。翻译插件: `"你好"`。IP 插件: `[{ip, type}, ...]`。
    pub data: serde_json::Value,
    /// 执行错误（成功时为 None）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<PluginErrorPayload>,
}

impl RawToolResult {
    /// 构造一个成功的纯数据结果。
    #[allow(dead_code)] // 主 crate 通过反序列化构造；插件示例 crate 有各自副本
    pub fn ok(id: impl Into<String>, data: serde_json::Value) -> Self {
        RawToolResult {
            id: id.into(),
            data,
            error: None,
        }
    }

    /// 构造一个错误结果。
    #[allow(dead_code)] // 同 ok
    pub fn err(id: impl Into<String>, code: impl Into<String>, message: impl Into<String>) -> Self {
        RawToolResult {
            id: id.into(),
            data: serde_json::Value::Null,
            error: Some(PluginErrorPayload {
                code: code.into(),
                message: message.into(),
            }),
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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PluginErrorPayload {
    pub code: String,
    pub message: String,
}

/// 插件运行时纯数据输出（0.14 轨道 A）——插件只吐纯 data，投影规则在 manifest。
///
/// **核心思想**（§3.1）：插件开发者只关心"返回正确的数据"。
/// 翻译插件: `data = "你好"`。IP 插件: `data = [{ip, type}, ...]`。
/// 怎么展示 / 怎么投影由 manifest 的 `ProjectionRule` 配置决定。
///
/// **与旧 `PluginResponse` 的关系**：`PluginRawResult` 是新协议（轨道 A），
/// `PluginResponse` 是旧协议（`{items: Vec<PluginItem>}`）。0.14.3 插件迁移后
/// 旧协议废弃。0.14.0 只定义结构，`PluginCapabilityAdapter` 的迁移在 0.14.3。
#[allow(dead_code)] // 0.14.0 定义结构，0.14.3 插件迁移时消费
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PluginRawResult {
    /// 纯数据，零展示逻辑。翻译插件: `"你好"`。IP 插件: `[{ip, type}, ...]`。
    pub data: serde_json::Value,
    /// 执行错误（成功时为 None）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<PluginErrorPayload>,
}

#[allow(dead_code)] // 0.14.0 定义结构，0.14.3 插件迁移时消费
impl PluginRawResult {
    /// 构造一个成功的纯数据结果。
    pub fn ok(data: serde_json::Value) -> Self {
        PluginRawResult {
            data,
            error: None,
        }
    }

    /// 构造一个错误结果。
    pub fn err(code: impl Into<String>, message: impl Into<String>) -> Self {
        PluginRawResult {
            data: serde_json::Value::Null,
            error: Some(PluginErrorPayload {
                code: code.into(),
                message: message.into(),
            }),
        }
    }
}

/// 插件结果项。
///
/// **0.11.0 改进 1**：新增 `payload: Option<Value>` 字段——结构化数据给 AI 读，
/// 与 `title`（前端展示用）分离。与 `CapabilityResult::Items` 的 `ItemResult.payload`
/// 设计对齐："前端展示用 title/subtitle，AI 读 payload"，两套消费分离。
///
/// 向后兼容：老插件不填 payload 时为 `None`，消费方从 `action` 提取兜底
/// （Copy→`{text}`，Open→`{path}`）。
///
/// **0.14 注**：此结构在 0.14.3 插件迁移后将由 `PluginRawResult`（轨道 A）取代。
/// 0.14.0 仅定义新结构，不删除旧结构。
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

    // ── 0.14 PluginRawResult（轨道 A 纯数据输出）─────────────────────────

    #[test]
    fn plugin_raw_result_ok_constructs() {
        let r = PluginRawResult::ok(serde_json::json!("你好"));
        assert_eq!(r.data, "你好");
        assert!(r.error.is_none());
    }

    #[test]
    fn plugin_raw_result_err_constructs() {
        let r = PluginRawResult::err("TIMEOUT", "查询超时");
        assert!(r.data.is_null());
        assert_eq!(r.error.as_ref().unwrap().code, "TIMEOUT");
        assert_eq!(r.error.as_ref().unwrap().message, "查询超时");
    }

    #[test]
    fn plugin_raw_result_serializes_with_data() {
        let r = PluginRawResult::ok(serde_json::json!({ "translated": "你好" }));
        let json = serde_json::to_string(&r).unwrap();
        assert!(json.contains("\"data\""));
        assert!(json.contains("\"translated\""));
        assert!(json.contains("\"你好\""));
        // error=None 时 skip_serializing_if 生效
        assert!(!json.contains("\"error\""));
    }

    #[test]
    fn plugin_raw_result_serializes_with_error() {
        let r = PluginRawResult::err("FAIL", "服务不可用");
        let json = serde_json::to_string(&r).unwrap();
        assert!(json.contains("\"error\""));
        assert!(json.contains("\"FAIL\""));
        assert!(json.contains("\"服务不可用\""));
    }

    #[test]
    fn plugin_raw_result_parses_from_json() {
        let json = r#"{"data":"你好世界"}"#;
        let r: PluginRawResult = serde_json::from_str(json).unwrap();
        assert_eq!(r.data, "你好世界");
        assert!(r.error.is_none());
    }

    #[test]
    fn plugin_raw_result_parses_array_data() {
        // IP 插件场景：data 是数组
        let json = r#"{"data":[{"ip":"192.168.1.1","type":"本地"},{"ip":"8.8.8.8","type":"公网"}]}"#;
        let r: PluginRawResult = serde_json::from_str(json).unwrap();
        assert!(r.data.is_array());
        assert_eq!(r.data.as_array().unwrap().len(), 2);
        assert_eq!(r.data[0]["ip"], "192.168.1.1");
    }

    #[test]
    fn plugin_raw_result_roundtrip() {
        let r = PluginRawResult::ok(serde_json::json!({
            "city": "北京",
            "temp": 25,
            "condition": "晴"
        }));
        let json = serde_json::to_string(&r).unwrap();
        let restored: PluginRawResult = serde_json::from_str(&json).unwrap();
        assert_eq!(r.data, restored.data);
        assert_eq!(r.error, restored.error);
    }

    // ── 0.14.3 RawToolResult（轨道 A 纯数据 tool 结果）─────────────────────

    #[test]
    fn raw_tool_result_ok_constructs() {
        let r = RawToolResult::ok("tc_1", serde_json::json!("你好"));
        assert_eq!(r.id, "tc_1");
        assert_eq!(r.data, "你好");
        assert!(r.error.is_none());
    }

    #[test]
    fn raw_tool_result_err_constructs() {
        let r = RawToolResult::err("tc_1", "FAIL", "翻译失败");
        assert_eq!(r.id, "tc_1");
        assert!(r.data.is_null());
        assert_eq!(r.error.as_ref().unwrap().code, "FAIL");
        assert_eq!(r.error.as_ref().unwrap().message, "翻译失败");
    }

    #[test]
    fn raw_tool_result_serializes_with_type_tag() {
        let r = RawToolResult::ok("tc_1", serde_json::json!("你好"));
        let wrapped = PluginUpstreamMessage::RawResult(r);
        let json = serde_json::to_string(&wrapped).unwrap();
        assert!(json.contains("\"type\":\"raw_result\""));
        assert!(json.contains("\"id\":\"tc_1\""));
        assert!(json.contains("\"data\""));
        assert!(json.contains("\"你好\""));
        // error=None 时 skip_serializing_if 生效
        assert!(!json.contains("\"error\""));
    }

    #[test]
    fn raw_tool_result_parses_from_json() {
        let json = r#"{"type":"raw_result","id":"tc_1","data":{"ip":"192.168.1.1","type":"本地"}}"#;
        let msg: PluginUpstreamMessage = serde_json::from_str(json).unwrap();
        match msg {
            PluginUpstreamMessage::RawResult(r) => {
                assert_eq!(r.id, "tc_1");
                assert_eq!(r.data["ip"], "192.168.1.1");
                assert!(r.error.is_none());
            }
            _ => panic!("应是 RawResult"),
        }
    }

    #[test]
    fn raw_tool_result_with_error_parses() {
        let json = r#"{"type":"raw_result","id":"tc_2","data":null,"error":{"code":"FAIL","message":"错误"}}"#;
        let msg: PluginUpstreamMessage = serde_json::from_str(json).unwrap();
        match msg {
            PluginUpstreamMessage::RawResult(r) => {
                assert_eq!(r.id, "tc_2");
                assert!(r.data.is_null());
                assert_eq!(r.error.as_ref().unwrap().code, "FAIL");
            }
            _ => panic!("应是 RawResult"),
        }
    }

    #[test]
    fn raw_tool_result_array_data_parses() {
        // IP 插件场景：data 是数组
        let json = r#"{"type":"raw_result","id":"tc_3","data":[{"ip":"192.168.1.1","type":"本地"},{"ip":"8.8.8.8","type":"公网"}]}"#;
        let msg: PluginUpstreamMessage = serde_json::from_str(json).unwrap();
        match msg {
            PluginUpstreamMessage::RawResult(r) => {
                assert!(r.data.is_array());
                assert_eq!(r.data.as_array().unwrap().len(), 2);
            }
            _ => panic!("应是 RawResult"),
        }
    }
}
