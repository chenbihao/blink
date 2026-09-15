//! 0.22.15 事务化句子状态管理。
//!
//! ## 样本坐标系统（0.22.15 follow-up: 统一绝对坐标）
//!
//! 所有坐标一律使用**绝对样本坐标**（录音会话内从 0 开始的单调位置）。
//!
//! - `committed_sample_end`：已 committed 的绝对末尾
//! - `buffer_base_sample`：`samples[0]` 在录音会话中的绝对位置
//!   （compact 后推进到 `committed_sample_end`）
//! - 当前绝对尾端 = `buffer_base_sample.checked_add(samples.len())`
//!
//! 局部切片通过 [`SentenceState::abs_to_local_range`] 统一转换，禁止手工减法。

use super::{AudioRange, DraftSpan};

/// 0.22.15：pending segment 的 identity——finalize task 返回时必须匹配。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SegmentIdentity {
    pub(crate) session_generation: u64,
    pub(crate) commit_generation: u64,
    pub(crate) segment_id: u64,
}

/// 录音结束时唯一拥有剩余音频提交权的终态请求。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TerminalFinalizeIdentity {
    pub(crate) session_generation: u64,
    pub(crate) commit_generation: u64,
}

/// 0.22.15：pending segment——句尾产生的候选，等待定稿结果决定 commit 或 rollback。
#[derive(Debug, Clone)]
pub(crate) struct PendingSegment {
    /// 此 segment 的 identity
    pub(crate) identity: SegmentIdentity,
    /// 候选音频范围 [start, end)
    pub(crate) range: std::ops::Range<usize>,
    /// 句尾时的 preview 快照（rollback 时恢复）
    pub(crate) preview_snapshot: String,
}

/// 0.22.15：finalize task 的返回结果——携带 identity，使写入方可以校验。
#[derive(Debug)]
pub(crate) struct FinalizeResult {
    pub(crate) identity: SegmentIdentity,
    pub(crate) text: String,
    /// 是否成功（false = 错误或超时）
    pub(crate) ok: bool,
}

/// 0.22.15 事务化句子状态管理。
pub(crate) struct SentenceState {
    /// 已定稿的句子列表
    pub(crate) confirmed_sentences: Vec<String>,
    /// 已 committed 的音频末尾（绝对坐标，下一句的起始）
    pub(crate) committed_sample_end: usize,
    /// 已预留给 running/pending Draft 的绝对末端。
    ///
    /// `committed_sample_end` 只在成功 Draft 后推进；reserved 水位防止
    /// worker 忙时后续候选再次从 committed 起点创建重叠请求。
    pub(crate) draft_reserved_sample_end: usize,
    /// 新版 Draft lane 需要按 reserved 水位分配不重叠区间；Legacy lane
    /// 保留旧的 deferred 起点并在前段 commit 后再 rebase。
    pub(crate) reserve_draft_ranges: bool,
    /// 单调 session generation（每次 reset 递增）
    pub(crate) session_generation: u64,
    /// 单调 segment id（每个句尾递增）
    pub(crate) next_segment_id: u64,
    /// 提交权代际。terminal finalize 接管或 reset 时递增，使所有旧 task 失效。
    pub(crate) commit_generation: u64,
    /// 当前 pending segment（如果有）
    pub(crate) pending: Option<PendingSegment>,
    /// 有 finalize task 在飞行中
    pub(crate) finalize_in_flight: bool,
    /// terminal finalize 已冻结音频并独占提交权；此时不再接受新 chunk。
    pub(crate) finalizing: Option<TerminalFinalizeIdentity>,
    /// 0.22.15 fix: pending 期间再次出现句尾时排队的 deferred segment
    /// （finalize_in_flight 为 true 时，新句尾暂存于此，finalize 完成后再处理）
    pub(crate) deferred: Option<PendingSegment>,
    /// `samples[0]` 在录音会话中的绝对位置。
    ///
    /// compact 后推进到 `committed_sample_end`。
    /// 初始为 0，compact 时 `drain_count = committed_sample_end - buffer_base_sample`，
    /// drain 后 `buffer_base_sample = committed_sample_end`。
    pub(crate) buffer_base_sample: usize,
    /// confirmed 提交版本：每次成功 commit（confirmed 实际增长）递增。
    ///
    /// 供上层做「状态边沿触发」判定——引擎状态是否变化由本版本号与
    /// 预览版本号共同表达，避免每块音频都对外搬运全量正文。
    pub(crate) confirmed_revision: u64,
    /// 已成功提交的 Draft ledger（按音频顺序追加）。
    pub(crate) draft_spans: Vec<DraftSpan>,
}

