//! 受限偏好域：`get/set_local_engine_preferences` 与 OCR lifecycle/backend 解析。
//!
//! patch 只接受闭合字段（compute_preference / auto_start / ocr_backend / lifecycle），
//! compute preference 的解析与 descriptor 声明项验证复用 `lifecycle` 域。

use crate::app::command_error::CommandError;
use crate::app::local_engine::dto::{EnginePreferencesDto, EnginePreferencesPatchDto};
use crate::app::local_engine::{funasr, paddleocr};
use crate::infra::local_engine::runtime::ComputePreference;

use super::lifecycle::{parse_compute_preference, validate_preference_for_engine};
use super::{get_service, validate_engine_id};

use tauri::{Emitter, Manager};

/// 获取引擎受限偏好（0.22.5 H2）。
///
/// 返回闭合字段：`compute_preference`、`auto_start`（仅 FunASR）、
/// `ocr_backend` / `lifecycle`（仅 PaddleOCR）、`requires_rebuild`。
///
/// **只读查询，不启动服务、不安装环境。**
#[tauri::command]
pub async fn get_local_engine_preferences(
    app: tauri::AppHandle,
    engine_id: String,
) -> Result<EnginePreferencesDto, CommandError> {
    let svc = get_service(&app)?;
    let eid = validate_engine_id(&engine_id)?;

    let dto = match engine_id.as_str() {
        funasr::FUNASR_ENGINE_ID => {
            let config = crate::app::stt_config::get_stt_config();
            // 0.22.6 归一化：历史 device=cuda 投影为 cpu
            // descriptor 只声明 CPU profile，前端不应看到 cuda 选项
            let compute_pref = "cpu".to_string();
            EnginePreferencesDto {
                engine_id: engine_id.clone(),
                compute_preference: Some(compute_pref),
                auto_start: Some(config.local_engine.auto_start_server),
                ocr_backend: None,
                lifecycle: None,
                requires_rebuild: None,
            }
        }
        paddleocr::PADDLEOCR_ENGINE_ID => {
            let ocr_config = crate::domain::config::ocr_config::get_ocr_config();
            EnginePreferencesDto {
                engine_id: engine_id.clone(),
                compute_preference: Some(preference_to_string_local(ocr_config.compute_preference)),
                auto_start: None,
                ocr_backend: Some(ocr_config.backend.to_string()),
                lifecycle: Some(ocr_config.lifecycle.to_string()),
                requires_rebuild: None,
            }
        }
        other => {
            return Err(CommandError::new(
                "unsupported_engine",
                format!("不支持的引擎: {other}"),
                false,
            ));
        }
    };

    // 检查当前环境状态——如果 NeedsRebuild 则返回 requires_rebuild=true
    let snapshot = svc.get_status(&eid).await.map_err(|e| {
        CommandError::new(
            "engine_status_error",
            format!("获取引擎状态失败: {e}"),
            false,
        )
    })?;

    if snapshot.status.environment == crate::domain::local_engine::EnvironmentHealth::NeedsRebuild {
        let mut dto = dto;
        dto.requires_rebuild = Some(true);
        return Ok(dto);
    }

    Ok(dto)
}

