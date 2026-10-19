//! 托管 Everything 实例治理（0.26.14）。
//!
//! 用户未安装 Everything 时，`FileEngine` 的 HTTP 查询路径零改动——本模块
//! 负责 blink 专属实例（`-instance blink` + 专属 ini）的**懒启动、就绪探测、
//! 崩溃退避重启、退出清理**；探测到用户已装并开启 HTTP 的 Everything 时由
//! `FileEngine` 复用而不经本模块（data_source 优先级链见 file_engine.rs）。
//!
//! **实例隔离**：ini 写在托管 artifact 目录内（`-config` 显式指定），
//! http_server 开、`http_server_bindings=127.0.0.1` 仅回环、专属端口区间、
//! ETP/托盘/窗口全关——与用户自己的 Everything 设置互不污染。
//!
//! **锁纪律（2026-10-19 实机教训）**：`state` 锁内**绝不 await 长等待**——
//! 首版监督循环在锁内 `child.wait()` 挂整个进程生命周期，`stop()` 抢不到锁
//! 永远排队（设置页「停止实例」无反应、日志静默），期间 `shutting_down`
//! 标志未复位又让后续启动报「正在退出」。现规则：锁只覆盖 spawn 决策/
//! 状态翻转/`try_wait` 等瞬时操作；就绪等待与监督均无锁轮询，`stop()` 经
//! CancellationToken 立即打断所有等待循环。
//!
//! **退出清理的身份校验**：持有 `tokio::process::Child` 句柄执行 kill——
//! 内核级钉住确切进程对象，是「instance 名 + PID 双重校验防误杀」的最强
//! 形式（不存在 PID 复用误杀）；`-instance blink` 的单实例守卫保证重复
//! 启动只聚焦已有实例。
//!
//! **未提权退化**：Everything 无管理员权限时仍可启动（实测无提权弹框），
//! 无服务不保证存在 NTFS 索引，auto 使用本地目录兜底；实时 NTFS 索引需显式安装服务
//! （app 层 run_elevated）。

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use blink_infra::managed_runtime::{self, KIND_EVERYTHING};
use tokio_util::sync::CancellationToken;

/// 托管实例名（`-instance blink`）：窗口标题/进程身份/单实例守卫键。
pub const INSTANCE_NAME: &str = "blink";
pub const DEFAULT_MANAGED_PORT: u16 = 52819;
/// 端口探测区间长度：从偏好端口向上尝试 N 个。
pub const PORT_SCAN_COUNT: u16 = 20;
/// 启动后 HTTP 就绪探测的单次间隔。
const PROBE_INTERVAL: Duration = Duration::from_millis(250);
/// 监督循环轮询间隔（try_wait，短锁）。
const SUPERVISE_INTERVAL: Duration = Duration::from_millis(500);
/// 搜索路径懒启动的等待预算：本轮查不到就地降级，下一轮命中（Everything
/// 无服务慢扫首启可能数十秒，不能阻塞搜索热路径）。
pub const SEARCH_START_BUDGET: Duration = Duration::from_millis(300);
/// 设置页「启动并测试」的等待预算。
pub const TEST_START_BUDGET: Duration = Duration::from_secs(20);
/// 崩溃重启退避：1s 起、指数翻倍、上限 60s。
const RESTART_BACKOFF_INITIAL: Duration = Duration::from_secs(1);
const RESTART_BACKOFF_MAX: Duration = Duration::from_secs(60);
/// 稳定运行超过此时长后退避重置。
const STABLE_UPTIME: Duration = Duration::from_secs(60);

/// 启动失败原因（设置页按 code 展示；搜索路径静默降级）。
#[derive(Debug, Clone, thiserror::Error)]
pub enum StartError {
    #[error("内置搜索引擎未安装")]
    NotInstalled,
    #[error("正在退出，不再启动实例")]
    ShuttingDown,
    #[error("托管实例已由用户停止，请在托管引擎页启动")]
    Stopped,
    #[error("无可用端口（{base}..+{count}）")]
    NoFreePort { base: u16, count: u16 },
    #[error("实例启动后未就绪: {0}")]
    NotReady(String),
    #[error("启动失败: {0}")]
    Spawn(String),
}

