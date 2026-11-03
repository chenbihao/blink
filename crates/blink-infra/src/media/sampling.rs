use super::{Budget, FrameList, MediaError, PlanFrame};

/// A sparse page directory over an immutable frame projection. Output samples are
/// located from cumulative rational time, never expanded or rounded per frame.
#[derive(Clone, Debug)]
pub struct SamplingPlan {
    frames: FrameList<PlanFrame>,
    pages: Vec<u64>,
    pub output_frames: u32,
    fps: u32,
}
impl SamplingPlan {
    pub const FPS: u32 = 30;
    pub fn new(
        frames: &[PlanFrame],
        source_count: u32,
        budget: &Budget,
    ) -> Result<Self, MediaError> {
        Self::from_frames(frames.to_vec().into(), source_count, budget, Self::FPS)
    }
    pub fn from_frames(
        frames: FrameList<PlanFrame>,
        source_count: u32,
        budget: &Budget,
        fps: u32,
    ) -> Result<Self, MediaError> {
        if frames.is_empty() || ![10, 15, 24, 30, 60].contains(&fps) {
            return Err(MediaError::InvalidPlan("没有有效帧或不支持的帧率"));
        }
        if frames.len() > budget.max_source_frames {
            return Err(MediaError::Budget("素材帧数"));
        }
        let mut pages = Vec::new();
        let mut total = 0u64;
        for (i, frame) in frames.iter().enumerate() {
            if frame.duration_ms == 0 || frame.src_idx >= source_count {
                return Err(MediaError::InvalidPlan("帧时长或索引"));
            }
            if i % 256 == 0 {
                pages.push(total);
            }
            total = total
                .checked_add(frame.duration_ms as u64)
                .ok_or(MediaError::Budget("时长溢出"))?;
            if total > budget.max_duration_ms {
                return Err(MediaError::Budget("编辑后总时长"));
            }
        }
        let samples = total
            .checked_mul(fps as u64)
            .ok_or(MediaError::Budget("采样溢出"))?
            .div_ceil(1000);
        let output_frames =
            u32::try_from(samples).map_err(|_| MediaError::Budget("输出帧号溢出"))?;
        Ok(Self {
            frames,
            pages,
            output_frames,
            fps,
        })
    }
    pub fn source_index(&self, sample: u32) -> Option<u32> {
        if sample >= self.output_frames {
            return None;
        }
        let numerator = sample as u64 * 1000;
        let page = self
            .pages
            .partition_point(|start| *start * self.fps as u64 <= numerator)
            .saturating_sub(1);
        let mut at = self.pages[page];
        for i in page * 256..self.frames.len().min((page + 1) * 256) {
            let f = self.frames.get(i)?;
            at += f.duration_ms as u64;
            if at * self.fps as u64 > numerator {
                return Some(f.src_idx);
            }
        }
        None
    }
    pub fn duration_ms(&self) -> u64 {
        (self.output_frames as u64 * 1000).div_ceil(self.fps as u64)
    }
}
