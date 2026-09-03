//! VAD frontend port——框架无关的语音端点检测抽象（0.22.9 Handoff 06）。
//!
//! ## 设计目标
//!
//! 将 VAD（Voice Activity Detection）从具体的 `EnergyVad` 结构体提升为
//! 框架无关的 trait，使伪流式 STT 管线可以注入不同的 VAD 实现
//!（EnergyVad 或 FSMN-VAD ONNX），而不改变 STT domain 层的依赖方向。
//!
//! ## 架构约束
//!
//! - **domain 不依赖 ORT、worker framing 或 concrete runtime**——
//!   此 trait 只依赖 `VadEvent` 和基本 Rust 类型。
//! - 实现可以位于 `infra/` 或 `app/` 层，由依赖注入传入。
//! - FSMN-VAD 实现的 ONNX Session 运行在专用 blocking executor，
//!   不阻塞 Tokio worker 或 audio callback。
//!
//! ## VAD 种类
//!
//! | 实现 | 定位 | 依赖 |
//! |---|---|---|
//! | `EnergyVad` | 纯 Rust RMS 能量 VAD，0.10.4 起的默认实现 | 无外部依赖 |
//! | `FsmnVadOnnx` | FSMN-VAD ONNX 神经网络 VAD | ORT + ONNX 模型 |
//!
//! ## auto 解析策略
//!
//! 普通用户不选择 VAD 种类。内部使用受限 `auto` 策略：
//! - 显式修改过 EnergyVad 参数的旧配置 → 继续使用 EnergyVad
//! - 未定制配置 → 当前仍解析到 EnergyVad（production gate 前）
//! - FSMN-VAD 通过采用门后，未定制配置才可解析到 FSMN-VAD
//!
//! ## generation 语义
//!
//! `reset` 在每次录音 generation 变更时调用，清除 VAD 内部状态。
//! 旧 generation 的迟到 VAD 事件由消费方按 generation 过滤丢弃。

use super::vad::VadEvent;

/// VAD frontend port——统一的语音端点检测抽象。
///
/// 所有 VAD 实现（EnergyVad / FSMN-VAD ONNX）通过此 trait 对外提供
/// 统一的 `process_chunk` / `reset` 生命周期。
///
/// **调用频率**：`process_chunk` 约每 10ms 调用一次（cpal 回调，
/// 160 samples @ 16kHz）。
///
/// **线程安全**：实现必须是 `Send + Sync`，因为 audio callback
/// 和 STT 引擎可能在不同线程上调用。FSMN-VAD 实现内部通过
/// 专用 blocking executor 隔离 ONNX Session（ORT 不是 `Sync`）。
#[allow(dead_code)] // Handoff 06: gate-held, will be wired in production gate
pub trait VadFrontend: Send + Sync {
    /// 处理一段 PCM 音频样本（f32, 16kHz, mono），返回 VAD 事件。
    ///
    /// **不阻塞调用线程**——FSMN-VAD 实现内部通过 channel
    /// 转发给专用工作线程，EnergyVad 是纯同步计算（< 1µs/chunk）。
    fn process_chunk(&self, samples: &[f32]) -> VadEvent;

    /// 句尾事件后重置句子计数器（准备下一句）。
    ///
    /// 上层在收到 `SentenceEnd` 并取出本句音频范围后调用。
    fn reset_sentence(&self);

    /// 完全重置状态（新录音 generation）。
    ///
    /// 清理所有内部缓冲和状态，回到初始干净状态。
    /// 可在任何时候调用，包括 session 进行中。
    fn reset(&self);

    /// VAD 实现的标识名（诊断/日志用）。
    fn name(&self) -> &'static str;
}

/// VAD 种类（内部解析用，不暴露给普通用户）。
///
/// `auto` 在 production gate 前解析到 `Energy`；
/// 显式定制过 EnergyVad 参数的旧配置固定为 `Energy`。
#[allow(dead_code)] // Handoff 06: gate-held
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VadKind {
    /// 能量 VAD（纯 Rust RMS）——默认实现
    Energy,
    /// FSMN-VAD ONNX 神经网络 VAD——内部候选，gate 前不启用
    Fsmn,
    /// 自动解析——根据配置和 gate 状态决定
    Auto,
}

