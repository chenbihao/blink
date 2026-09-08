//! AdapterConfig 配置真源（0.22.6 收敛）。
//!
//! **唯一入口**：`adapter_config_for_engine`——从现有配置真源
//! （`SttConfig` / `OcrConfig`）为指定引擎构造 `AdapterConfig`。
//!
//! ## 去重背景
//!
//! 此前 config→AdapterConfig 的构造逻辑（含旧 `device` 值的安全归一化）在 commands、maintenance 兼容层、
//! `EngineManager::read_adapter_config_for_engine` 和 main.rs 自启链路
//! 各有一份副本。规则漂移会导致 repair 用 A 配置装、start 用 B 配置跑。
//! 现在所有调用方都经过本模块。
//!
//! ## 安全约束
//!
//! **禁止前端直接提交 `AdapterConfig.engine_config`**——本模块只从
//! 后端配置真源构造，不接受外部 executable/argv/env/URL。

use crate::domain::local_engine::AdapterConfig;
use crate::infra::local_engine::runtime::{ComputePreference, EngineId};

/// 构造 FunASR 引擎的 `AdapterConfig`。
pub fn funasr_adapter_config() -> AdapterConfig {
    let config = crate::app::stt_config::get_stt_config();
    funasr_adapter_config_from(&config.local_engine)
}

/// 将历史 FunASR 模型别名收敛到当前 GGUF 模型 id。
///
/// 该映射只处理已知旧值；未知模型保留原值并由 descriptor/registry 后续 fail closed，
/// 不把未知身份静默伪装成另一个模型。
pub fn normalize_funasr_model_id(model_id: &str) -> String {
    crate::domain::config::stt_config::legacy_model_to_gguf_id(model_id)
        .unwrap_or(model_id)
        .to_string()
}

/// 从 `SttConfig.local_engine` 构造 FunASR `AdapterConfig`（纯函数，可测）。
///
/// `device` 是旧配置兼容字段。它先按闭合 preference 解析，再依据当前
/// 模型的 descriptor 候选归一化；因此未来模型声明 Vulkan/CUDA 时可以保留
/// 合法显式值，而只声明 CPU 的模型会安全收敛到 CPU。未知值不会穿透到
/// 启动层，也不会凭空制造 profile。
pub fn funasr_adapter_config_from(
    local: &crate::domain::config::stt_config::LocalEngineConfig,
) -> AdapterConfig {
    let model_id = normalize_funasr_model_id(&local.funasr_model);
    let descriptor = crate::app::local_engine::funasr::make_funasr_adapter();
    let requested = ComputePreference::parse(&local.device);
    let preference = descriptor
        .descriptor()
        .normalize_preference_for_model(&model_id, requested);

    if requested.is_none() {
        tracing::warn!(
            device = %local.device,
            model = %model_id,
            "FunASR 配置 device 未知，按当前模型候选安全归一化"
        );
    } else if Some(preference) != requested {
        tracing::warn!(
            device = %local.device,
            model = %model_id,
            normalized = %preference,
            "FunASR 配置 backend 不属于当前模型，按 descriptor 候选归一化"
        );
    }

    let mut funasr_config =
        crate::app::local_engine::funasr::FunasrEngineConfig::from_stt_config(local);
    funasr_config.funasr_model = model_id.clone();
    // engine_config.device 与 compute_preference 保持同一真相，供后续 worker
    // 消费；最终实际 backend 仍由 ResolvedProfile 决定。
    funasr_config.device = preference.to_string();

    AdapterConfig {
        preferred_port: Some(local.server_port),
        model_id: Some(model_id),
        compute_preference: Some(preference),
        engine_config: funasr_config.to_json(),
    }
}

/// 构造 PaddleOCR 引擎的 `AdapterConfig`。
pub fn paddleocr_adapter_config() -> AdapterConfig {
    let ocr_config = crate::domain::config::ocr_config::get_ocr_config();
    let engine_config =
        crate::app::local_engine::paddleocr::PaddleOcrEngineConfig::from_ocr_config();

    AdapterConfig {
        preferred_port: None,
        model_id: None,
        compute_preference: Some(ocr_config.compute_preference),
        engine_config: engine_config.to_json(),
    }
}

/// 按引擎 id 从配置真源构造 `AdapterConfig`。
///
/// 未接线的引擎返回 `None`（调用方决定如何报错）。
pub fn adapter_config_for_engine(engine_id: &EngineId) -> Option<AdapterConfig> {
    match engine_id.as_str() {
        crate::app::local_engine::funasr::FUNASR_ENGINE_ID => Some(funasr_adapter_config()),
        crate::app::local_engine::paddleocr::PADDLEOCR_ENGINE_ID => {
            Some(paddleocr_adapter_config())
        }
        _ => None,
    }
}

