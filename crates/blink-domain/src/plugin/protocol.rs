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
//! **stderr 级别标记约定**：core 逐行转发插件 stderr 到 tracing,默认 debug。
//! 行内含 `[error]`/`[warn]`/`[info]` 方括号标记时（如 `[translate][error] ...`）
//! 按标记级别转发,让引擎失败等真实错误在默认日志级别（warn+）可见,无需开 trace。
//!
//! bin crate 无 lib target,示例插件目前各持一份本 struct 的副本(后续抽 SDK crate)。

use serde::{Deserialize, Serialize};

/// core 支持的插件进程协议版本上限（0.28.1）。
///
/// - v1：现状——宿主不发初始化帧，插件冷启动直接收 query/tool_call。
///   **v1 插件不得被强发不认识的帧**，宿主对 v1 保持完全旧行为。
/// - v2：启动握手——宿主 spawn 后先发 `initialize`（声明宿主能力与数据目录），
///   插件回 `initialized`（声明实际协议版本）。manifest `protocol_version` 声明。
///
/// 0.28.13 在 v2 帧面追加数据集自举帧（`dataset_init`/`fetch_resource`/
/// `install_dataset` 等）——协议版本不递增：旧 v2 插件不声明
/// `dataset_init` 能力就永远收不到这些业务帧，与 v1 保护同构。
pub const SUPPORTED_PROTOCOL_VERSION: u32 = 2;

/// 宿主 initialize 能力：HTTP 代理执行（既有）。
pub const CAPABILITY_HTTP_PROXY: &str = "http_proxy";
/// 宿主 initialize 能力（0.28.13）：数据集自举——宿主可代插件下载源锁定
/// 声明内的资源并挂载插件构建的数据集。插件侧在 manifest
/// `capabilities` 声明同名能力后才收到 `dataset_init` 帧。
pub const CAPABILITY_DATASET_INIT: &str = "dataset_init";

/// core → 插件请求。`type` 标签区分 query/cancel/http_response。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum PluginRequest {
    /// v2 启动握手（0.28.1）——spawn 后、任何业务帧之前发送，仅一次。
    ///
    /// 只发给 manifest 声明 `protocol_version >= 2` 的插件；v1 插件收不到本帧。
    /// 插件以 `PluginUpstreamMessage::Initialized` 应答；在应答到达前宿主
    /// 缓冲业务帧（0.28.1：等待有界超时，超时按 v1 行为放行——插件可忽略握手）。
    #[serde(rename = "initialize")]
    Initialize {
        /// 宿主声明的协议版本（= SUPPORTED_PROTOCOL_VERSION）。
        protocol: u32,
        /// 宿主提供给插件的能力（当前固定 ["http_proxy"]——HTTP 代理执行）。
        #[serde(default)]
        capabilities: Vec<String>,
        /// 宿主为该插件分配的私有目录（绝对路径，逐插件隔离）。
        /// v1 插件不收本帧、也就没有这些目录——按需从参数/配置取数据。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        dirs: Option<PluginDirs>,
    },
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
        /// 从宿主总 deadline 扣除准备/排队时间后的剩余预算；旧插件可忽略。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        budget_ms: Option<u64>,
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
        #[serde(default, skip_serializing_if = "Option::is_none")]
        budget_ms: Option<u64>,
    },
    /// 数据集自举启动请求（0.28.13，握手后的独立操作）。
    ///
    /// 只发给 manifest `capabilities` 声明 `dataset_init` 且宿主支持该能力的
    /// 插件；一次一个数据集（必备库与可选词源是独立操作，各自重试）。
    /// 构建可达分钟级——本帧**不设应答 deadline**，完成以插件回
    /// `init_result` 为准（进程退出/操作取消由宿主自举管理器兜底）。
    #[serde(rename = "dataset_init")]
    DatasetInit {
        id: String,
        /// 宿主自举操作 id（后续 fetch_resource/install_dataset/stage 均携带）。
        op_id: String,
        /// 目标数据集（必须在 manifest datasets 声明中）。
        dataset_id: String,
        /// 宿主为本次操作分配的工作目录（cache 下独立 op 目录，绝对路径）：
        /// `resources/` 放宿主下载的源文件，插件自建 `build/` 放构建产物。
        work_dir: String,
    },
    /// `fetch_resource` 的应答（成功）——资源已下载并通过源锁定 sha256 校验。
    /// `path` 为 work_dir 下的**受限相对路径**（`resources/<resource_id>`）。
    #[serde(rename = "resource_result")]
    ResourceResult {
        id: String,
        op_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        path: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        bytes: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sha256: Option<String>,
        /// 源锁定声明的上游修订/许可（插件写进 dataset.json 溯源字段）。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        revision: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        license: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<PluginErrorPayload>,
    },
    /// `install_dataset` 的应答——数据集已过宿主校验并提交挂载（active.json 切换）。
    #[serde(rename = "dataset_result")]
    DatasetResult {
        id: String,
        op_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        version: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<PluginErrorPayload>,
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
            budget_ms: None,
        }
    }
}

