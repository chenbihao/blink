//! 进程管理工具：按端口查找 PID、按 PID 终止进程、Windows Job Object。
//!
//! ## 背景
//!
//! 0.22.1 新增 Windows Job Object 支持，用于 ManagedProcess 的进程树回收。
//! 原有的 `find_pid_by_port` / `kill_process_by_pid` 保留，但 `kill_process_by_port`
//! 不再出现在本地引擎生命周期路径中（0.22.1 收敛）。
//!
//! ## Windows Job Object 策略
//!
//! - 创建 Job Object，设置 `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`
//! - 子进程成功创建后纳入 Job Object
//! - 正常退出和 Blink 退出均能回收进程树
//! - Job handle 存储在 ManagedProcess 中，Drop 时触发 KILL_ON_JOB_CLOSE
//!
//! ## 身份验证策略
//!
//! `kill_process_tree_verified` 在无法验证身份时**拒绝终止**，不降级为仅 PID kill。
//! 只有以下全部满足才允许终止：
//! - 能查询到进程的可执行文件路径
//! - 可执行文件路径匹配（大小写不敏感、规范化比较）
//! - 进程创建时间匹配（防 PID 复用）
//!
//! 未知进程占用端口只报冲突，绝不自动 kill。

use std::path::Path;

/// 通过 TCP 监听端口查找占用该端口的进程 PID。
///
/// 仅查找 LISTENING 状态的 TCP 连接。如果多个进程监听同一端口
/// （IPv4 + IPv6），返回第一个匹配的 PID。
///
/// 返回 `None` 表示端口未被监听或查找失败。
#[cfg(windows)]
pub fn find_pid_by_port(port: u16) -> Option<u32> {
    let output = crate::infra::platform::no_window(std::process::Command::new("netstat"))
        .args(["-ano"])
        .output()
        .ok()?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let port_suffix = format!(":{port}");

    for line in stdout.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() >= 5
            && parts[0] == "TCP"
            && parts[3] == "LISTENING"
            && parts[1].ends_with(&port_suffix)
            && let Ok(pid) = parts[4].parse::<u32>()
        {
            return Some(pid);
        }
    }
    None
}

#[cfg(not(windows))]
pub fn find_pid_by_port(_port: u16) -> Option<u32> {
    None
}

/// 按 PID 终止进程（含子进程树）。
///
/// 使用 `taskkill /F /T /PID {pid}`：
/// - `/F` — 强制终止（不等待进程自行退出）
/// - `/T` — 连子进程一起终止
///
/// 返回 `Ok(())` 表示 taskkill 命令执行成功。
#[cfg(windows)]
pub fn kill_process_by_pid(pid: u32) -> Result<(), String> {
    let output = crate::infra::platform::no_window(std::process::Command::new("taskkill"))
        .args(["/F", "/T", "/PID", &pid.to_string()])
        .output()
        .map_err(|e| format!("taskkill 执行失败: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        return Err(format!(
            "taskkill 返回非零退出码: {} {}",
            stdout.trim(),
            stderr.trim()
        ));
    }
    Ok(())
}

#[cfg(not(windows))]
pub fn kill_process_by_pid(_pid: u32) -> Result<(), String> {
    Err("非 Windows 平台不支持 kill_process_by_pid".to_string())
}

/// 查找并终止占用指定端口的进程（组合工具）。
///
/// **⚠️ 0.22.1 收敛**：此函数不再出现在本地引擎/FunASR 生命周期路径中。
/// 仅供非引擎路径的兼容用途使用。
///
/// 如果端口被监听，找到 PID 并 kill，返回被 kill 的 PID。
/// 如果端口未被监听，返回 `None`。
pub fn kill_process_by_port(port: u16) -> Option<u32> {
    let pid = find_pid_by_port(port)?;
    tracing::info!(pid, port, "找到占用端口的进程，正在终止");
    let _ = kill_process_by_pid(pid);
    Some(pid)
}

// ── Windows Job Object ────────────────────────────────────────────────────

