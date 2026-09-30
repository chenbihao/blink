//! STT 语音转文字域 re-export shim（0.25.3 crate 化）。
//!
//! 实体已拆至 workspace crate `blink-domain-stt`（17K 行整体：引擎 trait +
//! 伪流式管线 + VAD + wav 解码 + 听写状态机）。依赖基座：blink-infra（audio/
//! secret）、blink-domain-capability（CapabilityError）、blink-domain-config
//! （stt_config）。本模块保持 `crate::domain::stt::*` 旧路径可用。

pub use blink_domain_stt::*;
pub use blink_domain_stt::{
    cloud, dictation, gguf_postprocess, local, postprocess, pseudo_streaming,
    sentence_state, streaming_port, transcribe, vad, vad_diagnostics, wav,
};
