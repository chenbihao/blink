//! CLI 命令实现（0.13.5）——复用 domain/app 后端逻辑，不启动 GUI。
//!
//! ## 架构
//!
//! CLI 模式创建一个最小化的 Tauri app（无窗口、无托盘），仅初始化必要 state
//! （DbPools / CapabilityRegistry），然后执行 CLI 命令。
//!
//! 与 GUI 模式共享 `src/domain/` 和 `src/app/` 的全部后端逻辑。

use std::sync::Arc;

use tauri::Manager;

use crate::cli::{Cli, Commands, ConfigAction};

/// CLI 命令分发入口。
///
/// 创建最小化 Tauri app，初始化必要 state，执行命令，返回 exit code。
pub fn dispatch(cli: Cli) -> i32 {
    // 初始化日志（CLI 模式用 info 级别，方便排查）
    crate::infra::utils::logging::init("info");

    // Windows DPI 感知（与 GUI 模式一致，截图等能力需要）
    #[cfg(windows)]
    unsafe {
        use windows::Win32::UI::HiDpi::{
            DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext,
        };
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }

    // 构建最小化 Tauri app（无窗口、无托盘、无插件）
    let app = tauri::Builder::default()
        .setup(|app| {
            // 初始化 DB 四层拆分
            let pools = tauri::async_runtime::block_on(async {
                crate::infra::data::pools::init_all().await
            });

            match pools {
                Ok(p) => {
                    app.manage(p);
                    Ok(())
                }
                Err(e) => {
                    eprintln!("数据库初始化失败: {e}");
                    Err(e.into())
                }
            }
        })
        .build(tauri::generate_context!());

    let app = match app {
        Ok(app) => app,
        Err(e) => {
            eprintln!("应用初始化失败: {e}");
            return 1;
        }
    };

    // 初始化注册表
    let cap_registry = Arc::new(crate::domain::capability::CapabilityRegistry::new());

    // 0.14.6 §2.2：创建 DomainEnv 桥接器（CLI 模式最小化，仅注入 Capability）。
    let pools = app.state::<crate::infra::data::DbPools>().inner().clone();
    let domain_env = Arc::new(crate::app::domain_env::TauriDomainEnv::new(
        app.handle().clone(),
        pools,
    ));
    domain_env.set_cap_registry(cap_registry.clone());
    app.manage(domain_env);

    app.manage(cap_registry.clone());
    let handle = app.handle().clone();

    // 执行 CLI 命令
    let exit_code = match cli.command {
        Commands::McpServer => run_mcp_server(),
        Commands::Search { query, json } => run_search(&handle, &query, json),
        Commands::Run { capability, args } => {
            run_capability(&handle, &cap_registry, &capability, args)
        }
        Commands::Capabilities { json } => list_capabilities(&cap_registry, json),
        Commands::Config { action } => run_config(&handle, action),
        Commands::Chat {
            model,
            conversation,
        } => run_chat(&handle, model, conversation),
    };

    exit_code
}

/// `blink mcp-server` — 已迁移到主进程 Streamable HTTP（0.19.13）。
///
/// 旧 stdio 子进程路径已收口。执行时打印迁移指引并退出。
fn run_mcp_server() -> i32 {
    eprintln!(
        "Blink MCP Server 已迁移到主进程 Streamable HTTP（0.19.13）。\n\
         \n\
         旧 `blink mcp-server` stdio 子进程路径已停用。\n\
         MCP Server 现由 Blink 主进程托管，请在设置页「MCP Server」中启用。\n\
         连接地址：http://127.0.0.1:<port>/mcp（默认端口 32123）\n\
         \n\
         在外部 MCP 客户端配置中使用 Streamable HTTP URL 连接，例如：\n\
         {{\n\
           \"mcpServers\": {{\n\
             \"blink\": {{\n\
               \"url\": \"http://127.0.0.1:32123/mcp\"\n\
             }}\n\
           }}\n\
         }}"
    );
    1
}

/// `blink search <query>` — 搜索应用。
///
/// CLI 模式直接创建 `StartMenuEngine`（通过 `build_engines`），不走 `SearchService`
/// （`SearchService` 需要完整的路由 / 插件引擎初始化，CLI 场景太重）。
/// `StartMenuEngine::search` 内部会在缓存空时触发全量扫描，保证首次搜索也有结果。
fn run_search(handle: &tauri::AppHandle, query: &str, json: bool) -> i32 {
    use std::collections::HashMap;
    use tauri::Manager;

    use crate::domain::search::{EngineConfigs, QueryContext, build_engines};
    use crate::infra::platform::context::ContextSnapshot;

    let pools = handle.state::<crate::infra::data::DbPools>();

    let results = tauri::async_runtime::block_on(async {
        let engines = build_engines(
            EngineConfigs {
                start_menu: Default::default(),
                file: Default::default(),
                calc: Default::default(),
            },
            pools.history.clone(),
            pools.cache.clone(),
        );

        // 找到 start_menu 引擎并搜索
        let start_menu = engines.iter().find(|e| e.id() == "start_menu");
        match start_menu {
            Some(engine) => {
                let history = HashMap::new();
                let snapshot = ContextSnapshot::default();
                let disabled: Vec<String> = Vec::new();
                let ctx = QueryContext {
                    history: &history,
                    snapshot: &snapshot,
                    disabled_builtin_actions: &disabled,
                    disabled_context_bindings: &[],
                    language: "zh",
                };
                engine.search(query, &ctx).await
            }
            None => Vec::new(),
        }
    });

    if json {
        // SearchItem 未 impl Serialize，手动构建 JSON
        let json_items: Vec<_> = results
            .iter()
            .map(|item| {
                serde_json::json!({
                    "title": item.title,
                    "subtitle": item.subtitle,
                    "score": item.score,
                })
            })
            .collect();
        let json = serde_json::to_string_pretty(&json_items)
            .unwrap_or_else(|e| format!("序列化失败: {e}"));
        println!("{json}");
    } else {
        if results.is_empty() {
            println!("未找到匹配「{query}」的应用");
        } else {
            for (i, item) in results.iter().enumerate() {
                println!(
                    "{}. {} — {}",
                    i + 1,
                    item.title,
                    item.subtitle.as_deref().unwrap_or("")
                );
            }
        }
    }

    0
}

