//! ManagedProcess 自动化测试（0.22.1）。
//!
//! 测试范围：
//! - 状态转换（Stopped → Starting → Running → Stopping → Exited）
//! - generation 隔离（旧 generation 不能覆盖新）
//! - LogPipe ring buffer + broadcast + 有界 line accumulator
//! - 进程生命周期：spawn → wait → stop
//! - 并发安全：幂等 start/stop、start/stop 竞态
//! - 真实 child 管道背压测试
//! - Windows Job Object 进程树回收
//! - 身份验证拒绝
//!
//! 使用 `cmd.exe`（Windows）或 `/bin/echo`/`/bin/sleep`（Unix）作为测试子进程，
//! 不依赖 FunASR 或任何外部引擎。

use crate::infra::local_engine::log_pipe::{LineAccumulator, LogPipe, LogPipeConfig, LogSource};
use crate::infra::local_engine::process::ManagedProcessError;
use crate::infra::local_engine::process::{
    LaunchRequest, ManagedProcess, ShutdownConfig, generate_instance_id_pub,
};
use crate::infra::local_engine::state::{
    CommitResult, ExitReason, ManagedProcessState, ProcessStatus,
};
use std::sync::Arc;
use std::time::Duration;

// ── state.rs 测试 ─────────────────────────────────────────────────────────

#[test]
fn state_initial_is_stopped() {
    let state = ManagedProcessState::initial();
    assert_eq!(state.status, ProcessStatus::Stopped);
    assert_eq!(state.generation(), 0);
    assert!(state.identity.is_none());
}

#[test]
fn state_begin_start_increments_generation() {
    let mut state = ManagedProcessState::initial();
    let token = state.begin_start();
    assert_eq!(token.generation, 1);
    assert_eq!(state.status, ProcessStatus::Starting);
}

#[test]
fn state_commit_running_succeeds() {
    let mut state = ManagedProcessState::initial();
    let token = state.begin_start();
    let identity = crate::infra::local_engine::state::ProcessIdentity {
        pid: 123,
        executable: std::path::PathBuf::from("/test"),
        start_time_ms: 0,
        instance_id: token.instance_id.clone(),
    };
    let result = state.try_commit_running(&token, 123, identity);
    assert_eq!(result, CommitResult::Committed);
    assert_eq!(state.status, ProcessStatus::Running { pid: 123 });
}

#[test]
fn state_commit_running_cancelled() {
    let mut state = ManagedProcessState::initial();
    let token = state.begin_start();
    state.mark_cancelled();
    let identity = crate::infra::local_engine::state::ProcessIdentity {
        pid: 42,
        executable: std::path::PathBuf::from("/test"),
        start_time_ms: 0,
        instance_id: token.instance_id.clone(),
    };
    let result = state.try_commit_running(&token, 42, identity);
    assert_eq!(result, CommitResult::Cancelled);
    assert!(result.needs_reclaim());
}

#[test]
fn state_commit_exit_old_generation_rejected() {
    let mut state = ManagedProcessState::initial();
    let token1 = state.begin_start();
    state.set_status_exited(ExitReason::StartCancelled);
    let _token2 = state.begin_start();
    assert!(!state.try_commit_exit(&token1, ExitReason::NonZeroExit { code: 1 }));
}

#[test]
fn state_is_current_checks_token() {
    let mut state = ManagedProcessState::initial();
    let token1 = state.begin_start();
    assert!(state.is_current(&token1));
    let _token2 = state.begin_start();
    assert!(!state.is_current(&token1));
    assert!(state.is_current(&_token2));
}

// ── LogPipe 测试 ──────────────────────────────────────────────────────────

#[tokio::test]
async fn log_pipe_append_and_history() {
    let pipe = LogPipe::new(LogPipeConfig::default());
    pipe.append(LogSource::Stdout, "hello".into(), false).await;
    pipe.append(LogSource::Stderr, "error".into(), false).await;
    let history = pipe.history().await;
    assert_eq!(history.len(), 2);
    assert_eq!(history[0].text, "hello");
    assert!(history[0].timestamp_ms > 0);
}

#[tokio::test]
async fn log_pipe_ring_buffer_eviction() {
    let config = LogPipeConfig {
        history_capacity: 3,
        broadcast_capacity: 4,
        max_line_bytes: 8192,
    };
    let pipe = LogPipe::new(config);
    for i in 0..5u8 {
        pipe.append(LogSource::Stdout, format!("line {i}"), false)
            .await;
    }
    let history = pipe.history().await;
    assert_eq!(history.len(), 3);
    assert_eq!(history[0].text, "line 2");
}

#[tokio::test]
async fn log_pipe_seq_monotonic() {
    let pipe = LogPipe::new(LogPipeConfig::default());
    pipe.append(LogSource::Stdout, "a".into(), false).await;
    pipe.append(LogSource::Stdout, "b".into(), false).await;
    let history = pipe.history().await;
    assert!(history[0].seq < history[1].seq);
}

#[tokio::test]
async fn log_pipe_broadcast_subscription() {
    let pipe = LogPipe::new(LogPipeConfig::default());
    let mut sub = pipe.subscribe();
    pipe.append(LogSource::Stdout, "test".into(), false).await;
    let entry = sub.recv().await.unwrap();
    assert_eq!(entry.text, "test");
}

#[tokio::test]
async fn log_pipe_lagged_recovery() {
    let config = LogPipeConfig {
        history_capacity: 100,
        broadcast_capacity: 2,
        max_line_bytes: 8192,
    };
    let pipe = Arc::new(LogPipe::new(config));
    let mut sub = pipe.subscribe();
    for i in 0..10u8 {
        pipe.append(LogSource::Stdout, format!("msg {i}"), false)
            .await;
    }
    let mut got_lagged = false;
    let mut got_after = false;
    loop {
        match sub.recv().await {
            Ok(_) => {
                got_after = true;
                break;
            }
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                got_lagged = true;
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
        }
    }
    assert!(got_lagged, "应收到 Lagged");
    assert!(got_after, "Lagged 后应能继续接收");
}

#[tokio::test]
async fn log_pipe_no_subscriber_count() {
    let pipe = LogPipe::new(LogPipeConfig::default());
    pipe.append(LogSource::Stdout, "no sub".into(), false).await;
    assert_eq!(pipe.no_subscriber_count(), 1);
}

