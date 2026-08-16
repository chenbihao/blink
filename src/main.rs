#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod cli;
mod domain;
mod infra;

use domain::capability::Capability;
use domain::event_names::EventNames;
use tauri::{
    Emitter, Manager, WindowEvent,
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
};

fn main() {
    // 0.13.5: CLI 模式检测——如果命令行参数匹配 CLI 子命令，执行 CLI 逻辑后退出。
    // 必须在任何 Tauri 初始化之前检测，避免创建不必要的 GUI 资源。
    if let Some(exit_code) = cli::try_run_cli() {
        std::process::exit(exit_code);
    }

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
            app::window_orchestrator::invoke(app);
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
                // 0.17.1：stock: 前缀走 SHGetStockIconInfo 路径（系统文件夹图标）
                let icon = tauri::async_runtime::spawn_blocking(move || {
                    if let Some(rest) = path.strip_prefix("stock:") {
                        rest.parse::<u32>()
                            .ok()
                            .and_then(crate::infra::platform::icon::get_stock_icon_png)
                    } else {
                        crate::infra::platform::icon::get_icon_png(&path)
                    }
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
        // 自定义协议：http://blink-clipimg.localhost/<image-id>（0.16.4）——
        // 按需懒加载剪贴板图片缩略图 PNG。前端 <img src> 直接引用。
        .register_asynchronous_uri_scheme_protocol("blink-clipimg", |_ctx, request, responder| {
            let raw = request.uri().path().trim_start_matches('/').to_string();
            let image_id = percent_decode(&raw);
            tauri::async_runtime::spawn(async move {
                let thumb = crate::infra::data::clipboard_images::get_thumb_by_id_global(&image_id).await;
                let response = match thumb {
                    Some(bytes) => tauri::http::Response::builder()
                        .status(200)
                        .header("Content-Type", "image/png")
                        .header("Cache-Control", "max-age=3600")
                        .body(bytes)
                        .unwrap(),
                    None => {
                        tracing::debug!(id = %image_id, "blink-clipimg: 未找到缩略图，返回 404");
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
        .register_asynchronous_uri_scheme_protocol("blink-screenshot", |_ctx, request, responder| {
            let path = request.uri().path().trim_matches('/');
            let editor_payload = path == "editor";
            let raw_payload = path == "raw";
            tauri::async_runtime::spawn(async move {
                if raw_payload {
                    // A+B 优化：按显示器分块返回 BGRA（不做 RGBA swap），前端做 swap。
                    // ?monitor=N → 只返回第 N 个显示器的 BGRA 区域 + 偏移 headers；
                    // 无 monitor 参数 → 返回完整虚拟桌面 BGRA（向后兼容）。
                    let query = request.uri().query().unwrap_or("");
                    let monitor_idx: Option<usize> = query
                        .split('&')
                        .find(|p| p.starts_with("monitor="))
                        .and_then(|p| p.strip_prefix("monitor="))
                        .and_then(|v| v.parse::<usize>().ok());

                    let result = tauri::async_runtime::spawn_blocking(move || {
                        use crate::infra::platform::screenshot;
                        let meta = screenshot::session_meta()?;
                        if let Some(idx) = monitor_idx {
                            // 按显示器裁剪 BGRA
                            let displays = screenshot::list_displays();
                            let display = displays.get(idx)?;
                            let (bgra, w, h) = screenshot::crop_bgra_virtual(
                                display.x, display.y, display.w, display.h,
                            )?;
                            let offset_x = display.x - meta.virtual_x;
                            let offset_y = display.y - meta.virtual_y;
                            Some((bgra, w, h, offset_x, offset_y))
                        } else {
                            // 完整虚拟桌面 BGRA
                            let (bgra, w, h) = screenshot::crop_bgra_virtual(
                                meta.virtual_x, meta.virtual_y, meta.width, meta.height,
                            )?;
                            Some((bgra, w, h, 0, 0))
                        }
                    })
                    .await
                    .ok()
                    .flatten();

                    let response = match result {
                        Some((bgra, w, h, ox, oy)) => tauri::http::Response::builder()
                            .status(200)
                            .header("Content-Type", "application/octet-stream")
                            .header("Cache-Control", "no-store")
                            .header("Access-Control-Allow-Origin", "*")
                            .header("X-Width", w.to_string())
                            .header("X-Height", h.to_string())
                            .header("X-Offset-X", ox.to_string())
                            .header("X-Offset-Y", oy.to_string())
                            .header("X-Pixel-Format", "bgra")
                            .body(bgra)
                            .unwrap(),
                        None => {
                            tracing::warn!("blink-screenshot://raw: SESSION 为空,返回 404");
                            tauri::http::Response::builder()
                                .status(404)
                                .body(Vec::new())
                                .unwrap()
                        }
                    };
                    responder.respond(response);
                } else {
                    // 原有 PNG 路径（capture / editor）
                    let bytes = tauri::async_runtime::spawn_blocking(move || {
                        if editor_payload {
                            crate::infra::platform::image_editor::session_png()
                        } else {
                            crate::infra::platform::screenshot::session_png()
                        }
                    })
                    .await
                    .ok()
                    .flatten()
                    // M3 优化：session_png 返回 Arc<Vec<u8>>，此处转 owned Vec 供 HTTP body
                    .map(|arc| (*arc).clone());

                    let response = match bytes {
                        Some(bytes) => tauri::http::Response::builder()
                            .status(200)
                            .header("Content-Type", "image/png")
                            .header("Access-Control-Allow-Origin", "*")
                            .header("Cache-Control", "no-store")
                            .body(bytes)
                            .unwrap(),
                        None => {
                            tracing::warn!(editor_payload, "blink-screenshot: SESSION 为空,返回 404");
                            tauri::http::Response::builder()
                                .status(404)
                                .body(Vec::new())
                                .unwrap()
                        }
                    };
                    responder.respond(response);
                }
            });
        })
        // 自定义协议：http://blink-pin.localhost/{seq}（0.19.14）——
        // pin 窗口 <img src="blink-pin://{seq}"> 通过此协议拉取进程内 PNG bytes。
        // 替代 base64 data URL：消除 7.2MB PNG → 9.6MB base64 → WebView 解析巨型
        // data URL eval 的瓶颈。seq 由 `store_pin_image()` 递增分配，URL 不同 → 不缓存。
        .register_asynchronous_uri_scheme_protocol("blink-pin", |_ctx, request, responder| {
            let path = request.uri().path().trim_start_matches('/');
            let seq: u64 = match path.parse() {
                Ok(n) => n,
                Err(_) => {
                    responder.respond(
                        tauri::http::Response::builder()
                            .status(404)
                            .body(Vec::new())
                            .unwrap(),
                    );
                    return;
                }
            };
            tauri::async_runtime::spawn(async move {
                let result = tauri::async_runtime::spawn_blocking(move || {
                    crate::infra::platform::window::get_pin_image(seq)
                })
                .await
                .ok()
                .flatten();

                // P6: PinImage::Png 直接返回；PinImage::Bgra lazy 编码 PNG
                let bytes = match result {
                    Some(crate::infra::platform::window::PinImage::Png(arc)) => {
                        Some((*arc).clone())
                    }
                    Some(crate::infra::platform::window::PinImage::Bgra(arc, w, h)) => {
                        let bgra = (*arc).clone();
                        match crate::infra::platform::screenshot::encode_png(&bgra, w, h) {
                            Ok(png) => {
                                tracing::debug!(seq, w, h, "blink-pin: lazy PNG 编码完成");
                                Some(png)
                            }
                            Err(e) => {
                                tracing::error!(seq, error = %e, "blink-pin: lazy PNG 编码失败");
                                None
                            }
                        }
                    }
                    None => None,
                };

                let response = match bytes {
                    Some(bytes) => tauri::http::Response::builder()
                        .status(200)
                        .header("Content-Type", "image/png")
                        .header("Cache-Control", "no-store")
                        .body(bytes)
                        .unwrap(),
                    None => {
                        tracing::warn!(seq, "blink-pin: 未找到 pin 图片，返回 404");
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

            // 初始化 DB 四层拆分（0.12.0 §2.2）——config/history/ai/cache 各独立 pool
            let pools = tauri::async_runtime::block_on(infra::data::pools::init_all())
                .expect("failed to init db pools");

            // 0.16.4：注册全局 cache pool（blink-clipimg 协议懒加载缩略图用）
            infra::data::clipboard_images::set_pool(pools.cache.clone());

            // 初始化配置（配置库）
            let config_start = std::time::Instant::now();
            tauri::async_runtime::block_on(app::config::init_config(&pools.config))
                .expect("failed to init config");
            // 记录配置加载耗时（缓存库）
            let config_elapsed = config_start.elapsed().as_secs_f64() * 1000.0;
            tauri::async_runtime::block_on(infra::utils::perf::record_blocking(
                &pools.cache,
                infra::utils::perf::MetricCategory::Startup,
                "config_load",
                config_elapsed,
                None,
            ));

            // 读取应用配置(快照)（配置库）
            let app_config = tauri::async_runtime::block_on(app::config::get_config(&pools.config));

            // 日志级别 reload 到配置值（init 时为默认 error）
            infra::utils::logging::update_level(&app_config.log_level);
            // AI 详细日志开关 reload（0.12.6）
            infra::utils::logging::update_ai_verbose_log(app_config.ai_verbose_log);

            // pools 交给 Tauri 管理(command 层用 app.state::<DbPools>() 取)
            app.manage(pools.clone());

            // 0.14.6 §2.2：创建 DomainEnv 桥接器（TauriDomainEnv）
            // 各 service 构造后通过 set_* 注入（OnceLock 解决构造顺序倒挂）
            let domain_env = std::sync::Arc::new(app::domain_env::TauriDomainEnv::new(
                app.handle().clone(),
                pools.clone(),
            ));

            // 同步开机自启（确保注册表 Run 项与配置一致，覆盖用户在 app 外改动的情况）
            //
            // dev 模式跳过 enable：tauri-plugin-autostart 的 enable() 用 current_exe()
            // 决定注册哪个 exe，dev 下拿到的是 target\debug\blink.exe（PE console 子系统），
            // 写入 Run 键后开机会为它分配控制台窗口。dev 下改为主动 disable，清除可能残留
            // 的 debug exe 注册项；release 下正常按配置同步。配置 DB 仍记录 auto_start 偏好，
            // 下次跑 release 时同步逻辑会恢复注册表。
            {
                use tauri_plugin_autostart::ManagerExt;
                let manager = app.autolaunch();
                let _ = if cfg!(debug_assertions) {
                    manager.disable()
                } else if app_config.auto_start {
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

            // 0.17.3：首次启动弹出独立引导窗口（主窗口照常 hide）
            if app_config.first_run {
                infra::platform::window::show_welcome_window(app.handle());
            }

            // 托盘菜单：显示主窗口 + 设置 + 便签管理 + 关于 + 退出（按当前语言构建，运行时切语言会重建）
            let menu = app::tray::build_menu(app, &app_config.language)?;
            TrayIconBuilder::new()
                .icon(app.default_window_icon().unwrap().clone())
                .tooltip("Blink")
                .menu(&menu)
                .on_menu_event(|app, event| match event.id().as_ref() {
                    // 0.17.2：托盘菜单第一项"显示主窗口"，走 invoke 全流程拉起搜索框
                    "show_main" => app::window_orchestrator::invoke(app),
                    "settings" => open_settings(app),
                    "sticky_manager" => {
                        let _ = crate::infra::platform::window::show_sticky_manager_window(app);
                    }
                    // 0.18.4：托盘新增"AI 对话窗口"项，复用 chord Alt+Q 的 show_chat_window 链路
                    "chat_window" => {
                        let _ = crate::infra::platform::window::show_chat_window(app, None);
                    }
                    "about" => open_about(app),
                    // 0.19.17：输入钩子逃生舱——Alt+Space 失效时从此手动恢复
                    "recover_hook" => {
                        tracing::info!("托盘菜单：用户请求恢复输入钩子");
                        crate::infra::platform::hotkey::InputController::request_manual_recovery();
                    }
                    "quit" => app.exit(0),
                    _ => {}
                })
                // 0.17.2：托盘图标左键单击拉起主窗口（符合 Windows 惯例）
                .on_tray_icon_event(|tray, event| {
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } = event
                    {
                        app::window_orchestrator::invoke(tray.app_handle());
                    }
                })
                .build(app)?;

            // 加载 builtin 插件
            // 全局代理配置(engine:_global_proxy → {http,https})，进程启动时 env 注入，ureq/reqwest 原生读取
            let global_proxy = tauri::async_runtime::block_on(app::config::get_engine_config(&pools.config, "_global_proxy"));
            let proxy = global_proxy.and_then(|v| {
                let http = v.get("http").and_then(|s| s.as_str()).map(|s| s.to_string());
                let https = v.get("https").and_then(|s| s.as_str()).map(|s| s.to_string());
                if http.is_some() || https.is_some() { Some((http.unwrap_or_default(), https.unwrap_or_default())) }
                else { None }
            });
            let plugins_dir = if cfg!(debug_assertions) {
                domain::plugin::builtin_plugins_dir()
            } else {
                app.path()
                    .resource_dir()
                    .map(|d| d.join("plugins").join("builtin"))
                    .unwrap_or_else(|_| domain::plugin::builtin_plugins_dir())
            };
            let plugins = domain::plugin::load_builtin_plugins(&plugins_dir, proxy.clone());
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
            let plugin_engine = std::sync::Arc::new(domain::plugin::PluginEngine::new(plugins.clone(), pools.config.clone(), proxy));
            domain_env.set_plugin_engine(plugin_engine.clone());
            // 0.4→0.5 配置迁移（首次运行时执行一次，后续 marker 跳过；空 plugins 时循环空转）
            tauri::async_runtime::block_on(app::config::migrate_0_4_to_0_5(&pools.config, &plugins));
            // 0.9.5 camelCase→snake_case 迁移（前端重构统一字段命名，存量 DB 需改写）
            // 此函数读写 config 表，必须用配置库 pool
            tauri::async_runtime::block_on(infra::data::config::migrate_camelcase_to_snake(&pools.config));
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
            let start_menu_config = tauri::async_runtime::block_on(app::config::get_start_menu_config(&pools.config));
            let file_config = tauri::async_runtime::block_on(app::config::get_file_search_config(&pools.config));
            let calc_config = tauri::async_runtime::block_on(app::config::get_calc_config(&pools.config));
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
                domain_env.clone() as std::sync::Arc<dyn domain::event::EventPort>,
                pools.history.clone(),
                domain::search::build_engines(engine_configs, pools.history.clone(), pools.cache.clone()),
                plugin_engine.clone(),
                router.clone(),
                min_score_shared,
            ));
            domain_env.set_search_service(search_service.clone());
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
            // 初始化 ClipboardEngine 展示页数快照（0.20.1：display_pages 配置项）
            search_service.update_clipboard_display_pages(app_config.clipboard.display_pages);
            // 0.20.1: 同步 page_size 到 ClipboardEngine（effective_limit = display_pages × page_size）
            search_service.update_clipboard_page_size(app_config.page_size);
            // 启动后台清理（0.12.0 §2.2.4）：搜索历史 + 剪贴板历史 + AI 审计日志
            // 缓存库（performance_metrics / icon_cache）的清理在各自 init 时已 spawn。
            pools.spawn_startup_cleanup(infra::data::CleanupParams {
                search_history_enabled: app_config.search_history_enabled,
                search_history_days: app_config.search_history_days,
                clipboard_enabled: app_config.clipboard.enabled,
                clipboard_retention_days: app_config.clipboard.retention_days,
            });
            // 注入 ContextConfig 内存缓存：invoke 热键回调零 IO 读它（热更新见 update_context_config）
            let context_config = tauri::async_runtime::block_on(app::config::get_context_config(&pools.config));
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
                    if let Some(fg) = infra::platform::context::foreground_app()
                        && cfg.is_sensitive(&fg.process_name) {
                            tracing::debug!(app = %fg.process_name, "剪贴板变化 hook：前台敏感,跳过 snapshot 回写");
                            return;
                        }
                    ss.update_clipboard_text(Some(text.to_string()));
                    tracing::debug!(len = text.chars().count(), "剪贴板变化 → snapshot 已局部刷新");
                    // 通知主窗口用当前 query 重跑一次（刷 Context Ghost / AI 四筛子）。
                    // 只在主窗口可见时发——隐藏时窗口收不到 emit（Tauri 2 hidden webview
                    // drop event,见 [[tauri-hidden-webview-emit-dropped]]）,而且下一次
                    // invoke 会 collect() 重拍快照,不需要 push。
                    if infra::platform::window::is_visible()
                        && let Err(e) = app_handle.emit(EventNames::AWARENESS_UPDATED, ()) {
                            tracing::debug!(?e, "emit blink://awareness-updated 失败");
                        }
                }));
            }
            // RuleRouter 单独注册供设置页 API 用（triggers 热更新）
            app.manage(router.clone());

            // 0.8.5 Chord：构建 registry（注册 stub 动作）
            let chord_registry = std::sync::Arc::new(crate::domain::chord::build_default_registry());
            // 0.9.7 Capability 能力协议层（inventory 自动收集 5 个样板能力）
            let capability_registry = std::sync::Arc::new(crate::domain::capability::CapabilityRegistry::new());
            domain_env.set_cap_registry(capability_registry.clone());

            // 0.13.7:注册插件 tool 到 CapabilityRegistry——插件语义是「纯计算→返回结果」，
            // 天然属于 Capability 范畴（入参→出参，不碰 UI）。0.13.7 从旧 ActionRegistry 迁入。
            // 遍历所有已加载插件的 manifest.tools，为每个 tool 创建 PluginCapabilityAdapter 并注册。
            {
                let mut plugin_tool_count = 0usize;
                for plugin_handle in plugin_engine.all_plugins() {
                    let manifest = plugin_handle.manifest();
                    if manifest.tools.is_empty() {
                        continue;
                    }
                    for tool_def in &manifest.tools {
                        let adapter = crate::domain::plugin::PluginCapabilityAdapter::new(
                            plugin_handle.clone(),
                            tool_def,
                        );
                        if capability_registry.get(adapter.id()).is_some() {
                            tracing::warn!(
                                plugin = %manifest.id,
                                tool = %adapter.id(),
                                "插件 tool id 与已有 Capability 冲突,跳过"
                            );
                            continue;
                        }
                        let adapter_id = adapter.id().to_string();
                        tracing::info!(
                            plugin = %manifest.id,
                            tool = %adapter_id,
                            danger = ?tool_def.danger_class,
                            "注册插件 tool（Capability）"
                        );
                        // 0.21.13：register 返回 Result，插件注册前已用 get() 检查去重。
                        if let Err(e) = capability_registry.register(std::sync::Arc::new(adapter)) {
                            tracing::error!(
                                plugin = %manifest.id,
                                tool = %adapter_id,
                                error = %e,
                                "插件 tool 注册失败（id 冲突）"
                            );
                            continue;
                        }
                        plugin_tool_count += 1;
                    }
                }
                if plugin_tool_count > 0 {
                    tracing::info!(
                        count = plugin_tool_count,
                        total = capability_registry.len(),
                        "插件 tool 注册完成（CapabilityRegistry）"
                    );
                }
            }

            // 0.17.11: CM→keyring 密钥迁移（一次性,幂等）
            // 必须在 AIConfig 读取前执行——AIConfig 加载会触发密钥读取,需先迁到 keyring 新命名。
            tauri::async_runtime::block_on(
                crate::infra::platform::secret::migrate::migrate_legacy_cm_to_keyring(&pools.config),
            );

            // 0.9.2 Phase 5b:AIProviderRegistry 用 RigFactory 真接 rig-core。
            // AI 配置分片(第 7 分片,独立于 AppConfig 门面);默认 enabled=false,老用户零副作用。
            let ai_config = tauri::async_runtime::block_on(
                app::config::ConfigStore::get::<app::ai_config::AIConfig>(&pools.config),
            );
            // 0.12 §2.7: 初始化 AIConfig 内存缓存（供 CloudSttEngine 等非 async 上下文读取）
            app::ai_config::init_ai_cache(ai_config.clone());
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
            let mut stt_config = tauri::async_runtime::block_on(
                app::config::ConfigStore::get::<app::stt_config::SttConfig>(&pools.config),
            );
            // 启动期一次性迁移 STT 云端配置（0.12 cloud 引用模式 → 独立 cloud_provider 模式）
            if stt_config.apply_migration(&ai_config) {
                let _ = tauri::async_runtime::block_on(
                    app::config::ConfigStore::set::<app::stt_config::SttConfig>(
                        &pools.config,
                        &stt_config,
                    ),
                );
                tracing::info!("STT 云端配置迁移已持久化到配置库");
            }
            app::stt_config::init_cache(stt_config);

            let voice_service = std::sync::Arc::new(app::voice::VoiceService::new(app.handle().clone()));

            // 后台服务编排:按依赖拓扑顺序启动。
            // 0.8.6 §8.2.3：AppContext 持有全部核心服务引用（真依赖容器）。
            let ctx = app::service::AppContext {
                app: app.handle().clone(),
                pools: pools.clone(),
                config: app_config,
                ai_config: ai_config.clone(),
                search_service: search_service.clone(),
                plugin_engine: plugin_engine_for_ctx,
                router: router.clone(),
                chord_registry: chord_registry.clone(),
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
                &pools.cache,
                infra::utils::perf::MetricCategory::Startup,
                "services_init",
                svc_elapsed,
                None,
            ));
            // 记录启动总耗时（setup 在同步上下文，需用 block_on）
            let startup_total_ms = startup_start.elapsed().as_secs_f64() * 1000.0;
            tauri::async_runtime::block_on(infra::utils::perf::record_blocking(
                &pools.cache,
                infra::utils::perf::MetricCategory::Startup,
                "total",
                startup_total_ms,
                None,
            ));

            // 0.12.1 Phase 3B-1: chat AgentProvider 懒构造；memory 归 ChatService 所有。
            // 0.13.0: McpClientManager 管理 MCP server 子进程生命周期，传入 ChatService
            // 供 ensure_provider() 拉 MCP tool 进对话窗口 tool 池。
            // 0.17.8: PendingConfirms 使用 with_persistence，注入 config_pool + AiPermissionConfig。
            let ai_permission_config =
                tauri::async_runtime::block_on(
                    crate::domain::config::app_config::get_ai_permission_config(&pools.config),
                );
            let pending_confirms = std::sync::Arc::new(
                domain::ai::tool_adapter::PendingConfirms::with_persistence(
                    pools.config.clone(),
                    ai_permission_config,
                ),
            );
            let mcp_client = std::sync::Arc::new(domain::mcp::McpClientManager::new());
            let chat_service = std::sync::Arc::new(
                tauri::async_runtime::block_on(domain::ai::chat_service::ChatService::new(
                    domain_env.clone() as std::sync::Arc<dyn domain::event::EventPort>,
                    domain_env.clone() as std::sync::Arc<dyn domain::event::CapabilityEnv>,
                    // 0.21: AI tool 经 Registry 调 GUI starter 能力需真实 gui_surface 运行时
                    Some(
                        domain_env.clone()
                            as std::sync::Arc<dyn domain::capability::SurfacePort>,
                    ),
                    ai_registry.clone(),
                    capability_registry.clone(),
                    pending_confirms.clone(),
                    mcp_client.clone(),
                    pools.ai.clone(),
                    pools.config.clone(),
                )),
            );
            domain_env.set_chat_service(chat_service.clone());

            // 注册到 Tauri state（command 层 app.state 取用）
            app.manage(plugin_engine);
            app.manage(chord_registry);
            // 0.9.7 Capability 能力协议层（Step 4 起 AI tool 池消费）
            app.manage(capability_registry.clone());
            // 0.21.5: 首次启动生成 AI 推荐 allowlist（静默，不提示用户）
            tauri::async_runtime::block_on(
                crate::domain::config::ai_capability_access::AiCapabilityAccessStore::ensure_recommended(
                    &pools.config,
                    capability_registry.as_ref(),
                ),
            );
            // 0.21.10: 首次启动生成 MCP 默认暴露集合（无风险能力，静默）
            tauri::async_runtime::block_on(
                crate::domain::mcp::server_config::McpServerModeConfigStore::ensure_default_exposure(
                    &pools.config,
                    capability_registry.as_ref(),
                ),
            );
            // 0.9.1 Phase 5a：AI Provider registry(Phase 5b 起 SearchService 消费)
            app.manage(ai_registry);
            // 0.10: VoiceService(command 层 cancel_voice_recording / is_voice_recording 消费)
            app.manage(voice_service);
            // 0.12.0 §2.4: 对话窗口危险确认闭环（tool_adapter call 挂起 + confirm_chat_action 唤醒）
            app.manage(pending_confirms);
            app.manage(chat_service);
            // 0.13.0: MCP client 管理器（command 层 app.state 取用）
            app.manage(mcp_client.clone());
            // 0.14.6 §2.2：DomainEnv 注册为 managed state（command 层 app.state 取用）
            app.manage(domain_env);
            // 0.19.13: 保留 clone 供 McpServerRuntime 构造用
            let domain_env_for_mcp = app
                .state::<std::sync::Arc<app::domain_env::TauriDomainEnv>>()
                .inner()
                .clone();
            let cap_registry_for_mcp = app
                .state::<std::sync::Arc<domain::capability::CapabilityRegistry>>()
                .inner()
                .clone();

            // 0.13.3: 启动时扫描 Skill 目录（非阻塞，后台执行）
            {
                let ai_config = app::ai_config::get_ai_config();
                if ai_config.chat_config.skill_config.enabled {
                    let skill_config = ai_config.chat_config.skill_config;
                    let app_clone = app.handle().clone();
                    tauri::async_runtime::spawn(async move {
                        use tauri::Manager;
                        if let Some(cs) = app_clone
                            .try_state::<std::sync::Arc<domain::ai::chat_service::ChatService>>()
                        {
                            cs.apply_skill_config(&skill_config);
                            let count = cs.skill_registry().count();
                            if count > 0 {
                                tracing::info!(count, "启动时 Skill 扫描完成");
                            }
                        }
                    });
                }
            }

            // 后台预热次级窗口（1s 延迟，不阻塞启动；WebView2 冷启动 300~400ms → 预热后 show <50ms）
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

            // 0.13.7: MCP server 不在启动时自动拉起——改为 lazy connect。
            // 对话窗口首次可见后台预热；prompt 侧最多等待 5 秒并复用同一连接任务。
            // 设置页的「测试连接」按钮也可手动探测。

            // 持有服务列表,保证其生命周期与 app 一致。
            app.manage(services);

            // 0.16.3：内容编辑器 payload 暂存（open → get 中转）
            app.manage(app::commands::PendingEditorPayload::default());

            // 0.16.7：便签服务（domain 层，框架无关；command 层经 app.state 取用）
            let sticky_service = std::sync::Arc::new(
                domain::sticky::StickyService::new(pools.history.clone()),
            );
            app.manage(sticky_service.clone());

            // 0.21.14：窗口事件回调注册——infra 层通过 Tauri state 消费，
            // 不反向依赖 domain/app。
            // chat 窗口 CloseRequested → abort active request
            app.manage(app::ChatCloseCallback(std::sync::Arc::new(|app: &tauri::AppHandle| {
                use tauri::Manager;
                if let Some(cs) =
                    app.try_state::<std::sync::Arc<domain::ai::chat_service::ChatService>>()
                {
                    cs.abort_active();
                }
            })));
            // welcome 窗口 CloseRequested → first_run = false
            app.manage(app::WelcomeCloseCallback(std::sync::Arc::new(|app: &tauri::AppHandle| {
                use tauri::Manager;
                let pools = app.state::<crate::infra::data::DbPools>().inner().clone();
                tauri::async_runtime::spawn(async move {
                    let _ = app::config::update_first_run(&pools.config, false).await;
                    tracing::info!("welcome window: CloseRequested -> first_run = false");
                });
            })));
            // sticky spare close → trash_note + emit STICKY_TRASHED
            app.manage(app::StickySpareCloseCallback(std::sync::Arc::new(
                |app: &tauri::AppHandle, sticky_id: &str| {
                    use tauri::{Emitter, Manager};
                    let sticky_id = sticky_id.to_string();
                    let app = app.clone();
                    Box::pin(async move {
                        if let Some(svc) = app
                            .try_state::<std::sync::Arc<domain::sticky::StickyService>>()
                        {
                            if let Err(e) = svc.trash_note(&sticky_id).await {
                                tracing::warn!(error = %e, "预热便签关闭时移入回收站失败");
                            } else {
                                let _ = app.emit(
                                    domain::event_names::EventNames::STICKY_TRASHED,
                                    serde_json::json!({ "stickyId": sticky_id }),
                                );
                            }
                        }
                    })
                },
            )));

            // 0.16.13：便签恢复逻辑已提取为 StickyRecoveryService，纳入 all_services() 编排。
            // 见 src/app/service.rs::StickyRecoveryService。

            // 0.19.13: MCP Server Runtime——主进程 Streamable HTTP MCP Server 生命周期管理。
            // 在 CapabilityRegistry、EventPort/CapabilityEnv、pools.ai 都就绪后构造。
            // 启动时按配置自动启动 listener（如果 enabled=true）。
            let mcp_server_runtime = std::sync::Arc::new(
                app::mcp_server_runtime::McpServerRuntime::new(
                    cap_registry_for_mcp,
                    domain_env_for_mcp.clone() as std::sync::Arc<dyn domain::event::CapabilityEnv>,
                    domain_env_for_mcp.clone() as std::sync::Arc<dyn domain::event::EventPort>,
                    pools.ai.clone(),
                ),
            );
            app.manage(mcp_server_runtime.clone());

            // 后台加载配置并按需启动 listener（不阻塞启动流程）
            {
                let runtime = mcp_server_runtime.clone();
                let config_pool = pools.config.clone();
                tauri::async_runtime::spawn(async move {
                    let config = domain::mcp::McpServerModeConfigStore::load(&config_pool)
                        .await
                        .unwrap_or_default();
                    runtime.apply_config(&config).await;
                });
            }

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            app::commands::hide_window,
            app::commands::hide_settings_window,

            app::commands::search_apps,
            app::commands::launch_app,
            app::commands::run_builtin_action,
            app::commands::confirm_chat_action,
            app::commands::hide_chat_window,
            app::commands::take_chat_prefill,
            app::commands::ack_chat_prefill,
            app::commands::chat_prompt,
            app::commands::chat_abort,
            app::commands::clear_main_ai_active,
            app::commands::promote_ephemeral_conversation,
            app::commands::get_chat_status,
            app::commands::get_chat_models,
            app::commands::select_chat_model,
            // 0.17.9: 主窗口 AI 独立模型选择
            app::commands::get_ephemeral_models,
            app::commands::select_ephemeral_model,
            app::commands::start_chat_stt,
            app::commands::stop_chat_stt,
            app::commands::list_chat_conversations,
            app::commands::delete_chat_conversation,
            app::commands::rename_chat_conversation,
            app::commands::get_chat_messages,
            app::commands::get_conversation_system_prompt, // 0.12.7 §6.5
            app::commands::open_settings_tab,
            app::commands::save_text_file,
            app::commands::generate_conversation_title,
            app::commands::truncate_messages,
            app::commands::list_conversation_groups,
            app::commands::create_conversation_group,
            app::commands::rename_conversation_group,
            app::commands::delete_conversation_group,
            app::commands::update_conversation_group_system_prompt,
            app::commands::move_conversation_to_group,
            app::commands::set_group_sort_order,
            app::commands::set_group_expanded,
            app::commands::list_builtin_actions,
            app::commands::list_context_bindings,
            // 0.21.4：功能目录聚合与批量 binding 操作
            app::commands::list_feature_catalog,
            app::commands::apply_binding_ops,
            // 0.21.6：MCP 能力摘要
            app::commands::get_catalog_mcp_summary,
            app::commands::trigger_chord,
            app::commands::list_chord_actions,
            app::commands::list_all_chord_actions,
            app::commands::get_awareness_text,
            app::commands::register_main_input_view,
            app::commands::update_main_input_context,
app::commands::screenshot_copy,
app::commands::screenshot_copy_region,
app::commands::screenshot_copy_rgba,
            app::commands::screenshot_cancel,
            app::commands::screenshot_pin,
            app::commands::screenshot_pin_region,
            app::commands::screenshot_pin_hide,
            app::commands::screenshot_pin_transform,
            app::commands::screenshot_pin_move,
            app::commands::screenshot_pin_refresh,
            app::commands::screenshot_pin_get_rect,
            // 多 Pin N+1 + pin 保存
            app::commands::pin_spare_ready,
            app::commands::pin_save_clipboard,
            app::commands::pin_save_as,
            app::commands::screenshot_save,
            app::commands::image_editor_copy,
            app::commands::image_editor_pin,
            app::commands::image_editor_save,
            app::commands::image_editor_cancel,
            // 0.20.4：多来源图片编辑入口
            app::commands::open_image_editor_from_clipboard,
            app::commands::open_image_editor_from_history,
            app::commands::open_image_editor_from_pin,
            app::commands::screenshot_save_replay_file,
            app::commands::screenshot_set_annotation_mode,
            app::commands::screenshot_window_list,
            app::commands::screenshot_control_hints,
            app::commands::screenshot_set_capture_exclusion,
            app::commands::screenshot_capture_band,
            app::commands::screenshot_capture_probe,
            app::commands::screenshot_cursor_position,
            app::commands::screenshot_forward_wheel,
            app::commands::list_system_fonts,
            app::commands::ocr_image,
            app::commands::ocr_diagnose,
            app::commands::translate_text,
            app::commands::translate_lines,
            app::commands::analyze_palette,
app::commands::generate_palette_schemes,
            app::commands::hide_screenshot_overlay,
            app::commands::get_storage_info,
            app::commands::clear_history,
            app::commands::clear_ai_audit,
            app::commands::clear_all_conversations,
            app::commands::open_data_folder,
            app::commands::retry_migration,
            app::commands::clear_cache_db,
            app::commands::optimize_storage,
            app::commands::get_cleanup_info,
            app::commands::cleanup_all_data,
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
            app::commands::take_context_menu_payload,
            app::commands::probe_interpreters,
            app::commands::get_interpreter_paths,
            app::commands::open_file_dialog,
            app::commands::pick_directory_dialog,
app::commands::get_clipboard_history,
app::commands::search_clipboard,
app::commands::search_clipboard_history,
            app::commands::get_clipboard_text,
            app::commands::get_clipboard_text_batch,
            app::commands::record_clipboard_hit,
            app::commands::delete_clipboard_item,
            app::commands::delete_clipboard_image,
            app::commands::clear_clipboard_history,
            app::commands::clear_clipboard_images,
            // 0.17.8: AI 权限记忆管理
            app::commands::clear_all_permission_memory,
            app::commands::get_clipboard_stats,
            // 0.16.4 剪贴板图片
            app::commands::copy_clipboard_image,
            // 0.16.5 剪贴板图片 pin
            app::commands::pin_clipboard_image,
            // 0.16.3 内容编辑器
            app::commands::open_content_editor,
            app::commands::get_content_editor_payload,
            app::commands::save_content_editor,
            app::commands::get_perf_overview,
            app::commands::get_perf_percentiles,
            app::commands::get_perf_slow_queries,
            app::commands::get_perf_recent,
            app::commands::export_perf_report,
            app::commands::clear_perf_data,
app::commands::open_url,
// 0.18.6: 命令执行 MVP（`> ` 前缀调起终端）
app::commands::run_in_terminal,
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
            app::commands::save_stt_secret,
            app::commands::delete_stt_secret,
            app::commands::has_stt_secret,
            app::commands::get_stt_secret_hint,
            app::commands::get_stt_space_usage,
            app::commands::cleanup_stt_space,
            app::commands::open_stt_folder,
            app::commands::resize_voice_overlay,
            app::commands::get_default_hotkey,
            // 0.13.0 MCP client
            app::commands::list_mcp_servers,
            app::commands::upsert_mcp_server,
            app::commands::delete_mcp_server,
            app::commands::set_mcp_server_enabled,
            app::commands::start_mcp_server,
            app::commands::stop_mcp_server,
            app::commands::reconnect_mcp_server,
            app::commands::test_mcp_connection,
app::commands::ensure_mcp_connected,
            app::commands::get_mcp_server_tools,
            app::commands::set_mcp_server_disabled_tools,
            app::commands::get_mcp_tool_pool_size,
            app::commands::get_mcp_tool_names,
            // 0.13.6 MCP 导入增强
            app::commands::detect_mcp_config_file,
            app::commands::import_mcp_from_agent,
            app::commands::import_mcp_from_json,
            app::commands::batch_import_mcp_servers,
            app::commands::batch_set_mcp_enabled,
            // 0.13.6 上下文窗口状态
            app::commands::get_context_window_status,
            app::commands::compress_context_now,
            // 0.13.4 MCP server（暴露 Blink 能力）
            app::commands::get_mcp_server_config,
            app::commands::set_mcp_server_config,
            app::commands::list_exposable_capabilities,
            // 0.19.13 MCP server 运行时状态
            app::commands::get_mcp_server_runtime_status,
            // 0.13.3 Skill 约定式
            app::commands::list_skills,
            app::commands::refresh_skills,
            app::commands::open_skill_dir,
            // 0.13.6 Skill 导入 + 粒度开关
            app::commands::import_skill,
            // 0.13.7 外部来源枚举（导入面板用）
            app::commands::list_external_skill_sources,
            app::commands::set_skill_enabled,
            app::commands::open_dir_in_explorer,
            // 0.13.6 聊天窗口展现优化
            app::commands::get_mcp_tool_sources,
            // Composer bar 悬浮预览快照
            app::commands::get_composer_bar_snapshot,
            // 0.13.6 CLI 能力识别
            app::commands::recognize_cli_tool,
            // Skill 编辑/删除
            app::commands::save_skill_md,
            app::commands::get_skill_content,
            app::commands::delete_skill,
            // 0.16.7 便签
            app::commands::create_sticky_note,
            app::commands::get_sticky_note,
            app::commands::list_sticky_notes,
            app::commands::update_sticky_content,
            app::commands::update_sticky_appearance,
            app::commands::update_sticky_geometry,
            app::commands::set_sticky_visible,
            app::commands::set_sticky_always_on_top,
            app::commands::delete_sticky_note,
            app::commands::get_sticky_stats,
            app::commands::show_sticky_window_cmd,
            app::commands::destroy_sticky_window_cmd,
            app::commands::show_sticky_manager_cmd,
            // 0.17.7 便签回收站
            app::commands::trash_sticky_note,
            app::commands::restore_sticky_note,
            app::commands::list_trashed_sticky_notes,
            app::commands::clear_trashed_sticky_notes,
            // 0.20.0 便签原子关闭
            app::commands::close_sticky_note,
            // 0.18.3 便签预热
            app::commands::sticky_spare_ready,
            // P0-2 便签关闭 ack
            app::commands::sticky_close_ack,
            // 0.21.5: AI Capability Access
            app::commands::get_ai_capability_access,
            app::commands::toggle_ai_capability,
            app::commands::toggle_ai_capabilities,
            app::commands::reset_ai_capability_access,
        ])
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|_app, event| {
            if let tauri::RunEvent::Exit = event {
                // 0.16.11：标记应用正在退出——便签窗口的 CloseRequested handler
                // 据此跳过 set_visible(false)，保证退出不把所有便签写成 hidden
                crate::infra::platform::window::set_app_exiting();

                // 0.16.11：退出前 flush 所有便签窗口的未保存内容（防抖 500ms 内的编辑）
                let flushed = crate::infra::platform::window::flush_all_sticky_windows(_app);
                if flushed > 0 {
                    // P1-#16 fix: 增加等待时间到 500ms（匹配前端防抖间隔），
                    //   eval 是 fire-and-forget，只能 best-effort 等待
                    std::thread::sleep(std::time::Duration::from_millis(500));
                }

                // Blink 退出时 kill funasr-server 子进程，避免孤儿进程
                crate::app::commands::shutdown_funasr_server_blocking();
                // 0.13.0: 停止所有 MCP server 子进程
                if let Some(mcp) = _app.try_state::<std::sync::Arc<domain::mcp::McpClientManager>>() {
                    // P1-#16 fix: 加超时兜底，防止 mcp.stop_all() 无限挂住退出
                    tauri::async_runtime::block_on(async {
                        let _ = tokio::time::timeout(
                            std::time::Duration::from_secs(3),
                            mcp.stop_all(),
                        ).await;
                    });
                }

                // 0.19.13: 关闭 MCP Server HTTP listener，释放端口
                if let Some(runtime) = _app.try_state::<std::sync::Arc<app::mcp_server_runtime::McpServerRuntime>>() {
                    tauri::async_runtime::block_on(async {
                        let _ = tokio::time::timeout(
                            std::time::Duration::from_secs(3),
                            runtime.shutdown(),
                        ).await;
                    });
                }
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
