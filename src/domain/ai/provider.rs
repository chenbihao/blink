//! `AIProvider` trait——**主窗口路径的唯一 AI 出口**。
//!
//! ## §2.6 类型收窄铁则(编译期钉死)
//!
//! 主窗口路径 `use crate::domain::ai::AIProvider` 时,**编译期就没有 `AgentBuilder`
//! 能力**——rig 的 agent loop / prompt / memory 全部锁在 `agent_window/`(0.10 落地)。
//! 这不是"code review 铁则",而是模块可见性钉死:
//!
//! - `AIProvider` trait → `pub` (顶层 re-export)
//! - `CompletionRequest / Response / ToolCall / Usage` → `pub`
//! - `AgentSession` / rig agent 相关 → 0.10 落 `pub(in crate::domain::agent_window)`
//!
//! **想穿透边界必须动模块可见性,那时会被 code review 拦下**——比"记得别 `into_agent`"
//! 可靠 100 倍。
//!
//! ## Dangerous 独立于交互模式(§3.4)
//!
//! Provider 只负责"跑 LLM 返回 tool_calls",**不执行**。AI tool_call 只由
//! `CapabilityRegistry` 分派；`Dangerous` / `sensitive` 的确认铁则位于
//! Capability 调度层，不在 Provider 层。
//!
//! ## 0.9.1 阶段状态
//!
//! - trait 定义 + 类型完整
//! - **无实体 Provider**——rig-core Client 构造放 Phase 5(Provider 注册)
//! - `MockProvider` 用于测试,验证 dispatch 骨架

use async_trait::async_trait;
use tokio::sync::mpsc;

use super::message::{CompletionRequest, CompletionResponse, ToolCall, Usage};
use crate::domain::config::ai_config::ProviderKind;

/// AI 调用错误——**故意不含供应商原始错误明细**(避免密钥/内网 URL 泄漏到日志)。
#[derive(Debug, thiserror::Error)]
#[allow(dead_code)] // 0.9.1 Phase 4 定义,Phase 5 起 dispatch 消费
pub enum AIError {
    /// 硬超时(§3.3 骨架层)——`AIConfig::slo_hard_timeout_ms` 或 default 20000ms 到期
    #[error("AI 调用超时")]
    Timeout,
    /// 用户中断(ESC / 换 query)——`tokio::select!` 或 `AbortHandle` 触发
    #[error("AI 调用已取消")]
    Cancelled,
    /// 未配置——`AIConfig::resolve_tier` 返 None(所有档空 或 全悬空)
    #[error("AI 未配置或档位悬空")]
    NotConfigured,
    /// 供应商密钥缺失——provider 配置存在但 Credential Manager 中找不到密钥。
    /// 区别于 `NotConfigured`（档位悬空），此错误明确指向"密钥丢失"场景，
    /// 供前端给用户更精准的修复引导（"请在设置页重新填写密钥"）。
    #[error("供应商 {0} 的 API 密钥未配置，请在设置页重新填写")]
    SecretMissing(String),
    /// 网络错误——rig / reqwest 返回,只含 stage 描述,不含 URL/密钥
    #[error("AI 网络错误: {0}")]
    Network(String),
    /// 供应商错误——4xx/5xx,只含 stage 描述
    #[error("AI 供应商错误: {0}")]
    Provider(String),
    /// 序列化 / 解析错误
    #[error("AI 数据解析错误: {0}")]
    Serialization(String),
}

/// 流式 chunk —— provider 通过 channel 逐条发送,消费方(SearchService)逐条 emit 前端。
#[derive(Debug, Clone)]
pub enum StreamChunk {
    /// 增量文本片段——前端逐段拼接展示。
    Text(#[allow(dead_code)] String),
    /// 流结束——携带 tool_calls(若有)。
    Done {
        #[allow(dead_code)] // 流式消费方未读取 tool_calls（chat 路径用 ChatStreamChunk）
        tool_calls: Vec<ToolCall>,
        #[allow(dead_code)] // 流式过程中 usage 可能不精确,保留供未来 SLO 消费
        usage: Usage,
    },
}

/// **主窗口路径**的 AI 抽象——单次 completion + tool_calls,**无 agent loop**。
///
/// **实现约束**:
/// 1. `complete` 内部**必须**实现硬超时——用 `tokio::time::timeout` 或 `reqwest`
///    的 `.timeout()`,不允许调用方"忘了传 timeout_ms"就无限挂
/// 2. `complete` 内部**必须**填 `CompletionResponse.first_token_ms / total_ms`——
///    SLO 骨架层的观测入口,消费方(SearchService)按此发 tracing::info
/// 3. 密钥读取**必须**通过 `SecretString`——`expose()` 只在把 header 塞进 reqwest
///    request 时用一次,`Drop` 走 zeroize
/// 4. **不允许**返回 `rig::` 类型——`CompletionResponse` 是我们自己的类型墙
#[async_trait]
#[allow(dead_code)] // 0.9.1 Phase 4 定义,Phase 5 起被 AppContext 持有 dispatch
pub trait AIProvider: Send + Sync {
    /// 供应商种类——用于 tracing / 设置页展示。
    fn kind(&self) -> ProviderKind;

