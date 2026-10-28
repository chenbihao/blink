//! Blink 离线词典插件（0.28.4；0.28.13 线上化 + 客户端自举词库）——
//! 主窗快查 / 独立详情页 / AI·CLI·MCP 纯数据复用同一查询实现（dict.rs），
//! 词库数据集独立版本化（builder.rs 构建）。
//!
//! 运行形态：
//! - 无参数：JSONL stdio 服务（宿主拉起；manifest protocol_version 2，spawn 后
//!   先收 initialize 帧获得 datasets 目录，应答 initialized 后进业务帧循环）。
//! - `--build-basic / --build-ety / --verify / --check-quality`：词库构建与
//!   校验 CLI（xtask 与数据发布流程调用；不依赖宿主）。
//!
//! 数据集自举（0.28.13）：收到 `dataset_init` 后在**独立工作线程**执行
//! fetch 源 → 构建 → 逐文件校验 → 质量样本 → install_dataset → init_result；
//! 服务循环保持响应（期间查询如实返回 not_installed）。构建在插件进程内
//! ——宿主杀进程即有界终止，不存在 `--build-*` 孤儿子进程。
//!
//! 查询语义（phase §3.4）：
//! - 明确查词关键词走确定路由（"查词 apple" Takeover）；普通单词的混排/划词
//!   建议不能抢占应用搜索——本插件不注册 context 触发。
//! - 纯中文输入 → 基础汉英反查；英文输入 → 精确词头 → 词形还原 → 前缀 →
//!   拼写候选；未收录给显式转翻译入口（Route），不自动联网或 AI 补词源。
//! - 词源缺失如实显示（etymology 空）；发音后置。

mod builder;
mod dict;
mod protocol;

use dict::{BASIC_DATASET_ID, DictState, DictionaryService};
use protocol::{
    CoreToPlugin, PluginAction, PluginItem, PluginResponse, PluginToCore, RawToolResult,
};
use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        run_service();
        return;
    }
    let code = run_cli(&args);
    std::process::exit(code);
}

// ── JSONL 服务（宿主子进程）─────────────────────────────────────────────────

/// 服务上下文：查询即时应答；dataset_init 派发到工作线程。写出互斥共享
/// （服务循环与 init 工作线程都发帧），writer 参数化供测试捕获。
struct ServiceCtx<W: Write + Send + 'static> {
    service: Arc<DictionaryService>,
    out: Arc<Mutex<W>>,
    /// init 工作线程的请求应答路由（fetch_resource/install_dataset 的回包）。
    pending: Arc<Mutex<HashMap<String, std::sync::mpsc::Sender<WorkerReply>>>>,
    next_aux_id: Arc<AtomicU64>,
    /// 仅挂载交换期间独占；查询 try_read 即时返回 busy，不阻塞宿主回包。
    dataset_gate: Arc<RwLock<()>>,
}

/// 工作线程收到的宿主应答。
#[derive(Debug)]
enum WorkerReply {
    Resource(Result<ResourceOk, protocol::PluginError>),
    Dataset(Result<String, protocol::PluginError>),
}

/// fetch_resource 成功载荷。
#[derive(Debug, Clone)]
struct ResourceOk {
    path: String,
    revision: Option<String>,
    license: Option<String>,
}

fn run_service() {
    let ctx: ServiceCtx<std::io::BufWriter<std::io::Stdout>> = ServiceCtx {
        service: Arc::new(DictionaryService::new()),
        out: Arc::new(Mutex::new(std::io::BufWriter::new(std::io::stdout()))),
        pending: Arc::new(Mutex::new(HashMap::new())),
        next_aux_id: Arc::new(AtomicU64::new(1)),
        dataset_gate: Arc::new(RwLock::new(())),
    };
    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        handle_frame(&ctx, &line);
    }
}

/// 单帧分派（服务循环主体；测试 harness 直接驱动本函数）。
fn handle_frame<W: Write + Send + 'static>(ctx: &ServiceCtx<W>, line: &str) {
    let line = line.trim();
    if line.is_empty() {
        return;
    }
    let frame: CoreToPlugin = match serde_json::from_str(line) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("[dictionary][warn] 帧解析失败: {e}");
            return;
        }
    };
    match frame {
        CoreToPlugin::Initialize { protocol, dirs } => {
            if let Some(dirs) = dirs
                && let Some(datasets) = dirs.datasets
            {
                ctx.service.set_datasets_dir(PathBuf::from(&datasets));
                eprintln!("[dictionary][info] v2 握手：datasets = {datasets}（协议 {protocol}）");
            }
            write_frame_locked(&ctx.out, &PluginToCore::Initialized { protocol: 2 });
        }
        CoreToPlugin::Query {
            id,
            query,
            settings,
        } => {
            let items = match ctx.dataset_gate.try_read() {
                Ok(_permit) => handle_query(&ctx.service, &query, settings.as_ref()),
                Err(_) => vec![info_item("词库正在挂载", "请稍后重试")],
            };
            write_frame_locked(
                &ctx.out,
                &PluginToCore::Response(PluginResponse {
                    id,
                    items,
                    error: None,
                }),
            );
        }
        CoreToPlugin::ToolCall {
            id,
            tool_name,
            arguments,
            settings: _,
        } => {
            let result = match ctx.dataset_gate.try_read() {
                Ok(_permit) => handle_tool(&ctx.service, &id, &tool_name, &arguments),
                Err(_) => RawToolResult {
                    id,
                    data: serde_json::Value::Null,
                    error: Some(protocol::PluginError {
                        code: "busy".into(),
                        message: "词库正在挂载，请稍后重试".into(),
                    }),
                    desc: None,
                },
            };
            write_frame_locked(&ctx.out, &PluginToCore::RawResult(result));
        }
        CoreToPlugin::Cancel { .. } => {
            // 本地查询即时完成，无可取消阶段
        }
        CoreToPlugin::DatasetInit {
            op_id,
            dataset_id,
            work_dir,
            ..
        } => {
            eprintln!("[dictionary][info] 自举开始：dataset={dataset_id} op={op_id}");
            let worker: ServiceCtx<W> = ServiceCtx {
                service: Arc::clone(&ctx.service),
                out: Arc::clone(&ctx.out),
                pending: Arc::clone(&ctx.pending),
                next_aux_id: Arc::clone(&ctx.next_aux_id),
                dataset_gate: Arc::clone(&ctx.dataset_gate),
            };
            std::thread::spawn(move || {
                let error = run_init_worker(&worker, &op_id, &dataset_id, PathBuf::from(work_dir));
                if let Some((code, message)) = &error {
                    eprintln!(
                        "[dictionary][error] 自举失败 dataset={dataset_id}: {code} {message}"
                    );
                } else {
                    eprintln!("[dictionary][info] 自举完成 dataset={dataset_id}");
                }
                write_frame_locked(
                    &worker.out,
                    &PluginToCore::InitResult {
                        op_id,
                        dataset_id,
                        error: error.map(|(code, message)| protocol::PluginError { code, message }),
                    },
                );
            });
        }
        CoreToPlugin::ResourceResult {
            id,
            path,
            revision,
            license,
            error,
            ..
        } => {
            let reply = match error {
                Some(e) => Err(e),
                None => Ok(ResourceOk {
                    path: path.unwrap_or_default(),
                    revision,
                    license,
                }),
            };
            route_worker_reply(&ctx.pending, &id, move || WorkerReply::Resource(reply));
        }
        CoreToPlugin::DatasetResult { id, version, error } => {
            route_worker_reply(&ctx.pending, &id, move || {
                WorkerReply::Dataset(match (version, error) {
                    (Some(v), None) => Ok(v),
                    (_, Some(e)) => Err(e),
                    (None, None) => Err(protocol::PluginError {
                        code: "internal".into(),
                        message: "dataset_result 缺少 version".into(),
                    }),
                })
            });
        }
    }
}

