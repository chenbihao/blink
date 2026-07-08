//! Action 注册表（0.8.6 §8.1.1）。
//!
//! `ActionRegistry` 持有所有内置动作的 `Arc<dyn Action>` 实例，
//! 按 id 查找。`run_builtin_action` command 通过此注册表分派。

use std::collections::HashMap;
use std::sync::Arc;

use super::builtin::*;
use super::Action;

/// 动作注册表。id → `Arc<dyn Action>` 的映射。
pub struct ActionRegistry {
    actions: HashMap<String, Arc<dyn Action>>,
}

impl ActionRegistry {
    /// 构建默认注册表（12 个内置动作）。
    pub fn new() -> Self {
        let mut actions: HashMap<String, Arc<dyn Action>> = HashMap::new();

        // 无参动作
        let builtins: Vec<Arc<dyn Action>> = vec![
            Arc::new(OpenSettingsAction),
            Arc::new(LockWorkstationAction),
            Arc::new(ShutdownAction),
            Arc::new(RestartAction),
            Arc::new(SleepAction),
            Arc::new(ClearHistoryAction),
            Arc::new(ExitBlinkAction),
            Arc::new(OpenLogsAction),
            Arc::new(OpenDataDirAction),
            // 参数化动作（0.8.0 §1.3）
            Arc::new(OpenUrlAction),
            Arc::new(OpenPathAction),
            Arc::new(RevealInExplorerAction),
        ];

        for action in builtins {
            actions.insert(action.id().to_string(), action);
        }

        ActionRegistry { actions }
    }

    /// 按 id 查找动作。
    pub fn get(&self, id: &str) -> Option<&Arc<dyn Action>> {
        self.actions.get(id)
    }

    /// 列出所有已注册的动作 id。
    #[allow(dead_code)]
    pub fn ids(&self) -> impl Iterator<Item = &str> {
        self.actions.keys().map(|s| s.as_str())
    }

    /// 注册一个自定义动作（0.9 AI Provider 用）。
    #[allow(dead_code)]
    pub fn register(&mut self, action: Arc<dyn Action>) {
        self.actions.insert(action.id().to_string(), action);
    }
}

impl Default for ActionRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_has_12_builtin_actions() {
        let reg = ActionRegistry::new();
        assert_eq!(reg.ids().count(), 12);
    }

    #[test]
    fn registry_contains_expected_ids() {
        let reg = ActionRegistry::new();
        let expected = [
            "open_settings", "lock", "shutdown", "restart", "sleep",
            "clear_history", "exit_blink", "open_logs", "open_data_dir",
            "open_url", "open_path", "reveal_in_explorer",
        ];
        for id in &expected {
            assert!(reg.get(id).is_some(), "缺少动作: {id}");
        }
    }

    #[test]
    fn registry_unknown_id_returns_none() {
        let reg = ActionRegistry::new();
        assert!(reg.get("nonexistent").is_none());
    }

    #[test]
    fn action_ids_match_original_kind() {
        // 验证每个 Action::id() 与原 BuiltinActionKind 的 action_id 一致
        let reg = ActionRegistry::new();
        let expected_ids = [
            "open_settings", "lock", "shutdown", "restart", "sleep",
            "clear_history", "exit_blink", "open_logs", "open_data_dir",
            "open_url", "open_path", "reveal_in_explorer",
        ];
        for id in &expected_ids {
            let action = reg.get(id).expect(id);
            assert_eq!(action.id(), *id, "Action::id() 与注册表 key 不一致");
        }
    }

    /// 0.9.0 §3.3 铁则:12 个 builtin 全部显式覆盖 `schema()`——
    /// name 与 id 一致 + description 非空。若有人新增 builtin 忘了写 schema
    /// (走了 default impl 的空 description),此测会 fail。
    #[test]
    fn all_builtins_have_explicit_schema() {
        let reg = ActionRegistry::new();
        for id in [
            "open_settings", "lock", "shutdown", "restart", "sleep",
            "clear_history", "exit_blink", "open_logs", "open_data_dir",
            "open_url", "open_path", "reveal_in_explorer",
        ] {
            let action = reg.get(id).expect(id);
            let schema = action.schema();
            assert_eq!(schema.name, id, "{id}: schema.name 与 id 不一致");
            assert!(
                !schema.description.is_empty(),
                "{id}: schema.description 为空——走了 default impl,请显式覆盖(0.9.0 §3.3 铁则)"
            );
        }
    }

    /// 0.9.0 §5.4 白名单铁则:分类正确性——参数化 3 个是 Safe,
    /// 系统级不可逆 6 个(lock/shutdown/restart/sleep/clear_history/exit_blink)是 Dangerous。
    #[test]
    fn danger_class_matches_expected_partition() {
        use crate::domain::execution::DangerClass;
        let reg = ActionRegistry::new();

        // Safe:只读打开 UI + 参数化动作(参数走 UserExplicit 类型墙)
        for id in ["open_settings", "open_logs", "open_data_dir", "open_url", "open_path", "reveal_in_explorer"] {
            let action = reg.get(id).expect(id);
            assert_eq!(action.danger_class(), DangerClass::Safe, "{id} 应为 Safe");
        }

        // Dangerous:系统级不可逆 / 数据不可逆 / 让用户失去 Blink
        for id in ["lock", "shutdown", "restart", "sleep", "clear_history", "exit_blink"] {
            let action = reg.get(id).expect(id);
            assert_eq!(action.danger_class(), DangerClass::Dangerous, "{id} 应为 Dangerous");
        }
    }
}
