//! 脚本解释器托管分发（0.25.20）。
//!
//! **定位**：为 `RuntimeType::Python / Node` 脚本插件提供 Blink 托管的解释器
//! 发行版——版本由编译期锁文件锁定（[asset-lock.json](../../../resources/script-interpreters/asset-lock.json)），
//! 安装到 `runtimes/script_interpreters/{artifact_id}/`，与用户系统 PATH 完全
//! 隔离。取代 0.6 的"探测系统解释器 + 手动配置路径"体系（已删除）。
//!
//! **与 local_engine 的关系**：复用 `runtimes_root()` 目录约定与镜像下载/
//! SHA-256 校验模式，但**不进 EngineManager 引擎注册表**——脚本解释器没有
//! 模型契约与服务生命周期，只是插件系统的共享运行时制品。安装语义也比
//! `InstallTransaction` 轻：目标目录存在性即安装成功判据，staging → 目标
//! 的原子 rename 保证无半装状态，无需 slot/pointer/journal。
//!
//! **生命周期**：
//! - `install`：staging（`.staging-{kind}-{ts}`）内下载 zip → 解压（zip slip
//!   防护）→ self-test（`--version` 输出匹配锁文件前缀）→ rename 到目标；
//!   开始前清扫其他孤儿 staging。
//! - `resolve`：只读查 exe 路径（plugin spawn 热路径调用，纯存在性检查）。
//! - `sweep_staging`：显式清扫孤儿 staging（app 启动时调用，只读恢复之外
//!   的显式恢复动作）。
//!
//! **失败残留**：下载/解压/校验失败只污染 staging，最晚在下次安装尝试或
//! 应用启动清扫时回收；成功路径 rename 后 staging 整体删除。
//!
//! 已知取舍：下载循环与 `local_engine/providers/onnx.rs` 的
//! `download_and_verify` 存在受控重复（错误/回调类型不同），待第三处使用
//! 时统一收敛到 utils 层。

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// 编译期嵌入的解释器锁文件（版本 + URL + SHA-256 + 大小的 single source of truth）。
pub const SCRIPT_INTERPRETER_LOCK_JSON: &str =
    include_str!("../../../resources/script-interpreters/asset-lock.json");

// ── 错误 ───────────────────────────────────────────────────────────────────

/// 脚本解释器安装/解析错误（app 层转为 `CommandError`）。
#[derive(Debug, thiserror::Error)]
pub enum ScriptInterpreterError {
    #[error("锁文件解析失败: {0}")]
    LockParse(String),
    #[error("未知的解释器类型: {0}")]
    UnknownKind(String),
    #[error("该解释器正在安装中")]
    AlreadyInstalling,
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

impl ScriptInterpreterError {
    /// 稳定错误码（CommandError.code 用）。
    pub fn code(&self) -> &'static str {
        match self {
            Self::LockParse(_) => "script_interpreter_lock_parse",
            Self::UnknownKind(_) => "script_interpreter_unknown_kind",
            Self::AlreadyInstalling => "script_interpreter_already_installing",
            Self::Download(_) => "script_interpreter_download",
            Self::ChecksumMismatch { .. } => "script_interpreter_checksum_mismatch",
            Self::InstallFailed(_) => "script_interpreter_install",
            Self::SelfTestFailed(_) => "script_interpreter_self_test",
            Self::Cancelled => "script_interpreter_cancelled",
            Self::Io(_) => "script_interpreter_io",
        }
    }
}

// ── Kind ───────────────────────────────────────────────────────────────────

/// 托管解释器种类（与插件 manifest `runtime.type` 的 `python` / `node` 对应）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScriptInterpreterKind {
    Python,
    Node,
}

impl ScriptInterpreterKind {
    /// 从锁文件/前端 wire 字符串解析。
    pub fn parse(s: &str) -> Result<Self, ScriptInterpreterError> {
        match s {
            "python" => Ok(Self::Python),
            "node" => Ok(Self::Node),
            other => Err(ScriptInterpreterError::UnknownKind(other.to_string())),
        }
    }