/// 运行时状态快照（设置页展示 + data_source 链决策参考）。
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeSnapshot {
    pub installed: bool,
    pub service_installed: bool,
    pub index_count: Option<u64>,
    /// ready 的实例才算 running（启动中/等就绪显示为未运行 + lastError）。
    pub running: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    pub version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    /// 待处理重启的退避剩余毫秒，None = 无待处理重启。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub restart_backoff_ms: Option<u64>,
}

struct Running {
    port: u16,
    child: tokio::process::Child,
    /// 即使 Blink 被强制终止，内核也会回收其托管子进程。
    #[cfg(windows)]
    _job: Option<blink_infra::platform::process::JobObjectHandle>,
    started_at: Instant,
    /// HTTP 探测已通过。未 ready 的实例仍是活 child（后台继续初始化）。
    ready: bool,
}

struct State {
    instance: Option<Running>,
    /// 启动轮次：每次 spawn/stop 递增，探测循环与监督据此判断自己是否过期。
    run_id: u64,
    /// 崩溃退避截止时刻（绝对时间，None = 无退避）。
    backoff_until: Option<Instant>,
    /// 上一次退避时长（稳定期重置；`backoff_until` 到期清空后据此继续翻倍）。
    last_backoff: Duration,
    last_error: Option<String>,
}

struct Inner {
    /// 独占命名实例的进程租约；CLI/另一个 Blink 进程不得回收活跃持有者。
    owner_lease: std::sync::Mutex<Option<std::fs::File>>,
    /// 仅串行化启动前的孤儿恢复，不占用 stop()/状态读取所需的 state 锁。
    start_gate: tokio::sync::Mutex<()>,
    /// 串行化 spawn 决策/状态翻转（锁内零长等待，见模块注释锁纪律）。
    state: tokio::sync::Mutex<State>,
    /// 展示快照（同步读，无 IO）。
    snapshot: std::sync::RwLock<RuntimeSnapshot>,
    shutting_down: AtomicBool,
    suspended: AtomicBool,
    /// stop() 通知通道：cancel 立即打断所有就绪等待/监督循环；
    /// stop 收尾时换新 token 允许后续重启。
    stop_signal: std::sync::RwLock<CancellationToken>,
    /// 上次启动使用的端口（重启沿用）。
    last_port: std::sync::RwLock<Option<u16>>,
}

/// 托管 Everything 实例治理器（Clone = 共享同一实例）。
#[derive(Clone)]
pub struct EverythingRuntime {
    inner: Arc<Inner>,
}

impl Default for EverythingRuntime {
    fn default() -> Self {
        Self::new()
    }
}

impl EverythingRuntime {
    /// 同一进程的搜索、Capability 与设置共用一个命名实例控制器。
    /// `new()` 保留给隔离测试；业务入口不得另建同名实例的所有权状态。
    pub(super) fn shared() -> Self {
        static RUNTIME: std::sync::LazyLock<EverythingRuntime> =
            std::sync::LazyLock::new(EverythingRuntime::new);
        RUNTIME.clone()
    }

    pub fn new() -> Self {
        let version = managed_runtime::lock_entry(KIND_EVERYTHING)
            .map(|e| e.version)
            .unwrap_or_default();
        Self {
            inner: Arc::new(Inner {
                owner_lease: std::sync::Mutex::new(None),
                start_gate: tokio::sync::Mutex::new(()),
                state: tokio::sync::Mutex::new(State {
                    instance: None,
                    run_id: 0,
                    backoff_until: None,
                    last_backoff: RESTART_BACKOFF_INITIAL,
                    last_error: None,
                }),
                snapshot: std::sync::RwLock::new(RuntimeSnapshot {
                    installed: managed_runtime::resolve(KIND_EVERYTHING).is_some(),
                    service_installed: false,
                    index_count: None,
                    running: false,
                    port: None,
                    version,
                    last_error: None,
                    restart_backoff_ms: None,
                }),
                shutting_down: AtomicBool::new(false),
                suspended: AtomicBool::new(false),
                stop_signal: std::sync::RwLock::new(CancellationToken::new()),
                last_port: std::sync::RwLock::new(None),
            }),
        }
    }

