//! 插件进程管理(见 §3.1):懒启动 + 常驻复用,stdio JSONL 往返。
//!
//! - 命中 trigger 才 spawn;进程复用,崩溃则下次重启。
//! - 三路并发:stdout reader task(按 request id 路由到 pending oneshot)+ stderr
//!   reader task(汇入 tracing)+ stdin 写入(互斥)。不读 stdout/stderr 会因 pipe
//!   写满而死锁,故必须各起一个 reader task。
//! - 查询用 tokio::time::timeout 兜底;超时清理 pending,不 kill 进程(下次复用)。
//! - Windows:CREATE_NO_WINDOW 防控制台子进程弹窗。

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin};
use tokio::sync::{Mutex, Semaphore, mpsc, oneshot};

use super::manifest::{PluginManifest, RuntimeType};
use super::protocol::{
    PluginAction, PluginErrorPayload, PluginItem, PluginRequest, PluginResponse,
    PluginUpstreamMessage, RawToolResult,
};

/// 查询错误。
#[derive(Debug, thiserror::Error)]
pub enum PluginError {
    /// 进程拉起失败。
    #[error("spawn 失败: {0}")]
    Spawn(String),
    /// 托管脚本运行时未安装（0.25.20：Python/Node 解释器改 Blink 托管分发，
    /// 不再探测系统 PATH——缺失时引导用户到设置页安装）。
    #[error("脚本运行时（{kind}）未安装")]
    InterpreterNotInstalled { kind: &'static str },
    /// 进程已关闭(stdout EOF / 写 stdin 失败)。
    #[error("进程已关闭")]
    ProcessClosed,
    /// 查询超时。
    #[error("查询超时")]
    Timeout,
    /// 插件返回 error。
    #[error("插件返回错误 ({code}): {message}", code = .0.code, message = .0.message)]
    PluginReturned(PluginErrorPayload),
    #[error("插件忙，请稍后重试")]
    Busy,
    /// 协议/IO 错误。
    #[error("IO 错误: {0}")]
    Io(String),
}

type RequestMap<T> = Arc<std::sync::Mutex<HashMap<String, oneshot::Sender<T>>>>;
type PendingMap = RequestMap<PluginResponse>;
/// 轨道 A 纯数据 tool-call 的 pending map（0.14.3）。
type PendingRawToolMap = RequestMap<RawToolResult>;

struct PluginStdin {
    pipe: Mutex<ChildStdin>,
    valid: AtomicBool,
}

/// stdin 写到半行时被 deadline/drop 打断，后续 JSONL 不能接在残帧之后。
/// 标记此进程不可复用；下一次调用重新拉起，旧代次等待者仍由各自 deadline 兜底。
struct FrameGuard<'a> {
    valid: &'a AtomicBool,
    completed: bool,
}
impl Drop for FrameGuard<'_> {
    fn drop(&mut self) {
        if !self.completed {
            self.valid.store(false, Ordering::Release);
        }
    }
}

async fn write_frame<W: AsyncWrite + Unpin>(
    writer: &mut W,
    valid: &AtomicBool,
    line: String,
) -> Result<(), PluginError> {
    if !valid.load(Ordering::Acquire) {
        return Err(PluginError::ProcessClosed);
    }
    let mut frame = FrameGuard {
        valid,
        completed: false,
    };
    writer
        .write_all((line + "\n").as_bytes())
        .await
        .map_err(|_| PluginError::ProcessClosed)?;
    writer
        .flush()
        .await
        .map_err(|_| PluginError::ProcessClosed)?;
    frame.completed = true;
    Ok(())
}

/// Drop 同步清理自身 pending（包含调用方取消 future），取消消息经有界队列尽力写入。
/// map 与队列均属于原进程实例，旧请求不会误清重启后的新实例。
struct PendingGuard<T> {
    requests: RequestMap<T>,
    id: String,
    cancel: mpsc::Sender<String>,
}

impl<T> Drop for PendingGuard<T> {
    fn drop(&mut self) {
        if self.requests.lock().unwrap().remove(&self.id).is_some() {
            let _ = self.cancel.try_send(self.id.clone());
        }
    }
}

fn remaining_budget(deadline: tokio::time::Instant) -> Result<u64, PluginError> {
    let ms = deadline
        .saturating_duration_since(tokio::time::Instant::now())
        .as_millis() as u64;
    if ms == 0 {
        Err(PluginError::Timeout)
    } else {
        Ok(ms)
    }
}

/// 已拉起的插件进程。
struct PluginProcess {
    /// `Mutex<Child>` 使存活检测可 `&self`(try_wait 需 `&mut`),从而 query 能在
    /// process 锁外并发执行——多个 in-flight query 各持一份 `Arc<PluginProcess>`。
    child: Mutex<Child>,
    stdin: Arc<PluginStdin>,
    pending: PendingMap,
    /// 轨道 A 纯数据 tool-call 的 pending map（0.14.3）。
    pending_raw_tools: PendingRawToolMap,
    /// 单调递增的请求 id 计数。
    next_id: std::sync::atomic::AtomicU64,
    cancel: mpsc::Sender<String>,
}

/// 解析托管脚本解释器 exe 路径（0.25.20 取代 PATH 探测）。
///
/// 版本由 Blink 锁文件锁定、安装在 `runtimes/script_interpreters/` 下，与用户
/// 系统隔离；**未安装不回退到系统 PATH**——版本不可控会破坏托管语义，
/// 缺失时返回 `InterpreterNotInstalled`，由四层兜底转为引导安装的错误项。
fn managed_interpreter(
    kind: blink_infra::script_interpreter::ScriptInterpreterKind,
) -> Result<PathBuf, PluginError> {
    let kind_name = match kind {
        blink_infra::script_interpreter::ScriptInterpreterKind::Python => "Python",
        blink_infra::script_interpreter::ScriptInterpreterKind::Node => "Node.js",
    };
    blink_infra::script_interpreter::resolve(kind)
        .ok_or(PluginError::InterpreterNotInstalled { kind: kind_name })
}

