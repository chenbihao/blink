//! 进程持久化 lease 与崩溃恢复（0.22.6.1）。
//!
//! 提供版本化、可原子写入的进程 lease 文件，用于 Blink 崩溃重启后
//! 判断是否有遗留受管引擎进程可安全接管或需要报告。
//!
//! ## 核心设计
//!
//! - **lease 文件位置**：`%APPDATA%\blink\runtimes\leases\{engine_id}.json`
//!   每个 engine 一份 lease 文件，文件名即 engine_id，原子写入用
//!   `write tmp → rename(MOVEFILE_REPLACE_EXISTING | WRITE_THROUGH)` 替换。
//! - **原子写入**：先写临时文件（同目录），再用 `MoveFileExW` 原子替换。
//!   非 Windows 回退到 `std::fs::rename`。
//! - **删除安全性**：删除前必须验证规范化路径和 instance id。
//!   文件名即 engine_id，不接受外部路径。
//! - **恢复判断为纯测决策函数**：`RecoveryDecision` 由证据闭合驱动，
//!   任何证据不足或不符都返回 `DoNotAdopt`，只产结构化诊断，不终止进程。
//! - **token 安全**：lease 只存 token fingerprint，不存明文 token。
//!   恢复时无法仅凭 lease 完成 token health 验证——保持 fail-closed。
//! - **Job Object 不被替代**：lease 是崩溃恢复证据，正常退出仍依赖 Job Object。
//!
//! ## 分层归属
//!
//! - `infra/local_engine`：不依赖 `crate::app` 或 `crate::domain`。
//!   lease 本身只存 infra 层原语（PID、路径、endpoint、token fingerprint）。

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

// ── Lease 数据模型 ─────────────────────────────────────────────────────────

/// lease 文件的 schema 版本。
///
/// 未来 lease 字段扩展时递增，旧版本 lease 在恢复时按版本路由处理。
pub const LEASE_SCHEMA_VERSION: u32 = 1;

/// 持久化进程 lease 记录。
///
/// 记录 Blink 受管引擎进程的身份快照，用于崩溃恢复时的安全判断。
///
/// **安全铁则**：
/// - 只存 token fingerprint，不存明文 token。
/// - 文件名即 `engine_id`（只允许小写字母/数字/连字符）。
/// - 写入时使用原子 rename。
/// - 删除时必须验证 instance_id 匹配。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProcessLease {
    /// lease schema 版本。
    pub schema_version: u32,
    /// 引擎 id（如 "funasr"、"paddleocr"）。
    pub engine_id: String,
    /// 实例 id（随机生成，用于区分同引擎的不同启动实例）。
    pub instance_id: String,
    /// OS PID。
    pub pid: u32,
    /// OS 真实进程创建时间（Unix 毫秒时间戳，用于防 PID 复用）。
    /// 0 表示查询失败——恢复时视为证据不足（fail-closed）。
    pub creation_time_ms: u64,
    /// 规范化可执行文件路径。
    pub executable: String,
    /// 服务端点（如 "127.0.0.1:8100"）。
    pub endpoint: String,
    /// token 的 fingerprint（SHA-256 前 16 hex 字符，带 "fp:" 前缀）。
    /// 不存明文 token。
    pub token_fingerprint: String,
    /// generation/install id（与 runtime generation manifest 对应）。
    pub generation_id: String,
    /// lease 写入时间（Unix 毫秒时间戳）。
    pub written_at_ms: u64,
}

impl ProcessLease {
    /// 创建新的 lease 记录。
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        engine_id: impl Into<String>,
        instance_id: impl Into<String>,
        pid: u32,
        creation_time_ms: u64,
        executable: impl Into<String>,
        endpoint: impl Into<String>,
        token_fingerprint: impl Into<String>,
        generation_id: impl Into<String>,
    ) -> Self {
        Self {
            schema_version: LEASE_SCHEMA_VERSION,
            engine_id: engine_id.into(),
            instance_id: instance_id.into(),
            pid,
            creation_time_ms,
            executable: executable.into(),
            endpoint: endpoint.into(),
            token_fingerprint: token_fingerprint.into(),
            generation_id: generation_id.into(),
            written_at_ms: now_unix_ms(),
        }
    }
}