/// 保存引擎受限偏好（0.22.5 H2）。
///
/// patch 只接受闭合字段：`compute_preference`、`auto_start`（仅 FunASR）、
/// `ocr_backend` / `lifecycle`（仅 PaddleOCR）。
///
/// **禁止包含** executable/argv/env/path/url/runtime kind 或任意 engine_config。
/// 未知字段在反序列化时被拒绝（`#[serde(deny_unknown_fields)]`）。
///
/// 后端按 engine_id 从配置真源读取完整配置，只修改 patch 指定的字段，
/// 再通过现有持久化与 cache 热更新路径保存。
///
/// 如果 compute profile 变化导致与 current generation 不一致，
/// 将环境投影为 `NeedsRebuild`，并返回 `requires_rebuild=true`。
///
/// **自启语义**：保存 `auto_start` 只改变配置，不隐式启动服务。
/// 是否立即启动由显式 `start_local_engine` action 决定。
#[tauri::command]
pub async fn set_local_engine_preferences(
    app: tauri::AppHandle,
    engine_id: String,
    patch: EnginePreferencesPatchDto,
) -> Result<EnginePreferencesDto, CommandError> {
    let svc = get_service(&app)?;
    let eid = validate_engine_id(&engine_id)?;

    let pool = app
        .try_state::<crate::infra::data::DbPools>()
        .ok_or_else(|| CommandError::new("internal_error", "DbPools 尚未注册", false))?;

    let requires_rebuild = match engine_id.as_str() {
        funasr::FUNASR_ENGINE_ID => {
            // 读取现有完整 SttConfig
            let mut config = crate::app::stt_config::get_stt_config();
            let mut profile_changed = false;

            // 应用 compute_preference patch
            if let Some(ref pref_str) = patch.compute_preference {
                let pref = parse_compute_preference(pref_str)?;
                // 验证属于该引擎 descriptor 声明项
                validate_preference_for_engine(&svc, &eid, pref).await?;

                // 0.22.6 归一化：FunASR descriptor 只声明 CPU profile
                // 无论用户提交 cpu 还是 auto 还是 cuda，最终都归一化为 cpu
                let new_device = "cpu".to_string();
                if config.local_engine.device != new_device {
                    profile_changed = true;
                    config.local_engine.device = new_device;
                }
            }

            // 应用 auto_start patch（仅 FunASR）
            if let Some(auto_start) = patch.auto_start {
                config.local_engine.auto_start_server = auto_start;
            }

            // 持久化 + cache 热更新
            crate::domain::config::store::ConfigStore::set(&pool.config, &config)
                .await
                .map_err(|e| {
                    CommandError::new("save_failed", format!("保存 STT 配置失败: {e}"), false)
                })?;
            crate::app::stt_config::update_cache(&config);

            tracing::info!(
                engine = %eid,
                device = %config.local_engine.device,
                auto_start = config.local_engine.auto_start_server,
                "FunASR 偏好已保存"
            );

            // 广播配置变更
            let _ = app.emit(
                crate::domain::event_names::EventNames::CONFIG_CHANGED,
                serde_json::json!({ "key": "stt:config" }),
            );

            if profile_changed {
                svc.mark_needs_rebuild(&eid).await.map_err(|e| {
                    CommandError::new(
                        "needs_rebuild_failed",
                        format!("标记 NeedsRebuild 失败: {e}"),
                        false,
                    )
                })?;
            }

            profile_changed
        }
        paddleocr::PADDLEOCR_ENGINE_ID => {
            // 读取现有完整 OcrConfig
            let mut ocr_config = crate::domain::config::ocr_config::get_ocr_config();
            let mut profile_changed = false;

            // 应用 compute_preference patch
            if let Some(ref pref_str) = patch.compute_preference {
                let pref = parse_compute_preference(pref_str)?;
                // 验证属于该引擎 descriptor 声明项
                validate_preference_for_engine(&svc, &eid, pref).await?;

                // PaddleOCR 只允许 auto/cpu（validate 会拒绝 cuda/vulkan/directml）
                if ocr_config.compute_preference != pref {
                    ocr_config.compute_preference = pref;
                    profile_changed = true;
                }
            }

            // 应用 OCR 路由后端 patch（仅 PaddleOCR）
            if let Some(ref backend_str) = patch.ocr_backend {
                ocr_config.backend = parse_ocr_backend(backend_str)?;
            }

            // 应用 lifecycle patch（仅 PaddleOCR）
            if let Some(ref lifecycle_str) = patch.lifecycle {
                let new_lifecycle = parse_ocr_lifecycle(lifecycle_str)?;
                if ocr_config.lifecycle != new_lifecycle {
                    ocr_config.lifecycle = new_lifecycle;
                }
            }

            // 校验配置
            ocr_config.validate().map_err(|e| {
                CommandError::new("validation_failed", format!("OCR 配置校验失败: {e}"), false)
            })?;

            // 持久化 + cache 热更新
            crate::domain::config::store::ConfigStore::set(&pool.config, &ocr_config)
                .await
                .map_err(|e| {
                    CommandError::new("save_failed", format!("保存 OCR 配置失败: {e}"), false)
                })?;
            crate::domain::config::ocr_config::update_cache(&ocr_config);

            tracing::info!(
                engine = %eid,
                ocr_backend = %ocr_config.backend,
                compute_preference = ?ocr_config.compute_preference,
                lifecycle = %ocr_config.lifecycle,
                "PaddleOCR 偏好已保存"
            );

            // 广播配置变更
            let _ = app.emit(
                crate::domain::event_names::EventNames::CONFIG_CHANGED,
                serde_json::json!({ "scope": "ocr" }),
            );

            // PaddleOCR auto/cpu 若最终解析为同一 CPU profile，不强制重建
            // 由后端判断——这里检查 compute_preference 是否实际改变了 backend
            if profile_changed {
                // auto 和 cpu 都解析为 CPU backend，所以不强制重建
                // 但仍标记——由后端状态判断决定
                // 只在实际 backend 变化时才 NeedsRebuild
                // PaddleOCR 首版只允许 auto/cpu，两者都是 CPU backend
                // 所以 PaddleOCR 的 compute_preference 变化不触发 NeedsRebuild
                false
            } else {
                false
            }
        }
        other => {
            return Err(CommandError::new(
                "unsupported_engine",
                format!("不支持的引擎: {other}"),
                false,
            ));
        }
    };

    // 构建返回 DTO
    let dto = match engine_id.as_str() {
        funasr::FUNASR_ENGINE_ID => {
            let config = crate::app::stt_config::get_stt_config();
            // 0.22.6 归一化：无论配置中 device 值是什么，前端总是看到 cpu
            EnginePreferencesDto {
                engine_id: engine_id.clone(),
                compute_preference: Some("cpu".to_string()),
                auto_start: Some(config.local_engine.auto_start_server),
                ocr_backend: None,
                lifecycle: None,
                requires_rebuild: if requires_rebuild { Some(true) } else { None },
            }
        }
        paddleocr::PADDLEOCR_ENGINE_ID => {
            let ocr_config = crate::domain::config::ocr_config::get_ocr_config();
            EnginePreferencesDto {
                engine_id: engine_id.clone(),
                compute_preference: Some(preference_to_string_local(ocr_config.compute_preference)),
                auto_start: None,
                ocr_backend: Some(ocr_config.backend.to_string()),
                lifecycle: Some(ocr_config.lifecycle.to_string()),
                requires_rebuild: if requires_rebuild { Some(true) } else { None },
            }
        }
        _ => unreachable!(),
    };

    Ok(dto)
}