impl PluginProcess {
    /// 拉起进程,挂上 stdout/stderr reader。proxy=(http,https),None=不注入。
    fn spawn(
        exec_path: &PathBuf,
        work_dir: &PathBuf,
        plugin_id: &str,
        runtime_type: RuntimeType,
        proxy: Option<(String, String)>,
    ) -> Result<Self, PluginError> {
        // 根据 runtime 类型构造 Command
        let mut cmd = match runtime_type {
            RuntimeType::Process => {
                let mut c = tokio::process::Command::new(exec_path);
                c.current_dir(work_dir);
                c
            }
            RuntimeType::Python => {
                let interpreter = managed_interpreter(
                    blink_infra::script_interpreter::ScriptInterpreterKind::Python,
                )?;
                let mut c = tokio::process::Command::new(interpreter);
                c.current_dir(work_dir).arg(exec_path);
                c
            }
            RuntimeType::Node => {
                let interpreter = managed_interpreter(
                    blink_infra::script_interpreter::ScriptInterpreterKind::Node,
                )?;
                let mut c = tokio::process::Command::new(interpreter);
                c.current_dir(work_dir).arg(exec_path);
                c
            }
            RuntimeType::Powershell => {
                let mut c = tokio::process::Command::new("powershell");
                c.current_dir(work_dir)
                    .args(["-ExecutionPolicy", "Bypass", "-File"])
                    .arg(exec_path);
                c
            }
        };

        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(windows)]
        cmd.creation_flags(blink_infra::platform::CREATE_NO_WINDOW);
        // 注入全局代理 env（ureq/reqwest 原生读取，插件零代码）
        if let Some((http, https)) = proxy {
            if !http.is_empty() {
                cmd.env("HTTP_PROXY", http);
            }
            if !https.is_empty() {
                cmd.env("HTTPS_PROXY", https);
            }
        }

        let mut child = cmd.spawn().map_err(|e| PluginError::Spawn(e.to_string()))?;

        let stdin = child.stdin.take().ok_or(PluginError::ProcessClosed)?;
        let stdout = child.stdout.take().ok_or(PluginError::ProcessClosed)?;
        let stderr = child.stderr.take().ok_or(PluginError::ProcessClosed)?;

        let pending: PendingMap = Arc::new(std::sync::Mutex::new(HashMap::new()));
        let pending_raw_tools: PendingRawToolMap = Arc::new(std::sync::Mutex::new(HashMap::new()));
        let stdin = Arc::new(PluginStdin {
            pipe: Mutex::new(stdin),
            valid: AtomicBool::new(true),
        });
        let (cancel, mut cancellations) = mpsc::channel::<String>(64);
        {
            let stdin = Arc::clone(&stdin);
            tokio::spawn(async move {
                while let Some(id) = cancellations.recv().await {
                    write_message(&stdin, &PluginRequest::Cancel { id }).await;
                }
            });
        }

        // stdout reader:区分普通查询响应、HTTP 请求、tool-call 结果。
        {
            let pending = Arc::clone(&pending);
            let pending_raw_tools = Arc::clone(&pending_raw_tools);
            let stdin = Arc::clone(&stdin);
            let id = plugin_id.to_string();
            // 每进程的网络请求及回写任务均有界；超额立即回 busy，不创建等待队列。
            let http_slots = Arc::new(Semaphore::new(8));
            tokio::spawn(async move {
                let mut lines = BufReader::new(stdout).lines();
                loop {
                    match lines.next_line().await {
                        Ok(Some(line)) => {
                            if line.trim().is_empty() {
                                continue;
                            }
                            // 先尝试新协议格式（PluginUpstreamMessage enum）
                            match serde_json::from_str::<PluginUpstreamMessage>(&line) {
                                Ok(PluginUpstreamMessage::Response(resp)) => {
                                    if let Some(tx) = pending.lock().unwrap().remove(&resp.id) {
                                        let _ = tx.send(resp);
                                    } else {
                                        tracing::debug!(plugin = %id, id = %resp.id, "孤儿响应(无 pending)");
                                    }
                                }
                                Ok(PluginUpstreamMessage::HttpRequest(req)) => {
                                    // 插件发起 HTTP 请求 → core 代为执行
                                    tracing::debug!(plugin = %id, id = %req.id, "插件发起 HTTP 请求");
                                    let permit = match Arc::clone(&http_slots).try_acquire_owned() {
                                        Ok(permit) => permit,
                                        Err(_) => {
                                            write_message(
                                                &stdin,
                                                &PluginRequest::HttpResponse {
                                                    id: req.id,
                                                    status: 429,
                                                    body: None,
                                                    error: Some("busy: 宿主 HTTP 并发已满".into()),
                                                },
                                            )
                                            .await;
                                            continue;
                                        }
                                    };
                                    let stdin = Arc::clone(&stdin);
                                    tokio::spawn(async move {
                                        let _permit = permit;
                                        let started = std::time::Instant::now();
                                        let (status, body, error) = execute_http_request(
                                            &req.method,
                                            &req.url,
                                            req.body.as_deref(),
                                            req.timeout_ms,
                                            &req.headers,
                                        )
                                        .await;
                                        tracing::debug!(id = %req.id, status, elapsed_ms = started.elapsed().as_millis(), "插件 HTTP 完成");
                                        // 构造 http_response 消息写回插件
                                        let resp = PluginRequest::HttpResponse {
                                            id: req.id,
                                            status,
                                            body,
                                            error,
                                        };
                                        write_message(&stdin, &resp).await;
                                    });
                                }
                                Ok(PluginUpstreamMessage::RawResult(payload)) => {
                                    // 0.14.3:轨道 A 纯数据结果路由到 pending_raw_tools
                                    if let Some(tx) =
                                        pending_raw_tools.lock().unwrap().remove(&payload.id)
                                    {
                                        let _ = tx.send(payload);
                                    } else {
                                        tracing::debug!(plugin = %id, id = %payload.id, "孤儿 raw_result(无 pending)");
                                    }
                                }
                                Err(_) => {
                                    // 向后兼容：尝试旧格式（直接 PluginResponse struct）
                                    match serde_json::from_str::<PluginResponse>(&line) {
                                        Ok(resp) => {
                                            if let Some(tx) =
                                                pending.lock().unwrap().remove(&resp.id)
                                            {
                                                let _ = tx.send(resp);
                                            } else {
                                                tracing::debug!(plugin = %id, id = %resp.id, "孤儿响应(无 pending)");
                                            }
                                        }
                                        Err(_) => {
                                            tracing::warn!(plugin = %id, "无效 stdout JSONL");
                                        }
                                    }
                                }
                            }
                        }
                        Ok(None) => {
                            tracing::debug!(plugin = %id, "插件 stdout 关闭");
                            break;
                        }
                        Err(e) => {
                            tracing::warn!(plugin = %id, error = %e, "读 stdout 失败");
                            break;
                        }
                    }
                }
                // EOF/读失败立即唤醒此代次的所有等待者，不等总期限耗尽。
                pending.lock().unwrap().clear();
                pending_raw_tools.lock().unwrap().clear();
            });
        }

