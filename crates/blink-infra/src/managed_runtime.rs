//! 托管外部运行时分发（0.26.14）。
//!
//! **定位**：为 Blink 托管的外部搜索引擎（首个成员：Everything 1.4 stable
//! 便携版）提供编译期锁定的按需下载与安装事务。沿 0.25.20
//! [script_interpreter](self::script_interpreter) 的 asset-lock 模式——
//! 锁 URL+sha256+size+exe_relpath+version_check，staging 内完成
//! 下载→校验→解压→self-test，原子 rename 到
//! `runtimes/managed/{kind}-{version}/`，与用户系统 PATH 完全隔离。
//!
//! **与 script_interpreter 的关系**：复用其 `InstallReporter` 回调与
//! staging/sweep 约定，但锁文件独立（resources/runtimes/）、artifact 目录
//! 独立、错误码独立。两处下载循环仍是受控重复（连同 local_engine/onnx.rs
//! 共三处），统一收敛到 utils 层留待后续版本。
//!
//! **self-test 语义差异**：Everything.exe 是 GUI 子系统程序，无控制台
//! `--version` 输出，`version_check` 字段约定为 exe 文件头 magic（"MZ" =
//! PE），字节级身份由 sha256 锁定（2026-10-19 实测下载核实）。
//!
//! **许可**：Everything 1.4 为 MIT（含 PCRE BSD-3 组件），允许再分发；
//! 关于页附声明即合规（phase 0.26.14 §5.15 路线决策）。

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// 编译期嵌入的运行时锁文件（版本 + URL + SHA-256 + 大小的 single source of truth）。
pub const MANAGED_RUNTIME_LOCK_JSON: &str =
    include_str!("../../../resources/runtimes/everything-asset-lock.json");

/// 托管运行时种类（当前仅 Everything；新增成员在此扩展）。
pub const KIND_EVERYTHING: &str = "everything";

// ── 错误 ───────────────────────────────────────────────────────────────────

/// 托管运行时安装/解析错误（app 层转为 `CommandError`）。
#[derive(Debug, thiserror::Error)]
pub enum ManagedRuntimeError {
    #[error("锁文件解析失败: {0}")]
    LockParse(String),
    #[error("未知的运行时类型: {0}")]
    UnknownKind(String),
    #[error("该运行时正在安装中")]
    AlreadyInstalling,
    #[error("另一个 Blink 进程正在管理托管实例，当前进程不会接管")]
    InstanceBusy,
    #[error("下载失败: {0}")]
    Download(String),
    #[error("SHA-256 校验失败: 期望 {expected}, 实际 {actual}")]
    ChecksumMismatch { expected: String, actual: String },
    #[error("安装失败: {0}")]
    InstallFailed(String),
    #[error("自检失败: {0}")]
    SelfTestFailed(String),
    #[error("操作被取消")]
    Cancelled,
    #[error("IO 错误: {0}")]
    Io(#[from] std::io::Error),
}

impl ManagedRuntimeError {
    /// 稳定错误码（CommandError.code 用）。
    pub fn code(&self) -> &'static str {
        match self {
            Self::LockParse(_) => "managed_runtime_lock_parse",
            Self::UnknownKind(_) => "managed_runtime_unknown_kind",
            Self::AlreadyInstalling => "managed_runtime_already_installing",
            Self::InstanceBusy => "managed_runtime_instance_busy",
            Self::Download(_) => "managed_runtime_download",
            Self::ChecksumMismatch { .. } => "managed_runtime_checksum_mismatch",
            Self::InstallFailed(_) => "managed_runtime_install",
            Self::SelfTestFailed(_) => "managed_runtime_self_test",
            Self::Cancelled => "managed_runtime_cancelled",
            Self::Io(_) => "managed_runtime_io",
        }
    }
}

// ── 锁文件 ───────────────────────────────────────────────────────────────────

/// 锁文件根结构。
#[derive(Debug, Clone, Deserialize)]
pub struct ManagedRuntimeLock {
    pub schema_version: u32,
    pub runtimes: Vec<RuntimeLockEntry>,
}

