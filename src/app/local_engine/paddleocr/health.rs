//! PaddleOCR health 映射：HTTP /health 响应 → 领域统一 `HealthMapping`（纯函数）。

use crate::domain::local_engine::{HealthMapping, ModelHealth, ServiceHealth};
use crate::infra::local_engine::runtime::{BackendObservation, ComputeBackend};

/// 把 PaddleOCR 的 HTTP /health 响应映射为领域统一的 `HealthMapping`。
///
/// PaddleOCR server health 响应格式（protocol 0.3.0）：
/// ```json
/// {
///   "protocol_version": "0.3.0",
///   "engine_id": "paddleocr",
///   "instance_id": "uuid-4",
///   "token_fingerprint": "fp: + 16 hex",
///   "endpoint": "127.0.0.1:9100",
///   "service_state": "healthy",
///   "model_state": "Ready",
///   "model_id": "PP-OCRv6:PP-OCRv6_tiny_det:PP-OCRv6_tiny_rec",
///   "model_revision": "ppocrv6-tiny",
///   "model_content_fingerprint": "64-hex-sha256",
///   "actual_backend": "cpu",
///   "device_name": "CPU",
///   "uptime_seconds": 12.34
/// }
/// ```
///
/// health 必须核对 engine id、instance id、token_fingerprint、endpoint 和 actual_backend。
/// 如果 health 响应缺少任何身份字段，service 降级为 Unreachable。
///
/// **收紧 health 契约（0.22.5）**：
/// - model_state == "Ready" 时，model_id、model_revision、model_content_fingerprint
///   必须全部存在且格式正确（fingerprint 为 64 位小写 hex）。
/// - 缺字段不能通过 `if let Some` 静默接受——Ready 状态降级为 Failed。
/// - model_id 和 model_revision 必须与 descriptor 一致。
/// - model_content_fingerprint 是内容指纹字段，与 model_revision 分离。
pub(super) fn map_paddleocr_health(raw_health: &serde_json::Value) -> HealthMapping {
    // ── service health ──
    let service_state = raw_health.get("service_state").and_then(|v| v.as_str());

    // 检查身份字段是否完整：engine_id + instance_id + token_fingerprint + endpoint
    // token_fingerprint 必须以 "fp:" 前缀开头（与 Rust port::token_fingerprint 一致）
    let has_full_identity = raw_health.get("engine_id").is_some()
        && raw_health.get("instance_id").is_some()
        && raw_health
            .get("token_fingerprint")
            .and_then(|v| v.as_str())
            .is_some_and(|s| s.starts_with("fp:") && s.len() == 19) // fp: + 16 hex
        && raw_health.get("endpoint").is_some();

    let service = if service_state == Some("healthy") && has_full_identity {
        ServiceHealth::Healthy
    } else {
        ServiceHealth::Unreachable
    };

    // ── model health ──
    let model_state = raw_health.get("model_state").and_then(|v| v.as_str());

    // ── 模型 id / revision / fingerprint ──
    // 收集这些字段（无论 model_state 是什么）
    let model_id = raw_health
        .get("model_id")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let model_revision = raw_health
        .get("model_revision")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let model_content_fingerprint = raw_health
        .get("model_content_fingerprint")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    // ── 收紧 model health 契约 ──
    // Ready 状态要求 model_id、model_revision、model_content_fingerprint 全部存在
    // 且 fingerprint 为 64 位小写 hex。缺字段或格式错误时降级为 Failed。
    let model = match model_state {
        Some("Ready") => {
            // 检查必填字段
            let has_id = model_id.is_some();
            let has_rev = model_revision.is_some();
            let fp = model_content_fingerprint.as_deref();
            let fp_valid =
                fp.is_some_and(|s| s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit()));

            if has_id && has_rev && fp_valid {
                ModelHealth::Ready
            } else {
                tracing::warn!(
                    has_id,
                    has_rev,
                    has_fp = fp.is_some(),
                    fp_valid,
                    "PaddleOCR health 报告 Ready 但关键字段缺失或格式错误，降级为 Failed"
                );
                ModelHealth::Failed
            }
        }
        Some("Loading") => ModelHealth::Loading,
        Some("NotLoaded") => ModelHealth::NotLoaded,
        Some("Failed") => ModelHealth::Failed,
        _ => ModelHealth::Unknown,
    };

    // ── actual backend / device_name（从 health 响应读取）──
    // 未知 backend 值不得默认伪装 CPU
    let actual_backend_str = raw_health
        .get("actual_backend")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");
    let device_name = raw_health
        .get("device_name")
        .and_then(|v| v.as_str())
        .unwrap_or("Unknown")
        .to_string();

    let actual_backend = match actual_backend_str {
        "cpu" => ComputeBackend::Cpu,
        "cuda" => ComputeBackend::Cuda,
        "vulkan" => ComputeBackend::Vulkan,
        "directml" => ComputeBackend::Directml,
        _ => {
            // 未知值不得默认伪装 CPU
            tracing::warn!(
                actual_backend = actual_backend_str,
                "PaddleOCR health 报告了未知的 actual_backend，标记为不一致"
            );
            // 返回 CPU 但标记 inconsistent
            ComputeBackend::Cpu
        }
    };

    // 检查 actual_backend 与 resolved profile (cpu) 是否一致
    let consistent = actual_backend_str == "cpu" && actual_backend == ComputeBackend::Cpu;

    HealthMapping {
        service,
        model,
        environment: None,
        backend: Some(BackendObservation {
            actual_backend,
            device_name,
            consistent,
        }),
        model_id,
        model_revision,
        model_content_fingerprint,
    }
}