/// 把宿主应答路由给等待的 init 工作线程（无等待者 = 丢弃并记日志）。
fn route_worker_reply(
    pending: &Arc<Mutex<HashMap<String, std::sync::mpsc::Sender<WorkerReply>>>>,
    id: &str,
    make: impl FnOnce() -> WorkerReply,
) {
    let sender = pending.lock().unwrap().remove(id);
    match sender {
        Some(tx) => {
            let _ = tx.send(make());
        }
        None => {
            eprintln!("[dictionary][warn] 孤儿应答（无等待的 init 请求）: {id}");
        }
    }
}

fn write_frame_locked<W: Write>(out: &Arc<Mutex<W>>, frame: &PluginToCore) {
    let mut guard = out.lock().unwrap();
    let _ = write_frame(&mut *guard, frame);
}

fn write_frame<W: Write>(out: &mut W, frame: &PluginToCore) -> std::io::Result<()> {
    serde_json::to_writer(&mut *out, frame).map_err(std::io::Error::other)?;
    out.write_all(b"\n")?;
    // BufWriter 不 flush 则帧滞留 8KB 缓冲，宿主读到超时（0.28.4 起 stdio
    // 全链路因此恒超时）。JSONL 协议按行应答，每帧必须立刻冲刷。
    out.flush()
}

// ── 数据集自举工作线程 ──────────────────────────────────────────────────────

/// 宿主资源应答等待上限：对齐宿主侧下载预算（进程存活是真正的上界）。
const HOST_REPLY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(50 * 60);
/// 质量样本内嵌（随二进制分发——安装目录无源码树，CLI 相对路径不可用）。
const QUALITY_SAMPLES: &str = include_str!("../quality-samples.json");