        // stderr reader:逐行汇入 tracing。
        // 行内带 [error]/[warn]/[info] 方括号标记时按标记级别转发（约定见 protocol.rs），
        // 无标记保持 debug——插件 stderr 是常规诊断输出的默认归宿，默认级别下只露错误。
        {
            let id = plugin_id.to_string();
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    match stderr_line_level(&line) {
                        tracing::Level::ERROR => {
                            tracing::error!(plugin = %id, "stderr: {}", line);
                        }
                        tracing::Level::WARN => {
                            tracing::warn!(plugin = %id, "stderr: {}", line);
                        }
                        tracing::Level::INFO => {
                            tracing::info!(plugin = %id, "stderr: {}", line);
                        }
                        _ => tracing::debug!(plugin = %id, "stderr: {}", line),
                    }
                }
            });
        }

        Ok(PluginProcess {
            child: Mutex::new(child),
            stdin,
            pending,
            pending_raw_tools,
            next_id: std::sync::atomic::AtomicU64::new(1),
            cancel,
        })
    }

    /// 是否仍存活(尝试非阻塞回收退出码)。
    async fn is_alive(&self) -> bool {
        self.stdin.valid.load(Ordering::Acquire)
            && matches!(self.child.lock().await.try_wait(), Ok(None))
    }

    /// 写入与接收均被 PluginHandle 的总 deadline 约束。
    async fn query(
        &self,
        query: &str,
        context: &super::protocol::PluginQueryContext,
        settings: Option<&serde_json::Value>,
        deadline: tokio::time::Instant,
    ) -> Result<Vec<PluginItem>, PluginError> {
        let seq = self
            .next_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let req_id = format!("req_{seq}");
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(req_id.clone(), tx);
        let _pending = PendingGuard {
            requests: Arc::clone(&self.pending),
            id: req_id.clone(),
            cancel: self.cancel.clone(),
        };
        let mut stdin = self.stdin.pipe.lock().await;
        let req = PluginRequest::Query {
            id: req_id,
            query: query.to_string(),
            context: context.clone(),
            settings: settings.cloned(),
            budget_ms: Some(remaining_budget(deadline)?),
        };
        let line =
            serde_json::to_string(&req).map_err(|_| PluginError::Io("请求序列化失败".into()))?;
        write_frame(&mut *stdin, &self.stdin.valid, line).await?;
        drop(stdin);
        let resp = rx.await.map_err(|_| PluginError::ProcessClosed)?;
        if let Some(err) = resp.error {
            return Err(PluginError::PluginReturned(err));
        }
        Ok(resp.items)
    }

    /// 搜索出口的系统失败项；不带可执行动作。
    fn error_item(message: &str) -> Vec<PluginItem> {
        vec![PluginItem {
            title: message.to_string(),
            score: -1.0,
            action: PluginAction::None,
            ..Default::default()
        }]
    }

    async fn execute_tool_raw(
        &self,
        tool_name: &str,
        arguments: &serde_json::Value,
        settings: Option<&serde_json::Value>,
        deadline: tokio::time::Instant,
    ) -> Result<RawToolResult, PluginError> {
        let seq = self
            .next_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let req_id = format!("tc_{seq}");
        let (tx, rx) = oneshot::channel();
        self.pending_raw_tools
            .lock()
            .unwrap()
            .insert(req_id.clone(), tx);
        let _pending = PendingGuard {
            requests: Arc::clone(&self.pending_raw_tools),
            id: req_id.clone(),
            cancel: self.cancel.clone(),
        };
        let mut stdin = self.stdin.pipe.lock().await;
        let req = PluginRequest::ToolCall {
            id: req_id,
            tool_name: tool_name.to_string(),
            arguments: arguments.clone(),
            settings: settings.cloned(),
            budget_ms: Some(remaining_budget(deadline)?),
        };
        let line =
            serde_json::to_string(&req).map_err(|_| PluginError::Io("请求序列化失败".into()))?;
        write_frame(&mut *stdin, &self.stdin.valid, line).await?;
        drop(stdin);
        let payload = rx.await.map_err(|_| PluginError::ProcessClosed)?;
        if let Some(err) = payload.error {
            Err(PluginError::PluginReturned(err))
        } else {
            Ok(payload)
        }
    }
}

/// HTTP 回写和 best-effort cancel 不得长期占用 stdin 锁。
async fn write_message(stdin: &PluginStdin, message: &PluginRequest) {
    let Ok(line) = serde_json::to_string(message) else {
        return;
    };
    let write = async {
        let mut pipe = stdin.pipe.lock().await;
        write_frame(&mut *pipe, &stdin.valid, line).await
    };
    let _ = tokio::time::timeout(Duration::from_millis(200), write).await;
}
/// 一个插件:manifest + 懒启动的进程句柄。
pub struct PluginHandle {
    manifest: Arc<PluginManifest>,
    /// manifest 所在目录(解析 exec 相对路径用)。
    dir: PathBuf,
    process: Mutex<Option<Arc<PluginProcess>>>,
    /// 全局代理配置(进程启动时 env 注入),ureq/reqwest 原生读取。运行时可更新。
    proxy: std::sync::Mutex<Option<(String, String)>>,
    query_slots: Arc<Semaphore>,
}

impl PluginHandle {
    pub fn new(
        manifest: Arc<PluginManifest>,
        dir: PathBuf,
        proxy: Option<(String, String)>,
    ) -> Self {
        let concurrency = manifest.runtime.concurrency.unwrap_or(4).clamp(1, 64) as usize;
        PluginHandle {
            manifest,
            dir,
            process: Mutex::new(None),
            proxy: std::sync::Mutex::new(proxy),
            query_slots: Arc::new(Semaphore::new(concurrency)),
        }
    }

    pub fn id(&self) -> &str {
        &self.manifest.id
    }

