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
    // 0.22.18：初始化有界审计队列
    tauri::async_runtime::block_on(async {
        crate::domain::capability::CapabilityRegistry::init_audit_writer();
    });

    // 0.14.6 §2.2：创建 DomainEnv 桥接器（CLI 模式最小化，仅注入 Capability）。
    let pools = app.state::<crate::infra::data::DbPools>().inner().clone();
    let domain_env = Arc::new(crate::app::domain_env::TauriDomainEnv::new(
        app.handle().clone(),
        pools.clone(),
    ));
    domain_env.set_cap_registry(cap_registry.clone());

    // 文件转写 CLI 构造最小 FunASR EngineManager：只使用已安装环境/模型，
    // 不安装、不下载，也不创建 GUI。其他 CLI 命令不承担本地引擎启动成本。
    let cli_engine_service = if matches!(&cli.command, Commands::TranscribeAudio { .. }) {
        let stt_config = tauri::async_runtime::block_on(crate::app::config::ConfigStore::get::<
            crate::app::stt_config::SttConfig,
        >(&pools.config));
        crate::app::stt_config::init_cache(stt_config);
        Some(build_cli_engine_manager())
    } else {
        None
    };

    if let Some(engine_service) = cli_engine_service.as_ref() {
        let audio_registry = Arc::new(crate::app::audio_resource::AudioResourceRegistry::default());
        let engine_conn: Arc<dyn crate::app::audio_transcription_service::EngineConnectionPort> =
            Arc::new(
                crate::app::audio_transcription_service::EngineConnectionAdapter::new(
                    engine_service.clone(),
                ),
            );
        let cloud_auth: Arc<dyn crate::app::audio_transcription_service::CloudEgressAuthorizer> =
            Arc::new(crate::app::audio_transcription_service::SttCloudEgressAuthorizer::new());
        let transcription_service = Arc::new(
            crate::app::audio_transcription_service::AudioTranscriptionService::new(
                audio_registry.clone(),
                engine_conn,
                cloud_auth,
                crate::app::audio_transcription_service::TranscriptionConfig::default(),
            ),
        );
        app.manage(audio_registry);
        domain_env.set_audio_transcription(
            transcription_service
                as Arc<dyn crate::domain::stt::transcribe::AudioTranscriptionPort>,
        );
        app.manage(engine_service.clone());
    }

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
        Commands::TranscribeAudio { path, json } => run_transcribe_audio(
            &handle,
            &cap_registry,
            &path,
            json,
            cli_engine_service.expect("transcribe CLI engine manager initialized"),
        ),
        // onnx-validate 在 try_run_cli 中被直接拦截，不走 clap 标准分派。
        // 此分支不可达——OnnxValidate 命令不会进入 dispatch。
        Commands::OnnxValidate { .. } => {
            eprintln!("onnx-validate 应通过 try_run_cli 直接分派，不应到达 dispatch");
            1
        }
    };
    tauri::async_runtime::block_on(
        crate::domain::capability::CapabilityRegistry::shutdown_audit_writer(),
    );
    exit_code
}

