//! STT 真实双向切模 E2E（0.22.9 Handoff 08）。
//!
//! 隐藏内部 CLI 入口——不进入普通 CLI help / Capability / MCP。
//! 在**真实 blink.exe 生产路径**上验证跨 runtime 模型切换事务：
//!
//! ```text
//! 安装（GGUF 模型 + ParaformerOnline per-implementation deployment）
//!   → start GGUF（NDJSON ready 握手）
//!   → switch GGUF → ONNX（事务：stop → commit selected → start → Ready）
//!   → get_connection 投影 streaming port（二进制协议 v2）
//!   → 真流式 roundtrip（begin → push 静音 → finish → Final）
//!   → switch ONNX → GGUF（事务回程）
//!   → GGUF transport 就绪复核 → stop
//! ```
//!
//! ## 用法
//!
//! ```text
//! blink.exe stt-switch-e2e [--output <json 路径>] [--gguf-model <id>] [--skip-install]
//! ```
//!
//! 前置：GGUF 运行时环境已安装（设置页「引擎」安装过）；ONNX 模型与 GGUF
//! 模型未安装时由本命令通过真实安装事务下载（ONNX 约 250MB）。
//!
//! ## 设计铁则
//!
//! - 不改默认模型——结束时恢复初始 selected；
//! - 切换失败矩阵（未安装/回滚失败等）由单测覆盖，本命令只验证真实成功链路；
//! - 选取 ONNX deployment 的 model_generation_id 幂等——重复运行不重复下载。

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use crate::app::local_engine::manager::{SelectedModelStore, SwitchModelOutcome};
use crate::domain::stt::{StreamingSttPort, SttEvent};
use crate::infra::local_engine::runtime::EngineId;

// ── 参数解析 ─────────────────────────────────────────────────────────────

struct SwitchE2eArgs {
    output: Option<PathBuf>,
    gguf_model: String,
    skip_install: bool,
}

fn parse_args(args: &[String]) -> Result<SwitchE2eArgs, String> {
    let mut output = None;
    let mut gguf_model = crate::domain::config::stt_config::GGUF_SENSEVOICE_MODEL_ID.to_string();
    let mut skip_install = false;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--output" => {
                i += 1;
                output = args.get(i).map(PathBuf::from);
            }
            "--gguf-model" => {
                i += 1;
                gguf_model = args.get(i).cloned().ok_or("--gguf-model 缺少值")?;
            }
            "--skip-install" => skip_install = true,
            other => return Err(format!("未知参数: {other}")),
        }
        i += 1;
    }
    Ok(SwitchE2eArgs {
        output,
        gguf_model,
        skip_install,
    })
}

// ── CLI EventPort（状态/安装日志投影到 stdout）──────────────────────────────

struct CliEventPort;

impl crate::app::local_engine::manager::EventPort for CliEventPort {
    fn emit_status(&self, snapshot: &crate::domain::local_engine::EngineStatusSnapshot) {
        let s = &snapshot.status;
        eprintln!(
            "[status] engine={} rev={} desired={:?} process={:?} service={:?} model={:?} impl={:?}",
            snapshot.engine_id,
            snapshot.revision,
            s.desired,
            s.process,
            s.service,
            s.model,
            s.active_implementation,
        );
    }
    fn emit_log(
        &self,
        engine_id: &EngineId,
        instance_id: &str,
        _seq: u64,
        level: crate::app::local_engine::dto::EngineLogLevel,
        line: &str,
    ) {
        eprintln!("[log] {engine_id}/{instance_id} {level}: {line}");
    }
    fn emit_install_log(
        &self,
        engine_id: &EngineId,
        operation_id: &str,
        _seq: u64,
        level: crate::app::local_engine::dto::EngineLogLevel,
        text: &str,
    ) {
        eprintln!("[install] {engine_id}/{operation_id} {level}: {text}");
    }
    fn emit_install_stage(&self, engine_id: &EngineId, operation_id: &str, stage: &str) {
        eprintln!("[stage] {engine_id}/{operation_id}: {stage}");
    }
}

// ── CLI selected 存储（缓存读写；E2E 专用，不落 DB）─────────────────────────

struct CliSelectedStore;

#[async_trait::async_trait]
impl SelectedModelStore for CliSelectedStore {
    fn read_selected(&self) -> Option<String> {
        let m = crate::app::stt_config::get_stt_config()
            .local_engine
            .funasr_model;
        (!m.is_empty()).then_some(m)
    }

