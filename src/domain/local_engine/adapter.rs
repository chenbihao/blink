//! 本地引擎 adapter 契约（0.22.3）。
//!
//! adapter 实现引擎特有行为：
//! - 从已校验配置构造受限启动描述
//! - service/model health 映射
//! - adapter self-test
//! - 引擎专属诊断/DTO 投影边界
//!
//! ## 设计铁则
//!
//! - **adapter 不能接收前端提供的 executable、argv、脚本路径、环境变量或任意 URL**。
//!   `LaunchDescriptor` 是 adapter 内部生成的受限结构，不接受外部字符串注入。
//! - **adapter 是 provider-neutral 的**：不假设 Python/PyTorch/Paddle，
//!   各引擎 adapter 只从自己的 descriptor + 配置产生启动描述。
//! - **domain 不发送 Tauri 事件**：adapter 返回纯数据，由 app 层桥接成事件。
//! - **adapter 不被设计成任意进程托管器**：它只服务于编译期内置的引擎。

use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::identity::{BackendObservation, ResolvedProfile};

use super::descriptor::EngineDefinition;
use super::error::LocalEngineError;
use super::status::{EnvironmentHealth, ModelHealth, ServiceHealth};

// ── LaunchDescriptor ───────────────────────────────────────────────────────

/// 受限启动描述（adapter 内部生成，不接受外部字符串注入）。
///
/// adapter 从已校验配置 + descriptor 产生此结构。
/// `executable` 和 `args` 由 provider/adapter 从 locked artifact 解析，
/// 不从前端接收。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LaunchDescriptor {
    /// 可执行文件路径（由 adapter 从 resolved profile 的 locked artifact 解析）。
    pub executable: PathBuf,
    /// 启动参数（由 adapter 从 descriptor 的 model_contract 等生成）。
    pub args: Vec<String>,
    /// 工作目录（由 adapter 从引擎目录解析）。
    pub current_dir: Option<PathBuf>,
    /// 受限环境变量（由 adapter 从 descriptor 配置生成，不接收前端注入）。
    pub env: HashMap<String, String>,
    /// 引擎 instance label（日志和诊断用）。
    pub label: String,
}

// ── HealthMapping ──────────────────────────────────────────────────────────

/// adapter 从引擎特有的 health 协议映射出的 service/model 健康状态。
///
/// adapter 负责把各引擎不同的 health 响应格式映射为领域统一的状态。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HealthMapping {
    /// 服务健康状态。
    pub service: ServiceHealth,
    /// 模型健康状态。
    pub model: ModelHealth,
    /// 环境健康状态（可选，部分 adapter 在 health 中也回报环境状态）。
    pub environment: Option<EnvironmentHealth>,
    /// backend 观测（可选，部分 adapter 在 health 中回报 actual backend）。
    pub backend: Option<BackendObservation>,
    /// 模型 id（可选，health 回报的实际模型 id）。
    pub model_id: Option<String>,
    /// 模型 revision（可选，health 回报的实际模型 revision）。
    pub model_revision: Option<String>,
    /// 模型内容指纹（可选，health 回报的实际模型文件内容指纹）。
    ///
    /// 与 `model_revision` 分离：revision 是逻辑版本标识，
    /// fingerprint 是实际缓存文件的内容哈希，用于检测模型文件损坏/篡改。
    pub model_content_fingerprint: Option<String>,
}

// ── AdapterSelfTest ──────────────────────────────────────────────────────────

/// adapter self-test 结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdapterSelfTest {
    /// 是否通过。
    pub passed: bool,
    /// 失败原因（如果未通过）。
    pub failure_reason: Option<String>,
}

impl AdapterSelfTest {
    /// 构造通过的 self-test 结果。
    pub fn passed() -> Self {
        Self {
            passed: true,
            failure_reason: None,
        }
    }

    /// 构造失败的 self-test 结果。
    pub fn failed(reason: impl Into<String>) -> Self {
        Self {
            passed: false,
            failure_reason: Some(reason.into()),
        }
    }
}

// ── EngineDiagnostic ───────────────────────────────────────────────────────

/// 引擎专属诊断投影（受限 DTO）。
///
/// adapter 可以提供有限的额外诊断信息供 UI 展示，
/// 但不暴露内部实现细节。结构由各 adapter 自行定义，
/// 但必须通过 serde 序列化且不包含敏感信息。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EngineDiagnostic {
    /// 诊断条目列表（key-value 对，前端按 key 展示）。
    pub entries: Vec<DiagnosticEntry>,
}

/// 单条诊断条目。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiagnosticEntry {
    /// 条目 key（稳定标识，前端 i18n 引用）。
    pub key: String,
    /// 显示值（不含敏感信息）。
    pub value: String,
    /// 条目标签（如 "info" / "warning" / "error"）。
    pub label: String,
}