/// 单个运行时的锁定条目。
#[derive(Debug, Clone, Deserialize)]
pub struct RuntimeLockEntry {
    pub kind: String,
    pub version: String,
    pub url: String,
    pub sha256: String,
    pub size_bytes: u64,
    pub exe_relpath: String,
    /// exe 文件头 magic 前缀（GUI 程序无 --version 输出，见模块注释）。
    pub version_check: String,
}

impl RuntimeLockEntry {
    /// 版本化不可变 artifact id（`{kind}-{version}`）。
    pub fn artifact_id(&self) -> String {
        format!("{}-{}", self.kind, self.version)
    }
}

/// 解析嵌入的锁文件。
pub fn parse_lock() -> Result<ManagedRuntimeLock, ManagedRuntimeError> {
    serde_json::from_str(MANAGED_RUNTIME_LOCK_JSON)
        .map_err(|e| ManagedRuntimeError::LockParse(e.to_string()))
}

/// 取指定种类的锁定条目。
pub fn lock_entry(kind: &str) -> Result<RuntimeLockEntry, ManagedRuntimeError> {
    let lock = parse_lock()?;
    lock.runtimes
        .into_iter()
        .find(|e| e.kind == kind)
        .ok_or_else(|| {
            ManagedRuntimeError::LockParse(format!("锁文件缺少 {kind} 条目"))
        })
}

// ── 路径与状态 ───────────────────────────────────────────────────────────────

/// 托管运行时根目录：`runtimes/managed`（复用 local_engine 的 runtimes_root，
/// 测试态指向临时目录）。
pub fn root() -> PathBuf {
    crate::local_engine::runtime::runtimes_root().join("managed")
}

/// 指定种类安装后的 artifact 目录。
fn artifact_dir(kind: &str) -> Result<PathBuf, ManagedRuntimeError> {
    Ok(root().join(lock_entry(kind)?.artifact_id()))
}

/// 解析托管运行时 exe 路径（只读存在性检查）。
///
/// 未安装返回 None——调用方转为「未安装」状态/错误项，不回退系统 PATH：
/// 版本锁定与系统隔离是托管体系的核心语义。
pub fn resolve(kind: &str) -> Option<PathBuf> {
    let entry = lock_entry(kind).ok()?;
    let exe = root().join(entry.artifact_id()).join(&entry.exe_relpath);
    exe.is_file().then_some(exe)
}

/// 生命周期租约放在 artifact 外，卸载该版本时仍保有互斥。
/// 内核在持有者退出（包括崩溃）时释放锁，避免 CLI 把活跃 GUI 实例认成孤儿。
pub fn acquire_instance_lease(kind: &str) -> Result<std::fs::File, ManagedRuntimeError> {
    lock_entry(kind)?;
    std::fs::create_dir_all(root())?;
    acquire_instance_lease_at(&root().join(format!(".{kind}-instance.lock")))
}

fn acquire_instance_lease_at(path: &Path) -> Result<std::fs::File, ManagedRuntimeError> {
    let file = std::fs::OpenOptions::new()
        .read(true).write(true).create(true).truncate(false).open(path)?;
    file.try_lock().map_err(|error| match error {
        std::fs::TryLockError::WouldBlock => ManagedRuntimeError::InstanceBusy,
        std::fs::TryLockError::Error(error) => ManagedRuntimeError::Io(error),
    })?;
    Ok(file)
}

/// 安装内状态（设置页展示，只读）。
#[derive(Debug, Clone, Serialize)]
pub struct ManagedRuntimeStatus {
    pub kind: String,
    pub version: String,
    pub installed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exe_path: Option<String>,
}

/// 全部托管运行时的安装状态（设置页展示，只读）。
pub fn status() -> Vec<ManagedRuntimeStatus> {
    let mut out = Vec::new();
    let Ok(lock) = parse_lock() else {
        return out;
    };
    for entry in &lock.runtimes {
        let exe = resolve(&entry.kind);
        out.push(ManagedRuntimeStatus {
            kind: entry.kind.clone(),
            version: entry.version.clone(),
            installed: exe.is_some(),
            exe_path: exe.map(|p| p.display().to_string()),
        });
    }
    out
}

// ── 安装事务 ─────────────────────────────────────────────────────────────────

