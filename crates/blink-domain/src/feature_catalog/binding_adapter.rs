//! Binding adapter —— 批量操作写回各 binding store（0.21.4 §5.5 第 6/8 条）。
//!
//! 功能目录的"本地"状态是各 binding store 的聚合投影；
//! 行/组级批量操作调用各 binding store 的批量 adapter 写回原真源。
//!
//! 三类 binding store：
//! - `SearchKeyword` / `ContextBinding` → `disabled_builtin_actions` / `disabled_context_bindings`
//!   （`DisableConfig` 分片）
//! - `ChordKey` → `disabled_chord_actions`（`DisableConfig` 分片）
//!
//! 所有写操作成功后广播 `blink://config-changed`，前端据此刷新目录。

use sqlx::SqlitePool;

use super::types::*;

use crate::config::{ConfigStore, DisableConfig};

/// 批量执行 binding 操作，写回各 binding store。
///
/// # 参数
/// - `pool`: 配置库连接池
/// - `ops`: 操作列表
///
/// # 返回
/// 每个操作的结果列表。单个操作失败不影响其他操作。
///
/// # 副作用
/// 成功写入后广播 `blink://config-changed`（由调用方 command 层负责，
/// 因为 adapter 在 domain 层，不持有 AppHandle）。
pub async fn apply_binding_batch(pool: &SqlitePool, ops: &[BindingOp]) -> Vec<ApplyBindingResult> {
    let mut results = Vec::with_capacity(ops.len());

    // 生成操作结果；持久化在最新 DisableConfig 上应用同一组字段变更。
    let mut config = crate::config::AppConfig::default();

    for op in ops {
        let result = apply_single_op(&mut config, op);
        results.push(result);
    }

    // 一次性写回
    if let Err(e) = ConfigStore::update(pool, |disable: &mut DisableConfig| {
        for op in ops {
            let (list, id) = match op.kind {
                BindingKind::SearchKeyword => (
                    &mut disable.disabled_builtin_actions,
                    op.binding_id.as_str(),
                ),
                BindingKind::ContextBinding => (
                    &mut disable.disabled_context_bindings,
                    op.binding_id.as_str(),
                ),
                BindingKind::ChordKey => (
                    &mut disable.disabled_chord_actions,
                    op.binding_id
                        .strip_prefix("chord.")
                        .unwrap_or(&op.binding_id),
                ),
            };
            apply_to_disabled_list(list, id, op.op)?;
        }
        Ok(())
    })
    .await
    {
        tracing::warn!(error = %e, "apply_binding_batch: 配置写回失败");
        // 标记所有操作为失败
        for result in &mut results {
            if result.success {
                result.success = false;
                result.error = Some(format!("配置写回失败: {e}"));
            }
        }
    }

    results
}

/// 对内存 config 快照执行单个操作。
fn apply_single_op(
    config: &mut crate::config::AppConfig,
    op: &BindingOp,
) -> ApplyBindingResult {
    match op.kind {
        BindingKind::SearchKeyword => {
            apply_to_disabled_list(&mut config.disabled_builtin_actions, &op.binding_id, op.op)
        }
        BindingKind::ContextBinding => {
            apply_to_disabled_list(&mut config.disabled_context_bindings, &op.binding_id, op.op)
        }
        BindingKind::ChordKey => {
            // chord binding_id 格式为 "chord.{chord_id}"，提取实际 chord_id
            let chord_id = op
                .binding_id
                .strip_prefix("chord.")
                .unwrap_or(&op.binding_id);
            apply_to_disabled_list(&mut config.disabled_chord_actions, chord_id, op.op)
        }
    }
    .map_err(|e| e.to_string())
    .into_result(&op.binding_id)
}

/// 从 disabled 列表中添加或移除 id。
///
/// - `Enable` → 从 disabled 列表移除（如果存在）
/// - `Disable` → 添加到 disabled 列表（如果不存在）
fn apply_to_disabled_list(
    disabled: &mut Vec<String>,
    id: &str,
    op: BindingOpKind,
) -> Result<(), String> {
    match op {
        BindingOpKind::Enable => {
            disabled.retain(|d| d != id);
            tracing::debug!(id, "binding enabled (removed from disabled list)");
        }
        BindingOpKind::Disable => {
            if !disabled.iter().any(|d| d == id) {
                disabled.push(id.to_string());
                disabled.sort();
                disabled.dedup();
                tracing::debug!(id, "binding disabled (added to disabled list)");
            }
        }
    }
    Ok(())
}

/// 便捷 trait：把 Result 转成 ApplyBindingResult。
trait IntoResult {
    fn into_result(self, binding_id: &str) -> ApplyBindingResult;
}