// ── LaunchContext ──────────────────────────────────────────────────────────

/// 受控启动上下文（由 service 层构造，不来自前端）。
///
/// service 在分配 endpoint、生成 instance_id/token 后构造此结构，
/// 交给 adapter 产生最终 `LaunchRequest`。
///
/// **adapter 必须使用此上下文中的 endpoint 和身份参数**——
/// 不允许从原始 preferred_port 独立决定监听端口。
#[derive(Debug, Clone)]
pub struct LaunchContext {
    /// service 分配的 endpoint（child 必须绑定此端口）。
    /// 0.22.10：当前两个引擎均为非 HTTP 子进程或 in-process（funasr 走
    /// NDJSON stdio，paddleocr 走 ONNX in-process），生产暂无消费者；
    /// 作为受限启动上下文的协议位保留，未来 HTTP transport 引擎复用。
    #[allow(dead_code)]
    pub endpoint: super::identity::Endpoint,
    /// engine id（用于身份验证）。
    pub engine_id: String,
    /// instance id（由 service 每次启动随机生成）。
    pub instance_id: String,
    /// 服务 token（由 service 随机生成，不写普通日志）。
    pub token: String,
    /// resolved profile（从 descriptor 声明的候选列表中选择）。
    pub resolved_profile: ResolvedProfile,
}

// ── ResolvedLaunch ─────────────────────────────────────────────────────────

/// adapter 解析后的启动信息（包含 profile 解析结果和启动描述）。
///
/// fallback 不在此表达——compute profile 的兼容性回退只发生在
/// `InstallTransaction::resolve_profile`（结果记录进部署 manifest 并在
/// start 时冻结）；adapter 启动路径使用 manifest 已解析的 profile。
#[derive(Debug, Clone)]
pub struct ResolvedLaunch {
    /// 解析后的 profile。
    pub profile: ResolvedProfile,
    /// 启动描述。
    pub launch: LaunchDescriptor,
}

// ── LocalEngineAdapter ──────────────────────────────────────────────────────

/// 本地引擎 adapter 契约。
///
/// 每个内置引擎实现此 trait，提供引擎特有的行为。
///
/// ## 边界
///
/// - **不接收外部注入的可执行路径/argv/环境变量/URL**：
///   `prepare_launch` 接收的是已校验配置和 descriptor，不接收前端字符串。
/// - **不发送 Tauri 事件**：返回纯数据，由 app 层桥接。
/// - **不持有 AppHandle**：adapter 是纯逻辑，不接触 Tauri。
/// - **不是任意进程托管器**：只服务于编译期内置引擎。
pub trait LocalEngineAdapter: Send + Sync {
    /// 返回此 adapter 对应的引擎描述符。
    fn descriptor(&self) -> &EngineDefinition;

    /// 从已校验配置、resolved profile 和受控启动上下文产生受限启动描述。
    ///
    /// **不接受前端提供的 executable、argv、脚本路径、环境变量或任意 URL**。
    /// adapter 从 descriptor 锁定的 artifact + model_contract 自行解析。
    ///
    /// `LaunchContext` 由 service 层构造，包含 endpoint、engine_id、instance_id、
    /// token 和 resolved_profile——adapter 必须使用这些值构造启动参数，
    /// 不允许从原始 preferred_port 独立决定监听端口。
    fn prepare_launch(
        &self,
        ctx: &LaunchContext,
        config: &AdapterConfig,
    ) -> Result<ResolvedLaunch, LocalEngineError>;

    /// 把引擎特有的 health 响应映射为领域统一的 service/model 健康状态。
    fn map_health(&self, raw_health: &serde_json::Value) -> HealthMapping;

    /// 模型身份是否由 Blink 统一 model storage 的 generation manifest 管理。
    ///
    /// 默认启用：Ready health 必须与 model storage 中已安装 generation 的
    /// model id、revision 和 content fingerprint 完全一致。
    /// 尚未迁入统一 model storage、但由受信任 wrapper 自行校验专属 manifest
    /// 的引擎可显式返回 false；此时仍按 descriptor 校验 id/revision，并要求
    /// health 在 Ready 时提供合法 content fingerprint。
    fn uses_managed_model_storage(&self) -> bool {
        true
    }

    /// adapter self-test。
    ///
    /// 在安装后和启动前执行，验证引擎环境是否可用。
    fn self_test(&self) -> AdapterSelfTest;

    /// 引擎专属诊断投影。
    ///
    /// 返回有限的额外诊断信息供 UI 展示，不暴露内部实现细节。
    fn diagnostics(&self) -> EngineDiagnostic;
}

// ── AdapterConfig ──────────────────────────────────────────────────────────