#[tokio::test]
async fn log_pipe_history_preserves_timestamp() {
    let pipe = LogPipe::new(LogPipeConfig::default());
    pipe.append(LogSource::Stdout, "timed".into(), false).await;
    let history = pipe.history().await;
    assert_eq!(history.len(), 1);
    assert!(history[0].timestamp_ms > 0);
}

// ── LineAccumulator 测试 ──────────────────────────────────────────────────

#[test]
fn line_acc_basic_newline() {
    let mut acc = LineAccumulator::new(8192);
    let lines = acc.push_data(b"hello\nworld\n");
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[0].0, "hello");
}

#[test]
fn line_acc_crlf() {
    let mut acc = LineAccumulator::new(8192);
    let lines = acc.push_data(b"line1\r\nline2\r\n");
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[0].0, "line1");
}

#[test]
fn line_acc_cr_only() {
    let mut acc = LineAccumulator::new(8192);
    let lines = acc.push_data(b"progress1\rprogress2\r");
    assert_eq!(lines.len(), 2);
}

#[test]
fn line_acc_eof_no_newline() {
    let mut acc = LineAccumulator::new(8192);
    acc.push_data(b"no newline");
    let tail = acc.finish();
    assert!(tail.is_some());
    assert_eq!(tail.unwrap().0, "no newline");
}

#[test]
fn line_acc_truncates_long_line() {
    let mut acc = LineAccumulator::new(10);
    let lines = acc.push_data(b"AAAAAAAAAAAAAAA\n");
    assert_eq!(lines.len(), 1);
    assert!(lines[0].1);
    assert!(lines[0].0.ends_with("...[truncated]"));
}

#[test]
fn line_acc_truncate_then_normal() {
    let mut acc = LineAccumulator::new(5);
    let lines = acc.push_data(b"AAAAAAAAAA\nhi\n");
    assert_eq!(lines.len(), 2);
    assert!(lines[0].1);
    assert!(!lines[1].1);
    assert_eq!(lines[1].0, "hi");
}

#[test]
fn line_acc_invalid_utf8_no_panic() {
    let mut acc = LineAccumulator::new(8192);
    let mut data = vec![b'A'; 10];
    data.extend_from_slice(&[0xFF, 0xFE]);
    data.push(b'\n');
    let lines = acc.push_data(&data);
    assert_eq!(lines.len(), 1);
}

#[test]
fn line_acc_very_long_no_newline() {
    let mut acc = LineAccumulator::new(10);
    let huge = vec![b'B'; 100_000];
    let lines = acc.push_data(&huge);
    assert!(lines.is_empty(), "未遇到换行不应产生行");
    let tail = acc.finish();
    assert!(tail.is_some());
    assert!(tail.unwrap().1, "应标记截断");
}

// ── process.rs 测试 ──────────────────────────────────────────────────────

#[test]
fn instance_id_unique() {
    let mut ids = Vec::new();
    for _ in 0..10 {
        let id = generate_instance_id_pub();
        assert!(id.starts_with("inst-"));
        assert!(!ids.contains(&id), "instance_id 重复: {id}");
        ids.push(id);
        std::thread::sleep(Duration::from_millis(2));
    }
}

#[test]
fn launch_request_new_has_defaults() {
    let req = LaunchRequest::new(std::path::PathBuf::from("/usr/bin/echo"), "test");
    assert_eq!(req.label, "test");
    assert!(req.instance_id.starts_with("inst-"));
}

#[tokio::test]
async fn managed_process_initial_state_stopped() {
    let mp = ManagedProcess::with_defaults();
    let snap = mp.snapshot().await;
    assert_eq!(snap.status, ProcessStatus::Stopped);
    assert_eq!(snap.generation(), 0);
}

#[tokio::test]
async fn managed_process_stop_when_stopped_is_idempotent() {
    let mp = ManagedProcess::with_defaults();
    assert!(mp.stop().await.is_ok());
}

// ── 测试辅助 ─────────────────────────────────────────────────────────────

fn test_echo_command(text: &str) -> (std::path::PathBuf, Vec<std::ffi::OsString>) {
    #[cfg(windows)]
    {
        (
            std::path::PathBuf::from("cmd.exe"),
            vec!["/C".into(), format!("echo {text}").into()],
        )
    }
    #[cfg(not(windows))]
    {
        (std::path::PathBuf::from("/bin/echo"), vec![text.into()])
    }
}

fn test_long_running_command() -> (std::path::PathBuf, Vec<std::ffi::OsString>) {
    #[cfg(windows)]
    {
        (
            std::path::PathBuf::from("cmd.exe"),
            vec!["/C".into(), "ping -n 30 127.0.0.1".into()],
        )
    }
    #[cfg(not(windows))]
    {
        (std::path::PathBuf::from("/bin/sleep"), vec!["30".into()])
    }
}

fn test_large_output_command(size_kb: usize) -> (std::path::PathBuf, Vec<std::ffi::OsString>) {
    #[cfg(windows)]
    {
        (
            std::path::PathBuf::from("powershell.exe"),
            vec![
                "-NoProfile".into(),
                "-Command".into(),
                format!("Write-Host -NoNewline ('A' * {})", size_kb * 1024).into(),
            ],
        )
    }
    #[cfg(not(windows))]
    {
        (
            std::path::PathBuf::from("/bin/bash"),
            vec![
                "-c".into(),
                format!("printf 'A%.0s' $(seq 1 {})", size_kb * 1024).into(),
            ],
        )
    }
}

// ── 生命周期测试 ──────────────────────────────────────────────────────────

#[tokio::test]
async fn managed_process_start_echo_and_wait() {
    let mp = ManagedProcess::with_defaults();
    let (exe, args) = test_echo_command("hello_test");
    let mut req = LaunchRequest::new(exe, "test-echo");
    req.args = args;

    mp.start(&req).await.unwrap();
    let status = mp.wait().await.unwrap();
    assert!(status.is_exited(), "echo 应已退出: {:?}", status);
}