    /// wire 字符串。
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Python => "python",
            Self::Node => "node",
        }
    }
}

// ── 锁文件 ─────────────────────────────────────────────────────────────────

/// 锁文件根结构。
#[derive(Debug, Clone, Deserialize)]
pub struct ScriptInterpreterLock {
    pub schema_version: u32,
    pub interpreters: Vec<InterpreterLockEntry>,
}

/// 单个解释器的锁定条目。
#[derive(Debug, Clone, Deserialize)]
pub struct InterpreterLockEntry {
    /// 种类（`python` / `node`）。
    pub kind: String,
    /// 发行版版本（如 `3.12.10`）。
    pub version: String,
    /// 下载 URL（主链；镜像候选由 utils::mirrors 按 URL 形态分派）。
    pub url: String,
    /// 发行版 zip 的 SHA-256（hex）。
    pub sha256: String,
    /// zip 大小（字节，进度总量 fallback）。
    pub size_bytes: u64,
    /// 解压后解释器 exe 相对 artifact 根的路径（`/` 分隔）。
    pub exe_relpath: String,
    /// self-test 期望的 `--version` 输出前缀。
    pub version_check: String,
}

impl InterpreterLockEntry {
    /// 版本化不可变 artifact id（`{kind}-{version}`）。
    ///
    /// 版本变更 = 新目录，旧目录由卸载/清理回收——不存在原地升级。
    pub fn artifact_id(&self) -> String {
        format!("{}-{}", self.kind, self.version)
    }
}

/// 解析嵌入的锁文件。
pub fn parse_lock() -> Result<ScriptInterpreterLock, ScriptInterpreterError> {
    serde_json::from_str(SCRIPT_INTERPRETER_LOCK_JSON)
        .map_err(|e| ScriptInterpreterError::LockParse(e.to_string()))
}

/// 取指定种类的锁定条目。
pub fn lock_entry(
    kind: ScriptInterpreterKind,
) -> Result<InterpreterLockEntry, ScriptInterpreterError> {
    let lock = parse_lock()?;
    lock.interpreters
        .into_iter()
        .find(|e| e.kind == kind.as_str())
        .ok_or_else(|| {
            ScriptInterpreterError::LockParse(format!(
                "锁文件缺少 {} 条目",
                kind.as_str()
            ))
        })
}

// ── 路径 ───────────────────────────────────────────────────────────────────

/// 托管解释器根目录：`runtimes/script_interpreters`（复用 local_engine 的
/// runtimes_root，测试态指向临时目录）。
pub fn root() -> PathBuf {
    crate::local_engine::runtime::runtimes_root().join("script_interpreters")
}

/// 指定种类安装后的 artifact 目录。
fn artifact_dir(kind: ScriptInterpreterKind) -> Result<PathBuf, ScriptInterpreterError> {
    Ok(root().join(lock_entry(kind)?.artifact_id()))
}

/// 指定种类的安装内状态（只读，无副作用）。
#[derive(Debug, Clone, Serialize)]
pub struct ScriptInterpreterStatus {
    pub kind: String,
    pub version: String,
    pub installed: bool,
    /// 已安装时的解释器 exe 绝对路径。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exe_path: Option<String>,
}

/// 解析托管解释器 exe 路径（plugin spawn 热路径，纯存在性检查）。
///
/// 未安装返回 `None`——调用方（plugin spawn）转为"脚本运行时未安装"错误项，
/// **不回退到用户 PATH**：版本锁定与系统隔离是托管体系的核心语义。
pub fn resolve(kind: ScriptInterpreterKind) -> Option<PathBuf> {
    let entry = lock_entry(kind).ok()?;
    let exe = root().join(entry.artifact_id()).join(&entry.exe_relpath);
    exe.is_file().then_some(exe)
}