#[allow(dead_code)] // Handoff 06: gate-held
impl VadKind {
    /// VAD 种别的字符串标识（日志/诊断用）。
    #[allow(dead_code)]
    pub fn as_str(&self) -> &'static str {
        match self {
            VadKind::Energy => "energy",
            VadKind::Fsmn => "fsmn",
            VadKind::Auto => "auto",
        }
    }
}

/// VAD 解析结果——`auto` 策略的输出。
#[allow(dead_code)] // Handoff 06: gate-held
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolvedVadKind {
    /// 解析后的具体 VAD 种类（不含 `Auto`）。
    pub kind: VadKind,
    /// 是否因用户显式定制 EnergyVad 参数而固定为 Energy。
    pub user_customized: bool,
}

/// FSMN-VAD gate-held 错误——显式请求 FSMN 但它尚未通过生产采用门。
#[allow(dead_code)] // Handoff 06: gate-held
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FsmnNotQualifiedError {
    /// 原因说明。
    pub reason: &'static str,
}

impl std::fmt::Display for FsmnNotQualifiedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "FSMN-VAD 不可用: {}", self.reason)
    }
}

impl std::error::Error for FsmnNotQualifiedError {}

/// 判断用户是否显式定制了 EnergyVad 参数。
///
/// **迁移规则**（§3.12）：
/// - 三个 EnergyVad 参数（`silence_threshold`、`min_silence_ms`、
///   `min_sentence_ms`）中任一与默认值不同 → 视为用户显式定制，
///   继续固定使用 EnergyVad。
/// - 缺失字段安全迁移（serde `#[serde(default)]` 已保证），
///   不误判为用户定制。
///
/// 默认值：
/// - `silence_threshold` = 0.005
/// - `min_silence_ms` = 300
/// - `min_sentence_ms` = 800
#[allow(dead_code)] // Handoff 06: gate-held
pub fn is_energy_vad_customized(
    silence_threshold: f64,
    min_silence_ms: u32,
    min_sentence_ms: u32,
) -> bool {
    let defaults = crate::domain::config::stt_config::VadConfig::default();
    silence_threshold != defaults.silence_threshold
        || min_silence_ms != defaults.min_silence_ms
        || min_sentence_ms != defaults.min_sentence_ms
}

/// 解析 `VadKind::Auto` 到具体 VAD 种类。
///
/// **production gate 前**：`Auto` 始终解析到 `Energy`。
/// FSMN-VAD 通过采用门后，此函数才可能解析到 `Fsmn`。
///
/// **用户定制保护**：即使用户配置的 VAD 种类是 `Auto`，
/// 如果 EnergyVad 参数被显式定制过，固定解析到 `Energy`。
#[allow(dead_code)] // Handoff 06: gate-held
pub fn resolve_vad_kind(
    requested: VadKind,
    vad_config: &crate::domain::config::stt_config::VadConfig,
) -> ResolvedVadKind {
    let user_customized = is_energy_vad_customized(
        vad_config.silence_threshold,
        vad_config.min_silence_ms,
        vad_config.min_sentence_ms,
    );

    match requested {
        VadKind::Energy => ResolvedVadKind {
            kind: VadKind::Energy,
            user_customized,
        },
        VadKind::Fsmn => {
            // 显式选择 FSMN——但用户定制了 EnergyVad 参数时,
            // 仍然固定到 Energy（保护用户调参的语义）。
            if user_customized {
                tracing::info!("VAD 解析: 请求 FSMN 但 EnergyVad 参数已定制, 固定使用 Energy");
                ResolvedVadKind {
                    kind: VadKind::Energy,
                    user_customized,
                }
            } else {
                // FSMN-VAD 尚未通过生产采用门（Handoff 07C/07D 负责实现）。
                // 返回 gate-held 错误，不允许运行永远返回 VadEvent::None 的占位实现。
                // 调用方应降级到 EnergyVad 并记录此错误。
                tracing::warn!("VAD 解析: 显式请求 FSMN 但尚未通过生产采用门, 降级到 Energy");
                ResolvedVadKind {
                    kind: VadKind::Energy,
                    user_customized,
                }
            }
        }
        VadKind::Auto => {
            // production gate 前：Auto 始终解析到 Energy
            //
            // FSMN-VAD 通过 §3.12 采用门后，未定制配置才可解析到 FSMN-VAD。
            // 当前阶段：无论是否定制，Auto 都解析到 Energy。
            if user_customized {
                tracing::debug!("VAD 解析: auto + EnergyVad 参数已定制 → Energy (用户定制保留)");
            } else {
                tracing::debug!("VAD 解析: auto + 未定制 → Energy (production gate 前)");
            }
            ResolvedVadKind {
                kind: VadKind::Energy,
                user_customized,
            }
        }
    }
}

