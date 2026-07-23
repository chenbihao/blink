//! 插件进程管理(见 §3.1):懒启动 + 常驻复用,stdio JSONL 往返。
//!
//! - 命中 trigger 才 spawn;进程复用,崩溃则下次重启。
//! - 三路并发:stdout reader task(按 request id 路由到 pending oneshot)+ stderr
//!   reader task(汇入 tracing)+ stdin 写入(互斥)。不读 stdout/stderr 会因 pipe
//!   写满而死锁,故必须各起一个 reader task。
//! - 查询用 tokio::time::timeout 兜底;超时清理 pending,不 kill 进程(下次复用)。
//! - Windows:CREATE_NO_WINDOW 防控制台子进程弹窗。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin};
use tokio::sync::{Mutex, oneshot};

use super::manifest::{PluginManifest, RuntimeType};
use super::protocol::{
    PluginAction, PluginItem, PluginRequest, PluginResponse, PluginUpstreamMessage,
    ToolResultPayload,
};

/// 查询错误。
#[derive(Debug)]
#[allow(dead_code)] // 错误处理骨架，0.3+ 完整实现
pub enum PluginError {
    /// 进程拉起失败。
    Spawn(String),
    /// 解释器未找到。
    InterpreterNotFound(String),
    /// 进程已关闭(stdout EOF / 写 stdin 失败)。
    ProcessClosed,
    /// 查询超时。
    Timeout,
    /// 插件返回 error。
    PluginReturned(String),
    /// 协议/IO 错误。
    Io(String),
}

impl std::fmt::Display for PluginError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PluginError::Spawn(e) => write!(f, "spawn 失败: {e}"),
            PluginError::InterpreterNotFound(name) => write!(f, "未找到解释器: {name}"),
            PluginError::ProcessClosed => write!(f, "进程已关闭"),
            PluginError::Timeout => write!(f, "查询超时"),
            PluginError::PluginReturned(e) => write!(f, "插件返回错误: {e}"),
            PluginError::Io(e) => write!(f, "IO 错误: {e}"),
        }
    }
}

type PendingMap = Arc<Mutex<HashMap<String, oneshot::Sender<PluginResponse>>>>;
type PendingToolMap = Arc<Mutex<HashMap<String, oneshot::Sender<ToolResultPayload>>>>;

/// 已拉起的插件进程。
struct PluginProcess {
    /// `Mutex<Child>` 使存活检测可 `&self`(try_wait 需 `&mut`),从而 query 能在
    /// process 锁外并发执行——多个 in-flight query 各持一份 `Arc<PluginProcess>`。
    child: Mutex<Child>,
    stdin: Arc<Mutex<ChildStdin>>,
    pending: PendingMap,
    /// tool-call 请求的 pending map(0.9.3)。
    pending_tools: PendingToolMap,
    /// 单调递增的请求 id 计数。
    next_id: std::sync::atomic::AtomicU64,
}

/// 解释器探测结果。
#[derive(Debug, Clone, serde::Serialize)]
pub struct InterpreterStatus {
    /// 是否找到
    pub found: bool,
    /// 可执行文件路径
    pub path: Option<String>,
    /// 版本号（如 "3.11.4"）
    pub version: Option<String>,
    /// 版本是否符合最低要求
    pub version_ok: bool,
    /// 错误信息（未找到/版本过低时）
    pub error: Option<String>,
}

/// 检测路径是否应该跳过（无效/无用的系统目录）。
fn should_skip_path(path: &std::path::Path) -> bool {
    let path_str = path.to_string_lossy().to_lowercase();
    // 排除 WindowsApps（里面都是假 exe，实际是 AppX 执行代理）
    path_str.contains("microsoft\\windowsapps")
        || path_str.contains("appdata\\local\\microsoft\\windowsapps")
        || path_str.contains("program files\\windowsapps")
}