/// 进程内安装互斥（single-flight）：同 kind 并发安装直接拒绝。
static INSTALLING: std::sync::LazyLock<std::sync::Mutex<std::collections::HashSet<&'static str>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashSet::new()));

/// 安装进度/日志上报回调（复用 script_interpreter 的 trait，事件桥同构）。
pub use crate::script_interpreter::InstallReporter;

/// 安装托管运行时（幂等：已安装直接返回成功）。
///
/// 事务性：下载/解压/self-test 全部发生在 staging，通过后原子 rename 到
/// 目标目录；任何失败只留 staging 残留，由下次安装/启动清扫回收。
pub async fn install(
    kind: &str,
    cancel_token: Option<&tokio_util::sync::CancellationToken>,
    reporter: &dyn InstallReporter,
) -> Result<(), ManagedRuntimeError> {
    {
        let mut guard = INSTALLING.lock().expect("INSTALLING 锁中毒");
        let key: &'static str = KIND_EVERYTHING;
        if kind == KIND_EVERYTHING && !guard.insert(key) {
            return Err(ManagedRuntimeError::AlreadyInstalling);
        }
        if kind != KIND_EVERYTHING {
            return Err(ManagedRuntimeError::UnknownKind(kind.to_string()));
        }
    }
    // panic 时泄漏占用是可接受的降级（进程内状态，重启即愈）；正常路径必释放。
    let result = install_inner(kind, cancel_token, reporter).await;
    INSTALLING.lock().expect("INSTALLING 锁中毒").remove(kind);
    result
}

async fn install_inner(
    kind: &str,
    cancel_token: Option<&tokio_util::sync::CancellationToken>,
    reporter: &dyn InstallReporter,
) -> Result<(), ManagedRuntimeError> {
    let entry = lock_entry(kind)?;
    let exe_path = root().join(entry.artifact_id()).join(&entry.exe_relpath);
    if exe_path.is_file() {
        tracing::debug!(kind, "托管运行时已安装，跳过");
        return Ok(());
    }

    // 清扫其他孤儿 staging（上次失败/崩溃残留）。
    let swept = sweep_staging();
    if swept > 0 {
        tracing::info!(count = swept, "安装前清扫孤儿 staging");
    }

    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let staging = root().join(format!(".staging-{kind}-{ts}"));
    std::fs::create_dir_all(&staging)?;
    let cleanup = |staging: &Path| {
        let _ = std::fs::remove_dir_all(staging);
    };

    // ── 1. 下载 zip ──
    reporter.on_stage("downloading");
    reporter.on_log("info", &format!("正在下载 Everything {}", entry.version));
    let archive = staging.join("archive.zip");
    if let Err(e) = download_archive(&entry, &archive, cancel_token, reporter).await {
        cleanup(&staging);
        return Err(e);
    }

    // ── 2. 解压（zip slip 防护） ──
    reporter.on_stage("extracting");
    reporter.on_log("info", "正在解压…");
    let extract_dir = staging.join("extract");
    match tokio::task::spawn_blocking({
        let extract_dir = extract_dir.clone();
        let archive = archive.clone();
        move || extract_zip(&archive, &extract_dir)
    })
    .await
    {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            cleanup(&staging);
            return Err(e);
        }
        Err(e) => {
            cleanup(&staging);
            return Err(ManagedRuntimeError::InstallFailed(format!(
                "解压任务 join 失败: {e}"
            )));
        }
    }

    // ── 3. self-test：exe 存在 + 文件头 magic 匹配锁文件前缀 ──
    reporter.on_stage("verifying");
    reporter.on_log("info", "正在验证运行时…");
    let staged_exe = extract_dir.join(&entry.exe_relpath);
    if let Err(e) = self_test(&staged_exe, &entry.version_check) {
        cleanup(&staging);
        reporter.on_log("error", &format!("自检失败: {e}"));
        return Err(e);
    }

    // ── 4. promote：清旧目标（升级残留）→ 原子 rename ──
    reporter.on_stage("promoting");
    let target = root().join(entry.artifact_id());
    if target.exists()
        && let Err(e) = std::fs::remove_dir_all(&target)
    {
        cleanup(&staging);
        return Err(ManagedRuntimeError::InstallFailed(format!(
            "清理旧目标目录失败（文件被占用？）: {e}"
        )));
    }
    if let Err(e) = std::fs::rename(&extract_dir, &target) {
        cleanup(&staging);
        return Err(ManagedRuntimeError::InstallFailed(format!("promote 失败: {e}")));
    }
    cleanup(&staging);

    reporter.on_stage("done");
    reporter.on_log(
        "info",
        &format!("Everything {} 安装完成", entry.version),
    );
    tracing::info!(
        kind,
        version = %entry.version,
        target = %target.display(),
        "托管运行时安装完成"
    );
    Ok(())
}