    pub fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }

    /// 更新代理配置(保存全局代理后调用)。下次 query spawn 时会用新值。
    pub fn update_proxy(&self, proxy: Option<(String, String)>) {
        // 注意:只能更新到内存字段，已启动的进程 env 无法修改，必须杀掉重启
        *self.proxy.lock().unwrap() = proxy;
    }

    /// 重置插件进程(保存全局代理后调用)。下次 query 自动用新 env 重启。
    pub async fn reset_process(&self) {
        let mut guard = self.process.lock().await;
        if guard.is_some() {
            tracing::debug!(plugin = %self.manifest.id, "重置插件进程");
            *guard = None;
        }
    }

    /// 懒启动/复用；排队、准备进程、stdin 和结果共用同一总期限。
    async fn ensure_process(&self) -> Result<Arc<PluginProcess>, PluginError> {
        let mut guard = self.process.lock().await;
        let need_spawn = match guard.as_ref() {
            None => true,
            Some(p) => !p.is_alive().await,
        };
        if need_spawn {
            let exec = self.manifest.exec_path(&self.dir);
            let runtime_type = self.manifest.runtime.r#type;
            tracing::info!(plugin = %self.manifest.id, ?runtime_type, "拉起插件进程");
            let proxy = self.proxy.lock().unwrap().clone();
            let process =
                PluginProcess::spawn(&exec, &self.dir, &self.manifest.id, runtime_type, proxy)?;
            *guard = Some(Arc::new(process));
        }
        Ok(Arc::clone(guard.as_ref().unwrap()))
    }

    async fn clear_closed_process(&self, proc: &Arc<PluginProcess>) {
        let mut guard = self.process.lock().await;
        if guard.as_ref().is_some_and(|p| Arc::ptr_eq(p, proc)) {
            *guard = None;
        }
    }

    fn deadline(&self, caller: Option<std::time::Instant>) -> tokio::time::Instant {
        let manifest =
            tokio::time::Instant::now() + Duration::from_millis(self.manifest.timeout_ms());
        caller
            .map(tokio::time::Instant::from_std)
            .map_or(manifest, |d| d.min(manifest))
    }

    pub async fn query(
        &self,
        query: &str,
        context: &super::protocol::PluginQueryContext,
        settings: Option<&serde_json::Value>,
    ) -> Result<Vec<PluginItem>, PluginError> {
        let deadline = self.deadline(None);
        let call = async {
            let _permit = Arc::clone(&self.query_slots)
                .try_acquire_owned()
                .map_err(|_| PluginError::Busy)?;
            remaining_budget(deadline)?;
            let proc = self.ensure_process().await?;
            let result = proc.query(query, context, settings, deadline).await;
            if matches!(result, Err(PluginError::ProcessClosed)) {
                self.clear_closed_process(&proc).await;
            }
            result
        };
        let result = tokio::time::timeout_at(deadline, call)
            .await
            .unwrap_or(Err(PluginError::Timeout));
        match result {
            Ok(items) => Ok(items),
            Err(e) => {
                tracing::warn!(plugin = %self.manifest.id, "插件查询失败");
                let message = match &e {
                    PluginError::InterpreterNotInstalled { kind } => {
                        format!("脚本运行时（{kind}）未安装，请在 设置 → 引擎 → 脚本运行时 安装")
                    }
                    _ => e.to_string(),
                };
                Ok(PluginProcess::error_item(&message))
            }
        }
    }

    /// Tool-call 使用 manifest 与调用方剩余预算中更小的值。
    pub async fn execute_tool_raw(
        &self,
        tool_name: &str,
        arguments: &serde_json::Value,
        settings: Option<&serde_json::Value>,
        caller_deadline: Option<std::time::Instant>,
    ) -> Result<RawToolResult, PluginError> {
        let deadline = self.deadline(caller_deadline);
        let call = async {
            let _permit = Arc::clone(&self.query_slots)
                .try_acquire_owned()
                .map_err(|_| PluginError::Busy)?;
            remaining_budget(deadline)?;
            let proc = self.ensure_process().await?;
            let result = proc
                .execute_tool_raw(tool_name, arguments, settings, deadline)
                .await;
            if matches!(result, Err(PluginError::ProcessClosed)) {
                self.clear_closed_process(&proc).await;
            }
            result
        };
        tokio::time::timeout_at(deadline, call)
            .await
            .unwrap_or(Err(PluginError::Timeout))
    }
}
/// 执行 HTTP 请求（插件代理请求用）。使用 reqwest 并遵循全局代理配置。
/// 返回 (status_code, body, error)。
async fn execute_http_request(
    method: &str,
    url: &str,
    body: Option<&str>,
    timeout_ms: u64,
    headers: &[(String, String)],
) -> (u16, Option<String>, Option<String>) {
    use reqwest::Client;

    // 0.26.2:带鉴权头的请求禁止跨目的地重定向——reqwest 跟随重定向时只剥离
    // Authorization/Cookie 等内建敏感头，自定义头（如 X-QW-Api-Key）会原样重发，
    // 用户凭据可能被送往配置 Host 之外的服务器。同 host 的 HTTPS 重定向仍允许。
    //
    // 0.26.3:彩云签名请求（x-cy-signature）更严——App Key 就在 URL 路径中，
    // 且签名覆盖精确 method/path/query，任何重定向（含同 Host 改路径/query）
    // 都意味着凭据外发或签名失效；宿主拒绝跟随且不自动重签。
    let builder = match redirect_rule(headers) {
        RedirectRule::Never => Client::builder().redirect(reqwest::redirect::Policy::none()),
        RedirectRule::SameHostHttps => match url_host(url) {
            Some(host) => {
                Client::builder().redirect(reqwest::redirect::Policy::custom(move |attempt| {
                    if attempt.url().scheme() == "https"
                        && attempt.url().host_str() == Some(host.as_str())
                    {
                        attempt.follow()
                    } else {
                        attempt.stop()
                    }
                }))
            }
            None => Client::builder().redirect(reqwest::redirect::Policy::none()),
        },
        RedirectRule::Follow => Client::builder(),
    };
    let client = match builder
        .timeout(std::time::Duration::from_millis(
            timeout_ms.clamp(1, 60_000),
        ))
        .build()
    {
        Ok(c) => c,
        Err(_) => return (0, None, Some("创建 HTTP 客户端失败".into())),
    };

    let method = match method.to_uppercase().as_str() {
        "GET" => reqwest::Method::GET,
        "POST" => reqwest::Method::POST,
        "PUT" => reqwest::Method::PUT,
        "DELETE" => reqwest::Method::DELETE,
        "PATCH" => reqwest::Method::PATCH,
        "HEAD" => reqwest::Method::HEAD,
        "OPTIONS" => reqwest::Method::OPTIONS,
        _ => return (0, None, Some("不支持的 HTTP 方法".into())),
    };

    let mut req = client.request(method, url);
    if let Some(b) = body {
        req = req.body(b.to_string());
    }
    // 0.11.6: 插件自定义 headers（翻译插件 tencent 引擎需 Authorization 等）。
    // body 有但插件未指定 Content-Type 时，默认 application/json（向后兼容）。
    let has_content_type = headers
        .iter()
        .any(|(k, _)| k.eq_ignore_ascii_case("content-type"));
    if body.is_some() && !has_content_type {
        req = req.header("Content-Type", "application/json");
    }
    for (k, v) in headers {
        req = req.header(k, v);
    }

    match req.send().await {
        Ok(resp) => {
            let status = resp.status().as_u16();
            match resp.text().await {
                Ok(text) => (status, Some(text), None),
                Err(e) => (
                    status,
                    None,
                    Some(
                        if e.is_timeout() {
                            "timeout: HTTP 响应读取超时"
                        } else {
                            "读取 HTTP 响应失败"
                        }
                        .into(),
                    ),
                ),
            }
        }
        // 网络库错误通常包含完整 URL（彩云 Key 在路径，其他凭据可能在查询串）。
        Err(e) => (
            0,
            None,
            Some(
                if e.is_timeout() {
                    "timeout: HTTP 请求超时"
                } else {
                    "HTTP 请求失败"
                }
                .into(),
            ),
        ),
    }
}