    async fn commit_selected(&self, model_id: &str) -> Result<(), String> {
        let mut config = crate::app::stt_config::get_stt_config();
        config.local_stt_selection = Some(crate::app::stt_config::LocalSttSelection::new(
            crate::app::stt_config::LocalSttSelection::FUNASR_ENGINE_ID,
            model_id,
        ));
        config.local_model_id = Some(model_id.to_string());
        config.local_engine.funasr_model = model_id.to_string();
        crate::app::stt_config::update_cache(&config);
        Ok(())
    }
}

// ── E2E 步骤 ─────────────────────────────────────────────────────────────

fn make_manager() -> Arc<crate::app::local_engine::EngineManager> {
    use crate::app::local_engine::EngineManager;
    use crate::app::local_engine::funasr::make_funasr_provider_descriptor;
    use crate::app::local_engine::model_installer::make_funasr_model_registry;
    use crate::app::local_engine::registry::EngineRegistry;

    let funasr_adapter = crate::app::local_engine::funasr::make_funasr_adapter();
    let paddleocr_adapter = crate::app::local_engine::paddleocr::make_paddleocr_adapter();
    let registry = Arc::new(EngineRegistry::new_with_adapters(vec![
        funasr_adapter,
        paddleocr_adapter,
    ]));
    let provider_descriptors = [(
        EngineId::new(crate::app::local_engine::funasr::FUNASR_ENGINE_ID).unwrap(),
        make_funasr_provider_descriptor(),
    )]
    .into_iter()
    .collect();

    let svc = EngineManager::new_with_providers(
        registry,
        Arc::new(CliEventPort),
        provider_descriptors,
        crate::infra::local_engine::providers::python::PythonVenvProvider::new(),
        make_funasr_model_registry(),
        Arc::new(crate::app::local_engine::funasr::FunasrGgufModelInstallWorker::new()),
    );
    svc.set_selected_store(Arc::new(CliSelectedStore));
    svc
}

/// 等待 Final 事件（消费方侧 generation 过滤；超时报错）。
async fn wait_final(
    mut rx: tokio::sync::mpsc::UnboundedReceiver<SttEvent>,
    generation: u64,
    timeout: Duration,
) -> Result<String, String> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let remaining = deadline
            .checked_duration_since(tokio::time::Instant::now())
            .ok_or_else(|| "等待 Final 超时".to_string())?;
        match tokio::time::timeout(remaining, rx.recv()).await {
            Ok(Some(SttEvent::Final {
                generation: g,
                text,
            })) => {
                if g == generation {
                    return Ok(text);
                }
                // 旧 generation 事件——继续等
            }
            Ok(Some(_)) => continue,
            Ok(None) => return Err("事件通道已关闭（worker 可能已退出）".to_string()),
            Err(_) => return Err("等待 Final 超时".to_string()),
        }
    }
}

