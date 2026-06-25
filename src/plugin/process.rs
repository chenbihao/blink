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
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin};
use tokio::sync::{oneshot, Mutex};

use super::manifest::PluginManifest;
use super::protocol::{PluginItem, PluginRequest, PluginResponse};

/// Windows CreateProcess 标志:不创建控制台窗口。
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// 查询错误。
#[derive(Debug)]
pub enum PluginError {
    /// 进程拉起失败。
    Spawn(String),
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
            PluginError::ProcessClosed => write!(f, "进程已关闭"),
            PluginError::Timeout => write!(f, "查询超时"),
            PluginError::PluginReturned(e) => write!(f, "插件返回错误: {e}"),
            PluginError::Io(e) => write!(f, "IO 错误: {e}"),
        }
    }
}

type PendingMap = Arc<Mutex<HashMap<String, oneshot::Sender<PluginResponse>>>>;

/// 已拉起的插件进程。
struct PluginProcess {
    /// `Mutex<Child>` 使存活检测可 `&self`(try_wait 需 `&mut`),从而 query 能在
    /// process 锁外并发执行——多个 in-flight query 各持一份 `Arc<PluginProcess>`。
    child: Mutex<Child>,
    stdin: Mutex<ChildStdin>,
    pending: PendingMap,
    /// 单调递增的请求 id 计数。
    next_id: std::sync::atomic::AtomicU64,
}

impl PluginProcess {
    /// 拉起进程,挂上 stdout/stderr reader。
    fn spawn(exec_path: &PathBuf, work_dir: &PathBuf, plugin_id: &str) -> Result<Self, PluginError> {
        let mut cmd = tokio::process::Command::new(exec_path);
        cmd.current_dir(work_dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(windows)]
        cmd.creation_flags(CREATE_NO_WINDOW);

        let mut child = cmd.spawn().map_err(|e| PluginError::Spawn(e.to_string()))?;

        let stdin = child.stdin.take().ok_or(PluginError::ProcessClosed)?;
        let stdout = child.stdout.take().ok_or(PluginError::ProcessClosed)?;
        let stderr = child.stderr.take().ok_or(PluginError::ProcessClosed)?;

        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));

        // stdout reader:按 id 路由 response 到对应 pending oneshot。
        {
            let pending = Arc::clone(&pending);
            let id = plugin_id.to_string();
            tauri::async_runtime::spawn(async move {
                let mut lines = BufReader::new(stdout).lines();
                loop {
                    match lines.next_line().await {
                        Ok(Some(line)) => {
                            if line.trim().is_empty() {
                                continue;
                            }
                            match serde_json::from_str::<PluginResponse>(&line) {
                                Ok(resp) => {
                                    if let Some(tx) = pending.lock().await.remove(&resp.id) {
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
            stdin: Mutex::new(stdin),
            pending,
            next_id: std::sync::atomic::AtomicU64::new(1),
        })
    }

    /// 是否仍存活(尝试非阻塞回收退出码)。
    async fn is_alive(&self) -> bool {
        !matches!(self.child.lock().await.try_wait(), Ok(Some(_)))
    }

    /// 发送 query 并等待 response(带超时)。
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
        let line = serde_json::to_string(&req).map_err(|e| PluginError::Io(e.to_string()))? + "\n";
        {
            let mut stdin = self.stdin.lock().await;
            if stdin.write_all(line.as_bytes()).await.is_err() || stdin.flush().await.is_err() {
                self.pending.lock().await.remove(&req_id);
                return Err(PluginError::ProcessClosed);
            }
        }

        match tokio::time::timeout(Duration::from_millis(timeout_ms), rx).await {
            Ok(Ok(resp)) => {
                if let Some(err) = resp.error {
                    return Err(PluginError::PluginReturned(err.message));
                }
                Ok(resp.items)
            }
            Ok(Err(_)) => Err(PluginError::ProcessClosed), // sender 被 drop(reader 退出)
            Err(_) => {
                self.pending.lock().await.remove(&req_id);
                // best-effort 通知插件取消(限时,插件 stdin 堵塞则放弃)。
                self.send_cancel(&req_id).await;
                Err(PluginError::Timeout)
            }
        }
    }

    /// 发送 cancel 通知(best-effort):让支持取消的插件停止那次 query。
    /// 插件可忽略;整体限时 200ms——插件若不读 stdin(stdin 管道堵塞)则放弃,
    /// 避免独占 stdin 锁把单次超时拖垮成「插件永久不可用」。
    async fn send_cancel(&self, req_id: &str) {
        let req = PluginRequest::Cancel { id: req_id.to_string() };
        let Ok(line) = serde_json::to_string(&req) else { return; };
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
}

impl PluginHandle {
    pub fn new(manifest: Arc<PluginManifest>, dir: PathBuf) -> Self {
        PluginHandle {
            manifest,
            dir,
            process: Mutex::new(None),
        }
    }

    pub fn id(&self) -> &str {
        &self.manifest.id
    }

    pub fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }

    /// 查询插件:懒启动(或复用)进程 → 发 query(带上下文) → 收 items。
    /// 进程已死则重启一次。
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
                tracing::info!(plugin = %self.manifest.id, exec = %exec.display(), "拉起插件进程");
                let proc = PluginProcess::spawn(&exec, &self.dir, &self.manifest.id)?;
                *guard = Some(Arc::new(proc));
            }
            Arc::clone(guard.as_ref().unwrap())
        };

        let result = proc.query(query, context, settings, timeout_ms).await;

        // 进程关闭类错误:清理句柄,下次重启。
        // 守护竞态:并发 query 可能在 A 失败期间已重建新进程——只清「我这个已死的」,
        // 用 Arc::ptr_eq 判定身份,勿误清他人的新进程。
        if matches!(result, Err(PluginError::ProcessClosed)) {
            let mut guard = self.process.lock().await;
            if guard.as_ref().map(|p| Arc::ptr_eq(p, &proc)).unwrap_or(false) {
                *guard = None;
            }
        }
        result
    }
}
