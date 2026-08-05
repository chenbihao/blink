//! 服务骨架(0.2.1):统一生命周期 + 显式依赖。
//!
//! 设计(见 phases/0.2-core-plugin-design.md §1):
//! - `Service` 只是「有统一 start/stop 生命周期 + 显式依赖」的模块,**不是** actor、
//!   不跑在独立调度器上,也不引入 DI 容器 / 全局 Event Bus(§1.6)。
//! - 0.2.1 走「薄包装」:Service 是空壳,`start` 里调用现有模块函数,内部逻辑零改动。
//!   全局静态(尤其 Win32 hook 回调必须访问的)保留,Service 只做生命周期入口。
//! - 现有模块包成 5 个 Service;`SearchService`/`SearchEngine`(0.2.2)与插件/意图
//!   服务(0.3)本版不建——无可包装的现有逻辑,留到对应版本。
//!
//! `AppContext` 是共享依赖容器(§1.2),**不是** DI 容器:只把 setup 阶段散落的启动
//! 依赖收拢显式化。它**不替换** Tauri 的 `app.manage` / `app.state`——command 层继续
//! 用 `app.state::<DbPools>()`,AppContext 仅服务于 setup 期的 Service 编排。

use tauri::{AppHandle, Emitter, Manager};

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
    pub pools: crate::infra::data::DbPools,
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
    /// `build_capability_tools` 消费，AI tool_call 只命中 Capability。
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

/// 历史服务:DbPools 已由 main.rs `app.manage` 持有,本版无启动副作用。
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
                            // 0.17.6: AI 活跃时不隐藏，而是 set_focus + emit SHOWN
                            // 防止用户在 AI 生成过程中误按热键导致窗口消失
                            if crate::infra::platform::window::is_main_ai_active() {
                                if let Some(win) = app.get_webview_window("main") {
                                    let _ = win.set_focus();
                                }
                                let _ = app.emit(crate::domain::event_names::EventNames::SHOWN, ());
                            } else {
                                crate::infra::platform::window::hide(&app, "toggle");
                            }
                        } else {
                            let chan_ms = trigger_time.elapsed().as_secs_f64() * 1000.0;
                            tracing::debug!(
                                target: "perf",
                                chan_ms,
                                "[perf] Tap→invoke: channel+scheduling delay"
                            );
                            crate::infra::platform::window::invoke(&app);
                            let total_ms = trigger_time.elapsed().as_secs_f64() * 1000.0;
                            tracing::debug!(
                                target: "perf",
                                total_ms,
                                "[perf] Tap→shown: total (key-up to emit blink://shown)"
                            );
                            // 记录热键唤起耗时（按键 → 窗口 invoke）
                            crate::infra::utils::perf::record(
                                crate::infra::utils::perf::MetricCategory::Hotkey,
                                "key_to_show",
                                total_ms,
                                None,
                            );
                        }
                    }
                    crate::infra::platform::hotkey::HotkeyEvent::Hold(_) => {
                        // 长按开始 → 语音录音开始（async：可能需等待模型加载）
                        // 0.10.7：chord 门禁——chord 总开关关 / voice_input 在 disabled 列表 →
                        // 不启动录音。这让设置页的 voice_input 开关真正生效（而非仅控显示）。
                        let pool = &app.state::<crate::infra::data::DbPools>().config;
                        let chord_cfg = crate::app::config::get_chord_config(&pool).await;
                        let disabled = crate::app::config::get_disabled_chord_actions(&pool).await;
                        let voice_disabled = disabled.iter().any(|d| d == "voice_input");
                        if chord_cfg.chord_enabled && !voice_disabled {
                            voice_service.start_recording().await;
                            // 0.17.2：语音录音开始 → 托盘呼吸动画
                            crate::app::tray::start_breathing(&app);
                        } else {
                            tracing::debug!(
                                chord_enabled = chord_cfg.chord_enabled,
                                voice_disabled,
                                "hold 触发但 voice_input chord 已禁用,跳过录音"
                            );
                        }
                    }
                    crate::infra::platform::hotkey::HotkeyEvent::HoldRelease(_) => {
                        // 长按结束 → 停止录音 → STT → 注入/fill-query
                        voice_service.stop_recording().await;
                        // 0.17.2：语音录音结束 → 停止托盘呼吸动画
                        crate::app::tray::stop_breathing(&app);
                    }
                    crate::infra::platform::hotkey::HotkeyEvent::VoiceCancel(_) => {
                        // ESC 取消录音
                        voice_service.cancel_recording();
                        // 0.17.2：取消录音 → 停止托盘呼吸动画
                        crate::app::tray::stop_breathing(&app);
                    }
                    crate::infra::platform::hotkey::HotkeyEvent::Chord(key) => {
                        // 0.10.7.2：chord 独占模式吞键后,前端收不到 keydown,
                        // 由 hook 发此事件,此处复用 trigger_chord 逻辑触发动作。
                        let Some(registry) =
                            app.try_state::<std::sync::Arc<crate::domain::chord::ChordRegistry>>()
                        else {
                            tracing::warn!("chord registry 未就绪,跳过 Chord 事件");
                            continue;
                        };
                        let pool = &app.state::<crate::infra::data::DbPools>().config;
                        let chord_cfg = crate::app::config::get_chord_config(&pool).await;
                        let disabled = crate::app::config::get_disabled_chord_actions(&pool).await;
                        let key_lower = key.to_lowercase();
                        // 门禁：disabled 列表命中即跳过
                        if let Some(action_id) =
                            registry.action_id_for_key(&key_lower, &chord_cfg.bindings)
                        {
                            if disabled.iter().any(|d| d == action_id) {
                                tracing::debug!(%key_lower, %action_id, "chord 已禁用,跳过触发");
                                continue;
                            }
                        }
                        let env_arc = app
                            .state::<std::sync::Arc<crate::app::domain_env::TauriDomainEnv>>()
                            .inner()
                            .clone();
                        if let Err(e) = registry
                            .trigger(&key, &chord_cfg.bindings, env_arc.as_ref(), None, None)
                            .await
                        {
                            tracing::warn!(%key, %e, "chord trigger 失败");
                        }
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
        let cfg = crate::app::config::get_context_config(&ctx.pools.config).await;
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
        crate::infra::platform::clipboard::start_listener(
            ctx.pools.history.clone(),
            ctx.pools.cache.clone(),
            cfg,
        );
        Ok(())
    }
}

