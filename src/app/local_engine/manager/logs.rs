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
                // 与实时 pump 同一噪声抑制——历史回放不重演 llama.cpp/ORT 洪流
                .filter(|entry| {
                    !should_suppress_from_ui(
                        &entry.text,
                        classify_engine_log(entry.source, &entry.text),
                    )
                })
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
                .filter(|entry| {
                    !should_suppress_from_ui(
                        &entry.text,
                        classify_engine_log(entry.source, &entry.text),
                    )
                })
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
/// stderr，若直接映射为 warn 会产生大量伪告警。判定顺序：
/// 1. 受信任 wrapper 的显式前缀（`[ERROR]` / `WARNING:` 等）；
/// 2. tracing 格式的级别 token（worker 子进程 stderr 的 tracing 输出，
///    形如 `…Z  INFO blink::…: message`；ANSI 已在 LogPipe 入口剥离）；
/// 3. 未分类输出降为 debug。
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
        return EngineLogLevel::Error;
    }
    if trimmed.starts_with("[WARN]") || trimmed.starts_with("WARNING:") {
        return EngineLogLevel::Warn;
    }
    if trimmed.starts_with("[INFO]") || trimmed.starts_with("[STATE]") {
        return EngineLogLevel::Info;
    }
    if trimmed.starts_with("[TRACE]") {
        return EngineLogLevel::Trace;
    }
    // tracing 对齐格式：级别 token 两侧至少一个空格（" INFO " / " WARN "）
    for (token, level) in [
        (" ERROR ", EngineLogLevel::Error),
        (" WARN ", EngineLogLevel::Warn),
        (" INFO ", EngineLogLevel::Info),
        (" DEBUG ", EngineLogLevel::Debug),
        (" TRACE ", EngineLogLevel::Trace),
    ] {
        if text.contains(token) {
            return level;
        }
    }
    EngineLogLevel::Debug
}

/// 第三方推理栈内部噪声（llama.cpp / ORT / SenseVoice worker 计算细节）判定。
///
/// 这些行对用户不可读也不可行动，逐条透传会把前端日志面板刷成
/// `llama_kv_cache` / `GraphTransformer` 洪流。命中且级别低于 warn 的行
/// 只进 Blink tracing（trace 级），不投影 UI；warn/error 级别的第三方
/// 输出不受影响。前缀精确匹配——未列出的 `[sensevoice]` 行（如错误）
/// 仍然透传。
pub(super) fn is_third_party_internal_noise(text: &str) -> bool {
    let trimmed = text.trim_start();
    if trimmed.starts_with("llama_")
        || trimmed.starts_with("llm_load_")
        || trimmed.starts_with("sched_reserve:")
        || trimmed.starts_with("graph_reserve:")
        || trimmed.starts_with("resolve_fused_ops:")
        || trimmed.starts_with("print_info:")
        // SenseVoice GGUF worker 每次推理固定刷 6 行计算过程（实测样本），
        // 伪流式预览 500ms 一次即每秒 ~60 行
        || trimmed.starts_with("[sensevoice] building graph:")
        || trimmed.starts_with("[sensevoice] graph built")
        || trimmed.starts_with("[sensevoice] allocating graph")
        || trimmed.starts_with("[sensevoice] graph allocated")
        || trimmed.starts_with("[sensevoice] compute starting")
        || trimmed.starts_with("[sensevoice] compute complete:")
    {
        return true;
    }
    // ORT C++ 日志经 ort tracing 桥接：`…  INFO ort::logging: …`
    text.contains(" ort::logging:")
}

