//! GIF 时间轴编辑模型（0.28.7，phase §3.6「编辑期」）。
//!
//! **非破坏编辑**：素材帧不可变，编辑状态只是「素材帧 → 导出帧」的投影参数
//! （修剪区间、删除集合、速度、单帧停留、循环），全部命令可撤销/重做；
//! `timeline_revision` 每次变更单调递增，导出时冻结快照，编辑与编码不混用
//! revision（phase 验收「编辑导出不混用 revision」）。
//!
//! **时长语义**：
//! - 素材帧时长 `base_dur_ms` 来自录制时间戳差（暂停已剔除）；删除帧的时长
//!   随画面一起移除，总时长缩短。
//! - 单帧停留 `hold` 覆盖该帧计算值（不改素材）。
//! - 导出 delay 量化为 GIF 的 1/100s 单位，最小 2（20ms——更小会被多数
//!   浏览器按 100ms 以上回放），上限 6s（超长停留对演示无意义且兼容性差）。

use super::runs::{Change, Runs};
use std::sync::{Arc, OnceLock};

use blink_infra::media::FrameList;
use serde::{Deserialize, Serialize};

/// GIF delay 以 10ms 为单位的下限（低于 2 的 delay 被浏览器按 ≥100ms 回放）。
pub const MIN_DELAY_CS: u32 = 2;
/// 编辑器手动停留上限；原始素材长静帧不受此限制。
pub const MAX_DELAY_CS: u32 = 600;

/// 编辑命令（serde 序列化给前端；字段用稳定短名）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EditCommand {
    /// 设置修剪起点（含），帧号为**素材原始索引**。
    TrimStart { frame: u32 },
    /// 设置修剪终点（含），帧号为素材原始索引；早于起点时拒绝。
    TrimEnd { frame: u32 },
    /// 删除一帧（素材原始索引）；该帧时长随之移除（总时长缩短）。
    DeleteFrame { frame: u32 },
    /// 批量删除（素材原始索引集合）；单命令单 undo 粒度。任一越界整条拒绝。
    DeleteFrames { frames: Vec<u32> },
    /// 恢复已删除的帧（素材原始索引集合，时长从素材基线还原）；
    /// 单命令单 undo 粒度。任一越界整条拒绝；未删除的条目跳过。
    RestoreFrames { frames: Vec<u32> },
    /// 全局速度因子，clamp 到 [0.25, 4.0]。
    SetSpeed { factor: f32 },
    /// 单帧停留时长（ms）；0 表示清除停留（回退速度计算）。帧号为素材原始索引。
    SetHold { frame: u32, ms: u32 },
    /// 批量设置多帧停留时长（同值；ms=0 批量清除）。单命令单 undo 粒度。
    /// 任一越界整条拒绝。
    SetHoldMany { frames: Vec<u32>, ms: u32 },
    /// 循环次数：None = 无限循环；Some(0) = 播放一次（GIF Finite(0)）。
    SetLoop { count: Option<u16> },
    /// 两路源音轨的非破坏混音设置；索引 0=麦克风，1=系统声音。
    SetAudio {
        source: usize,
        gain: f32,
        muted: bool,
    },
    /// Compact batch selection. Ranges are inclusive source indices, validated atomically.
    EditRanges {
        operation: RangeOperation,
        ranges: Vec<(u32, u32)>,
        ms: u32,
    },
    /// 重置全部编辑（不可撤销其他命令之后——reset 本身可撤销）。
    Reset,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RangeOperation {
    Delete,
    Restore,
    Hold,
}

/// 编辑后的单帧导出视图。
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct FrameView {
    /// 素材原始帧索引。
    pub src_idx: u32,
    /// 该帧的显示时长（ms，未量化——前端预览用；导出时再量化）。
    pub dur_ms: u32,
    /// 该帧是否被单帧停留覆盖（前端标记用）。
    pub held: bool,
}

/// 冻结的导出计划（revision 快照；编码期间素材与编辑互不干扰的契约）。
#[derive(Debug, Clone, PartialEq)]
pub struct ExportPlan {
    pub revision: u64,
    pub src_w: u32,
    pub src_h: u32,
    /// 量化后的逐帧 delay（单位 1/100s）。
    pub frames: FrameList<PlanFrame>,
    /// None = 无限循环。
    pub loop_count: Option<u16>,
}

/// 格式中立的毫秒快照。MP4 直接使用此时钟，GIF 单独适配量化与循环。
#[derive(Debug, Clone, PartialEq)]
pub struct ExportSnapshot {
    pub revision: u64,
    pub src_w: u32,
    pub src_h: u32,
    pub frames: FrameList<FrameView>,
    pub loop_count: Option<u16>,
    pub audio: Vec<AudioSpan>,
    pub mix: [AudioMix; 2],
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct AudioMix {
    pub gain: f32,
    pub muted: bool,
}
impl Default for AudioMix {
    fn default() -> Self {
        Self {
            gain: 1.0,
            muted: false,
        }
    }
}

/// 连续原始区间映射到有效时间轴；新增停留是静音，随后统一保音调变速。
/// 相邻区间合并，不为每一帧复制音轨或滤镜。
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AudioSpan {
    pub source_ms: u64,
    pub sound_ms: u64,
    pub silence_ms: u64,
    pub output_ms: u64,
    pub speed: f32,
}
impl ExportSnapshot {
    pub fn gif_plan(&self) -> ExportPlan {
        ExportPlan {
            revision: self.revision,
            src_w: self.src_w,
            src_h: self.src_h,
            frames: super::quantized::frames(&self.frames),
            loop_count: self.loop_count,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct PlanFrame {
    pub src_idx: u32,
    /// 量化后的 delay，单位 1/100 秒。
    pub delay_cs: u32,
}

/// 只保存编辑参数；原始素材基线不进入撤销栈。
#[derive(Debug, Clone, PartialEq)]
struct Core {
    trim_start: u32,
    trim_end: u32,
    deleted: Runs,
    speed: f32,
    holds: Runs,
    loop_count: Option<u16>,
    mix: [AudioMix; 2],
}

impl Core {
    fn fresh(frame_count: u32) -> Self {
        let end = frame_count.saturating_sub(1);
        Self {
            trim_start: 0,
            trim_end: end,
            deleted: Runs::default(),
            speed: 1.0,
            holds: Runs::default(),
            loop_count: None,
            mix: [AudioMix::default(); 2],
        }
    }
}

/// Undo stores only changed edit parameters, never whole sparse states or material.
#[derive(Debug, Clone)]
struct ChangeSet {
    before: (u32, u32, f32, Option<u16>, [AudioMix; 2]),
    after: (u32, u32, f32, Option<u16>, [AudioMix; 2]),
    deleted: Vec<Change>,
    holds: Vec<Change>,
}
impl ChangeSet {
    fn between(a: &Core, b: &Core) -> Self {
        let scalar = |c: &Core| (c.trim_start, c.trim_end, c.speed, c.loop_count, c.mix);
        let deleted = a.deleted.changes(&b.deleted);
        let holds = a.holds.changes(&b.holds);
        Self {
            before: scalar(a),
            after: scalar(b),
            deleted,
            holds,
        }
    }
    fn apply(&self, core: &mut Core, forward: bool) {
        let (a, b, speed, count, mix) = if forward { self.after } else { self.before };
        core.trim_start = a;
        core.trim_end = b;
        core.speed = speed;
        core.loop_count = count;
        core.mix = mix;
        core.deleted.apply(&self.deleted, forward);
        core.holds.apply(&self.holds, forward);
    }

    fn bytes(&self) -> usize {
        128 + self.deleted.len() * std::mem::size_of::<Change>()
            + self.holds.len() * std::mem::size_of::<Change>()
    }
}

#[derive(Debug, Clone)]
struct PageStart {
    source: u32,
    play: usize,
    output_ms: u64,
    unscaled_ms: u64,
    source_ms: u64,
}
#[derive(Debug, Default)]
struct ViewIndex {
    pages: Vec<PageStart>,
    count: usize,
    total_ms: u64,
}

// Quantize the cumulative edited clock, not each 16/17ms source frame separately.
// The sparse directory stores both clocks so seeking and export use identical rounding.
fn scaled_duration(base: u32, speed: f32, unscaled: &mut u64, output: &mut u64) -> u32 {
    *unscaled = unscaled.saturating_add(base as u64);
    let target = (*unscaled as f64 / speed as f64).round() as u64;
    let duration = target.saturating_sub(*output).clamp(1, u32::MAX as u64) as u32;
    *output = output.saturating_add(duration as u64);
    duration
}

/// 时间轴（domain 纯状态；会话存储在 bin/infra 侧）。
#[derive(Debug, Clone)]
pub struct Timeline {
    frame_count: u32,
    core: Core,
    /// 不可变素材基线；克隆时间轴/撤销只共享基线，删除仅改变投影。
    origin_durs: Arc<blink_infra::gif::timing::Timings>,
    undo: Vec<ChangeSet>,
    redo: Vec<ChangeSet>,
    revision: u64,
    index: OnceLock<Arc<ViewIndex>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EditError {
    /// 帧号超出素材范围。
    FrameOutOfRange,
    /// 修剪终点早于起点。
    InvalidTrimRange,
    /// 草稿速度因子非法（非有限值或超出 clamp 范围）。
    InvalidSpeed,
    /// 草稿格式版本未知。
    InvalidDraftVersion,
    InvalidAudio,
}

impl std::fmt::Display for EditError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EditError::FrameOutOfRange => write!(f, "帧号超出素材范围"),
            EditError::InvalidTrimRange => write!(f, "修剪终点早于起点"),
            EditError::InvalidSpeed => write!(f, "速度因子非法"),
            EditError::InvalidDraftVersion => write!(f, "草稿格式版本不受支持"),
            EditError::InvalidAudio => write!(f, "音轨或音量无效"),
        }
    }
}

impl Timeline {
    /// 由素材原始时长表构建（录制 finalize 后产出）。
    pub fn from_durations(base_dur_ms: Vec<u32>) -> Self {
        Self::from_timings(blink_infra::gif::timing::Timings::memory(base_dur_ms))
    }
    pub fn from_timings(timings: blink_infra::gif::timing::Timings) -> Self {
        let frame_count = timings.len() as u32;
        Self {
            frame_count,
            core: Core::fresh(frame_count),
            origin_durs: Arc::new(timings),
            undo: Vec::new(),
            redo: Vec::new(),
            revision: 1,
            index: OnceLock::new(),
        }
    }

