//! 编译期引擎注册表（0.22.3）。
//!
//! `EngineRegistry` 是编译期内置 allowlist：
//! - 只接受预注册的 adapter
//! - 未知 engine_id 返回结构化 `UnknownEngine`
//! - 不接受调用者提交 runtime kind、executable、argv、脚本、env、URL
//!
//! ## 设计铁则
//!
//! - **编译期 allowlist**：注册项在 `EngineRegistry::new()` 中硬编码，
//!   前端只能传 `engine_id` 与有限动作（install/start/stop/repair/cleanup）。
//! - **无动态注册 API**：`EngineRegistry` 不暴露 `register()` 方法，
//!   不接受运行时新增引擎。
//! - **闭合枚举守卫**：`RuntimePlan`、`CapabilityKind` 均为闭合枚举，
//!   前端无法提交 runtime kind、URL、executable、argv 或环境变量。
//! - **adapter 无法注入未声明 profile/启动入口**：`prepare_launch` 在 adapter
//!   内部验证 profile 是否在 descriptor 声明范围内。

use std::collections::HashMap;
use std::sync::Arc;

use crate::domain::local_engine::{
    ErrorPhase, LocalEngineAdapter, LocalEngineError, LocalEngineErrorCode,
};
use crate::infra::local_engine::runtime::EngineId;

// ── RegistryEntry ─────────────────────────────────────────────────────────

/// 注册表条目：adapter + 可选 provider descriptor。
///
/// 当前 0.22.3 只实现 service 层骨架；真实 adapter（FunASR/PaddleOCR）
/// 由 H4 在此注册。测试用 fake adapter 也通过 `new_with_adapters` 注入。
pub struct RegistryEntry {
    pub adapter: Arc<dyn LocalEngineAdapter>,
}

// ── EngineRegistry ────────────────────────────────────────────────────────

/// 编译期引擎注册表（allowlist）。
///
/// 不暴露动态注册 API——所有注册项在构造时确定。
/// 未知 engine_id 返回 `UnknownEngine` 结构化错误。
pub struct EngineRegistry {
    entries: HashMap<EngineId, RegistryEntry>,
}

/// 注册表查找结果。
#[derive(Clone)]
#[allow(dead_code)]
pub enum RegistryLookup {
    /// 找到匹配的引擎。
    #[allow(dead_code)]
    Found(Arc<dyn LocalEngineAdapter>),
    /// 未知 engine_id。
    UnknownEngine { requested: String },
}

impl EngineRegistry {
    /// 创建空注册表（测试用）。
    #[allow(dead_code)]
    pub fn empty() -> Self {
        Self {
            entries: HashMap::new(),
        }
    }

    /// 创建带指定 adapter 列表的注册表（测试用）。
    ///
    /// 构造时逐个执行 `descriptor.validate()`——描述符是编译期声明，
    /// 内部不一致（runtime_kind 不匹配、候选引用未声明 artifact）属于
    /// wiring 错误，必须在构造时 fail-fast，而不是等到首次 install/start。
    pub fn new_with_adapters(adapters: Vec<Arc<dyn LocalEngineAdapter>>) -> Self {
        let mut entries = HashMap::new();
        for adapter in adapters {
            let descriptor = adapter.descriptor();
            descriptor.validate().unwrap_or_else(|e| {
                panic!(
                    "engine '{}' descriptor 校验失败（编译期声明错误）: {e}",
                    descriptor.engine_id
                )
            });
            let id = descriptor.engine_id.clone();
            entries.insert(id, RegistryEntry { adapter });
        }
        Self { entries }
    }

    /// 查找引擎。
    ///
    /// 返回 `RegistryLookup`——调用方据此区分 Found / UnknownEngine。
    /// 不接受 runtime kind、executable、argv、脚本、env、URL。
    pub fn lookup(&self, engine_id: &EngineId) -> RegistryLookup {
        match self.entries.get(engine_id) {
            Some(entry) => RegistryLookup::Found(entry.adapter.clone()),
            None => RegistryLookup::UnknownEngine {
                requested: engine_id.to_string(),
            },
        }
    }

    /// 查找引擎，返回 `Result` 形式（服务层便捷用）。
    ///
    /// 未知 engine_id 返回结构化 `LocalEngineError`。
    #[allow(dead_code)]
    pub fn get(
        &self,
        engine_id: &EngineId,
    ) -> Result<Arc<dyn LocalEngineAdapter>, LocalEngineError> {
        match self.lookup(engine_id) {
            RegistryLookup::Found(adapter) => Ok(adapter),
            RegistryLookup::UnknownEngine { requested } => Err(LocalEngineError::with_detail(
                LocalEngineErrorCode::Unsupported,
                ErrorPhase::Request,
                "未知引擎",
                format!("engine_id '{}' 不在编译期 allowlist 中", requested),
            )),
        }
    }

    /// 返回所有已注册引擎的 id 列表。
    #[allow(dead_code)]
    pub fn engine_ids(&self) -> Vec<EngineId> {
        self.entries.keys().cloned().collect()
    }

    /// 返回所有已注册引擎的 adapter 列表。
    pub fn adapters(&self) -> Vec<Arc<dyn LocalEngineAdapter>> {
        self.entries.values().map(|e| e.adapter.clone()).collect()
    }