    /// 当前状态快照（只读；installed 随安装/卸载即时重查）。
    pub fn snapshot(&self) -> RuntimeSnapshot {
        let mut snap = self.inner.snapshot.read().unwrap().clone();
        let exe = managed_runtime::resolve(KIND_EVERYTHING);
        snap.installed = exe.is_some();
        snap.service_installed = exe.as_ref().is_some_and(|path| {
            blink_infra::platform::shell::managed_everything_service_installed(path)
                .unwrap_or(false)
        });
        snap
    }

    pub fn resume(&self) {
        self.inner.suspended.store(false, Ordering::Release);
    }

    /// 查询热路径只读内存，不读取安装目录或服务注册表。
    pub fn index_ready(&self) -> bool {
        self.inner
            .snapshot
            .read()
            .unwrap()
            .index_count
            .is_some_and(|n| n > 0)
    }

    /// 就绪端口；未运行/未就绪返回 None。
    pub async fn port_if_running(&self) -> Option<u16> {
        let state = self.inner.state.lock().await;
        state.instance.as_ref().filter(|r| r.ready).map(|r| r.port)
    }

    /// 在识别外部 Everything 前先清理本程序遗留的命名实例，避免把自己认成用户来源。
    pub async fn recover_orphan(&self) -> Result<(), StartError> {
        let _gate = self.inner.start_gate.lock().await;
        self.recover_orphan_locked().await
    }

    async fn recover_orphan_locked(&self) -> Result<(), StartError> {
        if self.inner.shutting_down.load(Ordering::Acquire) { return Err(StartError::ShuttingDown); }
        if self.inner.suspended.load(Ordering::Acquire) { return Err(StartError::Stopped); }
        let recovery_run = {
            let state = self.inner.state.lock().await;
            (state.instance.is_none() && state.backoff_until.is_none_or(|until| Instant::now() >= until))
                .then_some(state.run_id)
        };
        if let Some(run_id) = recovery_run {
            if let Some(exe) = managed_runtime::resolve(KIND_EVERYTHING) {
                #[cfg(windows)]
                {
                    let inner = self.inner.clone();
                    let recovery = tokio::task::spawn_blocking(move || {
                        let mut lease = inner.owner_lease.lock().unwrap();
                        if lease.is_none() {
                            *lease = Some(managed_runtime::acquire_instance_lease(KIND_EVERYTHING)
                                .map_err(|error| error.to_string())?);
                        }
                        drop(lease);
                        // Everything 1.4 SDK 的命名 IPC 窗口与退出命令。
                        blink_infra::platform::process::reclaim_named_window_process(
                            &format!("EVERYTHING_TASKBAR_NOTIFICATION_({INSTANCE_NAME})"),
                            &exe, 40006, Duration::from_secs(2),
                        )
                    }).await.map_err(|e| StartError::Spawn(format!("旧实例恢复任务失败: {e}")))?;
                    if let Err(error) = recovery {
                        let mut state = self.inner.state.lock().await;
                        if state.run_id == run_id {
                            state.last_error = Some(error.clone());
                            self.publish(&state);
                        }
                        return Err(StartError::Spawn(error));
                    }
                }
                let state = self.inner.state.lock().await;
                if state.run_id != run_id { return Err(StartError::Stopped); }
            }
        }
        Ok(())
    }