/// 自举一个数据集；Err = 结构化 (code, message) 进 init_result。
fn run_init_worker<W: Write + Send + 'static>(
    ctx: &ServiceCtx<W>,
    op_id: &str,
    dataset_id: &str,
    work_dir: PathBuf,
) -> Option<(String, String)> {
    let (resource_id, version): (&str, &str) = match dataset_id {
        dict::BASIC_DATASET_ID => ("ecdict-csv", builder::BASIC_DATASET_VERSION),
        dict::ETY_DATASET_ID => ("wordroot-txt", builder::ETY_DATASET_VERSION),
        other => {
            return Some((
                "unknown_dataset".into(),
                format!("词典插件不构建数据集 {other}"),
            ));
        }
    };

    // ① fetch 源（宿主按源锁定下载并校验哈希）。
    send_stage(ctx, op_id, "downloading-source", None);
    let res = match request_resource(ctx, op_id, resource_id) {
        Ok(res) => res,
        Err(e) => return Some(e),
    };
    // 宿主回传的必须是 work_dir 下的相对正规路径；join 后仍须落在 work_dir
    // 内（防绝对路径/`..` 越界读任意文件）。
    let rel = std::path::Path::new(&res.path);
    let source_path = work_dir.join(rel);
    if rel.is_absolute()
        || rel
            .components()
            .any(|c| !matches!(c, std::path::Component::Normal(_)))
        || !source_path.starts_with(&work_dir)
    {
        return Some((
            "bad_resource_path".into(),
            format!("宿主回传的资源路径越界: {}", res.path),
        ));
    }

    // ② 构建（分钟级 CPU/IO——专用线程内，服务循环不受阻）。
    send_stage(ctx, op_id, "building", Some(version));
    let build_root = work_dir.join("build");
    let package_dir = if dataset_id == dict::BASIC_DATASET_ID {
        let opts = builder::BuildOptions {
            profile: builder::Profile::Basic,
            version: version.to_string(),
            source_revision: res.revision.clone(),
            name: "Blink 离线词库（ECDICT）".into(),
        };
        match builder::build_basic_dataset(&source_path, &opts, &build_root) {
            Ok(out) => out.package_dir,
            Err(e) => return Some(("build_failed".into(), format!("基础词库构建失败: {e}"))),
        }
    } else {
        match builder::build_etymology_dataset(&source_path, version, &build_root) {
            Ok(out) => out.package_dir,
            Err(e) => return Some(("build_failed".into(), format!("词源包构建失败: {e}"))),
        }
    };

    // ③ 校验：逐文件哈希 + 内容摘要 + 质量样本（构建期质量门禁——上游漂移
    //    导致不达标时自举失败，不静默降质）。
    send_stage(ctx, op_id, "verifying", None);
    match builder::verify_dataset(&package_dir) {
        Ok(true) => {}
        Ok(false) => return Some(("verify_failed".into(), "数据包自校验未通过".into())),
        Err(e) => return Some(("verify_failed".into(), format!("数据包校验失败: {e}"))),
    }
    if dataset_id == dict::BASIC_DATASET_ID {
        let samples_path = work_dir.join("quality-samples.json");
        if let Err(e) = std::fs::write(&samples_path, QUALITY_SAMPLES) {
            return Some(("io".into(), format!("写入质量样本失败: {e}")));
        }
        if let Err(e) = builder::check_quality_samples(&package_dir, &samples_path) {
            return Some(("quality_failed".into(), format!("质量样本不达标: {e}")));
        }
    }

    // ④ 请求宿主挂载（受限相对路径）。
    // 等已开始的查询完成后关闭旧快照，期间新查询即时返回 busy。不能仅清缓存
    // 再发送请求：窗口查询可能在宿主交换目录前重新打开旧 SQLite 文件。
    let _mount_permit = ctx.dataset_gate.write().unwrap();
    ctx.service.release_snapshots();
    eprintln!("[dictionary][info] 挂载前已释放词库快照：{dataset_id}");
    send_stage(ctx, op_id, "installing", None);
    match request_install(
        ctx,
        op_id,
        dataset_id,
        &format!("build/{dataset_id}/{version}"),
    ) {
        Ok(mounted) => {
            eprintln!(
                "[dictionary][info] 数据集已挂载：{dataset_id} v{mounted}（源修订 {:?}，许可 {:?}）",
                res.revision, res.license
            );
            None
        }
        Err(e) => Some(e),
    }
}

fn send_stage<W: Write + Send + 'static>(
    ctx: &ServiceCtx<W>,
    op_id: &str,
    stage: &str,
    note: Option<&str>,
) {
    write_frame_locked(
        &ctx.out,
        &PluginToCore::InitStage {
            op_id: op_id.into(),
            stage: stage.into(),
            note: note.map(str::to_string),
        },
    );
}

/// fetch_resource → 等 resource_result。Err = (code, message)。
fn request_resource<W: Write + Send + 'static>(
    ctx: &ServiceCtx<W>,
    op_id: &str,
    resource_id: &str,
) -> Result<ResourceOk, (String, String)> {
    let id = format!("aux_{}", ctx.next_aux_id.fetch_add(1, Ordering::Relaxed));
    let (tx, rx) = std::sync::mpsc::channel::<WorkerReply>();
    ctx.pending.lock().unwrap().insert(id.clone(), tx);
    write_frame_locked(
        &ctx.out,
        &PluginToCore::FetchResource {
            id: id.clone(),
            op_id: op_id.into(),
            resource_id: resource_id.into(),
        },
    );
    let _guard = PendingGuard {
        pending: Arc::clone(&ctx.pending),
        id: id.clone(),
    };
    match rx.recv_timeout(HOST_REPLY_TIMEOUT) {
        Ok(WorkerReply::Resource(Ok(res))) => Ok(res),
        Ok(WorkerReply::Resource(Err(e))) => Err((e.code, e.message)),
        Ok(other) => Err((
            "internal".into(),
            format!("resource_result 路由错误: {other:?}"),
        )),
        Err(_) => Err(("resource_timeout".into(), "等待宿主下载源文件超时".into())),
    }
}

/// install_dataset → 等 dataset_result。Ok = 已挂载版本。
fn request_install<W: Write + Send + 'static>(
    ctx: &ServiceCtx<W>,
    op_id: &str,
    dataset_id: &str,
    source_dir: &str,
) -> Result<String, (String, String)> {
    let id = format!("aux_{}", ctx.next_aux_id.fetch_add(1, Ordering::Relaxed));
    let (tx, rx) = std::sync::mpsc::channel::<WorkerReply>();
    ctx.pending.lock().unwrap().insert(id.clone(), tx);
    write_frame_locked(
        &ctx.out,
        &PluginToCore::InstallDataset {
            id: id.clone(),
            op_id: op_id.into(),
            dataset_id: dataset_id.into(),
            source_dir: source_dir.into(),
        },
    );
    let _guard = PendingGuard {
        pending: Arc::clone(&ctx.pending),
        id: id.clone(),
    };
    match rx.recv_timeout(HOST_REPLY_TIMEOUT) {
        Ok(WorkerReply::Dataset(Ok(version))) => Ok(version),
        Ok(WorkerReply::Dataset(Err(e))) => Err((e.code, e.message)),
        Ok(other) => Err((
            "internal".into(),
            format!("dataset_result 路由错误: {other:?}"),
        )),
        Err(_) => Err(("install_timeout".into(), "等待宿主挂载超时".into())),
    }
}

/// 提前返回时清理 pending 注册（防孤儿条目累积）。
struct PendingGuard {
    pending: Arc<Mutex<HashMap<String, std::sync::mpsc::Sender<WorkerReply>>>>,
    id: String,
}
impl Drop for PendingGuard {
    fn drop(&mut self) {
        self.pending.lock().unwrap().remove(&self.id);
    }
}

// ── 查询（主窗快查 / tool-call）────────────────────────────────────────────

