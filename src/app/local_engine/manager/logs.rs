//! EngineManager 日志用例：
//! 日志查询（原始 / 结构化）、运行日志投影 pump 与子进程输出级别分类。

use super::*;

impl EngineManager {
    // ── logs / history ──────────────────────────────────────────────────────

    /// 查询引擎日志（provider-neutral 入口）。
    #[allow(dead_code)] // 旧接口：生产用 get_logs_structured，此方法保留为简化查询入口
    pub async fn get_logs(
        &self,
        engine_id: &EngineId,
        max_lines: usize,
    ) -> Result<Vec<String>, LocalEngineError> {
        self.validate_engine_id(engine_id)?;
        let entry = self.get_entry(engine_id).await?;
        let mp = entry.managed_process.lock().await;
        match mp.as_ref() {
            Some(managed) => {
                let history = managed.log_history().await;
                let lines: Vec<String> = history
                    .into_iter()
                    .rev()
                    .take(max_lines)
                    .map(|entry| entry.text)
                    .collect();
                Ok(lines)
            }
            None => Ok(Vec::new()),
        }
    }

    // ── 结构化日志（0.22.5 H1）──────────────────────────────────────────────

    /// 查询引擎结构化日志（含 instance_id + seq）。
    ///
    /// 返回 `Vec<StructuredLogEntry>`，每条包含 `engine_id`、`instance_id`、
    /// `seq`、`timestamp_ms`、`level`、`text`。
    ///
    /// 历史与 `LOCAL_ENGINE_LOG` 实时事件使用同一 shape。
    /// 如果引擎未运行但有 `last_managed_process`，从上一实例读取 bounded history。
    pub async fn get_logs_structured(
        &self,
        engine_id: &EngineId,
        max_lines: usize,
    ) -> Result<Vec<StructuredLogEntry>, LocalEngineError> {
        self.validate_engine_id(engine_id)?;
        let entry = self.get_entry(engine_id).await?;

        // 获取当前实例的 instance_id
        let instance_id = entry
            .current_identity()
            .await
            .map(|i| i.instance_id.clone());

        // 优先从当前运行实例读取
        let mp = entry.managed_process.lock().await;
        if let Some(managed) = mp.as_ref() {
            let history = managed.log_history().await;
            let mut logs: Vec<StructuredLogEntry> = history
                .into_iter()
                .rev()
                .take(max_lines)
                .map(|entry| StructuredLogEntry {
                    engine_id: engine_id.to_string(),
                    instance_id: instance_id.clone().unwrap_or_default(),
                    seq: entry.seq,
                    timestamp_ms: entry.timestamp_ms,
                    level: classify_engine_log(entry.source, &entry.text).to_string(),
                    text: entry.text,
                })
                .collect();
            // ring buffer 是正序；先从尾部截取，再恢复为正序供 UI 与实时事件拼接。
            logs.reverse();
            return Ok(logs);
        }
        drop(mp);

        // fallback: 从上一实例读取
        let last_mp = entry.last_managed_process.lock().await;
        if let Some(managed) = last_mp.as_ref() {
            let history = managed.log_history().await;
            let mut logs: Vec<StructuredLogEntry> = history
                .into_iter()
                .rev()
                .take(max_lines)
                .map(|entry| StructuredLogEntry {
                    engine_id: engine_id.to_string(),
                    instance_id: instance_id.clone().unwrap_or_default(),
                    seq: entry.seq,
                    timestamp_ms: entry.timestamp_ms,
                    level: classify_engine_log(entry.source, &entry.text).to_string(),
                    text: entry.text,
                })
                .collect();
            logs.reverse();
            return Ok(logs);
        }

        Ok(Vec::new())
    }
}

/// 从子进程输出内容推断展示/tracing 级别。
///
/// stdout/stderr 只是传输通道，不等于日志级别：Paddle/PaddleX 会把下载进度写到
/// stderr，若直接映射为 warn 会产生大量伪告警。受信任 wrapper 的显式前缀优先，
/// 未分类输出降为 debug。
pub(super) fn classify_engine_log(
    _source: crate::infra::local_engine::log_pipe::LogSource,
    text: &str,
) -> super::super::dto::EngineLogLevel {
    use super::super::dto::EngineLogLevel;
    let trimmed = text.trim_start();
    if trimmed.starts_with("[ERROR]")
        || trimmed.starts_with("ERROR:")
        || trimmed.starts_with("Traceback ")
    {
        EngineLogLevel::Error
    } else if trimmed.starts_with("[WARN]") || trimmed.starts_with("WARNING:") {
        EngineLogLevel::Warn
    } else if trimmed.starts_with("[INFO]") || trimmed.starts_with("[STATE]") {
        EngineLogLevel::Info
    } else if trimmed.starts_with("[TRACE]") {
        EngineLogLevel::Trace
    } else {
        EngineLogLevel::Debug
    }
}

