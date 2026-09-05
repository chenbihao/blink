//! FunASR 本地引擎 adapter（0.22.3 建立；0.22.7.4 起为 GGUF 常驻 worker 唯一实现）。
//!
//! 把 FunASR GGUF worker（llama.cpp/GGUF，`cargo xtask funasr-worker` 从锁定
//! 源码构建）注册为 `LocalEngineAdapter`，使安装、启动、ready 握手、日志、
//! 空间和清理通过 `EngineManager` 管理。
//!
//! ## 目标拓扑（phase 0.22 §5.8.2）
//!
//! ```text
//! PseudoStreamingSttEngine → 既有 funasr adapter → NDJSON worker client
//!   → stdin/stdout → llama.cpp/GGUF native worker → 当前选中的一个模型
//! ```
//!
//! ## 设计铁则
//!
//! - **同一 `funasr` engine id**：0.22.7.2/.3 的双实现开关（`BLINK_STT_GGUF`）
//!   已随旧 Python/PyTorch 链路删除——本模块即唯一实现，不再有第二 engine。
//! - **descriptor 锁定 ManagedBinary artifact + StdioWorker 传输 + cpu-x64
//!   profile + GGUF 模型目录**（见 [`descriptor`] 与 [`gguf`]）。
//! - **transport 为 stdio**：无 HTTP 端口、无 venv、无 torch；启动身份经
//!   环境变量注入，ready 握手复用 `parse_and_verify_health` 身份/指纹校验。
//! - **保持已有配置 key 和 serde 形状**：`funasr_model`/`device`/`vad`/
//!   `auto_start_server` 语义（`hotwords`/`use_itn` 已于 0.22.7 契约收口删除）；
//!   模型 id 由 0.22.7 迁移确定映射。
//! - **日志使用 ManagedProcess 的 bounded history/broadcast**（worker 只写
//!   stderr；stdout 是 NDJSON 协议通道，不进日志）。
//! - **空间统计和清理区分 engine deployment / 模型 payload / provider
//!   公共缓存**；单引擎清理不能连带删除公共资产。
//!
//! ## 子模块
//!
//! - [`descriptor`]：`EngineDefinition` / `ProviderDescriptor` 编译期装配
//! - [`launch`]：`FunasrEngineConfig` 配置投影（SttConfig serde 形状）
//! - [`gguf`]：模型目录（URL/SHA-256 锁定）、GGUF 启动构造、环境 self-test
//! - [`gguf_installer`]：GGUF 模型下载 worker（下载 + hash + 离线缓存）
//! - [`health`]：ready JSON → 领域 health 映射（与旧 HTTP 字段同形）
//! - [`worker`]：`GgufSttTransport`（受管音频目录 + 路径约束 + 后处理）
//! - [`tests`]：契约测试 + 真实端到端（`BLINK_E2E_GGUF=1` 门控）

pub(crate) mod descriptor;
pub(crate) mod gguf;
pub(crate) mod gguf_installer;
mod health;
mod launch;
pub(crate) mod paraformer_online;
#[cfg(test)]
mod tests;
pub(crate) mod worker;

pub use self::descriptor::make_funasr_provider_descriptor;
pub use self::gguf_installer::FunasrGgufModelInstallWorker;
pub use self::launch::FunasrEngineConfig;

use self::descriptor::make_funasr_descriptor;
use self::health::map_funasr_health;

use std::sync::Arc;

use crate::domain::local_engine::{
    AdapterConfig, AdapterSelfTest, DiagnosticEntry, EngineDefinition, EngineDiagnostic,
    ErrorPhase, HealthMapping, LaunchContext, LocalEngineAdapter, LocalEngineError,
    LocalEngineErrorCode, ResolvedLaunch,
};

/// FunASR 稳定 engine id。
pub const FUNASR_ENGINE_ID: &str = "funasr";

// ── FunasrAdapter ──────────────────────────────────────────────────────────

/// FunASR 本地引擎 adapter（GGUF 常驻 worker 实现）。
///
/// 实现 `LocalEngineAdapter` trait，把 GGUF worker 特有的启动参数、ready
/// 映射、诊断和 self-test 适配到领域统一协议。
///
/// ## 边界
///
/// - **不接收前端提供的 executable、argv、脚本路径、环境变量或任意 URL**。
///   `prepare_launch` 从 descriptor 锁定的部署与模型资产自行解析。
/// - **不发送 Tauri 事件**：返回纯数据，由 app 层桥接。
/// - **不持有 AppHandle**：adapter 是纯逻辑，不接触 Tauri。
pub struct FunasrAdapter {
    descriptor: EngineDefinition,
}

impl FunasrAdapter {
    /// 创建 FunASR adapter（GGUF 常驻 worker 实现）。
    ///
    /// descriptor 在编译期声明，锁定 engine id、StdioWorker 传输、
    /// ManagedBinary runtime、cpu-x64 profile 和 GGUF 模型目录。
    pub fn new() -> Self {
        Self {
            descriptor: make_funasr_descriptor(),
        }
    }
}