    pub fn frame_count(&self) -> u32 {
        self.frame_count
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    pub fn trim_range(&self) -> (u32, u32) {
        (self.core.trim_start, self.core.trim_end)
    }

    #[cfg(test)]
    fn deleted_frames(&self) -> Vec<u32> {
        self.core
            .deleted
            .ranges()
            .iter()
            .flat_map(|&(a, b, _)| a..=b)
            .collect()
    }

    pub fn speed(&self) -> f32 {
        self.core.speed
    }

    #[cfg(test)]
    fn holds(&self) -> &Runs {
        &self.core.holds
    }

    pub fn loop_count(&self) -> Option<u16> {
        self.core.loop_count
    }

    /// 应用编辑命令；成功返回是否产生实际变化，失败不改状态、不推进 revision。
    pub fn apply(&mut self, cmd: &EditCommand) -> Result<bool, EditError> {
        let next = self.apply_core(cmd)?;
        if next == self.core {
            return Ok(false);
        }
        self.undo.push(ChangeSet::between(&self.core, &next));
        self.prune_history();
        self.redo.clear();
        self.core = next;
        self.index = OnceLock::new();
        self.revision += 1;
        Ok(true)
    }

    pub fn undo(&mut self) -> bool {
        let Some(prev) = self.undo.pop() else {
            return false;
        };
        prev.apply(&mut self.core, false);
        self.redo.push(prev);
        self.index = OnceLock::new();
        self.revision += 1;
        true
    }

    pub fn redo(&mut self) -> bool {
        let Some(next) = self.redo.pop() else {
            return false;
        };
        next.apply(&mut self.core, true);
        self.undo.push(next);
        self.prune_history();
        self.index = OnceLock::new();
        self.revision += 1;
        true
    }

    /// 同时约束撤销步数和稀疏编辑状态，长会话不会按帧数复制基线。
    fn prune_history(&mut self) {
        let mut bytes = self.undo.iter().map(ChangeSet::bytes).sum::<usize>();
        while self.undo.len() > 128 || (bytes > 8 << 20 && self.undo.len() > 1) {
            bytes = bytes.saturating_sub(self.undo.remove(0).bytes());
        }
    }

    fn apply_core(&self, cmd: &EditCommand) -> Result<Core, EditError> {
        let mut c = self.core.clone();
        if let EditCommand::EditRanges {
            operation,
            ranges,
            ms,
        } = cmd
        {
            if ranges.len() > 256 || ranges.iter().any(|&(a, b)| a > b || b >= self.frame_count) {
                return Err(EditError::FrameOutOfRange);
            }
            for &(a, b) in ranges {
                match operation {
                    RangeOperation::Delete => c.deleted.set(a, b, Some(1)),
                    RangeOperation::Restore => c.deleted.set(a, b, None),
                    RangeOperation::Hold => {
                        c.holds
                            .set(a, b, (*ms > 0).then_some((*ms).min(MAX_DELAY_CS * 10)))
                    }
                }
            }
            return Ok(c);
        }
        // 含 Vec 字段的命令（DeleteFrames/RestoreFrames/SetHoldMany）不能经
        // `match *cmd` 按值解构——前置特判。
        if let EditCommand::DeleteFrames { frames } = cmd {
            if frames.iter().any(|f| *f >= self.frame_count) {
                return Err(EditError::FrameOutOfRange);
            }
            let mut changed = false;
            for frame in frames {
                if c.deleted.insert(*frame, 1) {
                    changed = true;
                }
            }
            return Ok(if changed { c } else { self.core.clone() }); // 全删幂等=原状态
        }
        if let EditCommand::RestoreFrames { frames } = cmd {
            if frames.iter().any(|f| *f >= self.frame_count) {
                return Err(EditError::FrameOutOfRange);
            }
            let mut changed = false;
            for frame in frames {
                // 时长从素材原始基线还原（删除时置 0）
                if c.deleted.remove(frame) {
                    changed = true;
                }
            }
            return Ok(if changed { c } else { self.core.clone() }); // 未删除的条目跳过
        }
        if let EditCommand::SetHoldMany { frames, ms } = cmd {
            if frames.iter().any(|f| *f >= self.frame_count) {
                return Err(EditError::FrameOutOfRange);
            }
            let clamped = (*ms).min(MAX_DELAY_CS * 10);
            let mut changed = false;
            for frame in frames {
                let next = if clamped == 0 { None } else { Some(clamped) };
                let cur = c.holds.get(frame).copied();
                if cur != next {
                    match next {
                        Some(v) => {
                            c.holds.insert(*frame, v);
                        }
                        None => {
                            c.holds.remove(frame);
                        }
                    }
                    changed = true;
                }
            }
            return Ok(if changed { c } else { self.core.clone() }); // 同值幂等
        }
        match *cmd {
            EditCommand::TrimStart { frame } => {
                if frame >= self.frame_count {
                    return Err(EditError::FrameOutOfRange);
                }
                if frame > c.trim_end {
                    return Err(EditError::InvalidTrimRange);
                }
                c.trim_start = frame;
            }
            EditCommand::TrimEnd { frame } => {
                if frame >= self.frame_count {
                    return Err(EditError::FrameOutOfRange);
                }
                if frame < c.trim_start {
                    return Err(EditError::InvalidTrimRange);
                }
                c.trim_end = frame;
            }
            EditCommand::DeleteFrame { frame } => {
                if frame >= self.frame_count {
                    return Err(EditError::FrameOutOfRange);
                }
                if !c.deleted.insert(frame, 1) {
                    return Ok(c); // 已删除：幂等，无变化
                }
                // 2026-10-26 语义定案（手测反馈）：删除帧 = 画面与时长一并移除
                // （总时长缩短）。旧「时长并入前一帧」语义已废弃——用户删帧期望
                // 该帧的显示时间消失，不是跳帧等待。
            }
            EditCommand::EditRanges { .. } => unreachable!("range command handled above"),
            EditCommand::DeleteFrames { .. } => {
                unreachable!("DeleteFrames 由 apply_core 前置特判处理")
            }
            EditCommand::RestoreFrames { .. } => {
                unreachable!("RestoreFrames 由 apply_core 前置特判处理")
            }
            EditCommand::SetHoldMany { .. } => {
                unreachable!("SetHoldMany 由 apply_core 前置特判处理")
            }
            EditCommand::SetSpeed { factor } => {
                if !factor.is_finite() {
                    return Err(EditError::InvalidSpeed);
                }
                c.speed = factor.clamp(0.25, 4.0);
            }
            EditCommand::SetHold { frame, ms } => {
                if frame >= self.frame_count {
                    return Err(EditError::FrameOutOfRange);
                }
                if ms == 0 {
                    c.holds.remove(&frame);
                } else {
                    c.holds.insert(frame, ms.min(MAX_DELAY_CS * 10));
                }
            }
            EditCommand::SetLoop { count } => {
                c.loop_count = count;
            }
            EditCommand::SetAudio {
                source,
                gain,
                muted,
            } => {
                if source > 1 || !gain.is_finite() || !(0.0..=2.0).contains(&gain) {
                    return Err(EditError::InvalidAudio);
                }
                c.mix[source] = AudioMix { gain, muted };
            }
            EditCommand::Reset => {
                // Reset 回到素材原始状态（含原始时长——删除帧置 0 的时长恢复）；
                // 本身可撤销（undo 栈回滚 Core 快照）。
                c = Core::fresh(self.frame_count);
            }
        }
        Ok(c)
    }

    /// 逐帧导出视图（应用全部编辑；dur 未量化，供前端预览播放）。
    pub fn effective_frames(&self) -> Vec<FrameView> {
        self.frame_views().collect()
    }
    fn frame_views(&self) -> impl Iterator<Item = FrameView> + '_ {
        self.frame_views_from(self.core.trim_start, 0, 0)
    }
    fn frame_views_from(
        &self,
        start: u32,
        unscaled_ms: u64,
        output_ms: u64,
    ) -> impl Iterator<Item = FrameView> + '_ {
        (start..self.frame_count.min(self.core.trim_end.saturating_add(1)))
            .filter(|i| {
                *i >= self.core.trim_start
                    && *i <= self.core.trim_end
                    && !self.core.deleted.contains(i)
            })
            .scan((unscaled_ms, output_ms), move |clock, i| {
                let base = self.origin_durs.at(i as usize).max(1);
                let held = self.core.holds.get(&i).copied();
                let dur = scaled_duration(
                    held.unwrap_or(base),
                    self.core.speed,
                    &mut clock.0,
                    &mut clock.1,
                );
                Some(FrameView {
                    src_idx: i,
                    dur_ms: dur,
                    held: held.is_some(),
                })
            })
    }