/// 取消等待 future：token 为 None 时永不完成（select 分支保持 pending）。
async fn wait_cancel(cancel_token: Option<&tokio_util::sync::CancellationToken>) {
    match cancel_token {
        Some(ct) => ct.cancelled().await,
        None => std::future::pending::<()>().await,
    }
}

/// 下载发行版 zip 并校验 SHA-256（主链 + mirrors 候选；voidtools 无镜像时
/// 候选即主链）。镜像内容与锁定 hash 不一致时拒绝——换源不放宽供应链约束。
async fn download_archive(
    entry: &RuntimeLockEntry,
    dest: &Path,
    cancel_token: Option<&tokio_util::sync::CancellationToken>,
    reporter: &dyn InstallReporter,
) -> Result<(), ManagedRuntimeError> {
    let candidates = crate::utils::mirrors::download_candidates(&entry.url);
    let mut last_err: Option<ManagedRuntimeError> = None;
    for (idx, candidate) in candidates.iter().enumerate() {
        let has_next = idx + 1 < candidates.len();
        match download_once(entry, candidate, dest, cancel_token, reporter).await {
            Ok(()) => return Ok(()),
            Err(e) => {
                if matches!(e, ManagedRuntimeError::Cancelled) {
                    return Err(e);
                }
                if matches!(e, ManagedRuntimeError::ChecksumMismatch { .. })
                    && candidate.as_str() == entry.url
                {
                    return Err(e);
                }
                tracing::warn!(url = candidate.as_str(), error = %e, has_next, "托管运行时下载失败");
                if has_next {
                    reporter.on_log(
                        "warn",
                        &format!("下载失败（{e}），切换下载源 {}/{}", idx + 2, candidates.len()),
                    );
                }
                last_err = Some(e);
            }
        }
    }
    Err(last_err.unwrap_or_else(|| {
        ManagedRuntimeError::Download(format!("无可用下载源 ({})", entry.url))
    }))
}

/// 单候选源下载 + 全量 SHA-256 校验（流式 hash，覆盖写 staging 独占文件）。
async fn download_once(
    entry: &RuntimeLockEntry,
    url: &str,
    dest: &Path,
    cancel_token: Option<&tokio_util::sync::CancellationToken>,
    reporter: &dyn InstallReporter,
) -> Result<(), ManagedRuntimeError> {
    use futures::StreamExt;
    use tokio::io::AsyncWriteExt;

    if let Some(ct) = cancel_token
        && ct.is_cancelled()
    {
        return Err(ManagedRuntimeError::Cancelled);
    }

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(600))
        .build()
        .map_err(|e| ManagedRuntimeError::Download(format!("HTTP client 构造失败: {e}")))?;

    let response = tokio::select! {
        r = client.get(url).send() => {
            r.map_err(|e| ManagedRuntimeError::Download(format!("下载失败: {e}")))?
        }
        _ = wait_cancel(cancel_token) => {
            return Err(ManagedRuntimeError::Cancelled);
        }
    };

    if !response.status().is_success() {
        return Err(ManagedRuntimeError::Download(format!(
            "HTTP {}",
            response.status()
        )));
    }

    let progress_total = response.content_length().or(Some(entry.size_bytes));
    let mut hasher = Sha256::new();
    let mut stream = response.bytes_stream();
    let mut written: u64 = 0;

    let mut file = tokio::fs::File::create(dest).await?;
    loop {
        tokio::select! {
            chunk = stream.next() => {
                match chunk {
                    Some(Ok(bytes)) => {
                        hasher.update(&bytes);
                        file.write_all(&bytes).await?;
                        written += bytes.len() as u64;
                        reporter.on_progress(written, progress_total);
                    }
                    Some(Err(e)) => {
                        let _ = tokio::fs::remove_file(dest).await;
                        return Err(ManagedRuntimeError::Download(format!(
                            "下载流读取失败: {e}"
                        )));
                    }
                    None => break,
                }
            }
            _ = wait_cancel(cancel_token) => {
                let _ = tokio::fs::remove_file(dest).await;
                return Err(ManagedRuntimeError::Cancelled);
            }
        }
    }
    file.flush().await?;

    let actual = format!("{:x}", hasher.finalize());
    if actual != entry.sha256.to_lowercase() {
        let _ = tokio::fs::remove_file(dest).await;
        return Err(ManagedRuntimeError::ChecksumMismatch {
            expected: entry.sha256.clone(),
            actual,
        });
    }
    tracing::debug!(url, bytes = written, "托管运行时 zip 下载完成且 hash 匹配");
    Ok(())
}