impl Default for FunasrAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl LocalEngineAdapter for FunasrAdapter {
    fn descriptor(&self) -> &EngineDefinition {
        &self.descriptor
    }

    /// 从已校验配置、resolved profile 和受控启动上下文产生受限启动描述。
    ///
    /// **不接受前端提供的 executable、argv、脚本路径、环境变量或任意 URL**。
    /// adapter 从 active deployment（worker exe）+ model_storage payload
    /// （GGUF 文件）自行解析；身份经环境变量注入（ctx 中的值）。
    fn prepare_launch(
        &self,
        ctx: &LaunchContext,
        config: &AdapterConfig,
    ) -> Result<ResolvedLaunch, LocalEngineError> {
        // 验证 profile 在 descriptor 允许范围内
        if !self.descriptor.is_profile_allowed(&ctx.resolved_profile) {
            return Err(LocalEngineError::with_detail(
                LocalEngineErrorCode::Unsupported,
                ErrorPhase::Start,
                "不支持的 profile",
                format!(
                    "profile '{}' 不在 FunASR descriptor 声明范围内",
                    ctx.resolved_profile.profile_id
                ),
            ));
        }

        // 从 AdapterConfig.engine_config 解析 FunASR 配置
        let funasr_config: launch::FunasrEngineConfig =
            serde_json::from_value(config.engine_config.clone()).map_err(|e| {
                LocalEngineError::with_detail(
                    LocalEngineErrorCode::InvalidConfig,
                    ErrorPhase::Config,
                    "FunASR 引擎配置解析失败",
                    format!("engine_config 反序列化失败: {e}"),
                )
            })?;

        // 0.22.9 Handoff 08：按 start 冻结的 implementation 分派实现内启动构造
        // （ctx.implementation 由 manager 从编译期绑定表解析后注入，不接受前端提交）。
        // GGUF 模型走现有 GGUF 构造；ParaformerOnline 走 ONNX worker 构造。
        let launch = if ctx.implementation
            == Some(crate::domain::local_engine::ImplementationId::ParaformerOnnxWorker)
        {
            paraformer_online::build_paraformer_online_launch_descriptor()?
        } else {
            gguf::build_funasr_gguf_launch_descriptor(&funasr_config, config, ctx)?
        };

        Ok(ResolvedLaunch {
            profile: ctx.resolved_profile.clone(),
            launch,
        })
    }

    /// 把 worker ready JSON 映射为领域统一的 service/model 健康状态。
    ///
    /// ready 字段与旧 HTTP health 同形（model_status/model_id/
    /// model_revision/model_content_fingerprint/backend），映射逻辑复用。
    fn map_health(&self, raw_health: &serde_json::Value) -> HealthMapping {
        map_funasr_health(raw_health)
    }

    /// adapter self-test：active deployment 结构检查（worker exe 就位）。
    ///
    /// 完整 hash 校验发生在安装事务（candidate 内一次性执行）；
    /// 运行期探活由 start 的 NDJSON ready 握手承载。
    fn self_test(&self) -> AdapterSelfTest {
        match gguf::gguf_environment_self_test() {
            Ok(()) => AdapterSelfTest::passed(),
            Err(reason) => AdapterSelfTest::failed(&reason),
        }
    }

    /// 引擎专属诊断投影（GGUF runtime 可观测性）。
    fn diagnostics(&self) -> EngineDiagnostic {
        let mut entries = Vec::new();

        let deployed = crate::infra::local_engine::deployment::DeploymentStore::active_dir(
            &gguf::gguf_deployment_space(),
        )
        .ok()
        .flatten()
        .map(|(_, dir)| dir);
        let deploy_ok = deployed
            .as_ref()
            .is_some_and(|d| d.join("funasr-sensevoice-worker.exe").is_file());
        entries.push(DiagnosticEntry {
            key: "gguf_deployment_ready".to_string(),
            value: deploy_ok.to_string(),
            label: if deploy_ok { "info" } else { "warning" }.to_string(),
        });
        entries.push(DiagnosticEntry {
            key: "runtime_kind".to_string(),
            value: "managed_binary/llama.cpp".to_string(),
            label: "info".to_string(),
        });
        entries.push(DiagnosticEntry {
            key: "source_pin".to_string(),
            value: "FunASR runtime-llamacpp-v0.2.6 (55b662c)".to_string(),
            label: "info".to_string(),
        });
        entries.push(DiagnosticEntry {
            key: "protocol_version".to_string(),
            value: crate::infra::local_engine::worker_proto::WORKER_PROTOCOL_VERSION.to_string(),
            label: "info".to_string(),
        });

        EngineDiagnostic { entries }
    }
}

// ── 纯构造入口 ──────────────────────────────────────────────────────────────

/// 创建 FunASR adapter 的 `Arc` 引用（GGUF 实现）。
pub fn make_funasr_adapter() -> Arc<dyn LocalEngineAdapter> {
    Arc::new(FunasrAdapter::new())
}
