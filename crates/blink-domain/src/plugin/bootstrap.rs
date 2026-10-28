//! 插件数据集客户端自举（0.28.13）——词典等数据包插件安装后在本机
//! 「拉源 → 构建 → 挂载」的宿主侧操作管理。
//!
//! 架构切分（phase §五 0.28.13）：**插件拥有源声明与构建逻辑，宿主拥有
//! 传输、操作生命周期、调用方校验与挂载提交**。
//! - 源锁定声明（`bootstrap.json`）随插件包分发，安装期（install.rs）与
//!   运行期双重校验；运行时消息不能改写期望 URL/hash/bytes。
//! - 构建在插件进程内（其工作线程）执行——杀进程即有界终止，宿主不接
//!   管 `--build-*` 子进程；宿主只通过协议帧驱动（`dataset_init` →
//!   `fetch_resource`/`init_stage`/`install_dataset` → `init_result`）。
//! - 挂载复用 [`super::dataset::DatasetInstaller`] 内部的有界复制、逐文件
//!   校验、版本目录与 active.json 提交点；来源记 [`InstallSource::Managed`]。
//!
//! 操作模型：每 (插件, 数据集) 一个操作；重复自动 init、按钮重试与多窗口
//! 触发合并为一个在途操作；快照可查询（阶段/进度/可否取消/结构化错误），
//! 状态变化经 broadcast 通道由 app 层转发。取消/撤销杀插件进程并清理本次
//! op 目录；重启后孤儿 op 目录整体可弃（操作不跨重启存活，重试由用户触发）。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use blink_infra::plugin_storage::{InstallSource, PluginStorage};
use tokio::sync::mpsc;

use super::dataset::{ActiveVersion, DatasetError, DatasetInstaller, resolve_active_version};
use super::engine::PluginEngine;
use super::install::compare_versions;
use super::process::PluginHandle;
use super::protocol::{
    CAPABILITY_DATASET_INIT, PluginErrorPayload, PluginRequest, PluginUpstreamMessage,
};

/// bootstrap.json 支持的格式版本上限。
pub const SUPPORTED_BOOTSTRAP_FORMAT_VERSION: u32 = 1;
/// 源锁定文件名（插件包根，随包分发）。
pub const BOOTSTRAP_FILE: &str = "bootstrap.json";

/// 单个源文件的大小上限（准入实测 ECDICT ecdict.csv 约 63 MiB，基线 128 MiB
/// 保留裕量；声明超限直接拒绝，写入侧另以声明 bytes 为精确上限）。
pub const MAX_RESOURCE_BYTES: u64 = 128 * 1024 * 1024;
/// 单个插件全部源文件的总大小上限。
pub const MAX_TOTAL_RESOURCE_BYTES: u64 = 512 * 1024 * 1024;
/// 单次自举操作总时限（下载 + 构建 + 挂载；构建实测约 22s，留足余量）。
pub const OP_TOTAL_BUDGET: Duration = Duration::from_secs(45 * 60);
/// 单个资源下载的时限。
pub const RESOURCE_FETCH_BUDGET: Duration = Duration::from_secs(30 * 60);
/// 操作存活巡检间隔（进程退出/代次失效/禁用的发现时延上限）。
const LIVENESS_TICK: Duration = Duration::from_secs(2);
/// 进度事件最小发射粒度（字节）。
const PROGRESS_STEP_BYTES: u64 = 512 * 1024;

// ── 源锁定声明（bootstrap.json）──────────────────────────────────────────────

/// 单个锁定源资源。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct LockedResource {
    /// 资源 id（`[a-z0-9-]{1,64}`，插件按它发起 fetch_resource）。
    pub id: String,
    /// 原始 HTTPS URL（固定 commit 的 raw 地址；禁 master/latest 可变引用
    /// 由发布准入保证，此处强制 https）。
    pub url: String,
    /// 内容 sha256（64 位十六进制；镜像必须返回相同字节）。
    pub sha256: String,
    /// 实际字节数（写入硬上限与进度分母）。
    pub bytes: u64,
    /// 内容格式标签（如 "csv"/"text"；仅展示与溯源）。
    pub format: String,
    /// 上游许可（如 "MIT"）。
    pub license: String,
    /// 完整上游修订（commit SHA）。
    pub revision: String,
}

/// bootstrap.json 根结构：插件全部源锁定声明。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SourceLock {
    pub format_version: u32,
    pub resources: Vec<LockedResource>,
}

/// 源锁定解析/校验错误。
#[derive(Debug, thiserror::Error)]
pub enum SourceLockError {
    #[error("bootstrap.json 读取/解析失败: {0}")]
    Parse(String),
    #[error("源锁定声明非法: {0}")]
    Invalid(String),
}

/// 解析并静态校验源锁定声明。
pub fn parse_source_lock(raw: &str) -> Result<SourceLock, SourceLockError> {
    let lock: SourceLock =
        serde_json::from_str(raw).map_err(|e| SourceLockError::Parse(e.to_string()))?;
    if lock.format_version == 0 || lock.format_version > SUPPORTED_BOOTSTRAP_FORMAT_VERSION {
        return Err(SourceLockError::Invalid(format!(
            "format_version {} 不受支持（上限 {}）",
            lock.format_version, SUPPORTED_BOOTSTRAP_FORMAT_VERSION
        )));
    }
    if lock.resources.is_empty() || lock.resources.len() > 16 {
        return Err(SourceLockError::Invalid("resources 需在 1..=16 条".into()));
    }
    let mut total: u64 = 0;
    let mut seen = std::collections::HashSet::new();
    for res in &lock.resources {
        if res.id.is_empty()
            || res.id.len() > 64
            || !res
                .id
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        {
            return Err(SourceLockError::Invalid(format!(
                "资源 id 非法: {}",
                res.id
            )));
        }
        if !seen.insert(res.id.clone()) {
            return Err(SourceLockError::Invalid(format!(
                "资源 id 重复: {}",
                res.id
            )));
        }
        if !res.url.starts_with("https://") || res.url.len() > 2048 {
            return Err(SourceLockError::Invalid(format!(
                "资源 {} 的 URL 必须是 HTTPS 且 ≤2048 字符",
                res.id
            )));
        }
        if res.sha256.len() != 64 || !res.sha256.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(SourceLockError::Invalid(format!(
                "资源 {} 的 sha256 必须是 64 位十六进制",
                res.id
            )));
        }
        if res.bytes == 0 || res.bytes > MAX_RESOURCE_BYTES {
            return Err(SourceLockError::Invalid(format!(
                "资源 {} 的 bytes 需在 1..={MAX_RESOURCE_BYTES}",
                res.id
            )));
        }
        total = total.saturating_add(res.bytes);
        for (label, value) in [
            ("format", &res.format),
            ("license", &res.license),
            ("revision", &res.revision),
        ] {
            if value.is_empty() || value.len() > 128 {
                return Err(SourceLockError::Invalid(format!(
                    "资源 {} 的 {label} 需在 1..=128 字符",
                    res.id
                )));
            }
        }
    }
    if total > MAX_TOTAL_RESOURCE_BYTES {
        return Err(SourceLockError::Invalid(format!(
            "源文件总量 {total} 超过上限 {MAX_TOTAL_RESOURCE_BYTES}"
        )));
    }
    Ok(lock)
}

/// 从插件包根读取并校验源锁定声明（文件缺失 = 错误，不是空声明）。
pub fn load_source_lock(plugin_dir: &Path) -> Result<SourceLock, SourceLockError> {
    let path = plugin_dir.join(BOOTSTRAP_FILE);
    let raw = std::fs::read_to_string(&path)
        .map_err(|e| SourceLockError::Parse(format!("{} 读取失败: {e}", path.display())))?;
    parse_source_lock(&raw)
}

// ── 操作状态 ────────────────────────────────────────────────────────────────

/// 操作阶段（真实状态，不伪造构建百分比；下载进度是真实字节数）。
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum OpPhase {
    /// 发送 dataset_init 中。
    Starting,
    /// 宿主下载源文件（received/total 为真实字节）。
    FetchingResource {
        resource_id: String,
        received: u64,
        total: u64,
    },
    /// 插件侧阶段（building/verifying 等插件自定义短标签）。
    PluginStage {
        stage: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        note: Option<String>,
    },
    /// 宿主校验并挂载中。
    Mounting,
    /// 已挂载（active.json 已提交，等待 init_result 收尾）。
    Mounted {
        version: String,
    },
    Succeeded {
        version: String,
    },
    Failed {
        code: String,
        message: String,
    },
    Cancelled,
}

impl OpPhase {
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            OpPhase::Succeeded { .. } | OpPhase::Failed { .. } | OpPhase::Cancelled
        )
    }
}

/// 可查询的操作快照。
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct OpSnapshot {
    pub op_id: String,
    pub plugin_id: String,
    pub dataset_id: String,
    pub phase: OpPhase,
    /// 非终态即可取消。
    pub cancellable: bool,
    /// 触发来源（auto_install / enable / manual）。
    pub trigger: String,
}

// ── 传输抽象（测试可注入）───────────────────────────────────────────────────

/// 资源下载失败（分类错误码，进快照与回给插件的帧）。
#[derive(Debug, Clone)]
pub struct FetchFailure {
    pub code: &'static str,
    pub message: String,
}

