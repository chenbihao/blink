//! OCR 配置稳定枚举（0.22.4）。
//!
//! 定义 `OcrBackendKind`、`PaddleModel`、`OcrLifecycle` 等稳定配置枚举。
//! 这些枚举在 domain 层定义，`OcrConfig`（ConfigKey 分片）在
//! `domain/config/ocr_config.rs` 中引用。
//!
//! ## 设计
//!
//! - `OcrBackendKind::default()` = `Windows`：升级与全新安装均默认 Windows。
//! - `PaddleModel::default()` = `Tiny`：spike 资格门唯一通过候选。
//! - `OcrLifecycle::default()` = `OnDemand`：默认空闲 5 分钟后停止。

use serde::{Deserialize, Serialize};

/// OCR 后端种类（用户可配置）。
///
/// - `Windows`：始终使用 WinRT backend（默认）。
/// - `PaddleOcr`：明确选择 PaddleOCR；未安装/启动失败时返回可行动错误，不静默回退。
/// - `Auto`：仅在 PaddleOCR 已热态 Ready 时使用它；否则立即走 WinRT。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum OcrBackendKind {
    /// 始终使用 WinRT backend。
    #[default]
    Windows,
    /// 明确选择 PaddleOCR（PP-OCRv6）。
    PaddleOcr,
    /// 自动选择：热态 Ready 用 PaddleOCR，否则 WinRT。
    Auto,
}

impl std::fmt::Display for OcrBackendKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OcrBackendKind::Windows => write!(f, "windows"),
            OcrBackendKind::PaddleOcr => write!(f, "paddleocr"),
            OcrBackendKind::Auto => write!(f, "auto"),
        }
    }
}

/// PaddleOCR 模型档位。
///
/// **首版只开放 `Tiny`**——spike 资格门唯一通过候选。
/// `Small`/`Medium` 已被 decision.md 判定为延迟/峰值内存不适合桌面，
/// 保留枚举值但不作为可用项；descriptor/catalog 只声明真正通过资格门的可用项。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum PaddleModel {
    /// PP-OCRv6 tiny 模型（1.5M params，49 种语言）。
    /// 唯一通过 spike 资格门的候选。
    #[default]
    Tiny,
    /// PP-OCRv6 small 模型（7.7M params，50 种语言）。
    /// **未通过资格门**：热识别 ~10.6s，峰值 1482MB。
    #[allow(dead_code)]
    Small,
    /// PP-OCRv6 medium 模型（34.5M params，50 种语言）。
    /// **未通过资格门**：热识别 ~56s，峰值 3031MB。
    #[allow(dead_code)]
    Medium,
}

impl std::fmt::Display for PaddleModel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PaddleModel::Tiny => write!(f, "tiny"),
            PaddleModel::Small => write!(f, "small"),
            PaddleModel::Medium => write!(f, "medium"),
        }
    }
}

impl PaddleModel {
    /// 返回此模型档位是否已通过生产资格门。
    ///
    /// 只有 `Tiny` 通过了 spike 资格门（decision.md）。
    /// `Small`/`Medium` 未通过延迟和内存门。
    pub fn is_production_ready(self) -> bool {
        matches!(self, PaddleModel::Tiny)
    }

    /// 返回 PaddleOCR 官方模型名（det + rec）。
    ///
    /// 来源：spike lock.json MODEL_MAP。
    pub fn official_model_names(self) -> (&'static str, &'static str) {
        match self {
            PaddleModel::Tiny => ("PP-OCRv6_tiny_det", "PP-OCRv6_tiny_rec"),
            PaddleModel::Small => ("PP-OCRv6_small_det", "PP-OCRv6_small_rec"),
            PaddleModel::Medium => ("PP-OCRv6_medium_det", "PP-OCRv6_medium_rec"),
        }
    }
}

/// OCR 引擎生命周期策略。
///
/// - `OnDemand`：首次请求时启动，空闲 TTL 后停止（默认 5 分钟）。
/// - `KeepRunning`：启动后保持运行，不自动停止。
/// - `StopAfterUse`：每次请求结束后立即停止。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum OcrLifecycle {
    /// 按需启动，空闲 TTL 后停止（默认）。
    #[default]
    OnDemand,
    /// 保持运行，不自动停止。
    #[allow(dead_code)]
    KeepRunning,
    /// 使用后立即停止。
    #[allow(dead_code)]
    StopAfterUse,
}

