//! 基础设施层：数据持久化、平台 API、通用工具

pub mod data;
pub mod event_names; // 0.21.14：事件名常量（从 domain 下沉，消除 infra→domain 反向依赖）
pub mod platform;
pub mod utils;