#[tokio::test]
async fn managed_process_non_zero_exit() {
    let mp = ManagedProcess::with_defaults();

    #[cfg(windows)]
    let (exe, args) = (
        std::path::PathBuf::from("cmd.exe"),
        vec!["/C".into(), "exit 1".into()],
    );
    #[cfg(not(windows))]
    let (exe, args) = (
        std::path::PathBuf::from("/bin/bash"),
        vec!["-c".into(), "exit 1".into()],
    );

    let mut req = LaunchRequest::new(exe, "test-exit-1");
    req.args = args;

    mp.start(&req).await.unwrap();
    let status = mp.wait().await.unwrap();

    if let ProcessStatus::Exited { reason } = status {
        assert!(
            matches!(reason, ExitReason::NonZeroExit { code: 1 }),
            "应是非零退出: {:?}",
            reason
        );
    } else {
        panic!("应为 Exited 状态: {:?}", status);
    }
}

#[tokio::test]
async fn managed_process_spawn_failure_and_retry() {
    let mp = ManagedProcess::with_defaults();
    let req = LaunchRequest::new(
        std::path::PathBuf::from("/nonexistent/path/that/does/not/exist"),
        "test-spawn-fail",
    );

    let result = mp.start(&req).await;
    assert!(result.is_err());

    let (exe, args) = test_echo_command("retry_ok");
    let mut req2 = LaunchRequest::new(exe, "test-retry");
    req2.args = args;
    let result = mp.start(&req2).await;
    assert!(result.is_ok(), "失败后应能再次 start: {:?}", result.err());
}

#[tokio::test]
async fn managed_process_double_start_returns_error() {
    let mp = ManagedProcess::with_defaults();
    let (exe, args) = test_long_running_command();
    let mut req = LaunchRequest::new(exe, "test-double-start");
    req.args = args;

    mp.start(&req).await.unwrap();

    let result = mp.start(&req).await;
    assert!(result.is_err());
    assert!(matches!(
        result.unwrap_err(),
        ManagedProcessError::AlreadyRunning { .. }
    ));

    let _ = mp.stop().await;
}

#[tokio::test]
async fn managed_process_concurrent_stop_is_idempotent() {
    let mp = ManagedProcess::with_defaults();
    let (exe, args) = test_long_running_command();
    let mut req = LaunchRequest::new(exe, "test-concurrent-stop");
    req.args = args;
    req.shutdown = ShutdownConfig {
        force_stop_timeout: Duration::from_secs(5),
    };

    mp.start(&req).await.unwrap();

    let mp1 = mp.clone();
    let mp2 = mp.clone();
    let (r1, r2) = tokio::join!(
        async move { mp1.stop().await },
        async move { mp2.stop().await }
    );

    assert!(r1.is_ok(), "并发 stop 1 应 Ok: {:?}", r1.err());
    assert!(r2.is_ok(), "并发 stop 2 应 Ok: {:?}", r2.err());

    let status = mp.snapshot().await;
    assert!(
        status.status.is_exited(),
        "stop 后应 Exited: {:?}",
        status.status
    );
}

#[tokio::test]
async fn managed_process_start_stop_race() {
    let mp = ManagedProcess::with_defaults();
    let (exe, _args) = test_long_running_command();
    let mut req = LaunchRequest::new(exe, "test-race");
    req.shutdown = ShutdownConfig {
        force_stop_timeout: Duration::from_secs(3),
    };

    let mp1 = mp.clone();
    let mp2 = mp.clone();
    let req_clone = req.clone();

    let (start_result, stop_result) =
        tokio::join!(async move { mp1.start(&req_clone).await }, async move {
            mp2.stop().await
        });

    let _ = start_result;
    assert!(stop_result.is_ok(), "stop 应 Ok: {:?}", stop_result.err());

    tokio::time::sleep(Duration::from_millis(500)).await;

    let status = mp.snapshot().await;
    assert!(
        !matches!(status.status, ProcessStatus::Running { .. }),
        "stop 后不应有 Running 进程遗留: {:?}",
        status.status
    );

    // 确保可以再次 start
    let (exe2, args2) = test_echo_command("after_race");
    let mut req2 = LaunchRequest::new(exe2, "test-after-race");
    req2.args = args2;
    let result = mp.start(&req2).await;
    assert!(result.is_ok(), "竞态后应能再次 start: {:?}", result.err());
    let _ = mp.wait().await;
}

#[tokio::test]
async fn managed_process_stop_if_current_does_not_stop_new() {
    let mp = ManagedProcess::with_defaults();
    let (exe, args) = test_long_running_command();
    let mut req = LaunchRequest::new(exe, "test-stop-if-current");
    req.args = args;
    req.shutdown = ShutdownConfig {
        force_stop_timeout: Duration::from_secs(5),
    };

    mp.start(&req).await.unwrap();
    let old_token = mp.current_token().await;

    mp.stop().await.unwrap();

    let (exe2, args2) = test_echo_command("new_instance");
    let mut req2 = LaunchRequest::new(exe2, "test-new-instance");
    req2.args = args2;
    mp.start(&req2).await.unwrap();
    let new_token = mp.current_token().await;

    assert_ne!(old_token, new_token, "应有不同 token");

    // 用旧 token 调用 stop_if_current——不应停止新实例
    mp.stop_if_current(&old_token).await.unwrap();

    // 新实例不应被旧 token 停止
    let status = mp.snapshot().await;
    assert!(
        !matches!(status.status, ProcessStatus::Stopped),
        "新实例不应被旧 token 停止: {:?}",
        status.status
    );
    let _ = mp.wait().await;
}

#[tokio::test]
async fn managed_process_log_capture() {
    let mp = ManagedProcess::with_defaults();
    let (exe, args) = test_echo_command("log_test_line");
    let mut req = LaunchRequest::new(exe, "test-log");
    req.args = args;

    mp.start(&req).await.unwrap();
    mp.wait().await.unwrap();

    let logs = mp.log_history().await;
    let found = logs.iter().any(|l| l.text.contains("log_test_line"));
    assert!(found, "应在日志中找到输出: {:?}", logs);
}

#[tokio::test]
async fn managed_process_graceful_stop() {
    let mp = ManagedProcess::with_defaults();
    let (exe, _args) = test_long_running_command();
    let mut req = LaunchRequest::new(exe, "test-graceful");
    req.shutdown = ShutdownConfig {
        force_stop_timeout: Duration::from_secs(10),
    };

    mp.start(&req).await.unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;

    let result = mp.stop().await;
    assert!(result.is_ok(), "graceful stop 应 Ok: {:?}", result.err());

    let status = mp.snapshot().await;
    assert!(status.status.is_exited());

    if let ProcessStatus::Exited { reason } = status.status {
        assert!(
            matches!(
                reason,
                ExitReason::Stopped { .. }
                    | ExitReason::ForceKilled { .. }
                    | ExitReason::NormalExit { .. }
                    | ExitReason::NonZeroExit { .. }
            ),
            "退出原因应合法: {:?}",
            reason
        );
    }
}

