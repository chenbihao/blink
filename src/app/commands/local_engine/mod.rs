//! 通用本地引擎管理 commands（0.22.5 H1）。
//!
//! 为设置页提供 provider-neutral、allowlist 化的管理 API。
//! 前端只能提交 `engine_id`、闭合 action 和有限配置，
//! **绝不能**提交 executable、argv、env、脚本路径、runtime kind、artifact URL。
//!
//! ## 命令清单
//!
//! | command | 职责 |
//! |---|---|
//! | `get_local_engine_catalog` | 返回所有引擎 catalog（只读） |
//! | `get_local_engine_status` | 返回引擎状态（可选 engine_id；无值时返回全部） |
//! | `get_local_engine_logs` | 返回结构化日志历史 |
//! | `install_local_engine` | 安装引擎环境 |
//! | `start_local_engine` | 启动引擎服务 |
//! | `stop_local_engine` | 停止引擎服务 |
//! | `repair_local_engine` | 修复引擎环境 |
//! | `get_local_engine_storage` | 返回存储概览（只读，spawn_blocking） |
//! | `cleanup_local_engine` | 清理引擎资产（target_ids → 后端重新解析） |
//! | `cancel_local_engine_operation` | 取消匹配 operation_id 的操作 |
//! | `list_engine_models` | 列出引擎模型候选及状态（只读） |
//! | `install_engine_model` | 安装引擎模型（真实事务） |
//! | `delete_engine_model` | 删除引擎模型（引用检查 + 删除） |
//! | `repair_engine_model` | 修复引擎模型（重新下载/校验） |
//! | `cancel_model_operation` | 取消进行中的模型操作 |
//!
//! ## 安全约束
//!
//! - `engine_id` 必须在编译期 allowlist 中
//! - `compute_preference` 必须先验证属于该引擎 descriptor 声明项
//! - action command 内部从现有配置真源构造 `AdapterConfig`：
//!   - funasr → `SttConfig.local_engine`
//!   - paddleocr → `OcrConfig` / `PaddleOcrEngineConfig`
//! - 禁止前端直接提交 `AdapterConfig.engine_config`
//!
//! ## 兼容性
//!
//! 不破坏旧 `get_funasr_env` / `setup_python_env` / `start_funasr_server` 等兼容命令
//! 和旧事件投影。
//!
//! ## 子模块结构（0.22）
//!
//! 按命令域拆分：`catalog`（目录与兼容性）、`lifecycle`（安装/启停/修复/取消）、
//! `models`（模型资产生命周期）、`storage`（存储扫描与清理）、
//! `diagnostics`（状态/日志/诊断查询）、`preferences`（受限偏好读写）。
//! 跨域共享 helper 收敛在本文件；无法按域归类的通用契约测试在 `tests.rs`。

mod catalog;
mod diagnostics;
mod lifecycle;
mod models;
mod preferences;
mod storage;
#[cfg(test)]
mod tests;

pub use catalog::*;
pub use diagnostics::*;
pub use lifecycle::*;
pub use models::*;
pub use preferences::*;
pub use storage::*;

use std::sync::Arc;

use crate::app::command_error::CommandError;
use crate::app::local_engine::EngineManager;
use crate::app::local_engine::dto::{EngineLogDto, EngineLogLevel};
use crate::infra::local_engine::runtime::{ComputePreference, EngineId};

use tauri::Manager;

// ── 跨域共享 helper ──────────────────────────────────────────────────────────

/// 从 managed state 获取 `EngineManager` 引用。
fn get_service(app: &tauri::AppHandle) -> Result<Arc<EngineManager>, CommandError> {
    app.try_state::<Arc<EngineManager>>()
        .map(|s| s.inner().clone())
        .ok_or_else(|| CommandError::new("internal_error", "EngineManager 尚未注册", false))
}

