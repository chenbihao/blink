//! 存储域：`get_local_engine_storage`（只读扫描）与 `cleanup_local_engine`（清理）。
//!
//! 存储目标的解析真源在后端——前端只提交 `target_ids`，
//! 不信任前端提交的路径/size/shared/current。

use crate::app::command_error::CommandError;
use crate::app::local_engine::dto::{CleanupRequestDto, CleanupResultDto, EngineStorageDto};

use super::{get_service, validate_engine_id};

/// 获取本地引擎存储概览。
///
/// 返回所有可诊断/可清理的存储目标（generations、model cache、
/// shared artifacts、download cache、legacy）。
///
/// **只读扫描，在 spawn_blocking 中执行，不阻塞主链路。**
/// 前端据此展示预览和确认弹窗，不暴露用户目录完整路径。
#[tauri::command]
pub async fn get_local_engine_storage(
    app: tauri::AppHandle,
    engine_id: String,
) -> Result<EngineStorageDto, CommandError> {
    let svc = get_service(&app)?;
    let eid = validate_engine_id(&engine_id)?;

    let storage = svc.scan_storage(&eid).await.map_err(CommandError::from)?;

    tracing::info!(
        engine = %eid,
        targets = storage.targets.len(),
        total_bytes = storage.total_size_bytes,
        releasable_bytes = storage.releasable_size_bytes,
        "存储扫描完成"
    );
    Ok(storage)
}

/// 清理本地引擎资产。
///
/// 前端提交 `engine_id` + `target_ids` + `operation_id`（可选）。
/// 后端重新解析每个 `target_id`，**不信任前端提交的路径/size/shared/current**。
///
/// 禁止提交任意路径。current generation 默认不可删除。
/// 共享资产经过引用检查。
#[tauri::command]
pub async fn cleanup_local_engine(
    app: tauri::AppHandle,
    request: CleanupRequestDto,
) -> Result<CleanupResultDto, CommandError> {
    let svc = get_service(&app)?;
    let eid = validate_engine_id(&request.engine_id)?;

    if request.target_ids.is_empty() {
        return Err(CommandError::new(
            "invalid_request",
            "target_ids 不能为空",
            false,
        ));
    }

    tracing::info!(
        engine = %eid,
        targets = request.target_ids.len(),
        op_id = ?request.operation_id,
        "开始清理引擎资产"
    );

    let result = svc
        .cleanup_targets(&eid, &request.target_ids, request.operation_id)
        .await
        .map_err(CommandError::from)?;

    tracing::info!(
        engine = %eid,
        cleaned = result.cleaned_target_ids.len(),
        skipped = result.skipped_target_ids.len(),
        deferred = result.deferred_target_ids.len(),
        released_bytes = result.released_bytes,
        "清理完成"
    );
    Ok(result)
}

// ── 测试 ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {

    // ── storage DTO 不暴露完整路径 ──

    #[test]
    fn storage_dto_does_not_expose_full_paths() {
        use crate::app::local_engine::dto::{
            EngineStorageDto, StorageTargetDto, StorageTargetKindDto,
        };

        let target = StorageTargetDto {
            target_id: "environment:slot-a".to_string(),
            kind: StorageTargetKindDto::EngineEnvironment,
            engine_id: Some("funasr".to_string()),
            label_key: "local_engine.storage.engine_environment".to_string(),
            label_fallback: "当前环境".to_string(),
            size_bytes: 3000 * 1024 * 1024,
            current: true,
            removable: false,
            shared: false,
            requires_separate_confirmation: false,
            blocked_reason: Some("current_environment".to_string()),
            affected_engine_ids: None,
            reference_count: None,
            path_display: None,
        };

        let dto = EngineStorageDto {
            engine_id: Some("funasr".to_string()),
            targets: vec![target],
            total_size_bytes: 3000 * 1024 * 1024,
            releasable_size_bytes: 0,
        };

        let json = serde_json::to_value(&dto).unwrap();
        // 不包含完整文件路径字段
        assert!(json.get("path").is_none());
        assert!(json.get("file_path").is_none());
        assert!(json.get("dir_path").is_none());
        // target_id 是安全暴露的标识符
        assert!(json["targets"][0]["target_id"].is_string());
    }

    // ── cleanup DTO 包含 required fields ──

    #[test]
    fn cleanup_result_dto_has_required_fields() {
        use crate::app::local_engine::dto::CleanupResultDto;

        let dto = CleanupResultDto {
            engine_id: "funasr".to_string(),
            operation_id: "op-abc123".to_string(),
            cleaned_target_ids: vec!["gen:old".to_string()],
            skipped_target_ids: vec![],
            released_bytes: 1024 * 1024 * 500,
            deferred_target_ids: vec![],
            error: None,
        };

        let json = serde_json::to_value(&dto).unwrap();
        assert_eq!(json["engine_id"], "funasr");
        assert_eq!(json["operation_id"], "op-abc123");
        assert!(json["cleaned_target_ids"].is_array());
        assert!(json["skipped_target_ids"].is_array());
        assert!(json["deferred_target_ids"].is_array());
        assert!(json["released_bytes"].is_number());
    }

    // ── cleanup 请求空 target_ids 被拒绝 ──

    #[test]
    fn cleanup_request_dto_deserializes() {
        use crate::app::local_engine::dto::CleanupRequestDto;

        let json = serde_json::json!({
            "engine_id": "funasr",
            "target_ids": ["gen:abc123", "model_cache"],
            "operation_id": "op-123"
        });

        let dto: CleanupRequestDto = serde_json::from_value(json).unwrap();
        assert_eq!(dto.engine_id, "funasr");
        assert_eq!(dto.target_ids.len(), 2);
        assert_eq!(dto.operation_id, Some("op-123".to_string()));
    }
}
