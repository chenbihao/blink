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
    child: Child,
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
            child,
            stdin: Mutex::new(stdin),
            pending,
            next_id: std::sync::atomic::AtomicU64::new(1),
        })
    }

    /// 是否仍存活(尝试非阻塞回收退出码)。
    fn is_alive(&mut self) -> bool {
        !matches!(self.child.try_wait(), Ok(Some(_)))
    }

    /// 发送 query 并等待 response(带超时)。
    async fn query(&self, query: &str, timeout_ms: u64) -> Result<Vec<PluginItem>, PluginError> {
        let seq = self
            .next_id
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let req_id = format!("req_{seq}");
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(req_id.clone(), tx);

        let req = PluginRequest::query(req_id.clone(), query);
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
                Err(PluginError::Timeout)
            }
        }
    }
}

/// 一个插件:manifest + 懒启动的进程句柄。
pub struct PluginHandle {
    manifest: Arc<PluginManifest>,
    /// manifest 所在目录(解析 exec 相对路径用)。
    dir: PathBuf,
    process: Mutex<Option<PluginProcess>>,
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

    /// 查询插件:懒启动(或复用)进程 → 发 query → 收 items。
    /// 进程已死则重启一次。
    pub async fn query(&self, query: &str) -> Result<Vec<PluginItem>, PluginError> {
        let timeout_ms = self.manifest.timeout_ms();
        let mut guard = self.process.lock().await;

        // 进程不存在或已死 → (重新)拉起
        let need_spawn = match guard.as_mut() {
            None => true,
            Some(p) => !p.is_alive(),
        };
        if need_spawn {
            let exec = self.manifest.exec_path(&self.dir);
            tracing::info!(plugin = %self.manifest.id, exec = %exec.display(), "拉起插件进程");
            let proc = PluginProcess::spawn(&exec, &self.dir, &self.manifest.id)?;
            *guard = Some(proc);
        }

        let proc = guard.as_ref().unwrap();
        let result = proc.query(query, timeout_ms).await;

        // 进程关闭类错误:清理句柄,下次重启
        if matches!(result, Err(PluginError::ProcessClosed)) {
            *guard = None;
        }
        result
    }
}