/// 宿主为单个插件分配的目录集合（v2 握手下发，0.28.1）。
///
/// 三个目录职责分离：data 卸载时可保留（用户数据）、cache 可随时重建、
/// datasets 挂版本化数据包。插件只看到自己的目录，不获知其他插件路径。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PluginDirs {
    /// 插件私有数据目录（用户数据，卸载默认保留）。
    pub data: String,
    /// 插件私有缓存目录（可重建，卸载/清理直接删）。
    pub cache: String,
    /// 插件数据包根目录（版本化词库等挂载点）。
    pub datasets: String,
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
    pub fn from_snapshot(snapshot: &blink_infra::platform::context::ContextSnapshot) -> Self {
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
    /// v2 握手应答（0.28.1）——插件收到 `initialize` 后回此帧。
    ///
    /// `protocol` 为插件实际选择的版本（可低于宿主声明，宿主按插件版本走）。
    /// v1 插件永不发本帧。
    #[serde(rename = "initialized")]
    Initialized {
        /// 插件实际使用的协议版本。
        protocol: u32,
    },
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
    /// 数据集自举：插件请求宿主下载源锁定资源（0.28.13）。
    ///
    /// 宿主按**当前已校验插件版本随包的源锁定声明**（bootstrap.json）解析
    /// URL/hash/bytes——运行时消息不能改写期望值；资源 id 不在声明中即拒绝。
    #[serde(rename = "fetch_resource")]
    FetchResource {
        id: String,
        op_id: String,
        resource_id: String,
    },
    /// 数据集自举：插件请求宿主挂载本操作构建输出的数据集（0.28.13）。
    ///
    /// `source_dir` 是 work_dir 下的受限相对路径（宿主校验真实路径与链接
    /// 边界，不接受任意文件系统路径）；dataset.json 的 plugin_id/dataset_id
    /// 必须与进程身份及 manifest 声明相符。
    #[serde(rename = "install_dataset")]
    InstallDataset {
        id: String,
        op_id: String,
        dataset_id: String,
        source_dir: String,
    },
    /// 数据集自举：插件上报阶段（0.28.13，信息性——不伪造百分比）。
    /// `stage` 为插件自定义短标签（如 "building"/"verifying"）。
    #[serde(rename = "init_stage")]
    InitStage {
        op_id: String,
        stage: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        note: Option<String>,
    },
    /// 数据集自举：操作终态（0.28.13）。error 为 None 表示插件侧流程完成
    /// （若 install_dataset 已成功挂载，宿主以挂载结果为准——迟到失败不回滚）。
    #[serde(rename = "init_result")]
    InitResult {
        op_id: String,
        dataset_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<PluginErrorPayload>,
    },
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
    /// 给人看的元信息（成功时可带，如翻译插件标注实际使用的引擎）。
    /// core 注入 `CapabilityResult.desc`——AI 出口不读 desc，展示/命令层可读。
    /// 老插件不带此字段（serde default，向后兼容）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub desc: Option<String>,
}