    /// 懒启动并返回就绪端口；已就绪时直接复用。
    /// `budget` 限制 HTTP 就绪等待；旧实例恢复单独有界，不占 state 锁。
    pub async fn ensure_started(
        &self,
        preferred_port: u16,
        budget: Duration,
    ) -> Result<u16, StartError> {
        if self.inner.shutting_down.load(Ordering::Acquire) {
            return Err(StartError::ShuttingDown);
        }
        if self.inner.suspended.load(Ordering::Acquire) {
            return Err(StartError::Stopped);
        }
        let start_guard = self.inner.start_gate.lock().await;
        self.recover_orphan_locked().await?;
        // ── 锁段①：spawn 决策（无等待）─────────────────────────────────
        let (port, my_run, was_ready) = {
            let mut state = self.inner.state.lock().await;
            if self.inner.shutting_down.load(Ordering::Acquire) {
                return Err(StartError::ShuttingDown);
            }
            if self.inner.suspended.load(Ordering::Acquire) {
                return Err(StartError::Stopped);
            }
            if let Some(running) = state.instance.as_ref() {
                if running.ready {
                    return Ok(running.port);
                }
                // 已有实例在启动中：不重复 spawn，只参与等待就绪。
                (running.port, state.run_id, false)
            } else {
                if let Some(until) = state.backoff_until {
                    if Instant::now() < until {
                        return Err(StartError::NotReady("崩溃退避中".to_string()));
                    }
                    state.backoff_until = None; // 退避期满
                }
                let Some(exe) = managed_runtime::resolve(KIND_EVERYTHING) else {
                    state.last_error = Some("未安装".to_string());
                    self.publish(&state);
                    return Err(StartError::NotInstalled);
                };
                let port = select_free_port(preferred_port).ok_or(StartError::NoFreePort {
                    base: preferred_port,
                    count: PORT_SCAN_COUNT,
                })?;
                let ini_path =
                    write_instance_ini(&exe, port).map_err(|e| StartError::Spawn(e.to_string()))?;

                let mut cmd = tokio::process::Command::new(&exe);
                cmd.args([
                    "-instance".to_string(),
                    INSTANCE_NAME.to_string(),
                    "-config".to_string(),
                    ini_path.to_string_lossy().to_string(),
                    "-db".to_string(),
                    exe.with_file_name("Everything-blink.db")
                        .to_string_lossy()
                        .to_string(),
                    // 后台静默启动（无主窗口）；托盘由 ini 关闭（show_tray_icon=0）。
                    // 注意：1.4.1 无 -hide 开关——传它弹「命令行选项」错误框（模态），
                    // 用户关框后进程以 exit 1 结束（2026-10-19 实机教训）。
                    "-startup".to_string(),
                ]);
                #[cfg(windows)]
                cmd.creation_flags(blink_infra::platform::CREATE_NO_WINDOW);
                let child = cmd
                    .kill_on_drop(true)
                    .spawn()
                    .map_err(|e| StartError::Spawn(format!("启动 Everything 失败: {e}")))?;
                #[cfg(windows)]
                let job = child.id().and_then(|pid| {
                    blink_infra::platform::process::assign_job_object(pid).map_err(|error| {
                        tracing::warn!(pid, %error, "Everything 进程约束失败，将由下次启动恢复遗留实例");
                    }).ok()
                });

                state.run_id += 1;
                state.instance = Some(Running {
                    port,
                    child,
                    #[cfg(windows)]
                    _job: job,
                    started_at: Instant::now(),
                    ready: false,
                });
                state.last_error = None;
                self.publish(&state);
                tracing::info!(port, exe = %exe.display(), "托管 Everything 实例已启动，等待 HTTP 就绪");
                (port, state.run_id, false)
            }
        };
        drop(start_guard);

        let _ = was_ready; // 锁段①到达此处必然 false（ready 已提前 return）

        // ── 无锁段：等待就绪（可被 stop 打断）───────────────────────────
        let deadline = Instant::now() + budget;
        loop {
            if let Some(index_count) = probe_index_count(port).await {
                // ── 锁段②：翻转 ready（首个翻转者挂监督，防重复）────────
                let mut state = self.inner.state.lock().await;
                if state.run_id != my_run {
                    // 期间发生了 stop/新启动——本轮作废。
                    return Err(StartError::NotReady("启动轮次已被取代".to_string()));
                }
                let became_ready = state
                    .instance
                    .as_mut()
                    .is_some_and(|r| !std::mem::replace(&mut r.ready, true));
                if became_ready {
                    state.last_error = None;
                    self.inner.snapshot.write().unwrap().index_count = Some(index_count);
                    *self.inner.last_port.write().unwrap() = Some(port);
                    self.publish(&state);
                    drop(state);
                    self.spawn_supervisor(my_run);
                    self.spawn_index_monitor(my_run, port);
                }
                return Ok(port);
            }
            // 子进程退出检测（短锁 try_wait）。
            {
                let mut state = self.inner.state.lock().await;
                if state.run_id != my_run {
                    return Err(StartError::NotReady("启动轮次已被取代".to_string()));
                }
                let exited = state
                    .instance
                    .as_mut()
                    .and_then(|r| r.child.try_wait().ok().flatten());
                if let Some(exit) = exited {
                    tracing::warn!(?exit, port, "托管 Everything 启动后立即退出");
                    let started_at = state
                        .instance
                        .as_ref()
                        .map(|r| r.started_at)
                        .unwrap_or_else(Instant::now);
                    self.enter_backoff(&mut state, started_at);
                    return Err(StartError::Spawn(format!("实例启动即退出: {exit}")));
                }
            }
            if Instant::now() >= deadline {
                // 预算耗尽：保留 child 继续初始化，下次调用再探测。
                let mut state = self.inner.state.lock().await;
                if state.run_id == my_run {
                    state.last_error = Some("等待就绪超时（后台继续初始化）".to_string());
                    self.publish(&state);
                }
                return Err(StartError::NotReady(format!(
                    "等待 HTTP 就绪超时（{budget:?}）"
                )));
            }
            let signal = self.inner.stop_signal.read().unwrap().clone();
            tokio::select! {
                _ = tokio::time::sleep(PROBE_INTERVAL) => {}
                _ = signal.cancelled() => {
                    return Err(StartError::ShuttingDown);
                }
            }
        }
    }