// ── Lease 目录与路径 ──────────────────────────────────────────────────────

/// 返回 lease 文件目录：`%APPDATA%\blink\runtimes\leases`
///
/// 测试环境返回临时目录，不触碰真实 `%APPDATA%\blink`。
pub fn leases_dir() -> PathBuf {
    super::runtime::runtimes_root().join("leases")
}

/// 返回指定 engine 的 lease 文件路径。
///
/// 文件名即 `engine_id.json`，不接受外部路径注入。
fn lease_path(engine_id: &str) -> PathBuf {
    leases_dir().join(format!("{engine_id}.json"))
}

/// 验证 engine_id 只含合法字符（防路径穿越）。
fn validate_engine_id(engine_id: &str) -> bool {
    !engine_id.is_empty()
        && engine_id.len() <= 64
        && engine_id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

// ── 原子写入 / 删除 / 扫描 ─────────────────────────────────────────────────

/// lease 写入/删除错误。
#[derive(Debug, thiserror::Error)]
pub enum LeaseError {
    #[error("engine_id 无效: {0}")]
    InvalidEngineId(String),
    #[error("lease 文件 IO 错误: {0}")]
    Io(String),
    #[error("lease 序列化错误: {0}")]
    Serialize(String),
    #[error("lease 反序列化错误: {0}")]
    Deserialize(String),
    #[error("instance_id 不匹配，拒绝删除 lease: expected={expected}, got={actual}")]
    InstanceMismatch { expected: String, actual: String },
    #[error("lease 路径不属于 Blink runtime 根目录: {0}")]
    PathOutsideRuntime(String),
}

/// 原子写入 lease 文件。
///
/// 先写临时文件，再用 rename 原子替换。临时文件与目标文件在同一目录，
/// 保证 rename 在同一卷上（Windows 要求同卷 rename）。
pub fn write_lease(lease: &ProcessLease) -> Result<(), LeaseError> {
    if !validate_engine_id(&lease.engine_id) {
        return Err(LeaseError::InvalidEngineId(lease.engine_id.clone()));
    }

    let dir = leases_dir();
    std::fs::create_dir_all(&dir).map_err(|e| LeaseError::Io(e.to_string()))?;

    let target = lease_path(&lease.engine_id);
    let tmp_suffix = lease_tmp_suffix();
    let tmp = dir.join(format!(".{}.{}", lease.engine_id, tmp_suffix));

    // 写临时文件
    let json =
        serde_json::to_string_pretty(lease).map_err(|e| LeaseError::Serialize(e.to_string()))?;

    std::fs::write(&tmp, json.as_bytes()).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        LeaseError::Io(e.to_string())
    })?;

    // 原子 rename
    atomic_rename(&tmp, &target).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        LeaseError::Io(e)
    })?;

    tracing::debug!(
        engine_id = %lease.engine_id,
        instance_id = %lease.instance_id,
        pid = lease.pid,
        "lease 已写入: {}", target.display()
    );
    Ok(())
}

