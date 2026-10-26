//! GIF 短演示录制基础设施（0.28.7，phase §3.6）。
//!
//! **流水线**（录制期边采边压，原始帧有界、不整段驻留）：
//! ```text
//! 采集线程（GDI BitBlt + 光标叠加）
//!   → 有界帧队列（SyncChannel，帧数=字节上限）
//!   → 压缩线程（zstd 独立块 → spool.bin + JSONL 索引）
//! 编辑器/导出按需随机解码单帧；导出期才做调色板与 GIF 编码（encode）。
//! ```
//!
//! **与域层分工**：`blink_domain::gif` 持有过载决策/预算/时间轴纯逻辑；
//! 本模块是 Windows 采集、素材落盘与 GIF 编码的工程实现。

pub mod capture;
pub mod encode;
pub mod overload;
pub mod spool;
#[cfg(target_os = "windows")]
pub mod capture_windows;
#[cfg(target_os = "windows")]
pub use capture_windows::{GdiFrameSource, SharedRegionPos};

pub use capture::{spawn_pipeline, CaptureCommand, CaptureCtl, CaptureOutcome, FrameSource};
pub use overload::{
    decide_overload, finalize_durations, OverloadAction, RecordingBudget, StopReason,
    MAX_INTERVAL_FACTOR, QUEUE_HIGH_WATER,
};
pub use encode::{
    export_gif, EncodeFrame, EncodePlan, ExportError, ExportOptions, ExportQuality,
    ExportStage, ExportStats,
};
pub use spool::{
    cleanup_expired, generate_session_id, list_session_ids, CaptureRegion, SessionMeta,
    SpoolError, SpoolReader, SpoolWriter, SPOOL_VERSION,
};