// ── 日志投影辅助 ──────────────────────────────────────────────────────────

/// 把 ManagedProcess 的实时日志转发到 EventPort。
///
/// **0.22.3 Task H**: 真正的日志实例隔离。
///
/// 隔离机制（三重保障）：
/// 1. **CancellationToken**: stop/rollback/restart 时 cancel，pump 立即退出。
/// 2. **实时身份校验**: 每条日志 emit 前从 `entry.current_identity` 实时读取当前实例 ID，
///    如果与 pump 启动时的 `instance_id` 不匹配（说明已 restart），跳过并退出。
/// 3. **broadcast Closed**: ManagedProcess 的 LogPipe 被 drop 时 broadcast 关闭。
///
/// 不再比较两个静态 instance_id 副本——旧实现中 `expected_instance_id` 和
/// `instance_id` 都是启动时捕获的，永远相等，无法识别 restart。
///
/// 事件 payload 的 `instance_id` 始终为日志真实来源实例（pump 启动时的 instance_id），
/// 不受 stop/restart 后 current_identity 变化的影响。
pub(super) async fn pump_logs_to_event_port(
    mut subscriber: crate::infra::local_engine::log_pipe::LogSubscriber,
    event_port: Arc<dyn EventPort>,
    engine_id: EngineId,
    instance_id: String,
    entry: Arc<EngineEntry>,
    cancel_token: CancellationToken,
) {
    use tokio::sync::broadcast::error::RecvError;

    loop {
        // 先检查 cancellation——被 cancel 时立即退出
        if cancel_token.is_cancelled() {
            tracing::debug!(engine = %engine_id, "日志 pump 结束（cancelled）");
            break;
        }

        // 用 select 同时监听 broadcast 和 cancellation
        tokio::select! {
            biased; // 优先检查 cancellation
            _ = cancel_token.cancelled() => {
                tracing::debug!(engine = %engine_id, "日志 pump 结束（cancelled）");
                break;
            }
            result = subscriber.recv() => {
                match result {
                    Ok(log_entry) => {
                        // 实时读取当前 identity——如果已 stop/rollback/restart，
                        // current_identity 会变为 None 或不同的 instance_id
                        let current_instance_id = entry
                            .current_identity()
                            .await
                            .map(|i| i.instance_id.clone());

                        match current_instance_id {
                            Some(ref current) if current == &instance_id => {
                                // 身份匹配——同时进入 Blink tracing 与 UI 日志流。
                                // 未分类的第三方输出降为 debug，避免下载进度污染默认日志。
                                let level = classify_engine_log(log_entry.source, &log_entry.text);
                                match level {
                                    super::super::dto::EngineLogLevel::Error => tracing::error!(engine = %engine_id, instance = %instance_id, seq = log_entry.seq, output = %log_entry.text, "本地引擎输出"),
                                    super::super::dto::EngineLogLevel::Warn => tracing::warn!(engine = %engine_id, instance = %instance_id, seq = log_entry.seq, output = %log_entry.text, "本地引擎输出"),
                                    super::super::dto::EngineLogLevel::Info => tracing::info!(engine = %engine_id, instance = %instance_id, seq = log_entry.seq, output = %log_entry.text, "本地引擎输出"),
                                    super::super::dto::EngineLogLevel::Trace => tracing::trace!(engine = %engine_id, instance = %instance_id, seq = log_entry.seq, output = %log_entry.text, "本地引擎输出"),
                                    _ => tracing::debug!(engine = %engine_id, instance = %instance_id, seq = log_entry.seq, output = %log_entry.text, "本地引擎输出"),
                                }
                                event_port.emit_log(
                                    &engine_id,
                                    &instance_id,
                                    log_entry.seq,
                                    level,
                                    &log_entry.text,
                                );
                            }
                            Some(ref current) => {
                                // 身份不匹配——说明已 restart，旧 pump 退出
                                tracing::debug!(
                                    engine = %engine_id,
                                    expected = %instance_id,
                                    actual = %current,
                                    "日志 pump: 实例已切换，退出"
                                );
                                break;
                            }
                            None => {
                                // identity 已被清理（stop/rollback）——退出
                                tracing::debug!(
                                    engine = %engine_id,
                                    "日志 pump: identity 已清理，退出"
                                );
                                break;
                            }
                        }
                    }
                    Err(RecvError::Lagged(n)) => {
                        tracing::debug!(
                            engine = %engine_id,
                            missed = n,
                            "日志 pump 落后，跳过"
                        );
                        continue;
                    }
                    Err(RecvError::Closed) => {
                        tracing::debug!(engine = %engine_id, "日志 pump 结束（broadcast closed）");
                        break;
                    }
                }
            }
        }
    }
}