/// 在 PATH 中查找解释器，返回第一个找到的路径。
///
/// 如果提供了 `manual_path`，优先验证该路径是否存在：
/// - 有效则直接返回（用户手动配置优先）
/// - 无效则记录警告并回退到 PATH 扫描
pub fn find_interpreter(
    candidates: &[&str],
    manual_path: Option<&str>,
) -> Result<PathBuf, PluginError> {
    // 1. 优先使用用户手动配置的路径
    if let Some(path) = manual_path {
        let p = PathBuf::from(path);
        if p.is_file() {
            // 直接指向 exe 文件
            tracing::debug!(path = %path, "使用手动配置的解释器路径");
            return Ok(p);
        }
        if p.is_dir() {
            // 选了文件夹 → 在内部查找候选 exe
            for candidate in candidates {
                let exe = p.join(format!("{candidate}.exe"));
                if exe.exists() {
                    tracing::debug!(path = %exe.display(), "在手动配置的目录中找到解释器");
                    return Ok(exe);
                }
            }
            tracing::warn!(dir = %path, "手动配置的目录中未找到解释器，回退到 PATH 扫描");
        } else {
            tracing::warn!(path = %path, "手动配置的解释器路径不存在，回退到 PATH 扫描");
        }
    }

    // 2. 扫描 PATH 环境变量的所有目录
    let path_env = std::env::var("PATH").unwrap_or_default();
    for dir in std::env::split_paths(&path_env) {
        // 跳过无效系统目录
        if should_skip_path(&dir) {
            continue;
        }
        for candidate in candidates {
            let exe = dir.join(format!("{candidate}.exe"));
            if exe.exists() {
                return Ok(exe);
            }
        }
    }
    Err(PluginError::InterpreterNotFound(
        candidates.join(" / ").to_string(),
    ))
}

/// 探测解释器版本，返回 (version_string, version_ok)。
fn probe_version(exe_path: &Path, version_arg: &str, min_version: &str) -> (Option<String>, bool) {
    let output = match crate::infra::platform::no_window(std::process::Command::new(exe_path))
        .arg(version_arg)
        .output()
    {
        Ok(o) => o,
        Err(_) => return (None, false),
    };

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let version_output = if stdout.is_empty() { &stderr } else { &stdout };

    // 简单的版本提取：找第一个数字.数字.数字模式
    let version_re = regex::Regex::new(r"(\d+\.\d+\.\d+)").unwrap();
    let version = version_re
        .find(version_output)
        .map(|m| m.as_str().to_string());

    let version_ok = version
        .as_ref()
        .map(|v| version_is_gte(v, min_version))
        .unwrap_or(false);

    (version, version_ok)
}