    /// 进入崩溃退避：稳定运行过则重置为初始值，否则在上次时长上翻倍封顶。
    fn enter_backoff(&self, state: &mut State, started_at: Instant) {
        let backoff = if started_at.elapsed() >= STABLE_UPTIME {
            RESTART_BACKOFF_INITIAL
        } else {
            state
                .last_backoff
                .saturating_mul(2)
                .min(RESTART_BACKOFF_MAX)
        };
        state.last_backoff = backoff;
        state.instance = None;
        state.backoff_until = Some(Instant::now() + backoff);
        tracing::warn!(?backoff, "托管 Everything 进入崩溃退避");
        self.publish(state);
    }

    /// 监督循环（轮询 try_wait，短锁；可被 stop 打断）：
    /// child 退出且非关停 → 记退避、期满后重试一次懒启动。
    ///
    /// 一次监督循环只服务一次启动（成功重挂新循环），`run_id` 不匹配立即退出。
    fn spawn_supervisor(&self, my_run: u64) {
        let runtime = self.clone();
        tokio::spawn(async move {
            let signal = runtime.inner.stop_signal.read().unwrap().clone();
            loop {
                tokio::select! {
                    _ = signal.cancelled() => return, // stop() 已接管清理
                    _ = tokio::time::sleep(SUPERVISE_INTERVAL) => {}
                }
                let mut state = runtime.inner.state.lock().await;
                if state.run_id != my_run {
                    return; // 新轮次/已停止
                }
                let Some(running) = state.instance.as_mut() else {
                    return;
                };
                match running.child.try_wait() {
                    Ok(Some(exit)) => {
                        let started_at = running.started_at;
                        tracing::warn!(?exit, "托管 Everything 实例退出，进入退避重启");
                        runtime.enter_backoff(&mut state, started_at);
                        let backoff = state
                            .backoff_until
                            .map(|until| until.saturating_duration_since(Instant::now()))
                            .unwrap_or(RESTART_BACKOFF_INITIAL);
                        drop(state);
                        tokio::select! {
                            _ = tokio::time::sleep(backoff) => {}
                            _ = signal.cancelled() => return,
                        }
                        {
                            let mut state = runtime.inner.state.lock().await;
                            if state.run_id != my_run {
                                return;
                            }
                            state.backoff_until = None;
                        }
                        let preferred = runtime.inner.last_port.read().unwrap().unwrap_or_default();
                        let _ = runtime.ensure_started(preferred, TEST_START_BUDGET).await;
                        return; // 新启动成功会挂新监督循环
                    }
                    Ok(None) => { /* 存活，继续轮询 */ }
                    Err(error) => {
                        tracing::warn!(%error, "托管 Everything try_wait 失败，停止监督");
                        return;
                    }
                }
            }
        });
    }