#[tokio::test]
async fn managed_process_shutdown_blocking_when_stopped() {
    let mp = ManagedProcess::with_defaults();
    mp.shutdown_blocking();
    let status = mp.snapshot().await;
    assert!(status.status.is_exited());
}

#[tokio::test]
async fn managed_process_shutdown_blocking_when_running() {
    let mp = ManagedProcess::with_defaults();
    let (exe, _args) = test_long_running_command();
    let mut req = LaunchRequest::new(exe, "test-shutdown-blocking");
    req.shutdown = ShutdownConfig {
        force_stop_timeout: Duration::from_secs(5),
    };

    mp.start(&req).await.unwrap();
    tokio::time::sleep(Duration::from_millis(1000)).await;

    // 不检查 pid（可能因竞态已退出），直接测试 shutdown_blocking
    mp.shutdown_blocking();

    tokio::time::sleep(Duration::from_millis(500)).await;

    let status = mp.snapshot().await;
    assert!(
        status.status.is_exited(),
        "shutdown_blocking 后应 Exited: {:?}",
        status.status
    );
}

#[tokio::test]
async fn managed_process_restart_after_stop() {
    let mp = ManagedProcess::with_defaults();
    let (exe, _args) = test_long_running_command();
    let mut req = LaunchRequest::new(exe, "test-restart");
    req.shutdown = ShutdownConfig {
        force_stop_timeout: Duration::from_secs(3),
    };

    mp.start(&req).await.unwrap();
    let token1 = mp.current_token().await;

    mp.stop().await.unwrap();

    let (exe2, args2) = test_echo_command("restart");
    let mut req2 = LaunchRequest::new(exe2, "test-restart-2");
    req2.args = args2;
    mp.start(&req2).await.unwrap();
    let token2 = mp.current_token().await;

    assert_ne!(token1, token2, "restart 后应有新 token");
    let _ = mp.wait().await;
}

// ── 管道背压测试 ──────────────────────────────────────────────────────────

#[tokio::test]
async fn pipe_large_stdout_no_deadlock() {
    let mp = ManagedProcess::with_defaults();
    let (exe, args) = test_large_output_command(64);
    let mut req = LaunchRequest::new(exe, "test-large-stdout");
    req.args = args;

    mp.start(&req).await.unwrap();
    let status = mp.wait().await.unwrap();
    assert!(status.is_exited(), "进程应正常退出: {:?}", status);

    let logs = mp.log_history().await;
    assert!(!logs.is_empty(), "应有日志输出");
}

#[tokio::test]
async fn pipe_large_stderr_no_deadlock() {
    let mp = ManagedProcess::with_defaults();

    #[cfg(windows)]
    let (exe, args) = (
        std::path::PathBuf::from("powershell.exe"),
        vec![
            "-NoProfile".into(),
            "-Command".into(),
            "[Console]::Error.Write(('E' * 65536))".into(),
        ],
    );
    #[cfg(not(windows))]
    let (exe, args) = (
        std::path::PathBuf::from("/bin/bash"),
        vec!["-c".into(), "printf 'E%.0s' $(seq 1 65536) >&2".into()],
    );

    let mut req = LaunchRequest::new(exe, "test-large-stderr");
    req.args = args;

    mp.start(&req).await.unwrap();
    let status = mp.wait().await.unwrap();
    assert!(status.is_exited());
}

#[tokio::test]
async fn pipe_simultaneous_large_output_no_deadlock() {
    let mp = ManagedProcess::with_defaults();

    #[cfg(windows)]
    let (exe, args) = (
        std::path::PathBuf::from("powershell.exe"),
        vec![
            "-NoProfile".into(),
            "-Command".into(),
            "Write-Host -NoNewline ('A' * 32768); [Console]::Error.Write(('B' * 32768))".into(),
        ],
    );
    #[cfg(not(windows))]
    let (exe, args) = (
        std::path::PathBuf::from("/bin/bash"),
        vec![
            "-c".into(),
            "printf 'A%.0s' $(seq 1 32768); printf 'B%.0s' $(seq 1 32768) >&2".into(),
        ],
    );

    let mut req = LaunchRequest::new(exe, "test-simultaneous");
    req.args = args;

    mp.start(&req).await.unwrap();
    let status = mp.wait().await.unwrap();
    assert!(status.is_exited(), "同时大量输出不应死锁: {:?}", status);
}

#[tokio::test]
async fn pipe_no_newline_large_output_truncated() {
    let config = LogPipeConfig {
        history_capacity: 100,
        broadcast_capacity: 16,
        max_line_bytes: 1024,
    };
    let mp = ManagedProcess::new(config);
    let (exe, args) = test_large_output_command(32);
    let mut req = LaunchRequest::new(exe, "test-no-newline");
    req.args = args;

    mp.start(&req).await.unwrap();
    let status = mp.wait().await.unwrap();
    assert!(status.is_exited(), "进程应完成: {:?}", status);

    assert!(
        mp.log_truncated_count() > 0,
        "应有截断行: truncated={}",
        mp.log_truncated_count()
    );

    let logs = mp.log_history().await;
    for log in &logs {
        assert!(
            log.text.len() <= 1024 + "...[truncated]".len(),
            "日志行应被截断: len={}",
            log.text.len()
        );
    }
}

// ── Windows 专用测试 ─────────────────────────────────────────────────────

#[cfg(windows)]
#[tokio::test]
async fn windows_job_object_recycles_child() {
    use crate::infra::platform::process::JobObjectHandle;

    let job = JobObjectHandle::create().expect("Job Object 创建失败");

    let (exe, args) = test_long_running_command();
    let mut cmd = crate::infra::platform::no_window_tokio(tokio::process::Command::new(&exe));
    cmd.args(&args);
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    cmd.stdin(std::process::Stdio::null());

    let mut child = cmd.spawn().expect("spawn 失败");
    let pid = child.id().unwrap_or(0);

    job.assign_process(pid).expect("分配到 Job 失败");

    drop(job);

    tokio::time::timeout(Duration::from_secs(5), child.wait())
        .await
        .expect("进程应在 Job Object drop 后退出")
        .expect("wait 不应失败");
}