/// 删除指定 engine 的 lease。
///
/// **安全检查**：
/// 1. engine_id 合法
/// 2. 读取 lease 文件并验证 instance_id 匹配
/// 3. 验证规范化路径在 runtime 根目录内
/// 4. 删除文件
///
/// 如果 lease 文件不存在，返回 `Ok(())`（幂等删除）。
/// 如果 instance_id 不匹配，返回 `Err(InstanceMismatch)`，不删除。
pub fn remove_lease(engine_id: &str, instance_id: &str) -> Result<(), LeaseError> {
    if !validate_engine_id(engine_id) {
        return Err(LeaseError::InvalidEngineId(engine_id.to_string()));
    }

    let path = lease_path(engine_id);

    // 先检查文件是否存在——幂等：lease 不存在视为已删除
    // 在路径验证之前检查，避免对不存在的路径 canonicalize 失败
    if !path.exists() {
        return Ok(());
    }

    // 验证路径在 runtime 根目录内
    let runtime_root = super::runtime::runtimes_root();
    verify_path_within(&path, &runtime_root)?;

    // 读取并验证 instance_id
    let content = std::fs::read_to_string(&path).map_err(|e| LeaseError::Io(e.to_string()))?;
    let lease: ProcessLease =
        serde_json::from_str(&content).map_err(|e| LeaseError::Deserialize(e.to_string()))?;

    if lease.instance_id != instance_id {
        tracing::warn!(
            engine_id,
            expected = instance_id,
            actual = %lease.instance_id,
            "拒绝删除 lease: instance_id 不匹配"
        );
        return Err(LeaseError::InstanceMismatch {
            expected: instance_id.to_string(),
            actual: lease.instance_id,
        });
    }

    std::fs::remove_file(&path).map_err(|e| LeaseError::Io(e.to_string()))?;

    tracing::debug!(engine_id, instance_id, "lease 已删除: {}", path.display());
    Ok(())
}

/// 强制删除 lease（不验证 instance_id）。
///
/// 仅用于引擎卸载/清理场景，调用方需确保安全。
pub fn remove_lease_force(engine_id: &str) -> Result<(), LeaseError> {
    if !validate_engine_id(engine_id) {
        return Err(LeaseError::InvalidEngineId(engine_id.to_string()));
    }

    let path = lease_path(engine_id);

    if !path.exists() {
        return Ok(());
    }

    let runtime_root = super::runtime::runtimes_root();
    verify_path_within(&path, &runtime_root)?;

    std::fs::remove_file(&path).map_err(|e| LeaseError::Io(e.to_string()))?;

    tracing::debug!(engine_id, "lease 已强制删除: {}", path.display());
    Ok(())
}

/// 扫描所有 lease 文件。
///
/// 遍历 `leases_dir` 下的所有 `*.json` 文件，解析为 `ProcessLease`。
/// 损坏或无法解析的文件跳过（记录 warn 日志），不中断扫描。
pub fn scan_leases() -> Vec<ProcessLease> {
    let dir = leases_dir();
    let mut leases = Vec::new();

    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return leases, // 目录不存在，无 lease
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }

        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(path = %path.display(), %e, "lease 扫描: 读取失败，跳过");
                continue;
            }
        };

        match serde_json::from_str::<ProcessLease>(&content) {
            Ok(lease) => {
                // 验证路径安全（防路径穿越）
                let runtime_root = super::runtime::runtimes_root();
                if verify_path_within(&path, &runtime_root).is_err() {
                    tracing::warn!(
                        path = %path.display(),
                        "lease 扫描: 路径在 runtime 根目录外，跳过"
                    );
                    continue;
                }
                leases.push(lease);
            }
            Err(e) => {
                tracing::warn!(
                    path = %path.display(),
                    %e,
                    "lease 扫描: 反序列化失败，跳过"
                );
            }
        }
    }

    leases
}

// ── 恢复决策（纯测函数）────────────────────────────────────────────────────

/// 恢复判定结果。
///
/// **核心铁则**：任何不匹配场景都返回 `DoNotAdopt`，不接管、不终止。
/// 只有全部证据闭合时才返回 `Adoptable`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecoveryDecision {
    /// 全部证据闭合——可以有限恢复/回收。
    /// 调用方可选择接管或终止此进程。
    Adoptable {
        engine_id: String,
        instance_id: String,
        pid: u32,
    },
    /// 不接管、不终止——只产结构化诊断。
    DoNotAdopt(RecoveryDiagnostics),
}

/// 恢复诊断信息（不包含明文 token）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveryDiagnostics {
    /// 引擎 id。
    pub engine_id: String,
    /// 实例 id。
    pub instance_id: String,
    /// PID。
    pub pid: u32,
    /// 不匹配的原因分类。
    pub reason: RecoveryReason,
    /// 可读诊断描述（不含明文 token）。
    pub detail: String,
}