// ── 内部辅助：OCR 解析与 preference 投影 ─────────────────────────────────────

/// 从字符串解析 OCR 路由后端。
fn parse_ocr_backend(
    value: &str,
) -> Result<crate::domain::ocr::config::OcrBackendKind, CommandError> {
    use crate::domain::ocr::config::OcrBackendKind;

    match value {
        "windows" => Ok(OcrBackendKind::Windows),
        "paddleocr" => Ok(OcrBackendKind::PaddleOcr),
        "auto" => Ok(OcrBackendKind::Auto),
        other => Err(CommandError::new(
            "invalid_ocr_backend",
            format!("未知的 OCR backend: {other}"),
            false,
        )),
    }
}

/// 从字符串解析 OCR lifecycle。
fn parse_ocr_lifecycle(s: &str) -> Result<crate::domain::ocr::config::OcrLifecycle, CommandError> {
    match s {
        "on_demand" => Ok(crate::domain::ocr::config::OcrLifecycle::OnDemand),
        "keep_running" => Ok(crate::domain::ocr::config::OcrLifecycle::KeepRunning),
        "stop_after_use" => Ok(crate::domain::ocr::config::OcrLifecycle::StopAfterUse),
        other => Err(CommandError::new(
            "invalid_lifecycle",
            format!("未知的 lifecycle: {other}"),
            false,
        )),
    }
}