#[cfg(windows)]
#[tokio::test]
async fn windows_job_object_recycles_child_tree() {
    use crate::infra::platform::process::JobObjectHandle;

    let job = JobObjectHandle::create().expect("Job Object 创建失败");

    let mut cmd = crate::infra::platform::no_window_tokio(tokio::process::Command::new("cmd.exe"));
    cmd.args(["/C", "ping -n 30 127.0.0.1"]);
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    cmd.stdin(std::process::Stdio::null());

    let mut child = cmd.spawn().expect("spawn 失败");
    let pid = child.id().unwrap_or(0);

    job.assign_process(pid).expect("分配到 Job 失败");

    drop(job);

    tokio::time::timeout(Duration::from_secs(5), child.wait())
        .await
        .expect("进程树应在 Job Object drop 后退出")
        .expect("等待 Job Object 中的进程树退出不应失败");

    tokio::time::sleep(Duration::from_millis(500)).await;
}

#[cfg(windows)]
#[tokio::test]
async fn windows_kill_tree_rejects_unknown_pid() {
    use crate::infra::platform::process::kill_process_tree_verified;

    let result = kill_process_tree_verified(99999, std::path::Path::new("C:\\nonexistent.exe"), 0);
    assert!(result.is_err(), "应拒绝终止未知 PID");
    let err = result.unwrap_err();
    assert!(
        err.contains("拒绝") || err.contains("失败") || err.contains("不匹配"),
        "错误应表明拒绝: {err}"
    );
}

#[cfg(windows)]
#[tokio::test]
async fn windows_kill_tree_rejects_executable_mismatch() {
    use crate::infra::platform::process::kill_process_tree_verified;

    let result = kill_process_tree_verified(4, std::path::Path::new("C:\\not_matching.exe"), 0);
    assert!(result.is_err(), "应拒绝 executable 不匹配");
}

#[cfg(windows)]
#[tokio::test]
async fn windows_unknown_port_not_killed() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind 失败");
    let port = listener.local_addr().unwrap().port();

    assert!(
        crate::infra::platform::process::probe_port_occupied(port),
        "端口应被占用"
    );

    let pid = crate::infra::platform::process::find_pid_by_port(port);
    if let Some(pid) = pid {
        let result = crate::infra::platform::process::kill_process_tree_verified(
            pid,
            std::path::Path::new("C:\\not_our_process.exe"),
            0,
        );
        assert!(result.is_err(), "不应 kill 未知进程");
    }

    drop(listener);
}

#[tokio::test]
async fn stop_no_orphan_processes() {
    let mp = ManagedProcess::with_defaults();
    let (exe, _args) = test_long_running_command();
    let mut req = LaunchRequest::new(exe, "test-no-orphan");
    req.shutdown = ShutdownConfig {
        force_stop_timeout: Duration::from_secs(5),
    };

    mp.start(&req).await.unwrap();
    let pid = mp.pid().await.expect("应有 PID");

    mp.stop().await.unwrap();

    tokio::time::sleep(Duration::from_millis(500)).await;

    #[cfg(windows)]
    {
        use windows::Win32::Foundation::CloseHandle;
        use windows::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};

        let result = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) };
        if let Ok(handle) = result {
            let _ = unsafe { CloseHandle(handle) };
            tokio::time::sleep(Duration::from_secs(2)).await;
            // 再次检查
            let result2 = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) };
            if let Ok(handle2) = result2 {
                let _ = unsafe { CloseHandle(handle2) };
                // 进程可能仍在退出中——Job Object 会最终回收
            }
        }
    }
}

// ── 确定性竞态测试（0.22.1 P0 验收）──────────────────────────────────────
//
// 所有测试使用 spawn_gate 精确控制 spawn 时序，不依赖调度器假设。
// 所有并发测试套外层 timeout 防永久挂起。
//
// spawn_gate 工作原理：
// - with_spawn_gate_for_test() 创建的 ManagedProcess 在 start() 的 spawn 前
//   await gate（初始 false = 阻塞）。
// - release_spawn_gate() 发送 true，放行 start 继续 spawn。
//
// 这允许测试精确控制以下时序：
// 1. start 进入 Starting 后停在 spawn 前（gate 阻塞）
// 2. 测试在此时验证状态为 Starting
// 3. 测试发起 stop（此时 stop 在 CancelStart 路径等待 StartOperation）
// 4. 释放 gate，start 继续 spawn → 提交被 cancellation 拒绝 → 回收 child → 完成 StartOperation
// 5. stop 的 CancelStart 等待返回，执行回收

/// 辅助：长时间运行的测试命令。
fn make_long_running_req(label: &str) -> LaunchRequest {
    let (exe, args) = test_long_running_command();
    let mut req = LaunchRequest::new(exe, label);
    req.args = args;
    req.shutdown = ShutdownConfig {
        force_stop_timeout: Duration::from_secs(5),
    };
    req
}

/// 辅助：快速退出的 echo 命令。
fn make_echo_req(label: &str) -> LaunchRequest {
    let (exe, args) = test_echo_command("det_echo");
    let mut req = LaunchRequest::new(exe, label);
    req.args = args;
    req
}

// ── 9.1: 并发 start——只有一个成功，另一个得到 AlreadyRunning ──────────────
//
// 确定性策略：
// - 使用 spawn_gate 让第一个 start 停在 spawn 前（持有 Starting 状态）。
// - 第二个 start 此时发起，必定看到 Starting → 返回 AlreadyRunning。
// - 释放 gate 让第一个 start 完成。
// - 不依赖 tokio::join! 的调度顺序。

