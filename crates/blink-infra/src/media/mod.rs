//! 类型化媒体出口。调用方持有 TaskLease 与托管产物租约，不接受任意 FFmpeg 参数。
mod encode;
pub(crate) mod process;
mod sampling;
#[cfg(test)]
mod tests;

pub use encode::export_mp4;
pub use sampling::SamplingPlan;
use std::{fs::File, path::Path, sync::Arc, time::Duration};

#[derive(Debug, thiserror::Error)]
pub enum MediaError {
    #[error("已有媒体任务正在执行，请等待完成或取消")]
    Busy,
    #[error("导出已取消")]
    Cancelled,
    #[error("媒体任务超时，请缩短素材或降低尺寸")]
    Timeout,
    #[error("导出计划无效: {0}")]
    InvalidPlan(&'static str),
    #[error("超过媒体资源预算: {0}")]
    Budget(&'static str),
    #[error("媒体进程失败: {0}")]
    Process(&'static str),
    #[error("成品校验失败: {0}")]
    Verify(&'static str),
    #[error("目标文件已存在，请选择其他文件名")]
    FileExists,
    #[error("无法读取素材帧 {0}")]
    Source(u32),
    #[error("文件操作失败: {0}")]
    Io(#[from] std::io::Error),
}
impl MediaError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Busy => "media_busy",
            Self::Cancelled => "media_cancelled",
            Self::Timeout => "media_timeout",
            Self::InvalidPlan(_) => "media_plan",
            Self::Budget(_) => "media_budget",
            Self::Process(_) => "media_process",
            Self::Verify(_) => "media_verify",
            Self::FileExists => "media_exists",
            Self::Source(_) => "media_source",
            Self::Io(_) => "media_io",
        }
    }
}

#[derive(Clone, Debug)]
pub struct PlanFrame {
    pub src_idx: u32,
    pub duration_ms: u32,
}
#[derive(Clone, Debug)]
pub struct VideoPlan {
    pub width: u32,
    pub height: u32,
    pub frames: Vec<PlanFrame>,
}
#[derive(Clone, Copy, Debug, Default)]
pub enum Quality {
    High,
    #[default]
    Medium,
    Low,
}
impl Quality {
    fn crf(self) -> &'static str {
        match self {
            Self::High => "18",
            Self::Medium => "23",
            Self::Low => "28",
        }
    }
}
#[derive(Clone, Debug)]
pub struct Budget {
    pub max_duration_ms: u64,
    pub max_source_frames: usize,
    pub max_frame_bytes: usize,
    pub max_output_bytes: u64,
    pub timeout: Duration,
    pub threads: u32,
}
impl Default for Budget {
    fn default() -> Self {
        Self {
            max_duration_ms: 600_000,
            max_source_frames: 3600,
            max_frame_bytes: 16 << 20,
            max_output_bytes: 256 << 20,
            timeout: Duration::from_secs(1800),
            threads: 2,
        }
    }
}
#[derive(Clone, Debug, Default)]
pub struct VideoOptions {
    pub max_width: Option<u32>,
    pub quality: Quality,
    pub overwrite: bool,
    pub budget: Budget,
}
#[derive(Clone, serde::Serialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct VideoStats {
    pub format: &'static str,
    pub codec: &'static str,
    pub width: u32,
    pub height: u32,
    pub source_frames: u32,
    pub written_frames: u32,
    pub duration_ms: u64,
    pub fps: u32,
    pub bytes: u64,
    pub encode_ms: u64,
    pub peak_child_bytes: u64,
}
#[derive(Clone, Copy, Debug)]
pub struct Progress {
    pub stage: &'static str,
    pub frames_done: u32,
    pub frames_total: u32,
}
pub type ProgressCallback = Arc<dyn Fn(Progress) + Send + Sync>;

pub trait FrameSource: Send + 'static {
    fn dimensions(&self) -> (u32, u32);
    fn frame_count(&self) -> u32;
    fn read_bgra(&mut self, index: u32) -> Result<Vec<u8>, MediaError>;
}
impl FrameSource for crate::gif::SpoolReader {
    fn dimensions(&self) -> (u32, u32) {
        (self.meta.region.w, self.meta.region.h)
    }
    fn frame_count(&self) -> u32 {
        self.frame_count()
    }
    fn read_bgra(&mut self, index: u32) -> Result<Vec<u8>, MediaError> {
        self.read_frame(index)
            .map_err(|_| MediaError::Source(index))
    }
}

/// 跨 GUI/CLI 的重型任务锁，与安装/产物使用锁独立。无等待队列。
pub struct TaskLease {
    _file: File,
}
pub fn acquire_task_lease() -> Result<TaskLease, MediaError> {
    acquire_task_lease_at(&crate::local_engine::runtime::runtimes_root().join(".media-task.lock"))
}
fn acquire_task_lease_at(path: &Path) -> Result<TaskLease, MediaError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;
    file.try_lock().map_err(|e| match e {
        std::fs::TryLockError::WouldBlock => MediaError::Busy,
        std::fs::TryLockError::Error(e) => MediaError::Io(e),
    })?;
    Ok(TaskLease { _file: file })
}