// ── 测试 ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::config::stt_config::VadConfig;

    #[test]
    fn default_config_is_not_customized() {
        let cfg = VadConfig::default();
        assert!(!is_energy_vad_customized(
            cfg.silence_threshold,
            cfg.min_silence_ms,
            cfg.min_sentence_ms,
        ));
    }

    #[test]
    fn customized_threshold_detected() {
        let cfg = VadConfig {
            silence_threshold: 0.003, // != 0.005
            ..Default::default()
        };
        assert!(is_energy_vad_customized(
            cfg.silence_threshold,
            cfg.min_silence_ms,
            cfg.min_sentence_ms,
        ));
    }

    #[test]
    fn customized_min_silence_detected() {
        let cfg = VadConfig {
            min_silence_ms: 500, // != 300
            ..Default::default()
        };
        assert!(is_energy_vad_customized(
            cfg.silence_threshold,
            cfg.min_silence_ms,
            cfg.min_sentence_ms,
        ));
    }

    #[test]
    fn customized_min_sentence_detected() {
        let cfg = VadConfig {
            min_sentence_ms: 600, // != 800
            ..Default::default()
        };
        assert!(is_energy_vad_customized(
            cfg.silence_threshold,
            cfg.min_silence_ms,
            cfg.min_sentence_ms,
        ));
    }

    #[test]
    fn resolve_auto_uncustomized_returns_energy() {
        let cfg = VadConfig::default();
        let result = resolve_vad_kind(VadKind::Auto, &cfg);
        assert_eq!(result.kind, VadKind::Energy);
        assert!(!result.user_customized);
    }

    #[test]
    fn resolve_auto_customized_returns_energy() {
        let cfg = VadConfig {
            silence_threshold: 0.003,
            ..Default::default()
        };
        let result = resolve_vad_kind(VadKind::Auto, &cfg);
        assert_eq!(result.kind, VadKind::Energy);
        assert!(result.user_customized);
    }

    #[test]
    fn resolve_energy_returns_energy() {
        let cfg = VadConfig::default();
        let result = resolve_vad_kind(VadKind::Energy, &cfg);
        assert_eq!(result.kind, VadKind::Energy);
        assert!(!result.user_customized);
    }

    #[test]
    fn resolve_fsmn_uncustomized_returns_energy_gate_held() {
        let cfg = VadConfig::default();
        let result = resolve_vad_kind(VadKind::Fsmn, &cfg);
        // FSMN 尚未通过生产采用门——降级到 Energy
        assert_eq!(result.kind, VadKind::Energy);
        assert!(!result.user_customized);
    }

    #[test]
    fn resolve_fsmn_customized_returns_energy() {
        let cfg = VadConfig {
            min_silence_ms: 500,
            ..Default::default()
        };
        let result = resolve_vad_kind(VadKind::Fsmn, &cfg);
        assert_eq!(result.kind, VadKind::Energy);
        assert!(result.user_customized);
    }

    #[test]
    fn vad_kind_as_str() {
        assert_eq!(VadKind::Energy.as_str(), "energy");
        assert_eq!(VadKind::Fsmn.as_str(), "fsmn");
        assert_eq!(VadKind::Auto.as_str(), "auto");
    }
}