/// 真流式 roundtrip：begin → 推 1s 静音（100ms × 10）→ finish → Final。
async fn streaming_roundtrip(port: &Arc<dyn StreamingSttPort>) -> Result<String, String> {
    let generation = port
        .begin_session()
        .await
        .map_err(|e| format!("begin_session 失败: {e}"))?;
    let rx = port.events();
    let silence = vec![0.0f32; 1600]; // 100ms @16kHz
    for _ in 0..10 {
        port.push_audio(generation, &silence)
            .await
            .map_err(|e| format!("push_audio 失败: {e}"))?;
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    port.finish_session(generation)
        .await
        .map_err(|e| format!("finish_session 失败: {e}"))?;
    wait_final(rx, generation, Duration::from_secs(20)).await
}

/// 判定模型是否已安装（GGUF = model_storage；ONNX = impl 部署空间）。
async fn model_installed(
    svc: &Arc<crate::app::local_engine::EngineManager>,
    engine_id: &EngineId,
    model_id: &str,
) -> bool {
    svc.get_model_status(engine_id, model_id)
        .await
        .map(|s| s.is_usable())
        .unwrap_or(false)
}

fn assert_step(cond: bool, step: &str, detail: &str) -> Result<(), String> {
    if cond {
        Ok(())
    } else {
        Err(format!("步骤 [{step}] 失败: {detail}"))
    }
}

async fn run_e2e(args: &SwitchE2eArgs) -> Result<serde_json::Value, String> {
    let engine_id = EngineId::new(crate::app::local_engine::funasr::FUNASR_ENGINE_ID).unwrap();
    let onnx_model = crate::app::local_engine::funasr::paraformer_online::PARAFORMER_ONLINE_ID;
    let gguf_model = args.gguf_model.clone();

    // 0. 初始化 SttConfig 缓存（Local 模式 + 启用；E2E 进程独立）
    crate::app::stt_config::init_cache(crate::app::stt_config::SttConfig {
        enabled: true,
        mode: crate::app::stt_config::SttMode::Local,
        local_stt_selection: Some(crate::app::stt_config::LocalSttSelection::new(
            crate::app::stt_config::LocalSttSelection::FUNASR_ENGINE_ID,
            gguf_model.clone(),
        )),
        local_engine: crate::app::stt_config::LocalEngineConfig {
            funasr_model: gguf_model.clone(),
            ..Default::default()
        },
        ..Default::default()
    });

    let svc = make_manager();
    let mut steps = Vec::new();

    // 1. 确保 GGUF 运行时环境就绪（bundled worker 事务安装；已装则跳过）
    let config = crate::app::local_engine::config_source::adapter_config_for_engine(&engine_id)
        .ok_or("无法构造 funasr AdapterConfig")?;
    svc.ensure_installed(&engine_id, config.clone())
        .await
        .map_err(|e| format!("步骤 [GGUF 环境安装] 失败: {e}"))?;
    steps.push(("ensure_gguf_environment", true));
    eprintln!("[e2e] GGUF 运行时环境就绪");

    // 2. 安装 GGUF 模型（真实下载，幂等）
    if !args.skip_install && !model_installed(&svc, &engine_id, &gguf_model).await {
        eprintln!("[e2e] 安装 GGUF 模型 {gguf_model}（真实下载）...");
        let r = svc
            .install_model(&engine_id, &gguf_model, None)
            .await
            .map_err(|e| e.to_string())?;
        assert_step(r.success, "install GGUF 模型", &format!("{r:?}"))?;
        steps.push(("install_gguf_model", true));
    } else {
        steps.push(("install_gguf_model(skipped-or-installed)", true));
    }

    // 3. start GGUF（生产 start：冻结 → NDJSON ready → Healthy）
    svc.start(&engine_id, config)
        .await
        .map_err(|e| format!("步骤 [start GGUF] 失败: {e}"))?;
    let status = svc
        .get_status(&engine_id)
        .await
        .map_err(|e| e.to_string())?;
    assert_step(
        status.status.model == crate::domain::local_engine::ModelHealth::Ready,
        "start GGUF",
        &format!("model={:?}", status.status.model),
    )?;
    assert_step(
        status.status.active_implementation
            == Some(crate::domain::local_engine::ImplementationId::FunasrGgufWorker),
        "start GGUF implementation 冻结",
        &format!("{:?}", status.status.active_implementation),
    )?;
    let conn = svc
        .get_connection(&engine_id)
        .await
        .map_err(|e| e.to_string())?
        .ok_or("GGUF 运行中无连接")?;
    assert_step(
        conn.worker.is_some() && conn.streaming.is_none(),
        "GGUF 连接投影",
        "应为 worker transport",
    )?;
    conn.worker
        .as_ref()
        .unwrap()
        .check_ready()
        .await
        .map_err(|e| format!("步骤 [GGUF transport check_ready] 失败: {e}"))?;
    steps.push(("start_gguf", true));
    eprintln!("[e2e] GGUF 已运行（implementation=FunasrGgufWorker）");

    // 3. 安装 ParaformerOnline（Paraformer provider + per-implementation deployment）
    if !args.skip_install && !model_installed(&svc, &engine_id, onnx_model).await {
        eprintln!("[e2e] 安装 ParaformerOnline（真实下载 ~250MB，幂等）...");
        let r = svc
            .install_model(&engine_id, onnx_model, None)
            .await
            .map_err(|e| e.to_string())?;
        assert_step(r.success, "install ParaformerOnline", &format!("{r:?}"))?;
        steps.push(("install_onnx_model", true));
    } else {
        steps.push(("install_onnx_model(skipped-or-installed)", true));
    }

    // 4. 切换 GGUF → ONNX（跨 runtime 事务）
    eprintln!("[e2e] 切换 {gguf_model} → {onnx_model} ...");
    let outcome = svc
        .switch_model(&engine_id, onnx_model)
        .await
        .map_err(|e| format!("步骤 [switch GGUF→ONNX] 失败: {e:?}"))?;
    assert_step(
        matches!(
            &outcome,
            SwitchModelOutcome::Completed {
                implementation: crate::domain::local_engine::ImplementationId::ParaformerOnnxWorker,
            }
        ),
        "switch GGUF→ONNX 结果",
        &format!("{outcome:?}"),
    )?;
    let status = svc
        .get_status(&engine_id)
        .await
        .map_err(|e| e.to_string())?;
    assert_step(
        status.status.model == crate::domain::local_engine::ModelHealth::Ready
            && status.status.active_implementation
                == Some(crate::domain::local_engine::ImplementationId::ParaformerOnnxWorker),
        "ONNX Ready 状态",
        &format!(
            "{:?}/{:?}",
            status.status.model, status.status.active_implementation
        ),
    )?;
    steps.push(("switch_gguf_to_onnx", true));
    eprintln!("[e2e] ONNX 已运行（implementation=ParaformerOnnxWorker）");

    // 5. streaming port 投影 + 真流式 roundtrip
    let conn = svc
        .get_connection(&engine_id)
        .await
        .map_err(|e| e.to_string())?
        .ok_or("ONNX 运行中无连接")?;
    assert_step(
        conn.streaming.is_some() && conn.worker.is_none(),
        "ONNX 连接投影",
        "应为 streaming port",
    )?;
    let port: Arc<dyn StreamingSttPort> = conn.streaming.clone().unwrap();
    let final_text = streaming_roundtrip(&port)
        .await
        .map_err(|e| format!("步骤 [ONNX 真流式 roundtrip] 失败: {e}"))?;
    steps.push(("onnx_streaming_roundtrip", true));
    eprintln!(
        "[e2e] ONNX 真流式 roundtrip 通过（Final 文本长度 {}）",
        final_text.chars().count()
    );

    // 6. 切换 ONNX → GGUF（回程事务）
    eprintln!("[e2e] 切换 {onnx_model} → {gguf_model} ...");
    let outcome = svc
        .switch_model(&engine_id, &gguf_model)
        .await
        .map_err(|e| format!("步骤 [switch ONNX→GGUF] 失败: {e:?}"))?;
    assert_step(
        matches!(
            &outcome,
            SwitchModelOutcome::Completed {
                implementation: crate::domain::local_engine::ImplementationId::FunasrGgufWorker,
            }
        ),
        "switch ONNX→GGUF 结果",
        &format!("{outcome:?}"),
    )?;
    steps.push(("switch_onnx_to_gguf", true));

    // 7. GGUF transport 复核
    let conn = svc
        .get_connection(&engine_id)
        .await
        .map_err(|e| e.to_string())?
        .ok_or("GGUF 回切后无连接")?;
    assert_step(
        conn.worker.is_some() && conn.streaming.is_none(),
        "GGUF 回切连接投影",
        "应为 worker transport",
    )?;
    conn.worker
        .as_ref()
        .unwrap()
        .check_ready()
        .await
        .map_err(|e| format!("步骤 [回切后 GGUF check_ready] 失败: {e}"))?;
    steps.push(("gguf_transport_ready_after_switch_back", true));
    eprintln!("[e2e] GGUF 回切通过（transport 就绪）");

    // 8. 收尾：恢复初始 selected（default = SenseVoice）并停止实例
    let _ = svc.stop(&engine_id).await;
    let status = svc
        .get_status(&engine_id)
        .await
        .map_err(|e| e.to_string())?;
    assert_step(
        status.status.desired == crate::domain::local_engine::DesiredState::Stopped
            && status.status.active_implementation.is_none(),
        "收尾停止",
        &format!(
            "{:?}/{:?}",
            status.status.desired, status.status.active_implementation
        ),
    )?;
    steps.push(("final_stop", true));

    Ok(serde_json::json!({
        "verdict": "PASS",
        "gguf_model": gguf_model,
        "onnx_model": onnx_model,
        "steps": steps,
        "onnx_final_text_chars": final_text.chars().count(),
    }))
}

/// CLI 入口。
pub fn run_from_args(args: &[String]) -> i32 {
    let parsed = match parse_args(args) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("stt-switch-e2e: 参数解析失败: {e}");
            eprintln!(
                "用法: blink.exe stt-switch-e2e [--output <json>] [--gguf-model <id>] [--skip-install]"
            );
            return 1;
        }
    };

    let _ = tracing_subscriber::fmt()
        .with_env_filter("info")
        .with_writer(std::io::stderr)
        .try_init();

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("stt-switch-e2e: 创建 tokio runtime 失败: {e}");
            return 1;
        }
    };

    let result = runtime.block_on(run_e2e(&parsed));
    match result {
        Ok(report) => {
            let text = serde_json::to_string_pretty(&report).unwrap_or_default();
            if let Some(path) = &parsed.output
                && let Err(e) = std::fs::write(path, text.clone())
            {
                eprintln!("stt-switch-e2e: 报告写入失败: {e}");
            }
            println!("{text}");
            println!("stt-switch-e2e: PASS（真实双向切模 E2E 通过）");
            0
        }
        Err(e) => {
            let report = serde_json::json!({ "verdict": "FAIL", "error": e });
            if let Some(path) = &parsed.output
                && let Err(e2) = std::fs::write(
                    path,
                    serde_json::to_string_pretty(&report).unwrap_or_default(),
                )
            {
                eprintln!("stt-switch-e2e: 报告写入失败: {e2}");
            }
            eprintln!("stt-switch-e2e: FAIL — {e}");
            1
        }
    }
}
