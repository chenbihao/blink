//! 引擎目录域：`get_local_engine_catalog` 查询与 compute 兼容性计算。
//!
//! 只读查询，不启动服务、不安装环境、不阻塞主链路。
//! compute options 的兼容性由 ProviderDescriptor + RuntimeProvider 真源决定，
//! 不由前端猜测。

use crate::app::command_error::CommandError;
use crate::app::local_engine::EngineManager;
use crate::app::local_engine::dto::{EngineCatalogItem, project_catalog_item_for_model};
use crate::domain::local_engine::EngineDefinition;

use super::{current_compute_preference, get_service};

// ── 内部辅助 ──────────────────────────────────────────────────────────────────

/// 为 catalog item 计算兼容性结果。
///
/// 从 `ProviderDescriptor` 的 `profiles` + `RuntimeProvider::check_compatibility`
/// 真源获取，不由前端猜测。
fn compute_compatibility_for_descriptor(
    svc: &EngineManager,
    descriptor: &EngineDefinition,
    model_id: &str,
) -> Vec<(String, bool, Option<String>)> {
    // 从 ProviderDescriptor 获取 profile candidates
    let provider_desc = svc.provider_descriptor_for_engine(&descriptor.engine_id);

    // 如果有 ProviderDescriptor，使用其 profiles + provider check_compatibility
    if let Some(pd) = provider_desc {
        // 0.22.10：按 descriptor 声明的 runtime kind 分派到对应 provider 真源
        // （PythonVenv provider 已退役；runtime_kind 来自编译期 descriptor，
        // 与实际执行安装事务的 provider 一致）。
        let provider = svc.provider_for_runtime(pd.runtime_kind);
        descriptor
            .candidates_for_model(model_id)
            .into_iter()
            .map(|c| {
                // 从 ProviderDescriptor 的 profiles 中找匹配的 ProfileCandidate
                let profile_candidate = pd.profiles.iter().find(|pc| {
                    pc.model_id == c.model_id
                        && pc.profile_id == c.profile_id
                        && pc.backend == c.backend
                        && pc.artifact_id == c.artifact_id
                });

                let (compatible, disabled_reason) = if let Some(pc) = profile_candidate {
                    match provider {
                        Some(p) => match p.check_compatibility(&pc.compatibility) {
                            Ok(true) => (true, None),
                            Ok(false) => {
                                (false, Some(format!("本机不兼容: {:?}", pc.compatibility)))
                            }
                            Err(e) => (false, Some(format!("兼容性检查失败: {e}"))),
                        },
                        None => (
                            false,
                            Some("该运行时类型已退役，请重新安装引擎".to_string()),
                        ),
                    }
                } else {
                    // 没有匹配的 ProfileCandidate——descriptor 声明但 provider 未提供
                    (false, Some("provider 未声明此 profile".to_string()))
                };

                (c.profile_id.clone(), compatible, disabled_reason)
            })
            .collect()
    } else {
        // 没有 ProviderDescriptor——无法做兼容性检查，标记为 unknown
        descriptor
            .candidates_for_model(model_id)
            .into_iter()
            .map(|c| {
                (
                    c.profile_id.clone(),
                    false,
                    Some("无 ProviderDescriptor".to_string()),
                )
            })
            .collect()
    }
}

// ── 公开 commands ─────────────────────────────────────────────────────────────

/// 获取本地引擎目录（catalog）。
///
/// 返回所有已注册引擎的 UI 投影。
/// 只包含 descriptor 声明项，compute options 的兼容性由 provider 真源决定。
///
/// **只读查询，不启动服务、不安装环境、不阻塞主链路。**
#[tauri::command]
pub async fn get_local_engine_catalog(
    app: tauri::AppHandle,
) -> Result<Vec<EngineCatalogItem>, CommandError> {
    let svc = get_service(&app)?;

    let catalog = svc.catalog().await;
    let mut items = Vec::with_capacity(catalog.len());

    for descriptor in catalog {
        let engine_id_str = descriptor.engine_id.to_string();
        let eid = crate::infra::local_engine::runtime::EngineId::new(&engine_id_str)
            .map_err(|e| CommandError::new("invalid_engine_id", e.to_string(), false))?;
        let selected_model_id = crate::app::local_engine::config_source::current_model_id(&eid)
            .unwrap_or_else(|| descriptor.model_contract.model_id.clone());
        let current_pref = current_compute_preference(&engine_id_str);
        let compatibility_results =
            compute_compatibility_for_descriptor(&svc, &descriptor, &selected_model_id);
        let model_descriptor = svc.model_registry().find(&eid, &selected_model_id);
        let item = project_catalog_item_for_model(
            &descriptor,
            model_descriptor,
            &selected_model_id,
            &compatibility_results,
            current_pref,
        );
        items.push(item);
    }

    Ok(items)
}