impl SentenceState {
    pub(crate) fn new() -> Self {
        Self {
            confirmed_sentences: Vec::new(),
            committed_sample_end: 0,
            draft_reserved_sample_end: 0,
            reserve_draft_ranges: false,
            session_generation: 1,
            next_segment_id: 1,
            commit_generation: 1,
            pending: None,
            finalize_in_flight: false,
            finalizing: None,
            deferred: None,
            buffer_base_sample: 0,
            confirmed_revision: 0,
            draft_spans: Vec::new(),
        }
    }

    /// 选择是否由句子状态直接预留 Draft 的 owned range。
    ///
    /// Legacy 累计 Partial 仍让 deferred 从 committed 起点开始，保持旧
    /// 的 rollback/rebase 契约；PreviewDraft 则开启该开关，避免 worker
    /// 忙时后续候选反复拥有同一段音频。
    pub(crate) fn set_draft_range_reservation(&mut self, enabled: bool) {
        self.reserve_draft_ranges = enabled;
        if !enabled {
            self.draft_reserved_sample_end = self.committed_sample_end;
        }
    }

    /// 将绝对 range 转换为相对于 `samples` 的局部 range。
    ///
    /// 校验：`start >= buffer_base`、`end >= start`、
    /// `end <= buffer_base + samples_len`。
    /// 非法 range 返回 `None`（调用方应安全终止，不 panic）。
    ///
    /// **所有 `samples` 切片必须通过此 helper，禁止手工减法。**
    pub(crate) fn abs_to_local_range(
        &self,
        abs_range: &std::ops::Range<usize>,
        samples_len: usize,
    ) -> Option<std::ops::Range<usize>> {
        let base = self.buffer_base_sample;
        if abs_range.start < base {
            tracing::error!(
                abs_start = abs_range.start,
                buffer_base = base,
                "坐标错误：abs_start < buffer_base"
            );
            return None;
        }
        if abs_range.end < abs_range.start {
            tracing::error!(
                abs_start = abs_range.start,
                abs_end = abs_range.end,
                "坐标错误：abs_end < abs_start"
            );
            return None;
        }
        let local_start = abs_range.start - base;
        let local_end = abs_range.end - base;
        if local_end > samples_len {
            tracing::error!(
                local_end = local_end,
                samples_len = samples_len,
                "坐标错误：local_end > samples_len"
            );
            return None;
        }
        Some(local_start..local_end)
    }