/// 资源传输通道：宿主把候选源逐个降级下载到 dest 并校验哈希/大小。
#[async_trait::async_trait]
pub trait ResourceTransport: Send + Sync {
    async fn fetch(
        &self,
        candidates: &[String],
        expected_sha256: &str,
        declared_bytes: u64,
        dest: &Path,
        progress: &(dyn Fn(u64) + Send + Sync),
    ) -> Result<(), FetchFailure>;
}

/// 生产实现：infra net 工厂（代理策略生效）流式下载 + sha256 校验。
pub struct HttpResourceTransport;

#[async_trait::async_trait]
impl ResourceTransport for HttpResourceTransport {
    async fn fetch(
        &self,
        candidates: &[String],
        expected_sha256: &str,
        declared_bytes: u64,
        dest: &Path,
        progress: &(dyn Fn(u64) + Send + Sync),
    ) -> Result<(), FetchFailure> {
        use futures::StreamExt;
        use sha2::Digest;
        use tokio::io::AsyncWriteExt;

        let mut last_err = FetchFailure {
            code: "network_failed",
            message: "无候选端点".into(),
        };
        for url in candidates {
            let attempt = async {
                let client = blink_infra::net::client(blink_infra::net::RequestCategory::Download);
                let response =
                    tokio::time::timeout(Duration::from_secs(60), client.get(url).send())
                        .await
                        .map_err(|_| FetchFailure {
                            code: "timeout",
                            message: "连接超时".into(),
                        })?
                        .map_err(|e| FetchFailure {
                            code: "network_failed",
                            message: format!("下载失败: {e}"),
                        })?;
                if !response.status().is_success() {
                    return Err(FetchFailure {
                        code: "network_failed",
                        message: format!("HTTP {}", response.status()),
                    });
                }
                if let Some(len) = response.content_length()
                    && len > declared_bytes
                {
                    return Err(FetchFailure {
                        code: "over_limit",
                        message: format!("远端长度 {len} 超过锁定 {declared_bytes}"),
                    });
                }
                let mut file = tokio::fs::File::create(dest)
                    .await
                    .map_err(|e| FetchFailure {
                        code: "io",
                        message: format!("创建下载文件失败: {e}"),
                    })?;
                let mut stream = response.bytes_stream();
                let mut hasher = sha2::Sha256::new();
                let mut written: u64 = 0;
                let mut failure: Option<FetchFailure> = None;
                loop {
                    match stream.next().await {
                        Some(Ok(bytes)) => {
                            written += bytes.len() as u64;
                            if written > declared_bytes {
                                failure = Some(FetchFailure {
                                    code: "over_limit",
                                    message: format!("下载内容超过锁定大小（>{declared_bytes}）"),
                                });
                                break;
                            }
                            hasher.update(&bytes);
                            if file.write_all(&bytes).await.is_err() {
                                failure = Some(FetchFailure {
                                    code: "io",
                                    message: "写入失败（磁盘空间不足？）".into(),
                                });
                                break;
                            }
                            progress(written);
                        }
                        Some(Err(e)) => {
                            failure = Some(FetchFailure {
                                code: "network_failed",
                                message: format!("下载流中断: {e}"),
                            });
                            break;
                        }
                        None => break,
                    }
                }
                let _ = file.flush().await;
                if let Some(err) = failure {
                    let _ = tokio::fs::remove_file(dest).await;
                    return Err(err);
                }
                let actual = format!("{:x}", hasher.finalize());
                if !actual.eq_ignore_ascii_case(expected_sha256) {
                    let _ = tokio::fs::remove_file(dest).await;
                    return Err(FetchFailure {
                        code: "hash_mismatch",
                        message: format!("镜像内容与源锁定不符（候选源 {url}）"),
                    });
                }
                Ok(())
            };
            match attempt.await {
                Ok(()) => {
                    if Some(url.as_str()) != candidates.first().map(String::as_str) {
                        tracing::info!(mirror = %url, "源文件经镜像下载成功");
                    }
                    return Ok(());
                }
                Err(e) => {
                    tracing::debug!(
                        url = %url,
                        code = e.code,
                        error = %e.message,
                        "源下载候选失败"
                    );
                    last_err = e;
                }
            }
        }
        Err(last_err)
    }
}

// ── 错误 ────────────────────────────────────────────────────────────────────

/// begin 错误（结构化，命令层转 CommandError）。
#[derive(Debug, thiserror::Error)]
pub enum BootstrapError {
    #[error("插件未加载或已卸载")]
    NotLoaded,
    #[error("插件已禁用；启用后再准备词库")]
    Disabled,
    #[error("插件未声明 dataset_init 能力（宿主或插件版本过旧）")]
    Unsupported,
    #[error("数据集 {0} 未在插件 manifest 声明")]
    UndeclaredDataset(String),
    #[error("源锁定声明无效: {0}")]
    InvalidSourceLock(String),
    #[error("IO 错误: {0}")]
    Io(String),
}

/// begin 结果。
#[derive(Debug, Clone, PartialEq)]
pub enum BeginOutcome {
    Started(OpSnapshot),
    /// 同 (插件, 数据集) 已有在途操作（触发合并，返回现有快照）。
    AlreadyRunning(OpSnapshot),
    /// 数据集已挂载且版本兼容，无需自举。
    AlreadyReady,
}

// ── 管理器 ──────────────────────────────────────────────────────────────────

/// 上行自举帧（带进程身份）：process.rs stdout reader → 管理器事件循环。
pub type AuxInbound = (String, u64, PluginUpstreamMessage);

struct OpEntry {
    snapshot: std::sync::Mutex<OpSnapshot>,
    /// op task 收件箱（FetchResource/InstallDataset/InitStage/InitResult）。
    inbox: mpsc::UnboundedSender<PluginUpstreamMessage>,
    cancel: tokio::sync::watch::Sender<bool>,
    instance: std::sync::atomic::AtomicU64,
}

impl OpEntry {
    async fn cancelled(&self) {
        let mut rx = self.cancel.subscribe();
        let _ = rx.wait_for(|cancelled| *cancelled).await;
    }
}

