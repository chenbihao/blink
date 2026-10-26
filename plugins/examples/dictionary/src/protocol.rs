//! JSONL 协议消息定义（与宿主 crates/blink-domain/src/plugin/protocol.rs 对齐）。
//!
//! 词典插件声明 protocol_version 2——宿主 spawn 后会先发 `initialize`（携带
//! data/cache/datasets 三个私有目录），本插件应答 `initialized` 后才收业务帧。

use serde::{Deserialize, Serialize};

/// 宿主为插件分配的私有目录（v2 initialize 帧）。
#[derive(Debug, Clone, Deserialize)]
pub struct PluginDirs {
    #[serde(default)]
    #[allow(dead_code)] // wire 对齐：本插件只消费 datasets
    pub data: Option<String>,
    #[serde(default)]
    #[allow(dead_code)] // 同上
    pub cache: Option<String>,
    #[serde(default)]
    pub datasets: Option<String>,
}

/// core → 插件的消息。
#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
pub enum CoreToPlugin {
    #[serde(rename = "initialize")]
    Initialize {
        protocol: u32,
        #[serde(default)]
        dirs: Option<PluginDirs>,
    },
    #[serde(rename = "query")]
    Query {
        id: String,
        query: String,
        #[serde(default)]
        settings: Option<serde_json::Value>,
    },
    #[serde(rename = "tool_call")]
    ToolCall {
        id: String,
        tool_name: String,
        #[serde(default)]
        arguments: serde_json::Value,
        /// 词典无配置项，settings 不消费（wire 对齐保留）。
        #[serde(default)]
        #[allow(dead_code)]
        settings: Option<serde_json::Value>,
    },
    #[serde(rename = "cancel")]
    Cancel {
        #[allow(dead_code)]
        id: String,
    },
}

/// 插件 → core 的上行消息。
#[derive(Debug, Serialize)]
#[serde(tag = "type")]
pub enum PluginToCore {
    #[serde(rename = "initialized")]
    Initialized {
        protocol: u32,
    },
    #[serde(rename = "response")]
    Response(PluginResponse),
    /// 轨道 A 纯数据 tool 结果——manifest 配了 projection 的 tool 走此路径。
    #[serde(rename = "raw_result")]
    RawResult(RawToolResult),
}

#[derive(Debug, Serialize)]
pub struct PluginResponse {
    pub id: String,
    pub items: Vec<PluginItem>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<PluginError>,
}

#[derive(Debug, Serialize)]
pub struct RawToolResult {
    pub id: String,
    pub data: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<PluginError>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub desc: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct PluginError {
    pub code: String,
    pub message: String,
}

/// 插件结果项（字段与宿主协议对齐；payload 供 AI/宿主读结构化数据）。
#[derive(Debug, Serialize, Default)]
pub struct PluginItem {
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subtitle: Option<String>,
    pub score: f32,
    pub action: PluginAction,
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
    /// 打开本插件声明的独立页面（宿主注入归属；page_id 缺省取首个页面）。
    #[serde(rename = "open_page")]
    OpenPage {
        #[serde(skip_serializing_if = "Option::is_none")]
        page_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        payload: Option<serde_json::Value>,
    },
    /// 以新查询重路由主窗输入（未收录转翻译）。
    Route {
        query: String,
    },
}
