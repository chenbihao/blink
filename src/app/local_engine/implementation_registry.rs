//! builtin implementation 注册表装配（0.22.9）。
//!
//! 把 0.22.9 的内部 implementation descriptor 与模型→implementation 绑定表
//! 在**编译期**装配为 `ImplementationRegistry`（domain 类型），构造期执行
//! fail-closed 校验——与 `EngineRegistry::new_with_adapters` 相同的
//! fail-fast 原则：wiring 错误不允许留到首次 start。
//!
//! ## 最终绑定表（本 handoff 的唯一绑定真源）
//!
//! | 模型 | implementation |
//! |---|---|
//! | `gguf/sensevoice-small-q8` | `funasr_gguf_worker` |
//! | `gguf/paraformer-zh-q8` | `funasr_gguf_worker` |
//! | `gguf/fun-asr-nano-q4km` | `funasr_gguf_worker` |
//! | `PP-OCRv6:PP-OCRv6_tiny_det:PP-OCRv6_tiny_rec` | `paddleocr_onnx_in_process` |

//! 0.22.10（handoff-11）：ASR 侧 ONNX 线路（`onnx/paraformer-online` /
//! `paraformer_onnx_worker`）已随产品决策退役，从注册表中整体移除。
//!
//! ## 边界
//!
//! - 不新增产品 engine/adapter：产品层仍只有 `funasr` / `paddleocr`。
//! - 不接受前端提交 implementation/runtime/transport。
//! - 本模块只装配与校验，不改变任何执行路径。

use crate::domain::local_engine::{
    ImplementationBinding, ImplementationDescriptor, ImplementationId, ImplementationRegistry,
    InstallPlanRef, ResourceBudget,
};
use crate::infra::local_engine::runtime::{ArtifactId, EngineId, RuntimePlan};

use super::funasr::FUNASR_ENGINE_ID;
use super::funasr::descriptor::FUNASR_GGUF_ARTIFACT_ID;
use super::funasr::gguf::{GGUF_NANO_ID, GGUF_PARAFORMER_ID, GGUF_SENSEVOICE_ID};
use super::paddleocr::PADDLEOCR_ENGINE_ID;

// ── 可承载模型 id 常量（绑定表与测试共用）────────────────────────────────────

/// FunASR GGUF implementation 可承载的全部模型（模型目录唯一来源）。
pub fn funasr_gguf_carried_models() -> Vec<String> {
    vec![
        GGUF_SENSEVOICE_ID.to_string(),
        GGUF_PARAFORMER_ID.to_string(),
        GGUF_NANO_ID.to_string(),
    ]
}

/// PaddleOCR in-process implementation 唯一可承载的模型 id
/// （与 `make_paddleocr_descriptor` 的 model_contract 同源构造）。
pub fn paddleocr_inprocess_model_id() -> String {
    use crate::domain::ocr::config::PaddleModel;
    let (det, rec) = PaddleModel::Tiny.official_model_names();
    format!("PP-OCRv6:{}:{}", det, rec)
}

// ── descriptor 构造 ────────────────────────────────────────────────────────

/// FunASR GGUF 常驻 worker implementation（承载全部三个既有 GGUF 模型）。
fn make_funasr_gguf_implementation() -> ImplementationDescriptor {
    ImplementationDescriptor {
        id: ImplementationId::FunasrGgufWorker,
        engine_id: EngineId::new(FUNASR_ENGINE_ID).expect("funasr is valid"),
        runtime_kind: RuntimePlan::ManagedBinary,
        // GGUF worker：stdin/stdout NDJSON，受管子进程
        service_transport: crate::domain::local_engine::ServiceTransport::StdioWorker,
        executor_topology: crate::domain::local_engine::ExecutorTopology::ManagedWorker,
        install_plan: InstallPlanRef {
            runtime_kind: RuntimePlan::ManagedBinary,
            artifact_ids: vec![ArtifactId::new(FUNASR_GGUF_ARTIFACT_ID).expect("artifact id 合法")],
            compute_candidates: Vec::new(),
            schema_version: 1,
        },
        carried_models: funasr_gguf_carried_models(),
        resource_budget: ResourceBudget::default(),
        timeouts: None, // 使用 funasr engine descriptor 默认超时
    }
}

/// PaddleOCR ONNX in-process implementation（承载 PP-OCRv6 tiny）。
fn make_paddleocr_inprocess_implementation() -> ImplementationDescriptor {
    ImplementationDescriptor {
        id: ImplementationId::PaddleOcrOnnxInProcess,
        engine_id: EngineId::new(PADDLEOCR_ENGINE_ID).expect("paddleocr is valid"),
        runtime_kind: RuntimePlan::OnnxRuntime,
        // in-process：无外部服务通道，blink.exe 直持 ORT lazy Session
        service_transport: crate::domain::local_engine::ServiceTransport::InProcess,
        executor_topology: crate::domain::local_engine::ExecutorTopology::InProcess,
        install_plan: InstallPlanRef {
            runtime_kind: RuntimePlan::OnnxRuntime,
            artifact_ids: vec![
                crate::infra::local_engine::asset_lock::ort_dll_artifact_id()
                    .expect("asset-lock.json 必须可解析且 ORT artifact id 构造成功"),
            ],
            compute_candidates: Vec::new(),
            schema_version: 1,
        },
        carried_models: vec![paddleocr_inprocess_model_id()],
        resource_budget: ResourceBudget::default(),
        timeouts: None, // 使用 paddleocr engine descriptor 默认超时
    }
}

