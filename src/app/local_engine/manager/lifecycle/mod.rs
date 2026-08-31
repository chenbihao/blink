//! EngineManager 进程生命周期用例（0.22 B2 拆分）：
//! start / stop / 条件停止、回滚（rollback_started_instance）、exit monitor、
//! lease 写入与退出回收（shutdown_all / shutdown_all_blocking）。
//!
//! ## 子模块职责
//!
//! - `start` — start 与启动后身份/health/registry 提交、lease 写入、exit monitor
//! - `stop` — stop、stop_if_current、graceful_stop_worker、clear_running_instance、rollback
//! - `shutdown` — shutdown_all、shutdown_all_blocking、进程级收尾
//!
//! ## 共享私有 helper
//!
//! `build_process_lease` 和 `resolve_expected_model_identity` 是跨子模块
//! 共享的私有 helper，定义在本 facade 中。

use super::*;

mod shutdown;
mod start;
mod stop;

// ── 动态模型身份解析（0.22.6 B2）─────────────────────────────────────────

/// 从 model_storage manifest 动态解析当前安装的模型身份。
///
/// 返回 `(model_id, revision, fingerprint)` 三元组（如果模型已安装且有效）。
///
/// **asset_key 真源**：managed 模式下用 `selected_model_id`（配置选中的模型，
/// 如 funasr 的 `funasr_model`）查找 manifest；`fallback_contract.model_id`
/// 只是 descriptor 默认占位——用户可能安装/选择了其他模型（如装了
/// paraformer-zh 而 descriptor 默认 SenseVoiceSmall），按硬编码查找会
/// 误报"模型未安装"。
///
/// **0.22.6 B2 fail-closed 铁则**：模型未安装、损坏或恢复失败时返回 `Err`，
/// 不再回退到 descriptor 静态值。调用方必须将此视为启动/健康检查失败。
///
/// 这确保 health Ready 校验只与实际安装的 manifest 比对，
/// 而非与 descriptor 中编译期常量比对——防止
/// "下载了模型 A 但 health 期望模型 B" 的静默通过。
pub(super) fn resolve_expected_model_identity(
    engine_id: &EngineId,
    selected_model_id: Option<&str>,
    fallback_contract: &ModelContract,
    uses_managed_model_storage: bool,
) -> Result<(String, String, Option<String>), String> {
    if !uses_managed_model_storage {
        return Ok((
            fallback_contract.model_id.clone(),
            fallback_contract.revision.clone(),
            None,
        ));
    }

    // 使用配置选中的 model_id 作为 asset_key 的来源
    let model_id_for_key = selected_model_id
        .filter(|m| !m.is_empty())
        .unwrap_or(&fallback_contract.model_id);
    let asset_key = mstore::encode_asset_key(model_id_for_key);
    match mstore::restore_model_state(engine_id, &asset_key) {
        Ok(mstore::RestoredModelState::Installed { manifest, .. }) => Ok((
            manifest.model_id,
            manifest.revision,
            Some(manifest.content_fingerprint),
        )),
        Ok(mstore::RestoredModelState::Corrupted { reason, .. }) => {
            tracing::warn!(
                engine_id = %engine_id,
                model_id = %model_id_for_key,
                reason = %reason,
                "模型状态 Corrupted——fail-closed，不回退到 descriptor 静态身份"
            );
            Err(format!("模型状态 Corrupted: {reason}"))
        }
        Ok(mstore::RestoredModelState::NotInstalled) => {
            tracing::debug!(
                engine_id = %engine_id,
                model_id = %model_id_for_key,
                "模型未安装——fail-closed，不回退到 descriptor 静态身份"
            );
            Err(format!("模型未安装: {model_id_for_key}"))
        }
        Err(e) => {
            tracing::warn!(
                engine_id = %engine_id,
                model_id = %model_id_for_key,
                error = %e,
                "模型状态恢复失败——fail-closed，不回退到 descriptor 静态身份"
            );
            Err(format!("模型状态恢复失败: {e}"))
        }
    }
}

/// 用服务身份与 OS 进程证据构造持久化 lease。
///
/// `ManagedProcess` 的 `ProcessIdentity::instance_id` 是 infra 状态机用于隔离
/// generation 的内部 token；health、回滚与恢复协议使用的是
/// `ServiceIdentityInput::instance_id`。lease 必须保存后者，否则 start 回滚时
/// 无法通过 instance 校验删除本次写入的 lease。
pub(super) fn build_process_lease(
    engine_id: &EngineId,
    process_identity: &ProcessIdentity,
    service_identity: &ServiceIdentityInput,
    endpoint: &crate::infra::local_engine::port::Endpoint,
    generation_id: String,
) -> ProcessLease {
    ProcessLease::new(
        engine_id.to_string(),
        service_identity.instance_id.clone(),
        process_identity.pid,
        process_identity.start_time_ms,
        process_identity.executable.to_string_lossy().to_string(),
        endpoint.base_url(),
        service_identity.token_fingerprint(),
        generation_id,
    )
}