/// 恢复不匹配原因分类。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecoveryReason {
    /// PID 不存在（进程已退出）→ 清除 stale lease。
    PidNotFound,
    /// PID 存在但可执行路径不匹配（PID 可能被复用）。
    ExecutableMismatch { expected: String, actual: String },
    /// PID 存在但进程创建时间不匹配（PID 被复用）。
    CreationTimeMismatch { expected: u64, actual: u64 },
    /// lease 中 creation_time_ms 为 0（原始查询失败）。
    CreationTimeMissing,
    /// 无法查询进程的可执行路径或创建时间。
    ProcessQueryFailed,
    /// token fingerprint 不匹配（health 返回了不同 fingerprint）。
    TokenFingerprintMismatch,
    /// instance id 不匹配（health 返回了不同 instance）。
    InstanceIdMismatch,
    /// engine id 不匹配（health 返回了不同 engine）。
    EngineIdMismatch,
    /// 无法完成 health 验证（服务不可达）。
    HealthUnreachable,
    /// lease schema 版本不兼容。
    SchemaVersion { expected: u32, actual: u32 },
}

/// 平台进程身份证据（从 OS 查询得到）。
///
/// 恢复决策需要调用方从 OS 查询进程身份后传入。
/// 这使得 `decide_recovery` 成为纯函数，可单元测试。
#[derive(Debug, Clone)]
pub struct ProcessEvidence {
    /// PID 是否存在。
    pub pid_exists: bool,
    /// 实际可执行路径（如果 PID 存在且可查询）。
    pub actual_executable: Option<String>,
    /// 实际进程创建时间（如果 PID 存在且可查询）。
    pub actual_creation_time_ms: Option<u64>,
}

/// health 验证证据（从 health 端点查询得到）。
///
/// 恢复决策需要调用方从 health 端点查询身份回显后传入。
/// 如果 health 不可达，传 `None`。
#[derive(Debug, Clone)]
pub struct HealthEvidence {
    /// health 回显的 engine id。
    pub engine_id: Option<String>,
    /// health 回显的 instance id。
    pub instance_id: Option<String>,
    /// health 回显的 token fingerprint。
    pub token_fingerprint: Option<String>,
}