/// 简单的版本比较：a >= b？只支持 x.y.z 格式。
fn version_is_gte(a: &str, b: &str) -> bool {
    let parse = |s: &str| -> Option<(u32, u32, u32)> {
        let parts: Vec<&str> = s.split('.').collect();
        if parts.len() < 2 {
            return None;
        }
        let major = parts[0].parse().ok()?;
        let minor = parts[1].parse().ok()?;
        let patch = parts.get(2).and_then(|p| p.parse().ok()).unwrap_or(0);
        Some((major, minor, patch))
    };

    match (parse(a), parse(b)) {
        (Some(a), Some(b)) => a >= b,
        _ => false,
    }
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
                let interpreter = find_interpreter(&["python", "python3", "py"], None)?;
                let mut c = tokio::process::Command::new(interpreter);
                c.current_dir(work_dir).arg(exec_path);
                c
            }
            RuntimeType::Node => {
                let interpreter = find_interpreter(&["node", "nodejs"], None)?;
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
        cmd.creation_flags(crate::infra::platform::CREATE_NO_WINDOW);
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

        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
        let pending_tools: PendingToolMap = Arc::new(Mutex::new(HashMap::new()));
        let stdin = Arc::new(Mutex::new(stdin));

        // stdout reader:区分普通查询响应、HTTP 请求、tool-call 结果。
        {
            let pending = Arc::clone(&pending);
            let pending_tools = Arc::clone(&pending_tools);
            let stdin = Arc::clone(&stdin);
            let id = plugin_id.to_string();
            tauri::async_runtime::spawn(async move {
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
                                    if let Some(tx) = pending.lock().await.remove(&resp.id) {
                                        let _ = tx.send(resp);
                                    } else {
                                        tracing::debug!(plugin = %id, id = %resp.id, "孤儿响应(无 pending)");
                                    }
                                }
                                Ok(PluginUpstreamMessage::HttpRequest(req)) => {
                                    // 插件发起 HTTP 请求 → core 代为执行
                                    tracing::debug!(plugin = %id, url = %req.url.chars().take(80).collect::<String>(), "插件发起 HTTP 请求");
                                    let stdin = Arc::clone(&stdin);
                                    tauri::async_runtime::spawn(async move {
                                        let (status, body, error) = execute_http_request(
                                            &req.method,
                                            &req.url,
                                            req.body.as_deref(),
                                            req.timeout_ms,
                                            &req.headers,
                                        )
                                        .await;
                                        // 构造 http_response 消息写回插件
                                        let resp = PluginRequest::HttpResponse {
                                            id: req.id,
                                            status,
                                            body,
                                            error,
                                        };
                                        if let Ok(line) = serde_json::to_string(&resp) {
                                            let mut stdin = stdin.lock().await;
                                            let _ = stdin.write_all((line + "\n").as_bytes()).await;
                                            let _ = stdin.flush().await;
                                        }
                                    });
                                }
                                Ok(PluginUpstreamMessage::ToolResult(payload)) => {
                                    // 0.9.3:tool-call 结果路由到 pending_tools
                                    if let Some(tx) = pending_tools.lock().await.remove(&payload.id)
                                    {
                                        let _ = tx.send(payload);
                                    } else {
                                        tracing::debug!(plugin = %id, id = %payload.id, "孤儿 tool_result(无 pending)");
                                    }
                                }
                                Err(_) => {
                                    // 向后兼容：尝试旧格式（直接 PluginResponse struct）
                                    match serde_json::from_str::<PluginResponse>(&line) {
                                        Ok(resp) => {
                                            if let Some(tx) = pending.lock().await.remove(&resp.id)
                                            {
                                                let _ = tx.send(resp);
                                            } else {
                                                tracing::debug!(plugin = %id, id = %resp.id, "孤儿响应(无 pending)");
                                            }
                                        }
                                        Err(e) => {
                                            tracing::warn!(plugin = %id, error = %e, %line, "无效 stdout JSONL");
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
            });
        }

        // stderr reader:逐行汇入 tracing。
        {
            let id = plugin_id.to_string();
            tauri::async_runtime::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    tracing::debug!(plugin = %id, "stderr: {}", line);
                }
            });
        }

        Ok(PluginProcess {
            child: Mutex::new(child),
            stdin,
            pending,
            pending_tools,
            next_id: std::sync::atomic::AtomicU64::new(1),
        })
    }

    /// 是否仍存活(尝试非阻塞回收退出码)。
    async fn is_alive(&self) -> bool {
        !matches!(self.child.lock().await.try_wait(), Ok(Some(_)))
    }

    /// 发送 query 并等待 response(带超时)。
    ///
    /// 统一错误处理：所有错误（超时、IO、进程崩溃、协议错误）都转化为
    /// 负分的 PluginItem 错误项返回，前端显示友好错误信息。
    /// 这确保用户永远不会看到「一直转圈」的占位符。
    async fn query(
        &self,
        query: &str,
        context: &super::protocol::PluginQueryContext,
        settings: Option<&serde_json::Value>,
        timeout_ms: u64,
    ) -> Result<Vec<PluginItem>, PluginError> {
        let seq = self
            .next_id
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let req_id = format!("req_{seq}");
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(req_id.clone(), tx);

        let req = PluginRequest::Query {
            id: req_id.clone(),
            query: query.to_string(),
            context: context.clone(),
            settings: settings.cloned(),
        };
        let line = match serde_json::to_string(&req) {
            Ok(s) => s + "\n",
            Err(e) => {
                self.pending.lock().await.remove(&req_id);
                return Ok(Self::error_item(&format!("请求序列化失败：{e}")));
            }
        };

        {
            let mut stdin = self.stdin.lock().await;
            if stdin.write_all(line.as_bytes()).await.is_err() || stdin.flush().await.is_err() {
                self.pending.lock().await.remove(&req_id);
                return Ok(Self::error_item("插件进程已关闭"));
            }
        }

        match tokio::time::timeout(Duration::from_millis(timeout_ms), rx).await {
            Ok(Ok(resp)) => {
                // 插件返回 error 时，转成特殊的 PluginItem 让前端显示错误信息
                if let Some(err) = resp.error {
                    tracing::debug!(id = %req_id, error = %err.message, "插件返回错误信息");
                    return Ok(vec![PluginItem {
                        title: err.message,
                        subtitle: None,
                        score: -1.0,                // 负分，排序到最后
                        action: PluginAction::None, // 纯展示，无操作
                        ..Default::default()
                    }]);
                }
                Ok(resp.items)
            }
            Ok(Err(_)) => {
                // sender 被 drop = reader 退出 = 进程崩溃
                Ok(Self::error_item("插件进程意外退出"))
            }
            Err(_) => {
                self.pending.lock().await.remove(&req_id);
                // best-effort 通知插件取消(限时,插件 stdin 堵塞则放弃)。
                self.send_cancel(&req_id).await;
                Ok(Self::error_item("查询超时，请稍后重试"))
            }
        }
    }

    /// 构造错误信息项（score=-1，排序到最后，纯展示不执行动作）。
    /// 所有插件系统错误统一走这个方法转化，确保前端占位符能被替换为友好错误。
    fn error_item(message: &str) -> Vec<PluginItem> {
        vec![PluginItem {
            title: message.to_string(),
            subtitle: None,
            score: -1.0,
            action: PluginAction::Open {
                path: String::new(),
            },
            ..Default::default()
        }]
    }

    /// 执行 tool-call(0.9.3):发 ToolCall 请求 → 等 ToolResult 响应。
    ///
    /// 与 `query()` 同模式:oneshot channel + timeout + 错误转 PluginError。
    /// 返回 `Vec<PluginItem>` —— 与 `PluginResponse.items` 统一格式。
    ///
    /// req_id 格式 `"tc_{seq}"` 与 query 的 `"{seq}"` 共用 `next_id` 计数器,
    /// 前缀保证不冲突;pending map 也分开(`pending` vs `pending_tools`)。
    async fn execute_tool(
        &self,
        tool_name: &str,
        arguments: &serde_json::Value,
        settings: Option<&serde_json::Value>,
        timeout_ms: u64,
    ) -> Result<Vec<PluginItem>, PluginError> {
        let seq = self
            .next_id
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let req_id = format!("tc_{seq}");
        let (tx, rx) = oneshot::channel();
        self.pending_tools.lock().await.insert(req_id.clone(), tx);

        let req = PluginRequest::ToolCall {
            id: req_id.clone(),
            tool_name: tool_name.to_string(),
            arguments: arguments.clone(),
            settings: settings.cloned(),
        };
        let line = serde_json::to_string(&req)
            .map_err(|e| PluginError::Io(format!("请求序列化失败: {e}")))?;

        {
            let mut stdin = self.stdin.lock().await;
            stdin
                .write_all((line + "\n").as_bytes())
                .await
                .map_err(|_| PluginError::ProcessClosed)?;
            stdin
                .flush()
                .await
                .map_err(|_| PluginError::ProcessClosed)?;
        }

        match tokio::time::timeout(Duration::from_millis(timeout_ms), rx).await {
            Ok(Ok(payload)) => {
                if let Some(err) = payload.error {
                    Err(PluginError::PluginReturned(err.message))
                } else {
                    Ok(payload.items)
                }
            }
            Ok(Err(_)) => Err(PluginError::ProcessClosed),
            Err(_) => {
                self.pending_tools.lock().await.remove(&req_id);
                self.send_cancel(&req_id).await;
                Err(PluginError::Timeout)
            }
        }
    }

    /// 发送 cancel 通知(best-effort):让支持取消的插件停止那次 query。
    /// 插件可忽略;整体限时 200ms——插件若不读 stdin(stdin 管道堵塞)则放弃,
    /// 避免独占 stdin 锁把单次超时拖垮成「插件永久不可用」。
    async fn send_cancel(&self, req_id: &str) {
        let req = PluginRequest::Cancel {
            id: req_id.to_string(),
        };
        let Ok(line) = serde_json::to_string(&req) else {
            return;
        };
        let write = async {
            let mut stdin = self.stdin.lock().await;
            stdin.write_all((line + "\n").as_bytes()).await?;
            stdin.flush().await?;
            Ok::<(), std::io::Error>(())
        };
        let _ = tokio::time::timeout(Duration::from_millis(200), write).await;
    }
}

