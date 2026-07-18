//! 进程管理工具：按端口查找 PID、按 PID 终止进程。
//!
//! 主要用于 funasr-server 孤儿进程清理。
//!
//! ## 背景
//!
//! Blink 异常退出（崩溃 / 任务管理器杀掉）后，funasr-server 子进程可能
//! 变成孤儿进程继续运行，占用监听端口。Blink 重启后 `FUNASR_SERVER_CHILD`
//! 为空，无法通过 child handle 管理，需要通过端口 → PID → kill 的方式清理。
//!
//! ## 实现
//!
//! 使用 `netstat -ano` + `taskkill /F /T /PID`，不依赖额外 windows crate feature。
//! 这不是热路径（仅在 start/stop funasr-server 时调用），性能不是考量因素。

/// 通过 TCP 监听端口查找占用该端口的进程 PID。
///
/// 仅查找 LISTENING 状态的 TCP 连接。如果多个进程监听同一端口
/// （IPv4 + IPv6），返回第一个匹配的 PID。
///
/// 返回 `None` 表示端口未被监听或查找失败。
#[cfg(windows)]
pub fn find_pid_by_port(port: u16) -> Option<u32> {
    let output = std::process::Command::new("netstat")
        .args(["-ano"])
        .output()
        .ok()?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let port_suffix = format!(":{port}");

    for line in stdout.lines() {
        // netstat -ano 输出格式（状态列始终为英文，不受系统语言影响）:
        //   TCP    0.0.0.0:8000           0.0.0.0:0              LISTENING       274296
        //   TCP    [::]:8000              [::]:0                 LISTENING       274296
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() >= 5
            && parts[0] == "TCP"
            && parts[3] == "LISTENING"
            && parts[1].ends_with(&port_suffix)
        {
            if let Ok(pid) = parts[4].parse::<u32>() {
                return Some(pid);
            }
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
/// - `/T` — 连子进程一起终止（funasr-server 可能 spawn 了子进程）
///
/// 返回 `Ok(())` 表示 taskkill 命令执行成功（不代表目标进程一定已退出，
/// 但 taskkill /F 通常能立即终止同用户进程）。
#[cfg(windows)]
pub fn kill_process_by_pid(pid: u32) -> Result<(), String> {
    let output = std::process::Command::new("taskkill")
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
/// 如果端口被监听，找到 PID 并 kill，返回被 kill 的 PID。
/// 如果端口未被监听，返回 `None`（无需清理）。
///
/// 用于 funasr-server 孤儿进程清理。
pub fn kill_process_by_port(port: u16) -> Option<u32> {
    let pid = find_pid_by_port(port)?;
    tracing::info!(pid, port, "找到占用端口的进程，正在终止");
    // 忽略 kill 错误——调用方会通过 is_server_ready 再次确认端口是否释放
    let _ = kill_process_by_pid(pid);
    Some(pid)
}