/// 主窗快查（Takeover）→ 条目列表。
fn handle_query(
    service: &DictionaryService,
    query: &str,
    settings: Option<&serde_json::Value>,
) -> Vec<PluginItem> {
    let show_en_def = settings
        .and_then(|s| s.get("show_english_definition"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let q = query.trim();

    match service.state() {
        DictState::NoDirs | DictState::NotInstalled => {
            return vec![info_item(
                "离线词库未就绪",
                "首次使用需下载源数据并构建（设置 → 插件 或 词典窗口内点击准备）",
            )];
        }
        DictState::Corrupt(reason) => {
            return vec![info_item(
                "离线词库数据损坏",
                &format!("{reason}；重新准备词库可修复"),
            )];
        }
        DictState::Ready => {}
    }

    // 纯中文（含 CJK）→ 基础汉英反查
    if q.chars().any(|c| ('\u{4e00}'..='\u{9fff}').contains(&c)) {
        return match service.reverse_zh(q) {
            Ok(hits) if !hits.is_empty() => hits
                .into_iter()
                .map(|h| PluginItem {
                    title: h.word.clone(),
                    subtitle: Some(truncate(&h.translation, 60)),
                    score: 0.9,
                    action: PluginAction::OpenPage {
                        page_id: None,
                        payload: Some(serde_json::json!({"word": h.word})),
                    },
                    payload: Some(serde_json::json!({"word": h.word, "translation": h.translation})),
                })
                .collect(),
            Ok(_) => vec![route_to_translate(q, "无对应词条")],
            Err(e) => vec![error_item(&e)],
        };
    }

    match service.lookup(q) {
        Ok((Some(entry), _)) => {
            let summary = service.summarize(&entry);
            let mut subtitle = entry
                .lemma_of
                .clone()
                .unwrap_or_else(|| entry.phonetic.clone());
            if show_en_def
                && let Some(first_def) = entry
                    .definition
                    .split("\\n")
                    .map(str::trim)
                    .find(|l| !l.is_empty())
            {
                subtitle.push_str(" · ");
                subtitle.push_str(&truncate(first_def, 40));
            }
            let mut items = vec![
                PluginItem {
                    title: format!("{} {}", entry.display, summary_word_marker(&summary)),
                    subtitle: Some(format!(
                        "{subtitle} | 按 Enter 打开详情 | {summary}"
                    )),
                    score: 0.95,
                    action: PluginAction::OpenPage {
                        page_id: None,
                        payload: Some(serde_json::json!({"word": entry.word})),
                    },
                    payload: Some(serde_json::to_value(&entry).unwrap_or_default()),
                },
                PluginItem {
                    title: format!("复制「{}」释义", entry.display),
                    subtitle: Some(truncate(&summary, 80)),
                    score: 0.6,
                    action: PluginAction::Copy { text: summary.clone() },
                    payload: Some(serde_json::json!({"word": entry.word, "copy": summary})),
                },
            ];
            if entry.lemma_of.is_none() && !entry.forms.is_empty() {
                let forms: Vec<String> = entry
                    .forms
                    .iter()
                    .map(|f| format!("{}（{}）", f.form, form_kind_zh(&f.kind)))
                    .collect();
                items.push(PluginItem {
                    title: format!("{} 词形：{}", entry.display, forms.join(" / ")),
                    subtitle: None,
                    score: 0.5,
                    action: PluginAction::None,
                    payload: None,
                });
            }
            items
        }
        Ok((None, candidates)) if !candidates.is_empty() => candidates
            .into_iter()
            .map(|c| PluginItem {
                title: c.word.clone(),
                subtitle: Some(format!(
                    "{} | {}",
                    if c.kind == "spell" { "拼写建议" } else { "前缀匹配" },
                    truncate(&c.translation, 50)
                )),
                score: 0.5,
                action: PluginAction::OpenPage {
                    page_id: None,
                    payload: Some(serde_json::json!({"word": c.word})),
                },
                payload: Some(serde_json::to_value(&c).unwrap_or_default()),
            })
            .collect(),
        // 英文未收录（无候选）→ 显式转翻译；原输入保留在输入框
        Ok((None, _)) => vec![route_to_translate(q, "未收录")],
        Err(e) => vec![error_item(&e)],
    }
}

fn handle_tool(
    service: &DictionaryService,
    id: &str,
    tool: &str,
    args: &serde_json::Value,
) -> RawToolResult {
    let tool = match tool {
        "dictionary_lookup" => Tool::Lookup,
        "dictionary_search_zh" => Tool::SearchZh,
        other => {
            return RawToolResult {
                id: id.into(),
                data: serde_json::Value::Null,
                error: Some(protocol::PluginError {
                    code: "unknown_tool".into(),
                    message: format!("未知 tool: {other}"),
                }),
                desc: None,
            };
        }
    };

    let input = match tool {
        Tool::Lookup => args
            .get("word")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string()),
        Tool::SearchZh => args
            .get("term")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string()),
    };
    let Some(input) = input else {
        return RawToolResult {
            id: id.into(),
            data: serde_json::Value::Null,
            error: Some(protocol::PluginError {
                code: "invalid_arguments".into(),
                message: "缺少查询参数（word / term）".into(),
            }),
            desc: None,
        };
    };

    match tool {
        Tool::Lookup => {
            let (data, desc) = match service.state() {
                DictState::NoDirs | DictState::NotInstalled => (
                    serde_json::json!({
                        "status": "not_installed",
                        "dataset": BASIC_DATASET_ID,
                        "word": input,
                    }),
                    "离线词库未就绪",
                ),
                DictState::Corrupt(reason) => (
                    serde_json::json!({
                        "status": "corrupt",
                        "dataset": BASIC_DATASET_ID,
                        "word": input,
                        "reason": reason,
                    }),
                    "离线词库损坏",
                ),
                DictState::Ready => match service.lookup(&input) {
                    Ok((Some(entry), _)) => (
                        serde_json::to_value(&entry).unwrap_or_default(),
                        "离线词典 · 已收录",
                    ),
                    Ok((None, candidates)) => (
                        serde_json::json!({
                            "status": "not_found",
                            "word": input,
                            "candidates": candidates,
                        }),
                        "离线词典 · 未收录",
                    ),
                    Err(e) => (
                        serde_json::json!({"status": "error", "word": input, "error": e}),
                        "离线词典 · 查询失败",
                    ),
                },
            };
            RawToolResult {
                id: id.into(),
                data,
                error: None,
                desc: Some(desc.into()),
            }
        }
        Tool::SearchZh => match service.reverse_zh(&input) {
            Ok(hits) => {
                let data = serde_json::to_value(&hits).unwrap_or_default();
                RawToolResult {
                    id: id.into(),
                    data,
                    error: None,
                    desc: Some(format!("汉英反查 · {} 条", hits.len())),
                }
            }
            Err(e) => RawToolResult {
                id: id.into(),
                data: serde_json::Value::Null,
                error: Some(protocol::PluginError {
                    code: "lookup_failed".into(),
                    message: e,
                }),
                desc: None,
            },
        },
    }
}

