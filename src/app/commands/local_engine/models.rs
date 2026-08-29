//! 模型资产生命周期域：list/install/delete/repair/cancel 模型 commands。
//!
//! 模型资产业务由 EngineManager 统一承载（单一业务真相）——
//! 删除冲突检查（selected/active）、事务与互斥都在 manager 内部完成，
//! commands 层只做参数校验与 DTO 投影。

use crate::app::command_error::CommandError;

use super::{get_service, validate_engine_id};

/// 列出引擎的所有模型候选及其当前状态。
///
/// **只读查询，无副作用。** 前端据此展示模型列表，
/// 但**不触发下载**——下载只在引擎页管理。
#[tauri::command]
pub async fn list_engine_models(
    app: tauri::AppHandle,
    engine_id: String,
) -> Result<Vec<crate::app::local_engine::model_installer::ModelCatalogItemDto>, CommandError> {
    let svc = get_service(&app)?;
    let eid = validate_engine_id(&engine_id)?;

    let models = svc.list_models(&eid).await;

    // 投影为 DTO
    let dtos: Vec<crate::app::local_engine::model_installer::ModelCatalogItemDto> = models
        .iter()
        .map(|status| {
            let desc = svc
                .model_registry()
                .find(&eid, &status.model_id)
                .expect("模型状态必须有对应 descriptor");
            crate::app::local_engine::model_installer::project_model_status(desc, status)
        })
        .collect();

    Ok(dtos)
}

/// 安装引擎模型（真实事务：staging/下载/校验/提升）。
///
/// 前端只需提交 `engine_id`、`model_id`、`operation_id`（可选）。
/// **禁止包含 URL、路径、脚本、外部命令。**
#[tauri::command]
pub async fn install_engine_model(
    app: tauri::AppHandle,
    request: crate::app::local_engine::model_installer::ModelOperationRequestDto,
) -> Result<crate::app::local_engine::model_installer::ModelOperationResultDto, CommandError> {
    let svc = get_service(&app)?;
    let eid = validate_engine_id(&request.engine_id)?;

    tracing::info!(
        engine = %eid,
        model = %request.model_id,
        op_id = ?request.operation_id,
        "收到模型安装请求"
    );

    let result = svc
        .install_model(&eid, &request.model_id, request.operation_id)
        .await
        .map_err(CommandError::from)?;

    tracing::info!(
        engine = %eid,
        model = %result.model_id,
        op_id = ?result.operation_id,
        success = result.success,
        "模型安装操作完成"
    );

    Ok(crate::app::local_engine::model_installer::project_model_operation_result(&result))
}

/// 删除引擎模型（引用检查 + 删除）。
///
/// **删除正在使用或被配置引用的模型必须返回结构化冲突**，
/// 不能静默切换到其他模型。冲突判定：
/// - selected（配置真源）；
/// - active（launch snapshot 冻结的模型身份 + instance_id）；
/// - descriptor 默认模型不构成删除保护。
#[tauri::command]
pub async fn delete_engine_model(
    app: tauri::AppHandle,
    request: crate::app::local_engine::model_installer::ModelOperationRequestDto,
) -> Result<crate::app::local_engine::model_installer::ModelOperationResultDto, CommandError> {
    let svc = get_service(&app)?;
    let eid = validate_engine_id(&request.engine_id)?;

    tracing::info!(
        engine = %eid,
        model = %request.model_id,
        op_id = ?request.operation_id,
        "收到模型删除请求"
    );

    let result = svc
        .delete_model(&eid, &request.model_id, request.operation_id)
        .await
        .map_err(CommandError::from)?;

    tracing::info!(
        engine = %eid,
        model = %result.model_id,
        op_id = ?result.operation_id,
        success = result.success,
        "模型删除操作完成"
    );

    Ok(crate::app::local_engine::model_installer::project_model_operation_result(&result))
}

/// 修复引擎模型（重新下载/校验）。
#[tauri::command]
pub async fn repair_engine_model(
    app: tauri::AppHandle,
    request: crate::app::local_engine::model_installer::ModelOperationRequestDto,
) -> Result<crate::app::local_engine::model_installer::ModelOperationResultDto, CommandError> {
    let svc = get_service(&app)?;
    let eid = validate_engine_id(&request.engine_id)?;

    tracing::info!(
        engine = %eid,
        model = %request.model_id,
        op_id = ?request.operation_id,
        "收到模型修复请求"
    );

    let result = svc
        .repair_model(&eid, &request.model_id, request.operation_id)
        .await
        .map_err(CommandError::from)?;

    tracing::info!(
        engine = %eid,
        model = %result.model_id,
        op_id = ?result.operation_id,
        success = result.success,
        "模型修复操作完成"
    );

    Ok(crate::app::local_engine::model_installer::project_model_operation_result(&result))
}

/// 取消模型操作（只触发匹配 operation_id 的 claim token；
/// worker 结束前 claim 不释放）。
#[tauri::command]
pub async fn cancel_model_operation(
    app: tauri::AppHandle,
    engine_id: String,
    model_id: String,
    operation_id: String,
) -> Result<crate::app::local_engine::model_installer::ModelOperationResultDto, CommandError> {
    let svc = get_service(&app)?;
    let eid = validate_engine_id(&engine_id)?;

    tracing::info!(
        engine = %eid,
        model = %model_id,
        op_id = %operation_id,
        "收到取消模型操作请求"
    );

    let result = svc
        .cancel_model_operation(&eid, &model_id, &operation_id)
        .await
        .map_err(CommandError::from)?;

    tracing::info!(
        engine = %eid,
        model = %result.model_id,
        op_id = ?result.operation_id,
        success = result.success,
        "取消模型操作完成"
    );

    Ok(crate::app::local_engine::model_installer::project_model_operation_result(&result))
}