impl RawToolResult {
    /// 构造一个成功的纯数据结果。
    #[allow(dead_code)] // 主 crate 通过反序列化构造；插件示例 crate 有各自副本
    pub fn ok(id: impl Into<String>, data: serde_json::Value) -> Self {
        RawToolResult {
            id: id.into(),
            data,
            error: None,
            desc: None,
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
            desc: None,
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
        PluginRawResult { data, error: None }
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
    /// 打开**本插件**声明的独立页面（0.28.4 词典详情等）。
    ///
    /// 故意不带 plugin_id——归属由宿主按结果来源插件注入（engine::to_search_item），
    /// 插件不能宣称其他插件的身份开窗（phase §3.2）。宿主侧映射为
    /// `open_plugin_page` 内置动作，页面/启用态在打开时复核。
    /// `page_id: None` = manifest 首个页面；payload 为唤起载荷（如待查单词）。
    #[serde(rename = "open_page", rename_all = "snake_case")]
    OpenPage {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        page_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        payload: Option<serde_json::Value>,
    },
    /// 以新查询重路由主窗输入（0.28.4 词典未收录转翻译）——前端回写输入框
    /// 并派发一轮新搜索（与 args.hints 引导项同语义）。插件只给查询文本，
    /// 是否命中/由谁接管由宿主路由决定；原输入保留在输入框（可改可撤）。
    Route { query: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── 0.28.1 v2 握手帧 ─────────────────────────────────────────────────

    #[test]
    fn initialize_frame_serializes_with_dirs() {
        let req = PluginRequest::Initialize {
            protocol: 2,
            capabilities: vec!["http_proxy".into()],
            dirs: Some(PluginDirs {
                data: "D:/appdata/blink/plugins/data/extra.dict".into(),
                cache: "D:/appdata/blink/plugins/cache/extra.dict".into(),
                datasets: "D:/appdata/blink/plugins/datasets/extra.dict".into(),
            }),
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("\"type\":\"initialize\""));
        assert!(json.contains("\"protocol\":2"));
        assert!(json.contains("\"http_proxy\""));
        assert!(json.contains("\"datasets\""));
    }

    #[test]
    fn initialize_frame_without_dirs_omits_field() {
        let req = PluginRequest::Initialize {
            protocol: 2,
            capabilities: vec![],
            dirs: None,
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(!json.contains("\"dirs\""));
    }

    #[test]
    fn initialized_ack_roundtrips() {
        let json = r#"{"type":"initialized","protocol":2}"#;
        let msg: PluginUpstreamMessage = serde_json::from_str(json).unwrap();
        match msg {
            PluginUpstreamMessage::Initialized { protocol } => assert_eq!(protocol, 2),
            _ => panic!("应是 Initialized"),
        }
    }

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
    fn open_page_action_roundtrips_and_omits_optional_fields() {
        // 完整形态
        let item: PluginItem = serde_json::from_str(
            r#"{"title":"t","score":0.9,"action":{"type":"open_page","page_id":"main","payload":{"word":"apple"}}}"#,
        )
        .unwrap();
        match &item.action {
            PluginAction::OpenPage { page_id, payload } => {
                assert_eq!(page_id.as_deref(), Some("main"));
                assert_eq!(payload.as_ref().unwrap()["word"], "apple");
            }
            other => panic!("应是 OpenPage: {other:?}"),
        }
        // 缺省形态：page_id/payload 均省略 → None（老消费者/极简声明兼容）
        let minimal: PluginItem = serde_json::from_str(
            r#"{"title":"t","action":{"type":"open_page"}}"#,
        )
        .unwrap();
        assert!(matches!(
            minimal.action,
            PluginAction::OpenPage { page_id: None, payload: None }
        ));
    }

    #[test]
    fn route_action_roundtrips() {
        let item: PluginItem = serde_json::from_str(
            r#"{"title":"未收录","action":{"type":"route","query":"翻译 apple"}}"#,
        )
        .unwrap();
        match item.action {
            PluginAction::Route { query } => assert_eq!(query, "翻译 apple"),
            other => panic!("应是 Route: {other:?}"),
        }
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
            budget_ms: Some(800),
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("\"settings\""));
        assert!(json.contains("\"use_ipv6\""));
        assert!(json.contains("\"budget_ms\":800"));
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
            budget_ms: None,
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
            budget_ms: None,
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
                budget_ms,
            } => {
                assert_eq!(id, "tc_1");
                assert_eq!(tool_name, "translate");
                assert_eq!(arguments["text"], "hello");
                assert!(settings.is_none());
                assert!(budget_ms.is_none());
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
        let json =
            r#"{"data":[{"ip":"192.168.1.1","type":"本地"},{"ip":"8.8.8.8","type":"公网"}]}"#;
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

    // ── 0.28.13 数据集自举帧 ──────────────────────────────────────────────

    #[test]
    fn dataset_init_frame_serializes_with_type_tag() {
        let req = PluginRequest::DatasetInit {
            id: "init_1".into(),
            op_id: "boot-1a".into(),
            dataset_id: "dictionary-basic".into(),
            work_dir: "D:/appdata/blink/plugins/cache/builtin.dictionary/ops/boot-1a".into(),
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("\"type\":\"dataset_init\""));
        assert!(json.contains("\"op_id\":\"boot-1a\""));
        assert!(json.contains("\"dataset_id\":\"dictionary-basic\""));
        assert!(json.contains("\"work_dir\""));
    }

    #[test]
    fn fetch_resource_and_install_dataset_upstream_roundtrip() {
        let fetch: PluginUpstreamMessage = serde_json::from_str(
            r#"{"type":"fetch_resource","id":"fr_1","op_id":"boot-1a","resource_id":"ecdict-csv"}"#,
        )
        .unwrap();
        match fetch {
            PluginUpstreamMessage::FetchResource { id, op_id, resource_id } => {
                assert_eq!(id, "fr_1");
                assert_eq!(op_id, "boot-1a");
                assert_eq!(resource_id, "ecdict-csv");
            }
            _ => panic!("应是 FetchResource"),
        }
        let install: PluginUpstreamMessage = serde_json::from_str(
            r#"{"type":"install_dataset","id":"di_1","op_id":"boot-1a","dataset_id":"dictionary-basic","source_dir":"build/dictionary-basic/1.1.0"}"#,
        )
        .unwrap();
        match install {
            PluginUpstreamMessage::InstallDataset { id, dataset_id, source_dir, .. } => {
                assert_eq!(id, "di_1");
                assert_eq!(dataset_id, "dictionary-basic");
                assert_eq!(source_dir, "build/dictionary-basic/1.1.0");
            }
            _ => panic!("应是 InstallDataset"),
        }
    }

    #[test]
    fn resource_result_and_dataset_result_serialize_restricted_fields() {
        let ok = PluginRequest::ResourceResult {
            id: "fr_1".into(),
            op_id: "boot-1a".into(),
            path: Some("resources/ecdict-csv".into()),
            bytes: Some(1024),
            sha256: Some("a".repeat(64)),
            revision: Some("8defb761".into()),
            license: Some("MIT".into()),
            error: None,
        };
        let json = serde_json::to_string(&ok).unwrap();
        assert!(json.contains("\"type\":\"resource_result\""));
        assert!(json.contains("resources/ecdict-csv"));
        assert!(!json.contains("\"error\""));

        let err = PluginRequest::DatasetResult {
            id: "di_1".into(),
            op_id: "boot-1a".into(),
            version: None,
            error: Some(PluginErrorPayload {
                code: "path_escape".into(),
                message: "source_dir 越界".into(),
            }),
        };
        let json = serde_json::to_string(&err).unwrap();
        assert!(json.contains("\"type\":\"dataset_result\""));
        assert!(json.contains("path_escape"));
    }

    #[test]
    fn init_stage_and_init_result_roundtrip() {
        let stage: PluginUpstreamMessage = serde_json::from_str(
            r#"{"type":"init_stage","op_id":"boot-1a","stage":"building"}"#,
        )
        .unwrap();
        match stage {
            PluginUpstreamMessage::InitStage { op_id, stage, note } => {
                assert_eq!(op_id, "boot-1a");
                assert_eq!(stage, "building");
                assert!(note.is_none());
            }
            _ => panic!("应是 InitStage"),
        }
        let done: PluginUpstreamMessage = serde_json::from_str(
            r#"{"type":"init_result","op_id":"boot-1a","dataset_id":"dictionary-basic"}"#,
        )
        .unwrap();
        match done {
            PluginUpstreamMessage::InitResult { op_id, dataset_id, error } => {
                assert_eq!(op_id, "boot-1a");
                assert_eq!(dataset_id, "dictionary-basic");
                assert!(error.is_none());
            }
            _ => panic!("应是 InitResult"),
        }
    }
}