    /// 冻结导出计划（revision 快照 + delay 量化到 1/100s）。
    pub fn export_plan(&self, src_w: u32, src_h: u32) -> ExportPlan {
        self.export_snapshot(src_w, src_h).gif_plan()
    }

    pub fn export_snapshot(&self, src_w: u32, src_h: u32) -> ExportSnapshot {
        ExportSnapshot {
            revision: self.revision,
            src_w,
            src_h,
            frames: self.frozen_frames(),
            loop_count: self.core.loop_count,
            audio: self.audio_spans(),
            mix: self.core.mix,
        }
    }

    /// Page directory bounds lookup work; the closure shares the immutable baseline
    /// and freezes only sparse edit state, without retaining undo/redo history.
    fn frozen_frames(&self) -> FrameList<FrameView> {
        let directory = self.view_index().clone();
        let frozen = Self {
            frame_count: self.frame_count,
            core: self.core.clone(),
            origin_durs: self.origin_durs.clone(),
            undo: Vec::new(),
            redo: Vec::new(),
            revision: self.revision,
            index: OnceLock::from(directory.clone()),
        };
        let count = directory.count;
        let cache = std::sync::Mutex::new((usize::MAX, Vec::<FrameView>::new()));
        FrameList::generated(count, move |index| {
            let mut cache = cache.lock().unwrap_or_else(|e| e.into_inner());
            let page = index / 128;
            if cache.0 != page {
                cache.1 = frozen
                    .frame_views_from(
                        directory.pages[page].source,
                        directory.pages[page].unscaled_ms,
                        directory.pages[page].output_ms,
                    )
                    .take(128)
                    .collect();
                cache.0 = page;
            }
            cache.1[index % 128]
        })
    }

    pub fn mix(&self) -> [AudioMix; 2] {
        self.core.mix
    }

    /// 有界分页帧源契约：调用方只取可见页；0.28.25 可替换内部存储而不改 IPC。
    pub fn frame_page(&self, offset: usize, limit: usize) -> Vec<FrameView> {
        let frames = self.frozen_frames();
        (offset..frames.len().min(offset.saturating_add(limit.min(256))))
            .filter_map(|i| frames.get(i))
            .collect()
    }

    fn view_index(&self) -> &Arc<ViewIndex> {
        self.index.get_or_init(|| {
            let mut out = ViewIndex::default();
            let mut source_ms = self.origin_durs.start_ms();
            let mut unscaled_ms = 0u64;
            for i in 0..self.frame_count {
                if i >= self.core.trim_start
                    && i <= self.core.trim_end
                    && !self.core.deleted.contains(&i)
                {
                    if out.count % 128 == 0 {
                        out.pages.push(PageStart {
                            source: i,
                            play: out.count,
                            output_ms: out.total_ms,
                            unscaled_ms,
                            source_ms,
                        });
                    }
                    let base = self
                        .core
                        .holds
                        .get(&i)
                        .copied()
                        .unwrap_or(self.origin_durs.at(i as usize));
                    scaled_duration(
                        base.max(1),
                        self.core.speed,
                        &mut unscaled_ms,
                        &mut out.total_ms,
                    );
                    out.count += 1;
                }
                source_ms += self.origin_durs.at(i as usize) as u64;
            }
            Arc::new(out)
        })
    }
    pub fn effective_count(&self) -> usize {
        self.view_index().count
    }

    /// A sparse directory resolves arbitrary seeks with at most two metadata pages.
    pub fn window(
        &self,
        play: Option<usize>,
        time: Option<u64>,
        source: Option<u32>,
    ) -> serde_json::Value {
        let index = self.view_index();
        if index.count == 0 {
            return serde_json::json!({"revision":self.revision,"offset":0,"frames":[],"effectiveCount":0,"totalMs":0});
        }
        let page_index = if let Some(play) = play {
            index.pages.partition_point(|p| p.play <= play)
        } else if let Some(time) = time {
            index.pages.partition_point(|p| p.output_ms <= time)
        } else if let Some(source) = source {
            index.pages.partition_point(|p| p.source <= source)
        } else {
            1
        };
        let p = &index.pages[page_index.saturating_sub(1).min(index.pages.len() - 1)];
        let mut at = p.output_ms;
        let mut unscaled_at = p.unscaled_ms;
        let mut source_at = p.source_ms;
        let mut cursor = p.source;
        let mut selected = None;
        let mut frames = Vec::new();
        for (relative, f) in self
            .frame_views_from(p.source, p.unscaled_ms, p.output_ms)
            .enumerate()
        {
            let i = p.play + relative;
            while cursor < f.src_idx {
                source_at += self.origin_durs.at(cursor as usize) as u64;
                cursor += 1;
            }
            let target = play.is_some_and(|v| i >= v)
                || time.is_some_and(|v| at + f.dur_ms as u64 > v)
                || source.is_some_and(|v| f.src_idx >= v)
                || (play.is_none() && time.is_none() && source.is_none());
            if selected.is_none() && (target || i + 1 == index.count) {
                selected = Some(i);
            }
            if selected.is_some() {
                let original = self.origin_durs.at(f.src_idx as usize);
                let hold = self.core.holds.get(&f.src_idx).copied().unwrap_or(original);
                frames.push(serde_json::json!({"playIdx":i,"srcIdx":f.src_idx,"startMs":at,"durMs":f.dur_ms,"held":f.held,
                    "sourceMs":source_at,"soundMs":original.min(hold),"silenceMs":hold.saturating_sub(original),
                    "speed":self.core.speed,"audioKey":format!("{}:{}",self.revision,source_at as i128 - unscaled_at as i128)}));
                if frames.len() == 128 {
                    break;
                }
            }
            at += f.dur_ms as u64;
            unscaled_at += self
                .core
                .holds
                .get(&f.src_idx)
                .copied()
                .unwrap_or(self.origin_durs.at(f.src_idx as usize))
                .max(1) as u64;
        }
        serde_json::json!({"revision":self.revision,"offset":selected.unwrap_or(0),"frames":frames,"effectiveCount":index.count,"totalMs":index.total_ms})
    }