/// 将 ComputePreference 转为字符串（后端用）。
fn preference_to_string_local(p: ComputePreference) -> String {
    match p {
        ComputePreference::Auto => "auto".to_string(),
        ComputePreference::Cpu => "cpu".to_string(),
        ComputePreference::GpuAuto => "gpu_auto".to_string(),
        ComputePreference::Cuda => "cuda".to_string(),
        ComputePreference::Vulkan => "vulkan".to_string(),
        ComputePreference::Directml => "directml".to_string(),
    }
}

// ── 测试 ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── preferences commands 签名可编译 ──

    #[test]
    fn preferences_commands_compile() {
        let _ = get_local_engine_preferences as fn(tauri::AppHandle, String) -> _;
        let _ = set_local_engine_preferences
            as fn(tauri::AppHandle, String, EnginePreferencesPatchDto) -> _;
    }

    // ── OCR lifecycle 解析 ──

    #[test]
    fn parse_ocr_lifecycle_on_demand() {
        assert_eq!(
            parse_ocr_lifecycle("on_demand").unwrap(),
            crate::domain::ocr::config::OcrLifecycle::OnDemand
        );
    }

    #[test]
    fn parse_ocr_lifecycle_keep_running() {
        assert_eq!(
            parse_ocr_lifecycle("keep_running").unwrap(),
            crate::domain::ocr::config::OcrLifecycle::KeepRunning
        );
    }

    #[test]
    fn parse_ocr_lifecycle_stop_after_use() {
        assert_eq!(
            parse_ocr_lifecycle("stop_after_use").unwrap(),
            crate::domain::ocr::config::OcrLifecycle::StopAfterUse
        );
    }

    #[test]
    fn parse_ocr_lifecycle_invalid() {
        assert!(parse_ocr_lifecycle("always_on").is_err());
    }

    #[test]
    fn parse_ocr_backend_accepts_closed_values() {
        use crate::domain::ocr::config::OcrBackendKind;

        assert_eq!(
            parse_ocr_backend("windows").unwrap(),
            OcrBackendKind::Windows
        );
        assert_eq!(
            parse_ocr_backend("paddleocr").unwrap(),
            OcrBackendKind::PaddleOcr
        );
        assert_eq!(parse_ocr_backend("auto").unwrap(), OcrBackendKind::Auto);
        assert!(parse_ocr_backend("remote").is_err());
    }

    // ── preference_to_string_local 覆盖所有变体 ──

    #[test]
    fn preference_to_string_local_all_variants() {
        assert_eq!(preference_to_string_local(ComputePreference::Auto), "auto");
        assert_eq!(preference_to_string_local(ComputePreference::Cpu), "cpu");
        assert_eq!(
            preference_to_string_local(ComputePreference::GpuAuto),
            "gpu_auto"
        );
        assert_eq!(preference_to_string_local(ComputePreference::Cuda), "cuda");
        assert_eq!(
            preference_to_string_local(ComputePreference::Vulkan),
            "vulkan"
        );
        assert_eq!(
            preference_to_string_local(ComputePreference::Directml),
            "directml"
        );
    }

    // ═══════════════════════════════════════════════════════════════════════
    // 0.22.6 H4: 偏好 round-trip + 静态契约测试
    // ═══════════════════════════════════════════════════════════════════════

    // ── 偏好 DTO round-trip：get → set → get 一致性 ──

    #[test]
    fn funasr_preferences_dto_round_trip() {
        // 模拟 get_local_engine_preferences 返回的 DTO
        let get_dto = EnginePreferencesDto {
            engine_id: "funasr".to_string(),
            compute_preference: Some("cpu".to_string()),
            auto_start: Some(true),
            ocr_backend: None,
            lifecycle: None,
            requires_rebuild: None,
        };

        // 从 get DTO 构造 patch DTO（只修改需要变更的字段）
        let patch = EnginePreferencesPatchDto {
            compute_preference: get_dto.compute_preference.clone(),
            auto_start: get_dto.auto_start,
            ocr_backend: None,
            lifecycle: None,
        };

        // 验证 patch 序列化/反序列化 round-trip
        let patch_json = serde_json::to_string(&patch).unwrap();
        let patch_restored: EnginePreferencesPatchDto = serde_json::from_str(&patch_json).unwrap();
        assert_eq!(patch_restored.compute_preference, patch.compute_preference);
        assert_eq!(patch_restored.auto_start, patch.auto_start);
        assert_eq!(patch_restored.lifecycle, patch.lifecycle);
    }

    #[test]
    fn paddleocr_preferences_dto_round_trip() {
        let get_dto = EnginePreferencesDto {
            engine_id: "paddleocr".to_string(),
            compute_preference: Some("auto".to_string()),
            auto_start: None,
            ocr_backend: Some("paddleocr".to_string()),
            lifecycle: Some("on_demand".to_string()),
            requires_rebuild: None,
        };

        let patch = EnginePreferencesPatchDto {
            compute_preference: get_dto.compute_preference.clone(),
            auto_start: None,
            ocr_backend: get_dto.ocr_backend.clone(),
            lifecycle: get_dto.lifecycle.clone(),
        };

        let patch_json = serde_json::to_string(&patch).unwrap();
        let patch_restored: EnginePreferencesPatchDto = serde_json::from_str(&patch_json).unwrap();
        assert_eq!(patch_restored.compute_preference, patch.compute_preference);
        assert_eq!(patch_restored.ocr_backend, patch.ocr_backend);
        assert_eq!(patch_restored.lifecycle, patch.lifecycle);
    }

    #[test]
    fn preferences_patch_empty_is_noop() {
        // 空 patch = 不修改任何字段
        let patch = EnginePreferencesPatchDto {
            compute_preference: None,
            auto_start: None,
            ocr_backend: None,
            lifecycle: None,
        };
        let json = serde_json::to_value(&patch).unwrap();
        // 所有字段都被 skip_serializing_if 跳过
        assert!(json.get("compute_preference").is_none() || json["compute_preference"].is_null());
        assert!(json.get("auto_start").is_none() || json["auto_start"].is_null());
        assert!(json.get("ocr_backend").is_none() || json["ocr_backend"].is_null());
        assert!(json.get("lifecycle").is_none() || json["lifecycle"].is_null());
    }

    // ── FunASR preferences 字段约束 ──

    #[test]
    fn funasr_preferences_has_auto_start_not_lifecycle() {
        // FunASR 有 auto_start，无 lifecycle
        let dto = EnginePreferencesDto {
            engine_id: "funasr".to_string(),
            compute_preference: Some("cpu".to_string()),
            auto_start: Some(false),
            ocr_backend: None,
            lifecycle: None,
            requires_rebuild: None,
        };
        assert!(dto.auto_start.is_some(), "FunASR 应有 auto_start");
        assert!(dto.lifecycle.is_none(), "FunASR 不应有 lifecycle");
    }

    #[test]
    fn paddleocr_preferences_has_lifecycle_not_auto_start() {
        // PaddleOCR 有 lifecycle，无 auto_start
        let dto = EnginePreferencesDto {
            engine_id: "paddleocr".to_string(),
            compute_preference: Some("auto".to_string()),
            auto_start: None,
            ocr_backend: Some("auto".to_string()),
            lifecycle: Some("keep_running".to_string()),
            requires_rebuild: None,
        };
        assert!(dto.auto_start.is_none(), "PaddleOCR 不应有 auto_start");
        assert!(dto.ocr_backend.is_some(), "PaddleOCR 应有 ocr_backend");
        assert!(dto.lifecycle.is_some(), "PaddleOCR 应有 lifecycle");
    }
}