/// 噪声行是否应从 UI 投影中剔除（warn/error 始终保留）。
pub(super) fn should_suppress_from_ui(
    text: &str,
    level: super::super::dto::EngineLogLevel,
) -> bool {
    use super::super::dto::EngineLogLevel;
    is_third_party_internal_noise(text)
        && !matches!(level, EngineLogLevel::Error | EngineLogLevel::Warn)
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
                                // 第三方内部噪声（llama.cpp/ORT 细节）：Blink tracing
                                // 降为 trace 且不投影前端——warn/error 仍按原级透传
                                let suppress_ui = should_suppress_from_ui(&log_entry.text, level);
                                if suppress_ui {
                                    tracing::trace!(engine = %engine_id, instance = %instance_id, seq = log_entry.seq, output = %log_entry.text, "本地引擎输出（已抑制）");
                                } else {
                                    match level {
                                        super::super::dto::EngineLogLevel::Error => tracing::error!(engine = %engine_id, instance = %instance_id, seq = log_entry.seq, output = %log_entry.text, "本地引擎输出"),
                                        super::super::dto::EngineLogLevel::Warn => tracing::warn!(engine = %engine_id, instance = %instance_id, seq = log_entry.seq, output = %log_entry.text, "本地引擎输出"),
                                        super::super::dto::EngineLogLevel::Info => tracing::info!(engine = %engine_id, instance = %instance_id, seq = log_entry.seq, output = %log_entry.text, "本地引擎输出"),
                                        super::super::dto::EngineLogLevel::Trace => tracing::trace!(engine = %engine_id, instance = %instance_id, seq = log_entry.seq, output = %log_entry.text, "本地引擎输出"),
                                        _ => tracing::trace!(engine = %engine_id, instance = %instance_id, seq = log_entry.seq, output = %log_entry.text, "本地引擎输出"),
                                    }
                                    event_port.emit_log(
                                        &engine_id,
                                        &instance_id,
                                        log_entry.seq,
                                        level,
                                        &log_entry.text,
                                    );
                                }
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

#[cfg(test)]
mod tests {
    use super::{classify_engine_log, should_suppress_from_ui};
    use crate::app::local_engine::dto::EngineLogLevel;
    use crate::infra::local_engine::log_pipe::LogSource;

    #[test]
    fn classify_explicit_prefixes_unchanged() {
        assert!(matches!(
            classify_engine_log(LogSource::Stderr, "[ERROR] boom"),
            EngineLogLevel::Error
        ));
        assert!(matches!(
            classify_engine_log(LogSource::Stdout, "WARNING: low disk"),
            EngineLogLevel::Warn
        ));
        assert!(matches!(
            classify_engine_log(LogSource::Stdout, "[STATE] model loaded"),
            EngineLogLevel::Info
        ));
    }

    #[test]
    fn classify_parses_tracing_format_level_tokens() {
        // ANSI 已在 LogPipe 入口剥离；此处为剥离后的文本
        let info = "2026-09-05T07:22:13.452382Z  INFO blink::infra::local_engine::paraformer_worker: worker: Begin -> Ack gen=1";
        assert!(matches!(
            classify_engine_log(LogSource::Stderr, info),
            EngineLogLevel::Info
        ));
        let warn = "2026-09-05T07:22:13.452382Z  WARN blink::infra::local_engine::x: slow";
        assert!(matches!(
            classify_engine_log(LogSource::Stderr, warn),
            EngineLogLevel::Warn
        ));
        let error = "2026-09-05T07:22:13.452382Z ERROR blink::x: failed";
        assert!(matches!(
            classify_engine_log(LogSource::Stderr, error),
            EngineLogLevel::Error
        ));
        // 无级别 token 的输出降为 debug（下载进度等）
        assert!(matches!(
            classify_engine_log(LogSource::Stdout, "sensevoice.bin: 已下载 32 MB (13%)"),
            EngineLogLevel::Debug
        ));
    }

    #[test]
    fn third_party_noise_detection() {
        // llama.cpp 内部日志（GGUF worker 实测洪流样本）
        assert!(should_suppress_from_ui(
            "llama_kv_cache: size = 224.00 MiB ( 2048 cells, 28 layers, 1/1 seqs)",
            EngineLogLevel::Debug
        ));
        assert!(should_suppress_from_ui(
            "sched_reserve: CPU compute buffer size = 1203.01 MiB",
            EngineLogLevel::Debug
        ));
        assert!(should_suppress_from_ui(
            "resolve_fused_ops: Flash Attention enabled",
            EngineLogLevel::Info
        ));
        // ORT 图优化日志（ort::logging 桥接，剥离 ANSI 后）
        assert!(should_suppress_from_ui(
            "2026-09-05T07:22:06.343287Z  INFO ort::logging: GraphTransformer Level1_RuleBasedTransformer modified: 1 with status: OK",
            EngineLogLevel::Info
        ));
        // SenseVoice GGUF worker 每次推理的计算过程行（实测样本）
        assert!(should_suppress_from_ui(
            "[sensevoice] building graph: 88 frames",
            EngineLogLevel::Debug
        ));
        assert!(should_suppress_from_ui(
            "[sensevoice] compute complete: status=0",
            EngineLogLevel::Debug
        ));
        // 未列入清单的 [sensevoice] 行（如错误）不受影响
        assert!(!should_suppress_from_ui(
            "[sensevoice] unexpected failure",
            EngineLogLevel::Debug
        ));
        // Blink 自己的 worker 行不受影响
        assert!(!should_suppress_from_ui(
            "2026-09-05T07:22:13.452Z  INFO blink::infra::local_engine::paraformer_worker: worker: Begin -> Ack gen=1",
            EngineLogLevel::Info
        ));
        // 噪声但 warn/error 级别 → 仍然透传 UI
        assert!(!should_suppress_from_ui(
            "llama_context: failed to load model",
            EngineLogLevel::Error
        ));
    }
}