/// 读取当前引擎的 compute preference（catalog/current 投影用）。
pub fn current_compute_preference(engine_id: &EngineId) -> ComputePreference {
    match engine_id.as_str() {
        crate::app::local_engine::funasr::FUNASR_ENGINE_ID => {
            let config = crate::app::stt_config::get_stt_config();
            funasr_adapter_config_from(&config.local_engine)
                .compute_preference
                .unwrap_or(ComputePreference::Auto)
        }
        crate::app::local_engine::paddleocr::PADDLEOCR_ENGINE_ID => {
            crate::domain::config::ocr_config::get_ocr_config().compute_preference
        }
        _ => ComputePreference::Auto,
    }
}

/// 读取当前引擎选择的模型 id，供 catalog/安装解析共用。
pub fn current_model_id(engine_id: &EngineId) -> Option<String> {
    match engine_id.as_str() {
        crate::app::local_engine::funasr::FUNASR_ENGINE_ID => {
            let model_id = crate::app::stt_config::get_stt_config()
                .local_engine
                .funasr_model;
            Some(normalize_funasr_model_id(&model_id))
        }
        crate::app::local_engine::paddleocr::PADDLEOCR_ENGINE_ID => {
            Some(crate::app::local_engine::implementation_registry::paddleocr_inprocess_model_id())
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn funasr_config_normalizes_device_to_cpu() {
        let config = funasr_adapter_config();
        assert_eq!(config.compute_preference, Some(ComputePreference::Cpu));
        assert!(config.preferred_port.is_some());
        assert!(!config.engine_config.is_null());
    }

    /// SenseVoice 已声明 CUDA profile，历史 device=cuda 配置应保持一致。
    #[test]
    fn funasr_config_source_computes_and_device_consistent() {
        let local = crate::domain::config::stt_config::LocalEngineConfig {
            server_port: 9000,
            funasr_model: "iic/SenseVoiceSmall".to_string(),
            device: "cuda".to_string(),
            ..Default::default()
        };
        let config = funasr_adapter_config_from(&local);

        assert_eq!(config.compute_preference, Some(ComputePreference::Cuda));
        assert_eq!(config.engine_config["device"], "cuda");
    }

    #[test]
    fn paraformer_vulkan_preference_is_preserved() {
        let local = crate::domain::config::stt_config::LocalEngineConfig {
            funasr_model: crate::app::local_engine::funasr::gguf::GGUF_PARAFORMER_ID.to_string(),
            device: "vulkan".to_string(),
            ..Default::default()
        };
        let config = funasr_adapter_config_from(&local);

        assert_eq!(config.compute_preference, Some(ComputePreference::Vulkan));
        assert_eq!(config.engine_config["device"], "vulkan");
    }

    #[test]
    fn unknown_funasr_device_is_normalized_to_current_model_cpu() {
        let local = crate::domain::config::stt_config::LocalEngineConfig {
            funasr_model: crate::app::local_engine::funasr::gguf::GGUF_SENSEVOICE_ID.to_string(),
            device: "quantum".to_string(),
            ..Default::default()
        };
        let config = funasr_adapter_config_from(&local);
        assert_eq!(
            config.model_id.as_deref(),
            Some(local.funasr_model.as_str())
        );
        assert_eq!(config.compute_preference, Some(ComputePreference::Cpu));
        assert_eq!(config.engine_config["device"], "cpu");
    }

    #[test]
    fn legacy_funasr_model_ids_normalize_once_at_config_boundary() {
        assert_eq!(
            normalize_funasr_model_id("iic/SenseVoiceSmall"),
            crate::app::local_engine::funasr::gguf::GGUF_SENSEVOICE_ID
        );
        assert_eq!(normalize_funasr_model_id("unknown-model"), "unknown-model");
    }

    /// engine_config 归一化后仍可被 `FunasrEngineConfig` 反序列化（wire 兼容）。
    #[test]
    fn funasr_config_source_engine_config_round_trips() {
        let local = crate::domain::config::stt_config::LocalEngineConfig {
            server_port: 8000,
            funasr_model: "paraformer-zh".to_string(),
            device: "cuda".to_string(),
            ..Default::default()
        };
        let config = funasr_adapter_config_from(&local);
        let back: crate::app::local_engine::funasr::FunasrEngineConfig =
            serde_json::from_value(config.engine_config).unwrap();
        assert_eq!(back.device, "cpu");
        assert_eq!(
            back.funasr_model,
            crate::app::local_engine::funasr::gguf::GGUF_PARAFORMER_ID
        );
    }

    #[test]
    fn paddleocr_config_uses_ocr_preference() {
        let config = paddleocr_adapter_config();
        assert!(config.compute_preference.is_some());
        assert!(!config.engine_config.is_null());
    }

    #[test]
    fn unknown_engine_returns_none() {
        let eid = EngineId::new("unknown-engine").unwrap();
        assert!(adapter_config_for_engine(&eid).is_none());
    }

    #[test]
    fn known_engines_return_config() {
        let funasr = EngineId::new(crate::app::local_engine::funasr::FUNASR_ENGINE_ID).unwrap();
        assert!(adapter_config_for_engine(&funasr).is_some());
        let ocr = EngineId::new(crate::app::local_engine::paddleocr::PADDLEOCR_ENGINE_ID).unwrap();
        assert!(adapter_config_for_engine(&ocr).is_some());
    }
}