/// 全部托管解释器的安装状态（设置页展示，只读）。
pub fn status() -> Vec<ScriptInterpreterStatus> {
    let mut out = Vec::new();
    for kind in [ScriptInterpreterKind::Python, ScriptInterpreterKind::Node] {
        let Ok(entry) = lock_entry(kind) else {
            continue;
        };
        let exe = resolve(kind);
        out.push(ScriptInterpreterStatus {
            kind: kind.as_str().to_string(),
            version: entry.version.clone(),
            installed: exe.is_some(),
            exe_path: exe.map(|p| p.display().to_string()),
        });
    }
    out
}

// ── 安装 ───────────────────────────────────────────────────────────────────

/// 进程内安装互斥（single-flight）：同 kind 并发安装直接拒绝。
static INSTALLING: std::sync::LazyLock<std::sync::Mutex<std::collections::HashSet<&'static str>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashSet::new()));

/// 安装进度/日志上报回调。
pub trait InstallReporter: Send + Sync {
    fn on_stage(&self, stage: &str);
    fn on_log(&self, level: &str, text: &str);
    /// 下载进度（累计字节 / 总字节，总量未知为 `None`）。实现方自行节流。
    fn on_progress(&self, _downloaded: u64, _total: Option<u64>) {}
}

/// 空实现（测试/无事件环境）。
pub struct NullReporter;
impl InstallReporter for NullReporter {
    fn on_stage(&self, _stage: &str) {}
    fn on_log(&self, _level: &str, _text: &str) {}
}

/// 安装托管解释器（幂等：已安装直接返回成功）。
///
/// 事务性：全部构建发生在 staging，self-test 通过后原子 rename 到目标目录；
/// 任何失败（含取消）只留下 staging 残留，由下次安装/启动清扫回收。
pub async fn install(
    kind: ScriptInterpreterKind,
    cancel_token: Option<&tokio_util::sync::CancellationToken>,
    reporter: &dyn InstallReporter,
) -> Result<(), ScriptInterpreterError> {
    // single-flight：占用即拒绝（不排队——前端按事件反馈，排队语义收益低）。
    {
        let mut guard = INSTALLING.lock().expect("INSTALLING 锁中毒");
        if !guard.insert(kind.as_str()) {
            return Err(ScriptInterpreterError::AlreadyInstalling);
        }
    }
    // panic 时泄漏占用是可接受的降级（进程内状态，重启即愈）；正常路径必释放。
    let result = install_inner(kind, cancel_token, reporter).await;
    INSTALLING.lock().expect("INSTALLING 锁中毒").remove(kind.as_str());
    result
}