/// 带凭据语义的请求头——重定向到其他目的地时不得重发。
fn is_sensitive_header(name: &str) -> bool {
    name.eq_ignore_ascii_case("authorization")
        || name.eq_ignore_ascii_case("x-qw-api-key")
        || name.eq_ignore_ascii_case("proxy-authorization")
        || name.eq_ignore_ascii_case("x-cy-signature")
}

/// 重定向跟随策略分级。彩云签名请求取 Never（见 `execute_http_request` 注释）。
enum RedirectRule {
    /// 无凭据：默认跟随。
    Follow,
    /// 鉴权头（Authorization/X-QW-Api-Key 等）：仅同 host HTTPS 跟随。
    SameHostHttps,
    /// 彩云签名 URL（App Key 在路径、签名绑定精确 URL）：拒绝一切重定向。
    Never,
}

fn redirect_rule(headers: &[(String, String)]) -> RedirectRule {
    let is_caiyun = headers
        .iter()
        .any(|(name, _)| name.eq_ignore_ascii_case("x-cy-signature"));
    if is_caiyun {
        return RedirectRule::Never;
    }
    if headers.iter().any(|(name, _)| is_sensitive_header(name)) {
        return RedirectRule::SameHostHttps;
    }
    RedirectRule::Follow
}

/// 从 URL 字符串提取 host（去除 userinfo 与端口）；无法解析时返回 None。
fn url_host(url: &str) -> Option<String> {
    let rest = url.split_once("://")?.1;
    let authority = rest.split(['/', '?', '#']).next()?;
    let host = match authority.rsplit_once('@') {
        Some((_, host)) => host,
        None => authority,
    };
    let host = host.split(':').next()?;
    (!host.is_empty()).then(|| host.to_string())
}