    /// 选区摘要直接查询压缩时长区间，不展开长录制的逐帧表。
    pub fn selection_summary(&self, ranges: &[(u32, u32)]) -> serde_json::Value {
        let holds = self.core.holds.ranges();
        let has_hold = ranges.iter().any(|&(a, b)| {
            let first = holds.partition_point(|r| r.1 < a);
            holds.get(first).is_some_and(|r| r.0 <= b)
        });
        let duration = match ranges {
            &[(a, b)] if a == b && a < self.frame_count => Some(
                self.core
                    .holds
                    .get(&a)
                    .copied()
                    .unwrap_or(self.origin_durs.at(a as usize)),
            ),
            _ => None,
        };
        serde_json::json!({"revision":self.revision,"hasHold":has_hold,"durationMs":duration})
    }

    pub fn source_page(&self, offset: u32, limit: u32) -> serde_json::Value {
        let end = offset.saturating_add(limit.min(256)).min(self.frame_count);
        let frames: Vec<_> = (offset..end).map(|i| {
            let base = self.origin_durs.at(i as usize);
            let hold = self.core.holds.get(&i).copied();
            let in_trim = i >= self.core.trim_start && i <= self.core.trim_end;
            serde_json::json!({"srcIdx": i, "deleted": self.core.deleted.contains(&i), "inTrim": in_trim,
                "held": hold.is_some(), "holdMs": hold, "baseMs": base,
                "durMs": ((hold.unwrap_or(base) as f64 / self.core.speed as f64).round().max(1.0) as u32)})
        }).collect();
        serde_json::json!({"revision":self.revision,"frames":frames})
    }

    pub fn audio_spans(&self) -> Vec<AudioSpan> {
        let frames = self.frame_views();
        let mut source_at = self.origin_durs.start_ms();
        let mut cursor = 0usize;
        let mut out: Vec<AudioSpan> = Vec::new();
        for f in frames {
            while cursor < f.src_idx as usize {
                source_at += self.origin_durs.at(cursor) as u64;
                cursor += 1;
            }
            let original = self.origin_durs.at(f.src_idx as usize) as u64;
            let held = self
                .core
                .holds
                .get(&f.src_idx)
                .copied()
                .map(u64::from)
                .unwrap_or(original);
            let span = AudioSpan {
                source_ms: source_at,
                sound_ms: original.min(held),
                silence_ms: held.saturating_sub(original),
                output_ms: f.dur_ms as u64,
                speed: self.core.speed,
            };
            if let Some(last) = out.last_mut().filter(|last| {
                last.silence_ms == 0
                    && span.silence_ms == 0
                    && last.source_ms + last.sound_ms == span.source_ms
            }) {
                last.sound_ms += span.sound_ms;
                last.output_ms += span.output_ms;
            } else {
                out.push(span);
            }
        }
        out
    }

    /// 有效总时长（ms，未量化）。
    pub fn effective_total_ms(&self) -> u64 {
        self.view_index().total_ms
    }

    // ── 版本化编辑草稿（0.28.23 §5.7.4）────────────────────────────────────
    //
    // 保存/恢复非破坏编辑状态与播放位置。undo/redo 栈**不持久化**——恢复后
    // 栈为空（不可撤销到恢复前），编辑语义全部保留。素材身份（帧数/创建时间）
    // 由调用方（app 层草稿文件）附加校验，domain 只校验帧号范围。

    /// 导出当前编辑状态为草稿（`play_pos_ms` 为前端播放头位置）。
    pub fn to_draft(&self, play_pos_ms: u64) -> TimelineDraft {
        let deleted_ranges = self
            .core
            .deleted
            .ranges()
            .iter()
            .map(|&(a, b, _)| (a, b))
            .collect();
        let hold_ranges = self.core.holds.ranges().to_vec();
        TimelineDraft {
            version: TIMELINE_DRAFT_VERSION,
            trim_start: self.core.trim_start,
            trim_end: self.core.trim_end,
            deleted: Vec::new(),
            deleted_ranges,
            speed: self.core.speed,
            holds: Vec::new(),
            hold_ranges,
            loop_count: self.core.loop_count,
            revision: self.revision,
            play_pos_ms,
            mix: self.core.mix,
        }
    }

    /// 恢复草稿：帧号越界/修剪区间非法时拒绝且不改状态（素材已变化的调用方
    /// 应丢弃草稿并如实提示，不能声称恢复全部编辑）。成功后 undo/redo 栈为空，
    /// revision 沿用草稿保存值（前端按 revision 拒绝旧回包的语义继续成立）。
    pub fn restore_draft(&mut self, draft: TimelineDraft) -> Result<(), EditError> {
        if !(1..=TIMELINE_DRAFT_VERSION).contains(&draft.version) {
            return Err(EditError::InvalidDraftVersion);
        }
        let end = self.frame_count.saturating_sub(1);
        if (self.frame_count == 0
            && (!draft.deleted.is_empty()
                || !draft.holds.is_empty()
                || !draft.deleted_ranges.is_empty()
                || !draft.hold_ranges.is_empty()))
            || draft.trim_start > end
            || draft.trim_end > end
            || draft.trim_start > draft.trim_end
            || draft.deleted.iter().any(|&f| f > end)
            || draft.holds.iter().any(|&(f, _)| f > end)
            || draft.deleted_ranges.iter().any(|&(a, b)| a > b || b > end)
            || draft
                .hold_ranges
                .iter()
                .any(|&(a, b, ms)| a > b || b > end || ms > MAX_DELAY_CS * 10)
        {
            return Err(EditError::FrameOutOfRange);
        }
        if !draft.speed.is_finite() || !(0.25..=4.0).contains(&draft.speed) {
            return Err(EditError::InvalidSpeed);
        }
        if draft
            .mix
            .iter()
            .any(|m| !m.gain.is_finite() || !(0.0..=2.0).contains(&m.gain))
        {
            return Err(EditError::InvalidAudio);
        }
        let mut deleted = Runs::default();
        for &i in &draft.deleted {
            deleted.insert(i, 1);
        }
        for &(a, b) in &draft.deleted_ranges {
            deleted.set(a, b, Some(1));
        }
        let mut holds = Runs::default();
        for &(i, ms) in &draft.holds {
            holds.insert(i, ms.min(MAX_DELAY_CS * 10));
        }
        for &(a, b, ms) in &draft.hold_ranges {
            holds.set(a, b, Some(ms));
        }
        self.core = Core {
            trim_start: draft.trim_start,
            trim_end: draft.trim_end,
            deleted,
            speed: draft.speed,
            holds,
            loop_count: draft.loop_count,
            mix: draft.mix,
        };
        self.undo.clear();
        self.redo.clear();
        self.revision = draft.revision.max(1);
        self.index = OnceLock::new();
        Ok(())
    }
}

/// 版本化编辑草稿（`sessions/<id>/draft.json` 的编辑态部分；0.28.23 §5.7.4）。
///
/// `holds` 用 `Vec<(u32, u32)>` 而非 map——serde 会把非 u32 键的 BTreeMap
/// 序列化成字符串键，恢复端再解析回数字；显式元组数组避免该歧义。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TimelineDraft {
    /// 草稿格式版本（不兼容时整体丢弃并提示，不逐字段猜测）。
    pub version: u32,
    pub trim_start: u32,
    pub trim_end: u32,
    pub deleted: Vec<u32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deleted_ranges: Vec<(u32, u32)>,
    pub speed: f32,
    pub holds: Vec<(u32, u32)>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hold_ranges: Vec<(u32, u32, u32)>,
    pub loop_count: Option<u16>,
    /// 保存时的 timeline revision（恢复后沿用）。
    pub revision: u64,
    /// 播放头位置（编辑后有效时间轴，ms）。
    pub play_pos_ms: u64,
    #[serde(default)]
    pub mix: [AudioMix; 2],
}