/// 恢复决策纯函数。
///
/// 输入 lease + 进程证据 + health 证据（可选），输出 `RecoveryDecision`。
///
/// ## 决策逻辑
///
/// 1. **lease schema 版本不兼容** → `DoNotAdopt`
/// 2. **PID 不存在** → `DoNotAdopt(PidNotFound)` — 调用方应清除 stale lease
/// 3. **creation_time_ms == 0**（lease 原始查询失败） → `DoNotAdopt(CreationTimeMissing)`
/// 4. **PID 存在但无法查询实际可执行路径** → `DoNotAdopt(ProcessQueryFailed)`
/// 5. **PID 存在但可执行路径不匹配** → `DoNotAdopt(ExecutableMismatch)`
/// 6. **PID 存在但进程创建时间不匹配** → `DoNotAdopt(CreationTimeMismatch)`
/// 7. **health 证据为 None**（服务不可达） → `DoNotAdopt(HealthUnreachable)`
/// 8. **health 回显的 engine id 不匹配** → `DoNotAdopt(EngineIdMismatch)`
/// 9. **health 回显的 instance id 不匹配** → `DoNotAdopt(InstanceIdMismatch)`
/// 10. **health 回显的 token fingerprint 不匹配** → `DoNotAdopt(TokenFingerprintMismatch)`
/// 11. **全部证据闭合** → `Adoptable`
///
/// **安全设计分叉**：第 7 步是 fail-closed 的关键——
/// health 不可达时，无法验证端口上的服务属于 lease 记录的进程。
/// 即使 PID、路径和创建时间都匹配，也不接管，因为：
/// - 端口可能被其他进程占用（原进程已退出，PID 被复用且恰好路径也匹配的极低概率除外）
/// - 无法排除第三方进程恰好绑定到同一端口
/// - token 无法验证，明文 token 不在 lease 中
///
/// 只有 health 证据 + token fingerprint 全部闭合时才允许 `Adoptable`。
/// 但即使 `Adoptable`，调用方也不应自动 kill——
/// `Adoptable` 意味着"可以接管"，具体是否回收由调用方决策。
pub fn decide_recovery(
    lease: &ProcessLease,
    process: &ProcessEvidence,
    health: Option<&HealthEvidence>,
) -> RecoveryDecision {
    // 1. schema 版本检查
    if lease.schema_version != LEASE_SCHEMA_VERSION {
        return RecoveryDecision::DoNotAdopt(RecoveryDiagnostics {
            engine_id: lease.engine_id.clone(),
            instance_id: lease.instance_id.clone(),
            pid: lease.pid,
            reason: RecoveryReason::SchemaVersion {
                expected: LEASE_SCHEMA_VERSION,
                actual: lease.schema_version,
            },
            detail: format!(
                "lease schema 版本不兼容: expected {}, got {}",
                LEASE_SCHEMA_VERSION, lease.schema_version
            ),
        });
    }

    // 2. PID 不存在 → stale lease
    if !process.pid_exists {
        return RecoveryDecision::DoNotAdopt(RecoveryDiagnostics {
            engine_id: lease.engine_id.clone(),
            instance_id: lease.instance_id.clone(),
            pid: lease.pid,
            reason: RecoveryReason::PidNotFound,
            detail: "PID 不存在（进程已退出），应清除 stale lease".to_string(),
        });
    }

    // 3. lease creation_time_ms 为 0（原始查询失败）
    if lease.creation_time_ms == 0 {
        return RecoveryDecision::DoNotAdopt(RecoveryDiagnostics {
            engine_id: lease.engine_id.clone(),
            instance_id: lease.instance_id.clone(),
            pid: lease.pid,
            reason: RecoveryReason::CreationTimeMissing,
            detail: "lease 中 creation_time_ms 为 0（原始查询失败），证据不足".to_string(),
        });
    }

    // 4. PID 存在但无法查询实际可执行路径
    let actual_exe = match &process.actual_executable {
        Some(e) => e.clone(),
        None => {
            return RecoveryDecision::DoNotAdopt(RecoveryDiagnostics {
                engine_id: lease.engine_id.clone(),
                instance_id: lease.instance_id.clone(),
                pid: lease.pid,
                reason: RecoveryReason::ProcessQueryFailed,
                detail: "无法查询进程的可执行文件路径".to_string(),
            });
        }
    };

    // 5. 可执行路径不匹配
    if !paths_match_normalized(&lease.executable, &actual_exe) {
        return RecoveryDecision::DoNotAdopt(RecoveryDiagnostics {
            engine_id: lease.engine_id.clone(),
            instance_id: lease.instance_id.clone(),
            pid: lease.pid,
            reason: RecoveryReason::ExecutableMismatch {
                expected: lease.executable.clone(),
                actual: actual_exe.clone(),
            },
            detail: format!(
                "可执行文件不匹配: expected={}, actual={}",
                lease.executable, actual_exe
            ),
        });
    }

    // 6. 进程创建时间不匹配
    let actual_creation = match process.actual_creation_time_ms {
        Some(t) => t,
        None => {
            return RecoveryDecision::DoNotAdopt(RecoveryDiagnostics {
                engine_id: lease.engine_id.clone(),
                instance_id: lease.instance_id.clone(),
                pid: lease.pid,
                reason: RecoveryReason::ProcessQueryFailed,
                detail: "无法查询进程的创建时间".to_string(),
            });
        }
    };

    // 允许 2 秒误差（OS 创建时间精度差异）
    let diff = if actual_creation > lease.creation_time_ms {
        actual_creation - lease.creation_time_ms
    } else {
        lease.creation_time_ms - actual_creation
    };
    if diff > 2000 {
        return RecoveryDecision::DoNotAdopt(RecoveryDiagnostics {
            engine_id: lease.engine_id.clone(),
            instance_id: lease.instance_id.clone(),
            pid: lease.pid,
            reason: RecoveryReason::CreationTimeMismatch {
                expected: lease.creation_time_ms,
                actual: actual_creation,
            },
            detail: format!(
                "进程创建时间不匹配（PID 可能被复用）: expected={}, actual={}, diff={}ms",
                lease.creation_time_ms, actual_creation, diff
            ),
        });
    }

    // 7. health 证据为 None（服务不可达）
    let health_ev = match health {
        Some(h) => h,
        None => {
            return RecoveryDecision::DoNotAdopt(RecoveryDiagnostics {
                engine_id: lease.engine_id.clone(),
                instance_id: lease.instance_id.clone(),
                pid: lease.pid,
                reason: RecoveryReason::HealthUnreachable,
                detail: "health 端点不可达，无法完成身份验证（fail-closed）".to_string(),
            });
        }
    };

    // 8. engine id 不匹配
    if let Some(ref health_engine) = health_ev.engine_id {
        if health_engine != &lease.engine_id {
            return RecoveryDecision::DoNotAdopt(RecoveryDiagnostics {
                engine_id: lease.engine_id.clone(),
                instance_id: lease.instance_id.clone(),
                pid: lease.pid,
                reason: RecoveryReason::EngineIdMismatch,
                detail: format!(
                    "health 回显的 engine id 不匹配: expected={}, actual={}",
                    lease.engine_id, health_engine
                ),
            });
        }
    } else {
        // health 没有回显 engine id
        return RecoveryDecision::DoNotAdopt(RecoveryDiagnostics {
            engine_id: lease.engine_id.clone(),
            instance_id: lease.instance_id.clone(),
            pid: lease.pid,
            reason: RecoveryReason::EngineIdMismatch,
            detail: "health 回显缺少 engine id".to_string(),
        });
    }

    // 9. instance id 不匹配
    if let Some(ref health_instance) = health_ev.instance_id {
        if health_instance != &lease.instance_id {
            return RecoveryDecision::DoNotAdopt(RecoveryDiagnostics {
                engine_id: lease.engine_id.clone(),
                instance_id: lease.instance_id.clone(),
                pid: lease.pid,
                reason: RecoveryReason::InstanceIdMismatch,
                detail: format!(
                    "health 回显的 instance id 不匹配: expected={}, actual={}",
                    lease.instance_id, health_instance
                ),
            });
        }
    } else {
        return RecoveryDecision::DoNotAdopt(RecoveryDiagnostics {
            engine_id: lease.engine_id.clone(),
            instance_id: lease.instance_id.clone(),
            pid: lease.pid,
            reason: RecoveryReason::InstanceIdMismatch,
            detail: "health 回显缺少 instance id".to_string(),
        });
    }

    // 10. token fingerprint 不匹配
    if let Some(ref health_fp) = health_ev.token_fingerprint {
        if health_fp != &lease.token_fingerprint {
            return RecoveryDecision::DoNotAdopt(RecoveryDiagnostics {
                engine_id: lease.engine_id.clone(),
                instance_id: lease.instance_id.clone(),
                pid: lease.pid,
                reason: RecoveryReason::TokenFingerprintMismatch,
                detail: "health 回显的 token fingerprint 不匹配".to_string(),
            });
        }
    } else {
        return RecoveryDecision::DoNotAdopt(RecoveryDiagnostics {
            engine_id: lease.engine_id.clone(),
            instance_id: lease.instance_id.clone(),
            pid: lease.pid,
            reason: RecoveryReason::TokenFingerprintMismatch,
            detail: "health 回显缺少 token fingerprint".to_string(),
        });
    }

    // 11. 全部证据闭合
    RecoveryDecision::Adoptable {
        engine_id: lease.engine_id.clone(),
        instance_id: lease.instance_id.clone(),
        pid: lease.pid,
    }
}