async fn install_inner(
    kind: ScriptInterpreterKind,
    cancel_token: Option<&tokio_util::sync::CancellationToken>,
    reporter: &dyn InstallReporter,
) -> Result<(), ScriptInterpreterError> {
    let entry = lock_entry(kind)?;
    let exe_path = root().join(entry.artifact_id()).join(&entry.exe_relpath);
    if exe_path.is_file() {
        tracing::debug!(kind = kind.as_str(), "解释器已安装，跳过");
        return Ok(());
    }

    // 清扫其他孤儿 staging（上次失败/崩溃残留；保留即将创建的自身无冲突）。
    let swept = sweep_staging();
    if swept > 0 {
        tracing::info!(count = swept, "安装前清扫孤儿 staging");
    }

    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let staging = root().join(format!(".staging-{}-{ts}", kind.as_str()));
    std::fs::create_dir_all(&staging)?;
    let cleanup = |staging: &Path| {
        let _ = std::fs::remove_dir_all(staging);
    };

    // ── 1. 下载 zip ──
    reporter.on_stage("downloading");
    reporter.on_log("info", &format!("正在下载 {} {}", kind.as_str(), entry.version));
    let archive = staging.join("archive.zip");
    if let Err(e) =
        download_archive(&entry, &archive, cancel_token, reporter).await
    {
        cleanup(&staging);
        return Err(e);
    }

    // ── 2. 解压（zip slip 防护） ──
    reporter.on_stage("extracting");
    reporter.on_log("info", "正在解压…");
    let extract_dir = staging.join("extract");
    match tokio::task::spawn_blocking({
        let extract_dir = extract_dir.clone();
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
            return Err(ScriptInterpreterError::InstallFailed(format!(
                "解压任务 join 失败: {e}"
            )));
        }
    }

    // ── 3. self-test：exe 存在 + --version 输出匹配锁文件前缀 ──
    reporter.on_stage("verifying");
    reporter.on_log("info", "正在验证解释器…");
    let staged_exe = extract_dir.join(&entry.exe_relpath);
    if !staged_exe.is_file() {
        cleanup(&staging);
        return Err(ScriptInterpreterError::SelfTestFailed(format!(
            "发行版缺少解释器可执行文件: {}",
            entry.exe_relpath
        )));
    }
    if let Err(e) = self_test(&staged_exe, &entry.version_check).await {
        cleanup(&staging);
        reporter.on_log("error", &format!("自检失败: {e}"));
        return Err(e);
    }

    // ── 4. promote：清旧目标（升级残留）→ 原子 rename ──
    reporter.on_stage("promoting");
    let target = root().join(entry.artifact_id());
    if target.exists() {
        // 目标存在但 exe 缺失（异常残留）——覆盖性重建；删失败（占用）如实报错。
        if let Err(e) = std::fs::remove_dir_all(&target) {
            cleanup(&staging);
            return Err(ScriptInterpreterError::InstallFailed(format!(
                "清理旧目标目录失败（文件被占用？）: {e}"
            )));
        }
    }
    if let Err(e) = std::fs::rename(&extract_dir, &target) {
        cleanup(&staging);
        return Err(ScriptInterpreterError::InstallFailed(format!(
            "promote 失败: {e}"
        )));
    }
    cleanup(&staging);

    reporter.on_stage("done");
    reporter.on_log(
        "info",
        &format!("{} {} 安装完成", kind.as_str(), entry.version),
    );
    tracing::info!(
        kind = kind.as_str(),
        version = %entry.version,
        target = %target.display(),
        "脚本解释器安装完成"
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

/// 下载发行版 zip 并校验 SHA-256（单主链 + mirrors 候选）。
async fn download_archive(
    entry: &InterpreterLockEntry,
    dest: &Path,
    cancel_token: Option<&tokio_util::sync::CancellationToken>,
    reporter: &dyn InstallReporter,
) -> Result<(), ScriptInterpreterError> {
    let candidates = crate::utils::mirrors::download_candidates(&entry.url);
    let mut last_err: Option<ScriptInterpreterError> = None;
    for (idx, candidate) in candidates.iter().enumerate() {
        let has_next = idx + 1 < candidates.len();
        match download_once(entry, candidate, dest, cancel_token, reporter).await {
            Ok(()) => return Ok(()),
            Err(e) => {
                if matches!(e, ScriptInterpreterError::Cancelled) {
                    return Err(e);
                }
                // 锁定主链 hash 不匹配 = 上游供应链变更，换源无意义。
                if matches!(e, ScriptInterpreterError::ChecksumMismatch { .. })
                    && candidate.as_str() == entry.url
                {
                    return Err(e);
                }
                tracing::warn!(url = candidate.as_str(), error = %e, has_next, "解释器下载失败");
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
        ScriptInterpreterError::Download(format!("无可用下载源 ({})", entry.url))
    }))
}

/// 单候选源下载 + 全量 SHA-256 校验。
///
/// 镜像内容与锁定 hash 不一致时拒绝——换源不放宽供应链约束。
async fn download_once(
    entry: &InterpreterLockEntry,
    url: &str,
    dest: &Path,
    cancel_token: Option<&tokio_util::sync::CancellationToken>,
    reporter: &dyn InstallReporter,
) -> Result<(), ScriptInterpreterError> {
    use futures::StreamExt;
    use tokio::io::AsyncWriteExt;

    if let Some(ct) = cancel_token
        && ct.is_cancelled()
    {
        return Err(ScriptInterpreterError::Cancelled);
    }

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(600))
        .build()
        .map_err(|e| ScriptInterpreterError::Download(format!("HTTP client 构造失败: {e}")))?;

    let response = tokio::select! {
        r = client.get(url).send() => {
            r.map_err(|e| ScriptInterpreterError::Download(format!("下载失败: {e}")))?
        }
        // cancel_token 为 None 时永不完成，select 退化为纯下载等待。
        _ = wait_cancel(cancel_token) => {
            return Err(ScriptInterpreterError::Cancelled);
        }
    };

    if !response.status().is_success() {
        return Err(ScriptInterpreterError::Download(format!(
            "HTTP {}",
            response.status()
        )));
    }

    let progress_total = response.content_length().or(Some(entry.size_bytes));
    let mut hasher = Sha256::new();
    let mut stream = response.bytes_stream();
    let mut written: u64 = 0;

    // 覆盖写（staging 内独占文件，无需 tmp+rename）。
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
                        return Err(ScriptInterpreterError::Download(format!(
                            "下载流读取失败: {e}"
                        )));
                    }
                    None => break,
                }
            }
            _ = wait_cancel(cancel_token) => {
                let _ = tokio::fs::remove_file(dest).await;
                return Err(ScriptInterpreterError::Cancelled);
            }
        }
    }
    file.flush().await?;

    let actual = format!("{:x}", hasher.finalize());
    if actual != entry.sha256.to_lowercase() {
        let _ = tokio::fs::remove_file(dest).await;
        return Err(ScriptInterpreterError::ChecksumMismatch {
            expected: entry.sha256.clone(),
            actual,
        });
    }
    tracing::debug!(url, bytes = written, "解释器 zip 下载完成且 hash 匹配");
    Ok(())
}

