//! Lease 恢复胶水层：连接 OS 查询和 lease 决策（0.22.6.6）。
//!
//! 提供两个辅助函数：
//! - `build_process_evidence`：在 `spawn_blocking` 中查询 OS 进程身份。
//! - `probe_health_evidence`：异步 HTTP 探测 health 端点，提取身份回显。
//!
//! 这两个函数从调用方（`main.rs` 启动路径）传入 `decide_recovery` 纯函数，
//! 实现 fail-closed 恢复策略。
//!
//! ## 分层归属
//!
//! - `infra/local_engine`：不依赖 `app` 或 `domain`。
//! - 阻塞 OS 查询（`pid_exists` / `get_process_executable` / `get_process_creation_time_ms`）
//!   由调用方在 `spawn_blocking` 中调用 `build_process_evidence`。

use crate::infra::local_engine::lease::{HealthEvidence, ProcessEvidence};

/// 从 OS 查询进程身份，构造 `ProcessEvidence`。
///
/// **必须在 `spawn_blocking` 中调用**——内部调用 `OpenProcess`、
/// `QueryFullProcessImageNameW`、`GetProcessTimes` 均为阻塞 syscall。
///
/// 查询失败时返回 `pid_exists=false` + 其余字段 `None`，
/// 这使得 `decide_recovery` 走 `DoNotAdopt` 路径（fail-closed）。
pub fn build_process_evidence(pid: u32) -> ProcessEvidence {
    use crate::infra::platform::process::{
        get_process_creation_time_ms, get_process_executable, pid_exists,
    };

    let pid_exists = pid_exists(pid);
    if !pid_exists {
        return ProcessEvidence {
            pid_exists: false,
            actual_executable: None,
            actual_creation_time_ms: None,
        };
    }

    // PID 存在——查询可执行路径和创建时间
    let actual_executable = get_process_executable(pid).map(|p| p.to_string_lossy().to_string());
    let actual_creation_time_ms = get_process_creation_time_ms(pid);

    ProcessEvidence {
        pid_exists: true,
        actual_executable,
        actual_creation_time_ms,
    }
}

/// 异步探测 health 端点，提取身份回显构造 `HealthEvidence`。
///
/// 对 `endpoint`（如 `127.0.0.1:8100`）发起 `GET /health` 请求，
/// 超时 3 秒。解析 JSON 响应中的 `engine_id`、`instance_id`、
/// `token_fingerprint` 字段。
///
/// - health 不可达或响应超时 → 返回 `None`（fail-closed）。
/// - health 可达但字段缺失 → 返回 `Some(HealthEvidence { ...: None })`，
///   `decide_recovery` 会据此返回 `DoNotAdopt`。
pub async fn probe_health_evidence(endpoint: &str) -> Option<HealthEvidence> {
    let health_url = if endpoint.starts_with("http") {
        format!("{endpoint}/health")
    } else {
        format!("http://{endpoint}/health")
    };

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(3))
        .build()
        .ok()?;

    let resp = client.get(&health_url).send().await.ok()?;
    if !resp.status().is_success() {
        tracing::debug!(
            url = %health_url,
            status = %resp.status(),
            "lease 恢复: health 端点返回非 2xx"
        );
        return None;
    }

    let raw: serde_json::Value = resp.json().await.ok()?;

    Some(HealthEvidence {
        engine_id: raw
            .get("engine_id")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        instance_id: raw
            .get("instance_id")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        token_fingerprint: raw
            .get("token_fingerprint")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_process_evidence_nonexistent_pid() {
        // PID 99999 在大多数系统上不存在
        let evidence = build_process_evidence(99999);
        assert!(!evidence.pid_exists);
    }

    #[test]
    fn test_build_process_evidence_pid_zero() {
        // PID 0（System Idle Process）——Windows 上无法 OpenProcess
        let evidence = build_process_evidence(0);
        // PID 0 在 Windows 上 pid_exists 应返回 false
        assert!(!evidence.pid_exists);
    }

    #[tokio::test]
    async fn test_probe_health_evidence_unreachable() {
        // 使用一个几乎确定未被占用的端口
        let evidence = probe_health_evidence("127.0.0.1:59999").await;
        assert!(evidence.is_none(), "不可达的 health 端点应返回 None");
    }
}