enum Tool {
    Lookup,
    SearchZh,
}

// ── 条目构造 helper ─────────────────────────────────────────────────────────

/// 未收录转翻译：Route 回写输入框（"翻译 <原文>"），原文不丢。
fn route_to_translate(input: &str, reason: &str) -> PluginItem {
    PluginItem {
        title: format!("「{input}」{reason}，转翻译"),
        subtitle: Some("按 Enter 以翻译关键词重查".into()),
        score: 0.4,
        action: PluginAction::Route {
            query: format!("翻译 {input}"),
        },
        payload: None,
    }
}

fn info_item(title: &str, subtitle: &str) -> PluginItem {
    PluginItem {
        title: title.into(),
        subtitle: (!subtitle.is_empty()).then(|| subtitle.into()),
        score: 0.3,
        action: PluginAction::None,
        payload: None,
    }
}

fn error_item(message: &str) -> PluginItem {
    PluginItem {
        title: format!("词典查询失败: {message}"),
        subtitle: None,
        // 负分 = 错误信息项（前端不可执行，排最后）
        score: -1.0,
        action: PluginAction::None,
        payload: None,
    }
}

/// 标题的释义摘要（截断到 24 字符，避免长释义挤爆主行）。
fn summary_word_marker(summary: &str) -> String {
    truncate(summary, 24)
}

fn truncate(s: &str, max_chars: usize) -> String {
    let s = s.split("\\n").map(str::trim).find(|l| !l.is_empty()).unwrap_or(s.trim());
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let cut: String = s.chars().take(max_chars).collect();
    format!("{cut}…")
}

fn form_kind_zh(kind: &str) -> &'static str {
    match kind {
        "p" => "过去式",
        "d" => "过去分词",
        "i" => "现在分词",
        "3" => "第三人称单数",
        "r" => "比较级",
        "t" => "最高级",
        "s" => "复数",
        _ => "词形",
    }
}

// ── 构建/校验 CLI（xtask 与数据发布流程调用）─────────────────────────────────