/// 解析插件 stderr 行的级别标记（stderr reader 转发用）。
///
/// 约定：插件在 stderr 行内嵌入方括号级别标记（如 `[translate][error] ali HTTP error: ...`），
/// core 按标记升级转发级别；无标记视为 debug——插件 stderr 的常规诊断输出默认不打扰用户，
/// 但引擎失败等原因不明的错误必须在默认日志级别（info+）可见。
fn stderr_line_level(line: &str) -> tracing::Level {
    if line.contains("[error]") {
        tracing::Level::ERROR
    } else if line.contains("[warn]") {
        tracing::Level::WARN
    } else if line.contains("[info]") {
        tracing::Level::INFO
    } else {
        tracing::Level::DEBUG
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn interrupted_stdin_frame_cannot_be_reused_as_valid_jsonl() {
        let (mut writer, _reader) = tokio::io::duplex(1);
        let valid = AtomicBool::new(true);
        let result = tokio::time::timeout(
            Duration::from_millis(10),
            write_frame(&mut writer, &valid, "{\"id\":\"large-frame\"}".into()),
        )
        .await;
        assert!(result.is_err());
        assert!(!valid.load(Ordering::Acquire));
        assert!(matches!(
            write_frame(&mut writer, &valid, "{}".into()).await,
            Err(PluginError::ProcessClosed)
        ));
    }

    #[tokio::test]
    async fn http_error_does_not_echo_credentials_from_invalid_url() {
        let (_, body, error) = execute_http_request(
            "GET",
            "://private-api-key/path?secret=private-secret",
            None,
            10,
            &[],
        )
        .await;
        assert!(body.is_none());
        let error = error.unwrap();
        assert!(!error.contains("private-api-key"));
        assert!(!error.contains("private-secret"));
    }

    #[test]
    fn sensitive_headers_are_detected_case_insensitively() {
        assert!(is_sensitive_header("Authorization"));
        assert!(is_sensitive_header("x-qw-api-key"));
        assert!(is_sensitive_header("X-QW-API-KEY"));
        assert!(is_sensitive_header("Proxy-Authorization"));
        assert!(is_sensitive_header("X-Cy-Signature"));
        assert!(!is_sensitive_header("Accept-Encoding"));
        assert!(!is_sensitive_header("Content-Type"));
    }

    #[test]
    fn caiyun_signed_requests_never_follow_redirects() {
        let caiyun = vec![
            ("x-cy-signature".to_owned(), "sig".to_owned()),
            ("x-cy-nonce".to_owned(), "n".to_owned()),
        ];
        assert!(matches!(redirect_rule(&caiyun), RedirectRule::Never));
        let mut mixed = caiyun;
        mixed.push(("X-QW-Api-Key".to_owned(), "k".to_owned()));
        assert!(matches!(redirect_rule(&mixed), RedirectRule::Never));
        assert!(matches!(
            redirect_rule(&[("X-QW-Api-Key".to_owned(), "k".to_owned())]),
            RedirectRule::SameHostHttps
        ));
        assert!(matches!(redirect_rule(&[]), RedirectRule::Follow));
        // nonce/时间戳单独出现不代表已签名请求
        assert!(matches!(
            redirect_rule(&[("x-cy-nonce".to_owned(), "n".to_owned())]),
            RedirectRule::Follow
        ));
    }

    /// 本地回环真实 HTTP 重定向验证：默认跟随；带鉴权头/彩云签名头的请求
    /// 跨目的地重定向被停止（3xx 原样返回，目标服务器零命中），凭据不外发。
    #[tokio::test]
    async fn live_redirects_are_rejected_for_credential_bearing_requests() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        async fn redirect_target(
            listener: TcpListener,
            hits: Arc<AtomicUsize>,
        ) -> std::io::Result<()> {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return Ok(());
                };
                let hits = Arc::clone(&hits);
                tokio::spawn(async move {
                    let mut buffer = [0u8; 2048];
                    let mut read = 0usize;
                    loop {
                        match socket.read(&mut buffer[read..]).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => {
                                read += n;
                                if buffer[..read].windows(4).any(|w| w == b"\r\n\r\n") {
                                    break;
                                }
                                if read == buffer.len() {
                                    return;
                                }
                            }
                        }
                    }
                    hits.fetch_add(1, Ordering::SeqCst);
                    let response = "HTTP/1.1 200 OK\r\nContent-Length: 7\r\nConnection: close\r\n\r\nsteered";
                    let _ = socket.write_all(response.as_bytes()).await;
                    let _ = socket.shutdown().await;
                });
            }
        }

        async fn redirect_origin(listener: TcpListener, target: String) -> std::io::Result<()> {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return Ok(());
                };
                let target = target.clone();
                tokio::spawn(async move {
                    let mut buffer = [0u8; 2048];
                    let mut read = 0usize;
                    loop {
                        match socket.read(&mut buffer[read..]).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => {
                                read += n;
                                if buffer[..read].windows(4).any(|w| w == b"\r\n\r\n") {
                                    break;
                                }
                                if read == buffer.len() {
                                    return;
                                }
                            }
                        }
                    }
                    let response = format!(
                        "HTTP/1.1 302 Found\r\nLocation: {target}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    );
                    let _ = socket.write_all(response.as_bytes()).await;
                    let _ = socket.shutdown().await;
                });
            }
        }

        let target_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let target_port = target_listener.local_addr().unwrap().port();
        let origin_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin_port = origin_listener.local_addr().unwrap().port();
        let hits = Arc::new(AtomicUsize::new(0));
        tokio::spawn(redirect_target(target_listener, Arc::clone(&hits)));
        tokio::spawn(redirect_origin(
            origin_listener,
            format!("http://127.0.0.1:{target_port}/steered"),
        ));
        let origin = format!("http://127.0.0.1:{origin_port}/origin");

        // 无敏感头：默认策略跟随跨目的地重定向
        let (status, body, error) =
            execute_http_request("GET", &origin, None, 5_000, &[]).await;
        assert_eq!(status, 200);
        assert_eq!(body.as_deref(), Some("steered"));
        assert!(error.is_none());
        assert_eq!(hits.load(Ordering::SeqCst), 1);

        // 和风鉴权头：跨目的地（含 http）重定向被停止，3xx 原样返回
        let (status, _body, error) = execute_http_request(
            "GET",
            &origin,
            None,
            5_000,
            &[("X-QW-Api-Key".to_owned(), "test-key".to_owned())],
        )
        .await;
        assert_eq!(status, 302);
        assert!(error.is_none());
        assert_eq!(hits.load(Ordering::SeqCst), 1, "目标服务器不得被再次命中");

        // 彩云签名头：拒绝一切重定向，App Key 路径凭据不外发
        let (status, _body, error) = execute_http_request(
            "GET",
            &origin,
            None,
            5_000,
            &[
                ("x-cy-signature".to_owned(), "sig".to_owned()),
                ("x-cy-nonce".to_owned(), "nonce".to_owned()),
                ("x-cy-timestamp".to_owned(), "1".to_owned()),
            ],
        )
        .await;
        assert_eq!(status, 302);
        assert!(error.is_none());
        assert_eq!(hits.load(Ordering::SeqCst), 1, "目标服务器不得被再次命中");
    }

    #[test]
    fn url_host_extraction_strips_userinfo_and_port() {
        assert_eq!(
            url_host("https://abc.re.qweatherapi.com/geo/v2/city/lookup?location=x"),
            Some("abc.re.qweatherapi.com".to_string())
        );
        assert_eq!(
            url_host("https://user:pw@example.com:8443/path"),
            Some("example.com".to_string())
        );
        assert_eq!(url_host("example.com/no-scheme"), None);
        assert_eq!(url_host("https:///empty-host"), None);
    }

    /// 显式运行：先 cargo build -p blink-plugin-weather，再指定此 ignored test。
    /// 无用户配置/密钥写入；真实宿主进程、HTTP 代理与两个 JSONL 出口。
    #[tokio::test]
    #[ignore = "manual: requires built weather executable and live Open-Meteo network"]
    async fn smoke_weather_open_meteo() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .canonicalize()
            .unwrap();
        let mut value: serde_json::Value = serde_json::from_str(include_str!(
            "../../../../plugins/builtin/weather/manifest.json"
        ))
        .unwrap();
        value["runtime"]["exec"] =
            serde_json::json!(root.join("target/debug/blink-plugin-weather.exe"));
        let manifest: PluginManifest = serde_json::from_value(value).unwrap();
        let handle = PluginHandle::new(Arc::new(manifest), root, None);
        let assert_cache = |value: &serde_json::Value, expected_hit: bool, label: &str| {
            let cache = value
                .get("cache")
                .unwrap_or_else(|| panic!("{label}: cache metadata missing: {value}"));
            assert_eq!(
                cache["hit"].as_bool(),
                Some(expected_hit),
                "{label}: unexpected cache hit state: {cache}"
            );
            assert!(
                cache["age_ms"].as_u64().is_some(),
                "{label}: cache age_ms must be an unsigned integer: {cache}"
            );
        };
        let location_args = serde_json::json!({"query":"广州","country_code":"CN","limit":10});
        let mut settings = serde_json::json!({
            "default_city":"incorrect legacy city",
            "temperature_unit":"celsius",
            "settings_revision":1
        });

        let location_start = std::time::Instant::now();
        let locations = handle
            .execute_tool_raw("search_locations", &location_args, Some(&settings), None)
            .await
            .unwrap();
        assert_cache(&locations.data[0], false, "initial location lookup");
        assert_eq!(locations.data[0]["count"], 1);
        println!(
            "[weather-smoke] location cold hit={} age_ms={} elapsed_ms={}",
            locations.data[0]["cache"]["hit"],
            locations.data[0]["cache"]["age_ms"],
            location_start.elapsed().as_millis()
        );

        let location_hit_start = std::time::Instant::now();
        let locations_hit = handle
            .execute_tool_raw("search_locations", &location_args, Some(&settings), None)
            .await
            .unwrap();
        assert_cache(&locations_hit.data[0], true, "repeated location lookup");
        assert_eq!(locations_hit.data[0]["count"], 1);
        println!(
            "[weather-smoke] location warm hit={} age_ms={} elapsed_ms={}",
            locations_hit.data[0]["cache"]["hit"],
            locations_hit.data[0]["cache"]["age_ms"],
            location_hit_start.elapsed().as_millis()
        );

        let location = locations.data[0]["candidates"][0].clone();
        settings["default_location"] = location;

        // 第一条 get_weather ToolCall 必须是天气缓存冷请求。
        let cold_start = std::time::Instant::now();
        let cold = handle
            .execute_tool_raw("get_weather", &serde_json::json!({}), Some(&settings), None)
            .await
            .unwrap();
        let row = &cold.data[0];
        assert!(matches!(row["status"].as_str(), Some("ok" | "partial")));
        assert_eq!(row["provider"], "open_meteo");
        assert!(row["temp"].as_str().unwrap().ends_with("°C"));
        assert_eq!(row["daily"].as_array().unwrap().len(), 7);
        assert!(
            row["daily"][0]["temp_max"]
                .as_str()
                .unwrap()
                .ends_with("°C")
        );
        assert!(row["current"]["valid_at"].as_str().is_some());
        assert_cache(row, false, "initial weather ToolCall");
        let initial_complete = row["status"].as_str() == Some("ok");
        let fetched_at = row["fetched_at"]
            .as_str()
            .filter(|value| !value.is_empty())
            .map(str::to_owned);
        if initial_complete {
            assert!(
                fetched_at.is_some(),
                "complete weather result must include fetched_at"
            );
        }
        println!(
            "[weather-smoke] weather cold: {} status={} cache_hit={} age_ms={} days=7 elapsed_ms={}",
            row["summary"],
            row["status"],
            row["cache"]["hit"],
            row["cache"]["age_ms"],
            cold_start.elapsed().as_millis()
        );

        if !initial_complete {
            eprintln!(
                "[weather-smoke] DIAGNOSTIC: initial upstream result is partial; partial results are not cached, so subsequent weather cache hits and fetched_at equality are not assumed"
            );
        }

        // 相同地点/provider 的默认 Query 与显式温标 ToolCall 应复用冷请求结果。
        let query_start = std::time::Instant::now();
        let items = handle
            .query("", &Default::default(), Some(&settings))
            .await
            .unwrap();
        // 0.26.2 起 Query 出口附加一个可打开的数据来源声明项，排在天气主项之后。
        assert_eq!(items.len(), 2);
        assert!(items[0].title.contains("°C"), "{}", items[0].title);
        let query_data = items[0]
            .payload
            .as_ref()
            .expect("weather query item should expose structured payload");
        if initial_complete {
            assert_cache(query_data, true, "default weather Query");
            assert_eq!(query_data["fetched_at"].as_str(), fetched_at.as_deref());
        } else {
            eprintln!(
                "[weather-smoke] DIAGNOSTIC: default Query returned status={}; cache hit={}; fetched_at={}",
                query_data["status"], query_data["cache"]["hit"], query_data["fetched_at"]
            );
        }
        println!(
            "[weather-smoke] default Query status={} cache_hit={} age_ms={} elapsed_ms={}",
            query_data["status"],
            query_data["cache"]["hit"],
            query_data["cache"]["age_ms"],
            query_start.elapsed().as_millis()
        );

        for (unit, suffix) in [("fahrenheit", "°F"), ("celsius", "°C")] {
            let start = std::time::Instant::now();
            let raw = handle
                .execute_tool_raw(
                    "get_weather",
                    &serde_json::json!({"unit":unit}),
                    Some(&settings),
                    None,
                )
                .await
                .unwrap();
            let row = &raw.data[0];
            assert!(row["temp"].as_str().unwrap().ends_with(suffix));
            assert_eq!(row["provider"], "open_meteo");
            if initial_complete {
                assert_cache(row, true, "explicit temperature-unit ToolCall");
                assert_eq!(row["fetched_at"].as_str(), fetched_at.as_deref());
            } else {
                eprintln!(
                    "[weather-smoke] DIAGNOSTIC: explicit {unit} returned status={}; cache hit={}; fetched_at={}",
                    row["status"], row["cache"]["hit"], row["fetched_at"]
                );
            }
            println!(
                "[weather-smoke] explicit {unit}: status={} cache_hit={} age_ms={} elapsed_ms={}",
                row["status"],
                row["cache"]["hit"],
                row["cache"]["age_ms"],
                start.elapsed().as_millis()
            );
        }

        // 配置代次变化会清空插件内缓存；新代次首个天气调用必须冷启动。
        settings["settings_revision"] = serde_json::json!(2);
        let revision_start = std::time::Instant::now();
        let after_revision = handle
            .execute_tool_raw("get_weather", &serde_json::json!({}), Some(&settings), None)
            .await
            .unwrap();
        let revision_row = &after_revision.data[0];
        assert!(matches!(
            revision_row["status"].as_str(),
            Some("ok" | "partial")
        ));
        assert_cache(
            revision_row,
            false,
            "weather after settings revision change",
        );
        println!(
            "[weather-smoke] revision=2 weather cold: status={} cache_hit={} age_ms={} elapsed_ms={}",
            revision_row["status"],
            revision_row["cache"]["hit"],
            revision_row["cache"]["age_ms"],
            revision_start.elapsed().as_millis()
        );
        if revision_row["status"].as_str() == Some("partial") {
            eprintln!(
                "[weather-smoke] DIAGNOSTIC: revision=2 upstream result is partial and is not assumed to populate the weather cache"
            );
        }

        // 新 revision 的地点缓存也从冷查询开始，再次查询必须命中。
        let location_revision_start = std::time::Instant::now();
        let location_after_revision = handle
            .execute_tool_raw("search_locations", &location_args, Some(&settings), None)
            .await
            .unwrap();
        assert_eq!(location_after_revision.data[0]["count"], 1);
        assert_cache(
            &location_after_revision.data[0],
            false,
            "location lookup after settings revision change",
        );
        println!(
            "[weather-smoke] revision=2 location cold hit={} age_ms={} elapsed_ms={}",
            location_after_revision.data[0]["cache"]["hit"],
            location_after_revision.data[0]["cache"]["age_ms"],
            location_revision_start.elapsed().as_millis()
        );
        let location_revision_hit_start = std::time::Instant::now();
        let location_after_revision_hit = handle
            .execute_tool_raw("search_locations", &location_args, Some(&settings), None)
            .await
            .unwrap();
        assert_eq!(location_after_revision_hit.data[0]["count"], 1);
        assert_cache(
            &location_after_revision_hit.data[0],
            true,
            "repeated location lookup after settings revision change",
        );
        println!(
            "[weather-smoke] revision=2 location warm hit={} age_ms={} elapsed_ms={}",
            location_after_revision_hit.data[0]["cache"]["hit"],
            location_after_revision_hit.data[0]["cache"]["age_ms"],
            location_revision_hit_start.elapsed().as_millis()
        );

        match &items[0].action {
            PluginAction::Copy { text } => {
                assert!(text.contains("来源：Open-Meteo"));
                assert!(text.contains("当前天气有效时间"));
                assert!(text.contains("不代表全天持续"));
                assert!(text.lines().count() >= 10);
            }
            _ => panic!("weather query must copy the summary"),
        }
        println!("live query: {}", items[0].title);
        assert_eq!(items[1].title, "数据来源：Open-Meteo");
        match &items[1].action {
            PluginAction::Open { path } => assert_eq!(path, "https://open-meteo.com/"),
            _ => panic!("attribution item must open the source page"),
        }
        let ambiguous = handle
            .execute_tool_raw(
                "get_weather",
                &serde_json::json!({"city":"朝阳"}),
                Some(&settings),
                None,
            )
            .await
            .unwrap();
        assert_eq!(ambiguous.data[0]["status"], "needs_location");
        assert!(ambiguous.data[0]["candidates"].as_array().unwrap().len() > 1);
        println!(
            "live ambiguity: candidates={}",
            ambiguous.data[0]["candidates"].as_array().unwrap().len()
        );
    }

    #[tokio::test]
    async fn dropping_request_cleans_only_its_pending_and_sends_cancel() {
        let requests: RequestMap<u8> = Arc::new(std::sync::Mutex::new(HashMap::new()));
        let (cancel, mut cancellations) = mpsc::channel(2);
        let (tx_a, rx_a) = oneshot::channel();
        let (tx_b, rx_b) = oneshot::channel();
        requests.lock().unwrap().insert("a".into(), tx_a);
        requests.lock().unwrap().insert("b".into(), tx_b);
        let guard = PendingGuard {
            requests: Arc::clone(&requests),
            id: "a".into(),
            cancel,
        };
        drop(guard);
        assert!(rx_a.await.is_err());
        assert_eq!(cancellations.recv().await.as_deref(), Some("a"));
        requests
            .lock()
            .unwrap()
            .remove("b")
            .unwrap()
            .send(7)
            .unwrap();
        assert_eq!(rx_b.await.unwrap(), 7);
    }

    #[tokio::test]
    async fn successful_response_does_not_cancel_and_old_generation_is_isolated() {
        let old: RequestMap<u8> = Arc::new(std::sync::Mutex::new(HashMap::new()));
        let new: RequestMap<u8> = Arc::new(std::sync::Mutex::new(HashMap::new()));
        let (cancel, mut cancellations) = mpsc::channel(2);
        let (tx, rx) = oneshot::channel();
        old.lock().unwrap().insert("req_1".into(), tx);
        let guard = PendingGuard {
            requests: Arc::clone(&old),
            id: "req_1".into(),
            cancel,
        };
        old.lock()
            .unwrap()
            .remove("req_1")
            .unwrap()
            .send(1)
            .unwrap();
        assert_eq!(rx.await.unwrap(), 1);
        let (tx_new, _rx_new) = oneshot::channel();
        new.lock().unwrap().insert("req_1".into(), tx_new);
        drop(guard);
        assert!(cancellations.try_recv().is_err());
        assert_eq!(new.lock().unwrap().len(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn caller_deadline_covers_process_lock_and_capacity_releases_after_timeout() {
        let manifest: PluginManifest = serde_json::from_value(serde_json::json!({
            "schema_version":1,"id":"test.weather","name":"test","version":"1",
            "runtime":{"exec":"never-start.exe","timeout_ms":1000,"concurrency":1},
            "triggers":[],"capabilities":[]
        }))
        .unwrap();
        let handle = PluginHandle::new(Arc::new(manifest), PathBuf::new(), None);
        let _locked = handle.process.lock().await;
        let result = handle
            .execute_tool_raw(
                "get_weather",
                &serde_json::json!({}),
                None,
                Some((tokio::time::Instant::now() + Duration::from_millis(10)).into_std()),
            )
            .await;
        assert!(matches!(result, Err(PluginError::Timeout)));
        assert_eq!(handle.query_slots.available_permits(), 1);
    }

    #[tokio::test]
    async fn excess_work_returns_busy_without_starting_a_process() {
        let manifest: PluginManifest = serde_json::from_value(serde_json::json!({
            "schema_version":1,"id":"test.weather","name":"test","version":"1",
            "runtime":{"exec":"never-start.exe","concurrency":1},"triggers":[],"capabilities":[]
        }))
        .unwrap();
        let handle = PluginHandle::new(Arc::new(manifest), PathBuf::new(), None);
        let _permit = handle.query_slots.acquire().await.unwrap();
        let result = handle
            .execute_tool_raw("get_weather", &serde_json::json!({}), None, None)
            .await;
        assert!(matches!(result, Err(PluginError::Busy)));
        assert!(handle.process.lock().await.is_none());
    }

    #[test]
    fn stderr_level_markers_map_to_levels() {
        assert_eq!(
            stderr_line_level("[translate][error] ali HTTP error: status=404"),
            tracing::Level::ERROR
        );
        assert_eq!(
            stderr_line_level("[translate][warn] 降级到: 百度翻译 (baidu)"),
            tracing::Level::WARN
        );
        assert_eq!(
            stderr_line_level("[translate][info] ready"),
            tracing::Level::INFO
        );
    }

    #[test]
    fn stderr_level_unmarked_defaults_to_debug() {
        // 无标记的常规诊断行保持 debug
        assert_eq!(
            stderr_line_level("[translate] tool_call: id=tc_2, target=zh"),
            tracing::Level::DEBUG
        );
        // 单词 error 不带方括号不误升级
        assert_eq!(
            stderr_line_level("[translate] ali HTTP error: status=404"),
            tracing::Level::DEBUG
        );
        assert_eq!(stderr_line_level(""), tracing::Level::DEBUG);
    }
}
