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
    /// 构建默认注册表（11 个内置动作）。
    ///
    /// 0.14.4: open_url / open_path / reveal_in_explorer 的 Action 版本已删除，
    /// 由 Capability 版本承担（CapabilityRegistry）。
    /// 0.19.7: 新增 EditClipboardImageAction，共 11 个。
    /// 0.19.17: 新增 BlinkPrintDebugInfoAction 和 BlinkDebugInitHookAction，共 13 个。
    pub fn new() -> Self {
        let mut actions: HashMap<String, Arc<dyn Action>> = HashMap::new();

        // 无参动作
        let builtins: Vec<Arc<dyn Action>> = vec![
            Arc::new(OpenSettingsAction),
            Arc::new(ShowStickyManagerAction),
            Arc::new(EditClipboardImageAction),
            Arc::new(LockWorkstationAction),
            Arc::new(ShutdownAction),
            Arc::new(RestartAction),
            Arc::new(SleepAction),
            Arc::new(ClearHistoryAction),
            Arc::new(ExitBlinkAction),
            Arc::new(OpenLogsAction),
            Arc::new(OpenDataDirAction),
            Arc::new(BlinkPrintDebugInfoAction),
            Arc::new(BlinkDebugInitHookAction),
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

    /// 注册一个自定义动作（0.9.3 插件 tool 注册）。
    ///
    /// `&self` 而非 `&mut self` —— 内部 `RwLock` 允许启动后动态注册。
    /// 若 id 已存在，warn + 跳过（不覆盖 builtin）。
    ///
    /// **0.13.7**：插件 tool 迁入 CapabilityRegistry 后，本方法暂无调用者，
    /// 保留作为 ActionRegistry 的对称接口（与 CapabilityRegistry::register 一致），
    /// 未来运行期动态注册 Action 时消费。
    #[allow(dead_code)]
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
    #[allow(dead_code)] // 0.14.2: build_agent_tools 不再消费 ActionRegistry；保留供测试/调试
    pub fn len(&self) -> usize {
        self.actions.read().unwrap().len()
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
    fn registry_has_13_builtin_actions() {
        let reg = ActionRegistry::new();
        assert_eq!(reg.len(), 13);
    }

    #[test]
    fn registry_contains_expected_ids() {
        let reg = ActionRegistry::new();
        let expected = [
            "open_settings",
            "sticky_manager",
            "edit_clipboard_image",
            "lock",
            "shutdown",
            "restart",
            "sleep",
            "clear_history",
            "exit_blink",
            "open_logs",
            "open_data_dir",
            "blink_print_debug_info",
            "blink_debug_inithook",
        ];
        for id in &expected {
            assert!(reg.get(id).is_some(), "缺少动作: {id}");
        }
        // 0.14.4: open_url / open_path / reveal_in_explorer 已从 ActionRegistry 删除
        for id in ["open_url", "open_path", "reveal_in_explorer"] {
            assert!(reg.get(id).is_none(), "{id} 应已从 ActionRegistry 删除");
        }
    }

    #[test]
    fn registry_unknown_id_returns_none() {
        let reg = ActionRegistry::new();
        assert!(reg.get("nonexistent").is_none());
    }

    #[test]
    fn action_ids_match_registry_keys() {
        // 验证每个 Action::id() 与注册表 key 一致
        let reg = ActionRegistry::new();
        let expected_ids = [
            "open_settings",
            "sticky_manager",
            "edit_clipboard_image",
            "lock",
            "shutdown",
            "restart",
            "sleep",
            "clear_history",
            "exit_blink",
            "open_logs",
            "open_data_dir",
            "blink_print_debug_info",
            "blink_debug_inithook",
        ];
        for id in &expected_ids {
            let action = reg.get(id).expect(id);
            assert_eq!(action.id(), *id, "Action::id() 与注册表 key 不一致");
        }
    }

    /// 0.9.0 §3.3 铁则：所有 builtin 显式覆盖 `schema()`——
    /// name 与 id 一致 + description 非空。若有人新增 builtin 忘了写 schema
    /// (走了 default impl 的空 description),此测会 fail。
    #[test]
    fn all_builtins_have_explicit_schema() {
        let reg = ActionRegistry::new();
        for id in [
            "open_settings",
            "sticky_manager",
            "edit_clipboard_image",
            "lock",
            "shutdown",
            "restart",
            "sleep",
            "clear_history",
            "exit_blink",
            "open_logs",
            "open_data_dir",
            "blink_print_debug_info",
            "blink_debug_inithook",
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

        // Safe:只读打开 UI / 诊断信息
        for id in [
            "open_settings",
            "sticky_manager",
            "edit_clipboard_image",
            "open_logs",
            "open_data_dir",
            "blink_print_debug_info",
            "blink_debug_inithook",
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

    #[test]
    fn register_with_ref_self_works() {
        let reg = ActionRegistry::new();
        assert_eq!(reg.len(), 13);
        // 注册一个新动作
        reg.register(Arc::new(OpenSettingsAction)); // 重复 id,应跳过
        assert_eq!(reg.len(), 13, "重复 id 不应增加数量");
    }
}