// ── 绑定表 ─────────────────────────────────────────────────────────────────

/// 模型 → implementation 绑定表（与各模型目录同源，编译期锁定）。
pub fn builtin_model_bindings() -> Vec<ImplementationBinding> {
    let funasr = EngineId::new(FUNASR_ENGINE_ID).expect("funasr is valid");
    let paddleocr = EngineId::new(PADDLEOCR_ENGINE_ID).expect("paddleocr is valid");

    let mut bindings = Vec::new();
    // SenseVoice / Paraformer-zh / Nano → FunASR GGUF implementation
    for model in funasr_gguf_carried_models() {
        bindings.push(ImplementationBinding {
            engine_id: funasr.clone(),
            model_id: model,
            implementation: ImplementationId::FunasrGgufWorker,
        });
    }
    // PP-OCRv6 → PaddleOCR ONNX in-process implementation
    bindings.push(ImplementationBinding {
        engine_id: paddleocr,
        model_id: paddleocr_inprocess_model_id(),
        implementation: ImplementationId::PaddleOcrOnnxInProcess,
    });
    bindings
}

// ── 注册表装配 ─────────────────────────────────────────────────────────────

/// 构造 builtin implementation 注册表（fail-closed，wiring 错误直接 panic）。
///
/// descriptor 与绑定表都是编译期声明；任何矛盾（重复 id、跨 engine 绑定、
/// runtime/transport 拓扑不一致）属于 wiring 错误，必须在构造时 fail-fast。
pub fn make_builtin_implementation_registry() -> ImplementationRegistry {
    ImplementationRegistry::new_validated(
        vec![
            make_funasr_gguf_implementation(),
            make_paddleocr_inprocess_implementation(),
        ],
        builtin_model_bindings(),
    )
    .unwrap_or_else(|e| panic!("builtin implementation registry 校验失败（编译期声明错误）: {e}"))
}