/// adapter 配置（已校验，由 app 层从 ConfigStore 读取并校验后注入）。
///
/// **不接受前端直接传入的可执行路径/argv/环境变量**。
/// 配置只包含业务参数（模型选择、端口偏好等），不包含执行参数。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AdapterConfig {
    /// 首选端口（None = 自动分配）。
    pub preferred_port: Option<u16>,
    /// 用户请求的 compute preference。
    pub compute_preference: Option<super::identity::ComputePreference>,
    /// 引擎专属配置（闭合 JSON，各 adapter 自行解析）。
    pub engine_config: serde_json::Value,
}

impl AdapterConfig {
    /// 创建空配置。
    #[allow(dead_code)] // Default trait 已满足构造需求；new() 为显式语义入口保留
    pub fn new() -> Self {
        Self::default()
    }
}

// ── 测试 ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::local_engine::descriptor::{
        CapabilityKind, EngineDefinition, EngineDisplay, EngineTimeouts, InstallPlanRef,
        LifecyclePolicy, ResourceBudget, ServiceTransport,
    };
    use crate::domain::local_engine::error::{ErrorPhase, LocalEngineError, LocalEngineErrorCode};
    use crate::domain::local_engine::identity::{
        ArtifactId, ComputeBackend, ComputePreference, EngineId, ResolvedProfile, RuntimePlan,
    };

    /// 测试用 adapter——只验证契约行为，不实现真实引擎。
    struct TestAdapter {
        descriptor: EngineDefinition,
    }

    impl TestAdapter {
        fn new() -> Self {
            let artifact_id = ArtifactId::new("test-artifact").unwrap();
            Self {
                descriptor: EngineDefinition {
                    engine_id: EngineId::new("test-engine").unwrap(),
                    display: EngineDisplay {
                        name: "Test Engine".to_string(),
                        description: "Test".to_string(),
                        icon: "cpu".to_string(),
                        version: "0.1.0".to_string(),
                    },
                    capability_kind: CapabilityKind::Stt,
                    service_transport: ServiceTransport::Http,
                    runtime_kind: RuntimePlan::PythonVenv,
                    install_plan: InstallPlanRef {
                        runtime_kind: RuntimePlan::PythonVenv,
                        artifact_ids: vec![artifact_id.clone()],
                        compute_candidates: vec![super::super::descriptor::ComputeCandidate {
                            preference: ComputePreference::Cpu,
                            profile_id: "cpu-x64".to_string(),
                            artifact_id: artifact_id.clone(),
                        }],
                        schema_version: 1,
                    },
                    model_contract: crate::domain::local_engine::identity::ModelContract {
                        model_id: "test-model".to_string(),
                        revision: "v1.0".to_string(),
                        checksum_source:
                            crate::domain::local_engine::identity::ChecksumSource::Unverified,
                    },
                    lifecycle: LifecyclePolicy::Manual,
                    timeouts: EngineTimeouts::default(),
                    resource_budget: ResourceBudget::default(),
                },
            }
        }
    }

    impl LocalEngineAdapter for TestAdapter {
        fn descriptor(&self) -> &EngineDefinition {
            &self.descriptor
        }

        fn prepare_launch(
            &self,
            ctx: &LaunchContext,
            _config: &AdapterConfig,
        ) -> Result<ResolvedLaunch, LocalEngineError> {
            // 验证 profile 在 descriptor 允许范围内
            if !self.descriptor.is_profile_allowed(&ctx.resolved_profile) {
                return Err(LocalEngineError::with_detail(
                    LocalEngineErrorCode::Unsupported,
                    ErrorPhase::Start,
                    "不支持的 profile",
                    format!(
                        "profile '{}' 不在 descriptor 声明范围内",
                        ctx.resolved_profile.profile_id
                    ),
                ));
            }

            // adapter 自行构造启动描述——不接收外部注入
            let launch = LaunchDescriptor {
                executable: PathBuf::from("/test/engine/executable"),
                args: vec!["--serve".to_string(), "--port".to_string()],
                current_dir: Some(PathBuf::from("/test/engine")),
                env: HashMap::new(),
                label: "test-engine".to_string(),
            };

            Ok(ResolvedLaunch {
                profile: ctx.resolved_profile.clone(),
                launch,
            })
        }

        fn map_health(&self, raw: &serde_json::Value) -> HealthMapping {
            // 测试用简单映射
            let service = if raw.get("status").and_then(|v| v.as_str()) == Some("ok") {
                ServiceHealth::Healthy
            } else {
                ServiceHealth::Unreachable
            };
            let model = if raw.get("model_loaded").and_then(|v| v.as_bool()) == Some(true) {
                ModelHealth::Ready
            } else {
                ModelHealth::NotLoaded
            };

            HealthMapping {
                service,
                model,
                environment: None,
                backend: None,
                model_id: raw
                    .get("model_id")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string()),
                model_revision: raw
                    .get("model_revision")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string()),
                model_content_fingerprint: None,
            }
        }

        fn self_test(&self) -> AdapterSelfTest {
            AdapterSelfTest::passed()
        }

        fn diagnostics(&self) -> EngineDiagnostic {
            EngineDiagnostic {
                entries: vec![DiagnosticEntry {
                    key: "version".to_string(),
                    value: "0.1.0".to_string(),
                    label: "info".to_string(),
                }],
            }
        }
    }

    fn make_launch_context(profile: &ResolvedProfile) -> LaunchContext {
        LaunchContext {
            endpoint: crate::domain::local_engine::identity::Endpoint::new(8080),
            engine_id: "test-engine".to_string(),
            instance_id: "inst-test".to_string(),
            token: "test-token-abcdef0123456789".to_string(),
            resolved_profile: profile.clone(),
        }
    }

    #[test]
    fn adapter_prepare_launch_rejects_undeclared_profile() {
        let adapter = TestAdapter::new();
        let undeclared_profile = ResolvedProfile {
            profile_id: "cuda-sm99".to_string(),
            backend: ComputeBackend::Cuda,
            artifact_id: ArtifactId::new("undeclared").unwrap(),
            priority: 0,
        };
        let ctx = make_launch_context(&undeclared_profile);

        let result = adapter.prepare_launch(&ctx, &AdapterConfig::new());
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.code, LocalEngineErrorCode::Unsupported);
    }

    #[test]
    fn adapter_prepare_launch_succeeds_for_declared_profile() {
        let adapter = TestAdapter::new();
        let profile = ResolvedProfile {
            profile_id: "cpu-x64".to_string(),
            backend: ComputeBackend::Cpu,
            artifact_id: ArtifactId::new("test-artifact").unwrap(),
            priority: 0,
        };
        let ctx = make_launch_context(&profile);

        let result = adapter.prepare_launch(&ctx, &AdapterConfig::new());
        assert!(result.is_ok());
        let resolved = result.unwrap();
        assert_eq!(resolved.launch.label, "test-engine");
        // 启动描述由 adapter 内部生成，不接收外部注入
        assert!(!resolved.launch.executable.as_os_str().is_empty());
    }

    #[test]
    fn adapter_map_health_maps_healthy_service() {
        let adapter = TestAdapter::new();
        let raw = serde_json::json!({
            "status": "ok",
            "model_loaded": true,
            "model_id": "test-model",
            "model_revision": "v1.0",
        });

        let mapping = adapter.map_health(&raw);
        assert_eq!(mapping.service, ServiceHealth::Healthy);
        assert_eq!(mapping.model, ModelHealth::Ready);
        assert_eq!(mapping.model_id, Some("test-model".to_string()));
    }

    #[test]
    fn adapter_map_health_maps_unreachable_service() {
        let adapter = TestAdapter::new();
        let raw = serde_json::json!({});

        let mapping = adapter.map_health(&raw);
        assert_eq!(mapping.service, ServiceHealth::Unreachable);
        assert_eq!(mapping.model, ModelHealth::NotLoaded);
    }

    #[test]
    fn adapter_self_test_returns_passed() {
        let adapter = TestAdapter::new();
        let result = adapter.self_test();
        assert!(result.passed);
    }

    #[test]
    fn adapter_diagnostics_returns_entries() {
        let adapter = TestAdapter::new();
        let diag = adapter.diagnostics();
        assert!(!diag.entries.is_empty());
        assert_eq!(diag.entries[0].key, "version");
    }

    #[test]
    fn adapter_does_not_accept_external_executable() {
        // AdapterConfig 不包含 executable/argv/env 字段
        let config = AdapterConfig::new();
        // config 只包含 preferred_port, compute_preference, engine_config
        // 不包含 executable, args, env 等字段
        let json = serde_json::to_string(&config).unwrap();
        assert!(!json.contains("executable"));
        assert!(!json.contains("argv"));
    }

    #[test]
    fn launch_descriptor_is_generated_by_adapter() {
        let adapter = TestAdapter::new();
        let profile = ResolvedProfile {
            profile_id: "cpu-x64".to_string(),
            backend: ComputeBackend::Cpu,
            artifact_id: ArtifactId::new("test-artifact").unwrap(),
            priority: 0,
        };
        let ctx = make_launch_context(&profile);

        let resolved = adapter.prepare_launch(&ctx, &AdapterConfig::new()).unwrap();
        // executable 和 args 由 adapter 从 descriptor 锁定的 artifact 解析
        // 不从前端接收
        assert!(resolved.launch.executable.components().count() > 0);
    }

    #[test]
    fn adapter_self_test_failed_construction() {
        let failed = AdapterSelfTest::failed("missing dependency");
        assert!(!failed.passed);
        assert_eq!(
            failed.failure_reason,
            Some("missing dependency".to_string())
        );
    }
}