// ── 内部辅助 ───────────────────────────────────────────────────────────────

/// 当前 Unix 毫秒时间戳。
fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// lease 临时文件后缀。
fn lease_tmp_suffix() -> String {
    std::env::var("BLINK_TEST_LEASE_TMP_SUFFIX").unwrap_or_else(|_| "tmp".to_string())
}

/// 原子 rename（Windows 用 MoveFileExW，其他平台用 std::fs::rename）。
///
/// Windows 上 MoveFileExW 偶发"拒绝访问 (0x80070005)"——杀软实时扫描或
/// 并发测试竞争短暂占用文件句柄。这是暂态错误，有限次重试即可恢复。
#[cfg(windows)]
fn atomic_rename(from: &Path, to: &Path) -> Result<(), String> {
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;

    fn to_wide(s: &OsStr) -> Vec<u16> {
        s.encode_wide().chain(std::iter::once(0)).collect()
    }

    let from_w = to_wide(from.as_os_str());
    let to_w = to_wide(to.as_os_str());

    // MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH
    const MOVEFILE_REPLACE_EXISTING: u32 = 0x01;
    const MOVEFILE_WRITE_THROUGH: u32 = 0x08;

    const MAX_RETRIES: u32 = 5;
    let mut last_err: Option<windows::core::Error> = None;
    for attempt in 0..MAX_RETRIES {
        let result = unsafe {
            windows::Win32::Storage::FileSystem::MoveFileExW(
                windows::core::PCWSTR(from_w.as_ptr()),
                windows::core::PCWSTR(to_w.as_ptr()),
                windows::Win32::Storage::FileSystem::MOVE_FILE_FLAGS(
                    MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
                ),
            )
        };
        match result {
            Ok(()) => return Ok(()),
            Err(e) => {
                // 0x80070005 = ERROR_ACCESS_DENIED（杀软/文件锁竞争）
                if e.code() == windows::core::HRESULT::from_win32(0x80070005)
                    && attempt + 1 < MAX_RETRIES
                {
                    tracing::debug!(
                        attempt = attempt + 1,
                        max = MAX_RETRIES,
                        "MoveFileExW 暂态拒绝访问，重试"
                    );
                    std::thread::sleep(std::time::Duration::from_millis(10 * (1 << attempt)));
                    last_err = Some(e);
                    continue;
                }
                return Err(format!("MoveFileExW 失败: {e}"));
            }
        }
    }
    // 所有重试用尽
    Err(format!(
        "MoveFileExW 重试用尽: {}",
        last_err.unwrap_or_else(|| windows::core::Error::from(windows::Win32::Foundation::E_FAIL))
    ))
}