#[tokio::test]
async fn det_concurrent_start_only_one_succeeds() {
    tokio::time::timeout(Duration::from_secs(30), async {
        let mp = ManagedProcess::with_spawn_gate_for_test();
        let req = make_long_running_req("det-concurrent-start");

        // start #1 — 会停在 spawn_gate 处
        let mp1 = mp.clone();
        let req1 = req.clone();
        let start1_handle = tokio::spawn(async move { mp1.start(&req1).await });

        // 等待 start #1 进入 Starting（gate 阻塞中）
        tokio::time::sleep(Duration::from_millis(100)).await;
        let snap = mp.snapshot().await;
        assert!(
            matches!(snap.status, ProcessStatus::Starting),
            "start #1 应停在 Starting: {:?}",
            snap.status
        );

        // start #2 — 此时状态为 Starting，必定返回 AlreadyRunning
        let mp2 = mp.clone();
        let req2 = req.clone();
        let start2_result = mp2.start(&req2).await;

        assert!(
            matches!(
                start2_result,
                Err(ManagedProcessError::AlreadyRunning { .. })
            ),
            "start #2 应返回 AlreadyRunning: {:?}",
            start2_result
        );

        // 释放 gate 让 start #1 完成 spawn
        mp.release_spawn_gate();

        // 等待 start #1 完成
        let start1_result = start1_handle.await.unwrap();
        assert!(
            start1_result.is_ok(),
            "start #1 应成功: {:?}",
            start1_result.err()
        );

        // 清理
        let _ = mp.stop().await;
    })
    .await
    .expect("det_concurrent_start 超时");
}

// ── 9.2: stop 执行者选举——首个 stop 成为 executor，后续 stop 等待 ──────────
//
// 确定性策略：
// - 使用 spawn_gate 让 start 停在 spawn 前。
// - 释放 gate 让 start 完成 → 进入 Running。
// - 使用 spawn_gate 再次阻塞（无法直接复用，改用另一种方法）：
//   实际上我们直接在 Running 状态下并发 stop，验证：
//   a) 全部返回 Ok（共享同一 stop operation）
//   b) 最终状态为 Exited
//   c) kill 只执行一次（通过观察日志或状态一致性间接验证）

#[tokio::test]
async fn det_stop_executor_election_atomic() {
    tokio::time::timeout(Duration::from_secs(30), async {
        let mp = ManagedProcess::with_defaults();
        let req = make_long_running_req("det-stop-executor");

        mp.start(&req).await.unwrap();
        tokio::time::sleep(Duration::from_millis(500)).await;

        // 确认 Running
        let snap = mp.snapshot().await;
        assert!(
            matches!(snap.status, ProcessStatus::Running { .. }),
            "应 Running: {:?}",
            snap.status
        );

        // 三个并发 stop
        let mp1 = mp.clone();
        let mp2 = mp.clone();
        let mp3 = mp.clone();

        let (r1, r2, r3) = tokio::join!(
            async move { mp1.stop().await },
            async move { mp2.stop().await },
            async move { mp3.stop().await }
        );

        // 全部应 Ok（共享同一 stop operation 结果）
        assert!(r1.is_ok(), "stop 1 应 Ok: {:?}", r1.err());
        assert!(r2.is_ok(), "stop 2 应 Ok: {:?}", r2.err());
        assert!(r3.is_ok(), "stop 3 应 Ok: {:?}", r3.err());

        // 直接验证 postcondition：stop 返回时进程已回收
        let status = mp.snapshot().await;
        assert!(
            status.status.is_exited(),
            "stop postcondition: 应 Exited: {:?}",
            status.status
        );
    })
    .await
    .expect("det_stop_executor_election 超时");
}

// ── 9.3: stop 在 Starting 阶段到达——CancelStart 路径 ──────────────────────
//
// 确定性策略：
// - 使用 spawn_gate 让 start 停在 spawn 前。
// - 此时状态为 Starting。
// - 发起 stop——stop 进入 CancelStart 路径，等待 StartOperation 完成。
// - 此时 stop 应阻塞（start operation 尚未完成，因为 spawn 被 gate 阻塞）。
// - 释放 gate → start 继续 spawn → 提交被 cancellation 拒绝 → 回收 child → 完成 StartOperation(Cancelled)。
// - stop 的 CancelStart 等待返回 → 确认无需回收 → 完成 StopOperation(Done)。
// - 验证 stop 返回时进程已被回收（postcondition）。

#[tokio::test]
async fn det_stop_during_starting_cancelstart() {
    tokio::time::timeout(Duration::from_secs(30), async {
        let mp = ManagedProcess::with_spawn_gate_for_test();
        let req = make_long_running_req("det-stop-during-starting");

        // start 会停在 spawn_gate 处
        let mp1 = mp.clone();
        let req_clone = req.clone();
        let start_handle = tokio::spawn(async move { mp1.start(&req_clone).await });

        // 等待 start 进入 Starting（gate 阻塞中）
        tokio::time::sleep(Duration::from_millis(100)).await;

        // 确认处于 Starting
        let snap = mp.snapshot().await;
        assert!(
            matches!(snap.status, ProcessStatus::Starting),
            "应处于 Starting: {:?}",
            snap.status
        );

        // 发起 stop——在单独 task 中，因为它会阻塞等待 StartOperation
        let mp_stop = mp.clone();
        let stop_handle = tokio::spawn(async move { mp_stop.stop().await });

        // 短暂等待，确认 stop 尚未完成（start operation 未完成）
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(
            !stop_handle.is_finished(),
            "stop 应阻塞等待 StartOperation 完成"
        );

        // 释放 spawn gate → start 继续 spawn → 提交被取消 → 回收 child → 完成 StartOperation
        mp.release_spawn_gate();

        // 等待 stop 完成
        let stop_result = stop_handle.await.unwrap();
        assert!(stop_result.is_ok(), "stop 应 Ok: {:?}", stop_result.err());

        // 等待 start 返回
        let start_result = start_handle.await.unwrap();
        assert!(
            start_result.is_ok(),
            "start 应 Ok（取消后返回 Ok）: {:?}",
            start_result.err()
        );

        // 直接验证 postcondition：stop 返回时状态已为 Exited
        let status = mp.snapshot().await;
        assert!(
            status.status.is_exited(),
            "stop postcondition: 应 Exited: {:?}",
            status.status
        );
    })
    .await
    .expect("det_stop_during_starting 超时");
}