/// 自举操作管理器：每插件每数据集一个操作，长任务在独立 tokio task。
pub struct BootstrapManager {
    /// 宿主注入运行时；同步 setup 构造时不依赖当前线程已进入 Tokio。
    runtime: tokio::runtime::Handle,
    engine: Arc<PluginEngine>,
    storage: PluginStorage,
    dataset_installer: Arc<DatasetInstaller>,
    transport: Arc<dyn ResourceTransport>,
    ops: std::sync::Mutex<HashMap<String, Arc<OpEntry>>>,
    events: tokio::sync::broadcast::Sender<OpSnapshot>,
    aux_tx: mpsc::UnboundedSender<AuxInbound>,
    /// 事件循环任务句柄（持引用保活；构造后填充）。
    _loop: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl BootstrapManager {
    pub fn new(
        engine: Arc<PluginEngine>,
        storage: PluginStorage,
        dataset_installer: Arc<DatasetInstaller>,
        transport: Arc<dyn ResourceTransport>,
        runtime: tokio::runtime::Handle,
    ) -> Arc<Self> {
        let (events, _) = tokio::sync::broadcast::channel(64);
        let (aux_tx, mut aux_rx) = mpsc::unbounded_channel::<AuxInbound>();
        let manager = Arc::new(BootstrapManager {
            runtime,
            engine,
            storage,
            dataset_installer,
            transport,
            ops: std::sync::Mutex::new(HashMap::new()),
            events,
            aux_tx,
            _loop: std::sync::Mutex::new(None),
        });
        // 事件循环：上行帧 → 对应 op 收件箱；孤儿帧回结构化错误。
        let weak = Arc::downgrade(&manager);
        let loop_handle = manager.runtime.spawn(async move {
            while let Some((plugin_id, instance, msg)) = aux_rx.recv().await {
                if let Some(manager) = weak.upgrade() {
                    manager.route_aux(&plugin_id, instance, msg).await;
                }
            }
        });
        *manager._loop.lock().unwrap() = Some(loop_handle);
        manager
    }

    /// process.rs stdout reader 注入用发送端（经 engine 盖戳到各句柄）。
    pub fn aux_sender(&self) -> mpsc::UnboundedSender<AuxInbound> {
        self.aux_tx.clone()
    }

    /// 状态变化订阅（app 层转发为前端事件）。
    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<OpSnapshot> {
        self.events.subscribe()
    }

    /// 某插件全部操作快照（含终态，供状态查询）。
    pub fn status(&self, plugin_id: &str) -> Vec<OpSnapshot> {
        self.ops
            .lock()
            .unwrap()
            .values()
            .filter(|e| e.snapshot.lock().unwrap().plugin_id == plugin_id)
            .map(|e| e.snapshot.lock().unwrap().clone())
            .collect()
    }

    /// 某插件某数据集的当前操作快照。
    pub fn status_for(&self, plugin_id: &str, dataset_id: &str) -> Option<OpSnapshot> {
        self.ops.lock().unwrap().values().find_map(|e| {
            let snap = e.snapshot.lock().unwrap();
            (snap.plugin_id == plugin_id && snap.dataset_id == dataset_id).then(|| snap.clone())
        })
    }

    /// 启动（或合并到）一次自举操作。
    pub async fn begin(
        self: &Arc<Self>,
        plugin_id: &str,
        dataset_id: &str,
        trigger: &str,
    ) -> Result<BeginOutcome, BootstrapError> {
        if let Some(existing) = self.status_for(plugin_id, dataset_id)
            && !existing.phase.is_terminal()
        {
            return Ok(BeginOutcome::AlreadyRunning(existing));
        }
        let handle = self
            .engine
            .all_plugins()
            .into_iter()
            .find(|h| h.id() == plugin_id)
            .ok_or(BootstrapError::NotLoaded)?;
        if !self.engine.is_active_handle(plugin_id, &handle) {
            return Err(BootstrapError::NotLoaded);
        }
        if !self.engine.is_enabled(plugin_id) {
            return Err(BootstrapError::Disabled);
        }
        let manifest = handle.manifest();
        if !manifest
            .capabilities
            .iter()
            .any(|c| c == CAPABILITY_DATASET_INIT)
        {
            return Err(BootstrapError::Unsupported);
        }
        let def = manifest
            .datasets
            .iter()
            .find(|d| d.id == dataset_id)
            .ok_or_else(|| BootstrapError::UndeclaredDataset(dataset_id.to_string()))?;
        let source_lock = load_source_lock(handle.package_dir())
            .map_err(|e| BootstrapError::InvalidSourceLock(e.to_string()))?;
        // 三态核验：已挂载且 ≥ min_version → 无需自举（兼容保留库重装跳过构建）。
        if trigger != "manual"
            && let Some(base) = handle.dirs().map(|d| PathBuf::from(d.datasets))
            && let ActiveVersion::Mounted(version) = resolve_active_version(&base, dataset_id)
            && def.min_version.as_ref().map_or(true, |min| {
                compare_versions(&version, min) != std::cmp::Ordering::Less
            })
        {
            let dataset = dataset_id.to_string();
            if tokio::task::spawn_blocking(move || {
                super::dataset::verify_active_dataset(&base, &dataset, &version)
            })
            .await
            .unwrap_or(false)
            {
                return Ok(BeginOutcome::AlreadyReady);
            }
        }

        // 同一插件的启动、撤销和最终提交共享栅栏，检查与注册不能分开竞争。
        let fence = handle.bootstrap_fence();
        let allowed = fence.lock().await;
        if !*allowed
            || !self.engine.is_enabled(plugin_id)
            || !self.engine.is_active_handle(plugin_id, &handle)
        {
            return Err(BootstrapError::Disabled);
        }
        if let Some(existing) = self.status_for(plugin_id, dataset_id)
            && !existing.phase.is_terminal()
        {
            return Ok(BeginOutcome::AlreadyRunning(existing));
        }
        let op_id = format!("boot-{:x}", now_nanos());
        let work_dir = self.storage.cache_dir(plugin_id).join("ops").join(&op_id);
        std::fs::create_dir_all(work_dir.join("resources")).map_err(|e| {
            BootstrapError::Io(format!("创建操作目录失败（{}）: {e}", work_dir.display()))
        })?;

        let snapshot = OpSnapshot {
            op_id: op_id.clone(),
            plugin_id: plugin_id.to_string(),
            dataset_id: dataset_id.to_string(),
            phase: OpPhase::Starting,
            cancellable: true,
            trigger: trigger.to_string(),
        };
        let (inbox_tx, inbox_rx) = mpsc::unbounded_channel();
        let entry = Arc::new(OpEntry {
            snapshot: std::sync::Mutex::new(snapshot.clone()),
            inbox: inbox_tx,
            cancel: tokio::sync::watch::channel(false).0,
            instance: std::sync::atomic::AtomicU64::new(0),
        });
        {
            let mut ops = self.ops.lock().unwrap();
            // 同 (插件, 数据集) 的终态旧快照让位（重试覆盖）。
            ops.retain(|_, e| {
                let s = e.snapshot.lock().unwrap();
                !(s.plugin_id == plugin_id && s.dataset_id == dataset_id && s.phase.is_terminal())
            });
            ops.insert(op_id.clone(), entry.clone());
        }
        let runner = Arc::clone(self);
        let run_handle = handle.clone();
        let run_lock = source_lock;
        let run_work = work_dir;
        self.runtime.spawn(async move {
            runner
                .run_op(entry, inbox_rx, run_handle, run_lock, run_work)
                .await;
        });
        self.emit(&snapshot);
        Ok(BeginOutcome::Started(snapshot))
    }

    /// 取消在途操作（用户按钮）：标记 → 杀插件进程（构建随进程终止）→ 清目录。
    pub async fn cancel(&self, plugin_id: &str, dataset_id: &str) -> bool {
        let Some(entry) = self.find_op(plugin_id, dataset_id) else {
            return false;
        };
        // commit 若已开始，等它完成并如实返回“已不可取消”；准备阶段的取消
        // 与最终提交获取同一关卡，不会漏通知或在返回后迟到挂载。
        if let Some(handle) = self
            .engine
            .all_plugins()
            .into_iter()
            .find(|h| h.id() == plugin_id)
        {
            let fence = handle.bootstrap_fence();
            let _guard = fence.lock().await;
            if !entry.snapshot.lock().unwrap().cancellable {
                return false;
            }
            entry.cancel.send_replace(true);
        } else {
            entry.cancel.send_replace(true);
        }
        true
    }

    /// 显式启用/安装成功后才允许新自举；旧操作的取消标记永不清除。
    pub async fn resume_plugin(&self, handle: &Arc<PluginHandle>) {
        *handle.bootstrap_fence().lock().await = true;
    }

    /// 撤销某插件全部操作（禁用/更新/卸载联动）。旧代次不得挂载/复活：
    /// 立即置 Failed 终态（后续帧/迟到挂载被终态守卫与代次复核拒绝），
    /// 并杀进程兜底（禁用路径不经发布器 reset）。
    pub async fn revoke_plugin(&self, handle: &Arc<PluginHandle>, reason: &str) {
        let fence = handle.bootstrap_fence();
        let mut allowed = fence.lock().await;
        *allowed = false;
        let entries: Vec<Arc<OpEntry>> = self
            .ops
            .lock()
            .unwrap()
            .values()
            .filter(|e| e.snapshot.lock().unwrap().plugin_id == handle.id())
            .cloned()
            .collect();
        for entry in entries {
            entry.cancel.send_replace(true);
            self.set_phase(
                &entry,
                OpPhase::Failed {
                    code: "plugin_revoked".into(),
                    message: reason.to_string(),
                },
            );
        }
        drop(allowed);
        handle.reset_process().await;
    }

    /// 启动恢复：操作不跨重启——cache/*/ops/ 下全部目录视为孤儿清理。
    /// 返回清理的 op 目录数（失败仅告警，下次启动重试）。
    pub fn recover_orphan_ops(&self) -> usize {
        let cache_root = self.storage.root().join("cache");
        let Ok(plugin_dirs) = std::fs::read_dir(&cache_root) else {
            return 0;
        };
        let mut cleaned = 0;
        for plugin_dir in plugin_dirs.flatten() {
            let ops_dir = plugin_dir.path().join("ops");
            let Ok(entries) = std::fs::read_dir(&ops_dir) else {
                continue;
            };
            for op_dir in entries.flatten() {
                let p = op_dir.path();
                if std::fs::remove_dir_all(&p).is_ok() {
                    cleaned += 1;
                } else {
                    tracing::warn!(path = %p.display(), "自举孤儿操作目录清理失败（下次启动重试）");
                }
            }
        }
        if cleaned > 0 {
            tracing::info!(count = cleaned, "已清理自举孤儿操作目录");
        }
        cleaned
    }

    // ── 内部 ────────────────────────────────────────────────────────────────

    fn find_op(&self, plugin_id: &str, dataset_id: &str) -> Option<Arc<OpEntry>> {
        self.ops.lock().unwrap().values().find_map(|e| {
            let s = e.snapshot.lock().unwrap();
            (s.plugin_id == plugin_id && s.dataset_id == dataset_id && !s.phase.is_terminal())
                .then(|| Arc::clone(e))
        })
    }

    fn set_phase(&self, entry: &OpEntry, phase: OpPhase) {
        let mut snap = entry.snapshot.lock().unwrap();
        if snap.phase.is_terminal() {
            return; // 终态不回退（迟到事件不污染新会话）
        }
        snap.cancellable = !phase.is_terminal();
        snap.phase = phase;
        let snapshot = snap.clone();
        drop(snap);
        self.emit(&snapshot);
    }

    fn finish(&self, entry: &OpEntry, phase: OpPhase, work_dir: &Path) {
        self.set_phase(entry, phase);
        let dir = work_dir.to_path_buf();
        tokio::task::spawn_blocking(move || {
            let _ = std::fs::remove_dir_all(&dir);
        });
    }

    fn emit(&self, snapshot: &OpSnapshot) {
        let _ = self.events.send(snapshot.clone());
    }

    /// 事件循环分派：孤儿请求帧回结构化错误（op 不存在或已结束）。
    async fn route_aux(&self, plugin_id: &str, instance: u64, msg: PluginUpstreamMessage) {
        let (op_id, request) = match &msg {
            PluginUpstreamMessage::FetchResource { id, op_id, .. } => (
                op_id.clone(),
                Some(AuxRequest {
                    id: id.clone(),
                    kind: AuxReplyKind::Resource,
                }),
            ),
            PluginUpstreamMessage::InstallDataset { id, op_id, .. } => (
                op_id.clone(),
                Some(AuxRequest {
                    id: id.clone(),
                    kind: AuxReplyKind::Dataset,
                }),
            ),
            PluginUpstreamMessage::InitStage { op_id, .. }
            | PluginUpstreamMessage::InitResult { op_id, .. } => (op_id.clone(), None),
            _ => return,
        };
        let entry = self.ops.lock().unwrap().get(&op_id).cloned();
        match entry {
            Some(entry) => {
                let snapshot = entry.snapshot.lock().unwrap().clone();
                if snapshot.plugin_id != plugin_id
                    || entry.instance.load(std::sync::atomic::Ordering::Acquire) != instance
                    || snapshot.phase.is_terminal()
                    || *entry.cancel.borrow()
                {
                    tracing::warn!(plugin = plugin_id, op = %op_id, "拒绝非所属进程或已撤销操作的上行帧");
                    return;
                }
                let _ = entry.inbox.send(msg);
            }
            None => match request {
                Some(req) => {
                    tracing::warn!(plugin = plugin_id, op = %op_id, "未知自举操作的上行帧，回结构化错误");
                    self.reply_error(plugin_id, &req, &op_id, "unknown_op", "操作不存在或已结束")
                        .await;
                }
                None => {
                    tracing::debug!(plugin = plugin_id, op = %op_id, "未知操作的 stage/result 帧（忽略）");
                }
            },
        }
    }

    /// 给插件进程回一帧错误（句柄缺失/进程已死 = 静默，op 由巡检收敛）。
    async fn reply_error(
        &self,
        plugin_id: &str,
        req: &AuxRequest,
        op_id: &str,
        code: &str,
        message: &str,
    ) {
        let error = PluginErrorPayload {
            code: code.into(),
            message: message.into(),
        };
        let frame = match req.kind {
            AuxReplyKind::Resource => PluginRequest::ResourceResult {
                id: req.id.clone(),
                op_id: op_id.into(),
                path: None,
                bytes: None,
                sha256: None,
                revision: None,
                license: None,
                error: Some(error),
            },
            AuxReplyKind::Dataset => PluginRequest::DatasetResult {
                id: req.id.clone(),
                op_id: op_id.into(),
                version: None,
                error: Some(error),
            },
        };
        self.write_aux(plugin_id, frame).await;
    }

    async fn write_aux(&self, plugin_id: &str, frame: PluginRequest) {
        let Some(handle) = self
            .engine
            .all_plugins()
            .into_iter()
            .find(|h| h.id() == plugin_id)
        else {
            return;
        };
        let op_id = match &frame {
            PluginRequest::ResourceResult { op_id, .. }
            | PluginRequest::DatasetResult { op_id, .. } => op_id,
            _ => return,
        };
        let entry = self.ops.lock().unwrap().get(op_id).cloned();
        if let Some(entry) = entry {
            if entry.snapshot.lock().unwrap().plugin_id != plugin_id {
                return;
            }
            let instance = entry.instance.load(std::sync::atomic::Ordering::Acquire);
            handle.write_aux_frame(&frame, instance).await;
        }
    }

    /// 操作主体：驱动插件完成 fetch → build → install，全程巡检进程/代次/禁用。
    async fn run_op(
        self: &Arc<Self>,
        entry: Arc<OpEntry>,
        mut inbox: mpsc::UnboundedReceiver<PluginUpstreamMessage>,
        handle: Arc<PluginHandle>,
        source_lock: SourceLock,
        work_dir: PathBuf,
    ) {
        let fence = handle.bootstrap_fence();
        let allowed = fence.lock().await;
        if !*allowed
            || *entry.cancel.borrow()
            || !self.engine.is_active_handle(handle.id(), &handle)
            || !self.engine.is_enabled(handle.id())
        {
            self.finish(&entry, OpPhase::Cancelled, &work_dir);
            return;
        }
        let initial = entry.snapshot.lock().unwrap().clone();
        let plugin_id = initial.plugin_id.clone();
        let op_id = initial.op_id.clone();
        let dataset_id = initial.dataset_id.clone();
        let deadline = tokio::time::Instant::now() + OP_TOTAL_BUDGET;

        // 发 dataset_init（不设应答 deadline——完成以 init_result 为准，
        // 进程退出/超预算由下方巡检与 select 兜底）。
        let instance = match handle
            .begin_dataset_init(
                &op_id,
                &dataset_id,
                &work_dir.to_string_lossy(),
                &entry.instance,
            )
            .await
        {
            Ok(instance) => instance,
            Err(e) => {
                self.finish(
                    &entry,
                    OpPhase::Failed {
                        code: "spawn_failed".into(),
                        message: e.to_string(),
                    },
                    &work_dir,
                );
                return;
            }
        };

        drop(allowed);
        let mut mounted_version: Option<String> = None;
        let mut last_revision: Option<String> = None;
        let mut tick = tokio::time::interval(LIVENESS_TICK);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            tokio::select! {
                biased;
                _ = entry.cancelled() => {
                    break;
                }
                msg = inbox.recv() => {
                    let Some(msg) = msg else {
                        // 收件箱关闭（管理器内部不变量破坏）：如实失败，不悬挂。
                        self.finish(&entry, OpPhase::Failed {
                            code: "internal".into(),
                            message: "自举操作收件箱意外关闭".into(),
                        }, &work_dir);
                        return;
                    };
                    match msg {
                        PluginUpstreamMessage::FetchResource { id, op_id: msg_op, resource_id } => {
                            if msg_op != op_id { continue; }
                            let outcome = self.handle_fetch(
                                &entry, &source_lock, &work_dir, &plugin_id, &id,
                                &resource_id, &mut last_revision,
                            ).await;
                            if outcome.is_cancelled() {
                                break;
                            }
                        }
                        PluginUpstreamMessage::InstallDataset { id, op_id: msg_op, dataset_id: msg_dataset, source_dir } => {
                            if msg_op != op_id { continue; }
                            self.handle_install(
                                &entry, &handle, &work_dir, &plugin_id, &dataset_id, &id,
                                &msg_dataset, &source_dir, last_revision.as_deref(),
                                &mut mounted_version,
                            ).await;
                        }
                        PluginUpstreamMessage::InitStage { op_id: msg_op, stage, note } => {
                            if msg_op == op_id {
                                self.set_phase(&entry, OpPhase::PluginStage { stage, note });
                            }
                        }
                        PluginUpstreamMessage::InitResult { op_id: msg_op, error, .. } => {
                            if msg_op != op_id { continue; }
                            // 已挂载则挂载结果为准（插件迟到失败不回滚提交点）。
                            if let Some(version) = mounted_version.clone() {
                                self.finish(&entry, OpPhase::Succeeded { version }, &work_dir);
                                return;
                            }
                            match error {
                                None => {
                                    self.finish(&entry, OpPhase::Failed {
                                        code: "missing_mount".into(), message: "插件未挂载数据集就报告完成".into()
                                    }, &work_dir);
                                    return;
                                }
                                Some(err) => {
                                    self.finish(&entry, OpPhase::Failed { code: err.code, message: err.message }, &work_dir);
                                    return;
                                }
                            }
                        }
                        _ => {}
                    }
                }
                _ = tick.tick() => {
                    // 巡检：代次失效 / 进程退出（含被发布器杀掉的旧代次）/ 禁用。
                    if !self.engine.is_active_handle(&plugin_id, &handle) {
                        self.finish(&entry, OpPhase::Failed {
                            code: "plugin_revoked".into(),
                            message: "插件已更新或卸载，操作代次失效".into(),
                        }, &work_dir);
                        return;
                    }
                    if handle.current_process_instance().await != Some(instance) {
                        self.finish(&entry, OpPhase::Failed {
                            code: "process_lost".into(),
                            message: "插件进程退出（崩溃或被终止），操作中止".into(),
                        }, &work_dir);
                        return;
                    }
                    if !self.engine.is_enabled(&plugin_id) {
                        self.finish(&entry, OpPhase::Failed {
                            code: "plugin_disabled".into(),
                            message: "插件已禁用，操作中止".into(),
                        }, &work_dir);
                        return;
                    }
                }
                _ = tokio::time::sleep_until(deadline) => {
                    handle.reset_process().await;
                    self.finish(&entry, OpPhase::Failed {
                        code: "timeout".into(),
                        message: format!("自举操作超过总时限（{} 分钟）", OP_TOTAL_BUDGET.as_secs() / 60),
                    }, &work_dir);
                    return;
                }
            }
        }

        // 循环仅经取消路径退出（其余出口直接 return）：杀进程终止构建
        // （kill_on_drop），清理本次 op 目录。
        handle.reset_process().await;
        self.finish(&entry, OpPhase::Cancelled, &work_dir);
    }

