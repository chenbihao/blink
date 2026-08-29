//! AdapterConfig 配置真源（0.22.6 收敛）。
//!
//! **唯一入口**：`adapter_config_for_engine`——从现有配置真源
//! （`SttConfig` / `OcrConfig`）为指定引擎构造 `AdapterConfig`。
//!
//! ## 去重背景
//!
//! 此前 config→AdapterConfig 的构造逻辑（含 0.22.6 的
//! `device=cuda → Cpu` 归一化业务规则）在 commands、maintenance 兼容层、
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

/// 从 `SttConfig.local_engine` 构造 FunASR `AdapterConfig`（纯函数，可测）。
///
/// 0.22.6 归一化：descriptor 只声明 CPU profile，历史配置残留的
/// `device=cuda` 一律归一化为 `Cpu`——显式 `Cuda` 会在 `resolve_profile`
/// 中因无 CUDA profile 直接报错。
///
/// 0.22.6.1 设备唯一真相：`engine_config.device` 同步归一化为 `"cpu"`，
/// 使 compute_preference 与 engine_config.device 不再输出矛盾值——
/// 防止诊断和其他消费者看到 `Cpu` + `cuda` 双真相。历史 STT device
/// 字段仅保留 wire/config 兼容，不是启动执行真相。
pub fn funasr_adapter_config_from(
    local: &crate::domain::config::stt_config::LocalEngineConfig,
) -> AdapterConfig {
    if local.device != "cpu" {
        tracing::warn!(
            device = %local.device,
            "FunASR 历史配置 device 非 cpu，归一化为 Cpu（0.22.6 仅支持 CPU profile）"
        );
    }

    let mut funasr_config =
        crate::app::local_engine::funasr::FunasrEngineConfig::from_stt_config(local);
    // engine_config.device 归一化——与 compute_preference=Cpu 保持一致
    funasr_config.device = "cpu".to_string();

    AdapterConfig {
        preferred_port: Some(local.server_port),
        compute_preference: Some(ComputePreference::Cpu),
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
        crate::app::local_engine::funasr::FUNASR_ENGINE_ID => ComputePreference::Cpu,
        crate::app::local_engine::paddleocr::PADDLEOCR_ENGINE_ID => {
            crate::domain::config::ocr_config::get_ocr_config().compute_preference
        }
        _ => ComputePreference::Auto,
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

    /// 0.22.6.1：历史 device=cuda 配置在 config_source 输出的
    /// compute_preference 与 engine_config.device 必须一致（都是 CPU）。
    #[test]
    fn funasr_config_source_computes_and_device_consistent() {
        let local = crate::domain::config::stt_config::LocalEngineConfig {
            server_port: 9000,
            funasr_model: "iic/SenseVoiceSmall".to_string(),
            device: "cuda".to_string(),
            ..Default::default()
        };
        let config = funasr_adapter_config_from(&local);

        // compute_preference 归一化为 Cpu
        assert_eq!(config.compute_preference, Some(ComputePreference::Cpu));
        // engine_config.device 同步归一化——不再输出矛盾值
        assert_eq!(config.engine_config["device"], "cpu");
    }

    /// engine_config 归一化后仍可被 `FunasrEngineConfig` 反序列化（wire 兼容）。
    #[test]
    fn funasr_config_source_engine_config_round_trips() {
        let local = crate::domain::config::stt_config::LocalEngineConfig {
            server_port: 8000,
            funasr_model: "paraformer-zh".to_string(),
            device: "cuda".to_string(),
            use_itn: true,
            ..Default::default()
        };
        let config = funasr_adapter_config_from(&local);
        let back: crate::app::local_engine::funasr::FunasrEngineConfig =
            serde_json::from_value(config.engine_config).unwrap();
        assert_eq!(back.device, "cpu");
        assert_eq!(back.funasr_model, "paraformer-zh");
        assert!(back.use_itn);
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