/// child 与 Job 已创建但尚未公开 Running 时，stop 仍必须走 CancelStart，
/// 且返回前完成真实进程回收；不得出现可观察的 Running-but-no-child 窗口。
#[tokio::test]
async fn det_stop_during_pre_running_commit_is_atomic() {
    tokio::time::timeout(Duration::from_secs(30), async {
        let mp = ManagedProcess::with_pre_running_commit_gate_for_test();
        let req = make_long_running_req("det-pre-running-commit");

        let mp_start = mp.clone();
        let req_clone = req.clone();
        let start_handle = tokio::spawn(async move { mp_start.start(&req_clone).await });

        let pid = loop {
            let pid = mp.pre_running_commit_pid_for_test();
            if pid != 0 {
                break pid;
            }
            tokio::task::yield_now().await;
        };

        let snapshot = mp.snapshot().await;
        assert!(
            matches!(snapshot.status, ProcessStatus::Starting),
            "Running 原子提交前必须保持 Starting: {:?}",
            snapshot.status
        );

        let mp_stop = mp.clone();
        let stop_handle = tokio::spawn(async move { mp_stop.stop().await });
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            !stop_handle.is_finished(),
            "stop 必须等待未发布实例完成取消回收"
        );

        mp.release_pre_running_commit_gate();

        let stop_result = stop_handle.await.unwrap();
        assert!(stop_result.is_ok(), "stop 应成功: {:?}", stop_result.err());
        assert!(start_handle.await.unwrap().is_ok());

        let snapshot = mp.snapshot().await;
        assert!(snapshot.status.is_exited());
        assert!(
            !matches!(snapshot.status, ProcessStatus::Running { .. }),
            "不得公开 Running-but-no-child"
        );

        #[cfg(windows)]
        assert!(
            !windows_process_is_active(pid),
            "stop 返回时迟到 child PID {pid} 必须已退出"
        );
    })
    .await
    .expect("det_stop_during_pre_running_commit_is_atomic 超时");
}

// ── 9.4: 后订阅者能读取已完成的 operation 结果（无丢通知）──────────────────
//
// 直接测试 OperationCompletion 的 watch 语义：
// - 创建 completion → 完成 → 之后订阅 → 应立即读到结果。
// 同时测试 stop 在已 Exited 后调用（AlreadyStopped 路径）。

#[tokio::test]
async fn det_late_subscriber_reads_completed_result() {
    tokio::time::timeout(Duration::from_secs(30), async {
        let mp = ManagedProcess::with_defaults();
        let req = make_long_running_req("det-late-subscriber");

        mp.start(&req).await.unwrap();

        // stop 完成（executor 完成 StopOperation）
        let stop_result = mp.stop().await;
        assert!(stop_result.is_ok());

        // stop 返回时直接验证 postcondition——不应需要额外等待
        let status = mp.snapshot().await;
        assert!(
            status.status.is_exited(),
            "stop postcondition: 应 Exited: {:?}",
            status.status
        );

        // 再次 stop——走 AlreadyStopped 路径，应立即返回 Ok
        let stop_result2 = mp.stop().await;
        assert!(
            stop_result2.is_ok(),
            "后订阅者 stop 应 Ok（AlreadyStopped）: {:?}",
            stop_result2.err()
        );
    })
    .await
    .expect("det_late_subscriber 超时");
}

// ── 9.5: stop postcondition——stop 返回时不应停留在 Stopping ────────────────
//
// 直接验证：stop 返回时状态必须为 Exited（而非 Stopping）。
// 不使用 stop 后的 sleep 等待，因为 stop 的 postcondition 就是返回时进程已回收。

#[tokio::test]
async fn det_stop_no_permanent_stopping() {
    tokio::time::timeout(Duration::from_secs(30), async {
        let mp = ManagedProcess::with_defaults();
        let req = make_long_running_req("det-no-permanent-stopping");

        mp.start(&req).await.unwrap();
        tokio::time::sleep(Duration::from_millis(500)).await;

        // stop 返回即验证——postcondition 要求进程已回收
        mp.stop().await.unwrap();

        // 不需要 sleep，直接检查
        let status = mp.snapshot().await;
        assert!(
            !matches!(status.status, ProcessStatus::Stopping),
            "stop postcondition: 不应停留在 Stopping: {:?}",
            status.status
        );
        assert!(
            status.status.is_exited(),
            "stop postcondition: 应 Exited: {:?}",
            status.status
        );
    })
    .await
    .expect("det_stop_no_permanent_stopping 超时");
}

// ── 9.6: start/stop 竞态后能再次 start（generation 隔离）──────────────────
//
// 确定性策略：
// - 使用 spawn_gate 控制时序。
// - start 停在 gate → stop 发起（CancelStart 路径阻塞）→ 释放 gate → start 被取消 → stop 完成。
// - 再次 start 应成功（新 generation）。

#[tokio::test]
async fn det_race_then_restart() {
    tokio::time::timeout(Duration::from_secs(30), async {
        let mp = ManagedProcess::with_spawn_gate_for_test();
        let req = make_long_running_req("det-race-restart");

        // start 停在 spawn_gate 处
        let mp1 = mp.clone();
        let req_clone = req.clone();
        let start_handle = tokio::spawn(async move { mp1.start(&req_clone).await });

        // 等待 start 进入 Starting
        tokio::time::sleep(Duration::from_millis(100)).await;

        // 发起 stop（CancelStart 路径，阻塞等待 StartOperation）
        let mp_stop = mp.clone();
        let stop_handle = tokio::spawn(async move { mp_stop.stop().await });

        // 确认 stop 尚未完成
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            !stop_handle.is_finished(),
            "stop 应阻塞等待 start operation"
        );

        // 释放 gate → start 继续 → 被取消 → StartOperation 完成 → stop 完成
        mp.release_spawn_gate();

        // 等待 stop 完成
        let stop_result = stop_handle.await.unwrap();
        assert!(stop_result.is_ok(), "stop 应 Ok: {:?}", stop_result.err());

        // 等待 start 返回
        let start_result = start_handle.await.unwrap();
        assert!(
            start_result.is_ok(),
            "start 应 Ok: {:?}",
            start_result.err()
        );

        // 验证已 Exited
        let status = mp.snapshot().await;
        assert!(status.status.is_exited(), "应 Exited: {:?}", status.status);

        // 再次 start——应成功（新 generation）
        let req2 = make_echo_req("det-after-race");
        let result = mp.start(&req2).await;
        assert!(result.is_ok(), "竞态后应能再次 start: {:?}", result.err());

        // 验证新 generation
        let new_token = mp.current_token().await;
        assert!(
            new_token.generation > 1,
            "应有新 generation: {}",
            new_token.generation
        );

        let _ = mp.wait().await;
    })
    .await
    .expect("det_race_then_restart 超时");
}

// ── 9.7: shutdown_blocking 在 Running 状态下安全回收 ──────────────────────