    /// 停止托管实例（设置页/退出清理共用；app 层负责 async 上下文）。幂等。
    ///
    /// 立即 cancel 停止信号打断所有就绪等待/监督循环，再取锁清理（锁内零长
    /// 等待，不再排队）；收尾换新信号 token 并复位标志，允许后续重启。
    pub async fn stop(&self) {
        tracing::info!("正在停止托管 Everything 实例…");
        self.inner.suspended.store(true, Ordering::Release);
        self.inner.shutting_down.store(true, Ordering::Release);
        self.inner.stop_signal.read().unwrap().cancel();
        let running = {
            let mut state = self.inner.state.lock().await;
            let running = state.instance.take();
            state.run_id += 1;
            state.backoff_until = None;
            state.last_error = None;
            self.publish(&state);
            running
        };
        if let Some(mut running) = running {
            let graceful = async {
                let exe = managed_runtime::resolve(KIND_EVERYTHING).ok_or(())?;
                let mut command =
                    blink_infra::platform::no_window_tokio(tokio::process::Command::new(exe));
                command
                    .args(["-instance", INSTANCE_NAME, "-exit"])
                    .kill_on_drop(true);
                let status = command.status().await.map_err(|_| ())?;
                if !status.success() {
                    return Err(());
                }
                running.child.wait().await.map_err(|_| ())?;
                Ok::<(), ()>(())
            };
            if !matches!(
                tokio::time::timeout(Duration::from_secs(2), graceful).await,
                Ok(Ok(()))
            ) {
                tracing::debug!(
                    port = running.port,
                    "Everything 正常退出超时，按持有句柄终止"
                );
                let _ = running.child.kill().await;
            }
            tracing::info!(port = running.port, "托管 Everything 实例已停止");
        }
        *self.inner.stop_signal.write().unwrap() = CancellationToken::new();
        self.inner.shutting_down.store(false, Ordering::Release);
    }

    fn spawn_index_monitor(&self, my_run: u64, port: u16) {
        let runtime = self.clone();
        tokio::spawn(async move {
            let signal = runtime.inner.stop_signal.read().unwrap().clone();
            loop {
                tokio::select! {
                    _ = signal.cancelled() => return,
                    _ = tokio::time::sleep(Duration::from_secs(3)) => {}
                }
                let count = probe_index_count(port).await;
                let state = runtime.inner.state.lock().await;
                if state.run_id != my_run || state.instance.is_none() {
                    return;
                }
                runtime.inner.snapshot.write().unwrap().index_count = count;
            }
        });
    }

    fn publish(&self, state: &State) {
        let mut snap = self.inner.snapshot.write().unwrap();
        snap.installed = managed_runtime::resolve(KIND_EVERYTHING).is_some();
        snap.running = state.instance.as_ref().is_some_and(|r| r.ready);
        if !snap.running {
            snap.index_count = None;
        }
        snap.port = state.instance.as_ref().filter(|r| r.ready).map(|r| r.port);
        snap.last_error = state.last_error.clone();
        snap.restart_backoff_ms = state
            .backoff_until
            .map(|until| until.saturating_duration_since(Instant::now()).as_millis() as u64);
    }
}

/// 在 `[base, base+count)` 内找空闲端口（bind 试探）。
///
/// bind 成功即释放并采用——与 Everything 实际 bind 之间存在微小竞态，
/// 就绪探测失败会走退避重启换端口，可接受。
pub fn select_free_port(base: u16) -> Option<u16> {
    for offset in 0..PORT_SCAN_COUNT {
        let port = base.saturating_add(offset);
        if port == 0 {
            continue;
        }
        if std::net::TcpListener::bind(("127.0.0.1", port)).is_ok() {
            return Some(port);
        }
    }
    None
}

