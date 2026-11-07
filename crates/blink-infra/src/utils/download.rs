//! 共享下载内核（0.28.27）：流式下载检查、错误分类与脱敏日志。
//!
//! 0.28.27 下载链路治理的收敛层——此前各下载入口（托管运行时/解释器/模型/插件包/
//! 更新安装包）各自维护候选循环、超时与错误处理，行为不一致（部分入口只有请求头
//! 等待有界、响应体可能无限等待；FunASR 请求阶段失败直接返回绕过镜像）。本模块提供：
//!
//! - [`stream_to_file`]：单候选流式落盘——请求头/响应等待、响应体无进度停滞、
//!   字节上限、取消与哈希消费统一在一条通路里；
//! - [`classify_attempt_error`]：错误分类（本地终态 vs 可换源）；
//! - [`sanitize_url_for_log`] / [`format_speed`]：脱敏日志与速度格式化。
//!
//! 候选规划（镜像优先偏好 + 身份）见 [`super::mirrors`]。**不替代调用方职责**：
//! 候选循环、安装/修复的原子提交、版本租约、锁定摘要比对（官方锁定源摘要异常显式
//! 失败、镜像异常可继续下一候选）仍归调用方。失败路径统一删除本次暂存文件，
//! 不把另一源未验证的字节留给下一候选拼接。

use std::path::Path;
use std::time::{Duration, Instant};

use futures::StreamExt;
use tokio::io::AsyncWriteExt;
use tokio_util::sync::CancellationToken;

/// 更新安装包字节上限（0.28.27：共享内核传输侧兜底；签名验签在 install 内独立执行）。
pub const UPDATE_PACKAGE_MAX_BYTES: u64 = 256 * 1024 * 1024;

/// 单候选流式下载限额。
#[derive(Debug, Clone)]
pub struct StreamLimits {
    /// 单候选请求总超时（连接 + 响应头等待）。
    pub request_timeout: Duration,
    /// 响应体无新数据的停滞判定——超时即中断本候选（可换源）。
    pub stall_timeout: Duration,
    /// 字节上限；超限中断（官方锁定源尺寸锁定在此兜底）。
    pub max_bytes: Option<u64>,
}

/// 单候选尝试结果。
#[derive(Debug, Clone, Copy)]
pub struct AttemptOutcome {
    pub bytes: u64,
    pub elapsed: Duration,
}

/// 单候选尝试错误。
#[derive(Debug, Clone)]
pub enum AttemptError {
    /// 用户取消——立即终止整个任务，不换源。
    Cancelled,
    /// 本地磁盘/权限——直接报告，不换源。
    Local { reason: String },
    /// 网络/HTTP/流/停滞/超限——可尝试下一候选。
    Retryable { reason: String },
}

impl std::fmt::Display for AttemptError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cancelled => write!(f, "已取消"),
            Self::Local { reason } => write!(f, "{reason}"),
            Self::Retryable { reason } => write!(f, "{reason}"),
        }
    }
}

/// 本地 IO 错误分类：磁盘/权限错误直接报告（换源救不了本地），其余本地错误
/// 同样归 Local——换源只解决网络侧问题。
pub fn classify_io_error(err: &std::io::Error) -> AttemptError {
    let reason = format!("本地写入失败: {err}");
    AttemptError::Local { reason }
}

/// URL 脱敏：去掉 query/fragment（签名令牌、代理凭据常见位置），路径保留，
/// 便于排查资源名且不暴露凭据。
pub fn sanitize_url_for_log(url: &str) -> String {
    let mut out = url.to_string();
    if let Some(idx) = out.find(['?', '#']) {
        out.truncate(idx);
        out.push_str("?…");
    }
    out
}

/// 速度格式化（诊断日志用，KiB/s / MiB/s 一位小数）。
pub fn format_speed(bytes: u64, elapsed: Duration) -> String {
    let secs = elapsed.as_secs_f64().max(0.001);
    let kbps = bytes as f64 / 1024.0 / secs;
    if kbps >= 1024.0 {
        format!("{:.1} MiB/s", kbps / 1024.0)
    } else {
        format!("{:.1} KiB/s", kbps)
    }
}