    /// fetch_resource：源锁定解析 → 候选下载（哈希/限额）→ 受限相对路径回帧。
    /// 取消可在下载中途生效（select 丢弃传输 future，半成品随目录清理）。
    async fn handle_fetch(
        &self,
        entry: &Arc<OpEntry>,
        source_lock: &SourceLock,
        work_dir: &Path,
        plugin_id: &str,
        id: &str,
        resource_id: &str,
        last_revision: &mut Option<String>,
    ) -> FetchFlowOutcome {
        let Some(resource) = source_lock.resources.iter().find(|r| r.id == resource_id) else {
            let op_id = entry.snapshot.lock().unwrap().op_id.clone();
            self.reply_error(
                plugin_id,
                &AuxRequest {
                    id: id.into(),
                    kind: AuxReplyKind::Resource,
                },
                &op_id,
                "unknown_resource",
                &format!("资源 {resource_id} 不在源锁定声明中"),
            )
            .await;
            return FetchFlowOutcome::Continue;
        };
        let dest = work_dir.join("resources").join(&resource.id);
        let total = resource.bytes;
        self.set_phase(
            entry,
            OpPhase::FetchingResource {
                resource_id: resource.id.clone(),
                received: 0,
                total,
            },
        );
        // 进度回调 → 快照（节流广播事件）。闭包需 'static，持 Arc<OpEntry>。
        let last_emitted = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let events = self.events.clone();
        let progress_entry = Arc::clone(entry);
        let progress = move |received: u64| {
            let snapshot = {
                let mut snap = progress_entry.snapshot.lock().unwrap();
                if let OpPhase::FetchingResource { received: r, .. } = &mut snap.phase {
                    *r = received;
                }
                snap.clone()
            };
            let prev = last_emitted.load(std::sync::atomic::Ordering::Relaxed);
            if received.saturating_sub(prev) >= PROGRESS_STEP_BYTES || received >= total {
                last_emitted.store(received, std::sync::atomic::Ordering::Relaxed);
                let _ = events.send(snapshot);
            }
        };

        let candidates = blink_infra::utils::mirrors::download_candidates(&resource.url);
        let transport = Arc::clone(&self.transport);
        let expected = resource.sha256.clone();
        let declared = resource.bytes;
        let fetch_fut = async {
            tokio::time::timeout(
                RESOURCE_FETCH_BUDGET,
                transport.fetch(&candidates, &expected, declared, &dest, &progress),
            )
            .await
        };
        let result = tokio::select! {
            biased;
            _ = entry.cancelled() => return FetchFlowOutcome::Cancelled,
            r = fetch_fut => r,
        };
        let result = match result {
            Ok(inner) => inner,
            Err(_) => Err(FetchFailure {
                code: "timeout",
                message: format!(
                    "源下载超过时限（{} 分钟）",
                    RESOURCE_FETCH_BUDGET.as_secs() / 60
                ),
            }),
        };
        match result {
            Ok(()) => {
                *last_revision = Some(resource.revision.clone());
                let op_id = entry.snapshot.lock().unwrap().op_id.clone();
                self.write_aux(
                    plugin_id,
                    PluginRequest::ResourceResult {
                        id: id.into(),
                        op_id,
                        path: Some(format!("resources/{}", resource.id)),
                        bytes: Some(resource.bytes),
                        sha256: Some(resource.sha256.clone()),
                        revision: Some(resource.revision.clone()),
                        license: Some(resource.license.clone()),
                        error: None,
                    },
                )
                .await;
                self.set_phase(
                    entry,
                    OpPhase::PluginStage {
                        stage: "fetched".into(),
                        note: Some(resource.id.clone()),
                    },
                );
            }
            Err(e) => {
                tracing::warn!(plugin = plugin_id, resource = resource_id, code = e.code, error = %e.message, "源文件下载失败（全部候选）");
                let op_id = entry.snapshot.lock().unwrap().op_id.clone();
                self.reply_error(
                    plugin_id,
                    &AuxRequest {
                        id: id.into(),
                        kind: AuxReplyKind::Resource,
                    },
                    &op_id,
                    e.code,
                    &e.message,
                )
                .await;
            }
        }
        FetchFlowOutcome::Continue
    }

