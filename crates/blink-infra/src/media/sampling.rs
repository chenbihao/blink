use super::{Budget, MediaError, PlanFrame};

/// 累计时间定位素材，避免逐帧取整导致时长漂移。只存源帧索引，不复制停留帧。
#[derive(Clone, Debug)]
pub struct SamplingPlan {
    ends_ms: Vec<u64>,
    indices: Vec<u32>,
    pub output_frames: u32,
}
impl SamplingPlan {
    pub const FPS: u32 = 30;
    pub fn new(
        frames: &[PlanFrame],
        source_count: u32,
        budget: &Budget,
    ) -> Result<Self, MediaError> {
        if frames.is_empty() {
            return Err(MediaError::InvalidPlan("没有有效帧"));
        }
        if frames.len() > budget.max_source_frames {
            return Err(MediaError::Budget("素材帧数"));
        }
        let mut ends_ms = Vec::with_capacity(frames.len());
        let mut indices = Vec::with_capacity(frames.len());
        let mut total = 0u64;
        for frame in frames {
            if frame.duration_ms == 0 || frame.src_idx >= source_count {
                return Err(MediaError::InvalidPlan("帧时长或索引"));
            }
            total = total
                .checked_add(frame.duration_ms as u64)
                .ok_or(MediaError::Budget("时长溢出"))?;
            if total > budget.max_duration_ms.min(600_000) {
                return Err(MediaError::Budget("编辑后总时长最多 10 分钟"));
            }
            ends_ms.push(total);
            indices.push(frame.src_idx);
        }
        Ok(Self {
            ends_ms,
            indices,
            output_frames: (total * Self::FPS as u64).div_ceil(1000) as u32,
        })
    }
    pub fn source_index(&self, sample: u32) -> Option<u32> {
        if sample >= self.output_frames {
            return None;
        }
        let i = self
            .ends_ms
            .partition_point(|end| end * Self::FPS as u64 <= sample as u64 * 1000);
        self.indices.get(i).copied()
    }
    pub fn duration_ms(&self) -> u64 {
        (self.output_frames as u64 * 1000).div_ceil(Self::FPS as u64)
    }
}
