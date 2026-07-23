//! Action 注册表（0.8.6 §8.1.1）。
//!
//! `ActionRegistry` 持有所有内置动作的 `Arc<dyn Action>` 实例，
//! 按 id 查找。`run_builtin_action` command 通过此注册表分派。
//!
//! 0.9.3:内部改 `RwLock`，`register()` 变 `&self`，支持启动后动态注册插件 tool。

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use super::Action;
use super::builtin::*;

/// 动作注册表。id → `Arc<dyn Action>` 的映射。
///
/// 内部 `RwLock` 允许 `register(&self)` —— 启动时注册插件 tool 不需要 `&mut`。
/// 读多写少（注册只在启动时发生），`RwLock` 读可并行。
pub struct ActionRegistry {
    actions: RwLock<HashMap<String, Arc<dyn Action>>>,
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

        ActionRegistry {
            actions: RwLock::new(actions),
        }
    }

    /// 按 id 查找动作（clone Arc，锁内完成）。
    pub fn get(&self, id: &str) -> Option<Arc<dyn Action>> {
        self.actions.read().unwrap().get(id).cloned()
    }

    /// 列出所有已注册的动作 id。
    pub fn ids(&self) -> Vec<String> {
        self.actions.read().unwrap().keys().cloned().collect()
    }

    /// 注册一个自定义动作（0.9.3 插件 tool 注册）。
    ///
    /// `&self` 而非 `&mut self` —— 内部 `RwLock` 允许启动后动态注册。
    /// 若 id 已存在，warn + 跳过（不覆盖 builtin）。
    pub fn register(&self, action: Arc<dyn Action>) {
        let id = action.id().to_string();
        let mut actions = self.actions.write().unwrap();
        if actions.contains_key(&id) {
            tracing::warn!(id = %id, "ActionRegistry::register: id 已存在,跳过");
            return;
        }
        actions.insert(id, action);
    }

    /// 已注册动作数量。
    pub fn len(&self) -> usize {
        self.actions.read().unwrap().len()
    }

    /// 返回所有已注册动作的 `(id, Arc<dyn Action>)` 对（0.12.0 §2.4）。
    ///
    /// 供 `build_agent_tools()` 工厂函数遍历所有动作，包装成 `ToolDyn`。
    /// 读锁内一次性 clone 所有 Arc，避免多次锁获取。
    pub fn entries(&self) -> Vec<(String, Arc<dyn Action>)> {
        self.actions
            .read()
            .unwrap()
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
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
        assert_eq!(reg.len(), 12);
    }

    #[test]
    fn registry_contains_expected_ids() {
        let reg = ActionRegistry::new();
        let expected = [
            "open_settings",
            "lock",
            "shutdown",
            "restart",
            "sleep",
            "clear_history",
            "exit_blink",
            "open_logs",
            "open_data_dir",
            "open_url",
            "open_path",
            "reveal_in_explorer",
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
            "open_settings",
            "lock",
            "shutdown",
            "restart",
            "sleep",
            "clear_history",
            "exit_blink",
            "open_logs",
            "open_data_dir",
            "open_url",
            "open_path",
            "reveal_in_explorer",
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
            "open_settings",
            "lock",
            "shutdown",
            "restart",
            "sleep",
            "clear_history",
            "exit_blink",
            "open_logs",
            "open_data_dir",
            "open_url",
            "open_path",
            "reveal_in_explorer",
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
        for id in [
            "open_settings",
            "open_logs",
            "open_data_dir",
            "open_url",
            "open_path",
            "reveal_in_explorer",
        ] {
            let action = reg.get(id).expect(id);
            assert_eq!(action.danger_class(), DangerClass::Safe, "{id} 应为 Safe");
        }

        // Dangerous:系统级不可逆 / 数据不可逆 / 让用户失去 Blink
        for id in [
            "lock",
            "shutdown",
            "restart",
            "sleep",
            "clear_history",
            "exit_blink",
        ] {
            let action = reg.get(id).expect(id);
            assert_eq!(
                action.danger_class(),
                DangerClass::Dangerous,
                "{id} 应为 Dangerous"
            );
        }
    }

    /// 0.12.0 §2.4: ai_eligible 粒度控制--exit_blink 不暴露给 AI，其余默认 true。
    #[test]
    fn ai_eligible_excludes_self_destruct_actions() {
        let reg = ActionRegistry::new();
        // exit_blink 覆写为 false（AI 不该让 Blink 自杀）
        assert!(
            !reg.get("exit_blink").unwrap().ai_eligible(),
            "exit_blink 不该暴露给 AI"
        );
        // 其余动作默认 true（含 Dangerous 的 shutdown/lock 等--有确认卡片挡）
        for id in [
            "open_settings",
            "open_url",
            "shutdown",
            "lock",
            "clear_history",
        ] {
            assert!(reg.get(id).unwrap().ai_eligible(), "{id} 默认应暴露给 AI");
        }
    }

    /// 0.9.3:register(&self) 支持启动后动态注册。
    #[test]
    fn register_with_ref_self_works() {
        let reg = ActionRegistry::new();
        assert_eq!(reg.len(), 12);
        // 注册一个新动作
        reg.register(Arc::new(OpenSettingsAction)); // 重复 id,应跳过
        assert_eq!(reg.len(), 12, "重复 id 不应增加数量");
    }
}