    /// install_dataset：归属/路径/声明校验 → 复用安装器提交 → 回帧。
    async fn handle_install(
        &self,
        entry: &Arc<OpEntry>,
        handle: &Arc<PluginHandle>,
        work_dir: &Path,
        plugin_id: &str,
        op_dataset: &str,
        id: &str,
        msg_dataset: &str,
        source_dir: &str,
        last_revision: Option<&str>,
        mounted_version: &mut Option<String>,
    ) {
        let fail = |code: &str, message: String| AuxInstallError {
            code: code.to_string(),
            message,
        };
        if msg_dataset != op_dataset {
            self.install_error_reply(
                plugin_id,
                id,
                entry,
                fail(
                    "dataset_mismatch",
                    format!("install_dataset 数据集（{msg_dataset}）与操作（{op_dataset}）不符"),
                ),
            )
            .await;
            return;
        }
        // 受限相对路径：拒绝绝对路径/反斜杠/空段/`.`/`..`；canonicalize 前缀
        // 拒绝符号链接逃逸。
        if source_dir.is_empty()
            || source_dir.contains('\\')
            || source_dir
                .split('/')
                .any(|seg| seg.is_empty() || seg == "." || seg == "..")
        {
            self.install_error_reply(
                plugin_id,
                id,
                entry,
                fail(
                    "path_escape",
                    "source_dir 必须是 work_dir 下的正规相对路径".into(),
                ),
            )
            .await;
            return;
        }
        let full = work_dir.join(source_dir);
        let canonical_root = work_dir
            .canonicalize()
            .unwrap_or_else(|_| work_dir.to_path_buf());
        let canonical = match full.canonicalize() {
            Ok(c) => c,
            Err(_) => {
                self.install_error_reply(
                    plugin_id,
                    id,
                    entry,
                    fail("missing_output", format!("构建输出不存在（{source_dir}）")),
                )
                .await;
                return;
            }
        };
        if !canonical.starts_with(&canonical_root) {
            self.install_error_reply(
                plugin_id,
                id,
                entry,
                fail(
                    "path_escape",
                    "source_dir 越界（不在本操作工作目录内）".into(),
                ),
            )
            .await;
            return;
        }
        if !canonical.join("dataset.json").is_file() {
            self.install_error_reply(
                plugin_id,
                id,
                entry,
                fail("missing_output", "构建输出根下缺少 dataset.json".into()),
            )
            .await;
            return;
        }
        // 提交前复核：代次仍有效、仍启用（与禁用/更新/卸载的撤销窗口互斥；
        // 最后复核与提交在 install_managed_dir_checked 的生命周期许可内）。
        if !self.engine.is_active_handle(plugin_id, handle) {
            self.install_error_reply(
                plugin_id,
                id,
                entry,
                fail(
                    "plugin_revoked",
                    "插件代次已失效（更新/卸载中），拒绝挂载".into(),
                ),
            )
            .await;
            return;
        }
        if !self.engine.is_enabled(plugin_id) {
            self.install_error_reply(
                plugin_id,
                id,
                entry,
                fail("plugin_disabled", "插件已禁用，拒绝挂载".into()),
            )
            .await;
            return;
        }
        // 溯源：dataset.json 的 source.revision 优先（构建器写入上游修订）。
        let revision = std::fs::read_to_string(canonical.join("dataset.json"))
            .ok()
            .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
            .and_then(|v| v["source"]["revision"].as_str().map(str::to_string))
            .filter(|r| !r.is_empty())
            .or_else(|| last_revision.map(str::to_string))
            .unwrap_or_else(|| "unspecified".into());
        let op_id = entry.snapshot.lock().unwrap().op_id.clone();
        let source = InstallSource::Managed {
            plugin_version: handle.manifest().version.clone(),
            source_revision: revision,
            operation: op_id.clone(),
        };

        self.set_phase(entry, OpPhase::Mounting);
        let installer = Arc::clone(&self.dataset_installer);
        let expect_plugin = plugin_id.to_string();
        let expect_dataset = op_dataset.to_string();
        let dir = canonical.clone();
        let fence = handle.bootstrap_fence();
        let commit_entry = Arc::clone(entry);
        let engine = Arc::clone(&self.engine);
        let commit_handle = Arc::clone(handle);
        let joined = tokio::task::spawn_blocking(move || {
            installer.install_managed_dir_checked(
                &dir,
                &expect_plugin,
                &expect_dataset,
                source,
                || {
                    let permit = fence.blocking_lock_owned();
                    if !*permit
                        || *commit_entry.cancel.borrow()
                        || !engine.is_enabled(commit_handle.id())
                        || !engine.is_active_handle(commit_handle.id(), &commit_handle)
                    {
                        return Err(DatasetError::InvalidManifest(
                            "操作已取消或插件已撤销，拒绝提交".into(),
                        ));
                    }
                    commit_entry.snapshot.lock().unwrap().cancellable = false;
                    Ok(permit)
                },
            )
        })
        .await;
        let result = match joined {
            Ok(inner) => inner,
            Err(e) => Err(DatasetError::Io(format!("挂载任务失败: {e}"))),
        };
        match result {
            Ok(record) => {
                *mounted_version = Some(record.version.clone());
                self.write_aux(
                    plugin_id,
                    PluginRequest::DatasetResult {
                        id: id.into(),
                        op_id,
                        version: Some(record.version.clone()),
                        error: None,
                    },
                )
                .await;
                self.set_phase(
                    entry,
                    OpPhase::Mounted {
                        version: record.version.clone(),
                    },
                );
                tracing::info!(
                    plugin = plugin_id,
                    dataset = op_dataset,
                    version = %record.version,
                    "自举数据集已挂载"
                );
            }
            Err(e) => {
                tracing::warn!(plugin = plugin_id, dataset = op_dataset, %e, "自举数据集挂载失败");
                self.install_error_reply(plugin_id, id, entry, fail("mount_failed", e.to_string()))
                    .await;
            }
        }
    }

