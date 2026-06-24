//! 服务骨架(0.2.1):统一生命周期 + 显式依赖。
//!
//! 设计(见 production/0.2-core-plugin-design.md §1):
//! - `Service` 只是「有统一 start/stop 生命周期 + 显式依赖」的模块,**不是** actor、
//!   不跑在独立调度器上,也不引入 DI 容器 / 全局 Event Bus(§1.6)。
//! - 0.2.1 走「薄包装」:Service 是空壳,`start` 里调用现有模块函数,内部逻辑零改动。
//!   全局静态(尤其 Win32 hook 回调必须访问的)保留,Service 只做生命周期入口。
//! - 现有模块包成 5 个 Service;`SearchService`/`SearchEngine`(0.2.2)与插件/意图
//!   服务(0.3)本版不建——无可包装的现有逻辑,留到对应版本。
//!
//! `AppContext` 是共享依赖容器(§1.2),**不是** DI 容器:只把 setup 阶段散落的启动
//! 依赖收拢显式化。它**不替换** Tauri 的 `app.manage` / `app.state`——command 层继续
//! 用 `app.state::<SqlitePool>()`,AppContext 仅服务于 setup 期的 Service 编排。

use sqlx::SqlitePool;
use tauri::AppHandle;

use crate::config::AppConfig;

/// 服务启动期的共享依赖容器。
pub struct AppContext {
    pub app: AppHandle,
    /// 数据库连接池。0.2.1 各 Service 暂未直接用(command 层走 `app.state`),
    /// 预留给 0.2.2 起需在 Service 内访问 DB 的场景(如 SearchService 读历史权重)。
    #[allow(dead_code)]
    pub pool: SqlitePool,
    /// 启动时的配置快照(各 Service 据此初始化运行时状态)。
    pub config: AppConfig,
}

/// 统一生命周期接口。0.2.1 各服务的 `start` 多为同步初始化,但 trait 用 `async-trait`
/// 定义,为 0.2.2 `SearchEngine::search`(异步)及 0.3 插件进程交互铺路。
#[async_trait::async_trait]
pub trait Service: Send + Sync {
    /// 服务名(日志 / 诊断用)。
    fn name(&self) -> &'static str;

    /// 启动:注册后台任务、初始化运行时状态。按依赖拓扑顺序调用。
    async fn start(&self, ctx: &AppContext) -> Result<(), String>;

    /// 停止:应用退出时逆序调用。多数服务随进程退出即可,默认空实现。
    /// (0.3 插件进程需在此 kill 子进程。)
    #[allow(dead_code)]
    async fn stop(&self) {}
}

// ── 各服务(薄包装,内部逻辑复用现有模块函数) ──────────────────────────────────

/// 配置服务:配置已在 `AppContext::config` 快照中,本版无启动副作用。
/// 占位以统一生命周期,未来承载配置热更新 / 订阅入口。
pub struct ConfigService;

#[async_trait::async_trait]
impl Service for ConfigService {
    fn name(&self) -> &'static str {
        "config"
    }
    async fn start(&self, _ctx: &AppContext) -> Result<(), String> {
        Ok(())
    }
}

/// 历史服务:SqlitePool 已由 main.rs `app.manage` 持有,本版无启动副作用。
/// 占位以统一生命周期。
pub struct HistoryService;

#[async_trait::async_trait]
impl Service for HistoryService {
    fn name(&self) -> &'static str {
        "history"
    }
    async fn start(&self, _ctx: &AppContext) -> Result<(), String> {
        Ok(())
    }
}

/// 搜索生命周期服务:启动 SearchService 持有的各引擎后台任务(如开始菜单预扫)。
///
/// SearchService 本身由 main.rs 构造并 `app.manage(Arc<SearchService>)`(command 层经
/// state 取用);此 wrapper 只负责在 Service 框架内统一触发其 `start()`。
pub struct SearchLifecycle {
    service: std::sync::Arc<crate::search::SearchService>,
}

#[async_trait::async_trait]
impl Service for SearchLifecycle {
    fn name(&self) -> &'static str {
        "search"
    }
    async fn start(&self, _ctx: &AppContext) -> Result<(), String> {
        self.service.start();
        Ok(())
    }
}

/// 窗口服务:启动失焦隐藏看门狗。
pub struct WindowService;

#[async_trait::async_trait]
impl Service for WindowService {
    fn name(&self) -> &'static str {
        "window"
    }
    async fn start(&self, ctx: &AppContext) -> Result<(), String> {
        crate::window::start_watchdog(ctx.app.clone());
        Ok(())
    }
}

/// 热键服务:注册全局热键 + 启动事件循环(tap → toggle 窗口显隐)。
pub struct HotkeyService;

#[async_trait::async_trait]
impl Service for HotkeyService {
    fn name(&self) -> &'static str {
        "hotkey"
    }
    async fn start(&self, ctx: &AppContext) -> Result<(), String> {
        let app = ctx.app.clone();
        let mut rx = crate::hotkey::start(ctx.config.hotkey.clone(), ctx.config.tap_threshold);
        tauri::async_runtime::spawn(async move {
            while let Some(ev) = rx.recv().await {
                match ev {
                    crate::hotkey::HotkeyEvent::Tap(_) => {
                        // toggle:已可见则隐藏(仅快捷键;单实例重复运行仍走 invoke 总是显示)
                        if crate::window::is_visible() {
                            crate::window::hide(&app, "toggle");
                        } else {
                            crate::window::invoke(&app);
                        }
                    }
                }
            }
        });
        Ok(())
    }
}

/// 按依赖拓扑顺序构造服务列表(Config → History → 其余)。
///
/// `search` 为已构造的 SearchService(main.rs 持有并 manage),此处包成生命周期 wrapper
/// 统一启动其引擎后台任务。
pub fn all_services(search: std::sync::Arc<crate::search::SearchService>) -> Vec<Box<dyn Service>> {
    vec![
        Box::new(ConfigService),
        Box::new(HistoryService),
        Box::new(SearchLifecycle { service: search }),
        Box::new(WindowService),
        Box::new(HotkeyService),
    ]
}