/// `blink run <capability> [--args JSON]` — 调用任意 Capability。
fn run_capability(
    handle: &tauri::AppHandle,
    cap_registry: &Arc<crate::domain::capability::CapabilityRegistry>,
    capability: &str,
    args: Option<String>,
) -> i32 {
    // 解析参数
    let args_value = match args {
        Some(json_str) => match serde_json::from_str(&json_str) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("参数 JSON 解析失败: {e}");
                return 1;
            }
        },
        None => serde_json::Value::Null,
    };

    // 构造 InvokeContext
    let env_arc = handle
        .state::<std::sync::Arc<crate::app::domain_env::TauriDomainEnv>>()
        .inner()
        .clone();
    let ctx = crate::domain::capability::InvokeContext {
        env: env_arc.as_ref(),
        deadline: None,
    };

    let result = tauri::async_runtime::block_on(cap_registry.invoke(capability, args_value, &ctx));

    // 0.17.10: 获取 capability 的 projection 规则，传给 to_display_text 做展示投影
    let projection = cap_registry
        .get(capability)
        .and_then(|cap| cap.projection());

    match result {
        Ok(result) => {
            // 0.14.1: 改调 canonical 文本投影（to_display_text），消除内联 match + Blob 摘要重复
            // 0.17.10: 传 projection 参数，展示出口动态挑字段
            println!("{}", result.to_display_text(projection.as_ref()));
            0
        }
        Err(e) => {
            eprintln!("Capability '{capability}' 调用失败: {e}");
            1
        }
    }
}

/// `blink capabilities [--json]` — 列出所有可用 Capability。
fn list_capabilities(
    cap_registry: &Arc<crate::domain::capability::CapabilityRegistry>,
    json: bool,
) -> i32 {
    let schemas = cap_registry.list();

    if json {
        let json =
            serde_json::to_string_pretty(&schemas).unwrap_or_else(|e| format!("序列化失败: {e}"));
        println!("{json}");
    } else {
        if schemas.is_empty() {
            println!("无已注册 Capability");
        } else {
            println!("可用 Capability（{} 个）：", schemas.len());
            println!();
            for s in &schemas {
                let sensitive_tag = if s.sensitive { " [sensitive]" } else { "" };
                println!("  {}{sensitive_tag}", s.name);
                println!("    {}", s.description);
                println!();
            }
        }
    }

    0
}

/// `blink config get/set` — 读写配置。
fn run_config(handle: &tauri::AppHandle, action: ConfigAction) -> i32 {
    use tauri::Manager;

    let pools = handle.state::<crate::infra::data::DbPools>();

    match action {
        ConfigAction::Get { key } => {
            let value = tauri::async_runtime::block_on(async {
                crate::infra::data::config::get_config(&pools.config, &key).await
            });

            match value {
                Some(v) => {
                    println!("{v}");
                    0
                }
                None => {
                    eprintln!("配置项 '{key}' 不存在");
                    1
                }
            }
        }
        ConfigAction::Set { key, value } => {
            let result = tauri::async_runtime::block_on(async {
                crate::infra::data::config::set_config(&pools.config, &key, &value).await
            });

            match result {
                Ok(()) => {
                    println!("✓ 已设置 {key}");
                    0
                }
                Err(e) => {
                    eprintln!("写入配置失败: {e}");
                    1
                }
            }
        }
    }
}

/// `blink chat [--model <id>] [--conversation <id>]` — 终端对话模式。
///
/// **当前限制**：CLI 模式不初始化 `ChatService`（需要完整的 AI Provider 基础设施——
/// `AIProviderRegistry` / `AgentProvider` / `PendingConfirms` 等），仅 GUI 模式可用。
/// 终端交互式对话体验留后续版本（需在 CLI 模式下初始化 AI Provider 子集）。
fn run_chat(
    _handle: &tauri::AppHandle,
    _model: Option<String>,
    _conversation: Option<String>,
) -> i32 {
    eprintln!("Blink Chat — 终端对话模式");
    eprintln!();
    eprintln!("⚠ 此功能目前仅 GUI 模式可用（需要 AI Provider 基础设施初始化）。");
    eprintln!("  请使用 Alt+Q 唤起对话窗口进行 AI 对话。");
    eprintln!();
    eprintln!("替代方案：");
    eprintln!("  blink run search_files --args '{{\"query\": \"关键词\"}}'   # 搜文件");
    eprintln!("  blink run read_clipboard                                     # 读剪贴板");
    eprintln!("  blink capabilities                                            # 列出所有能力");
    1
}