/// 单候选流式落盘。
///
/// - 请求头等待受 [`StreamLimits::request_timeout`] 约束；响应体每 chunk 受
///   [`StreamLimits::stall_timeout`] 约束（补齐"只有请求头有界、响应体无限等待"缺口）；
/// - `on_progress(written, total)` 逐 chunk 回调（total 可缺省，由调用方节流）；
/// - `hash_update` 逐 chunk 消费字节（调用方自持 hasher，本模块不比对不截断）；
/// - 响应状态非 2xx → [`AttemptError::Retryable`]；磁盘错误 → [`AttemptError::Local`]；
///   取消 → [`AttemptError::Cancelled`]；
/// - 任何失败路径都删除本次暂存文件。
#[allow(clippy::too_many_arguments)]
pub async fn stream_to_file<P, H>(
    client: &reqwest::Client,
    url: &str,
    dest: &Path,
    limits: &StreamLimits,
    cancel: Option<&CancellationToken>,
    mut on_progress: P,
    mut hash_update: H,
) -> Result<AttemptOutcome, AttemptError>
where
    P: FnMut(u64, Option<u64>) + Send,
    H: for<'a> FnMut(&'a [u8]) + Send,
{
    let started = Instant::now();

    // ── 请求（send + 重定向跟随）：请求级超时 + 取消 ──
    let request_future = client.get(url).send();
    let send = tokio::time::timeout(limits.request_timeout, request_future);
    let response = if let Some(token) = cancel {
        tokio::select! {
            biased;
            _ = token.cancelled() => {
                tracing::info!(url = %sanitize_url_for_log(url), "下载已被取消（请求阶段）");
                return Err(AttemptError::Cancelled);
            }
            r = send => await_send_result(r, limits)?,
        }
    } else {
        await_send_result(send.await, limits)?
    };

    let status = response.status();
    if !status.is_success() {
        return Err(AttemptError::Retryable {
            reason: format!("HTTP {status}"),
        });
    }
    let total: Option<u64> = response.content_length().map(|v| v as u64);
    on_progress(0, total);

    let mut file = match tokio::fs::File::create(dest).await {
        Ok(f) => f,
        Err(e) => return Err(classify_io_error(&e)),
    };

    // ── 响应体：逐 chunk 消费，停滞判定 + 取消 + 上限 + 速度诊断日志 ──
    // 速度诊断 5s 节流（窗口速度），取消/失败记录已下载量与耗时——
    // 手动取消慢速源时日志能回答"下到哪、多快、哪个源"。
    let mut stream = response.bytes_stream();
    let mut written: u64 = 0;
    const SPEED_LOG_INTERVAL: Duration = Duration::from_secs(5);
    let mut last_speed_log = Instant::now();
    let mut last_speed_log_bytes: u64 = 0;
    loop {
        let next = tokio::time::timeout(limits.stall_timeout, stream.next());
        let item = if let Some(token) = cancel {
            tokio::select! {
                biased;
                _ = token.cancelled() => {
                    let _ = tokio::fs::remove_file(dest).await;
                    tracing::info!(
                        url = %sanitize_url_for_log(url),
                        downloaded = written,
                        elapsed = ?started.elapsed(),
                        "下载已被取消"
                    );
                    return Err(AttemptError::Cancelled);
                }
                item = next => item,
            }
        } else {
            next.await
        };
        let chunk = match item {
            Ok(Some(Ok(chunk))) => chunk,
            Ok(Some(Err(e))) => {
                let _ = tokio::fs::remove_file(dest).await;
                tracing::debug!(
                    url = %sanitize_url_for_log(url),
                    downloaded = written,
                    elapsed = ?started.elapsed(),
                    "下载响应体中断"
                );
                return Err(AttemptError::Retryable {
                    reason: format!("响应体中断: {e}"),
                });
            }
            Ok(None) => break,  // 流自然结束
            Err(_) => {
                let _ = tokio::fs::remove_file(dest).await;
                tracing::debug!(
                    url = %sanitize_url_for_log(url),
                    downloaded = written,
                    elapsed = ?started.elapsed(),
                    "下载停滞（响应体无进度）"
                );
                return Err(AttemptError::Retryable {
                    reason: format!("响应体 {}s 无进度（停滞）", limits.stall_timeout.as_secs()),
                });
            }
        };
        if chunk.is_empty() {
            continue;
        }
        written = written.saturating_add(chunk.len() as u64);
        if last_speed_log.elapsed() >= SPEED_LOG_INTERVAL {
            let window = written.saturating_sub(last_speed_log_bytes);
            tracing::debug!(
                url = %sanitize_url_for_log(url),
                downloaded = written,
                total = ?total,
                speed = %format_speed(window, last_speed_log.elapsed()),
                "下载进行中"
            );
            last_speed_log = Instant::now();
            last_speed_log_bytes = written;
        }
        if let Some(max) = limits.max_bytes
            && written > max
        {
            let _ = tokio::fs::remove_file(dest).await;
            tracing::debug!(
                url = %sanitize_url_for_log(url),
                downloaded = written,
                elapsed = ?started.elapsed(),
                "下载超过字节上限"
            );
            return Err(AttemptError::Retryable {
                reason: format!("超过字节上限（{max}）"),
            });
        }
        hash_update(&chunk);
        if let Err(e) = file.write_all(&chunk).await {
            let _ = tokio::fs::remove_file(dest).await;
            return Err(classify_io_error(&e));
        }
        on_progress(written, total);
    }
    if let Err(e) = file.flush().await {
        let _ = tokio::fs::remove_file(dest).await;
        return Err(classify_io_error(&e));
    }
    Ok(AttemptOutcome {
        bytes: written,
        elapsed: started.elapsed(),
    })
}