fn build_cli_engine_manager() -> Arc<crate::app::local_engine::EngineManager> {
    use crate::app::local_engine::model_installer::make_funasr_model_registry;
    use crate::app::local_engine::{EngineManager, EngineRegistry, NoopEventPort};

    let registry = Arc::new(EngineRegistry::new_with_adapters(vec![
        crate::app::local_engine::funasr::make_funasr_adapter(),
    ]));
    let descriptor = crate::app::local_engine::funasr::make_funasr_provider_descriptor();
    EngineManager::new_with_providers(
        registry,
        Arc::new(NoopEventPort),
        [(descriptor.engine_id.clone(), descriptor)]
            .into_iter()
            .collect(),
        make_funasr_model_registry(),
        Arc::new(crate::app::local_engine::funasr::FunasrGgufModelInstallWorker::new()),
    )
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
    // 0.21.0: 携带 origin=Cli + runtime（CLI 通过 Tauri handle 运行在主进程中）
    let env_arc = handle
        .state::<std::sync::Arc<crate::app::domain_env::TauriDomainEnv>>()
        .inner()
        .clone();
    let ctx = crate::domain::capability::InvokeContext {
        env: env_arc.as_ref(),
        origin: crate::domain::capability::InvocationOrigin::Cli,
        runtime: crate::domain::capability::RuntimeCapabilities {
            surface: None, // CLI 无 GUI surface（0.21.1+ 按需注入）
            main_process: true,
            desktop_session: true,
        },
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

/// `blink transcribe-audio <path> [--json]` — 转写本地音频文件（0.22.16 Handoff 06）。
///
/// **信任边界**：
/// - 在 CLI 信任边界验证用户显式 path（必须是绝对本地路径 + regular file）
/// - 拒绝 URL、file://、相对路径和不存在的文件
/// - 签发短期 audio_ref（scope = "stt_transcribe"）
/// - 构造 `InvocationOrigin::Cli`
/// - 调用 `CapabilityRegistry::invoke("transcribe_audio", { audio_ref })` —— 走同一原子执行语义
/// - 输出正文和简短 engine/model identity
/// - JSON 模式保留完整 canonical data
///
/// 不在 generic `run_capability()` 中按 capability id 加隐藏 path 特例。
fn run_transcribe_audio(
    handle: &tauri::AppHandle,
    cap_registry: &Arc<crate::domain::capability::CapabilityRegistry>,
    path: &str,
    json: bool,
    engine_service: Arc<crate::app::local_engine::EngineManager>,
) -> i32 {
    use tauri::Manager;

    // 1. 信任边界：拒绝 URL 和非本地路径
    if path.starts_with("http://")
        || path.starts_with("https://")
        || path.starts_with("file://")
        || path.starts_with("ftp://")
    {
        eprintln!("错误：不接受 URL 或远程路径");
        return 1;
    }

    let file_path = std::path::Path::new(path);

    // 2. 验证绝对路径
    if !file_path.is_absolute() {
        eprintln!("错误：路径必须是绝对本地路径");
        return 1;
    }

    // 3. 验证文件存在且是 regular file
    if !file_path.exists() {
        eprintln!("错误：文件不存在");
        return 1;
    }

    let metadata = match std::fs::metadata(file_path) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("错误：无法读取文件元数据: {e}");
            return 1;
        }
    };

    if !metadata.is_file() {
        eprintln!("错误：路径不是常规文件");
        return 1;
    }

    // 已有 GUI/其他 CLI worker 时不启动第二个实例。stdio pipe 不能跨进程接管，
    // 因此给出明确错误，保护“同一时刻单 worker”铁则。
    if crate::app::local_engine::funasr::corpus_runner::count_orphan_workers() > 0 {
        eprintln!(
            "错误：检测到另一个 FunASR worker 正在运行，请先结束当前语音任务或退出 Blink 后重试"
        );
        return 1;
    }

    let engine_id = match crate::infra::local_engine::runtime::EngineId::new(
        crate::app::local_engine::funasr::FUNASR_ENGINE_ID,
    ) {
        Ok(id) => id,
        Err(error) => {
            eprintln!("错误：本地 STT 引擎配置无效: {error}");
            return 1;
        }
    };
    let stt_config = crate::app::stt_config::get_stt_config();
    if stt_config.mode != crate::app::stt_config::SttMode::Local {
        eprintln!("错误：CLI 文件转写要求语音设置为本地 STT 模式；云端音频外发必须走交互式授权");
        return 1;
    }
    let Some(selection) = stt_config.local_stt_selection.as_ref() else {
        eprintln!("错误：尚未配置本地 STT 模型，请先在设置页选择已安装模型");
        return 1;
    };
    if selection.engine_id != engine_id.as_str() {
        eprintln!("错误：CLI 文件转写当前仅支持已安装的 FunASR 本地模型");
        return 1;
    }

    // 4. 获取 AudioResourceRegistry 并签发 audio_ref
    let audio_registry = handle
        .state::<std::sync::Arc<crate::app::audio_resource::AudioResourceRegistry>>()
        .inner()
        .clone();

    let audio_ref = match audio_registry.issue(file_path, "stt_transcribe") {
        Ok(ref_) => ref_,
        Err(e) => {
            eprintln!("错误：无法签发音频引用: {e}");
            return 1;
        }
    };

    let start_result = tauri::async_runtime::block_on(async {
        use crate::domain::local_engine::ModelInstallState;

        let installed = engine_service
            .list_models(&engine_id)
            .await
            .into_iter()
            .any(|model| {
                model.model_id == selection.model_id
                    && model.install_state == ModelInstallState::Installed
            });
        if !installed {
            return Err("所选 STT 模型尚未安装；CLI 不会自动下载模型".to_string());
        }
        let mut config = crate::app::local_engine::config_source::funasr_adapter_config();
        config.engine_config["funasr_model"] =
            serde_json::Value::String(selection.model_id.clone());
        engine_service
            .start(&engine_id, config)
            .await
            .map_err(|error| error.to_string())
    });
    if let Err(error) = start_result {
        eprintln!("错误：无法启动本地 STT 引擎: {error}");
        return 1;
    }

    // 5. 构造 InvokeContext（origin = Cli）
    let env_arc = handle
        .state::<std::sync::Arc<crate::app::domain_env::TauriDomainEnv>>()
        .inner()
        .clone();
    let ctx = crate::domain::capability::InvokeContext {
        env: env_arc.as_ref(),
        origin: crate::domain::capability::InvocationOrigin::Cli,
        runtime: crate::domain::capability::RuntimeCapabilities {
            surface: None,
            main_process: true,
            desktop_session: true,
        },
        deadline: None,
    };

    // 6. 调用 transcribe_audio Capability —— 走 Registry 唯一原子执行入口
    let args = serde_json::json!({ "audio_ref": audio_ref });
    let result =
        tauri::async_runtime::block_on(cap_registry.invoke("transcribe_audio", args, &ctx));

    // 7. 获取 projection（transcribe_audio 无 manifest projection，返回 None）
    let projection = cap_registry
        .get("transcribe_audio")
        .and_then(|cap| cap.projection());

    let exit_code = match result {
        Ok(cap_result) => {
            if json {
                // JSON 模式：输出完整 canonical data
                let json_str = serde_json::to_string_pretty(&cap_result)
                    .unwrap_or_else(|e| format!("序列化失败: {e}"));
                println!("{json_str}");
            } else {
                // 文本模式：输出正文和简短 identity
                let display = cap_result.to_display_text(projection.as_ref());
                println!("{display}");
            }
            0
        }
        Err(e) => {
            eprintln!("转写失败: {e}");
            1
        }
    };

    if let Err(error) = tauri::async_runtime::block_on(engine_service.stop(&engine_id)) {
        eprintln!("错误：转写完成后停止本地 STT 引擎失败: {error}");
        return 1;
    }
    exit_code
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