/// 校验真实 JSON 协议，同时获得索引条数。
pub async fn probe_index_count(port: u16) -> Option<u64> {
    static CLIENT: std::sync::LazyLock<reqwest::Client> =
        std::sync::LazyLock::new(super::file_source::http_client);
    let url = format!("http://127.0.0.1:{port}/?search=&json=1&count=0");
    let response = CLIENT.get(url).send().await.ok()?;
    if !response.status().is_success() {
        return None;
    }
    let value = response.json::<serde_json::Value>().await.ok()?;
    value["results"].is_array().then_some(())?;
    value["totalResults"].as_u64()
}

/// 生成/覆写专属 ini（artifact 目录内，`-config` 显式指向）。
///
/// 键名为 Everything 1.4 实测生成的 canonical 键（2026-10-19 本机核对
/// Everything.ini）：`http_server_enabled` / `http_server_bindings` /
/// `http_server_port` / `etp_server_enabled` / `show_tray_icon`。
/// **必须带 `[Everything]` 段头 + CRLF 行尾**：无段头时全部键被 INI 解析
/// 忽略（实测 HTTP 不启动）；Everything 退出时回写 ini 补全默认键，本组键保持不变。
pub fn write_instance_ini(exe: &std::path::Path, port: u16) -> std::io::Result<PathBuf> {
    let Some(dir) = exe.parent() else {
        return Err(std::io::Error::other("exe 无父目录"));
    };
    let ini = dir.join(format!("everything-{INSTANCE_NAME}.ini"));
    let existing = match std::fs::read_to_string(&ini) {
        Ok(value) => value,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(error),
    };
    let content = merge_instance_ini(&existing, port);
    std::fs::write(&ini, content)?;
    Ok(ini)
}

/// 仅覆盖 Blink 管理的键，保留 Everything 回写的卷信息和索引偏好。
fn merge_instance_ini(existing: &str, port: u16) -> String {
    let mut fields = std::collections::BTreeMap::<String, String>::new();
    let mut in_section = false;
    for line in existing.trim_start_matches('\u{feff}').lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_section = line.eq_ignore_ascii_case("[Everything]");
            continue;
        }
        if in_section && let Some((key, value)) = line.split_once('=') {
            fields.insert(key.trim().to_ascii_lowercase(), value.to_string());
        }
    }
    for (key, value) in [
        ("instance_name", INSTANCE_NAME.to_string()),
        ("http_server_enabled", "1".into()),
        ("http_server_bindings", "127.0.0.1".into()),
        ("http_server_port", port.to_string()),
        ("etp_server_enabled", "0".into()),
        ("show_tray_icon", "0".into()),
        ("run_in_background", "1".into()),
        ("run_as_admin", "0".into()),
        ("auto_include_fixed_volumes", "1".into()),
        ("db_compress", "0".into()),
    ] {
        fields.insert(key.into(), value);
    }
    let mut content = String::from("[Everything]\r\n");
    for (key, value) in fields {
        content.push_str(&format!("{key}={value}\r\n"));
    }
    content
}