/// send 结果归一：请求错误/超时 → 可换源。
fn await_send_result(
    result: Result<reqwest::Result<reqwest::Response>, tokio::time::error::Elapsed>,
    limits: &StreamLimits,
) -> Result<reqwest::Response, AttemptError> {
    match result {
        Ok(Ok(resp)) => Ok(resp),
        Ok(Err(e)) => Err(AttemptError::Retryable {
            reason: format!("请求失败: {e}"),
        }),
        Err(_) => Err(AttemptError::Retryable {
            reason: format!("请求超时（{}s）", limits.request_timeout.as_secs()),
        }),
    }
}

#[cfg(test)]
mod behavior_tests;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_url_strips_query_and_fragment() {
        assert_eq!(
            sanitize_url_for_log("https://mirror.example/file.zip?token=abc&sig=x"),
            "https://mirror.example/file.zip?…"
        );
        assert_eq!(
            sanitize_url_for_log("https://mirror.example/file.zip#frag"),
            "https://mirror.example/file.zip?…"
        );
        assert_eq!(
            sanitize_url_for_log("https://mirror.example/file.zip"),
            "https://mirror.example/file.zip"
        );
    }

    #[test]
    fn format_speed_units() {
        // 1024 KiB in 1s = 1.0 MiB/s
        assert_eq!(format_speed(1024 * 1024, Duration::from_secs(1)), "1.0 MiB/s");
        assert_eq!(format_speed(512 * 1024, Duration::from_secs(1)), "512.0 KiB/s");
    }

    #[test]
    fn io_errors_are_local_not_retryable() {
        let e = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied");
        assert!(matches!(classify_io_error(&e), AttemptError::Local { .. }));
        let e = std::io::Error::new(std::io::ErrorKind::Other, "boom");
        assert!(matches!(classify_io_error(&e), AttemptError::Local { .. }));
    }
}
