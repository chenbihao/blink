//! 0.18.6 命令执行 MVP：`> ` 前缀调起外部终端执行命令。
//!
//! 设计参考 `phases/0.18-enhancement-chord.md` §3.5 / §5.7。
//!
//! - 前缀解析纯函数（`is_command_mode` / `extract_command`）含单测
//! - `run_in_terminal` command：探测 wt.exe → ShellExecuteW 调起终端
//! - 不在主窗口内联渲染输出（MVP 范围）
//! - 日志不记命令内容（可能含敏感参数）

// ── 前缀解析纯函数 ────────────────────────────────────────────────────────

/// 判断输入是否处于命令模式（以 `>` 开头）。
///
/// 用户在主窗口输入 `> ` 前缀即进入命令模式：
/// - `">"` → true（刚输入前缀，尚无命令）
/// - `"> "` → true（前缀 + 空格，尚无命令）
/// - `"> echo hi"` → true（前缀 + 命令）
/// - `""` → false
/// - `"echo hi"` → false（普通搜索词）
///
/// **注**：前端 JS 实现等价逻辑（`command-mode.js`），此 Rust 函数为权威 spec + 单测载体。
#[allow(dead_code)]
pub fn is_command_mode(input: &str) -> bool {
    input.starts_with('>')
}

/// 从命令模式输入中提取命令文本。
///
/// 去掉 `>` 前缀及后续前导空白后，返回剩余部分：
/// - `">"` → `None`（无命令）
/// - `"> "` → `None`（无命令）
/// - `"> echo hi"` → `Some("echo hi")`
/// - `">echo hi"` → `Some("echo hi")`（宽松解析，容错无空格）
/// - `""` / `"echo hi"` → `None`（非命令模式）
///
/// **注**：前端 JS 实现等价逻辑（`command-mode.js`），此 Rust 函数为权威 spec + 单测载体。
#[allow(dead_code)]
pub fn extract_command(input: &str) -> Option<String> {
    let rest = input.strip_prefix('>')?;
    let command = rest.trim_start();
    if command.is_empty() {
        None
    } else {
        Some(command.to_string())
    }
}

// ── run_in_terminal command ──────────────────────────────────────────────

/// 0.18.6：在外部终端中执行命令。
///
/// 探测 Windows Terminal（`wt.exe`），存在则用 WT 打开新标签页执行，
/// 否则 fallback 到 `cmd.exe`。不在主窗口内联渲染输出。
///
/// **安全**：命令执行是用户主动输入（`> ` 前缀），不走 Action trait 危险动作白名单。
/// 日志不记命令内容（可能含敏感参数）。
#[tauri::command]
pub async fn run_in_terminal(command: String) -> Result<(), String> {
    let command = command.trim().to_string();
    if command.is_empty() {
        return Err("命令为空".to_string());
    }

    tracing::info!("run_in_terminal: 执行命令");

    let result = tokio::task::spawn_blocking(move || run_in_terminal_blocking(&command)).await;

    match result {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => {
            tracing::warn!(error = %e, "run_in_terminal: 执行失败");
            Err(e)
        }
        Err(e) => {
            tracing::warn!(error = %e, "run_in_terminal: spawn_blocking 失败");
            Err(format!("内部错误: {e}"))
        }
    }
}

/// 同步实现：ShellExecuteW 调起终端。
#[cfg(target_os = "windows")]
fn run_in_terminal_blocking(command: &str) -> Result<(), String> {
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    use windows::core::{PCWSTR, w};

    // 探测 wt.exe：`where wt.exe`（加 CREATE_NO_WINDOW 避免闪黑窗）
    let has_wt = crate::infra::platform::no_window(std::process::Command::new("where"))
        .args(["wt.exe"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);

    tracing::debug!(has_wt, "run_in_terminal: 终端类型");

    // 工作目录默认用户主目录（Blink 进程 cwd 不可控，用 USERPROFILE 最合理）
    let cwd = std::env::var("USERPROFILE").unwrap_or_else(|_| ".".to_string());

    let (exe, args) = if has_wt {
        // Windows Terminal：新标签页 + 指定目录 + cmd /K 保持窗口
        let args = format!("-d \"{cwd}\" cmd /K {command}");
        ("wt.exe", args)
    } else {
        // Fallback：cmd.exe /K 保持窗口
        let args = format!("/K {command}");
        ("cmd.exe", args)
    };

    let exe_wide: Vec<u16> = exe.encode_utf16().chain(std::iter::once(0)).collect();
    let args_wide: Vec<u16> = args.encode_utf16().chain(std::iter::once(0)).collect();

    let result = unsafe {
        ShellExecuteW(
            None,
            w!("open"),
            PCWSTR(exe_wide.as_ptr()),
            PCWSTR(args_wide.as_ptr()),
            None,
            SW_SHOWNORMAL,
        )
    };

    // ShellExecuteW 返回值 > 32 表示成功
    if result.0 as i32 <= 32 {
        return Err(format!("ShellExecuteW 失败，返回值: {}", result.0 as i32));
    }

    Ok(())
}

#[cfg(not(target_os = "windows"))]
fn run_in_terminal_blocking(_command: &str) -> Result<(), String> {
    Err("当前平台暂不支持命令执行".to_string())
}

// ── 单测 ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── is_command_mode ──

    #[test]
    fn test_is_command_mode_gt_only() {
        assert!(is_command_mode(">"));
    }

    #[test]
    fn test_is_command_mode_gt_space() {
        assert!(is_command_mode("> "));
    }

    #[test]
    fn test_is_command_mode_with_command() {
        assert!(is_command_mode("> echo hi"));
    }

    #[test]
    fn test_is_command_mode_empty() {
        assert!(!is_command_mode(""));
    }

    #[test]
    fn test_is_command_mode_normal_search() {
        assert!(!is_command_mode("echo hi"));
    }

    #[test]
    fn test_is_command_mode_gt_no_space_command() {
        // 宽松：`>echo` 也算命令模式
        assert!(is_command_mode(">echo hi"));
    }

    // ── extract_command ──

    #[test]
    fn test_extract_command_gt_only() {
        assert_eq!(extract_command(">"), None);
    }

    #[test]
    fn test_extract_command_gt_space() {
        assert_eq!(extract_command("> "), None);
    }

    #[test]
    fn test_extract_command_with_command() {
        assert_eq!(extract_command("> echo hi"), Some("echo hi".to_string()));
    }

    #[test]
    fn test_extract_command_empty() {
        assert_eq!(extract_command(""), None);
    }

    #[test]
    fn test_extract_command_normal_search() {
        assert_eq!(extract_command("echo hi"), None);
    }

    #[test]
    fn test_extract_command_gt_no_space() {
        // 宽松解析：`>echo hi` → Some("echo hi")
        assert_eq!(extract_command(">echo hi"), Some("echo hi".to_string()));
    }

    #[test]
    fn test_extract_command_multiple_spaces() {
        assert_eq!(extract_command(">   echo hi"), Some("echo hi".to_string()));
    }

    #[test]
    fn test_extract_command_preserves_internal_spaces() {
        assert_eq!(
            extract_command("> echo \"hello world\""),
            Some("echo \"hello world\"".to_string())
        );
    }
}