/// 一个插件:manifest + 懒启动的进程句柄。
pub struct PluginHandle {
    manifest: Arc<PluginManifest>,
    /// manifest 所在目录(解析 exec 相对路径用)。
    dir: PathBuf,
    process: Mutex<Option<Arc<PluginProcess>>>,
    /// 全局代理配置(进程启动时 env 注入),ureq/reqwest 原生读取。运行时可更新。
    proxy: std::sync::Mutex<Option<(String, String)>>,
}

impl PluginHandle {
    pub fn new(
        manifest: Arc<PluginManifest>,
        dir: PathBuf,
        proxy: Option<(String, String)>,
    ) -> Self {
        PluginHandle {
            manifest,
            dir,
            process: Mutex::new(None),
            proxy: std::sync::Mutex::new(proxy),
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

    /// 查询插件:懒启动(或复用)进程 → 发 query(带上下文) → 收 items。
    /// 进程已死则重启一次。
    ///
    /// 统一错误处理:所有失败(解释器未找到、进程拉起失败、超时、IO 错误)都转化为
    /// 负分的 PluginItem 错误项返回,前端显示友好错误。
    /// 只有 `ProcessClosed` 这类「可恢复」错误会清理句柄以便下次重启。
    ///
    /// 并发:process 锁只覆盖「确保进程存在」(spawn 判定),拿到 Arc 后立即释放,
    /// query 在锁外 await——同插件的多次 query 可并发 in-flight(stdin/pending 各自
    /// 互斥、按 id 路由,天然安全)。`manifest.concurrency` 信号量(§3.7 B3)留后续。
    pub async fn query(
        &self,
        query: &str,
        context: &super::protocol::PluginQueryContext,
        settings: Option<&serde_json::Value>,
    ) -> Result<Vec<PluginItem>, PluginError> {
        let timeout_ms = self.manifest.timeout_ms();

        // 短暂持锁:确保进程存在,clone Arc 后释放。
        let proc = {
            let mut guard = self.process.lock().await;
            let need_spawn = match guard.as_ref() {
                None => true,
                Some(p) => !p.is_alive().await,
            };
            if need_spawn {
                let exec = self.manifest.exec_path(&self.dir);
                let runtime_type = self.manifest.runtime.r#type;
                tracing::info!(plugin = %self.manifest.id, ?runtime_type, exec = %exec.display(), "拉起插件进程");
                let proxy = self.proxy.lock().unwrap().clone();
                match PluginProcess::spawn(&exec, &self.dir, &self.manifest.id, runtime_type, proxy)
                {
                    Ok(proc) => *guard = Some(Arc::new(proc)),
                    Err(e) => {
                        // 进程拉起失败 → 返回友好错误项,用户知道发生了什么
                        let msg = match e {
                            PluginError::InterpreterNotFound(name) => {
                                format!("未找到解释器：{name}，请在设置页配置")
                            }
                            PluginError::Spawn(e) => format!("进程启动失败：{e}"),
                            _ => e.to_string(),
                        };
                        tracing::warn!(plugin = %self.manifest.id, error = %msg, "插件拉起失败");
                        return Ok(PluginProcess::error_item(&msg));
                    }
                }
            }
            Arc::clone(guard.as_ref().unwrap())
        };

        let result = proc.query(query, context, settings, timeout_ms).await;

        // 进程关闭类错误:清理句柄,下次重启。
        // 守护竞态:并发 query 可能在 A 失败期间已重建新进程——只清「我这个已死的」,
        // 用 Arc::ptr_eq 判定身份,勿误清他人的新进程。
        if matches!(result, Err(PluginError::ProcessClosed)) {
            let mut guard = self.process.lock().await;
            if guard
                .as_ref()
                .map(|p| Arc::ptr_eq(p, &proc))
                .unwrap_or(false)
            {
                *guard = None;
            }
        }

        // 所有其他错误也转化为错误项返回(目前 query() 内部已转化,这里兜底)
        match result {
            Ok(items) => Ok(items),
            Err(e) => {
                tracing::warn!(plugin = %self.manifest.id, error = %e, "插件查询失败");
                Ok(PluginProcess::error_item(&e.to_string()))
            }
        }
    }

    /// 执行 tool-call(0.9.3):懒启动进程 → 发 ToolCall → 收 ToolResult。
    ///
    /// 与 `query()` 同模式:进程懒启动 + 超时 + 进程关闭时清理句柄。
    /// 返回 `Vec<PluginItem>` —— 与 `PluginResponse.items` 统一格式,
    /// 调用方(`PluginActionAdapter`)可直接复用。
    pub async fn execute_tool(
        &self,
        tool_name: &str,
        arguments: &serde_json::Value,
        settings: Option<&serde_json::Value>,
    ) -> Result<Vec<PluginItem>, PluginError> {
        let timeout_ms = self.manifest.timeout_ms();

        // 短暂持锁:确保进程存在,clone Arc 后释放。
        let proc = {
            let mut guard = self.process.lock().await;
            let need_spawn = match guard.as_ref() {
                None => true,
                Some(p) => !p.is_alive().await,
            };
            if need_spawn {
                let exec = self.manifest.exec_path(&self.dir);
                let runtime_type = self.manifest.runtime.r#type;
                tracing::info!(plugin = %self.manifest.id, ?runtime_type, exec = %exec.display(), "拉起插件进程(tool-call)");
                let proxy = self.proxy.lock().unwrap().clone();
                match PluginProcess::spawn(&exec, &self.dir, &self.manifest.id, runtime_type, proxy)
                {
                    Ok(proc) => *guard = Some(Arc::new(proc)),
                    Err(e) => {
                        let msg = match e {
                            PluginError::InterpreterNotFound(name) => {
                                format!("未找到解释器：{name}，请在设置页配置")
                            }
                            PluginError::Spawn(e) => format!("进程启动失败：{e}"),
                            _ => e.to_string(),
                        };
                        tracing::warn!(plugin = %self.manifest.id, error = %msg, "插件拉起失败(tool-call)");
                        return Err(PluginError::Spawn(msg));
                    }
                }
            }
            Arc::clone(guard.as_ref().unwrap())
        };

        let result = proc
            .execute_tool(tool_name, arguments, settings, timeout_ms)
            .await;

        // 进程关闭类错误:清理句柄,下次重启。
        if matches!(result, Err(PluginError::ProcessClosed)) {
            let mut guard = self.process.lock().await;
            if guard
                .as_ref()
                .map(|p| Arc::ptr_eq(p, &proc))
                .unwrap_or(false)
            {
                *guard = None;
            }
        }

        result
    }
}

/// 所有解释器的探测结果。
#[derive(Debug, Clone, serde::Serialize)]
pub struct InterpretersStatus {
    pub python: InterpreterStatus,
    pub node: InterpreterStatus,
}

/// 探测系统中所有支持的脚本解释器状态。
///
/// 如果提供了 `manual_python` 或 `manual_node`，优先验证该路径（用户手动配置），
/// 无效时才回退到 PATH 扫描。
pub fn probe_interpreters(
    manual_python: Option<&str>,
    manual_node: Option<&str>,
) -> InterpretersStatus {
    // Python: 最低 3.8
    let python = match find_interpreter(&["python", "python3", "py"], manual_python) {
        Ok(path) => {
            let (version, version_ok) = probe_version(&path, "--version", "3.8.0");
            InterpreterStatus {
                found: true,
                path: Some(path.to_string_lossy().to_string()),
                version,
                version_ok,
                error: if !version_ok {
                    Some("Python 版本需 >= 3.8".to_string())
                } else {
                    None
                },
            }
        }
        Err(e) => InterpreterStatus {
            found: false,
            path: None,
            version: None,
            version_ok: false,
            error: Some(e.to_string()),
        },
    };

    // Node.js: 最低 16.0
    let node = match find_interpreter(&["node", "nodejs"], manual_node) {
        Ok(path) => {
            let (version, version_ok) = probe_version(&path, "--version", "16.0.0");
            InterpreterStatus {
                found: true,
                path: Some(path.to_string_lossy().to_string()),
                version,
                version_ok,
                error: if !version_ok {
                    Some("Node.js 版本需 >= 16.0".to_string())
                } else {
                    None
                },
            }
        }
        Err(e) => InterpreterStatus {
            found: false,
            path: None,
            version: None,
            version_ok: false,
            error: Some(e.to_string()),
        },
    };

    InterpretersStatus { python, node }
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

    let client = match Client::builder()
        .timeout(std::time::Duration::from_millis(timeout_ms))
        .build()
    {
        Ok(c) => c,
        Err(e) => return (0, None, Some(format!("创建 HTTP 客户端失败：{e}"))),
    };

    let method = match method.to_uppercase().as_str() {
        "GET" => reqwest::Method::GET,
        "POST" => reqwest::Method::POST,
        "PUT" => reqwest::Method::PUT,
        "DELETE" => reqwest::Method::DELETE,
        "PATCH" => reqwest::Method::PATCH,
        "HEAD" => reqwest::Method::HEAD,
        "OPTIONS" => reqwest::Method::OPTIONS,
        m => return (0, None, Some(format!("不支持的 HTTP 方法：{m}"))),
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
                Err(e) => (status, None, Some(format!("读取响应失败：{e}"))),
            }
        }
        Err(e) => (0, None, Some(e.to_string())),
    }
}
