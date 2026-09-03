//! STT 基础设施层——VAD 实现与 ONNX 运行时。
//!
//! 0.22.9 Handoff 06：VAD frontend port 的实现位于此模块。
//! domain 层定义 `VadFrontend` trait，infra 层提供具体实现：
//!
//! - `energy_vad_adapter`：纯 Rust EnergyVad 的 trait 适配器
//! - `fsmn_vad_onnx`：FSMN-VAD ONNX 神经网络 VAD 候选

pub mod energy_vad_adapter;
pub mod fsmn_vad_onnx;
pub mod fsmn_vad_runner;