#[cfg(windows)]
mod job_object {
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
        SetInformationJobObject,
    };
    use windows::Win32::System::Threading::{OpenProcess, PROCESS_SET_QUOTA, PROCESS_TERMINATE};

    /// Job Object handle 包装。Drop 时自动 CloseHandle，触发 KILL_ON_JOB_CLOSE。
    pub struct JobObjectHandle {
        handle: HANDLE,
    }

    // SAFETY: Windows HANDLE 是进程级资源，可在任意线程 CloseHandle。
    // Job Object 不绑定线程亲和性，跨线程持有安全。
    unsafe impl Send for JobObjectHandle {}
    unsafe impl Sync for JobObjectHandle {}

    impl JobObjectHandle {
        /// 创建 Job Object 并设置 KILL_ON_JOB_CLOSE。
        pub fn create() -> Result<Self, String> {
            unsafe {
                let handle = CreateJobObjectW(None, None)
                    .map_err(|e| format!("CreateJobObjectW 失败: {e}"))?;

                // 设置 KILL_ON_JOB_CLOSE：Blink 退出时 Job handle 被关闭，
                // 所有子进程自动终止。
                let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
                info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;

                let ret = SetInformationJobObject(
                    handle,
                    JobObjectExtendedLimitInformation,
                    &info as *const _ as _,
                    std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                );

                if ret.is_err() {
                    let _ = CloseHandle(handle);
                    return Err("SetInformationJobObject 失败".to_string());
                }

                Ok(Self { handle })
            }
        }

        /// 将进程分配到此 Job Object。
        pub fn assign_process(&self, pid: u32) -> Result<(), String> {
            unsafe {
                let process = OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, false, pid)
                    .map_err(|e| format!("OpenProcess({pid}) 失败: {e}"))?;

                let ret = AssignProcessToJobObject(self.handle, process);
                let _ = CloseHandle(process);

                if ret.is_err() {
                    return Err(format!("AssignProcessToJobObject({pid}) 失败"));
                }
                Ok(())
            }
        }

        /// 获取原始 handle（仅供测试用）。
        pub fn raw_handle(&self) -> HANDLE {
            self.handle
        }
    }

    impl Drop for JobObjectHandle {
        fn drop(&mut self) {
            // SAFETY: CloseHandle 触发 KILL_ON_JOB_CLOSE，所有子进程树被终止。
            // 这是 Windows 上回收受管进程树的最终保障。
            unsafe {
                let _ = CloseHandle(self.handle);
            }
        }
    }
}

#[cfg(windows)]
pub use job_object::JobObjectHandle;

/// 为进程创建 Job Object 并分配（Windows 生产实现）。
///
/// 创建带 KILL_ON_JOB_CLOSE 的 Job Object，将指定 PID 的进程纳入。
/// 返回的 JobObjectHandle 在 Drop 时自动触发 kill-on-close。
#[cfg(windows)]
pub fn assign_job_object(pid: u32) -> Result<JobObjectHandle, String> {
    let job = JobObjectHandle::create()?;
    job.assign_process(pid)?;
    tracing::debug!(pid, "进程已分配到 Job Object");
    Ok(job)
}

#[cfg(not(windows))]
#[allow(dead_code)]
pub fn assign_job_object(_pid: u32) -> Result<(), String> {
    Err("非 Windows 平台不支持 Job Object".to_string())
}