    /// 句尾：创建 pending segment，不推进 committed end。
    ///
    /// `total_samples` 是**绝对**坐标（`buffer_base_sample + samples.len()`）。
    /// pending range `[committed_sample_end, total_samples)` 也是绝对坐标。
    ///
    /// 调用方随后 spawn finalize task，结果通过 `commit_or_rollback` 写入。
    ///
    /// 0.22.15 fix: 如果 finalize_in_flight 为 true（上一个句尾的定稿还在飞行中），
    /// 新句尾暂存到 deferred，等前一个 finalize 完成后再处理。
    /// 返回 None 表示暂存了，调用方不应 spawn 新 finalize task。
    pub(crate) fn on_sentence_end(
        &mut self,
        total_samples: usize,
        preview_snapshot: &str,
    ) -> Option<PendingSegment> {
        let segment_id = self.next_segment_id;
        self.next_segment_id += 1;

        // 绝对坐标 range：[committed_sample_end, total_samples)
        let start = if self.reserve_draft_ranges {
            self.draft_reserved_sample_end
                .max(self.committed_sample_end)
        } else {
            self.committed_sample_end
        };
        let end = total_samples;
        if end <= start {
            tracing::debug!(start, end, "忽略没有新增音频覆盖范围的 Draft 候选");
            return None;
        }
        let range = start..end;
        let identity = SegmentIdentity {
            session_generation: self.session_generation,
            commit_generation: self.commit_generation,
            segment_id,
        };
        let pending = PendingSegment {
            identity,
            range: range.clone(),
            preview_snapshot: preview_snapshot.to_string(),
        };

        if self.finalize_in_flight {
            // 上一个 finalize 还在飞行中，暂存
            tracing::debug!(
                seg = segment_id,
                "句尾时 finalize_in_flight，暂存到 deferred"
            );
            match self.deferred.as_mut() {
                Some(existing) => {
                    // 单槽合并：只扩展末端，不制造按候选增长的请求队列。
                    existing.range.end = existing.range.end.max(pending.range.end);
                    existing.preview_snapshot = pending.preview_snapshot;
                }
                None => self.deferred = Some(pending),
            }
            self.draft_reserved_sample_end = self
                .deferred
                .as_ref()
                .map(|deferred| deferred.range.end)
                .unwrap_or(self.draft_reserved_sample_end)
                .max(self.draft_reserved_sample_end);
            return None;
        }

        self.pending = Some(pending.clone());
        self.draft_reserved_sample_end = self.draft_reserved_sample_end.max(end);
        Some(pending)
    }

    /// 0.22.15：commit 或 rollback 一个 finalize 结果。
    ///
    /// - identity 不匹配 → 丢弃（旧 session 的迟到结果）
    /// - 非空结果 → commit：追加 confirmed、推进 committed end、清空对应 preview
    /// - 空/错误 → rollback：committed end 不变、保留 preview 快照
    ///
    /// 返回值：
    /// - `Some(pending)` = commit/rollback 后有 deferred segment 需要处理
    /// - `None` = 已处理，无 deferred
    /// - （被丢弃的 stale result 也返回 None）
    pub(crate) fn commit_or_rollback(&mut self, result: &FinalizeResult) -> Option<PendingSegment> {
        if result.identity.session_generation != self.session_generation
            || result.identity.commit_generation != self.commit_generation
            || self.finalizing.is_some()
        {
            tracing::debug!(
                result_session = result.identity.session_generation,
                current_session = self.session_generation,
                result_commit_generation = result.identity.commit_generation,
                current_commit_generation = self.commit_generation,
                terminal_finalizing = self.finalizing.is_some(),
                "丢弃已失去提交权的 finalize 结果"
            );
            return None;
        }
        // identity 校验
        let pending = match &self.pending {
            Some(p) if p.identity == result.identity => p,
            Some(p) => {
                tracing::debug!(
                    pending_seg = p.identity.segment_id,
                    result_seg = result.identity.segment_id,
                    "丢弃 identity 不匹配的 finalize 结果"
                );
                return None;
            }
            None => {
                tracing::debug!("无 pending segment，丢弃 finalize 结果");
                return None;
            }
        };

        let range = pending.range.clone();
        let preview_snapshot = &pending.preview_snapshot;

        let committed = result.ok && !result.text.is_empty();
        if committed {
            // ── commit ──
            // range.end 是绝对坐标，直接推进 committed_sample_end
            self.confirmed_sentences.push(result.text.clone());
            self.committed_sample_end = self.committed_sample_end.max(range.end);
            self.draft_reserved_sample_end = self
                .draft_reserved_sample_end
                .max(self.committed_sample_end);
            self.draft_spans.push(DraftSpan {
                span_id: result.identity.segment_id,
                audio_range: AudioRange::new(range.start as u64, range.end as u64),
                text: result.text.clone(),
                revision: 1,
            });
            self.confirmed_revision = self.confirmed_revision.wrapping_add(1);
            tracing::debug!(
                seg = result.identity.segment_id,
                text_len = result.text.chars().count(),
                range_end = range.end,
                "commit pending segment"
            );
        } else {
            // ── rollback ──
            // committed end 不变，preview 快照保留在 pending 中供 finalize() 兜底
            tracing::debug!(
                seg = result.identity.segment_id,
                ok = result.ok,
                text_empty = result.text.is_empty(),
                preview_len = preview_snapshot.chars().count(),
                "rollback pending segment"
            );
            // PreviewDraft 的 Draft 失败必须从 committed 水位重基；否则
            // reserved 仍停在失败请求末端，下一次会跳过本应以更长上下文
            // 重试的音频。Legacy 的 deferred 起点本来就是 committed，
            // 这里的重置同样保持其旧语义。
            self.draft_reserved_sample_end = self.committed_sample_end;
        }

        self.pending = None;
        self.finalize_in_flight = false;

        // 0.22.15 fix: 如果有 deferred segment，返回它让调用方 spawn 新 finalize
        let mut deferred = self.deferred.take();
        if let Some(ref mut d) = deferred {
            // deferred 在前一任务仍飞行时创建，其起点基于当时尚未推进的
            // committed_sample_end。前一段成功 commit 后必须重定位，避免
            // 下一任务再次拥有已经提交音频的提交权。
            d.range.start = if !committed && self.reserve_draft_ranges {
                self.committed_sample_end
            } else {
                d.range.start.max(self.committed_sample_end)
            };
            if d.range.end <= d.range.start {
                deferred = None;
                self.draft_reserved_sample_end = self.committed_sample_end;
            }
        }
        if let Some(ref d) = deferred {
            self.pending = Some(d.clone());
            self.finalize_in_flight = true;
            self.draft_reserved_sample_end = self.draft_reserved_sample_end.max(d.range.end);
        }
        deferred
    }

