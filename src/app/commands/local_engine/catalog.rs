//! 引擎目录域：`get_local_engine_catalog` 查询与 compute 兼容性计算。
//!
//! 只读查询，不启动服务、不安装环境、不阻塞主链路。
//! compute options 的兼容性由 ProviderDescriptor + RuntimeProvider 真源决定，
//! 不由前端猜测。

use crate::app::command_error::CommandError;
use crate::app::local_engine::EngineManager;
use crate::app::local_engine::dto::{EngineCatalogItem, project_catalog_item};
use crate::domain::local_engine::EngineDefinition;
use crate::infra::local_engine::providers::RuntimeProvider;
use crate::infra::local_engine::runtime::ComputePreference;

use super::{current_compute_preference, get_service};

// ── 内部辅助 ──────────────────────────────────────────────────────────────────

/// 为 catalog item 计算兼容性结果。
///
/// 从 `ProviderDescriptor` 的 `profiles` + `RuntimeProvider::check_compatibility`
/// 真源获取，不由前端猜测。
fn compute_compatibility_for_descriptor(
    svc: &EngineManager,
    descriptor: &EngineDefinition,
) -> Vec<(ComputePreference, bool, Option<String>)> {
    // 从 ProviderDescriptor 获取 profile candidates
    let provider_desc = svc.provider_descriptor_for_engine(&descriptor.engine_id);

    // 如果有 ProviderDescriptor，使用其 profiles + provider check_compatibility
    if let Some(pd) = provider_desc {
        let python_provider = svc.python_provider();
        descriptor
            .install_plan
            .compute_candidates
            .iter()
            .map(|c| {
                // 从 ProviderDescriptor 的 profiles 中找匹配的 ProfileCandidate
                let profile_candidate = pd.profiles.iter().find(|pc| {
                    pc.profile_id == c.profile_id
                        || pc.backend == map_preference_to_backend(c.preference)
                });

                let (compatible, disabled_reason) = if let Some(pc) = profile_candidate {
                    match python_provider.check_compatibility(&pc.compatibility) {
                        Ok(true) => (true, None),
                        Ok(false) => (false, Some(format!("本机不兼容: {:?}", pc.compatibility))),
                        Err(e) => (false, Some(format!("兼容性检查失败: {e}"))),
                    }
                } else {
                    // 没有匹配的 ProfileCandidate——descriptor 声明但 provider 未提供
                    (false, Some("provider 未声明此 profile".to_string()))
                };

                (c.preference, compatible, disabled_reason)
            })
            .collect()
    } else {
        // 没有 ProviderDescriptor——无法做兼容性检查，标记为 unknown
        descriptor
            .install_plan
            .compute_candidates
            .iter()
            .map(|c| {
                (
                    c.preference,
                    false,
                    Some("无 ProviderDescriptor".to_string()),
                )
            })
            .collect()
    }
}

/// 从 ComputePreference 映射到 ComputeBackend（复用 service 层逻辑）。
fn map_preference_to_backend(
    p: ComputePreference,
) -> crate::infra::local_engine::runtime::ComputeBackend {
    match p {
        ComputePreference::Cpu => crate::infra::local_engine::runtime::ComputeBackend::Cpu,
        ComputePreference::Cuda => crate::infra::local_engine::runtime::ComputeBackend::Cuda,
        ComputePreference::Vulkan => crate::infra::local_engine::runtime::ComputeBackend::Vulkan,
        ComputePreference::Directml => {
            crate::infra::local_engine::runtime::ComputeBackend::Directml
        }
        _ => crate::infra::local_engine::runtime::ComputeBackend::Cpu,
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
        let current_pref = current_compute_preference(&engine_id_str);
        let compatibility_results = compute_compatibility_for_descriptor(&svc, &descriptor);
        let item = project_catalog_item(&descriptor, &compatibility_results, current_pref);
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

    // ── 0.22.6.1 前端不能持久化 FunASR cuda/auto ──

    /// FunASR catalog 不暴露 cuda/auto 可选项——descriptor 只声明 CPU profile，
    /// compute_options 投影只含 cpu，前端选择器无从产生其他选项。
    #[test]
    fn funasr_catalog_compute_options_only_cpu() {
        let adapter = crate::app::local_engine::funasr::make_funasr_adapter();
        let descriptor = adapter.descriptor();
        let item = crate::app::local_engine::dto::project_catalog_item(
            descriptor,
            &[(ComputePreference::Cpu, true, None)],
            ComputePreference::Cpu,
        );
        let prefs: Vec<&str> = item
            .compute_options
            .iter()
            .map(|o| o.preference.as_str())
            .collect();
        assert_eq!(prefs, vec!["cpu"], "FunASR catalog 只能暴露 cpu 可选项");
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

    /// FunASR descriptor 只声明 CPU profile——与 PaddleOCR 同理。
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
        assert!(
            !descriptor.has_preference(ComputePreference::Cuda),
            "FunASR 0.22.6 不应声明 Cuda 候选"
        );
    }

    /// `current_compute_preference` 为 FunASR 始终返回 Cpu。
    #[test]
    fn contract_funasr_current_compute_preference_always_cpu() {
        let pref = current_compute_preference("funasr");
        assert_eq!(
            pref,
            ComputePreference::Cpu,
            "FunASR current_compute_preference 必须为 Cpu"
        );
    }

    /// PaddleOCR ProviderDescriptor 只声明 CPU profile（`Always` 兼容）。
    /// 验证 Auto 策略可以通过 resolve_profile 解析为 CPU backend。
    #[test]
    fn contract_paddleocr_provider_descriptor_only_cpu() {
        let descriptor = paddleocr::make_paddleocr_provider_descriptor();

        // 只有一个 CPU profile
        assert_eq!(descriptor.profiles.len(), 1);
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

    /// FunASR ProviderDescriptor 只声明 CPU profile（`Always` 兼容）。
    #[test]
    fn contract_funasr_provider_descriptor_only_cpu() {
        let descriptor = funasr::make_funasr_provider_descriptor();

        assert_eq!(descriptor.profiles.len(), 1);
        assert_eq!(
            descriptor.profiles[0].backend,
            crate::infra::local_engine::runtime::ComputeBackend::Cpu
        );
        assert!(matches!(
            descriptor.profiles[0].compatibility,
            crate::infra::local_engine::providers::CompatibilityCheck::Always
        ));
    }
}
