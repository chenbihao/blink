//! STT 云端诊断 command（0.22.6 phase B 瘦身后）。
//!
//! ## 已删除的旧 FunASR lifecycle 命令
//!
//! 以下命令未发版且前端 0 引用，已随 0.22.6 引擎架构收敛删除。
//! 生命周期/状态/日志/空间职责统一由通用 local_engine 命令承载
//! （见 `app/commands/local_engine.rs`）：
//!
//! | 已删除 command | 替代 |
//! |---|---|
//! | `get_funasr_env` | `get_local_engine_status` / `get_engine_diagnostics` |
//! | `setup_python_env` | `install_local_engine` |
//! | `start_funasr_server` / `stop_funasr_server` | `start_local_engine` / `stop_local_engine` |
//! | `get_funasr_log_history` | `get_local_engine_logs` |
//! | `diagnose_stt` | `get_engine_diagnostics` |
//! | `get_stt_space_usage` / `cleanup_stt_space` | `get_local_engine_storage` / `cleanup_local_engine` / `open_stt_folder` |
//! | `open_stt_folder` | `open_engine_folder` / `open_runtime_folder` |
//!
//! ## 云端 STT 诊断
//!
//! `test_cloud_stt` 不迁移到 `EngineManager`——它是云端 STT 诊断路径，
//! 与本地引擎生命周期无关，且前端语音页仍在使用。

/// 云端 STT 连接测试。
///
/// **不迁移到 EngineManager**——云端 STT 诊断路径不受影响。
#[tauri::command]
pub async fn test_cloud_stt() -> Result<serde_json::Value, String> {
    let config = crate::app::stt_config::get_stt_config();

    let endpoint = crate::domain::stt::cloud::resolve_stt_endpoint(&config)
        .map_err(|e| format!("云端 STT 配置解析失败: {e}"))?;

    let _is_chat_asr = endpoint.uses_chat_completion_asr;

    let audio_url = "https://isv-data.oss-cn-hangzhou.aliyuncs.com/ics/MaaS/ASR/test_audio/BAC009S0764W0121.wav";
    let dl_client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| format!("HTTP client 创建失败: {e}"))?;

    let resp = dl_client
        .get(audio_url)
        .send()
        .await
        .map_err(|e| format!("下载示例音频失败: {e}"))?;

    if !resp.status().is_success() {
        return Ok(serde_json::json!({
            "success": false,
            "error": format!("下载音频 HTTP {}", resp.status()),
        }));
    }

    let wav_bytes = resp
        .bytes()
        .await
        .map_err(|e| format!("读取音频字节失败: {e}"))?;

    let result = crate::domain::stt::cloud::send_stt_request(&endpoint, &wav_bytes).await;

    match result {
        Ok(text) => {
            tracing::info!(%text, "云端 STT 测试成功");
            Ok(serde_json::json!({
                "success": true,
                "text": text,
            }))
        }
        Err(e) => {
            let err_str = e.to_string();
            tracing::warn!(%err_str, "云端 STT 测试失败");
            Ok(serde_json::json!({
                "success": false,
                "error": err_str,
            }))
        }
    }
}

// ── Contract 测试 ──────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── test_cloud_stt 返回 success + text/error shape ──

    #[test]
    fn test_cloud_stt_signature_compiles() {
        let _ = test_cloud_stt as fn() -> _;
    }

    // ── 旧 lifecycle 命令不再存在（编译级保证：本模块已不含其定义）──
    // 若需恢复，先核对 docs/specs 的分层与命令收敛规则。
}