/// 解压 zip 全部 entry 到 `dest`（zip slip 防护：拒绝绝对路径 / 盘符 / `..` 段）。
fn extract_zip(archive: &Path, dest: &Path) -> Result<(), ManagedRuntimeError> {
    let file = std::fs::File::open(archive)?;
    let mut zip = zip::ZipArchive::new(file)
        .map_err(|e| ManagedRuntimeError::InstallFailed(format!("ZIP 打开失败: {e}")))?;

    std::fs::create_dir_all(dest)?;
    for i in 0..zip.len() {
        let mut entry = zip
            .by_index(i)
            .map_err(|e| ManagedRuntimeError::InstallFailed(format!("ZIP entry 读取失败: {e}")))?;
        let name = entry.name().replace('\\', "/");
        let Some(safe_rel) = safe_entry_relpath(&name) else {
            return Err(ManagedRuntimeError::InstallFailed(format!(
                "ZIP entry 路径不安全，拒绝解压: {name}"
            )));
        };
        let out_path = dest.join(&safe_rel);
        if entry.is_dir() {
            std::fs::create_dir_all(&out_path)?;
            continue;
        }
        if let Some(parent) = out_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut out = std::fs::File::create(&out_path)?;
        std::io::copy(&mut entry, &mut out)?;
    }
    Ok(())
}

/// zip entry 相对路径安全化：绝对路径、盘符、`..` 段返回 None（fail-closed）。
fn safe_entry_relpath(name: &str) -> Option<PathBuf> {
    if name.starts_with('/') {
        return None;
    }
    let mut parts = Vec::new();
    for seg in name.split('/') {
        match seg {
            "" | "." => continue,
            ".." => return None,
            s => {
                if s.len() >= 2 && s.as_bytes()[1] == b':' {
                    return None;
                }
                parts.push(s);
            }
        }
    }
    if parts.is_empty() {
        return None;
    }
    Some(PathBuf::from(parts.join(std::path::MAIN_SEPARATOR_STR)))
}

/// self-test：exe 存在 + 文件头以锁文件 magic 前缀开头（GUI 程序无 --version
/// 输出，字节身份已由 sha256 锁定，此处只挡「解压产物不是可执行文件」）。
fn self_test(exe: &Path, magic: &str) -> Result<(), ManagedRuntimeError> {
    if !exe.is_file() {
        return Err(ManagedRuntimeError::SelfTestFailed(format!(
            "发行版缺少可执行文件: {}",
            exe.display()
        )));
    }
    let mut head = vec![0u8; magic.len().max(2)];
    let mut f = std::fs::File::open(exe)?;
    use std::io::Read;
    let n = f.read(&mut head)?;
    if n < magic.len() || &head[..magic.len()] != magic.as_bytes() {
        return Err(ManagedRuntimeError::SelfTestFailed(format!(
            "可执行文件头不匹配：期望 {magic:?}，实际 {:?}",
            String::from_utf8_lossy(&head[..n.min(16)])
        )));
    }
    Ok(())
}

