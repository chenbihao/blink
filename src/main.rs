#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod domain;
mod infra;

use domain::execution::Action;
use tauri::{Emitter, Manager, WindowEvent, tray::TrayIconBuilder};

fn main() {
    // 开启 Per-Monitor V2 DPI 感知：混合 DPI（例如主屏 100% + 副屏 150%）跨屏时
    // 由系统按目标显示器的 scale 自动重算尺寸，避免文字虚化 / 位置漂移。
    // 必须在任何窗口创建前调用；调用失败静默忽略（Win10 早期版本走 System DPI 也可用）。
    #[cfg(windows)]
    unsafe {
        use windows::Win32::UI::HiDpi::{
            DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext,
        };
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }

    // 初始化日志（尽早，默认 error；setup 读配置后 reload 到用户级别）
    infra::utils::logging::init("error");

    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            infra::platform::window::invoke(app);
        }))
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None,
        ))
        .plugin(tauri_plugin_dialog::init())
        // 自定义协议：http://blink-icon.localhost/<url-encoded-path>（Windows）—— 按需懒加载应用图标 PNG。
        // 前端 <img src> 直接引用，图标提取移出搜索热路径（见 search/icon.rs）。
        .register_asynchronous_uri_scheme_protocol("blink-icon", |_ctx, request, responder| {
            // path 段形如 "/C%3A%5C..."，去掉前导 '/' 后做 percent-decode 还原真实路径。
            let raw = request.uri().path().trim_start_matches('/').to_string();
            let path = percent_decode(&raw);
            // tracing::debug!(%path, "blink-icon: 收到图标请求");
            tauri::async_runtime::spawn(async move {
                let path_for_log = path.clone();
                let icon = tauri::async_runtime::spawn_blocking(move || {
                    crate::domain::search::icon::get_icon_png(&path)
                })
                .await
                .ok()
                .flatten();
                let response = match icon {
                    Some(bytes) => tauri::http::Response::builder()
                        .status(200)
                        .header("Content-Type", "image/png")
                        .body(bytes)
                        .unwrap(),
                    None => {
                        tracing::debug!(path = %path_for_log, "blink-icon: 未取到图标，返回 404");
                        tauri::http::Response::builder()
                            .status(404)
                            .body(Vec::new())
                            .unwrap()
                    }
                };
                responder.respond(response);
            });
        })
        // 自定义协议：http://blink-screenshot.localhost/capture（Windows 上 Tauri v2 会把
        // `blink-screenshot://` 重写成 `http://blink-screenshot.localhost/`，与 blink-icon 同）
        // —— 返回当前 SESSION 的 PNG bytes。
        //
        // 由 `screenshot::begin_session()` 提前截屏并存 SESSION，协议只做"读内存 + 编码 PNG"，
        // 不再触发新的截屏——避免"overlay show 之后才 BitBlt"的时序竞态。
        // SESSION 为空 → 404，overlay 前端可显示错误提示。
        .register_asynchronous_uri_scheme_protocol("blink-screenshot", |_ctx, _request, responder| {
            tauri::async_runtime::spawn(async move {
                let bytes = tauri::async_runtime::spawn_blocking(|| {
                    crate::infra::platform::screenshot::session_png()
                })
                .await
                .ok()
                .flatten();

                let response = match bytes {
                    Some(bytes) => tauri::http::Response::builder()
                        .status(200)
                        .header("Content-Type", "image/png")
                        .header("Access-Control-Allow-Origin", "*")
                        .header("Cache-Control", "no-store")
                        .body(bytes)
                        .unwrap(),
                    None => {
                        tracing::warn!("blink-screenshot: SESSION 为空,返回 404");
                        tauri::http::Response::builder()
                            .status(404)
                            .body(Vec::new())
                            .unwrap()
                    }
                };
                responder.respond(response);
            });
        })
        .setup(|app| {
            // 启动总耗时（setup 结束时手动记录，因为 setup 在同步上下文，没有 runtime 句柄）
            let startup_start = std::time::Instant::now();

            // 初始化历史记录 SQLite
            let pool = tauri::async_runtime::block_on(infra::data::history::init_db())
                .expect("failed to init history db");

            // 初始化性能统计（0.7.0）
            tauri::async_runtime::block_on(infra::utils::perf::init(&pool))
                .expect("failed to init perf metrics");

            // 初始化剪贴板历史表（0.7.3）
            tauri::async_runtime::block_on(infra::data::clipboard::init_db(&pool))
                .expect("failed to init clipboard db");

            // 初始化 AI 工具调用审计表（0.11.4 改进 2 §2.2.7）
            tauri::async_runtime::block_on(infra::data::ai_audit::init_db(&pool))
                .expect("failed to init ai_tool_audit db");

            // 初始化图标缓存持久化（0.7.4）
            tauri::async_runtime::block_on(domain::search::icon::init(&pool))
                .expect("failed to init icon cache");

            // 初始化配置
            let config_start = std::time::Instant::now();
            tauri::async_runtime::block_on(app::config::init_config(&pool))
                .expect("failed to init config");
            // 记录配置加载耗时（setup 在同步上下文，需用 block_on）
            let config_elapsed = config_start.elapsed().as_secs_f64() * 1000.0;
            tauri::async_runtime::block_on(infra::utils::perf::record_blocking(
                &pool,
                infra::utils::perf::MetricCategory::Startup,
                "config_load",
                config_elapsed,
                None,
            ));

            // 读取应用配置(快照)
            let app_config = tauri::async_runtime::block_on(app::config::get_config(&pool));

            // 日志级别 reload 到配置值（init 时为默认 error）
            infra::utils::logging::update_level(&app_config.log_level);

            // pool 交给 Tauri 管理(command 层用 app.state 取);AppContext 再留一份 clone
            app.manage(pool.clone());

            // 同步开机自启（确保注册表 Run 项与配置一致，覆盖用户在 app 外改动的情况）
            {
                use tauri_plugin_autostart::ManagerExt;
                let manager = app.autolaunch();
                let _ = if app_config.auto_start {
                    manager.enable()
                } else {
                    manager.disable()
                };
            }

            // 主窗口启动即隐藏；注册焦点事件
            if let Some(w) = app.get_webview_window("main") {
                let _ = w.hide();
                // 拦截 Alt+Space 系统菜单 + 启用系统级圆角（Win11+ DWM）
                if let Ok(hwnd) = w.hwnd() {
                    // hwnd 来自 Tauri(windows 0.61)，转成本项目依赖的 windows 0.62 HWND
                    let hwnd = windows::Win32::Foundation::HWND(hwnd.0 as _);
                    infra::platform::window::install_sysmenu_blocker(hwnd);
                    infra::platform::window::enable_rounded_corners(hwnd);
                }
                w.on_window_event(move |event| {
                    if let WindowEvent::Focused(focused) = event {
                        infra::platform::window::on_focused(*focused);
                    }
                });
            }

            // 托盘菜单：设置 + 关于 + 分隔线 + 退出（按当前语言构建文案，运行时切语言会重建）
            let menu = app::tray::build_menu(app, &app_config.language)?;
            TrayIconBuilder::new()
                .icon(app.default_window_icon().unwrap().clone())
                .tooltip("Blink")
                .menu(&menu)
                .on_menu_event(|app, event| match event.id().as_ref() {
                    "settings" => open_settings(app),
                    "about" => open_about(app),
                    "quit" => app.exit(0),
                    _ => {}
                })
                .build(app)?;

            // 加载 builtin 插件
            // 全局代理配置(engine:_global_proxy → {http,https})，进程启动时 env 注入，ureq/reqwest 原生读取
            let global_proxy = tauri::async_runtime::block_on(app::config::get_engine_config(&pool, "_global_proxy"));
            let proxy = global_proxy.and_then(|v| {
                let http = v.get("http").and_then(|s| s.as_str()).map(|s| s.to_string());
                let https = v.get("https").and_then(|s| s.as_str()).map(|s| s.to_string());
                if http.is_some() || https.is_some() { Some((http.unwrap_or_default(), https.unwrap_or_default())) }
                else { None }
            });
            let plugins = domain::plugin::load_builtin_plugins(app.handle(), proxy.clone());
            // 构造意图路由 RuleRouter,从插件 manifest 注入规则(合并用户自定义 triggers)。
            let router = std::sync::Arc::new(domain::intent::RuleRouter::new(app_config.surface_takeover_enabled));
            // 0.8.6 §8.1.2：共享 min_score 引用（SearchService ↔ KeywordProducer）
            let min_score_shared = std::sync::Arc::new(std::sync::RwLock::new(app_config.autosuggest_min_score));
            router.init_arbiter(min_score_shared.clone());

            // 无条件构造 PluginEngine（空 plugins 也是合法态）。
            // 早期用 Option<Arc<PluginEngine>> 表示"无插件"，但导致：
            // - Tauri state 类型每处易拼错（`try_state::<Arc<PE>>` vs `state::<Option<Arc<PE>>>`）
            // - 消费点 15 处充斥 `as_ref()` / `if let Some(pe)` 样板
            // PluginEngine::new 无必须条件，plugins=vec![] 时各方法均安全（空迭代 / find_plugin 返 None）。
            tracing::info!(count = plugins.len(), "PluginEngine 已构造");
            let plugin_engine = std::sync::Arc::new(domain::plugin::PluginEngine::new(plugins.clone(), pool.clone(), proxy));
            // 0.4→0.5 自动迁移（首次运行时执行一次，后续 marker 跳过；空 plugins 时循环空转）
            tauri::async_runtime::block_on(infra::data::history::migrate_0_4_to_0_5(&pool, &plugins));
            // 0.9.5 camelCase→snake_case 迁移（前端重构统一字段命名，存量 DB 需改写）
            tauri::async_runtime::block_on(infra::data::history::migrate_camelcase_to_snake(&pool));
            // 加载/初始化每个插件配置(不存在则写默认 {enabled, settings:null})。
            tauri::async_runtime::block_on(plugin_engine.init_configs());

            // 0.8.2 §3.4：把 PluginEngine 作为 PluginSettingResolver 后置注入 RuleRouter,
            // 让 Context 触发能读插件 target_lang(如翻译)。同步 app_language 快照。
            router.set_setting_resolver(plugin_engine.clone() as std::sync::Arc<dyn domain::plugin::PluginSettingResolver>);
            router.set_app_language(app_config.language.clone());

            // 0.8.5 §6.4：注入本体 engine 的 keyword 规则（先注入 engine，再注入插件——
            // engine 优先级更高，命中即独占返回；插件 Takeover 走各自 manifest triggers）。
            domain::search::register_engine_rules(&router);

            // 注入规则到 RuleRouter（合并 manifest triggers + 用户自定义 triggers）
            for plugin in &plugins {
                if !plugin_engine.is_enabled(plugin.id()) {
                    tracing::debug!(plugin = %plugin.id(), "插件已禁用,跳过规则注入");
                    continue;
                }
                // 读取配置（含自定义 triggers）
                let config = plugin_engine.get_config(plugin.id()).unwrap_or_default();
                let effective_triggers = config.effective_triggers(&plugin.manifest().triggers);
                router.reload_plugin_triggers(plugin.id(), &effective_triggers);
            }

            // 构造三层搜索引擎配置（应用搜索 / 文件搜索 / 计算器）
            let start_menu_config = tauri::async_runtime::block_on(app::config::get_start_menu_config(&pool));
            let file_config = tauri::async_runtime::block_on(app::config::get_file_search_config(&pool));
            let calc_config = tauri::async_runtime::block_on(app::config::get_calc_config(&pool));
            tracing::info!(
                app_search = start_menu_config.enabled,
                file_search = file_config.enabled,
                data_source = %file_config.data_source,
                calc = calc_config.enabled,
                "搜索引擎配置"
            );

            // 构造 SearchService(多路引擎 + 意图路由)。command 层经 app.state 取用。
            let engine_configs = domain::search::EngineConfigs {
                start_menu: start_menu_config,
                file: file_config,
                calc: calc_config,
            };
            let search_service = std::sync::Arc::new(domain::search::SearchService::new(
                app.handle().clone(),
                pool.clone(),
                domain::search::build_engines(engine_configs, pool.clone()),
                plugin_engine.clone(),
                router.clone(),
                min_score_shared,
            ));
            app.manage(search_service.clone());
            // 初始化 SearchService 的 max_results 内存值（来自 AppConfig，搜索热路径零 IO）
            search_service.update_max_results(app_config.max_results as usize);
            // 初始化内置动作 disable 列表（0.8.0 §1.3）
            search_service.update_disabled_builtin_actions(app_config.disabled_builtin_actions.clone());
            // 初始化 Autosuggestion 配置（0.8.1 §2.5）
            search_service.update_autosuggest_config(
                app_config.autosuggest_enabled,
                app_config.autosuggest_min_score,
            );
            // 初始化 context binding 禁用列表（0.8.3 §4.6）
            search_service.update_disabled_context_bindings(
                app_config.disabled_context_bindings.clone(),
            );
            // 初始化界面语言快照（0.8.1）— 供 empty_arg_hint 等 LocalizableText 解析用
            search_service.update_language(app_config.language.clone());
            // 启动清理过期搜索历史（后台 spawn，不阻塞启动；enabled=false 或 days=0 跳过）
            {
                let cleanup_pool = pool.clone();
                let days = app_config.search_history_days;
                let enabled = app_config.search_history_enabled;
                tauri::async_runtime::spawn(async move {
                    if enabled {
                        infra::data::history::cleanup_old(&cleanup_pool, days).await;
                    }
                });
            }
            // 启动清理过期剪贴板历史（按 retention_days 清理，0.11.5 改为按天清理策略）
            {
                let cleanup_pool = pool.clone();
                let days = app_config.clipboard.retention_days;
                let clip_enabled = app_config.clipboard.enabled;
                tauri::async_runtime::spawn(async move {
                    if clip_enabled && days > 0 {
                        infra::data::clipboard::cleanup_old(&cleanup_pool, days).await;
                    }
                });
            }
            // 注入 ContextConfig 内存缓存：invoke 热键回调零 IO 读它（热更新见 update_context_config）
            let context_config = tauri::async_runtime::block_on(app::config::get_context_config(&pool));
            let context_config_arc = std::sync::Arc::new(std::sync::RwLock::new(context_config));
            app.manage(context_config_arc.clone());

            // 0.9.2.1：注册剪贴板变化 hook,让主窗口打开时用户在其内部（或其他 app）
            // Ctrl+C 后 AwarenessSnapshot 里的 Clipboard 项也能局部刷新——之前 snapshot
            // 只在 window::invoke 时一次性 collect,主窗口保持打开期间就一直是老快照。
            //
            // 门控与 context::collect() 保持一致：总开关 + clipboard_enabled + 前台敏感应用
            // 检查（复用 collect 内部逻辑,避免密码管理器 Ctrl+C 悄悄写入 snapshot）。
            //
            // hook 在 clipboard listener 线程被同步调用（消息循环）,只做轻量 RwLock 读 +
            // upsert_text；两者都不阻塞。
            {
                let ctx_arc = context_config_arc.clone();
                let ss = search_service.clone();
                let app_handle = app.handle().clone();
                infra::platform::clipboard::set_change_hook(Box::new(move |text: &str| {
                    let cfg = { ctx_arc.read().unwrap().clone() };
                    // 总开关 / 剪贴板开关 —— 与 collect() 前两道门一致
                    if !cfg.enabled || !cfg.clipboard_enabled {
                        return;
                    }
                    // 前台敏感应用（如密码管理器）—— 与 collect() 第三道门一致。
                    // 只有敏感时才丢弃；非敏感或拿不到前台照常回写。
                    if let Some(fg) = infra::platform::context::foreground_app() {
                        if cfg.is_sensitive(&fg.process_name) {
                            tracing::debug!(app = %fg.process_name, "剪贴板变化 hook：前台敏感,跳过 snapshot 回写");
                            return;
                        }
                    }
                    ss.update_clipboard_text(Some(text.to_string()));
                    tracing::debug!(len = text.chars().count(), "剪贴板变化 → snapshot 已局部刷新");
                    // 通知主窗口用当前 query 重跑一次（刷 Context Ghost / AI 四筛子）。
                    // 只在主窗口可见时发——隐藏时窗口收不到 emit（Tauri 2 hidden webview
                    // drop event,见 [[tauri-hidden-webview-emit-dropped]]）,而且下一次
                    // invoke 会 collect() 重拍快照,不需要 push。
                    if infra::platform::window::is_visible() {
                        if let Err(e) = app_handle.emit("blink://awareness-updated", ()) {
                            tracing::debug!(?e, "emit blink://awareness-updated 失败");
                        }
                    }
                }));
            }
            // RuleRouter 单独注册供设置页 API 用（triggers 热更新）
            app.manage(router.clone());

            // 0.8.5 Chord：构建 registry（注册 stub 动作）
            let chord_registry = std::sync::Arc::new(crate::domain::chord::build_default_registry());
            // 0.8.6 Action 统一执行入口
            let action_registry = std::sync::Arc::new(crate::domain::execution::ActionRegistry::new());
            // 0.9.7 Capability 能力协议层（inventory 自动收集 5 个样板能力）
            let capability_registry = std::sync::Arc::new(crate::domain::capability::CapabilityRegistry::new());

            // 0.9.3:注册插件 tool 到 ActionRegistry——让 AI 路由能调用插件能力。
            // 遍历所有已加载插件的 manifest.tools，为每个 tool 创建 PluginActionAdapter 并注册。
            {
                let mut plugin_tool_count = 0usize;
                for plugin_handle in plugin_engine.all_plugins() {
                    let manifest = plugin_handle.manifest();
                    if manifest.tools.is_empty() {
                        continue;
                    }
                    for tool_def in &manifest.tools {
                        let adapter = crate::domain::plugin::PluginActionAdapter::new(
                            plugin_handle.clone(),
                            tool_def,
                            &manifest.name,
                        );
                        if action_registry.get(adapter.id()).is_some() {
                            tracing::warn!(
                                plugin = %manifest.id,
                                tool = %adapter.id(),
                                "插件 tool id 与已有 Action 冲突,跳过"
                            );
                            continue;
                        }
                        tracing::info!(
                            plugin = %manifest.id,
                            tool = %adapter.id(),
                            danger = ?tool_def.danger_class,
                            "注册插件 tool"
                        );
                        action_registry.register(std::sync::Arc::new(adapter));
                        plugin_tool_count += 1;
                    }
                }
                if plugin_tool_count > 0 {
                    tracing::info!(
                        count = plugin_tool_count,
                        total = action_registry.len(),
                        "插件 tool 注册完成"
                    );
                }
            }

            // 0.9.2 Phase 5b:AIProviderRegistry 用 RigFactory 真接 rig-core。
            // AI 配置分片(第 7 分片,独立于 AppConfig 门面);默认 enabled=false,老用户零副作用。
            let ai_config = tauri::async_runtime::block_on(
                app::config::ConfigStore::get::<app::ai_config::AIConfig>(&pool),
            );
            let ai_registry = std::sync::Arc::new(
                crate::domain::ai::AIProviderRegistry::from_config(
                    crate::domain::ai::default_factory(),
                    &ai_config,
                ),
            );
            // 注入 SearchService(0.9.2 setter 注入,规避 search_service 先于 ai_registry
            // 构造的顺序倒挂)。未注入时 exec_mixed 的 AI lane 会安静跳过。
            search_service.set_ai_registry(ai_registry.clone());
            tracing::info!(
                enabled = ai_config.enabled,
                providers = ai_config.providers.len(),
                pool_size = ai_registry.size(),
                "AIProviderRegistry 已构造(5b RigFactory)"
            );

            // PluginEngine：clone 一份给 AppContext，原值继续 manage
            let plugin_engine_for_ctx = plugin_engine.clone();

            // 0.10: VoiceService(hold-to-talk 管线编排)
            // 初始化 STT 配置缓存（供 STT 引擎同步读取）
            let stt_config = tauri::async_runtime::block_on(
                app::config::ConfigStore::get::<app::stt_config::SttConfig>(&pool),
            );
            app::stt_config::init_cache(stt_config);

            let voice_service = std::sync::Arc::new(app::voice::VoiceService::new(app.handle().clone()));

            // 后台服务编排:按依赖拓扑顺序启动。
            // 0.8.6 §8.2.3：AppContext 持有全部核心服务引用（真依赖容器）。
            let ctx = app::service::AppContext {
                app: app.handle().clone(),
                pool: pool.clone(),
                config: app_config,
                ai_config: ai_config.clone(),
                search_service: search_service.clone(),
                plugin_engine: plugin_engine_for_ctx,
                router: router.clone(),
                chord_registry: chord_registry.clone(),
                action_registry: action_registry.clone(),
                capability_registry: capability_registry.clone(),
                ai_registry: ai_registry.clone(),
                voice_service: voice_service.clone(),
            };
            let services = app::service::all_services();
            let svc_start = std::time::Instant::now();
            for svc in &services {
                if let Err(e) = tauri::async_runtime::block_on(svc.start(&ctx)) {
                    tracing::error!(service = svc.name(), error = %e, "service start failed");
                }
            }
            // 记录服务初始化耗时
            let svc_elapsed = svc_start.elapsed().as_secs_f64() * 1000.0;
            tauri::async_runtime::block_on(infra::utils::perf::record_blocking(
                &pool,
                infra::utils::perf::MetricCategory::Startup,
                "services_init",
                svc_elapsed,
                None,
            ));
            // 记录启动总耗时（setup 在同步上下文，需用 block_on）
            // 注意：必须在 app.manage(pool) 之前记录，否则 pool 已被 move
            let startup_total_ms = startup_start.elapsed().as_secs_f64() * 1000.0;
            tauri::async_runtime::block_on(infra::utils::perf::record_blocking(
                &pool,
                infra::utils::perf::MetricCategory::Startup,
                "total",
                startup_total_ms,
                None,
            ));

            // 注册到 Tauri state（command 层 app.state 取用）
            app.manage(plugin_engine);
            app.manage(chord_registry);
            app.manage(action_registry);
            // 0.9.7 Capability 能力协议层（Step 4 起 AI tool 池消费）
            app.manage(capability_registry);
            // 0.9.1 Phase 5a：AI Provider registry(Phase 5b 起 SearchService 消费)
            app.manage(ai_registry);
            // 0.10: VoiceService(command 层 cancel_voice_recording / is_voice_recording 消费)
            app.manage(voice_service);

            // 后台预热次级窗口（3s 延迟，不阻塞启动；WebView2 冷启动 300~400ms → 预热后 show <50ms）
            infra::platform::window::preheat_secondary_windows(app.handle().clone());

            // 0.10: 自动启动 funasr-server（懒加载，延迟 5s 避免与启动竞争资源）
            {
                let stt_config = app::stt_config::get_stt_config();
                if stt_config.enabled
                    && stt_config.mode == app::stt_config::SttMode::Local
                    && stt_config.local_engine.auto_start_server
                {
                    let app_clone = app.handle().clone();
                    tauri::async_runtime::spawn(async move {
                        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                        tracing::info!("自动启动 funasr-server（auto_start_server 已开启）");
                        // 复用 start_funasr_server 命令逻辑（包含环境检查 + 子进程管理）
                        let _ = app::commands::start_funasr_server(app_clone).await;
                    });
                }
            }

            // 持有服务列表,保证其生命周期与 app 一致。
            app.manage(services);

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            app::commands::hide_window,
            app::commands::hide_settings_window,
            app::commands::frontend_log,
            app::commands::search_apps,
            app::commands::trigger_ai,
            app::commands::launch_app,
            app::commands::run_builtin_action,
            app::commands::confirm_ai_action,
            app::commands::list_builtin_actions,
            app::commands::list_context_bindings,
            app::commands::trigger_chord,
            app::commands::list_chord_actions,
            app::commands::list_all_chord_actions,
            app::commands::is_alt_down,
            app::commands::set_chord_mode,
            app::commands::screenshot_copy,
            app::commands::screenshot_copy_region,
            app::commands::screenshot_cancel,
            app::commands::screenshot_pin,
            app::commands::screenshot_pin_hide,
            app::commands::screenshot_pin_transform,
            app::commands::screenshot_save,
            app::commands::screenshot_set_annotation_mode,
            app::commands::ocr_image,
            app::commands::translate_text,
            app::commands::translate_lines,
            app::commands::hide_screenshot_overlay,
            app::commands::get_storage_info,
            app::commands::clear_history,
            app::commands::get_app_info,
            app::commands::check_update,
            app::commands::resize_window,
            app::commands::get_config,
            app::commands::set_config,
            app::commands::reset_config,
            app::commands::record_hotkey,
            app::commands::open_log_file,
            app::commands::open_log_dir,
            app::commands::get_log_info,
            app::commands::get_start_menu_config,
            app::commands::get_calc_config,
            app::commands::probe_everything,
            app::commands::get_engine_config,
            app::commands::get_plugins,
            app::commands::get_context_config,
            app::commands::open_containing_folder,
            app::commands::open_lnk_target,
            app::commands::copy_to_clipboard,
            app::commands::reset_item_history,
            app::commands::list_running_processes,
            app::commands::show_context_menu,
            app::commands::hide_context_menu,
            app::commands::context_menu_action,
            app::commands::probe_interpreters,
            app::commands::open_file_dialog,
            app::commands::get_clipboard_history,
            app::commands::search_clipboard_history,
            app::commands::record_clipboard_hit,
            app::commands::delete_clipboard_item,
            app::commands::clear_clipboard_history,
            app::commands::get_clipboard_stats,
            app::commands::get_perf_overview,
            app::commands::get_perf_percentiles,
            app::commands::get_perf_slow_queries,
            app::commands::get_perf_recent,
            app::commands::export_perf_report,
            app::commands::clear_perf_data,
            app::commands::open_url,
            app::commands::toggle_default_trigger,
            app::commands::add_custom_trigger,
            app::commands::delete_custom_trigger,
            app::commands::get_config_section,
            app::commands::set_config_section,
            app::commands::save_ai_secret,
            app::commands::delete_ai_secret,
            app::commands::has_ai_secret,
            app::commands::get_ai_secret_hint,
            app::commands::test_ai_provider,
            app::commands::fetch_ai_models,
            app::commands::get_system_prompt_info,
            // 0.10 STT / 语音
            app::commands::get_stt_config,
            app::commands::set_stt_config,
            app::commands::list_stt_models,
            app::commands::download_stt_model,
            app::commands::delete_stt_model,
app::commands::cancel_voice_recording,
app::commands::is_voice_recording,
app::commands::list_audio_devices,
app::commands::start_audio_test,
app::commands::stop_audio_test,
app::commands::get_funasr_env,
app::commands::get_funasr_log_history,
app::commands::setup_python_env,
app::commands::start_funasr_server,
app::commands::stop_funasr_server,
app::commands::diagnose_stt,
app::commands::test_cloud_stt,
app::commands::get_stt_space_usage,
app::commands::cleanup_stt_space,
app::commands::open_stt_folder,
            app::commands::resize_voice_overlay,
            app::commands::get_default_hotkey,
        ])
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|_app, event| {
            if let tauri::RunEvent::Exit = event {
                // Blink 退出时 kill funasr-server 子进程，避免孤儿进程
                crate::app::commands::shutdown_funasr_server_blocking();
            }
        });
}

/// 打开设置窗口（委托给 window 模块统一实现）。
fn open_settings(app: &tauri::AppHandle) {
    infra::platform::window::open_settings(app);
}

/// 打开设置窗口并定位到「关于」Tab。
fn open_about(app: &tauri::AppHandle) {
    open_settings(app);
    if let Some(w) = app.get_webview_window("settings") {
        // HTML 异步加载，需延迟等 DOM 就绪后再切 Tab
        let _ = w.eval(
            "setTimeout(() => document.querySelector('.tab[data-tab=\"about\"]')?.click(), 300)",
        );
    }
}

/// 最小 percent-decode：还原前端 `encodeURIComponent` 编码的图标路径。
/// 仅处理 `%XX`，非法序列原样保留。
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = (bytes[i + 1] as char).to_digit(16);
            let lo = (bytes[i + 2] as char).to_digit(16);
            if let (Some(h), Some(l)) = (hi, lo) {
                out.push((h * 16 + l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}