impl IntoResult for Result<(), String> {
    fn into_result(self, binding_id: &str) -> ApplyBindingResult {
        match self {
            Ok(()) => ApplyBindingResult {
                binding_id: binding_id.to_string(),
                success: true,
                error: None,
            },
            Err(e) => ApplyBindingResult {
                binding_id: binding_id.to_string(),
                success: false,
                error: Some(e),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AppConfig;

    #[tokio::test]
    async fn concurrent_batch_updates_merge_without_rewriting_appearance() {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::query("CREATE TABLE config (key TEXT PRIMARY KEY, value TEXT NOT NULL, updated_at INTEGER NOT NULL DEFAULT 0)")
            .execute(&pool).await.unwrap();
        crate::config::save_config(&pool, &AppConfig::default())
            .await
            .unwrap();
        let first = [BindingOp {
            op: BindingOpKind::Disable,
            kind: BindingKind::ChordKey,
            binding_id: "chord.chat".into(),
        }];
        let second = [BindingOp {
            op: BindingOpKind::Disable,
            kind: BindingKind::ChordKey,
            binding_id: "chord.screenshot".into(),
        }];
        let (first, second, appearance) = tokio::join!(
            apply_binding_batch(&pool, &first),
            apply_binding_batch(&pool, &second),
            crate::config::update_auto_start(&pool, true),
        );
        assert!(first[0].success);
        assert!(second[0].success);
        appearance.unwrap();
        let config = crate::config::get_config(&pool).await;
        assert!(config.auto_start);
        assert_eq!(config.disabled_chord_actions, vec!["chat", "screenshot"]);
    }

    #[test]
    fn enable_removes_from_disabled() {
        let mut config = AppConfig {
            disabled_builtin_actions: vec!["open_settings".into(), "lock".into()],
            ..Default::default()
        };

        let result = apply_single_op(
            &mut config,
            &BindingOp {
                op: BindingOpKind::Enable,
                kind: BindingKind::SearchKeyword,
                binding_id: "open_settings".into(),
            },
        );

        assert!(result.success);
        assert!(
            !config
                .disabled_builtin_actions
                .contains(&"open_settings".to_string())
        );
        assert!(
            config
                .disabled_builtin_actions
                .contains(&"lock".to_string())
        ); // 其他不受影响
    }

    #[test]
    fn disable_adds_to_disabled() {
        let mut config = AppConfig {
            disabled_builtin_actions: vec![],
            ..Default::default()
        };

        let result = apply_single_op(
            &mut config,
            &BindingOp {
                op: BindingOpKind::Disable,
                kind: BindingKind::SearchKeyword,
                binding_id: "open_settings".into(),
            },
        );

        assert!(result.success);
        assert!(
            config
                .disabled_builtin_actions
                .contains(&"open_settings".to_string())
        );
    }

    #[test]
    fn disable_idempotent() {
        let mut config = AppConfig {
            disabled_builtin_actions: vec!["open_settings".into()],
            ..Default::default()
        };

        let result = apply_single_op(
            &mut config,
            &BindingOp {
                op: BindingOpKind::Disable,
                kind: BindingKind::SearchKeyword,
                binding_id: "open_settings".into(),
            },
        );

        assert!(result.success);
        assert_eq!(config.disabled_builtin_actions.len(), 1); // 不重复添加
    }

    #[test]
    fn enable_idempotent() {
        let mut config = AppConfig {
            disabled_builtin_actions: vec![], // 本来就没有
            ..Default::default()
        };

        let result = apply_single_op(
            &mut config,
            &BindingOp {
                op: BindingOpKind::Enable,
                kind: BindingKind::SearchKeyword,
                binding_id: "open_settings".into(),
            },
        );

        assert!(result.success);
        assert!(config.disabled_builtin_actions.is_empty());
    }

    #[test]
    fn chord_key_strips_prefix() {
        let mut config = AppConfig {
            disabled_chord_actions: vec![],
            ..Default::default()
        };

        let result = apply_single_op(
            &mut config,
            &BindingOp {
                op: BindingOpKind::Disable,
                kind: BindingKind::ChordKey,
                binding_id: "chord.screenshot".into(),
            },
        );

        assert!(result.success);
        assert!(
            config
                .disabled_chord_actions
                .contains(&"screenshot".to_string())
        );
    }

    #[test]
    fn context_binding_uses_correct_list() {
        let mut config = AppConfig {
            disabled_context_bindings: vec![],
            ..Default::default()
        };

        let result = apply_single_op(
            &mut config,
            &BindingOp {
                op: BindingOpKind::Disable,
                kind: BindingKind::ContextBinding,
                binding_id: "builtin.open_url::clipboard_is_url".into(),
            },
        );

        assert!(result.success);
        assert!(
            config
                .disabled_context_bindings
                .contains(&"builtin.open_url::clipboard_is_url".to_string())
        );
        // 不影响其他列表
        assert!(config.disabled_builtin_actions.is_empty());
        assert!(config.disabled_chord_actions.is_empty());
    }

    #[test]
    fn batch_operations_mixed() {
        let mut config = AppConfig {
            disabled_builtin_actions: vec!["lock".into()],
            ..Default::default()
        };

        // 禁用 open_settings + 启用 lock + 禁用 chord.screenshot
        let ops = vec![
            BindingOp {
                op: BindingOpKind::Disable,
                kind: BindingKind::SearchKeyword,
                binding_id: "open_settings".into(),
            },
            BindingOp {
                op: BindingOpKind::Enable,
                kind: BindingKind::SearchKeyword,
                binding_id: "lock".into(),
            },
            BindingOp {
                op: BindingOpKind::Disable,
                kind: BindingKind::ChordKey,
                binding_id: "chord.screenshot".into(),
            },
        ];

        for op in &ops {
            apply_single_op(&mut config, op);
        }

        assert!(
            config
                .disabled_builtin_actions
                .contains(&"open_settings".to_string())
        );
        assert!(
            !config
                .disabled_builtin_actions
                .contains(&"lock".to_string())
        );
        assert!(
            config
                .disabled_chord_actions
                .contains(&"screenshot".to_string())
        );
    }
}