// ── 测试 ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::local_engine::{
        CapabilityKind, ComputeCandidate, EngineDefinition, EngineDisplay, EngineTimeouts,
        LifecyclePolicy, ServiceTransport,
    };
    use crate::infra::local_engine::runtime::{ComputeBackend, ComputePreference};

    #[test]
    fn builtin_registry_validates_and_resolves() {
        let registry = make_builtin_implementation_registry();

        // 三个既有 FunASR 模型均解析到 GGUF implementation
        for model in funasr_gguf_carried_models() {
            assert_eq!(
                registry
                    .resolve_for_model(&EngineId::new(FUNASR_ENGINE_ID).unwrap(), &model)
                    .expect("已声明模型应解析成功"),
                Some(ImplementationId::FunasrGgufWorker),
                "模型 {model} 必须绑定 GGUF implementation"
            );
        }

        // PP-OCRv6 解析到 OCR in-process implementation
        let ocr_model = paddleocr_inprocess_model_id();
        assert_eq!(
            registry
                .resolve_for_model(&EngineId::new(PADDLEOCR_ENGINE_ID).unwrap(), &ocr_model)
                .expect("已声明模型应解析成功"),
            Some(ImplementationId::PaddleOcrOnnxInProcess)
        );
    }

    #[test]
    fn unknown_legacy_model_stays_unavailable() {
        let registry = make_builtin_implementation_registry();
        // 未知旧模型（如 0.22.7 前的 Python 时代模型 id、handoff-11 退役的
        // onnx/paraformer-online）不静默换模
        for legacy in ["iic/SenseVoiceSmall", "paraformer-zh", "whisper-tiny", "onnx/paraformer-online"] {
            assert!(
                registry
                    .resolve_for_model(&EngineId::new(FUNASR_ENGINE_ID).unwrap(), legacy)
                    .is_err()
            );
        }
    }

    #[test]
    fn bindings_match_model_catalog_exactly() {
        // 绑定表覆盖的模型 = funasr 模型目录（3 GGUF）+ paddleocr 模型
        let registry = make_builtin_implementation_registry();
        let bindings = builtin_model_bindings();
        assert_eq!(bindings.len(), 4, "3 GGUF + 1 OCR 模型");

        let funasr = EngineId::new(FUNASR_ENGINE_ID).unwrap();
        let model_registry =
            crate::app::local_engine::model_installer::make_funasr_model_registry();
        let catalog = model_registry.list(&funasr);
        assert_eq!(catalog.len(), 3, "3 GGUF 模型");
        for spec in catalog {
            assert!(bindings.iter().any(|b| b.model_id == spec.model_id));
        }

        // 模型目录中的每个模型都能解析（目录与绑定表同源，防止漂移）
        for spec in catalog {
            assert!(
                registry
                    .resolve_for_model(&funasr, &spec.model_id)
                    .expect("模型目录中的模型必须可解析")
                    .is_some()
            );
        }
    }

    #[test]
    fn gguf_implementation_declares_worker_topology() {
        let registry = make_builtin_implementation_registry();
        let desc = registry
            .descriptor(ImplementationId::FunasrGgufWorker)
            .unwrap();
        assert_eq!(desc.runtime_kind, RuntimePlan::ManagedBinary);
        assert_eq!(desc.service_transport, ServiceTransport::StdioWorker);
        assert_eq!(
            desc.executor_topology,
            crate::domain::local_engine::ExecutorTopology::ManagedWorker
        );

        let desc = registry
            .descriptor(ImplementationId::PaddleOcrOnnxInProcess)
            .unwrap();
        assert_eq!(desc.runtime_kind, RuntimePlan::OnnxRuntime);
        assert_eq!(desc.service_transport, ServiceTransport::InProcess);
        assert_eq!(
            desc.executor_topology,
            crate::domain::local_engine::ExecutorTopology::InProcess
        );
    }

    /// 产品层 engine descriptor 与 implementation 绑定一致性：同一 engine id
    /// 只有一个产品引擎（Registry 对外仍只有一个 `funasr` / `paddleocr`），
    /// implementation 的 engine 归属与产品 descriptor 的 capability 对齐。
    #[test]
    fn implementations_align_with_product_engine_descriptors() {
        let funasr_adapter = crate::app::local_engine::funasr::make_funasr_adapter();
        let paddleocr_adapter = crate::app::local_engine::paddleocr::make_paddleocr_adapter();
        let registry = crate::app::local_engine::EngineRegistry::new_with_adapters(vec![
            funasr_adapter,
            paddleocr_adapter,
        ]);

        // 产品层 engine id 集合不变
        let ids = registry.engine_ids();
        assert_eq!(ids.len(), 2);
        assert!(ids.contains(&EngineId::new(FUNASR_ENGINE_ID).unwrap()));
        assert!(ids.contains(&EngineId::new(PADDLEOCR_ENGINE_ID).unwrap()));

        // implementation 归属 engine 与产品 descriptor 一致
        let impl_registry = make_builtin_implementation_registry();
        let funasr_desc = impl_registry
            .descriptor(ImplementationId::FunasrGgufWorker)
            .unwrap();
        let product_funasr = registry
            .get(&EngineId::new(FUNASR_ENGINE_ID).unwrap())
            .unwrap();
        assert_eq!(funasr_desc.engine_id, product_funasr.descriptor().engine_id);
        assert_eq!(
            product_funasr.descriptor().capability_kind,
            CapabilityKind::Stt
        );
        let ocr_desc = impl_registry
            .descriptor(ImplementationId::PaddleOcrOnnxInProcess)
            .unwrap();
        assert_eq!(
            ocr_desc.engine_id,
            EngineId::new(PADDLEOCR_ENGINE_ID).unwrap()
        );
    }

    /// 占位编译检查：descriptor 相关类型仍被 engine descriptor 消费。
    #[test]
    fn engine_descriptor_types_still_compatible() {
        let artifact = ArtifactId::new(FUNASR_GGUF_ARTIFACT_ID).unwrap();
        let desc = EngineDefinition {
            engine_id: EngineId::new(FUNASR_ENGINE_ID).unwrap(),
            display: EngineDisplay {
                name: "FunASR".to_string(),
                description: "test".to_string(),
                icon: "mic".to_string(),
                version: "0.1.0".to_string(),
            },
            capability_kind: CapabilityKind::Stt,
            service_transport: ServiceTransport::StdioWorker,
            runtime_kind: RuntimePlan::ManagedBinary,
            install_plan: InstallPlanRef {
                runtime_kind: RuntimePlan::ManagedBinary,
                artifact_ids: vec![artifact.clone()],
                compute_candidates: vec![ComputeCandidate {
                    preference: ComputePreference::Cpu,
                    profile_id: "cpu-x64".to_string(),
                    artifact_id: artifact,
                }],
                schema_version: 1,
            },
            model_contract: crate::infra::local_engine::runtime::ModelContract {
                model_id: GGUF_SENSEVOICE_ID.to_string(),
                revision: "gguf-v0.2.6".to_string(),
                checksum_source: crate::infra::local_engine::runtime::ChecksumSource::Unverified,
            },
            lifecycle: LifecyclePolicy::Manual,
            timeouts: EngineTimeouts::default(),
            resource_budget: ResourceBudget::default(),
        };
        assert!(desc.validate().is_ok());
        // backend 枚举仍兼容（compute candidate 投影消费）
        let backend = match ComputePreference::Cpu {
            ComputePreference::Cpu => ComputeBackend::Cpu,
            _ => unreachable!(),
        };
        assert_eq!(backend, ComputeBackend::Cpu);
    }
}