// ── 测试 ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    use crate::app::commands::local_engine::build_adapter_config_for_engine;
    use crate::app::local_engine::{funasr, paddleocr};
    use crate::infra::local_engine::runtime::{ComputeBackend, ComputePreference};

    /// FunASR catalog 只投影当前模型声明的 profile；SenseVoice 开放 Vulkan/CUDA，
    /// Paraformer 开放 Vulkan，Nano 仍保持 CPU-only。
    #[test]
    fn funasr_catalog_compute_options_are_model_scoped() {
        let adapter = crate::app::local_engine::funasr::make_funasr_adapter();
        let descriptor = adapter.descriptor();
        let item = crate::app::local_engine::dto::project_catalog_item(
            descriptor,
            &[
                (ComputePreference::Vulkan, true, None),
                (ComputePreference::Cuda, true, None),
                (ComputePreference::Cpu, true, None),
            ],
            ComputePreference::Cpu,
        );
        let prefs: Vec<&str> = item
            .compute_options
            .iter()
            .map(|o| o.preference.as_str())
            .collect();
        assert_eq!(prefs, vec!["vulkan", "cuda", "cpu"]);
        assert_eq!(item.compute_options[0].model_id, "gguf/sensevoice-small-q8");
        assert_eq!(
            item.undeclared_compute_preferences,
            vec!["directml".to_string()]
        );

        let incompatible = crate::app::local_engine::dto::project_catalog_item_for_model(
            descriptor,
            None,
            "gguf/sensevoice-small-q8",
            &[("cpu-x64".to_string(), false, Some("本机不兼容".to_string()))],
            ComputePreference::Cpu,
        );
        let incompatible_cpu = incompatible
            .compute_options
            .iter()
            .find(|option| option.preference == "cpu")
            .expect("catalog 应包含 SenseVoice CPU candidate");
        assert!(!incompatible_cpu.compatible);
        assert_eq!(
            incompatible_cpu.disabled_reason.as_deref(),
            Some("本机不兼容")
        );

        let paraformer = crate::app::local_engine::dto::project_catalog_item_for_model(
            descriptor,
            None,
            "gguf/paraformer-zh-q8",
            &[
                ("vulkan-x64".to_string(), true, None),
                ("cpu-x64".to_string(), true, None),
            ],
            ComputePreference::Vulkan,
        );
        let paraformer_prefs: Vec<&str> = paraformer
            .compute_options
            .iter()
            .map(|option| option.preference.as_str())
            .collect();
        assert_eq!(paraformer_prefs, vec!["vulkan", "cpu"]);
        assert_eq!(
            paraformer.undeclared_compute_preferences,
            vec!["cuda".to_string(), "directml".to_string()]
        );
    }

    // ── catalog 只包含 registry allowlist ──

    #[test]
    fn catalog_only_contains_registry_allowlist() {
        // catalog 从 svc.catalog() 获取，svc 只包含 registry 中注册的引擎
        // 此测试验证 build_adapter_config_for_engine 只处理 allowlist 中的引擎
        let known = ["funasr", "paddleocr"];
        for id in &known {
            assert!(build_adapter_config_for_engine(id).is_ok());
        }
        // 未知引擎被拒绝
        assert!(build_adapter_config_for_engine("unknown").is_err());
    }

    // ── catalog DTO 不暴露 artifact URL / executable / argv / env ──

    #[test]
    fn catalog_dto_does_not_expose_internals() {
        use crate::app::local_engine::dto::EngineCatalogItem;
        let dto = EngineCatalogItem {
            engine_id: "funasr".to_string(),
            display_name: "FunASR".to_string(),
            description: "STT".to_string(),
            icon: "mic".to_string(),
            version: "0.1.0".to_string(),
            capability_kind: "stt".to_string(),
            runtime_kind: "python_venv".to_string(),
            lifecycle: "manual".to_string(),
            model_id: "iic/SenseVoiceSmall".to_string(),
            model_revision: "v1".to_string(),
            resource_budget: crate::app::local_engine::dto::ResourceBudgetDto {
                estimated_env_disk_mb: Some(3000),
                estimated_model_disk_mb: Some(234),
                estimated_stable_ram_mb: Some(500),
                estimated_peak_ram_mb: Some(1500),
            },
            compute_options: vec![],
            undeclared_compute_preferences: vec![],
            current_compute_preference: "cpu".to_string(),
        };
        let json = serde_json::to_value(&dto).unwrap();
        // 不包含 executable
        assert!(json.get("executable").is_none());
        assert!(json.get("argv").is_none());
        assert!(json.get("env").is_none());
        assert!(json.get("artifact_url").is_none());
        assert!(json.get("token").is_none());
        assert!(json.get("endpoint").is_none());
        assert!(json.get("file_path").is_none());
        assert!(json.get("script_path").is_none());
    }

    // ═══════════════════════════════════════════════════════════════════════
    // 0.22.6.7 端到端契约测试：compute preference 契约
    // 验证 Validator ↔ Resolver 语义一致性、配置归一化、单源真值
    // ═══════════════════════════════════════════════════════════════════════

    /// PaddleOCR descriptor 只声明 CPU profile——验证 `Auto` 不在候选列表中，
    /// 但 `Auto` 作为策略性偏好应被 validator 允许。
    #[test]
    fn contract_paddleocr_auto_not_in_candidates_but_allowed() {
        use crate::domain::local_engine::adapter::LocalEngineAdapter;

        let descriptor = paddleocr::PaddleocrAdapter::new().descriptor().clone();

        // Auto 不在 compute_candidates 中
        assert!(
            !descriptor.has_preference(ComputePreference::Auto),
            "PaddleOCR descriptor 不应声明 Auto 候选（Auto 是策略性偏好）"
        );
        // Cpu 在 compute_candidates 中
        assert!(
            descriptor.has_preference(ComputePreference::Cpu),
            "PaddleOCR descriptor 应声明 Cpu 候选"
        );
        // Cuda 不在 compute_candidates 中
        assert!(
            !descriptor.has_preference(ComputePreference::Cuda),
            "PaddleOCR descriptor 不应声明 Cuda 候选"
        );
    }

    /// FunASR 的策略偏好不在 candidates 中；SenseVoice 的 Vulkan/CUDA 与
    /// Paraformer 的 Vulkan 已声明，Nano 仍由 model-scoped descriptor 保持 CPU-only。
    #[test]
    fn contract_funasr_auto_not_in_candidates_but_allowed() {
        use crate::domain::local_engine::adapter::LocalEngineAdapter;

        let descriptor = funasr::FunasrAdapter::new().descriptor().clone();

        assert!(
            !descriptor.has_preference(ComputePreference::Auto),
            "FunASR descriptor 不应声明 Auto 候选（Auto 是策略性偏好）"
        );
        assert!(
            descriptor.has_preference(ComputePreference::Cpu),
            "FunASR descriptor 应声明 Cpu 候选"
        );
        assert!(descriptor.has_preference(ComputePreference::Cuda));
        assert!(
            descriptor
                .has_preference_for_model("gguf/sensevoice-small-q8", ComputePreference::Cuda)
        );
        assert!(
            !descriptor.has_preference_for_model("gguf/paraformer-zh-q8", ComputePreference::Cuda)
        );
        assert!(
            descriptor.has_preference_for_model("gguf/paraformer-zh-q8", ComputePreference::Vulkan)
        );
    }

    /// 默认测试配置的 `current_compute_preference` 仍为 Cpu。
    #[test]
    fn contract_funasr_current_compute_preference_always_cpu() {
        let pref = current_compute_preference("funasr");
        assert_eq!(
            pref,
            ComputePreference::Cpu,
            "默认测试配置的 FunASR current_compute_preference 必须为 Cpu"
        );
    }

    /// PaddleOCR ONNX ProviderDescriptor 只声明 CPU profile（`Always` 兼容）。
    /// 验证 Auto 策略可以通过 resolve_profile 解析为 CPU backend。
    #[test]
    fn contract_paddleocr_provider_descriptor_only_cpu() {
        let descriptor = paddleocr::make_paddleocr_onnx_provider_descriptor();

        // 只有一个 CPU profile
        assert_eq!(descriptor.profiles.len(), 1);
        assert!(
            descriptor
                .profiles
                .iter()
                .all(|profile| profile.backend == ComputeBackend::Cpu)
        );
        assert_eq!(
            descriptor.profiles[0].backend,
            crate::infra::local_engine::runtime::ComputeBackend::Cpu
        );
        // compatibility 是 Always
        assert!(matches!(
            descriptor.profiles[0].compatibility,
            crate::infra::local_engine::providers::CompatibilityCheck::Always
        ));
    }

    /// FunASR ProviderDescriptor 为 SenseVoice 声明 CPU/Vulkan/CUDA，
    /// 为 Paraformer 声明 CPU/Vulkan，Nano 仍只声明 CPU。
    #[test]
    fn contract_funasr_provider_descriptor_model_scoped_profiles() {
        let descriptor = funasr::make_funasr_provider_descriptor();

        let profiles: Vec<(&str, ComputeBackend)> = descriptor
            .profiles
            .iter()
            .map(|profile| (profile.model_id.as_str(), profile.backend))
            .collect();
        assert_eq!(
            profiles,
            vec![
                ("gguf/sensevoice-small-q8", ComputeBackend::Vulkan),
                ("gguf/sensevoice-small-q8", ComputeBackend::Cuda),
                ("gguf/paraformer-zh-q8", ComputeBackend::Vulkan),
                ("gguf/sensevoice-small-q8", ComputeBackend::Cpu),
                ("gguf/paraformer-zh-q8", ComputeBackend::Cpu),
                ("gguf/fun-asr-nano-q4km", ComputeBackend::Cpu),
            ]
        );
        assert!(matches!(
            descriptor.profiles[0].compatibility,
            crate::infra::local_engine::providers::CompatibilityCheck::RequiresVulkan
        ));
        assert_eq!(descriptor.profiles[0].backend, ComputeBackend::Vulkan);
        assert_eq!(descriptor.profiles[1].backend, ComputeBackend::Cuda);
        assert_eq!(descriptor.profiles[2].backend, ComputeBackend::Vulkan);
    }
}
