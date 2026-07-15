//! 服务骨架(0.2.1):统一生命周期 + 显式依赖。
//!
//! 设计(见 production-design/phases/0.2-core-plugin-design.md §1):
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

use crate::app::ai_config::AIConfig;
use crate::app::config::AppConfig;
use crate::app::voice::VoiceService;
use crate::domain::ai::AIProviderRegistry;

/// 服务启动期的共享依赖容器（0.8.6 §8.2.3 扩展为真依赖容器）。
///
/// 持有所有核心服务的 `Arc` 引用——Service 启动时按需取用，
/// 不再散落在 `main.rs` 的 `app.manage()` 调用中。
pub struct AppContext {
    pub app: AppHandle,
    #[allow(dead_code)]
    pub pool: SqlitePool,
    pub config: AppConfig,
    /// 0.9.1 Phase 3:AI 配置分片(独立第 7 分片,不进 AppConfig 门面)。
    /// 默认 `enabled=false` —— 老用户零副作用。
    #[allow(dead_code)] // 0.9.1 Phase 5-6 起 AI dispatch 消费
    pub ai_config: AIConfig,
    // ── 0.8.6 §8.2.3：核心服务引用 ─────────────────────────
    pub search_service: std::sync::Arc<crate::domain::search::SearchService>,
    #[allow(dead_code)] // 0.9 插件查询时消费
    pub plugin_engine: std::sync::Arc<crate::domain::plugin::PluginEngine>,
    #[allow(dead_code)] // Service 启动时按需取用
    pub router: std::sync::Arc<crate::domain::intent::RuleRouter>,
    #[allow(dead_code)]
    pub chord_registry: std::sync::Arc<crate::domain::chord::ChordRegistry>,
    #[allow(dead_code)]
    pub action_registry: std::sync::Arc<crate::domain::execution::ActionRegistry>,
    /// 0.9.7 Capability 能力协议层（inventory 自动收集）。
    /// Step 4 起 `build_aggregated_tools` 消费，AI tool_call 命中 Capability。
    /// **运行时通过 `app.state::<Arc<CapabilityRegistry>>()` 访问**，AppContext 仅服务于 setup 期。
    #[allow(dead_code)]
    pub capability_registry: std::sync::Arc<crate::domain::capability::CapabilityRegistry>,
    /// 0.9.1 Phase 5a:AI Provider 池 + 三档 dispatch。
    /// **未配置或全 factory 失败 → 空池**,`resolve` 一律返 `NotConfigured`;
    /// SearchService 拿到 NotConfigured → fallback 常规 fuzzy(§6.4 兜底铁则)。
    #[allow(dead_code)] // 0.9.2 起 SearchService::exec_ai_intent 消费
    pub ai_registry: std::sync::Arc<AIProviderRegistry>,
    /// 0.10 语音服务(hold-to-talk 管线编排)。
    /// HotkeyService 在 Hold/HoldRelease 事件中调用。
    pub voice_service: std::sync::Arc<VoiceService>,
}

/// 统一生命周期接口。0.2.1 各服务的 `start` 多为同步初始化,但 trait 用 `async-trait`
/// 定义,为 0.2.2 `SearchEngine::search`(异步)及 0.3 插件进程交互铺路。
#[async_trait::async_trait]
pub trait Service: Send + Sync {
    /// 服务名(日志 / 诊断用)。
    fn name(&self) -> &'static str;

    /// 显式声明依赖的其他服务名（0.8.6 §8.2.3）。
    /// 启动器按拓扑顺序调用——被依赖的服务先 start。
    /// 默认无依赖。
    #[allow(dead_code)] // 拓扑排序启动器预留（当前按注册顺序启动）
    fn deps(&self) -> &'static [&'static str] {
        &[]
    }

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
/// 0.8.6 §8.2.3：从 AppContext 取 SearchService 引用，不再单独持有。
pub struct SearchLifecycle;