// ── 测试 ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_handles_have_one_lifecycle_owner() {
        let first = EverythingRuntime::shared();
        let second = EverythingRuntime::shared();
        assert!(Arc::ptr_eq(&first.inner, &second.inner));
        assert!(!Arc::ptr_eq(&first.inner, &EverythingRuntime::new().inner));
    }

    #[tokio::test]
    async fn stop_does_not_wait_for_start_recovery_gate_or_allow_queued_spawn() {
        let runtime = EverythingRuntime::new();
        let gate = runtime.inner.start_gate.lock().await;
        let pending = runtime.clone();
        let task = tokio::spawn(async move { pending.ensure_started(DEFAULT_MANAGED_PORT, SEARCH_START_BUDGET).await });
        tokio::task::yield_now().await;
        tokio::time::timeout(Duration::from_millis(200), runtime.stop()).await.expect("停止不得等待恢复锁");
        drop(gate);
        let result = tokio::time::timeout(Duration::from_secs(1), task).await.unwrap().unwrap();
        assert!(matches!(result, Err(StartError::Stopped)));
    }

    #[tokio::test]
    async fn ensure_started_without_install_returns_not_installed() {
        // cfg(test) 隔离根（临时目录）下未安装。
        let rt = EverythingRuntime::new();
        let err = rt
            .ensure_started(52819, SEARCH_START_BUDGET)
            .await
            .unwrap_err();
        assert!(matches!(err, StartError::NotInstalled));
        let snap = rt.snapshot();
        assert!(!snap.installed);
        assert!(!snap.running);
        assert!(snap.port.is_none());
    }

    #[tokio::test]
    async fn stop_is_idempotent_and_allows_restart() {
        let rt = EverythingRuntime::new();
        rt.stop().await;
        rt.stop().await;
        rt.resume();
        // 停止后 shutting_down 标记被复位：后续 ensure_started 不再被挡。
        let err = rt
            .ensure_started(52819, SEARCH_START_BUDGET)
            .await
            .unwrap_err();
        assert!(
            matches!(err, StartError::NotInstalled),
            "未被 ShuttingDown 挡住: {err:?}"
        );
    }

    /// 锁纪律回归：stop() 不得被 ensure_started 的等待循环阻塞。
    /// 未安装时 ensure_started 立即返回（不进等待段），此测试防退化——
    /// 若未来有人把长等待放回锁内，这里仍应瞬时完成。
    #[tokio::test]
    async fn stop_completes_immediately() {
        let rt = EverythingRuntime::new();
        let start = Instant::now();
        rt.stop().await;
        assert!(start.elapsed() < Duration::from_secs(2), "stop 不得长阻塞");
    }

    #[test]
    fn select_free_port_avoids_bound_ports() {
        let a = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let b = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port_a = a.local_addr().unwrap().port();
        let port_b = b.local_addr().unwrap().port();
        let (lo, hi) = if port_a < port_b {
            (port_a, port_b)
        } else {
            (port_b, port_a)
        };
        let picked = select_free_port(lo).expect("区间内应找到空闲端口");
        assert_ne!(picked, lo);
        assert_ne!(picked, hi);
    }

    #[test]
    fn instance_ini_contains_loopback_and_port() {
        let dir = std::env::temp_dir().join(format!("blink-ev-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let exe = dir.join("everything.exe");
        std::fs::write(&exe, b"MZ").unwrap();
        let ini = write_instance_ini(&exe, 52819).unwrap();
        assert_eq!(ini, dir.join("everything-blink.ini"));
        let content = std::fs::read_to_string(&ini).unwrap();
        assert!(
            content.starts_with("[Everything]\r\n"),
            "段头必需（无段头键全被忽略）"
        );
        assert!(content.contains("http_server_enabled=1\r\n"));
        assert!(
            content.contains("http_server_bindings=127.0.0.1"),
            "仅回环绑定"
        );
        assert!(content.contains("http_server_port=52819"));
        assert!(content.contains("etp_server_enabled=0"), "ETP 关闭");
        assert!(content.contains("show_tray_icon=0"), "托盘关闭");
        assert!(content.contains("instance_name=blink"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn instance_ini_preserves_discovered_volumes_and_updates_owned_keys() {
        let old = "[Everything]\r\nntfs_volume_guids=known-volume\r\nntfs_volume_includes=1\r\nhttp_server_port=1234\r\ndb_compress=1\r\n";
        let updated = merge_instance_ini(old, 52820);
        assert!(updated.contains("ntfs_volume_guids=known-volume\r\n"));
        assert!(updated.contains("ntfs_volume_includes=1\r\n"));
        assert!(updated.contains("http_server_port=52820\r\n"));
        assert!(!updated.contains("http_server_port=1234"));
        assert!(updated.contains("db_compress=0\r\n"));
        assert_eq!(merge_instance_ini(&updated, 52820), updated);
    }

    #[test]
    fn snapshot_reports_lock_version() {
        assert_eq!(EverythingRuntime::new().snapshot().version, "1.4.1.1032");
    }
}