pub const TIMELINE_DRAFT_VERSION: u32 = 2;

/// 草稿文件整体（`sessions/<id>/draft.json`）：编辑态 + 素材身份。
///
/// 身份不匹配（素材变化/串目录）时整体作废——不逐字段猜测半恢复；
/// 由调用方如实提示"已回退到原始素材"，不宣称恢复了编辑进度。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionDraftFile {
    pub version: u32,
    /// 素材创建时间（`session.json` 的 `created_at`）。
    pub session_created_at: u64,
    /// 素材原始帧数（索引帧数，与构建 Timeline 的输入一致）。
    pub material_frames: u32,
    pub draft: TimelineDraft,
}

impl SessionDraftFile {
    /// 素材身份匹配判定（恢复前置校验）。
    pub fn identity_matches(&self, created_at: u64, material_frames: u32) -> bool {
        (1..=TIMELINE_DRAFT_VERSION).contains(&self.version)
            && self.session_created_at == created_at
            && self.material_frames == material_frames
    }
}

/// ms → 1/100s 量化，clamp [MIN_DELAY_CS, MAX_DELAY_CS]。
pub fn quantize_delay_ms(ms: u32) -> u32 {
    (ms / 10).max(MIN_DELAY_CS)
}

// ── 测试 ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selection_duration_summary_tracks_holds_reset_and_undo_without_expanding_ranges() {
        let mut t = Timeline::from_durations(vec![17; 100_000]);
        assert_eq!(t.selection_summary(&[(99_999, 99_999)])["durationMs"], 17);
        assert_eq!(t.selection_summary(&[(0, 99_999)])["hasHold"], false);
        t.apply(&EditCommand::EditRanges {
            operation: RangeOperation::Hold,
            ranges: vec![(50_000, 99_999)],
            ms: 800,
        })
        .unwrap();
        assert_eq!(t.selection_summary(&[(1, 49_999)])["hasHold"], false);
        assert_eq!(t.selection_summary(&[(0, 99_999)])["hasHold"], true);
        assert_eq!(t.selection_summary(&[(99_999, 99_999)])["durationMs"], 800);
        assert!(t.selection_summary(&[(0, 99_999)])["durationMs"].is_null());
        t.apply(&EditCommand::EditRanges {
            operation: RangeOperation::Hold,
            ranges: vec![(50_000, 99_999)],
            ms: 0,
        })
        .unwrap();
        assert_eq!(t.selection_summary(&[(99_999, 99_999)])["durationMs"], 17);
        assert_eq!(t.selection_summary(&[(0, 99_999)])["hasHold"], false);
        assert!(t.undo());
        assert_eq!(
            t.selection_summary(&[(0, 0), (99_999, 99_999)])["hasHold"],
            true
        );
        assert_eq!(t.selection_summary(&[(99_999, 99_999)])["durationMs"], 800);
    }

    #[test]
    fn draft_restore_rejects_unknown_version_and_empty_material_indices() {
        let mut timeline = Timeline::from_durations(vec![100; 2]);
        let mut draft = timeline.to_draft(0);
        draft.version += 1;
        assert_eq!(
            timeline.restore_draft(draft),
            Err(EditError::InvalidDraftVersion)
        );
        assert_eq!(timeline.revision(), 1);

        let mut empty = Timeline::from_durations(vec![]);
        let mut draft = empty.to_draft(0);
        draft.deleted.push(0);
        assert_eq!(empty.restore_draft(draft), Err(EditError::FrameOutOfRange));
        assert_eq!(empty.frame_count(), 0);
    }

    #[test]
    fn audio_continuity_keys_only_change_at_edit_discontinuities() {
        let mut t = Timeline::from_durations(vec![16, 17, 17, 16, 17]);
        t.apply(&EditCommand::SetSpeed { factor: 1.5 }).unwrap();
        let page = t.window(None, None, None);
        assert_eq!(page["frames"][0]["audioKey"], page["frames"][4]["audioKey"]);
        assert_eq!(page["frames"][1]["speed"], 1.5);
        t.apply(&EditCommand::DeleteFrame { frame: 1 }).unwrap();
        let page = t.window(None, None, None);
        assert_ne!(page["frames"][0]["audioKey"], page["frames"][1]["audioKey"]);
        assert_eq!(page["frames"][1]["audioKey"], page["frames"][3]["audioKey"]);
        t.apply(&EditCommand::SetHold { frame: 2, ms: 100 })
            .unwrap();
        let page = t.window(None, None, None);
        assert_ne!(page["frames"][1]["audioKey"], page["frames"][2]["audioKey"]);
    }

    #[test]
    fn long_speed_changes_keep_cumulative_clock_across_page_boundaries() {
        let mut t = Timeline::from_durations(
            (0..108000u64)
                .map(|i| ((i + 1) * 1000 / 60 - i * 1000 / 60) as u32)
                .collect(),
        );
        for speed in [0.25, 1.5, 2.0, 4.0] {
            t.apply(&EditCommand::SetSpeed { factor: speed }).unwrap();
            let expected = (1_800_000f64 / speed as f64).round() as u64;
            assert_eq!(t.effective_total_ms(), expected);
            let snapshot = t.export_snapshot(1920, 1080);
            assert_eq!(
                snapshot.frames.iter().map(|f| f.dur_ms as u64).sum::<u64>(),
                expected
            );
            assert_eq!(
                snapshot.audio.iter().map(|f| f.output_ms).sum::<u64>(),
                expected
            );
            for play in [127usize, 128, 255, 256, 107999] {
                let frame = t.window(Some(play), None, None);
                assert_eq!(
                    frame["frames"][0]["durMs"],
                    snapshot.frames.get(play).unwrap().dur_ms
                );
            }
        }
        t.apply(&EditCommand::EditRanges {
            operation: RangeOperation::Delete,
            ranges: vec![(1, 107998)],
            ms: 0,
        })
        .unwrap();
        assert_eq!(t.core.deleted.ranges().len(), 1);
        assert!(t.undo.last().unwrap().bytes() < 256);
        assert_eq!(t.to_draft(0).deleted_ranges, vec![(1, 107998)]);
        assert!(t.undo());
        assert!(t.redo());
    }

    #[test]
    fn long_windows_ranges_and_snapshots_keep_clock_revision_and_undo() {
        let mut timeline = Timeline::from_durations(
            (0..108_000u64)
                .map(|i| ((i + 1) * 1000 / 60 - i * 1000 / 60) as u32)
                .collect(),
        );
        assert_eq!(timeline.effective_total_ms(), 1_800_000);
        let frozen = timeline.export_snapshot(1920, 1080);
        assert_eq!(frozen.frames.len(), 108_000);
        timeline
            .apply(&EditCommand::EditRanges {
                operation: RangeOperation::Delete,
                ranges: vec![(0, 999)],
                ms: 0,
            })
            .unwrap();
        assert_eq!(timeline.effective_count(), 107_000);
        assert_eq!(frozen.frames.get(0).unwrap().src_idx, 0);
        assert_eq!(timeline.frame_page(0, 1)[0].src_idx, 1000);
        let end = timeline.effective_total_ms();
        let window = timeline.window(None, Some(end), None);
        assert_eq!(window["frames"][0]["srcIdx"], 107_999);
        assert_eq!(window["frames"].as_array().unwrap().len(), 1);
        assert!(timeline.undo());
        assert_eq!(timeline.effective_total_ms(), 1_800_000);
        assert!(timeline.redo());
        assert_eq!(timeline.effective_count(), 107_000);
        let revision = timeline.revision();
        assert!(
            timeline
                .apply(&EditCommand::EditRanges {
                    operation: RangeOperation::Delete,
                    ranges: vec![(1, 2), (3, 108000)],
                    ms: 0
                })
                .is_err()
        );
        assert_eq!(timeline.revision(), revision);
        for n in 0..200 {
            timeline
                .apply(&EditCommand::SetSpeed {
                    factor: if n % 2 == 0 { 1.5 } else { 1.0 },
                })
                .unwrap();
        }
        assert!(timeline.undo.len() <= 128);
        assert!(Arc::ptr_eq(
            &timeline.origin_durs,
            &timeline.clone().origin_durs
        ));
    }

    fn tl() -> Timeline {
        Timeline::from_durations(vec![100, 100, 100, 100, 100])
    }

    #[test]
    fn capture_origin_is_retained_through_pages_trim_hold_speed_and_export() {
        let timings = blink_infra::gif::timing::Timings::memory(vec![100; 300]).with_start_ms(150);
        let mut timeline = Timeline::from_timings(timings);
        assert_eq!(
            timeline.window(None, Some(12900), None)["frames"][0]["sourceMs"],
            13050
        );
        assert_eq!(timeline.audio_spans()[0].source_ms, 150);
        assert_eq!(timeline.audio_spans()[0].sound_ms, 30000);
        timeline
            .apply(&EditCommand::TrimStart { frame: 129 })
            .unwrap();
        timeline
            .apply(&EditCommand::DeleteFrame { frame: 130 })
            .unwrap();
        timeline
            .apply(&EditCommand::SetHold {
                frame: 129,
                ms: 200,
            })
            .unwrap();
        timeline
            .apply(&EditCommand::SetSpeed { factor: 2.0 })
            .unwrap();
        let first = &timeline.window(Some(0), None, None)["frames"][0];
        assert_eq!(first["startMs"], 0);
        assert_eq!(first["sourceMs"], 13050);
        assert_eq!(first["durMs"], 100);
        let snapshot = timeline.export_snapshot(64, 64);
        assert_eq!(
            snapshot.audio[0],
            AudioSpan {
                source_ms: 13050,
                sound_ms: 100,
                silence_ms: 100,
                output_ms: 100,
                speed: 2.0
            }
        );
        assert_eq!(snapshot.audio[1].source_ms, 13250);
        assert_eq!(
            snapshot.audio.last().unwrap().source_ms + snapshot.audio.last().unwrap().sound_ms,
            30150
        );
    }

    #[test]
    fn fresh_timeline_is_all_frames_at_unit_speed() {
        let t = tl();
        assert_eq!(t.frame_count(), 5);
        let v = t.effective_frames();
        assert_eq!(v.len(), 5);
        assert_eq!(v[0].src_idx, 0);
        assert_eq!(v[0].dur_ms, 100);
        assert_eq!(t.effective_total_ms(), 500);
        assert_eq!(t.revision(), 1);
    }

    #[test]
    fn empty_recording_has_no_export_frames_instead_of_indexing_frame_zero() {
        let timeline = Timeline::from_durations(Vec::new());
        assert!(timeline.effective_frames().is_empty());
        assert_eq!(timeline.effective_total_ms(), 0);
        assert!(timeline.export_snapshot(640, 480).frames.is_empty());
        assert!(timeline.export_plan(640, 480).frames.is_empty());
    }

    #[test]
    fn trim_bounds_output_range() {
        let mut t = tl();
        t.apply(&EditCommand::TrimStart { frame: 1 }).unwrap();
        t.apply(&EditCommand::TrimEnd { frame: 3 }).unwrap();
        let v: Vec<u32> = t.effective_frames().iter().map(|f| f.src_idx).collect();
        assert_eq!(v, vec![1, 2, 3]);
        // 终点早于起点拒绝，状态不变
        let before = t.revision();
        assert_eq!(
            t.apply(&EditCommand::TrimEnd { frame: 0 }),
            Err(EditError::InvalidTrimRange)
        );
        assert_eq!(t.revision(), before);
    }

    #[test]
    fn out_of_range_frame_rejected() {
        let mut t = tl();
        assert_eq!(
            t.apply(&EditCommand::DeleteFrame { frame: 5 }),
            Err(EditError::FrameOutOfRange)
        );
        assert_eq!(
            t.apply(&EditCommand::SetHold { frame: 9, ms: 500 }),
            Err(EditError::FrameOutOfRange)
        );
    }

    #[test]
    fn delete_frame_removes_its_duration() {
        // 2026-10-26 语义定案：删帧 = 画面与时长一并移除（总时长缩短）
        let mut t = tl();
        t.apply(&EditCommand::DeleteFrame { frame: 2 }).unwrap();
        let v = t.effective_frames();
        assert_eq!(v.len(), 4);
        assert_eq!(v[1].src_idx, 1);
        assert_eq!(v[1].dur_ms, 100, "前一帧时长不变（不吸收）");
        assert_eq!(t.effective_total_ms(), 400, "总时长缩短被删帧的 100ms");
    }

    #[test]
    fn delete_frames_batch_single_undo_granularity() {
        let mut t = tl();
        let rev = t.revision();
        t.apply(&EditCommand::DeleteFrames { frames: vec![1, 3] })
            .unwrap();
        assert_eq!(t.revision(), rev + 1, "批量删除单命令推进一次 revision");
        let v = t.effective_frames();
        let idx: Vec<u32> = v.iter().map(|f| f.src_idx).collect();
        assert_eq!(idx, vec![0, 2, 4]);
        assert_eq!(t.effective_total_ms(), 300);
        // 一次 undo 恢复全部
        t.undo();
        assert_eq!(t.effective_frames().len(), 5);
        // 越界整条拒绝
        assert_eq!(
            t.apply(&EditCommand::DeleteFrames { frames: vec![1, 9] }),
            Err(EditError::FrameOutOfRange)
        );
        assert_eq!(t.effective_frames().len(), 5, "拒绝后状态不变");
        // 全部已删幂等：不推进 revision
        t.apply(&EditCommand::DeleteFrames { frames: vec![1, 3] })
            .unwrap();
        let rev = t.revision();
        assert_eq!(
            t.apply(&EditCommand::DeleteFrames { frames: vec![1, 3] }),
            Ok(false)
        );
        assert_eq!(t.revision(), rev);
    }

    #[test]
    fn delete_idempotent_no_revision_churn() {
        let mut t = tl();
        t.apply(&EditCommand::DeleteFrame { frame: 2 }).unwrap();
        let rev = t.revision();
        assert_eq!(t.apply(&EditCommand::DeleteFrame { frame: 2 }), Ok(false));
        assert_eq!(t.revision(), rev, "重复删除不推进 revision");
    }

    #[test]
    fn restore_frames_recovers_duration_single_undo() {
        let mut t = tl();
        t.apply(&EditCommand::DeleteFrames { frames: vec![1, 3] })
            .unwrap();
        // 恢复 1（3 保持删除）
        t.apply(&EditCommand::RestoreFrames { frames: vec![1] })
            .unwrap();
        let idx: Vec<u32> = t.effective_frames().iter().map(|f| f.src_idx).collect();
        assert_eq!(idx, vec![0, 1, 2, 4]);
        assert_eq!(t.effective_total_ms(), 400, "恢复帧时长从素材基线还原");
        // 全未删除的恢复 = 幂等，不推进 revision
        let rev = t.revision();
        assert_eq!(
            t.apply(&EditCommand::RestoreFrames { frames: vec![1, 2] }),
            Ok(false)
        );
        assert_eq!(t.revision(), rev);
        // 一次 undo 回到「1、3 均删除」
        t.undo();
        let idx: Vec<u32> = t.effective_frames().iter().map(|f| f.src_idx).collect();
        assert_eq!(idx, vec![0, 2, 4]);
        // 越界整条拒绝
        assert_eq!(
            t.apply(&EditCommand::RestoreFrames { frames: vec![1, 9] }),
            Err(EditError::FrameOutOfRange)
        );
    }

    #[test]
    fn set_hold_many_batch_single_undo_and_clear() {
        let mut t = tl();
        let rev = t.revision();
        t.apply(&EditCommand::SetHoldMany {
            frames: vec![0, 2, 4],
            ms: 300,
        })
        .unwrap();
        assert_eq!(t.revision(), rev + 1, "批量同值单命令推进一次 revision");
        let v = t.effective_frames();
        for i in [0, 2, 4] {
            assert_eq!(v[i].dur_ms, 300);
            assert!(v[i].held);
        }
        assert_eq!(v[1].dur_ms, 100, "未选中帧不受影响");
        // 同值幂等
        assert_eq!(
            t.apply(&EditCommand::SetHoldMany {
                frames: vec![0, 2],
                ms: 300
            }),
            Ok(false)
        );
        // ms=0 批量清除 → 回速度计算
        t.apply(&EditCommand::SetHoldMany {
            frames: vec![0, 2, 4],
            ms: 0,
        })
        .unwrap();
        assert!(t.effective_frames().iter().all(|f| !f.held));
        // 一次 undo 回到「0/2/4 hold 300」（帧 1/3 从未被 hold）
        t.undo();
        let v = t.effective_frames();
        for i in [0, 2, 4] {
            assert_eq!(v[i].dur_ms, 300);
            assert!(v[i].held);
        }
        assert_eq!(v[1].dur_ms, 100);
        // 越界整条拒绝
        assert_eq!(
            t.apply(&EditCommand::SetHoldMany {
                frames: vec![0, 9],
                ms: 100
            }),
            Err(EditError::FrameOutOfRange)
        );
        // 超 clamp 收敛到上限（MAX_DELAY_CS*10 = 6000ms）
        t.apply(&EditCommand::SetHoldMany {
            frames: vec![1],
            ms: 99_999,
        })
        .unwrap();
        assert_eq!(t.effective_frames()[1].dur_ms, 6000);
    }

    #[test]
    fn speed_and_hold_apply_with_clamps() {
        let mut t = tl();
        t.apply(&EditCommand::SetSpeed { factor: 2.0 }).unwrap();
        assert_eq!(t.effective_frames()[0].dur_ms, 50);
        // 超 clamp
        t.apply(&EditCommand::SetSpeed { factor: 99.0 }).unwrap();
        assert_eq!(t.speed(), 4.0);
        t.apply(&EditCommand::SetSpeed { factor: 0.01 }).unwrap();
        assert_eq!(t.speed(), 0.25);
        // hold 覆盖该帧
        t.apply(&EditCommand::SetHold { frame: 1, ms: 800 })
            .unwrap();
        let v = t.effective_frames();
        assert_eq!(v[1].dur_ms, 3200);
        assert!(v[1].held);
        // 清除 hold 恢复速度计算
        t.apply(&EditCommand::SetHold { frame: 1, ms: 0 }).unwrap();
        let v = t.effective_frames();
        assert_eq!(v[1].dur_ms, 400); // 100/0.25
        assert!(!v[1].held);
    }

    #[test]
    fn undo_redo_roundtrip_and_revision_monotonic() {
        let mut t = tl();
        t.apply(&EditCommand::SetSpeed { factor: 2.0 }).unwrap();
        t.apply(&EditCommand::DeleteFrame { frame: 3 }).unwrap();
        let mid = t.effective_frames();
        assert_eq!(mid.len(), 4);
        t.undo();
        assert_eq!(t.effective_frames().len(), 5, "撤销删帧");
        t.undo();
        assert_eq!(t.effective_frames()[0].dur_ms, 100, "撤销速度");
        assert!(t.can_redo());
        t.redo();
        assert_eq!(t.effective_frames()[0].dur_ms, 50);
        t.redo();
        assert_eq!(t.effective_frames().len(), 4);
        // undo/redo 后新命令清空 redo 栈
        t.undo();
        t.apply(&EditCommand::SetSpeed { factor: 1.0 }).unwrap();
        assert!(!t.can_redo());
    }

    #[test]
    fn reset_restores_original_durations() {
        let mut t = tl();
        t.apply(&EditCommand::DeleteFrame { frame: 2 }).unwrap();
        t.apply(&EditCommand::SetSpeed { factor: 2.0 }).unwrap();
        t.apply(&EditCommand::Reset).unwrap();
        let v = t.effective_frames();
        // reset 回到素材原始状态：全部帧恢复、时长恢复 100、速度 1.0
        assert_eq!(v.len(), 5);
        assert!(v.iter().all(|f| f.dur_ms == 100));
        assert_eq!(t.effective_total_ms(), 500);
        // reset 可撤销回删除态
        t.undo();
        assert_eq!(t.effective_frames().len(), 4);
    }

    #[test]
    fn export_plan_freezes_revision_and_quantizes() {
        let mut t = tl();
        t.apply(&EditCommand::SetSpeed { factor: 1.5 }).unwrap();
        t.apply(&EditCommand::SetLoop { count: Some(3) }).unwrap();
        let plan = t.export_plan(800, 600);
        assert_eq!(plan.revision, t.revision());
        assert_eq!(plan.src_w, 800);
        // 100/1.5 = 66.67ms → 66/10 = 6.6 → 量化 6 → 但 clamp 下限 2 不触发
        assert_eq!(plan.frames.get(0).unwrap().delay_cs, 6);
        assert_eq!(plan.loop_count, Some(3));
        // 编辑后原 plan 不变（快照语义）
        t.apply(&EditCommand::SetSpeed { factor: 4.0 }).unwrap();
        assert_eq!(plan.frames.get(0).unwrap().delay_cs, 6);
        assert_ne!(t.export_plan(800, 600).revision, plan.revision);
    }

    #[test]
    fn neutral_snapshot_preserves_milliseconds_and_is_frozen() {
        let mut t = Timeline::from_durations(vec![33, 33, 33, 33]);
        t.apply(&EditCommand::DeleteFrame { frame: 1 }).unwrap();
        t.apply(&EditCommand::SetSpeed { factor: 1.5 }).unwrap();
        t.apply(&EditCommand::SetHold { frame: 2, ms: 777 })
            .unwrap();
        t.apply(&EditCommand::SetLoop { count: Some(3) }).unwrap();
        let snapshot = t.export_snapshot(65, 67);
        assert_eq!(
            snapshot
                .frames
                .iter()
                .map(|f| (f.src_idx, f.dur_ms))
                .collect::<Vec<_>>(),
            vec![(0, 22), (2, 518), (3, 22)]
        );
        assert_eq!(snapshot.gif_plan(), t.export_plan(65, 67));
        assert_eq!(snapshot.gif_plan().frames.get(1).unwrap().delay_cs, 52);
        t.apply(&EditCommand::Reset).unwrap();
        assert_eq!(snapshot.frames.get(1).unwrap().dur_ms, 518);
        assert_ne!(snapshot.revision, t.revision());
    }

    #[test]
    fn quantize_keeps_browser_minimum_without_truncating_long_material() {
        assert_eq!(quantize_delay_ms(0), MIN_DELAY_CS);
        assert_eq!(quantize_delay_ms(5), MIN_DELAY_CS, "20ms 以下抬到 20ms");
        assert_eq!(quantize_delay_ms(30), 3);
        assert_eq!(quantize_delay_ms(u32::MAX), u32::MAX / 10);
    }

    #[test]
    fn serde_edit_command_roundtrip() {
        let cmds = vec![
            EditCommand::TrimStart { frame: 1 },
            EditCommand::TrimEnd { frame: 4 },
            EditCommand::DeleteFrame { frame: 2 },
            EditCommand::DeleteFrames { frames: vec![1, 2] },
            EditCommand::SetSpeed { factor: 1.5 },
            EditCommand::SetHold { frame: 0, ms: 300 },
            EditCommand::SetLoop { count: None },
            EditCommand::Reset,
        ];
        for cmd in cmds {
            let json = serde_json::to_string(&cmd).unwrap();
            let back: EditCommand = serde_json::from_str(&json).unwrap();
            assert_eq!(back, cmd);
        }
        assert!(serde_json::from_str::<EditCommand>(r#"{"type":"nope"}"#).is_err());
    }

    // ── 版本化编辑草稿（0.28.23）────────────────────────────────────────────

    #[test]
    fn draft_roundtrip_preserves_all_edit_semantics() {
        let mut t = tl();
        t.apply(&EditCommand::TrimStart { frame: 1 }).unwrap();
        t.apply(&EditCommand::DeleteFrame { frame: 3 }).unwrap();
        t.apply(&EditCommand::SetSpeed { factor: 2.0 }).unwrap();
        t.apply(&EditCommand::SetHold { frame: 2, ms: 1500 })
            .unwrap();
        t.apply(&EditCommand::SetLoop { count: Some(3) }).unwrap();
        let before = t.to_draft(1234);

        // 序列化/反序列化（文件往返）
        let json = serde_json::to_string(&before).unwrap();
        let parsed: TimelineDraft = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, before);
        assert_eq!(parsed.version, TIMELINE_DRAFT_VERSION);
        // holds 是元组数组（数字键），不是字符串键 map
        assert!(json.contains("\"holdRanges\":[[2,2,1500]]"));

        // 恢复到全新时间轴：编辑投影一致
        let mut restored = tl();
        restored.restore_draft(parsed).unwrap();
        assert_eq!(restored.trim_range(), t.trim_range());
        assert_eq!(restored.deleted_frames(), t.deleted_frames());
        assert_eq!(restored.speed(), t.speed());
        assert_eq!(restored.holds(), t.holds());
        assert_eq!(restored.loop_count(), t.loop_count());
        assert_eq!(restored.effective_total_ms(), t.effective_total_ms());
        // revision 沿用草稿值；undo 栈为空（栈不持久化）
        assert_eq!(restored.revision(), t.revision());
        assert!(!restored.can_undo());
        assert!(!restored.can_redo());
    }

    #[test]
    fn draft_restore_rejects_frame_indices_out_of_smaller_material() {
        let mut t = tl();
        t.apply(&EditCommand::DeleteFrame { frame: 4 }).unwrap();
        let draft = t.to_draft(0);
        // 素材只剩 3 帧（如另一段素材）：帧号越界整体拒绝且状态不变
        let mut smaller = Timeline::from_durations(vec![100, 100, 100]);
        assert_eq!(
            smaller.restore_draft(draft),
            Err(EditError::FrameOutOfRange)
        );
        assert_eq!(smaller.revision(), 1);
        assert!(smaller.deleted_frames().is_empty());
    }

    #[test]
    fn draft_restore_rejects_invalid_trim_and_speed_without_mutation() {
        let mk = |speed: f32, trim_start: u32, trim_end: u32| TimelineDraft {
            version: TIMELINE_DRAFT_VERSION,
            trim_start,
            trim_end,
            deleted: vec![],
            deleted_ranges: vec![],
            speed,
            holds: vec![],
            hold_ranges: vec![],
            loop_count: None,
            revision: 3,
            play_pos_ms: 0,
            mix: [AudioMix::default(); 2],
        };
        let mut t = tl();
        for (draft, err) in [
            (mk(1.0, 2, 1), EditError::FrameOutOfRange),
            (mk(f32::NAN, 0, 4), EditError::InvalidSpeed),
            (mk(9.0, 0, 4), EditError::InvalidSpeed),
        ] {
            let before_revision = t.revision();
            assert_eq!(t.restore_draft(draft), Err(err));
            assert_eq!(t.revision(), before_revision);
        }
    }

    #[test]
    fn draft_restore_deleted_frames_zero_duration_like_live_edits() {
        // 恢复后删除帧时长置零的 Core 不变式与实时编辑一致（effective_total 反映）
        let mut t = tl();
        t.apply(&EditCommand::DeleteFrame { frame: 1 }).unwrap();
        let draft = t.to_draft(0);
        let mut restored = tl();
        restored.restore_draft(draft).unwrap();
        assert_eq!(restored.effective_total_ms(), 400);
        // 修剪不受删除影响（trim 是素材索引语义）
        assert_eq!(restored.trim_range(), (0, 4));
    }

    #[test]
    fn draft_restore_revision_never_below_one() {
        let draft = TimelineDraft {
            version: TIMELINE_DRAFT_VERSION,
            trim_start: 0,
            trim_end: 4,
            deleted: vec![],
            deleted_ranges: vec![],
            speed: 1.0,
            holds: vec![],
            hold_ranges: vec![],
            loop_count: None,
            revision: 0,
            play_pos_ms: 0,
            mix: [AudioMix::default(); 2],
        };
        let mut t = tl();
        t.restore_draft(draft).unwrap();
        assert_eq!(t.revision(), 1);
    }

    #[test]
    fn audio_mapping_cuts_deleted_time_pads_holds_then_applies_speed() {
        let mut t = Timeline::from_durations(vec![100; 4]);
        t.apply(&EditCommand::DeleteFrame { frame: 1 }).unwrap();
        t.apply(&EditCommand::SetHold { frame: 0, ms: 300 })
            .unwrap();
        t.apply(&EditCommand::SetHold { frame: 2, ms: 50 }).unwrap();
        t.apply(&EditCommand::SetSpeed { factor: 2.0 }).unwrap();
        let s = t.audio_spans();
        assert_eq!(
            s.iter()
                .map(|s| (s.source_ms, s.sound_ms, s.silence_ms, s.output_ms))
                .collect::<Vec<_>>(),
            vec![(0, 100, 200, 150), (200, 50, 0, 25), (300, 100, 0, 50)]
        );
        assert_eq!(
            s.iter().map(|s| s.output_ms).sum::<u64>(),
            t.effective_total_ms()
        );
        t.undo();
        assert_eq!(t.audio_spans()[0].output_ms, 300);
    }
    #[test]
    fn continuous_audio_is_one_interval_and_mix_is_undoable_and_persisted() {
        let mut t = Timeline::from_durations(vec![33; 1000]);
        assert_eq!(t.audio_spans().len(), 1);
        t.apply(&EditCommand::SetAudio {
            source: 1,
            gain: 0.5,
            muted: true,
        })
        .unwrap();
        let snapshot = t.export_snapshot(640, 480);
        let draft = t.to_draft(100);
        t.undo();
        assert_eq!(t.mix()[1], AudioMix::default());
        assert_eq!(
            snapshot.mix[1],
            AudioMix {
                gain: 0.5,
                muted: true
            }
        );
        t.restore_draft(draft).unwrap();
        assert_eq!(t.mix(), snapshot.mix);
        let rev = t.revision();
        assert_eq!(
            t.apply(&EditCommand::SetAudio {
                source: 2,
                gain: 1.0,
                muted: false
            }),
            Err(EditError::InvalidAudio)
        );
        assert_eq!(t.revision(), rev);
    }
    #[test]
    fn old_draft_defaults_to_unmuted_tracks_and_pages_are_bounded() {
        let mut t = tl();
        let mut json = serde_json::to_value(t.to_draft(0)).unwrap();
        json.as_object_mut().unwrap().remove("mix");
        t.restore_draft(serde_json::from_value(json).unwrap())
            .unwrap();
        assert_eq!(t.mix(), [AudioMix::default(); 2]);
        assert_eq!(
            t.frame_page(2, 2)
                .iter()
                .map(|f| f.src_idx)
                .collect::<Vec<_>>(),
            vec![2, 3]
        );
        assert!(t.frame_page(99, 256).is_empty());
    }
}