impl std::fmt::Display for OcrLifecycle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OcrLifecycle::OnDemand => write!(f, "on_demand"),
            OcrLifecycle::KeepRunning => write!(f, "keep_running"),
            OcrLifecycle::StopAfterUse => write!(f, "stop_after_use"),
        }
    }
}

/// 计算设备偏好（0.22.4 §3.5）。
///
/// 首版只允许 `auto` | `cpu`。
/// 未验证的 cuda/vulkan/directml 不得开放。
///
/// 直接引用 domain 唯一定义（`domain::local_engine::identity`——infra 只是
/// re-export 该定义），保持 domain 不依赖 infra 的分层铁则。
pub type ComputePreference = crate::domain::local_engine::identity::ComputePreference;

/// 默认空闲 TTL（秒）。
pub const DEFAULT_IDLE_TTL_SECONDS: u32 = 300;

/// 最小空闲 TTL（秒）。
pub const MIN_IDLE_TTL_SECONDS: u32 = 10;

/// 最大空闲 TTL（秒）。
pub const MAX_IDLE_TTL_SECONDS: u32 = 3600;

/// 校验 idle TTL 是否在允许范围内。
pub fn validate_idle_ttl(seconds: u32) -> Result<u32, String> {
    if seconds < MIN_IDLE_TTL_SECONDS {
        return Err(format!(
            "idle_ttl_seconds {seconds} 小于最小值 {MIN_IDLE_TTL_SECONDS}"
        ));
    }
    if seconds > MAX_IDLE_TTL_SECONDS {
        return Err(format!(
            "idle_ttl_seconds {seconds} 大于最大值 {MAX_IDLE_TTL_SECONDS}"
        ));
    }
    Ok(seconds)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serde_roundtrip_backend_kind() {
        for kind in [
            OcrBackendKind::Windows,
            OcrBackendKind::PaddleOcr,
            OcrBackendKind::Auto,
        ] {
            let json = serde_json::to_string(&kind).unwrap();
            let back: OcrBackendKind = serde_json::from_str(&json).unwrap();
            assert_eq!(kind, back);
        }
    }

    #[test]
    fn serde_unknown_backend_kind_falls_back_to_windows() {
        // serde 默认值保证：缺字段或未知字符串回落到 Windows
        let json = "\"unknown_backend\"";
        let kind: OcrBackendKind = serde_json::from_str(json).unwrap_or_default();
        assert_eq!(kind, OcrBackendKind::Windows);
    }

    #[test]
    fn serde_missing_field_falls_back_to_windows() {
        // 空对象反序列化 → default
        let json = "{}";
        #[derive(Deserialize)]
        struct Wrapper {
            #[serde(default)]
            backend: OcrBackendKind,
        }
        let w: Wrapper = serde_json::from_str(json).unwrap();
        assert_eq!(w.backend, OcrBackendKind::Windows);
    }

    #[test]
    fn tiny_is_production_ready() {
        assert!(PaddleModel::Tiny.is_production_ready());
        assert!(!PaddleModel::Small.is_production_ready());
        assert!(!PaddleModel::Medium.is_production_ready());
    }

    #[test]
    fn official_model_names_match_spike() {
        let (det, rec) = PaddleModel::Tiny.official_model_names();
        assert_eq!(det, "PP-OCRv6_tiny_det");
        assert_eq!(rec, "PP-OCRv6_tiny_rec");
    }

    #[test]
    fn validate_idle_ttl_accepts_default() {
        assert_eq!(
            validate_idle_ttl(DEFAULT_IDLE_TTL_SECONDS).unwrap(),
            DEFAULT_IDLE_TTL_SECONDS
        );
    }

    #[test]
    fn validate_idle_ttl_rejects_too_small() {
        assert!(validate_idle_ttl(MIN_IDLE_TTL_SECONDS - 1).is_err());
    }

    #[test]
    fn validate_idle_ttl_rejects_too_large() {
        assert!(validate_idle_ttl(MAX_IDLE_TTL_SECONDS + 1).is_err());
    }
}