/// 卸载：删除指定种类的 artifact 目录（调用方须先停托管实例）。
///
/// exe 正被托管实例占用时 Windows 删除失败——如实返回错误，
/// 由前端提示停止实例/重启后重试。
pub fn uninstall(kind: &str) -> Result<(), ManagedRuntimeError> {
    let dir = artifact_dir(kind)?;
    if !dir.exists() {
        return Ok(()); // 幂等
    }
    std::fs::remove_dir_all(&dir).map_err(|e| {
        ManagedRuntimeError::InstallFailed(format!(
            "卸载失败（运行时可能正被占用，请先停止内置搜索引擎或重启应用）: {e}"
        ))
    })?;
    sweep_staging();
    tracing::info!(kind, "托管运行时已卸载");
    Ok(())
}

/// 清扫根目录下的孤儿 staging（`.staging-*` 前缀），返回清理数量。
///
/// 活跃保护：跳过正在安装中的 kind 的 staging（防并发安装互删，
/// 0.25.20 的教训；单实例应用，跨进程并发可接受）。
pub fn sweep_staging() -> usize {
    let root = root();
    let Ok(entries) = std::fs::read_dir(&root) else {
        return 0;
    };
    let actives: Vec<String> = INSTALLING
        .lock()
        .map(|g| g.iter().map(|k| format!(".staging-{k}-")).collect())
        .unwrap_or_default();
    let mut cleaned = 0;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if !name.starts_with(".staging-") {
            continue;
        }
        if actives.iter().any(|p| name.starts_with(p)) {
            continue;
        }
        if entry.path().is_dir() && std::fs::remove_dir_all(entry.path()).is_ok() {
            cleaned += 1;
        }
    }
    cleaned
}