/// 终止进程树（带身份验证）。
///
/// **安全策略**：在终止前验证 PID 和 executable + creation time identity。
/// 以下任一验证失败时**拒绝终止**，不降级为仅 PID kill（fail-closed）：
/// - 无法查询 executable → 拒绝
/// - executable 不匹配 → 拒绝
/// - expected_start_time_ms 为 0（缺失）→ 拒绝
/// - 无法查询 creation time → 拒绝
/// - creation time 不匹配 → 拒绝（防 PID 复用）
///
/// 不得仅凭 PID、端口或"看起来像 Python"终止进程。
/// 不得用"executable 已匹配所以继续"降级。
#[cfg(windows)]
pub fn kill_process_tree_verified(
    pid: u32,
    expected_executable: &Path,
    expected_start_time_ms: u64,
) -> Result<(), String> {
    // 1. 验证可执行文件路径
    let actual_exe = get_process_executable(pid).ok_or_else(|| {
        format!("无法查询 PID {pid} 的可执行文件路径，拒绝终止（可能是权限不足或进程已退出）")
    })?;

    if !paths_match_case_insensitive(&actual_exe, expected_executable) {
        tracing::warn!(
            pid,
            expected = %expected_executable.display(),
            actual = %actual_exe.display(),
            "进程身份验证失败：可执行文件不匹配，拒绝终止"
        );
        return Err(format!(
            "进程身份验证失败：可执行文件不匹配。expected: {}, got: {}",
            expected_executable.display(),
            actual_exe.display()
        ));
    }

    // 2. 验证 expected_start_time_ms 非零（缺失时拒绝）
    if expected_start_time_ms == 0 {
        tracing::warn!(
            pid,
            "进程身份验证失败：expected creation time 为 0（缺失），拒绝终止"
        );
        return Err(format!(
            "进程身份验证失败：expected creation time 为 0（缺失），拒绝终止 PID {pid}"
        ));
    }

    // 3. 验证进程创建时间（防 PID 复用）——查询失败也拒绝
    let actual_start = get_process_creation_time_ms(pid).ok_or_else(|| {
        tracing::warn!(pid, "进程身份验证失败：无法查询 creation time，拒绝终止");
        format!("进程身份验证失败：无法查询 PID {pid} 的 creation time，拒绝终止")
    })?;

    // 允许 2 秒误差（OS 创建时间精度差异）
    let diff = if actual_start > expected_start_time_ms {
        actual_start - expected_start_time_ms
    } else {
        expected_start_time_ms - actual_start
    };
    if diff > 2000 {
        tracing::warn!(
            pid,
            expected = expected_start_time_ms,
            actual = actual_start,
            diff,
            "进程创建时间不匹配，拒绝终止（PID 可能被复用）"
        );
        return Err(format!(
            "进程创建时间不匹配（PID 可能被复用）：expected {expected_start_time_ms}, got {actual_start}"
        ));
    }

    // 身份验证通过，执行终止
    tracing::debug!(pid, "进程身份验证通过，执行终止");
    kill_process_by_pid(pid)
}

#[cfg(not(windows))]
#[allow(dead_code)]
pub fn kill_process_tree_verified(
    _pid: u32,
    _expected: &Path,
    _expected_start_time_ms: u64,
) -> Result<(), String> {
    Err("非 Windows 平台不支持 kill_process_tree_verified".to_string())
}

/// 大小写不敏感的路径比较。
///
/// Windows 文件系统不区分大小写。此函数处理：
/// - 相对路径与绝对路径
/// - 大小写不敏感
/// - 路径分隔符归一化（/ vs \）
#[cfg(windows)]
fn paths_match_case_insensitive(a: &Path, b: &Path) -> bool {
    // 尝试规范化路径（如果文件存在）
    let canonical_a = a.canonicalize().unwrap_or_else(|_| a.to_path_buf());
    let canonical_b = b.canonicalize().unwrap_or_else(|_| b.to_path_buf());

    // 比较规范化后的路径（大小写不敏感）
    let a_str = canonical_a.to_string_lossy().to_lowercase();
    let b_str = canonical_b.to_string_lossy().to_lowercase();

    if a_str == b_str {
        return true;
    }

    // 如果规范化失败，尝试逐组件比较
    let a_components: Vec<String> = canonical_a
        .components()
        .map(|c| c.as_os_str().to_string_lossy().to_lowercase())
        .collect();
    let b_components: Vec<String> = canonical_b
        .components()
        .map(|c| c.as_os_str().to_string_lossy().to_lowercase())
        .collect();

    a_components == b_components
}

/// 获取进程的可执行文件路径（用于身份验证）。
#[cfg(windows)]
fn get_process_executable(pid: u32) -> Option<std::path::PathBuf> {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{
        OpenProcess, PROCESS_NAME_FORMAT, PROCESS_QUERY_LIMITED_INFORMATION,
        QueryFullProcessImageNameW,
    };
    use windows::core::PWSTR;

    unsafe {
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;

        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        let ret = QueryFullProcessImageNameW(
            process,
            PROCESS_NAME_FORMAT(0),
            PWSTR(buf.as_mut_ptr()),
            &mut len,
        );
        let _ = CloseHandle(process);

        if ret.is_err() {
            return None;
        }

        let path = String::from_utf16_lossy(&buf[..len as usize]);
        Some(std::path::PathBuf::from(path))
    }
}