/// 解压 zip 全部 entry 到 `dest`（zip slip 防护：拒绝绝对路径 / 盘符 / `..` 段）。
fn extract_zip(archive: &Path, dest: &Path) -> Result<(), ScriptInterpreterError> {
    let file = std::fs::File::open(archive)?;
    let mut zip = zip::ZipArchive::new(file)
        .map_err(|e| ScriptInterpreterError::InstallFailed(format!("ZIP 打开失败: {e}")))?;

    std::fs::create_dir_all(dest)?;
    for i in 0..zip.len() {
        let mut entry = zip
            .by_index(i)
            .map_err(|e| ScriptInterpreterError::InstallFailed(format!("ZIP entry 读取失败: {e}")))?;
        let name = entry.name().replace('\\', "/");
        let Some(safe_rel) = safe_entry_relpath(&name) else {
            return Err(ScriptInterpreterError::InstallFailed(format!(
                "ZIP entry 路径不安全，拒绝解压: {name}"
            )));
        };
        let out_path = dest.join(&safe_rel);
        // 目录 entry：创建后继续。
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

/// zip entry 相对路径安全化：绝对路径、盘符、`..` 段返回 `None`（fail-closed）。
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
                // Windows 盘符（`c:`）或保留设备名拒绝。
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

/// self-test：跑 `exe --version`，stdout 需以锁文件前缀开头。
async fn self_test(exe: &Path, version_check: &str) -> Result<(), ScriptInterpreterError> {
    let mut cmd = tokio::process::Command::new(exe);
    cmd.arg("--version");
    #[cfg(windows)]
    cmd.creation_flags(crate::platform::CREATE_NO_WINDOW);
    let output = tokio::time::timeout(std::time::Duration::from_secs(15), cmd.output())
        .await
        .map_err(|_| ScriptInterpreterError::SelfTestFailed("执行超时".into()))?
        .map_err(|e| ScriptInterpreterError::SelfTestFailed(format!("执行失败: {e}")))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let combined = if stdout.trim().is_empty() {
        stderr
    } else {
        stdout
    };
    if !combined.trim_start().starts_with(version_check) {
        return Err(ScriptInterpreterError::SelfTestFailed(format!(
            "版本输出不匹配：期望前缀 {version_check:?}，实际 {:?}",
            combined.trim().chars().take(40).collect::<String>()
        )));
    }
    Ok(())
}

/// 卸载：删除指定种类的 artifact 目录。
///
/// 解释器 exe 正被插件进程占用时 Windows 删除失败——如实返回错误，
/// 由前端提示重启后重试（不做强杀插件进程：占用方归 plugin 域管理）。
pub fn uninstall(kind: ScriptInterpreterKind) -> Result<(), ScriptInterpreterError> {
    let dir = artifact_dir(kind)?;
    if !dir.exists() {
        return Ok(()); // 幂等
    }
    std::fs::remove_dir_all(&dir).map_err(|e| {
        ScriptInterpreterError::InstallFailed(format!(
            "卸载失败（解释器可能正被插件进程占用，重启应用后重试）: {e}"
        ))
    })?;
    // 一并回收该 kind 的 staging 残留。
    sweep_staging();
    tracing::info!(kind = kind.as_str(), "脚本解释器已卸载");
    Ok(())
}

/// 清扫根目录下的孤儿 staging（`.staging-*` 前缀），返回清理数量。
///
/// **活跃保护（0.25.20 修复）**：跳过正在安装中的 kind 的 staging——
/// install 开始时的清扫若不排除并发活跃安装（如先点 node 再点 python），
/// 会把对方正在下载的 staging 当孤儿删掉，导致其下载写入 os error 3
/// （"系统找不到指定的路径"），重试才成功。活跃集合读 INSTALLING
/// （install 持锁期在 install_inner 之外，无死锁）。
///
/// 调用时机：安装开始前与 app 启动显式恢复。**不在 status() 里调用**
/// （只读铁则）。跨进程并发安装仍可能互删——单实例应用，可接受。
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
            continue; // 并发活跃安装的 staging，不是孤儿
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
    fn embedded_lock_parses_with_both_kinds() {
        let lock = parse_lock().expect("嵌入锁文件必须可解析");
        assert_eq!(lock.schema_version, 1);
        let mut kinds: Vec<_> = lock.interpreters.iter().map(|e| e.kind.as_str()).collect();
        kinds.sort_unstable();
        assert_eq!(kinds, ["node", "python"]);

        for e in &lock.interpreters {
            assert_eq!(e.sha256.len(), 64, "sha256 必须是 hex64");
            assert!(e.size_bytes > 0);
            assert!(
                safe_entry_relpath(&e.exe_relpath).is_some(),
                "exe_relpath 必须是安全相对路径: {}",
                e.exe_relpath
            );
            assert!(!e.version_check.is_empty());
            assert!(e.url.starts_with("https://"));
        }
    }

    #[test]
    fn kind_parse_roundtrip() {
        for k in [ScriptInterpreterKind::Python, ScriptInterpreterKind::Node] {
            assert_eq!(ScriptInterpreterKind::parse(k.as_str()).unwrap(), k);
        }
        assert!(ScriptInterpreterKind::parse("powershell").is_err());
    }

    #[test]
    fn artifact_id_is_kind_version() {
        let e = lock_entry(ScriptInterpreterKind::Python).unwrap();
        assert_eq!(e.artifact_id(), format!("python-{}", e.version));
    }

    #[test]
    fn resolve_missing_returns_none() {
        // 隔离根（runtimes_root 的 cfg(test) 临时目录）下无安装。
        assert!(resolve(ScriptInterpreterKind::Python).is_none());
        assert!(resolve(ScriptInterpreterKind::Node).is_none());
    }

    #[test]
    fn status_reports_not_installed_on_clean_root() {
        let st = status();
        assert_eq!(st.len(), 2);
        assert!(st.iter().all(|s| !s.installed && s.exe_path.is_none()));
    }

    #[test]
    fn safe_entry_relpath_rejects_escapes() {
        assert!(safe_entry_relpath("python.exe").is_some());
        assert!(safe_entry_relpath("a/b/c.txt").is_some());
        assert!(safe_entry_relpath("a//b/./c.txt").is_some());
        assert!(safe_entry_relpath("/abs/path").is_none());
        assert!(safe_entry_relpath("c:/windows/system32").is_none());
        assert!(safe_entry_relpath("../escape").is_none());
        assert!(safe_entry_relpath("a/../../escape").is_none());
        assert!(safe_entry_relpath("").is_none());
        assert!(safe_entry_relpath(".").is_none());
    }

    #[test]
    fn extract_zip_extracts_and_blocks_slip() {
        let dir = std::env::temp_dir().join(format!("blink-si-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // 构造正常 zip。
        let normal = dir.join("normal.zip");
        {
            let f = std::fs::File::create(&normal).unwrap();
            let mut z = zip::ZipWriter::new(f);
            z.start_file("hello.txt", zip::write::SimpleFileOptions::default())
                .unwrap();
            std::io::Write::write_all(&mut z, b"hi").unwrap();
            z.start_file("sub/nested.txt", zip::write::SimpleFileOptions::default())
                .unwrap();
            std::io::Write::write_all(&mut z, b"nested").unwrap();
            z.finish().unwrap();
        }
        let out = dir.join("out");
        extract_zip(&normal, &out).expect("正常 zip 应解压成功");
        assert_eq!(
            std::fs::read_to_string(out.join("hello.txt")).unwrap(),
            "hi"
        );
        assert_eq!(
            std::fs::read_to_string(out.join("sub/nested.txt")).unwrap(),
            "nested"
        );

        // 构造 zip slip 恶意 zip（`..` 逃逸）。
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
        assert!(matches!(err, ScriptInterpreterError::InstallFailed(_)));
        assert!(!dir.join("escaped.txt").exists());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sweep_staging_removes_only_staging_dirs() {
        let root = root();
        std::fs::create_dir_all(root.join(".staging-python-123")).unwrap();
        std::fs::create_dir_all(root.join("python-9.9.9")).unwrap();
        std::fs::write(root.join(".staging-python-123/keep.txt"), "x").unwrap();

        let cleaned = sweep_staging();
        assert_eq!(cleaned, 1);
        assert!(!root.join(".staging-python-123").exists());
        assert!(root.join("python-9.9.9").exists());

        let _ = std::fs::remove_dir_all(root.join("python-9.9.9"));
    }

    #[test]
    fn install_is_single_flight_per_kind() {
        // 占用后第二次 install 应被拒绝（网络请求不会真正发出——在占用检查即返回）。
        {
            let mut guard = INSTALLING.lock().unwrap();
            assert!(guard.insert("python"));
        }
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let err = rt
            .block_on(install(ScriptInterpreterKind::Python, None, &NullReporter))
            .unwrap_err();
        assert!(matches!(err, ScriptInterpreterError::AlreadyInstalling));
        INSTALLING.lock().unwrap().remove("python");
    }

    /// 真实安装链路集成验证（联网，~11-34MB 下载）。
    ///
    /// `#[ignore]` 常态跳过（单测不依赖网络）；发布前或锁文件更新后显式跑：
    /// `cargo test -p blink-infra script_interpreter -- --ignored`。
    /// 跑在 cfg(test) 隔离根（临时目录），验证 下载→hash→解压→self-test→promote
    /// 全链路与锁文件 URL/sha256 的真实性。
    #[test]
    #[ignore = "联网下载真实发行版，显式运行"]
    fn install_full_pipeline_downloads_and_installs_python() {
        // enable_all：self-test 的 tokio::time::timeout 需要计时器。
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        // 两种发行版 zip 结构不同（python 解压即根、node 带版本顶层目录），
        // 顺序安装分别验证 exe_relpath；幂等重装只在 python 上断言。
        for kind in [ScriptInterpreterKind::Python, ScriptInterpreterKind::Node] {
            rt.block_on(async {
                install(kind, None, &NullReporter)
                    .await
                    .expect("托管解释器全链路安装应成功");
            });
            let exe = resolve(kind).expect("安装后 resolve 必须命中");
            assert!(exe.is_file());
        }
        rt.block_on(async {
            install(ScriptInterpreterKind::Python, None, &NullReporter)
                .await
                .expect("重复安装应幂等成功");
        });
        // 清理隔离根（测试串行收尾，避免删到并行测试的 staging）。
        let _ = std::fs::remove_dir_all(root());
    }
}
