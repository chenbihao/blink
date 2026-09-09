//! `transcribe_audio` Capability（0.22.16 Handoff 05）。
//!
//! 原子文件转写能力——接收 opaque `audio_ref`，委托已注入的
//! `AudioTranscriptionPort` 执行一次性转写，把 service result 投影为
//! 单个 `CapabilityResult::Items` item。
//!
//! **信任边界**：
//! - 只接受闭合参数 `{ "audio_ref": "opaque token" }`；
//! - 拒绝 path、url、file://、base64、bytes、engine、model、worker 地址
//!   或任意 passthrough options；
//! - 即使框架不自动校验 JSON Schema，invoke() 也主动拒绝额外字段；
//! - 不读路径、不访问 EngineManager、不调用 VoiceService、不发 G1/G2/G3 事件。
//!
//! **Policy**：
//! - `allowed_origins: ALL`
//! - `runtime_requirement: MAIN_PROCESS`
//! - `danger: Safe`
//! - `sensitive: true`
//! - `ai_default: Off`
//! - `mcp_default: DefaultOff`
//! - `confirmation: sensitive()`
//!
//! Cloud 外发仍由 H04 service 做动态安全拒绝，静态 sensitive 确认
//! 不能被当作含糊的第三方上传授权。

use std::sync::Arc;

use serde_json::{Value, json};

use crate::domain::capability::{
    AiDefault, Capability, CapabilityError, CapabilityPolicy, CapabilityResult, CapabilitySchema,
    ConfirmationPolicy, DangerClass, ItemResult, McpDefault, OriginSet, RuntimeRequirement,
};
use crate::domain::stt::transcribe::{AudioTranscriptionPort, AudioTranscriptionRequest};

/// `transcribe_audio` — 通过 opaque audio_ref 转写音频文件。
///
/// 入参：`{ "audio_ref": "aref_..." }`
/// 出参：`Items { items: [ItemResult { data: { ...transcription_result } }] }`
pub struct TranscribeAudio;

/// 允许的唯一顶层字段。
const ALLOWED_KEYS: &[&str] = &["audio_ref"];

#[async_trait::async_trait]
impl Capability for TranscribeAudio {
    fn id(&self) -> &str {
        "transcribe_audio"
    }

    fn schema(&self) -> CapabilitySchema {
        CapabilitySchema {
            name: "transcribe_audio".into(),
            description:
                "转写音频文件为文本。接收 opaque audio_ref（由 app 层签发的短期有效引用），"
                    .to_string()
                    + "使用当前配置的 STT 引擎执行一次性转写，返回识别文本和诊断元数据。"
                    + "不接受路径、URL、base64 或任意 passthrough 选项。"
                    + "引擎未安装、未配置或不可用时返回结构化可行动错误。",
            parameters: json!({
                "type": "object",
                "properties": {
                    "audio_ref": {
                        "type": "string",
                        "description": "opaque 音频引用（由 app 层签发，短期有效）"
                    }
                },
                "required": ["audio_ref"],
                "additionalProperties": false
            }),
            sensitive: true,
        }
    }

    fn policy(&self) -> CapabilityPolicy {
        CapabilityPolicy {
            allowed_origins: OriginSet::ALL,
            runtime_requirement: RuntimeRequirement::MAIN_PROCESS,
            danger: DangerClass::Safe,
            sensitive: true,
            ai_default: AiDefault::Off,
            mcp_default: McpDefault::DefaultOff,
            confirmation: ConfirmationPolicy::sensitive(),
        }
    }

    async fn invoke(
        &self,
        args: Value,
        ctx: &crate::domain::capability::InvokeContext<'_>,
    ) -> Result<CapabilityResult, CapabilityError> {
        // 1. 前置截止时刻检查
        if ctx.is_expired() {
            return Err(CapabilityError::Timeout {
                detail: "transcribe_audio 截止时刻已过".into(),
            });
        }

        // 2. 主动拒绝额外字段（即使框架不自动校验 JSON Schema）
        if let Value::Object(map) = &args {
            for key in map.keys() {
                if !ALLOWED_KEYS.contains(&key.as_str()) {
                    return Err(CapabilityError::InvalidArgs {
                        detail: format!("transcribe_audio: 不接受参数 '{key}'，只接受 audio_ref"),
                    });
                }
            }
        }

        // 3. 解析 audio_ref（required、non-empty string）
        let audio_ref = args
            .get("audio_ref")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| CapabilityError::InvalidArgs {
                detail: "transcribe_audio: 缺少 audio_ref 参数或值为空".into(),
            })?;

        // 4. 获取 AudioTranscriptionPort
        let port: &Arc<dyn AudioTranscriptionPort> =
            ctx.env
                .audio_transcription()
                .ok_or_else(|| CapabilityError::Unsupported {
                    required: "main_process (audio_transcription port not injected)".into(),
                    actual: ctx.runtime.as_requirement().to_string(),
                })?;

        // 5. 构造请求并委托 port 执行
        let request = AudioTranscriptionRequest {
            audio_ref: audio_ref.to_string(),
        };

        let result = port
            .transcribe(request, ctx.deadline)
            .await
            .map_err(|e| e.to_capability_error())?;