fn run_cli(args: &[String]) -> i32 {
    let flag_value = |flag: &str| -> Option<String> {
        args.iter()
            .position(|a| a == flag)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };

    if args.iter().any(|a| a == "--build-basic") {
        let Some(csv) = flag_value("--ecdict") else {
            eprintln!("缺少 --ecdict <ecdict.csv>");
            return 2;
        };
        let profile = flag_value("--profile")
            .and_then(|p| builder::Profile::parse(&p))
            .unwrap_or(builder::Profile::Basic);
        let version = flag_value("--version").unwrap_or_else(|| "1.0.0".into());
        let out = flag_value("--out").unwrap_or_else(|| "target/dictionary".into());
        let opts = builder::BuildOptions {
            profile,
            version: version.clone(),
            source_revision: flag_value("--source-revision"),
            name: flag_value("--name").unwrap_or_else(|| "Blink 离线词库（ECDICT）".into()),
        };
        return match builder::build_basic_dataset(&PathBuf::from(csv), &opts, &PathBuf::from(out))
        {
            Ok(output) => {
                println!(
                    "OK {} entries={} phrases={} forms={} zh_terms={} content_sha256={}",
                    output.package_dir.display(),
                    output.stats.entries,
                    output.stats.phrases,
                    output.stats.forms,
                    output.stats.zh_terms,
                    output.content_sha256
                );
                0
            }
            Err(e) => {
                eprintln!("FAIL {e}");
                1
            }
        };
    }

    if args.iter().any(|a| a == "--build-ety") {
        let Some(wordroot) = flag_value("--wordroot") else {
            eprintln!("缺少 --wordroot <wordroot.txt>");
            return 2;
        };
        let version = flag_value("--version").unwrap_or_else(|| "1.0.0".into());
        let out = flag_value("--out").unwrap_or_else(|| "target/dictionary".into());
        return match builder::build_etymology_dataset(
            &PathBuf::from(wordroot),
            &version,
            &PathBuf::from(out),
        ) {
            Ok(output) => {
                println!(
                    "OK {} roots={} words={} content_sha256={}",
                    output.package_dir.display(),
                    output.stats.entries,
                    output.stats.forms,
                    output.content_sha256
                );
                0
            }
            Err(e) => {
                eprintln!("FAIL {e}");
                1
            }
        };
    }

    if args.iter().any(|a| a == "--verify") {
        let Some(dir) = flag_value("--verify") else {
            eprintln!("缺少 --verify <package_dir>");
            return 2;
        };
        let dir = PathBuf::from(dir);
        return match builder::verify_dataset(&dir) {
            Ok(true) => {
                println!("OK 数据包校验通过: {}", dir.display());
                0
            }
            Ok(false) => {
                eprintln!("FAIL 数据包校验未通过: {}", dir.display());
                1
            }
            Err(e) => {
                eprintln!("FAIL {e}");
                1
            }
        };
    }

    if args.iter().any(|a| a == "--check-quality") {
        let Some(dir) = flag_value("--check-quality") else {
            eprintln!("缺少 --check-quality <package_dir>");
            return 2;
        };
        let samples = flag_value("--samples")
            .unwrap_or_else(|| "plugins/dictionary/quality-samples.json".into());
        return match builder::check_quality_samples(&PathBuf::from(dir), &PathBuf::from(samples)) {
            Ok(()) => {
                println!("OK 质量样本全部达标");
                0
            }
            Err(e) => {
                eprintln!("FAIL {e}");
                1
            }
        };
    }

    eprintln!(
        "用法：\n  （无参数）JSONL 服务模式\n  --build-basic --ecdict <csv> [--profile basic|full] [--version V] [--out DIR] [--source-revision R]\n  --build-ety --wordroot <txt> [--version V] [--out DIR]\n  --verify <package_dir>\n  --check-quality <package_dir> [--samples <json>]"
    );
    2
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 帧捕获 writer：服务与测试共享同一日志缓冲。
    #[derive(Clone, Default)]
    struct LogWriter(Arc<Mutex<Vec<u8>>>);

    impl Write for LogWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn harness(log: &LogWriter) -> ServiceCtx<LogWriter> {
        ServiceCtx {
            service: Arc::new(DictionaryService::new()),
            out: Arc::new(Mutex::new(log.clone())),
            pending: Arc::new(Mutex::new(HashMap::new())),
            next_aux_id: Arc::new(AtomicU64::new(1)),
            dataset_gate: Arc::new(RwLock::new(())),
        }
    }

    /// 轮询捕获日志直到出现满足条件的帧（30s 超时；构建需数秒）。
    fn wait_frame(log: &LogWriter, pred: impl Fn(&serde_json::Value) -> bool) -> serde_json::Value {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            let buf = log.0.lock().unwrap().clone();
            let text = String::from_utf8_lossy(&buf).into_owned();
            for line in text.lines() {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(line)
                    && pred(&v)
                {
                    return v;
                }
            }
            assert!(
                std::time::Instant::now() < deadline,
                "等待帧超时；已捕获:\n{text}"
            );
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }

    /// 覆盖 quality-samples.json 全部样本的 fixture 源（质量门禁真实跑过）。
    fn write_quality_fixture_csv(path: &std::path::Path) {
        builder::write_csv_fixture(
            path,
            &[
                &[
                    "apple",
                    "ˈæpl",
                    "n. fruit",
                    "n. 苹果, 家伙\\n[医] 苹果",
                    "n:100",
                    "0",
                    "1",
                    "zk gk",
                    "1000",
                    "900",
                    "s:apples",
                    "",
                    "",
                ],
                &[
                    "computer",
                    "kəmˈpjuːtə",
                    "n. device",
                    "n. 计算机, 电脑",
                    "n:100",
                    "2",
                    "1",
                    "zk",
                    "800",
                    "700",
                    "",
                    "",
                    "",
                ],
                &[
                    "book",
                    "bʊk",
                    "n. pages",
                    "n. 书, 书籍\\nv. 预订",
                    "n:100",
                    "3",
                    "1",
                    "zk gk",
                    "900",
                    "800",
                    "",
                    "",
                    "",
                ],
                &[
                    "run",
                    "rʌn",
                    "vi. move fast",
                    "vi. 跑, 奔跑\\nn. 奔跑, 路程\\nvi. 运转, 经营",
                    "vi:100",
                    "5",
                    "1",
                    "zk gk",
                    "1000",
                    "950",
                    "i:running/s:runs/3:runs",
                    "",
                    "",
                ],
                &[
                    "beautiful",
                    "ˈbjuːtɪfl",
                    "adj. pretty",
                    "adj. 美丽的, 美好的",
                    "adj:100",
                    "4",
                    "1",
                    "zk gk",
                    "700",
                    "600",
                    "",
                    "",
                    "",
                ],
                &[
                    "bank",
                    "bæŋk",
                    "n. finance",
                    "n. 银行, 河岸\\nvt. 存入银行",
                    "n:100",
                    "6",
                    "1",
                    "zk gk",
                    "900",
                    "850",
                    "",
                    "",
                    "",
                ],
                &[
                    "compiler",
                    "kəmˈpaɪlə",
                    "n. software",
                    "n. 编译器, 编译程序\\n[计] 编译程序",
                    "n:100",
                    "0",
                    "0",
                    "cet6",
                    "300",
                    "250",
                    "",
                    "",
                    "",
                ],
                &[
                    "good",
                    "ɡʊd",
                    "adj. fine",
                    "adj. 好的, 良好的",
                    "adj:100",
                    "7",
                    "1",
                    "zk gk",
                    "1000",
                    "980",
                    "r:better/t:best",
                    "",
                    "",
                ],
                &[
                    "better",
                    "ˈbetə",
                    "adj. comp of good",
                    "adj. 更好的",
                    "adj:100",
                    "0",
                    "0",
                    "",
                    "500",
                    "400",
                    "",
                    "",
                    "",
                ],
                &[
                    "box",
                    "bɒks",
                    "n. container",
                    "n. 箱, 盒子\\nvt. 装箱",
                    "n:100",
                    "8",
                    "1",
                    "zk gk",
                    "800",
                    "700",
                    "s:boxes",
                    "",
                    "",
                ],
                &[
                    "boxes",
                    "ˈbɒksɪz",
                    "n. pl of box",
                    "n. 箱子（复数）",
                    "n:100",
                    "0",
                    "0",
                    "",
                    "100",
                    "80",
                    "",
                    "",
                    "",
                ],
                &[
                    "black tea",
                    "",
                    "",
                    "n. 红茶",
                    "n:100",
                    "0",
                    "1",
                    "",
                    "200",
                    "150",
                    "",
                    "",
                    "",
                ],
                &[
                    "look forward to",
                    "",
                    "",
                    "盼望, 期望",
                    "",
                    "0",
                    "1",
                    "",
                    "200",
                    "150",
                    "",
                    "",
                    "",
                ],
                &[
                    "take care of",
                    "",
                    "",
                    "照顾, 照料",
                    "",
                    "0",
                    "1",
                    "",
                    "200",
                    "150",
                    "",
                    "",
                    "",
                ],
                &[
                    "async",
                    "ˈeɪsɪŋk",
                    "adj. computing",
                    "adj. 异步的",
                    "adj:100",
                    "0",
                    "0",
                    "",
                    "300",
                    "250",
                    "",
                    "",
                    "",
                ],
                &[
                    "deprecated",
                    "ˈdeprəkeɪtɪd",
                    "adj. disapproved",
                    "adj. 不赞成的, 反对的, 弃用的",
                    "adj:100",
                    "0",
                    "0",
                    "",
                    "200",
                    "150",
                    "",
                    "",
                    "",
                ],
                &[
                    "thread",
                    "θred",
                    "n. computing",
                    "n. 线, 线索\\n[计] 线程",
                    "n:100",
                    "9",
                    "1",
                    "cet4",
                    "700",
                    "600",
                    "",
                    "",
                    "",
                ],
                &[
                    "docker",
                    "ˈdɒkə",
                    "n. worker",
                    "n. 码头工人",
                    "n:100",
                    "0",
                    "0",
                    "",
                    "300",
                    "250",
                    "",
                    "",
                    "",
                ],
                &[
                    "lambda",
                    "ˈlæmdə",
                    "n. greek letter",
                    "n. 希腊字母 λ",
                    "n:100",
                    "0",
                    "0",
                    "gre",
                    "300",
                    "250",
                    "",
                    "",
                    "",
                ],
                &[
                    "go",
                    "ɡəʊ",
                    "",
                    "vi. 去",
                    "vi:100",
                    "5",
                    "1",
                    "zk",
                    "100",
                    "90",
                    "p:went/d:gone/i:going/3:goes",
                    "",
                    "",
                ],
                &[
                    "went",
                    "went",
                    "",
                    "go 的过去式",
                    "",
                    "",
                    "",
                    "",
                    "0",
                    "0",
                    "0:go/1:p",
                    "",
                    "",
                ],
            ],
        );
    }

    /// 模拟宿主驱动完整自举流：dataset_init → fetch_resource → resource_result
    /// （指向真实 fixture CSV）→ install_dataset → dataset_result → init_result(ok)。
    /// 逐帧断言：阶段上报、构建产物真实、逐文件校验与质量样本真实跑过。
    #[test]
    fn init_flow_fetches_builds_verifies_and_installs() {
        let tmp = tempfile::tempdir().unwrap();
        let tmp = tmp.keep();
        let work_dir = tmp.join("ops").join("boot-1");
        std::fs::create_dir_all(work_dir.join("resources")).unwrap();
        write_quality_fixture_csv(&work_dir.join("resources").join("ecdict-csv"));

        let log = LogWriter::default();
        let ctx = harness(&log);

        // 已查询过的同版本旧库：Windows 上不关闭 SQLite 就无法交换目录。
        let old = builder::build_basic_dataset(
            &work_dir.join("resources/ecdict-csv"),
            &builder::BuildOptions::default(), &tmp.join("old-build"),
        ).unwrap();
        let dataset_dir = tmp.join("datasets/dictionary-basic");
        let active_dir = dataset_dir.join(builder::BASIC_DATASET_VERSION);
        std::fs::create_dir_all(&dataset_dir).unwrap();
        std::fs::rename(&old.package_dir, &active_dir).unwrap();
        std::fs::write(dataset_dir.join("active.json"),
            serde_json::json!({"version": builder::BASIC_DATASET_VERSION}).to_string()).unwrap();
        {
            let conn = rusqlite::Connection::open(active_dir.join("dict.db")).unwrap();
            conn.execute("UPDATE entries SET translation = '旧快照' WHERE word = 'apple'", []).unwrap();
        }

        // 握手并打开旧快照。
        handle_frame(
            &ctx,
            &serde_json::json!({"type":"initialize","protocol":2,
                "dirs":{"datasets": tmp.join("datasets").to_string_lossy()}})
            .to_string(),
        );

        assert!(ctx.service.lookup("apple").unwrap().0.unwrap().translation.contains("旧快照"));

        // 构建期间继续查旧库；只有挂载交换的短暂阶段返回 busy。
        handle_frame(
            &ctx,
            &serde_json::json!({"type":"dataset_init","id":"init_1","op_id":"boot-1",
                "dataset_id":"dictionary-basic","work_dir": work_dir.to_string_lossy()})
            .to_string(),
        );
        let query = r#"{"type":"query","id":"q1","query":"apple"}"#;
        handle_frame(&ctx, query);
        let resp = wait_frame(&log, |v| v["type"] == "response" && v["id"] == "q1");
        assert!(
            resp["items"][0]["title"].as_str().unwrap().contains("apple"),
            "构建期间应仍可查旧快照: {resp}"
        );

        // 模拟宿主：等 fetch_resource → 回 resource_result 指向 fixture。
        let frame = wait_frame(&log, |v| v["type"] == "fetch_resource");
        assert_eq!(frame["resource_id"], "ecdict-csv");
        assert_eq!(frame["op_id"], "boot-1");
        let fetch_id = frame["id"].as_str().unwrap().to_string();
        handle_frame(
            &ctx,
            &serde_json::json!({"type":"resource_result","id": fetch_id,"op_id":"boot-1",
                "path":"resources/ecdict-csv","bytes":1,"sha256":"x",
                "revision":"8defb761","license":"MIT"})
            .to_string(),
        );

        // 等 install_dataset：构建产物必须真实存在且自校验通过。
        let frame = wait_frame(&log, |v| v["type"] == "install_dataset");
        assert_eq!(frame["dataset_id"], "dictionary-basic");
        let source_dir = frame["source_dir"].as_str().unwrap().to_string();
        let package_dir = work_dir.join(&source_dir);
        assert!(
            package_dir.join("dataset.json").is_file(),
            "构建输出应已生成"
        );
        assert!(package_dir.join("dict.db").is_file());
        assert!(
            builder::verify_dataset(&package_dir).unwrap(),
            "构建产物应通过逐文件校验"
        );
        let meta: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(package_dir.join("dataset.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(meta["version"], builder::BASIC_DATASET_VERSION);
        assert_eq!(
            meta["source"]["revision"], "8defb761",
            "源修订应写入 dataset.json"
        );

        // 宿主等待回包期间，新查询不能重开旧句柄，也不能堵住 stdin 回包处理。
        handle_frame(&ctx, r#"{"type":"query","id":"during-mount","query":"apple"}"#);
        let busy = wait_frame(&log, |v| v["id"] == "during-mount");
        assert!(busy["items"][0]["title"].as_str().unwrap().contains("挂载"));
        std::fs::rename(&active_dir, tmp.join("old-snapshot")).unwrap();
        std::fs::rename(&package_dir, &active_dir).unwrap();

        let install_id = frame["id"].as_str().unwrap().to_string();
        handle_frame(
            &ctx,
            &serde_json::json!({"type":"dataset_result","id": install_id,"op_id":"boot-1",
                "version": builder::BASIC_DATASET_VERSION})
            .to_string(),
        );

        // 终态 init_result(ok)。
        let frame = wait_frame(&log, |v| v["type"] == "init_result");
        assert!(frame["error"].is_null(), "自举应成功: {frame}");
        assert_eq!(frame["dataset_id"], "dictionary-basic");
        let entry = ctx.service.lookup("apple").unwrap().0.unwrap();
        assert!(entry.translation.contains("苹果"), "同版本挂载后必须读取新库");
        assert!(!entry.translation.contains("旧快照"));
    }

    /// fetch 失败（宿主回错误）→ init_result 携带结构化错误，不进入构建。
    #[test]
    fn init_flow_propagates_fetch_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let tmp = tmp.keep();
        let work_dir = tmp.join("ops").join("boot-2");
        std::fs::create_dir_all(work_dir.join("resources")).unwrap();

        let log = LogWriter::default();
        let ctx = harness(&log);
        handle_frame(
            &ctx,
            &serde_json::json!({"type":"dataset_init","id":"init_1","op_id":"boot-2",
                "dataset_id":"dictionary-basic","work_dir": work_dir.to_string_lossy()})
            .to_string(),
        );
        let frame = wait_frame(&log, |v| v["type"] == "fetch_resource");
        let fetch_id = frame["id"].as_str().unwrap().to_string();
        handle_frame(
            &ctx,
            &serde_json::json!({"type":"resource_result","id": fetch_id,"op_id":"boot-2",
                "error":{"code":"network_failed","message":"断网"}})
            .to_string(),
        );
        let frame = wait_frame(&log, |v| v["type"] == "init_result");
        assert_eq!(frame["error"]["code"], "network_failed");
        assert!(!work_dir.join("build").exists(), "fetch 失败不得进入构建");
    }

    #[test]
    fn concurrent_datasets_keep_distinct_requests_and_route_errors_independently() {
        let tmp = tempfile::tempdir().unwrap();
        let log = LogWriter::default();
        let ctx = harness(&log);
        for (op, dataset) in [
            ("basic", "dictionary-basic"),
            ("ety", "dictionary-etymology"),
        ] {
            handle_frame(
                &ctx,
                &serde_json::json!({
                    "type":"dataset_init", "id":op, "op_id":op,
                    "dataset_id":dataset, "work_dir":tmp.path().join(op)
                })
                .to_string(),
            );
        }
        let basic = wait_frame(&log, |v| {
            v["type"] == "fetch_resource" && v["op_id"] == "basic"
        });
        let ety = wait_frame(&log, |v| {
            v["type"] == "fetch_resource" && v["op_id"] == "ety"
        });
        assert_ne!(basic["id"], ety["id"]);
        // 倒序回包，两个 worker 必须各自收到所属错误，无覆盖或孤儿等待。
        for (frame, code) in [(ety, "ety_failed"), (basic, "basic_failed")] {
            handle_frame(
                &ctx,
                &serde_json::json!({
                    "type":"resource_result", "id":frame["id"], "op_id":frame["op_id"],
                    "error":{"code":code,"message":"fixture failure"}
                })
                .to_string(),
            );
            let result = wait_frame(&log, |v| {
                v["type"] == "init_result" && v["op_id"] == frame["op_id"]
            });
            assert_eq!(result["error"]["code"], code);
        }
        assert!(ctx.pending.lock().unwrap().is_empty());
    }

    /// 未知数据集：直接结构化失败。
    #[test]
    fn init_flow_rejects_unknown_dataset() {
        let tmp = tempfile::tempdir().unwrap();
        let tmp = tmp.keep();
        let work_dir = tmp.join("ops").join("boot-3");
        std::fs::create_dir_all(&work_dir).unwrap();

        let log = LogWriter::default();
        let ctx = harness(&log);
        handle_frame(
            &ctx,
            &serde_json::json!({"type":"dataset_init","id":"init_1","op_id":"boot-3",
                "dataset_id":"dictionary-other","work_dir": work_dir.to_string_lossy()})
            .to_string(),
        );
        let frame = wait_frame(&log, |v| v["type"] == "init_result");
        assert_eq!(frame["error"]["code"], "unknown_dataset");
    }
}