/// 便签恢复服务（0.16.13）：启动时异步恢复 visible=true 的便签窗口。
///
/// 从 main.rs 内联 spawn 提取为 Service，纳入统一生命周期编排。
/// 恢复逻辑：延迟 2s -> 读取所有便签 -> visible=true 的恢复窗口 -> 每条间隔 50ms -> 不抢焦点。
///
/// **时序安全**：StickyService 在 main.rs setup 中 `app.manage()` 注册，
/// 晚于 Service 启动。但恢复任务延迟 2s 才执行，届时 StickyService 已就绪。
pub struct StickyRecoveryService;

#[async_trait::async_trait]
impl Service for StickyRecoveryService {
    fn name(&self) -> &'static str {
        "sticky_recovery"
    }
    fn deps(&self) -> &'static [&'static str] {
        &["config", "history"]
    }
    async fn start(&self, ctx: &AppContext) -> Result<(), String> {
        let app = ctx.app.clone();
        tauri::async_runtime::spawn(async move {
            // 延迟 2s 避免与启动竞争资源
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;

            // StickyService 在 main.rs setup 后半段 manage，此时应已就绪
            let Some(svc) = app.try_state::<std::sync::Arc<crate::domain::sticky::StickyService>>()
            else {
                tracing::warn!("便签恢复：StickyService 未就绪，跳过");
                return;
            };
            let svc = svc.inner().clone();

            let visible = svc.load_for_recovery().await;
            let total = visible.len();
            let mut restored = 0usize;
            let mut skipped = 0usize;
            for note in &visible {
                // 数据验证——隔离损坏记录
                if note.id.is_empty() {
                    tracing::warn!("便签恢复：跳过空 id 记录");
                    skipped += 1;
                    continue;
                }
                if note.width < 120 || note.height < 80 {
                    tracing::warn!(
                        sticky_id = %note.id,
                        width = note.width,
                        height = note.height,
                        "便签恢复：尺寸非法，跳过"
                    );
                    skipped += 1;
                    continue;
                }

                // 逐条恢复窗口，单条失败隔离
                // focus=false：恢复路径不抢主窗口焦点
                match crate::infra::platform::window::show_sticky_window(
                    &app,
                    &note.id,
                    note.x,
                    note.y,
                    note.width,
                    note.height,
                    note.always_on_top,
                    false,
                ) {
                    Ok(()) => restored += 1,
                    Err(e) => {
                        tracing::warn!(sticky_id = %note.id, error = %e, "便签窗口恢复失败，跳过");
                        skipped += 1;
                    }
                }

                // 节流——每条间隔 50ms，不抢占 tokio runtime
                if visible.len() > 1 {
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                }
            }
            if total > 0 {
                tracing::info!(
                    total,
                    restored,
                    skipped,
                    "便签恢复完成：共 {} 条，恢复 {} 条，跳过 {} 条",
                    total,
                    restored,
                    skipped
                );
            }
        });
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
        Box::new(StickyRecoveryService),
    ]
}