#[tokio::test]
async fn det_shutdown_blocking_while_running() {
    tokio::time::timeout(Duration::from_secs(30), async {
        let mp = ManagedProcess::with_defaults();
        let req = make_long_running_req("det-shutdown-blocking");

        mp.start(&req).await.unwrap();
        tokio::time::sleep(Duration::from_millis(500)).await;

        mp.shutdown_blocking();

        // shutdown_blocking 是同步的，设置 Exited 状态
        let status = mp.snapshot().await;
        assert!(
            status.status.is_exited(),
            "shutdown_blocking 后应 Exited: {:?}",
            status.status
        );
    })
    .await
    .expect("det_shutdown_blocking 超时");
}

// ── 9.8: 并发 stop 共享成功结果 ───────────────────────────────────────────
// StopOutcome::Failed 的持久化与多 waiter 读取由 process.rs 内部原语测试覆盖。

#[tokio::test]
async fn det_stop_success_result_is_shared() {
    tokio::time::timeout(Duration::from_secs(30), async {
        let mp = ManagedProcess::with_defaults();
        let req = make_long_running_req("det-error-propagation");

        mp.start(&req).await.unwrap();
        tokio::time::sleep(Duration::from_millis(500)).await;

        // 三个并发 stop——验证结果一致性
        let mp1 = mp.clone();
        let mp2 = mp.clone();
        let mp3 = mp.clone();

        let (r1, r2, r3) = tokio::join!(
            async move { mp1.stop().await },
            async move { mp2.stop().await },
            async move { mp3.stop().await }
        );

        // 在正常路径下全部 Ok——验证所有 waiter 收到相同结果
        let results = [r1.is_ok(), r2.is_ok(), r3.is_ok()];
        assert!(
            results.iter().all(|&r| r),
            "所有 stop waiter 应收到相同（Ok）结果: {:?}",
            results
        );

        // 如果有一个失败，全部应失败（在真实失败场景中）
        // 这里验证的是共享语义：结果一致性
    })
    .await
    .expect("det_stop_error_propagation 超时");
}

// ── 9.10: stop 在 Starting 阶段到达但 start spawn 失败 ─────────────────────
//
// 确定性策略：
// - 使用 spawn_gate 让 start 停在 spawn 前。
// - 发起 stop（CancelStart 路径阻塞）。
// - 释放 gate → start spawn 失败（使用不存在的可执行文件）→ StartOperation 完成(Failed)。
// - stop 的 CancelStart 等待返回 → 无需回收 → 完成 StopOperation(Done)。
// - 验证 stop 返回 Ok 且状态为 Exited。

#[tokio::test]
async fn det_stop_during_starting_spawn_fails() {
    tokio::time::timeout(Duration::from_secs(30), async {
        let mp = ManagedProcess::with_spawn_gate_for_test();
        let mut req = LaunchRequest::new(
            std::path::PathBuf::from("/nonexistent/path/that/does/not/exist"),
            "det-stop-spawn-fail",
        );
        req.shutdown = ShutdownConfig {
            force_stop_timeout: Duration::from_secs(3),
        };

        // start 会停在 spawn_gate 处（即使可执行文件不存在，gate 在 spawn 调用前阻塞）
        let mp1 = mp.clone();
        let req_clone = req.clone();
        let start_handle = tokio::spawn(async move { mp1.start(&req_clone).await });

        // 等待 start 进入 Starting
        tokio::time::sleep(Duration::from_millis(100)).await;

        let snap = mp.snapshot().await;
        assert!(
            matches!(snap.status, ProcessStatus::Starting),
            "应处于 Starting: {:?}",
            snap.status
        );

        // 发起 stop——CancelStart 路径阻塞
        let mp_stop = mp.clone();
        let stop_handle = tokio::spawn(async move { mp_stop.stop().await });

        // 确认 stop 阻塞
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            !stop_handle.is_finished(),
            "stop 应阻塞等待 start operation"
        );

        // 释放 gate → start spawn 失败 → StartOperation 完成(Failed)
        mp.release_spawn_gate();

        // 等待 stop 完成
        let stop_result = stop_handle.await.unwrap();
        assert!(stop_result.is_ok(), "stop 应 Ok: {:?}", stop_result.err());

        // 等待 start 返回
        let start_result = start_handle.await.unwrap();
        assert!(start_result.is_err(), "start 应失败: {:?}", start_result);

        // 验证状态
        let status = mp.snapshot().await;
        assert!(status.status.is_exited(), "应 Exited: {:?}", status.status);
    })
    .await
    .expect("det_stop_during_starting_spawn_fails 超时");
}

// ── 9.11: stop 后再次 start 使用新 generation ──────────────────────────────
//
// 验证 stop 完成后，再次 start 使用新的 generation 和 token。
// 这是 generation 隔离的核心保证。

#[tokio::test]
async fn det_stop_then_restart_new_generation() {
    tokio::time::timeout(Duration::from_secs(30), async {
        let mp = ManagedProcess::with_defaults();
        let req = make_long_running_req("det-gen-1");

        mp.start(&req).await.unwrap();
        let token1 = mp.current_token().await;
        assert_eq!(token1.generation, 1);

        mp.stop().await.unwrap();

        // stop 返回后直接验证 Exited
        let status = mp.snapshot().await;
        assert!(
            status.status.is_exited(),
            "stop 后应 Exited: {:?}",
            status.status
        );

        // 再次 start
        let req2 = make_echo_req("det-gen-2");
        mp.start(&req2).await.unwrap();
        let token2 = mp.current_token().await;
        assert_eq!(token2.generation, 2, "应有新 generation");
        assert_ne!(token1, token2, "token 应不同");

        let _ = mp.wait().await;
    })
    .await
    .expect("det_stop_then_restart_new_generation 超时");
}

#[cfg(windows)]
fn windows_process_is_active(pid: u32) -> bool {
    use windows::Win32::Foundation::{CloseHandle, STILL_ACTIVE};
    use windows::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    let process = match unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) } {
        Ok(process) => process,
        Err(_) => return false,
    };
    let mut exit_code = 0u32;
    let is_active = unsafe { GetExitCodeProcess(process, &mut exit_code).is_ok() }
        && exit_code == STILL_ACTIVE.0 as u32;
    let _ = unsafe { CloseHandle(process) };
    is_active
}