/// 合并 instance 日志和 operation 日志，去重、排序、截断。
///
/// 去重身份：`(source_kind, source_id, seq)`
/// - instance 日志：`("instance", instance_id, seq)`
/// - operation 日志：`("operation", operation_id, seq)`
///
/// 合并后按 timestamp 排序；timestamp 相同时用 `(source_kind, source_id, seq)` 做稳定 tie-break。
/// 最后统一执行 `max_lines` 截断。
async fn get_merged_logs(
    app: &tauri::AppHandle,
    svc: &EngineManager,
    eid: &EngineId,
    max_lines: usize,
) -> Result<Vec<EngineLogDto>, CommandError> {
    // ── source 1: instance 日志 ──
    let instance_logs = svc
        .get_logs_structured(eid, max_lines)
        .await
        .map_err(|e| CommandError::new("engine_logs_error", format!("获取日志失败: {e}"), false))?;

    let mut merged: Vec<((&str, String, u64), EngineLogDto)> = instance_logs
        .iter()
        .map(|entry| {
            let timestamp = chrono::DateTime::from_timestamp_millis(entry.timestamp_ms as i64)
                .map(|dt| dt.to_rfc3339())
                .unwrap_or_else(|| chrono::Utc::now().to_rfc3339());
            let dto = EngineLogDto {
                engine_id: entry.engine_id.clone(),
                instance_id: entry.instance_id.clone(),
                operation_id: None,
                seq: entry.seq.to_string(),
                timestamp,
                level: EngineLogLevel::from_str_lossy(&entry.level),
                text: entry.text.clone(),
            };
            (("instance", entry.instance_id.clone(), entry.seq), dto)
        })
        .collect();

    // ── source 2: operation 日志 ──
    if let Some(store) =
        app.try_state::<std::sync::Arc<crate::app::local_engine::OperationLogStore>>()
    {
        let op_logs = store.query(eid);
        for log in op_logs {
            let dto = EngineLogDto {
                engine_id: log.engine_id.clone(),
                instance_id: String::new(),
                operation_id: Some(log.operation_id.clone()),
                seq: log.seq.to_string(),
                timestamp: log.timestamp.clone(),
                level: EngineLogLevel::from_str_lossy(&log.level),
                text: log.text.clone(),
            };
            merged.push((("operation", log.operation_id.clone(), log.seq), dto));
        }
    }

    // ── 去重 ──
    let mut seen: std::collections::HashSet<(&str, String, u64)> = std::collections::HashSet::new();
    merged.retain(|(key, _)| seen.insert(key.clone()));

    // ── 排序：timestamp + 稳定 tie-break ──
    merged.sort_by(|a, b| {
        let cmp = a.1.timestamp.cmp(&b.1.timestamp);
        if cmp != std::cmp::Ordering::Equal {
            return cmp;
        }
        let kind_cmp = a.0.0.cmp(b.0.0);
        if kind_cmp != std::cmp::Ordering::Equal {
            return kind_cmp;
        }
        let sid_cmp = a.0.1.cmp(&b.0.1);
        if sid_cmp != std::cmp::Ordering::Equal {
            return sid_cmp;
        }
        a.0.2.cmp(&b.0.2)
    });

    // ── 截断 ──
    let start = if merged.len() > max_lines {
        merged.len() - max_lines
    } else {
        0
    };

    Ok(merged[start..].iter().map(|(_, dto)| dto.clone()).collect())
}

/// 从 `ProcessState` 投影为 `ProcessStateDto`（复用 dto.rs 中的投影函数）。
fn project_process_state_dto(
    process: &crate::domain::local_engine::ProcessState,
) -> crate::app::local_engine::dto::ProcessStateDto {
    crate::app::local_engine::dto::project_process_state(process)
}

/// 验证 engine_id 并返回 `EngineId`。
fn validate_engine_id(engine_id: &str) -> Result<EngineId, CommandError> {
    EngineId::new(engine_id).map_err(|e| {
        CommandError::new("invalid_engine_id", format!("无效的 engine_id: {e}"), false)
    })
}

/// 从配置真源读取当前 compute preference。
///
/// 真源在 [`crate::app::local_engine::config_source`]——command 层不再
/// 复制归一化规则（0.22.6：funasr descriptor 只声明 CPU profile）。
fn current_compute_preference(engine_id: &str) -> ComputePreference {
    EngineId::new(engine_id)
        .map(|eid| crate::app::local_engine::config_source::current_compute_preference(&eid))
        .unwrap_or(ComputePreference::Auto)
}

/// 从配置真源构造 `AdapterConfig`。
///
/// **禁止前端直接提交 `AdapterConfig.engine_config`**。
/// 唯一构造入口在 [`crate::app::local_engine::config_source`]——
/// 与 EngineManager（repair）、wiring（自启）共用同一份规则，
/// 避免 repair 用 A 配置装、start 用 B 配置跑的规则漂移。
fn build_adapter_config_for_engine(
    engine_id: &str,
) -> Result<crate::domain::local_engine::AdapterConfig, CommandError> {
    let eid = validate_engine_id(engine_id)?;
    crate::app::local_engine::config_source::adapter_config_for_engine(&eid).ok_or_else(|| {
        CommandError::new(
            "unsupported_engine",
            format!("不支持的引擎: {engine_id}"),
            false,
        )
    })
}