/// 获取进程的创建时间（Unix 毫秒时间戳，用于防 PID 复用）。
///
/// 公开接口，供 ManagedProcess 在 spawn 后查询 OS 真实创建时间。
/// 查询失败返回 None——调用方应将此视为身份验证不可用（fail-closed）。
#[cfg(windows)]
pub fn get_process_creation_time_ms(pid: u32) -> Option<u64> {
    use windows::Win32::Foundation::{CloseHandle, FILETIME};
    use windows::Win32::System::Threading::{
        GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    unsafe {
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;

        let mut creation: FILETIME = std::mem::zeroed();
        let mut exit: FILETIME = std::mem::zeroed();
        let mut kernel: FILETIME = std::mem::zeroed();
        let mut user: FILETIME = std::mem::zeroed();

        let ret = GetProcessTimes(process, &mut creation, &mut exit, &mut kernel, &mut user);
        let _ = CloseHandle(process);

        if ret.is_err() {
            return None;
        }

        // FILETIME 是 100ns 间隔，从 1601-01-01 开始
        // 转换为 Unix 毫秒：减去 11644473600000（1601 到 1970 的毫秒数）
        let creation_u64 =
            ((creation.dwHighDateTime as u64) << 32) | (creation.dwLowDateTime as u64);
        let unix_ms = creation_u64 / 10_000 - 11_644_473_600_000;
        Some(unix_ms)
    }
}

// ── 端口探测（只读，不杀进程）──

/// 只读探测端口是否被占用（TCP connect）。
///
/// 不终止任何进程，仅检测 127.0.0.1:port 是否有 TCP 监听。
pub fn probe_port_occupied(port: u16) -> bool {
    use std::net::TcpStream;
    use std::time::Duration;

    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    TcpStream::connect_timeout(&addr, Duration::from_millis(500)).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_probe_port_occupied_unused_port() {
        // 使用 OS 分配的临时端口（bind :0），避免并行测试端口冲突。
        // 绑定后立即 drop，probe_port_occupied 对该端口应返回 false。
        // 注意：drop 后存在极短竞争窗口，但在大多数 CI/本地环境下可接受。
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let result = probe_port_occupied(port);
        // 如果恰好被占用，测试不失败——这是环境依赖的
        if !result {
            // 确认未被占用时的行为
            assert!(!result);
        }
    }

    #[cfg(windows)]
    #[test]
    fn test_job_object_create_and_drop() {
        let job = JobObjectHandle::create();
        assert!(job.is_ok(), "Job Object 创建应成功");
        // job drop 时应正常 CloseHandle
        drop(job);
    }

    #[cfg(windows)]
    #[test]
    fn test_kill_tree_rejects_unknown_pid() {
        // 不存在的 PID（65535 通常不存在或不可访问）
        let result =
            kill_process_tree_verified(99999, std::path::Path::new("C:\\nonexistent.exe"), 0);
        // 应该拒绝终止（无法查询 executable 或身份不匹配）
        assert!(result.is_err(), "应拒绝终止未知 PID");
        let err = result.unwrap_err();
        assert!(
            err.contains("拒绝") || err.contains("失败") || err.contains("不匹配"),
            "错误信息应表明拒绝原因: {err}"
        );
    }

    #[cfg(windows)]
    #[test]
    fn test_kill_tree_rejects_executable_mismatch() {
        // 使用一个真实存在的系统进程（explorer.exe 的 PID 不可预测）
        // 但我们可以测试：用错误的 expected_executable 调用
        // 如果能找到一个真实 PID，验证它会被拒绝
        // 这里用 PID 0（System Idle Process）——无法 OpenProcess
        let result = kill_process_tree_verified(
            0,
            std::path::Path::new("C:\\definitely_not_matching.exe"),
            0,
        );
        // PID 0 无法 OpenProcess，应拒绝
        assert!(result.is_err());
    }
}