        // 6. 把 service result 投影为单个 CapabilityResult::Items item
        let data = json!({
            "text": result.text,
            "duration_ms": result.duration_ms,
            "engine_id": result.engine_id,
            "model_id": result.model_id,
            "engine_generation": result.engine_generation,
            "engine_instance_id": result.engine_instance_id,
            "source_format": result.source_format,
            "normalized_format": result.normalized_format,
            "normalization": result.normalization,
            "no_speech": result.no_speech,
        });

        tracing::debug!(
            engine_id = %result.engine_id,
            model_id = %result.model_id,
            duration_ms = result.duration_ms,
            no_speech = result.no_speech,
            "transcribe_audio: 转写完成"
        );

        let item = ItemResult {
            data,
            desc: None,
            actions: Vec::new(),
        };

        Ok(CapabilityResult::Items { items: vec![item] })
    }
}

inventory::submit!(crate::domain::capability::CapabilityEntry {
    factory: || Arc::new(TranscribeAudio) as Arc<dyn Capability>,
});

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::capability::{
        AiDefault, DangerClass, InvocationOrigin, McpDefault, OriginSet, RuntimeRequirement,
    };

    // ── identity & schema ──────────────────────────────────────────────────

    #[test]
    fn id_is_transcribe_audio() {
        assert_eq!(TranscribeAudio.id(), "transcribe_audio");
    }

    #[test]
    fn schema_name_matches_id() {
        let s = TranscribeAudio.schema();
        assert_eq!(s.name, "transcribe_audio");
    }

    #[test]
    fn schema_has_audio_ref_only() {
        let s = TranscribeAudio.schema();
        let props = &s.parameters["properties"];
        assert_eq!(props["audio_ref"]["type"], "string");
        assert!(props.get("path").is_none(), "schema 不应暴露 path 参数");
        assert!(props.get("url").is_none(), "schema 不应暴露 url 参数");
        assert!(props.get("engine").is_none(), "schema 不应暴露 engine 参数");
        assert!(props.get("model").is_none(), "schema 不应暴露 model 参数");
        assert!(props.get("bytes").is_none(), "schema 不应暴露 bytes 参数");
    }

    #[test]
    fn schema_requires_audio_ref() {
        let s = TranscribeAudio.schema();
        let required = s.parameters["required"].as_array().unwrap();
        assert_eq!(required.len(), 1);
        assert!(required.contains(&json!("audio_ref")));
    }

    #[test]
    fn schema_additional_properties_false() {
        let s = TranscribeAudio.schema();
        assert_eq!(s.parameters["additionalProperties"], json!(false));
    }

    #[test]
    fn schema_sensitive_is_true() {
        let s = TranscribeAudio.schema();
        assert!(s.sensitive, "transcribe_audio 必须 sensitive=true");
    }

    // ── policy 一致性 ──────────────────────────────────────────────────────

    #[test]
    fn policy_allowed_origins_all() {
        let p = TranscribeAudio.policy();
        assert!(p.allowed_origins.contains(InvocationOrigin::LocalAi));
        assert!(p.allowed_origins.contains(InvocationOrigin::Mcp));
        assert!(p.allowed_origins.contains(InvocationOrigin::Cli));
        assert!(p.allowed_origins.contains(InvocationOrigin::LocalSurface));
    }

    #[test]
    fn policy_runtime_requirement_main_process() {
        let p = TranscribeAudio.policy();
        assert!(
            p.runtime_requirement
                .is_satisfied_by(RuntimeRequirement::MAIN_PROCESS),
            "transcribe_audio 只需 MAIN_PROCESS"
        );
        assert!(
            !p.runtime_requirement
                .is_satisfied_by(RuntimeRequirement::NONE),
            "无主进程运行时不满足要求"
        );
    }

    #[test]
    fn policy_danger_safe() {
        let p = TranscribeAudio.policy();
        assert_eq!(p.danger, DangerClass::Safe);
    }

    #[test]
    fn policy_sensitive_true() {
        let p = TranscribeAudio.policy();
        assert!(p.sensitive);
    }

    #[test]
    fn policy_ai_default_off() {
        let p = TranscribeAudio.policy();
        assert_eq!(p.ai_default, AiDefault::Off);
    }

    #[test]
    fn policy_mcp_default_default_off() {
        let p = TranscribeAudio.policy();
        assert_eq!(p.mcp_default, McpDefault::DefaultOff);
    }

    #[test]
    fn policy_confirmation_sensitive() {
        let p = TranscribeAudio.policy();
        assert!(p.requires_confirmation(), "sensitive=true 应触发确认");
        assert!(p.confirmation.required);
    }

    #[test]
    fn policy_matches_handoff_contract() {
        let p = TranscribeAudio.policy();
        assert_eq!(p.allowed_origins, OriginSet::ALL);
        assert!(
            p.runtime_requirement
                .is_satisfied_by(RuntimeRequirement::MAIN_PROCESS)
        );
        assert!(
            !p.runtime_requirement
                .is_satisfied_by(RuntimeRequirement::NONE)
        );
        assert_eq!(p.danger, DangerClass::Safe);
        assert!(p.sensitive);
        assert_eq!(p.ai_default, AiDefault::Off);
        assert_eq!(p.mcp_default, McpDefault::DefaultOff);
        assert!(p.confirmation.required);
        assert!(p.confirmation.rememberable);
    }
}