/// 原子 rename（非 Windows）。
#[cfg(not(windows))]
fn atomic_rename(from: &Path, to: &Path) -> Result<(), String> {
    std::fs::rename(from, to).map_err(|e| format!("rename 失败: {e}"))
}

/// 验证路径在指定根目录内。
///
/// 规范化路径后检查是否以 `root` 为前缀。
fn verify_path_within(path: &Path, root: &Path) -> Result<(), LeaseError> {
    let canonical_path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let canonical_root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());

    if !canonical_path.starts_with(&canonical_root) {
        return Err(LeaseError::PathOutsideRuntime(format!(
            "{} 不在 {} 内",
            canonical_path.display(),
            canonical_root.display()
        )));
    }
    Ok(())
}

/// 规范化路径比较（大小写不敏感 + 路径分隔符归一）。
///
/// 不直接 canonicalize（文件可能不存在），而是做字符串级归一。
fn paths_match_normalized(a: &str, b: &str) -> bool {
    let normalize = |s: &str| -> String { s.replace('\\', "/").to_lowercase() };
    let a_norm = normalize(a);
    let b_norm = normalize(b);

    // 直接比较
    if a_norm == b_norm {
        return true;
    }

    // 尝试 canonicalize 比较（如果文件存在）
    let pa = Path::new(a);
    let pb = Path::new(b);
    if let (Ok(ca), Ok(cb)) = (pa.canonicalize(), pb.canonicalize()) {
        let ca_str = ca.to_string_lossy().replace('\\', "/").to_lowercase();
        let cb_str = cb.to_string_lossy().replace('\\', "/").to_lowercase();
        return ca_str == cb_str;
    }

    false
}

// ── 测试 ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests;
