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

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

/// GIF delay 以 10ms 为单位的下限（低于 2 的 delay 被浏览器按 ≥100ms 回放）。
pub const MIN_DELAY_CS: u32 = 2;
/// 单帧导出时长上限（60 × 10ms = 600ms 的 10 倍；防止极端 hold 产生不可回放的帧）。
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
    /// 重置全部编辑（不可撤销其他命令之后——reset 本身可撤销）。
    Reset,
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
    pub frames: Vec<PlanFrame>,
    /// None = 无限循环。
    pub loop_count: Option<u16>,
}

/// 格式中立的毫秒快照。MP4 直接使用此时钟，GIF 单独适配量化与循环。
#[derive(Debug, Clone, PartialEq)]
pub struct ExportSnapshot {
    pub revision: u64,
    pub src_w: u32,
    pub src_h: u32,
    pub frames: Vec<FrameView>,
    pub loop_count: Option<u16>,
}
impl ExportSnapshot {
    pub fn gif_plan(&self) -> ExportPlan {
        ExportPlan {
            revision: self.revision,
            src_w: self.src_w,
            src_h: self.src_h,
            frames: self
                .frames
                .iter()
                .map(|f| PlanFrame {
                    src_idx: f.src_idx,
                    delay_cs: quantize_delay_ms(f.dur_ms),
                })
                .collect(),
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

/// 可撤销的编辑核心状态（快照栈的单位；帧数 ≤ 3600，clone 成本可忽略）。
#[derive(Debug, Clone, PartialEq)]
struct Core {
    trim_start: u32,
    trim_end: u32,
    deleted: BTreeSet<u32>,
    /// 删除帧时长置零后的有效素材时长（与 deleted 同步维护）。
    base_dur_ms: Vec<u32>,
    speed: f32,
    holds: BTreeMap<u32, u32>,
    loop_count: Option<u16>,
}

impl Core {
    fn fresh(base_dur_ms: Vec<u32>) -> Self {
        let end = base_dur_ms.len().saturating_sub(1) as u32;
        Self {
            trim_start: 0,
            trim_end: end,
            deleted: BTreeSet::new(),
            base_dur_ms,
            speed: 1.0,
            holds: BTreeMap::new(),
            loop_count: None,
        }
    }
}

/// 时间轴（domain 纯状态；会话存储在 bin/infra 侧）。
#[derive(Debug, Clone)]
pub struct Timeline {
    frame_count: u32,
    core: Core,
    /// 素材原始时长（不可变基线）——`Reset` 恢复用；删除帧把 `core.base_dur_ms`
    /// 置 0，undo 栈回滚 Core 快照可恢复，但 Reset 不经 undo 栈、需原始基线。
    origin_durs: Vec<u32>,
    undo: Vec<Core>,
    redo: Vec<Core>,
    revision: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EditError {
    /// 帧号超出素材范围。
    FrameOutOfRange,
    /// 修剪终点早于起点。
    InvalidTrimRange,
}

impl std::fmt::Display for EditError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EditError::FrameOutOfRange => write!(f, "帧号超出素材范围"),
            EditError::InvalidTrimRange => write!(f, "修剪终点早于起点"),
        }
    }
}

impl Timeline {
    /// 由素材原始时长表构建（录制 finalize 后产出）。
    pub fn from_durations(base_dur_ms: Vec<u32>) -> Self {
        let frame_count = base_dur_ms.len() as u32;
        Self {
            frame_count,
            core: Core::fresh(base_dur_ms.clone()),
            origin_durs: base_dur_ms,
            undo: Vec::new(),
            redo: Vec::new(),
            revision: 1,
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

    pub fn deleted_frames(&self) -> Vec<u32> {
        self.core.deleted.iter().copied().collect()
    }

    pub fn speed(&self) -> f32 {
        self.core.speed
    }

    pub fn holds(&self) -> &BTreeMap<u32, u32> {
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
        self.undo.push(self.core.clone());
        self.redo.clear();
        self.core = next;
        self.revision += 1;
        Ok(true)
    }

    pub fn undo(&mut self) -> bool {
        let Some(prev) = self.undo.pop() else {
            return false;
        };
        self.redo.push(std::mem::replace(&mut self.core, prev));
        self.revision += 1;
        true
    }

    pub fn redo(&mut self) -> bool {
        let Some(next) = self.redo.pop() else {
            return false;
        };
        self.undo.push(std::mem::replace(&mut self.core, next));
        self.revision += 1;
        true
    }

    fn apply_core(&self, cmd: &EditCommand) -> Result<Core, EditError> {
        let mut c = self.core.clone();
        // 含 Vec 字段的命令（DeleteFrames/RestoreFrames/SetHoldMany）不能经
        // `match *cmd` 按值解构——前置特判。
        if let EditCommand::DeleteFrames { frames } = cmd {
            if frames.iter().any(|f| *f >= self.frame_count) {
                return Err(EditError::FrameOutOfRange);
            }
            let mut changed = false;
            for frame in frames {
                if c.deleted.insert(*frame) {
                    c.base_dur_ms[*frame as usize] = 0;
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
                    c.base_dur_ms[*frame as usize] =
                        self.origin_durs.get(*frame as usize).copied().unwrap_or(1).max(1);
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
                if !c.deleted.insert(frame) {
                    return Ok(c); // 已删除：幂等，无变化
                }
                // 2026-10-26 语义定案（手测反馈）：删除帧 = 画面与时长一并移除
                // （总时长缩短）。旧「时长并入前一帧」语义已废弃——用户删帧期望
                // 该帧的显示时间消失，不是跳帧等待。
                c.base_dur_ms[frame as usize] = 0;
            }
            EditCommand::DeleteFrames { .. } => unreachable!("DeleteFrames 由 apply_core 前置特判处理"),
            EditCommand::RestoreFrames { .. } => unreachable!("RestoreFrames 由 apply_core 前置特判处理"),
            EditCommand::SetHoldMany { .. } => unreachable!("SetHoldMany 由 apply_core 前置特判处理"),
            EditCommand::SetSpeed { factor } => {
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
            EditCommand::Reset => {
                // Reset 回到素材原始状态（含原始时长——删除帧置 0 的时长恢复）；
                // 本身可撤销（undo 栈回滚 Core 快照）。
                c = Core::fresh(self.origin_durs.clone());
            }
        }
        Ok(c)
    }

    /// 逐帧导出视图（应用全部编辑；dur 未量化，供前端预览播放）。
    pub fn effective_frames(&self) -> Vec<FrameView> {
        let mut out = Vec::new();
        for i in self.core.trim_start..=self.core.trim_end {
            if self.core.deleted.contains(&i) {
                continue;
            }
            let base = self.core.base_dur_ms[i as usize].max(1);
            let held = self.core.holds.get(&i).copied();
            let dur = match held {
                Some(h) => h,
                None => ((base as f32) / self.core.speed).round().max(1.0) as u32,
            };
            out.push(FrameView {
                src_idx: i,
                dur_ms: dur,
                held: held.is_some(),
            });
        }
        out
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
            frames: self.effective_frames(),
            loop_count: self.core.loop_count,
        }
    }

    /// 有效总时长（ms，未量化）。
    pub fn effective_total_ms(&self) -> u64 {
        self.effective_frames()
            .iter()
            .map(|f| f.dur_ms as u64)
            .sum()
    }
}

/// ms → 1/100s 量化，clamp [MIN_DELAY_CS, MAX_DELAY_CS]。
pub fn quantize_delay_ms(ms: u32) -> u32 {
    (ms / 10).clamp(MIN_DELAY_CS, MAX_DELAY_CS)
}

// ── 测试 ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn tl() -> Timeline {
        Timeline::from_durations(vec![100, 100, 100, 100, 100])
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
        t.apply(&EditCommand::DeleteFrames { frames: vec![1, 3] }).unwrap();
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
        t.apply(&EditCommand::DeleteFrames { frames: vec![1, 3] }).unwrap();
        let rev = t.revision();
        assert_eq!(t.apply(&EditCommand::DeleteFrames { frames: vec![1, 3] }), Ok(false));
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
        t.apply(&EditCommand::DeleteFrames { frames: vec![1, 3] }).unwrap();
        // 恢复 1（3 保持删除）
        t.apply(&EditCommand::RestoreFrames { frames: vec![1] }).unwrap();
        let idx: Vec<u32> = t.effective_frames().iter().map(|f| f.src_idx).collect();
        assert_eq!(idx, vec![0, 1, 2, 4]);
        assert_eq!(t.effective_total_ms(), 400, "恢复帧时长从素材基线还原");
        // 全未删除的恢复 = 幂等，不推进 revision
        let rev = t.revision();
        assert_eq!(t.apply(&EditCommand::RestoreFrames { frames: vec![1, 2] }), Ok(false));
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
        t.apply(&EditCommand::SetHoldMany { frames: vec![0, 2, 4], ms: 300 }).unwrap();
        assert_eq!(t.revision(), rev + 1, "批量同值单命令推进一次 revision");
        let v = t.effective_frames();
        for i in [0, 2, 4] {
            assert_eq!(v[i].dur_ms, 300);
            assert!(v[i].held);
        }
        assert_eq!(v[1].dur_ms, 100, "未选中帧不受影响");
        // 同值幂等
        assert_eq!(t.apply(&EditCommand::SetHoldMany { frames: vec![0, 2], ms: 300 }), Ok(false));
        // ms=0 批量清除 → 回速度计算
        t.apply(&EditCommand::SetHoldMany { frames: vec![0, 2, 4], ms: 0 }).unwrap();
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
            t.apply(&EditCommand::SetHoldMany { frames: vec![0, 9], ms: 100 }),
            Err(EditError::FrameOutOfRange)
        );
        // 超 clamp 收敛到上限（MAX_DELAY_CS*10 = 6000ms）
        t.apply(&EditCommand::SetHoldMany { frames: vec![1], ms: 99_999 }).unwrap();
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
        t.apply(&EditCommand::SetHold { frame: 1, ms: 800 }).unwrap();
        let v = t.effective_frames();
        assert_eq!(v[1].dur_ms, 800);
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
        assert_eq!(plan.frames[0].delay_cs, 6);
        assert_eq!(plan.loop_count, Some(3));
        // 编辑后原 plan 不变（快照语义）
        t.apply(&EditCommand::SetSpeed { factor: 4.0 }).unwrap();
        assert_eq!(plan.frames[0].delay_cs, 6);
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
            vec![(0, 22), (2, 777), (3, 22)]
        );
        assert_eq!(snapshot.gif_plan(), t.export_plan(65, 67));
        assert_eq!(snapshot.gif_plan().frames[1].delay_cs, 77);
        t.apply(&EditCommand::Reset).unwrap();
        assert_eq!(snapshot.frames[1].dur_ms, 777);
        assert_ne!(snapshot.revision, t.revision());
    }

    #[test]
    fn quantize_clamps_min_and_max() {
        assert_eq!(quantize_delay_ms(0), MIN_DELAY_CS);
        assert_eq!(quantize_delay_ms(5), MIN_DELAY_CS, "20ms 以下抬到 20ms");
        assert_eq!(quantize_delay_ms(30), 3);
        assert_eq!(quantize_delay_ms(u32::MAX), MAX_DELAY_CS);
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
}