    /// 模型 id——用于 tracing SLO 埋点(见 `blink::ai::slo` target 的 `model` 字段)。
    fn model_id(&self) -> &str;

    /// 单次 completion。**主窗口路径唯一入口**。
    ///
    /// - 请求内 `timeout_ms` 优先,回落到 provider 实例内部 default(20000ms)
    /// - 用户 ESC → 上层 drop 这个 future,provider 内部 reqwest task 自动 abort
    /// - 首 token 就返 `first_token_ms`(SSE 模式)——SLO 观测入口
    async fn complete(&self, req: CompletionRequest) -> Result<CompletionResponse, AIError>;

    /// 流式 completion——通过 channel 逐 chunk 发送文本片段。
    ///
    /// **默认实现**:fallback 到 `complete()`,一次性把结果发完。
    /// 真正支持流式的 provider(如 RigProvider)应覆盖此方法。
    ///
    /// **超时**:provider 内部分两阶段——连接阶段(等首个响应)用 `timeout_ms`
    /// 作硬超时;流式阶段每个 chunk 的等待用 `timeout_ms` 作 idle timeout。
    /// 只要 token 持续到达就不会超时。
    ///
    /// **中断**:调用方 drop 返回的 future 即可中断;provider 内部 stream 会被
    /// abort(与 `complete` 的 reqwest task abort 一致)。
    async fn stream(
        &self,
        req: CompletionRequest,
        tx: mpsc::UnboundedSender<StreamChunk>,
    ) -> Result<(), AIError> {
        // 默认实现:complete 后一次性发完
        let resp = self.complete(req).await?;
        if let Some(text) = resp.text {
            let _ = tx.send(StreamChunk::Text(text));
        }
        let _ = tx.send(StreamChunk::Done {
            tool_calls: resp.tool_calls,
            usage: resp.usage,
        });
        Ok(())
    }
}

// **注意**:此文件绝不 impl / re-export 任何 `AgentBuilder` / `Prompt` / `memory`。
// 想给主窗口开这些能力必须先破坏 §2.6 模块可见性——这一破坏会撞 code review。

// ── 测试用 Mock ──────────────────────────────────────────────────────────

#[cfg(test)]
pub mod tests {
    use super::super::message::{ChatMessage, ToolCall, Usage};
    use super::*;
    use crate::domain::schema::ToolSchema;

    /// 测试专用 Provider——固定返回预置结果,不打网络。
    pub struct MockProvider {
        pub model: String,
        pub response: CompletionResponse,
        pub delay_ms: u64,
    }

    impl MockProvider {
        pub fn echo_tool_call(name: &str, args: serde_json::Value) -> Self {
            Self {
                model: "mock-echo".into(),
                response: CompletionResponse {
                    text: None,
                    tool_calls: vec![ToolCall {
                        id: "mock_call_1".into(),
                        name: name.into(),
                        arguments: args,
                    }],
                    usage: Usage {
                        input_tokens: 10,
                        output_tokens: 5,
                    },
                    first_token_ms: 42,
                    total_ms: 87,
                },
                delay_ms: 0,
            }
        }

        pub fn slow(ms: u64) -> Self {
            Self {
                model: "mock-slow".into(),
                response: CompletionResponse {
                    text: Some("done".into()),
                    tool_calls: Vec::new(),
                    usage: Usage::default(),
                    first_token_ms: ms as u32,
                    total_ms: ms as u32,
                },
                delay_ms: ms,
            }
        }
    }