// ── 测试 ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instance_lease_refuses_another_owner_and_releases_on_drop() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("instance.lock");
        let first = acquire_instance_lease_at(&path).unwrap();
        assert!(matches!(acquire_instance_lease_at(&path), Err(ManagedRuntimeError::InstanceBusy)));
        drop(first);
        let _next = acquire_instance_lease_at(&path).expect("内核释放租约后可恢复孤儿");
    }

    #[test]
    fn embedded_lock_parses_with_everything_entry() {
        let lock = parse_lock().expect("嵌入锁文件必须可解析");
        assert_eq!(lock.schema_version, 1);
        assert_eq!(lock.runtimes.len(), 1);
        let e = lock_entry(KIND_EVERYTHING).unwrap();
        assert_eq!(e.artifact_id(), "everything-1.4.1.1032");
        assert_eq!(e.sha256.len(), 64);
        assert!(e.size_bytes > 0);
        assert_eq!(e.version_check, "MZ");
        assert!(e.url.starts_with("https://"));
        assert!(
            safe_entry_relpath(&e.exe_relpath).is_some(),
            "exe_relpath 必须是安全相对路径"
        );
    }

    #[test]
    fn unknown_kind_rejected() {
        assert!(matches!(
            lock_entry("wget").unwrap_err(),
            ManagedRuntimeError::LockParse(_)
        ));
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let err = rt.block_on(install("wget", None, &NullReporter)).unwrap_err();
        assert!(matches!(err, ManagedRuntimeError::UnknownKind(_)));
    }

    struct NullReporter;
    impl crate::script_interpreter::InstallReporter for NullReporter {
        fn on_stage(&self, _stage: &str) {}
        fn on_log(&self, _level: &str, _text: &str) {}
    }

    #[test]
    fn resolve_missing_returns_none_and_status_not_installed() {
        // 隔离根（runtimes_root 的 cfg(test) 临时目录）下无安装。
        assert!(resolve(KIND_EVERYTHING).is_none());
        let st = status();
        assert_eq!(st.len(), 1);
        assert!(!st[0].installed);
        assert!(st[0].exe_path.is_none());
    }

    #[test]
    fn safe_entry_relpath_rejects_escapes() {
        assert!(safe_entry_relpath("everything.exe").is_some());
        assert!(safe_entry_relpath("a/b/c.txt").is_some());
        assert!(safe_entry_relpath("/abs/path").is_none());
        assert!(safe_entry_relpath("c:/windows/system32").is_none());
        assert!(safe_entry_relpath("../escape").is_none());
        assert!(safe_entry_relpath("").is_none());
    }

    #[test]
    fn self_test_checks_pe_magic() {
        let dir = std::env::temp_dir().join(format!("blink-mr-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let exe = dir.join("fake.exe");
        std::fs::write(&exe, b"MZ\x90\x00 rest of pe").unwrap();
        assert!(self_test(&exe, "MZ").is_ok());
        assert!(matches!(
            self_test(&exe, "ELF").unwrap_err(),
            ManagedRuntimeError::SelfTestFailed(_)
        ));
        std::fs::write(dir.join("empty.exe"), b"").unwrap();
        assert!(matches!(
            self_test(&dir.join("empty.exe"), "MZ").unwrap_err(),
            ManagedRuntimeError::SelfTestFailed(_)
        ));
        assert!(matches!(
            self_test(&dir.join("missing.exe"), "MZ").unwrap_err(),
            ManagedRuntimeError::SelfTestFailed(_)
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn extract_zip_extracts_and_blocks_slip() {
        let dir = std::env::temp_dir().join(format!("blink-mr-zip-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let normal = dir.join("normal.zip");
        {
            let f = std::fs::File::create(&normal).unwrap();
            let mut z = zip::ZipWriter::new(f);
            z.start_file("everything.exe", zip::write::SimpleFileOptions::default())
                .unwrap();
            std::io::Write::write_all(&mut z, b"MZfake").unwrap();
            z.finish().unwrap();
        }
        let out = dir.join("out");
        extract_zip(&normal, &out).expect("正常 zip 应解压成功");
        assert_eq!(std::fs::read(out.join("everything.exe")).unwrap(), b"MZfake");

        let evil = dir.join("evil.zip");
        {
            let f = std::fs::File::create(&evil).unwrap();
            let mut z = zip::ZipWriter::new(f);
            z.start_file("../escaped.txt", zip::write::SimpleFileOptions::default())
                .unwrap();
            std::io::Write::write_all(&mut z, b"evil").unwrap();
            z.finish().unwrap();
        }
        let out2 = dir.join("out2");
        let err = extract_zip(&evil, &out2).expect_err("逃逸 entry 必须被拒绝");
        assert!(matches!(err, ManagedRuntimeError::InstallFailed(_)));
        assert!(!dir.join("escaped.txt").exists());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sweep_staging_removes_only_staging_dirs() {
        let root = root();
        std::fs::create_dir_all(root.join(".staging-everything-123")).unwrap();
        std::fs::create_dir_all(root.join("everything-9.9.9")).unwrap();
        std::fs::write(root.join(".staging-everything-123/keep.txt"), "x").unwrap();

        let cleaned = sweep_staging();
        assert_eq!(cleaned, 1);
        assert!(!root.join(".staging-everything-123").exists());
        assert!(root.join("everything-9.9.9").exists());

        let _ = std::fs::remove_dir_all(root.join("everything-9.9.9"));
    }

    #[test]
    fn install_is_single_flight() {
        {
            let mut guard = INSTALLING.lock().unwrap();
            assert!(guard.insert(KIND_EVERYTHING));
        }
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let err = rt
            .block_on(install(KIND_EVERYTHING, None, &NullReporter))
            .unwrap_err();
        assert!(matches!(err, ManagedRuntimeError::AlreadyInstalling));
        INSTALLING.lock().unwrap().remove(KIND_EVERYTHING);
    }

    /// 真实安装链路集成验证（联网，~1.9MB 下载）。
    ///
    /// `#[ignore]` 常态跳过；发布前或锁文件更新后显式跑：
    /// `cargo test -p blink-infra managed_runtime -- --ignored`。
    /// 验证 下载→hash→解压→self-test→promote 全链路与锁文件真实性。
    #[test]
    #[ignore = "联网下载真实发行版，显式运行"]
    fn install_full_pipeline_downloads_everything() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            install(KIND_EVERYTHING, None, &NullReporter)
                .await
                .expect("托管 Everything 全链路安装应成功");
        });
        let exe = resolve(KIND_EVERYTHING).expect("安装后 resolve 必须命中");
        assert!(exe.is_file());
        // 幂等重装
        rt.block_on(async {
            install(KIND_EVERYTHING, None, &NullReporter)
                .await
                .expect("重复安装应幂等成功");
        });
        let _ = std::fs::remove_dir_all(root());
    }
}
