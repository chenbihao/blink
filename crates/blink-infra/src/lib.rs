//! 基础设施层：数据持久化、平台 API、通用工具

pub mod data;
pub mod event_names; // 0.21.14：事件名常量（从 domain 下沉，消除 infra→domain 反向依赖）
pub mod local_engine; // 0.22.1：ManagedProcess 受管子进程生命周期与日志管道
pub mod platform;
pub mod script_interpreter; // 0.25.20：脚本插件解释器托管分发（锁文件+下载事务+resolver）
pub mod stt; // 0.22.9 Handoff 06：VAD frontend port 实现（EnergyVad adapter + FSMN-VAD ONNX）
pub mod utils;

// ── 运行时模式开关（0.25.1 crate 化引入）──────────────────────────────────

/// 生产模式运行时开关。
///
/// crate 化前，infra 用 `#[cfg(test)]` 在测试编译期关闭 DB 缓存、把引擎目录
/// 指到临时路径——但依赖 crate 永远以非 test 配置编译，bin 测试进程里跑的是
/// 生产分支：全局 `CONFIG_CACHE` 串台（11 个配置测试 flaky）、引擎路径指向
/// 真实 `%APPDATA%`。改为运行时开关后：
/// - **默认隔离模式**（缓存关闭 + 临时目录）：一切测试进程天然隔离；
/// - 生产入口（`main`）显式调用 [`activate_production`] 激活。
pub mod runtime_mode {
    use std::sync::atomic::{AtomicBool, Ordering};

    static PRODUCTION: AtomicBool = AtomicBool::new(false);

    /// 生产入口调用（main 顶部）：启用 DB 读缓存 + 真实数据目录。
    pub fn activate_production() {
        PRODUCTION.store(true, Ordering::Release);
    }

    /// 是否运行在生产模式（未激活 = 测试/隔离模式）。
    pub fn is_production() -> bool {
        PRODUCTION.load(Ordering::Acquire)
    }
}
