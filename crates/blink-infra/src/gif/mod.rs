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

pub mod annotation;
pub mod audio;
pub(crate) mod audio_capture;
pub mod capture;
mod capture_ahead;
#[cfg(target_os = "windows")]
pub mod capture_windows;
pub mod encode;
pub mod overload;
pub mod preview;
pub mod space;
pub mod spool;
pub mod timing;
#[cfg(target_os = "windows")]
pub use capture_windows::{GdiFrameSource, SharedRegionPos};

pub use capture::{CaptureCommand, CaptureCtl, CaptureOutcome, FrameSource, spawn_pipeline};
pub use encode::{
    EncodeFrame, EncodePlan, ExportError, ExportOptions, ExportQuality, ExportStage, ExportStats,
    export_gif,
};
pub use overload::{
    MAX_INTERVAL_FACTOR, OverloadAction, QUEUE_HIGH_WATER, RecordingBudget, StopReason,
    decide_overload, finalize_durations,
};
pub use spool::{
    CaptureRegion, SPOOL_VERSION, SessionMeta, SessionSummary, SpoolError, SpoolReader,
    SpoolWriter, cleanup_expired, generate_session_id, list_session_ids, list_summaries,
    read_draft, read_session_meta, read_summary, write_draft,
};