    /// 返回已注册引擎数量。
    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// 是否为空。
    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

// ── 测试 ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::local_engine::*;

    /// 最小 fake adapter（测试用）。
    fn fake_adapter(id: &str) -> Arc<dyn LocalEngineAdapter> {
        struct FakeAdapter {
            descriptor: EngineDefinition,
        }
        impl FakeAdapter {
            fn new(id: &str) -> Self {
                let artifact = ArtifactId::new("fake-artifact").unwrap();
                Self {
                    descriptor: EngineDefinition {
                        engine_id: EngineId::new(id).unwrap(),
                        display: EngineDisplay {
                            name: format!("Fake {}", id),
                            description: "test".to_string(),
                            icon: "cpu".to_string(),
                            version: "0.1.0".to_string(),
                        },
                        capability_kind: CapabilityKind::Stt,
                        runtime_kind: RuntimePlan::PythonVenv,
                        install_plan: InstallPlanRef {
                            runtime_kind: RuntimePlan::PythonVenv,
                            artifact_ids: vec![artifact.clone()],
                            compute_candidates: vec![ComputeCandidate {
                                preference: ComputePreference::Cpu,
                                profile_id: "cpu-x64".to_string(),
                                artifact_id: artifact,
                            }],
                            schema_version: 1,
                        },
                        model_contract: ModelContract {
                            model_id: "fake-model".to_string(),
                            revision: "v1".to_string(),
                            checksum_source: ChecksumSource::Unverified,
                        },
                        lifecycle: LifecyclePolicy::Manual,
                        timeouts: EngineTimeouts::default(),
                        resource_budget: ResourceBudget::default(),
                    },
                }
            }
        }
        impl LocalEngineAdapter for FakeAdapter {
            fn descriptor(&self) -> &EngineDefinition {
                &self.descriptor
            }
            fn prepare_launch(
                &self,
                ctx: &crate::domain::local_engine::LaunchContext,
                _config: &AdapterConfig,
            ) -> Result<ResolvedLaunch, LocalEngineError> {
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
                Ok(ResolvedLaunch {
                    profile: ctx.resolved_profile.clone(),
                    launch: LaunchDescriptor {
                        executable: std::path::PathBuf::from("/fake/executable"),
                        args: vec!["--serve".to_string()],
                        current_dir: None,
                        env: std::collections::HashMap::new(),
                        label: self.descriptor.engine_id.to_string(),
                    },
                })
            }
            fn map_health(&self, _raw: &serde_json::Value) -> HealthMapping {
                HealthMapping {
                    service: ServiceHealth::Healthy,
                    model: ModelHealth::Ready,
                    environment: None,
                    backend: None,
                    model_id: None,
                    model_revision: None,
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
        Arc::new(FakeAdapter::new(id))
    }

    #[test]
    fn registry_rejects_unknown_engine_id() {
        let registry = EngineRegistry::new_with_adapters(vec![fake_adapter("engine-a")]);
        let unknown = EngineId::new("unknown-engine").unwrap();
        let result = registry.get(&unknown);
        assert!(result.is_err());
        let err = result.err().unwrap();
        assert_eq!(err.code, LocalEngineErrorCode::Unsupported);
        assert!(err.detail.contains("unknown-engine"));
    }

    #[test]
    fn registry_finds_registered_engine() {
        let registry = EngineRegistry::new_with_adapters(vec![fake_adapter("engine-a")]);
        let id = EngineId::new("engine-a").unwrap();
        let result = registry.get(&id);
        assert!(result.is_ok());
        assert_eq!(result.unwrap().descriptor().engine_id, id);
    }

    #[test]
    fn registry_lookup_returns_unknown_engine_struct() {
        let registry = EngineRegistry::empty();
        let unknown = EngineId::new("no-such-engine").unwrap();
        match registry.lookup(&unknown) {
            RegistryLookup::UnknownEngine { requested } => {
                assert_eq!(requested, "no-such-engine");
            }
            RegistryLookup::Found(_) => panic!("应返回 UnknownEngine"),
        }
    }

    #[test]
    fn registry_does_not_accept_dynamic_registration() {
        // EngineRegistry 不暴露 register() 方法
        // 只有 new_with_adapters 在构造时确定
        let registry = EngineRegistry::new_with_adapters(vec![fake_adapter("engine-a")]);
        assert_eq!(registry.len(), 1);
        assert!(!registry.is_empty());

        let empty = EngineRegistry::empty();
        assert!(empty.is_empty());
        assert_eq!(empty.len(), 0);
    }

    #[test]
    fn registry_engine_ids_returns_all() {
        let registry = EngineRegistry::new_with_adapters(vec![
            fake_adapter("engine-a"),
            fake_adapter("engine-b"),
        ]);
        let ids = registry.engine_ids();
        assert_eq!(ids.len(), 2);
    }

    #[test]
    fn registry_adapters_returns_all() {
        let registry = EngineRegistry::new_with_adapters(vec![
            fake_adapter("engine-a"),
            fake_adapter("engine-b"),
        ]);
        let adapters = registry.adapters();
        assert_eq!(adapters.len(), 2);
    }
}
