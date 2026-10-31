//! GIF 时间轴非破坏编辑域（0.28.7，phase §3.6「编辑期」）。
//!
//! **职责边界**：本模块只持「素材帧 → 导出帧」的编辑投影业务——修剪/删帧/
//! 速度/停留/循环/撤销重做、`timeline_revision` 与导出冻结。录制预算、过载
//! 决策与时长后处理属采集工程侧（`blink_infra::gif::overload`），不在此。
//!
//! **非破坏编辑**：素材帧不可变，编辑状态只是投影参数；导出时冻结快照，
//! 编辑与编码不混用 revision（phase 验收）。

pub mod timeline;

pub use timeline::{
    quantize_delay_ms, EditCommand, EditError, ExportPlan, ExportSnapshot, FrameView, PlanFrame, Timeline,
    MAX_DELAY_CS, MIN_DELAY_CS,
};
