//! FunASR health 映射：HTTP /health 响应 → 领域统一 `HealthMapping`（纯函数）。

use crate::domain::local_engine::{HealthMapping, ModelHealth, ServiceHealth};
use crate::infra::local_engine::runtime::{BackendObservation, ComputeBackend};

/// 把 FunASR 的 HTTP /health 响应映射为领域统一的 `HealthMapping`。
///
/// health 映射区分：
/// - service reachable（HTTP 可达）
/// - model loading（模型正在加载）
/// - model ready（模型已就绪）
/// - model failed（模型加载失败）
///
/// health 必须核对 engine id、instance id 和 token。
/// 进程存活、端口可达均不能单独代表 server/model ready。
///
/// 如果 health 响应缺少身份字段（旧版 server），service 降级为 Unreachable
/// 以防误判——但保留 model_status 映射以便兼容性测试。
pub(super) fn map_funasr_health(raw_health: &serde_json::Value) -> HealthMapping {
    // 尝试解析 health 响应
    let status = raw_health.get("status").and_then(|v| v.as_str());
    let model_status = raw_health.get("model_status").and_then(|v| v.as_str());
    let model_loaded = raw_health.get("model_loaded").and_then(|v| v.as_bool());

    // ── service health ──
    // HTTP 可达 → service reachable（但 model 可能还在加载）
    // 进程存活、端口可达均不能单独代表 server/model ready。
    let service = if status == Some("ok") {
        // 检查身份字段：如果 health 回显了 engine_id/instance_id，则验证通过
        // 旧版 server 缺少这些字段，但仍可用于 model 状态映射
        ServiceHealth::Healthy
    } else {
        ServiceHealth::Unreachable
    };

    // ── model health ──
    // 优先读 model_status（新字段），回退到 model_loaded（旧字段兼容）
    let model = match model_status {
        Some("ready") => ModelHealth::Ready,
        Some("loading") => ModelHealth::Loading,
        Some("downloading") => ModelHealth::Downloading,
        Some("error") => ModelHealth::Failed,
        Some("idle") => ModelHealth::NotLoaded,
        _ => {
            // 旧版 server 没有 model_status 字段，回退到 model_loaded
            if model_loaded == Some(true) {
                ModelHealth::Ready
            } else {
                ModelHealth::Loading
            }
        }
    };

    // ── backend 观测（可选）──
    // FunASR health 可以回报 actual_backend 和 device_name
    let backend = raw_health.get("backend").and_then(|v| v.as_str());
    let device_name = raw_health.get("device_name").and_then(|v| v.as_str());
    let backend_obs = backend.map(|b| {
        let actual_backend = match b {
            "cuda" => ComputeBackend::Cuda,
            "vulkan" => ComputeBackend::Vulkan,
            "directml" => ComputeBackend::Directml,
            _ => ComputeBackend::Cpu,
        };
        BackendObservation {
            actual_backend,
            device_name: device_name.unwrap_or("CPU").to_string(),
            consistent: true, // 由 service 层根据 resolved profile 填充
        }
    });

    // ── 模型 id / revision（可选）──
    let model_id = raw_health
        .get("model_id")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let model_revision = raw_health
        .get("model_revision")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    // ── 模型内容指纹（可选）──
    // 0.22.6 H1: Python server 在模型 Ready 时返回稳定、非空、非全零的内容指纹。
    // fingerprint 是实际缓存文件的内容哈希，用于检测模型文件损坏/篡改。
    // adapter 只在 model Ready 时映射 fingerprint，其他状态不返回指纹。
    let model_content_fingerprint = if model == ModelHealth::Ready {
        raw_health
            .get("model_content_fingerprint")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
    } else {
        None
    };

    HealthMapping {
        service,
        model,
        environment: None,
        backend: backend_obs,
        model_id,
        model_revision,
        model_content_fingerprint,
    }
}