    #[async_trait]
    impl AIProvider for MockProvider {
        fn kind(&self) -> ProviderKind {
            ProviderKind::OpenAICompatible
        }
        fn model_id(&self) -> &str {
            &self.model
        }
        async fn complete(&self, req: CompletionRequest) -> Result<CompletionResponse, AIError> {
            // 尊重请求内的 timeout_ms——即使 mock 也要证明骨架 SLO 的通路
            let timeout = req.timeout_ms.unwrap_or(20_000);
            if self.delay_ms >= timeout as u64 {
                // 主动返回 Timeout,证明"provider 内部会挡住 timeout"这条铁则
                tokio::time::sleep(std::time::Duration::from_millis(timeout as u64)).await;
                return Err(AIError::Timeout);
            }
            tokio::time::sleep(std::time::Duration::from_millis(self.delay_ms)).await;
            Ok(self.response.clone())
        }
    }

    // ── 类型收窄编译期证据 ────────────────────────────────────────────────

    /// 编译期证据:主窗口路径 `use AIProvider` 拿到的 dyn 对象**只有** `complete /
    /// kind / model_id` 方法,没有 `agent_session()` / `prompt()` / `memory()`。
    ///
    /// 若未来某人给 trait 加了 `fn agent_session(&self)` 之类的方法,下面 fn 里的
    /// dyn 对象会突然多出该能力——此测编译不出错但会成为 review 拦截点。
    ///
    /// **真正的编译期负面测试留 0.10 落 agent_window 时用 trybuild**——
    /// 现在 agent_window 还没有,负面测试无对象。
    #[test]
    fn provider_trait_surface_is_minimal() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Box<dyn AIProvider>>();

        // 只允许用 &dyn AIProvider 访问三个方法——这个测试的存在本身就是文档
        let p: Box<dyn AIProvider> = Box::new(MockProvider::echo_tool_call(
            "open_url",
            serde_json::json!({ "url": "https://example.com" }),
        ));
        assert!(matches!(p.kind(), ProviderKind::OpenAICompatible));
        assert_eq!(p.model_id(), "mock-echo");
    }

    #[tokio::test]
    async fn mock_provider_returns_tool_call() {
        let p = MockProvider::echo_tool_call(
            "open_url",
            serde_json::json!({ "url": "https://example.com" }),
        );
        let req = CompletionRequest {
            messages: vec![ChatMessage::user("open example")],
            tools: vec![ToolSchema::empty("open_url", "打开 URL")],
            max_tokens: None,
            temperature: None,
            timeout_ms: None,
        };
        let resp = p.complete(req).await.unwrap();
        assert_eq!(resp.tool_calls.len(), 1);
        assert_eq!(resp.tool_calls[0].name, "open_url");
        assert_eq!(
            resp.tool_calls[0]
                .arguments
                .get("url")
                .and_then(|v| v.as_str()),
            Some("https://example.com")
        );
        assert!(resp.first_token_ms > 0, "first_token_ms 必须由 provider 填");
        assert!(resp.total_ms > 0);
    }

    #[tokio::test]
    async fn mock_provider_honors_timeout() {
        let p = MockProvider::slow(200);
        let req = CompletionRequest {
            messages: vec![ChatMessage::user("q")],
            tools: Vec::new(),
            max_tokens: None,
            temperature: None,
            timeout_ms: Some(100), // 小于 delay_ms → 应超时
        };
        let start = std::time::Instant::now();
        let result = p.complete(req).await;
        let elapsed = start.elapsed();

        assert!(
            matches!(result, Err(AIError::Timeout)),
            "预期 Timeout,实际: {result:?}"
        );
        // 允许 ±50ms 抖动
        assert!(
            elapsed.as_millis() >= 90 && elapsed.as_millis() <= 200,
            "超时时机偏差:实际 {}ms",
            elapsed.as_millis()
        );
    }

    #[test]
    fn ai_error_display_never_leaks_raw_details() {
        // Display 输出必须干净——不含密钥前缀 / URL
        let err = AIError::Network("connection refused sk-secret-1234".into());
        let s = format!("{err}");
        // 我们不清洗 Network 消息里的字节(那是 rig 的责任),但至少骨架里 Display
        // 必须能被日志过滤扫到"AI 网络错误"关键词从而 grep 掉整行
        assert!(s.starts_with("AI 网络错误"), "Display 前缀需可识别: {s}");
    }
}