    async fn install_error_reply(
        &self,
        plugin_id: &str,
        id: &str,
        entry: &OpEntry,
        err: AuxInstallError,
    ) {
        let op_id = entry.snapshot.lock().unwrap().op_id.clone();
        self.reply_error(
            plugin_id,
            &AuxRequest {
                id: id.into(),
                kind: AuxReplyKind::Dataset,
            },
            &op_id,
            &err.code,
            &err.message,
        )
        .await;
    }
}

struct AuxRequest {
    id: String,
    kind: AuxReplyKind,
}

#[derive(Clone, Copy)]
enum AuxReplyKind {
    Resource,
    Dataset,
}

struct AuxInstallError {
    code: String,
    message: String,
}

enum FetchFlowOutcome {
    Continue,
    Cancelled,
}

impl FetchFlowOutcome {
    fn is_cancelled(&self) -> bool {
        matches!(self, FetchFlowOutcome::Cancelled)
    }
}

fn now_nanos() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── 源锁定校验 ──────────────────────────────────────────────────────────

    fn valid_lock_json() -> String {
        serde_json::json!({
            "format_version": 1,
            "resources": [{
                "id": "ecdict-csv",
                "url": "https://raw.githubusercontent.com/skywind3000/ECDICT/8defb761f7c7ad1818ca94290a1844d7b33d6b23/ecdict.csv",
                "sha256": "d0ce61e560b50d9905d20de3173aa3ca80950ce235bedd21c53e025cf9f38cb0",
                "bytes": 65936699,
                "format": "csv",
                "license": "MIT",
                "revision": "8defb761f7c7ad1818ca94290a1844d7b33d6b23"
            }]
        })
        .to_string()
    }

    #[test]
    fn source_lock_parses_valid_declaration() {
        let lock = parse_source_lock(&valid_lock_json()).unwrap();
        assert_eq!(lock.resources.len(), 1);
        assert_eq!(lock.resources[0].id, "ecdict-csv");
        assert_eq!(lock.resources[0].bytes, 65936699);
    }

    #[test]
    fn source_lock_rejects_http_bad_hash_zero_bytes_and_dup_ids() {
        let base: serde_json::Value = serde_json::from_str(&valid_lock_json()).unwrap();

        let mut v = base.clone();
        v["resources"][0]["url"] = "http://example.com/ecdict.csv".into();
        assert!(parse_source_lock(&v.to_string()).is_err());
        let mut v = base.clone();
        v["resources"][0]["sha256"] = "xyz".into();
        assert!(parse_source_lock(&v.to_string()).is_err());
        let mut v = base.clone();
        v["resources"][0]["bytes"] = 0.into();
        assert!(parse_source_lock(&v.to_string()).is_err());
        let mut v = base.clone();
        v["resources"][0]["bytes"] = (MAX_RESOURCE_BYTES + 1).into();
        assert!(parse_source_lock(&v.to_string()).is_err());
        let mut v = base.clone();
        v["resources"][0]["id"] = "Bad_Id".into();
        assert!(parse_source_lock(&v.to_string()).is_err());
        let mut v = base.clone();
        let r = v["resources"][0].clone();
        v["resources"].as_array_mut().unwrap().push(r);
        assert!(parse_source_lock(&v.to_string()).is_err());
        let mut v = base.clone();
        v["format_version"] = 2.into();
        assert!(parse_source_lock(&v.to_string()).is_err());
        let mut v = base.clone();
        v["resources"][0]
            .as_object_mut()
            .unwrap()
            .remove("revision");
        assert!(parse_source_lock(&v.to_string()).is_err());
        // 空 resources
        let mut v = base.clone();
        v["resources"] = serde_json::json!([]);
        assert!(parse_source_lock(&v.to_string()).is_err());
    }

    // ── 管理器（注入假传输；exec 不存在 → op 以 spawn_failed 收敛）──────────

    /// 假传输：内容与 expected 匹配则成功——让测试能走通完整 fetch→install 流。
    struct ScriptedTransport {
        body: Vec<u8>,
    }

    #[async_trait::async_trait]
    impl ResourceTransport for ScriptedTransport {
        async fn fetch(
            &self,
            _candidates: &[String],
            expected_sha256: &str,
            declared_bytes: u64,
            dest: &Path,
            progress: &(dyn Fn(u64) + Send + Sync),
        ) -> Result<(), FetchFailure> {
            use sha2::Digest;
            if self.body.len() as u64 > declared_bytes {
                return Err(FetchFailure {
                    code: "over_limit",
                    message: "超量".into(),
                });
            }
            std::fs::write(dest, &self.body).unwrap();
            progress(self.body.len() as u64);
            let mut h = sha2::Sha256::new();
            h.update(&self.body);
            let actual = format!("{:x}", h.finalize());
            if actual == expected_sha256 {
                Ok(())
            } else {
                Err(FetchFailure {
                    code: "hash_mismatch",
                    message: "假传输内容不匹配".into(),
                })
            }
        }
    }

    fn sha256_hex(body: &[u8]) -> String {
        use sha2::Digest;
        let mut h = sha2::Sha256::new();
        h.update(body);
        format!("{:x}", h.finalize())
    }

    /// 最小合法数据集目录（dataset.json + 单文件 + 正确哈希）。
    fn write_minimal_dataset(dir: &Path, plugin_id: &str, dataset_id: &str, version: &str) {
        let data_body = b"minimal-dataset-payload";
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join("data.bin"), data_body).unwrap();
        std::fs::write(
            dir.join("dataset.json"),
            serde_json::json!({
                "format_version": 1,
                "dataset_id": dataset_id,
                "plugin_id": plugin_id,
                "version": version,
                "source": {"name": "test", "license": "MIT", "revision": "r1"},
                "files": {"data.bin": {"sha256": sha256_hex(data_body), "bytes": data_body.len()}},
                "counts": {"entries": 1}
            })
            .to_string(),
        )
        .unwrap();
    }

    fn bootstrap_plugin_dir(dir: &Path, body: &[u8]) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(
            dir.join("manifest.json"),
            serde_json::json!({
                "schema_version": 1,
                "id": "test.bootstrap",
                "name": "T",
                "version": "0.1.0",
                "protocol_version": 2,
                "runtime": {"type": "process", "exec": "nonexistent.exe"},
                "capabilities": ["query", "dataset_init"],
                "datasets": [
                    {"id": "ds-basic", "min_version": "1.1.0", "optional": false},
                    {"id": "ds-ety", "optional": true}
                ]
            })
            .to_string(),
        )
        .unwrap();
        std::fs::write(
            dir.join(BOOTSTRAP_FILE),
            serde_json::json!({
                "format_version": 1,
                "resources": [{
                    "id": "src-a",
                    "url": "https://raw.githubusercontent.com/skywind3000/ECDICT/8defb761f7c7ad1818ca94290a1844d7b33d6b23/ecdict.csv",
                    "sha256": sha256_hex(body),
                    "bytes": body.len(),
                    "format": "csv",
                    "license": "MIT",
                    "revision": "8defb761f7c7ad1818ca94290a1844d7b33d6b23"
                }]
            })
            .to_string(),
        )
        .unwrap();
    }

    async fn manager_fixture(
        dir: &Path,
        body: &[u8],
    ) -> (Arc<BootstrapManager>, Arc<PluginEngine>) {
        let storage_root = dir.join("storage-root");
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .connect("sqlite::memory:")
            .await
            .unwrap();
        let storage = PluginStorage::new(&storage_root);
        storage.ensure_layout().unwrap();
        let manifest =
            crate::plugin::PluginManifest::from_path(&dir.join("manifest.json")).unwrap();
        let handle = Arc::new(PluginHandle::new(
            Arc::new(manifest),
            dir.to_path_buf(),
            None,
        ));
        handle.set_dirs(crate::plugin::PluginDirs {
            data: storage
                .data_dir("test.bootstrap")
                .to_string_lossy()
                .into_owned(),
            cache: storage
                .cache_dir("test.bootstrap")
                .to_string_lossy()
                .into_owned(),
            datasets: storage
                .datasets_dir("test.bootstrap")
                .to_string_lossy()
                .into_owned(),
        });
        let engine = Arc::new(PluginEngine::new(vec![handle], pool, None));
        let dataset_installer = Arc::new(DatasetInstaller::new(storage.clone()));
        (
            BootstrapManager::new(
                engine.clone(),
                storage,
                dataset_installer,
                Arc::new(ScriptedTransport {
                    body: body.to_vec(),
                }),
                tokio::runtime::Handle::current(),
            ),
            engine,
        )
    }

    /// 与 Tauri setup 相同：构造发生在 runtime 外，运行时由宿主显式注入。
    #[test]
    fn constructs_on_sync_thread_with_injected_runtime() {
        assert!(tokio::runtime::Handle::try_current().is_err());
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let pool = runtime.block_on(async {
            sqlx::sqlite::SqlitePoolOptions::new()
                .connect("sqlite::memory:")
                .await
                .unwrap()
        });
        let tmp = tempfile::tempdir().unwrap();
        let storage = PluginStorage::new(tmp.path());
        let manager = BootstrapManager::new(
            Arc::new(PluginEngine::new(vec![], pool, None)),
            storage.clone(),
            Arc::new(DatasetInstaller::new(storage)),
            Arc::new(ScriptedTransport { body: vec![] }),
            runtime.handle().clone(),
        );
        assert!(tokio::runtime::Handle::try_current().is_err());
        assert!(!manager._loop.lock().unwrap().as_ref().unwrap().is_finished());
        runtime.block_on(async { tokio::task::yield_now().await });
        assert!(!manager._loop.lock().unwrap().as_ref().unwrap().is_finished());
    }

    /// 等待 (插件, 数据集) 到达终态并返回终态快照。
    async fn await_terminal(manager: &Arc<BootstrapManager>, dataset_id: &str) -> OpSnapshot {
        tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                if let Some(snap) = manager.status_for("test.bootstrap", dataset_id)
                    && snap.phase.is_terminal()
                {
                    return snap;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("操作应在超时前到达终态")
    }

    #[tokio::test]
    async fn begin_rejects_undeclared_dataset_unsupported_and_missing_lock() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("pkg");
        bootstrap_plugin_dir(&dir, b"src");
        let (manager, _engine) = manager_fixture(&dir, b"src").await;

        let err = manager
            .begin("test.bootstrap", "ds-unknown", "manual")
            .await
            .unwrap_err();
        assert!(matches!(err, BootstrapError::UndeclaredDataset(_)));

        // 去掉 dataset_init 能力 → Unsupported
        let mut m: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("manifest.json")).unwrap())
                .unwrap();
        m["capabilities"] = serde_json::json!(["query"]);
        std::fs::write(dir.join("manifest.json"), m.to_string()).unwrap();
        let (manager2, _engine2) = manager_fixture(&dir, b"src").await;
        assert!(matches!(
            manager2
                .begin("test.bootstrap", "ds-basic", "manual")
                .await
                .unwrap_err(),
            BootstrapError::Unsupported
        ));

        // 恢复能力但删 bootstrap.json → InvalidSourceLock
        let mut m: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("manifest.json")).unwrap())
                .unwrap();
        m["capabilities"] = serde_json::json!(["query", "dataset_init"]);
        std::fs::write(dir.join("manifest.json"), m.to_string()).unwrap();
        std::fs::remove_file(dir.join(BOOTSTRAP_FILE)).unwrap();
        let (manager3, _engine3) = manager_fixture(&dir, b"src").await;
        assert!(matches!(
            manager3
                .begin("test.bootstrap", "ds-basic", "manual")
                .await
                .unwrap_err(),
            BootstrapError::InvalidSourceLock(_)
        ));
    }

    #[tokio::test]
    async fn begin_already_ready_skips_and_concurrent_begin_merges() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("pkg");
        bootstrap_plugin_dir(&dir, b"src");
        let (manager, _engine) = manager_fixture(&dir, b"src").await;
        let installer = DatasetInstaller::new(PluginStorage::new(dir.join("storage-root")));

        // 预挂载兼容版本 → AlreadyReady（兼容保留库重装跳过构建）。
        let build_out = dir.join("prebuilt").join("ds-basic").join("1.1.0");
        write_minimal_dataset(&build_out, "test.bootstrap", "ds-basic", "1.1.0");
        installer.install_dir(&build_out).unwrap();
        assert!(matches!(
            manager
                .begin("test.bootstrap", "ds-basic", "auto_install")
                .await
                .unwrap(),
            BeginOutcome::AlreadyReady
        ));

        // 卸载后 begin → Started；并发第二次 begin → AlreadyRunning（触发合并）。
        installer.uninstall("test.bootstrap", "ds-basic").unwrap();
        assert!(matches!(
            manager
                .begin("test.bootstrap", "ds-basic", "auto_install")
                .await
                .unwrap(),
            BeginOutcome::Started(_)
        ));
        match manager
            .begin("test.bootstrap", "ds-basic", "manual")
            .await
            .unwrap()
        {
            BeginOutcome::AlreadyRunning(snap) => {
                assert_eq!(snap.trigger, "auto_install", "触发合并沿用首个在途操作");
            }
            other => panic!("应合并为 AlreadyRunning，实际 {other:?}"),
        }

        // exec 不存在 → op 异步收敛为 spawn_failed（不悬挂）。
        let failed = await_terminal(&manager, "ds-basic").await;
        assert!(
            matches!(&failed.phase, OpPhase::Failed { code, .. } if code == "spawn_failed"),
            "exec 不存在应 spawn_failed: {:?}",
            failed.phase
        );
    }

    #[tokio::test]
    async fn fetch_flow_mounts_dataset_and_source_traces_to_revision() {
        // 直接驱动 handle_fetch/handle_install 的完整链路（不经真实进程）：
        // begin 失败（spawn_failed）后仍可用内部方法验证自举帧处理正确性。
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("pkg");
        let body = b"ecdict-fixture-bytes";
        bootstrap_plugin_dir(&dir, body);
        let (manager, engine) = manager_fixture(&dir, body).await;

        // 手工注册一个 op 条目（绕过 begin 的进程 spawn——单元验证帧处理层）。
        let op_id = "boot-test".to_string();
        let work_dir = dir
            .join("storage-root")
            .join("cache")
            .join("test.bootstrap")
            .join("ops")
            .join(&op_id);
        std::fs::create_dir_all(work_dir.join("resources")).unwrap();
        let (inbox_tx, _inbox_rx) = mpsc::unbounded_channel();
        let entry = Arc::new(OpEntry {
            snapshot: std::sync::Mutex::new(OpSnapshot {
                op_id: op_id.clone(),
                plugin_id: "test.bootstrap".into(),
                dataset_id: "ds-basic".into(),
                phase: OpPhase::Starting,
                cancellable: true,
                trigger: "manual".into(),
            }),
            inbox: inbox_tx,
            cancel: tokio::sync::watch::channel(false).0,
            instance: std::sync::atomic::AtomicU64::new(0),
        });
        manager
            .ops
            .lock()
            .unwrap()
            .insert(op_id.clone(), Arc::clone(&entry));
        let handle = engine
            .all_plugins()
            .into_iter()
            .find(|h| h.id() == "test.bootstrap")
            .unwrap();

        // ① fetch：源锁定命中 → 文件落位 + 阶段推进。
        let mut last_revision = None;
        manager
            .handle_fetch(
                &entry,
                &load_source_lock(&dir).unwrap(),
                &work_dir,
                "test.bootstrap",
                "fr_1",
                "src-a",
                &mut last_revision,
            )
            .await;
        assert!(work_dir.join("resources").join("src-a").is_file());
        assert_eq!(
            last_revision.as_deref(),
            Some("8defb761f7c7ad1818ca94290a1844d7b33d6b23")
        );
        assert!(matches!(
            entry.snapshot.lock().unwrap().phase,
            OpPhase::PluginStage { ref stage, .. } if stage == "fetched"
        ));

        // ② 构建输出（模拟插件 build/）：合法数据集 → 挂载成功 + 来源可追溯。
        let build_dir = work_dir.join("build").join("ds-basic").join("1.1.0");
        write_minimal_dataset(&build_dir, "test.bootstrap", "ds-basic", "1.1.0");
        let mut mounted = None;
        manager
            .handle_install(
                &entry,
                &handle,
                &work_dir,
                "test.bootstrap",
                "ds-basic",
                "di_1",
                "ds-basic",
                "build/ds-basic/1.1.0",
                last_revision.as_deref(),
                &mut mounted,
            )
            .await;
        assert_eq!(mounted.as_deref(), Some("1.1.0"));
        assert!(matches!(
            entry.snapshot.lock().unwrap().phase,
            OpPhase::Mounted { ref version } if version == "1.1.0"
        ));
        // registry 来源 = Managed（插件版本/源修订/操作 id）。
        let installer = DatasetInstaller::new(PluginStorage::new(dir.join("storage-root")));
        let records = installer.list().unwrap();
        let record = records.iter().find(|r| r.dataset_id == "ds-basic").unwrap();
        match &record.source {
            InstallSource::Managed {
                plugin_version,
                source_revision,
                operation,
            } => {
                assert_eq!(plugin_version, "0.1.0");
                assert_eq!(
                    source_revision, "r1",
                    "dataset.json 的 source.revision 优先"
                );
                assert_eq!(operation, "boot-test");
            }
            other => panic!("来源应为 Managed: {other:?}"),
        }

        // ③ 越界/伪造路径全被拒（不落任何挂载）。
        let mut sink = None;
        for (dataset, source_dir, _expect_code) in [
            ("ds-basic", "../escape", "path_escape"),
            ("ds-basic", "build/missing", "missing_output"),
            ("ds-ety", "build/ds-basic/1.1.0", "dataset_mismatch"),
        ] {
            manager
                .handle_install(
                    &entry,
                    &handle,
                    &work_dir,
                    "test.bootstrap",
                    "ds-basic",
                    "di_x",
                    dataset,
                    source_dir,
                    None,
                    &mut sink,
                )
                .await;
            assert_eq!(
                mounted.as_deref(),
                Some("1.1.0"),
                "失败路径不得影响已挂载状态"
            );
        }
        // 伪造归属：dataset.json 声明别的插件 → 拒绝（install_managed_dir 内核验）。
        let rogue = work_dir.join("build").join("rogue").join("1.0.0");
        write_minimal_dataset(&rogue, "other.plugin", "ds-basic", "1.0.0");
        let mut sink2 = None;
        manager
            .handle_install(
                &entry,
                &handle,
                &work_dir,
                "test.bootstrap",
                "ds-basic",
                "di_y",
                "ds-basic",
                "build/rogue/1.0.0",
                None,
                &mut sink2,
            )
            .await;
        assert!(sink2.is_none(), "伪造 plugin_id 的挂载必须被拒");
    }

    #[tokio::test]
    async fn pending_remove_plugin_rejects_mount() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("pkg");
        let body = b"ecdict-fixture-bytes";
        bootstrap_plugin_dir(&dir, body);
        let (manager, engine) = manager_fixture(&dir, body).await;
        let op_id = "boot-pending".to_string();
        let work_dir = dir
            .join("storage-root")
            .join("cache")
            .join("test.bootstrap")
            .join("ops")
            .join(&op_id);
        std::fs::create_dir_all(&work_dir).unwrap();
        let entry = Arc::new(OpEntry {
            snapshot: std::sync::Mutex::new(OpSnapshot {
                op_id: op_id.clone(),
                plugin_id: "test.bootstrap".into(),
                dataset_id: "ds-basic".into(),
                phase: OpPhase::Starting,
                cancellable: true,
                trigger: "manual".into(),
            }),
            inbox: mpsc::unbounded_channel().0,
            cancel: tokio::sync::watch::channel(false).0,
            instance: std::sync::atomic::AtomicU64::new(0),
        });
        let handle = engine
            .all_plugins()
            .into_iter()
            .find(|h| h.id() == "test.bootstrap")
            .unwrap();

        // 插件记录标记 PendingRemove（模拟卸载事务第一步）。
        let storage = PluginStorage::new(dir.join("storage-root"));
        storage
            .update_registry(|reg| {
                reg.records.insert(
                    "test.bootstrap".into(),
                    blink_infra::plugin_storage::InstallRecord {
                        plugin_id: "test.bootstrap".into(),
                        version: "0.1.0".into(),
                        source: InstallSource::LocalDev {
                            origin: String::new(),
                        },
                        installed_at: 0,
                        state: blink_infra::plugin_storage::InstallState::PendingRemove,
                        sha256: None,
                        manifest_schema_version: 1,
                        protocol_version: 2,
                        pending_paths: vec![],
                    },
                );
                Ok::<(), ()>(())
            })
            .unwrap()
            .unwrap();

        let build_dir = work_dir.join("build").join("ds-basic").join("1.1.0");
        write_minimal_dataset(&build_dir, "test.bootstrap", "ds-basic", "1.1.0");
        let mut mounted = None;
        manager
            .handle_install(
                &entry,
                &handle,
                &work_dir,
                "test.bootstrap",
                "ds-basic",
                "di_1",
                "ds-basic",
                "build/ds-basic/1.1.0",
                None,
                &mut mounted,
            )
            .await;
        assert!(mounted.is_none(), "插件待卸载时必须拒绝挂载");
        drop(entry);
    }

    fn test_entry(op: &str) -> (Arc<OpEntry>, mpsc::UnboundedReceiver<PluginUpstreamMessage>) {
        let (inbox, rx) = mpsc::unbounded_channel();
        (
            Arc::new(OpEntry {
                snapshot: std::sync::Mutex::new(OpSnapshot {
                    op_id: op.into(),
                    plugin_id: "test.bootstrap".into(),
                    dataset_id: "ds-basic".into(),
                    phase: OpPhase::Starting,
                    cancellable: true,
                    trigger: "manual".into(),
                }),
                inbox,
                cancel: tokio::sync::watch::channel(false).0,
                instance: std::sync::atomic::AtomicU64::new(42),
            }),
            rx,
        )
    }

    #[tokio::test]
    async fn upstream_frames_require_plugin_and_process_ownership() {
        let tmp = tempfile::tempdir().unwrap();
        bootstrap_plugin_dir(tmp.path(), b"src");
        let (manager, _) = manager_fixture(tmp.path(), b"src").await;
        let (entry, mut rx) = test_entry("owned-op");
        manager
            .ops
            .lock()
            .unwrap()
            .insert("owned-op".into(), entry.clone());
        let frame = || PluginUpstreamMessage::InitStage {
            op_id: "owned-op".into(),
            stage: "building".into(),
            note: None,
        };
        manager.route_aux("other.plugin", 42, frame()).await;
        manager.route_aux("test.bootstrap", 41, frame()).await;
        assert!(rx.try_recv().is_err());
        manager.route_aux("test.bootstrap", 42, frame()).await;
        assert!(rx.try_recv().is_ok());
        entry.cancel.send_replace(true);
        manager.route_aux("test.bootstrap", 42, frame()).await;
        assert!(rx.try_recv().is_err());
        tokio::time::timeout(Duration::from_secs(1), entry.cancelled())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn cancelled_or_revoked_operation_cannot_mount() {
        for revoked in [false, true] {
            let tmp = tempfile::tempdir().unwrap();
            bootstrap_plugin_dir(tmp.path(), b"src");
            let (manager, engine) = manager_fixture(tmp.path(), b"src").await;
            let handle = engine.all_plugins()[0].clone();
            let (entry, _rx) = test_entry("cancel-op");
            let work = tmp.path().join("op");
            write_minimal_dataset(&work.join("build"), "test.bootstrap", "ds-basic", "1.1.0");
            if revoked {
                manager.revoke_plugin(&handle, "fixture revoke").await;
            } else {
                entry.cancel.send_replace(true);
            }
            let mut mounted = None;
            manager
                .handle_install(
                    &entry,
                    &handle,
                    &work,
                    "test.bootstrap",
                    "ds-basic",
                    "i1",
                    "ds-basic",
                    "build",
                    None,
                    &mut mounted,
                )
                .await;
            assert!(mounted.is_none());
            assert_eq!(
                resolve_active_version(&PathBuf::from(handle.dirs().unwrap().datasets), "ds-basic"),
                ActiveVersion::NotMounted
            );
        }
    }

    #[tokio::test]
    async fn cancellation_during_staging_is_rechecked_at_commit() {
        let tmp = tempfile::tempdir().unwrap();
        bootstrap_plugin_dir(tmp.path(), b"src");
        let (manager, engine) = manager_fixture(tmp.path(), b"src").await;
        let handle = engine.all_plugins()[0].clone();
        let (entry, _rx) = test_entry("staged-cancel");
        let work = tmp.path().join("op");
        write_minimal_dataset(&work.join("build"), "test.bootstrap", "ds-basic", "1.1.0");
        let fence = handle.bootstrap_fence().lock_owned().await;
        let mut events = manager.subscribe();
        let run_manager = manager.clone();
        let run_handle = handle.clone();
        let run_entry = entry.clone();
        let run = tokio::spawn(async move {
            let mut mounted = None;
            run_manager.handle_install(&run_entry, &run_handle, &work, "test.bootstrap", "ds-basic", "i1",
                "ds-basic", "build", None, &mut mounted).await;
            mounted
        });
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if matches!(events.recv().await.unwrap().phase, OpPhase::Mounting) { break; }
            }
        }).await.unwrap();
        entry.cancel.send_replace(true);
        drop(fence);
        assert!(tokio::time::timeout(Duration::from_secs(3), run).await.unwrap().unwrap().is_none());
        assert_eq!(resolve_active_version(&PathBuf::from(handle.dirs().unwrap().datasets), "ds-basic"), ActiveVersion::NotMounted);
    }

    #[tokio::test]
    async fn corrupt_compatible_dataset_does_not_skip_repair() {
        let tmp = tempfile::tempdir().unwrap();
        bootstrap_plugin_dir(tmp.path(), b"src");
        let (manager, _) = manager_fixture(tmp.path(), b"src").await;
        let output = tmp.path().join("prebuilt");
        write_minimal_dataset(&output, "test.bootstrap", "ds-basic", "1.1.0");
        manager.dataset_installer.install_dir(&output).unwrap();
        let active = manager
            .storage
            .dataset_version_dir("test.bootstrap", "ds-basic", "1.1.0");
        std::fs::write(active.join("data.bin"), b"broken").unwrap();
        assert!(matches!(
            manager
                .begin("test.bootstrap", "ds-basic", "auto_install")
                .await
                .unwrap(),
            BeginOutcome::Started(_)
        ));
        let terminal = await_terminal(&manager, "ds-basic").await;
        assert!(matches!(terminal.phase, OpPhase::Failed { .. })); // fixture 无 exe，不能误报 AlreadyReady
    }

    #[tokio::test]
    async fn recover_orphan_ops_cleans_all_op_dirs() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("pkg");
        bootstrap_plugin_dir(&dir, b"src");
        let (manager, _engine) = manager_fixture(&dir, b"src").await;
        let ops = dir
            .join("storage-root")
            .join("cache")
            .join("test.bootstrap")
            .join("ops");
        std::fs::create_dir_all(ops.join("boot-1")).unwrap();
        std::fs::create_dir_all(ops.join("boot-2")).unwrap();
        assert_eq!(manager.recover_orphan_ops(), 2);
        assert!(!ops.join("boot-1").exists());
    }

    #[test]
    fn minimal_dataset_helper_produces_installable_output() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("ds-x").join("1.0.0");
        write_minimal_dataset(&dir, "test.bootstrap", "ds-x", "1.0.0");
        let installer = DatasetInstaller::new(PluginStorage::new(tmp.path().join("storage")));
        let record = installer.install_dir(&dir).unwrap();
        assert_eq!(record.dataset_id, "ds-x");
        assert_eq!(record.version, "1.0.0");
    }
}