#[async_trait::async_trait]
impl Service for SearchLifecycle {
    fn name(&self) -> &'static str {
        "search"
    }
    fn deps(&self) -> &'static [&'static str] {
        &["config", "history"] // 依赖配置和历史服务先初始化
    }
    async fn start(&self, ctx: &AppContext) -> Result<(), String> {
        ctx.search_service.start();
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
        crate::infra::platform::window::start_watchdog(ctx.app.clone());
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
        let voice_service = ctx.voice_service.clone();
        let mut rx = crate::infra::platform::hotkey::start(
            ctx.config.hotkey.clone(),
            ctx.config.tap_threshold,
        );
        tauri::async_runtime::spawn(async move {
            while let Some(ev) = rx.recv().await {
                match ev {
                    crate::infra::platform::hotkey::HotkeyEvent::Tap(trigger_time) => {
                        // toggle:已可见则隐藏(仅快捷键;单实例重复运行仍走 invoke 总是显示)
                        if crate::infra::platform::window::is_visible() {
                            crate::infra::platform::window::hide(&app, "toggle");
                        } else {
                            let elapsed = trigger_time.elapsed().as_secs_f64() * 1000.0;
                            crate::infra::platform::window::invoke(&app);
                            // 记录热键唤起耗时（按键 → 窗口 invoke）
                            crate::infra::utils::perf::record(
                                crate::infra::utils::perf::MetricCategory::Hotkey,
                                "key_to_show",
                                elapsed,
                                None,
                            );
                        }
                    }
                    crate::infra::platform::hotkey::HotkeyEvent::Hold(_) => {
                        // 长按开始 → 语音录音开始
                        voice_service.start_recording();
                    }
                    crate::infra::platform::hotkey::HotkeyEvent::HoldRelease(_) => {
                        // 长按结束 → 停止录音 → STT → 注入/fill-query
                        voice_service.stop_recording().await;
                    }
                    crate::infra::platform::hotkey::HotkeyEvent::VoiceCancel(_) => {
                        // ESC 取消录音
                        voice_service.cancel_recording();
                    }
                }
            }
        });
        Ok(())
    }
}

/// 选区感知服务(0.8.0 §1.1)：启动划词监听(全局鼠标钩子 → 黄金时机 UIA 抓取 → 缓存)。
/// 唤起时由 window::invoke 读取缓存,绕开 Electron 应用失焦退化选区的问题。
pub struct SelectionService;

#[async_trait::async_trait]
impl Service for SelectionService {
    fn name(&self) -> &'static str {
        "selection"
    }
    fn deps(&self) -> &'static [&'static str] {
        &["config"] // 依赖 ContextConfig
    }
    async fn start(&self, ctx: &AppContext) -> Result<(), String> {
        // 依 ContextConfig.selection_enabled 决定是否启用划词监听。
        // 用户可在设置-上下文-环境感知里热切换（见 commands::update_context_config）。
        // 局限：钩子一旦装上，关闭态只是让回调短路（不再抓取），不会真正卸钩子
        //      —— 低级鼠标钩子跨线程卸载不安全，且实测再启用比反复装卸更稳。
        let cfg = crate::app::config::get_context_config(&ctx.pool).await;
        // 敏感应用黑名单：灌初始值，让钩子回调启动即门控。热更新走 commands::update_context_config。
        crate::infra::platform::selection::set_sensitive_apps(cfg.sensitive_apps.clone());
        if cfg.selection_enabled {
            crate::infra::platform::selection::start_listener();
        }
        Ok(())
    }
}

/// 按依赖拓扑顺序构造服务列表(Config → History → 其余)。
///
/// 剪贴板历史监听服务（0.8.5）：启动 AddClipboardFormatListener 监听写入。
/// 监听器自包含（infra/platform/clipboard），不耦合 domain/commands——只依赖 data 层存。
pub struct ClipboardService;

#[async_trait::async_trait]
impl Service for ClipboardService {
    fn name(&self) -> &'static str {
        "clipboard"
    }
    async fn start(&self, ctx: &AppContext) -> Result<(), String> {
        let cfg = ctx.config.clipboard.clone();
        // 0.8.5.1 §6.6：改回尊重 cfg.enabled——设置页新增 Chord tab 后用户有明确开关入口,
        // 不再"内存覆盖 true"绕过老用户 db 里的 false（那是 0.7 遗留默认值兜底策略）。
        // ClipboardConfig::default().enabled = true（新用户 opt-in）,老用户如已 false 会尊重之。
        if !cfg.enabled {
            tracing::info!("剪贴板监听器: cfg.enabled=false, 跳过启动");
            return Ok(());
        }
        crate::infra::platform::clipboard::start_listener(ctx.pool.clone(), cfg);
        Ok(())
    }
}

/// 按依赖拓扑顺序构造服务列表。
///
/// 0.8.6 §8.2.3：SearchService 从 AppContext 取，不再作为参数传入。
pub fn all_services() -> Vec<Box<dyn Service>> {
    vec![
        Box::new(ConfigService),
        Box::new(HistoryService),
        Box::new(SearchLifecycle),
        Box::new(WindowService),
        Box::new(HotkeyService),
        Box::new(SelectionService),
        Box::new(ClipboardService),
    ]
}