    /// 0.22.15 fix: 尝试回收已 committed 的 PCM 样本。
    ///
    /// 条件：无 pending 和 deferred（没有飞行中的 finalize 引用旧音频）。
    /// 返回需要丢弃的前缀样本数。调用方据此 `samples.drain(..n)`，
    /// 本方法同时推进 `buffer_base_sample` 到 `committed_sample_end`。
    pub(crate) fn try_compact(
        &mut self,
        samples_len: usize,
    ) -> Result<Option<usize>, &'static str> {
        if self.pending.is_some() || self.deferred.is_some() {
            return Ok(None);
        }
        if self.buffer_base_sample >= self.committed_sample_end {
            return Ok(None);
        }
        let buffer_end = self
            .buffer_base_sample
            .checked_add(samples_len)
            .ok_or("compact buffer end 溢出")?;
        if self.committed_sample_end > buffer_end {
            return Err("committed end 超出当前 PCM buffer");
        }
        let to_compact = self
            .committed_sample_end
            .checked_sub(self.buffer_base_sample)
            .ok_or("compact base 超过 committed end")?;
        self.buffer_base_sample = self.committed_sample_end;
        if to_compact > 0 {
            tracing::debug!(
                compacted = to_compact,
                buffer_base = self.buffer_base_sample,
                total_committed = self.committed_sample_end,
                "compact 已 committed PCM"
            );
            Ok(Some(to_compact))
        } else {
            Ok(None)
        }
    }

    /// 追加一句定稿文本（测试辅助）。
    #[cfg(test)]
    pub(crate) fn append_confirmed(&mut self, text: &str) {
        if !text.is_empty() {
            self.confirmed_sentences.push(text.to_string());
        }
    }

    /// 获取已确认部分的文本。
    pub(crate) fn confirmed_text(&self) -> String {
        self.confirmed_sentences.join("")
    }

    /// terminal finalize 原子接管所有尚未提交的音频。
    ///
    /// 无论等待是否超时，接管都会推进 `commit_generation` 并清除
    /// pending/deferred；旧 segment task 即使迟到也无法再通过代际校验。
    pub(crate) fn begin_terminal_finalize(&mut self) -> TerminalFinalizeIdentity {
        self.commit_generation = self.commit_generation.wrapping_add(1);
        let identity = TerminalFinalizeIdentity {
            session_generation: self.session_generation,
            commit_generation: self.commit_generation,
        };
        self.pending = None;
        self.deferred = None;
        self.finalize_in_flight = false;
        self.draft_reserved_sample_end = self.committed_sample_end;
        self.finalizing = Some(identity);
        identity
    }

    /// 提交 terminal finalize；reset/新接管发生后返回 false 并确定性丢弃。
    pub(crate) fn commit_terminal_finalize(
        &mut self,
        identity: TerminalFinalizeIdentity,
        range_end: usize,
        text: &str,
    ) -> bool {
        if self.finalizing != Some(identity)
            || self.session_generation != identity.session_generation
            || self.commit_generation != identity.commit_generation
        {
            return false;
        }
        let range_start = self.committed_sample_end;
        if !text.is_empty() {
            self.confirmed_sentences.push(text.to_string());
            self.committed_sample_end = self.committed_sample_end.max(range_end);
            self.draft_reserved_sample_end = self
                .draft_reserved_sample_end
                .max(self.committed_sample_end);
            self.confirmed_revision = self.confirmed_revision.wrapping_add(1);
            self.draft_spans.push(DraftSpan {
                span_id: self.next_segment_id,
                audio_range: AudioRange::new(range_start as u64, range_end as u64),
                text: text.to_string(),
                revision: 1,
            });
            self.next_segment_id = self.next_segment_id.wrapping_add(1);
        } else {
            // NoSpeech 是成功消费整个 owned range，而不是失败回滚。
            self.committed_sample_end = self.committed_sample_end.max(range_end);
            self.draft_reserved_sample_end = self
                .draft_reserved_sample_end
                .max(self.committed_sample_end);
        }
        self.finalizing = None;
        true
    }

    /// terminal transport 失败时释放 finalize 所有权，但不推进覆盖水位。
    ///
    /// 与 `commit_terminal_finalize(..., "")` 区分：后者表示 gate 判定
    /// `NoSpeech`，应消费整个 owned range；这里表示真实推理失败，必须保留
    /// 尾段给上层重试/报告错误。
    pub(crate) fn abort_terminal_finalize(&mut self, identity: TerminalFinalizeIdentity) -> bool {
        if self.finalizing != Some(identity)
            || self.session_generation != identity.session_generation
            || self.commit_generation != identity.commit_generation
        {
            return false;
        }
        self.finalizing = None;
        self.draft_reserved_sample_end = self.committed_sample_end;
        true
    }

    /// reset：递增 session_generation，清空所有状态。
    pub(crate) fn reset(&mut self) {
        self.confirmed_sentences.clear();
        self.committed_sample_end = 0;
        self.draft_reserved_sample_end = 0;
        self.session_generation = self.session_generation.wrapping_add(1);
        self.commit_generation = self.commit_generation.wrapping_add(1);
        self.next_segment_id = 1;
        self.pending = None;
        self.deferred = None;
        self.finalize_in_flight = false;
        self.finalizing = None;
        self.buffer_base_sample = 0;
        self.confirmed_revision = 0;
        self.draft_spans.clear();
    }

    /// NoSpeech 请求消费 owned range，但不产生 DraftSpan。
    pub(crate) fn consume_no_speech(
        &mut self,
        identity: SegmentIdentity,
        range_end: usize,
    ) -> Option<PendingSegment> {
        let Some(pending) = self.pending.as_ref() else {
            return None;
        };
        if pending.identity != identity {
            return None;
        }
        self.committed_sample_end = self.committed_sample_end.max(range_end);
        self.draft_reserved_sample_end = self
            .draft_reserved_sample_end
            .max(self.committed_sample_end);
        self.pending = None;
        self.finalize_in_flight = false;
        let mut deferred = self.deferred.take();
        if let Some(ref mut next) = deferred {
            next.range.start = next.range.start.max(self.committed_sample_end);
            if next.range.end <= next.range.start {
                deferred = None;
            }
        }
        if let Some(ref next) = deferred {
            self.pending = Some(next.clone());
            self.finalize_in_flight = true;
            self.draft_reserved_sample_end = self.draft_reserved_sample_end.max(next.range.end);
        } else {
            self.draft_reserved_sample_end = self.committed_sample_end;
        }
        deferred
    }

    pub(crate) fn draft_spans(&self) -> &[DraftSpan] {
        &self.draft_spans
    }
}
