//! 统一听写账本（0.23.3 §3.6 引入，0.23.13 收敛为 G2/Editor 共用，
//! 0.23.14 增加注入交付 ack 双水位）。
//!
//! 伪流式引擎在一次 STT session 内的 confirmed 文本单调增长（0.22.15
//! 事务化句尾：commit 才追加，rollback 不回退；Draft span 一经提交不可
//! 修订——`revision` 字段仅预留）。0.23.9 起优先接受带有
//! `AudioRange`/`span_id` 的 Draft；旧累计 confirmed/Final 方法只保留给
//! Legacy 兼容路径。
//!
//! 0.23.13 起同一账本服务两条交付路径，仅保留窗口大小不同：
//! - **G2 渐进上屏**（retention=0）：定稿即投递注入 worker，会话终态只
//!   补交剩余（尾段 + 未上报 terminal span）；
//! - **Editor 连续听写**（retention=1）：窗口外草稿先行写入正文，
//!   Final 时冲刷 pending + 尾段收尾。
//!
//! 0.23.14：投递（queued）与确认交付（acked）分离。G2 注入 worker 成功
//! ack 前段保持浮窗可见；终态补交按 queued 前缀裁剪（worker 保序，按
//! acked 裁剪会把 deferred 文本重复上屏）。Editor 的段经事件同步写正文，
//! 投递即 ack。
//!
//! 纯逻辑、框架无关：不依赖 tauri / tokio，单测覆盖去重、保留窗口、
//! Final 前缀裁剪与不一致时的兜底路径。

use super::DraftSpan;

/// 统一听写账本：Draft 去重接受 + 保留窗口调度 + Final 前缀裁剪。
///
/// 一个听写会话一个实例；会话结束后整体丢弃（不跨会话复用）。
///
/// 0.23.14 双水位交付：`queued`（已投递注入 worker / 已同步交付）与
/// `acked`（worker 确认注入成功）分离——queued 未 ack 的段保持可见，
/// 只有 ack 后才从浮窗 confirmed 窗口退场；终态剩余按 queued 前缀裁剪
/// （worker 单消费者保序，deferred 文本终态必达，按 acked 裁剪会重复）。
#[derive(Debug)]
pub struct DictationLedger {
    /// 保留窗口：账本内保持未投递的最新段数（G2=0 / Editor=1）。
    retention: usize,
    /// 已分配的最大 seq；段号从 1 开始单调递增（含 Legacy 增量路径）。
    last_seq: u64,
    /// 已接受的类型化 Draft span（按接受顺序，含未冲刷）。
    entries: Vec<(u64, DraftSpan)>,
    /// 已接受文本累计（span + Legacy 增量 + 尾段）；Final 尾段裁剪基准。
    delivered_text: String,
    /// 已确认交付（ack）的段数——浮窗保留窗口基准。
    acked: usize,
    /// 已投递注入 worker、尚无 ack 的段数。
    queued: usize,
    /// 已投递（含未 ack）文本累计；终态剩余推导基准（绝不能重复上屏的前缀）。
    flushed_text: String,
    /// 已接受 Draft 的最后音频边界；只用于时间轴去重，不参与文本合并。
    last_audio_end: Option<u64>,
}

impl DictationLedger {
    pub fn new(retention: usize) -> Self {
        Self {
            retention,
            last_seq: 0,
            entries: Vec::new(),
            delivered_text: String::new(),
            acked: 0,
            queued: 0,
            flushed_text: String::new(),
            last_audio_end: None,
        }
    }

    /// 最后分配的 seq（0 = 尚无段）。
    pub fn last_seq(&self) -> u64 {
        self.last_seq
    }

    /// 接受一个类型化 Draft span；同一 `span_id`、与已接受范围重叠、或
    /// 起点早于已接受末端的 span 会被忽略。首版不做文本 LCP/LCS 合并。
    ///
    /// 返回 `true` 表示新接受（seq 已分配，文本进入 `delivered_text`）；
    /// 交付由 [`DictationLedger::drain_flushable`] / [`DictationLedger::take_pending`] 调度。
    pub fn accept_draft_span(&mut self, span: DraftSpan) -> bool {
        if span.text.is_empty()
            || self
                .entries
                .iter()
                .any(|(_, accepted)| accepted.span_id == span.span_id)
        {
            return false;
        }

        let range = span.audio_range;
        if !range.is_empty()
            && self
                .entries
                .iter()
                .any(|(_, accepted)| accepted.audio_range.overlaps(range))
        {
            return false;
        }
        if !range.is_empty()
            && self
                .last_audio_end
                .is_some_and(|last_end| range.start_sample < last_end)
        {
            return false;
        }

        self.last_seq += 1;
        if !range.is_empty() {
            self.last_audio_end = Some(range.end_sample);
        }
        self.delivered_text.push_str(&span.text);
        self.entries.push((self.last_seq, span));
        true
    }

    /// 投递保留窗口外的最老未投递段（接受第 retention+1 段后触发）。
    ///
    /// 0.23.14：只推进 `queued` 水位——投递 ≠ 交付；成功 ack 前（见
    /// [`DictationLedger::ack_delivered`]）这些段仍留在浮窗 confirmed
    /// 窗口。返回本次投递的段（按 seq 升序）；空 Vec = 窗口未满。
    pub fn queue_flushable(&mut self) -> Vec<(u64, DraftSpan)> {
        let mut out = Vec::new();
        while self.entries.len() > self.queued + self.retention {
            let (seq, span) = self.entries[self.queued].clone();
            self.flushed_text.push_str(&span.text);
            self.queued += 1;
            out.push((seq, span));
        }
        out
    }

    /// 注入 worker 的交付确认：`upto_seq = None` 确认全部已投递段
    /// （终态 job / 同步交付）；`Some(seq)` 确认所有 seq' ≤ seq 的段。
    /// 只有 ack 之后段才从浮窗 confirmed 窗口退场。
    pub fn ack_delivered(&mut self, upto_seq: Option<u64>) {
        match upto_seq {
            None => self.acked = self.queued,
            Some(seq) => {
                let target = self
                    .entries
                    .iter()
                    .take(self.queued)
                    .filter(|(entry_seq, _)| *entry_seq <= seq)
                    .count();
                self.acked = self.acked.max(target);
            }
        }
    }

    /// 投递全部未投递段（会话终态：stop/error/cancel 补交）。
    ///
    /// 同样只推进 `queued`——终态 job 交付成功后以 `ack_delivered(None)`
    /// 收口；此前这些段保持可见。
    pub fn take_pending(&mut self) -> Vec<(u64, DraftSpan)> {
        let mut out = Vec::new();
        while self.queued < self.entries.len() {
            let (seq, span) = self.entries[self.queued].clone();
            self.flushed_text.push_str(&span.text);
            self.queued += 1;
            out.push((seq, span));
        }
        out
    }

    /// 未交付段文本（浮窗 confirmed 窗口展示：Editor 为最近 1 段；
    /// G2 为 queued 未 ack 的待交付段——注入成功前保持可见）。
    pub fn pending_text(&self) -> String {
        self.entries[self.acked..]
            .iter()
            .map(|(_, span)| span.text.as_str())
            .collect()
    }

    /// 已投递文本（含未 ack——worker 保序必达；调用方做 Final 前缀
    /// 一致性核对与终态剩余裁剪的基准）。
    pub fn flushed_text(&self) -> &str {
        &self.flushed_text
    }

    /// 从累积 confirmed 快照推导新增段（Legacy 兼容路径）。
    ///
    /// 返回 `Some((seq, text))` 表示 confirmed 相比上次交付有新增；
    /// `None` 表示无新增。confirmed 长度回退（不应发生）时保守忽略并
    /// 由调用方记日志——已交付给前端的内容绝不回撤。
    pub fn extract_delta(&mut self, confirmed: &str) -> Option<(u64, String)> {
        let confirmed_chars = confirmed.chars().count();
        let delivered_chars = self.delivered_text.chars().count();
        if confirmed_chars <= delivered_chars {
            return None;
        }
        // confirmed 以 delivered 为前缀是正常路径；按字符边界取增量。
        let delta: String = confirmed.chars().skip(delivered_chars).collect();
        self.delivered_text.push_str(&delta);
        self.last_seq += 1;
        Some((self.last_seq, delta))
    }

    /// 从 Final 全文推导尾段（confirmed + 尾段定稿）。
    ///
    /// 正常路径 Final 以已交付文本开头，剩余部分即尾段；前缀不一致
    /// （引擎后处理差异等异常）时退化为最长公共字符前缀裁剪，宁可多交
    /// 少交几个字符也不丢弃或重复整段。
    ///
    /// 注意：只推导尾段文本，不冲刷 pending 段（冲刷由调用方先行
    /// `take_pending` 完成，保证尾段以全部已接受文本为基准）。
    pub fn extract_final_tail(&mut self, final_text: &str) -> Option<(u64, String)> {
        let delta = self.trim_delivered_prefix(final_text)?;
        self.delivered_text.push_str(&delta);
        self.last_seq += 1;
        Some((self.last_seq, delta))
    }

    /// G2 终态剩余交付文本：Final 全文剥去已投递前缀。
    ///
    /// 覆盖三种来源：未投递 pending 段 + 引擎 terminal finalize 直接写入、
    /// 未逐段上报的 span + 尾段。正常路径 Final 以 `flushed_text`（已投递
    /// worker 的文本，含未 ack——worker 保序必达）开头；不一致（引擎整段
    /// 重排等异常）时退化为最长公共字符前缀裁剪——绝不重复已上屏文本。
    /// Final 为空（finalize 失败/超时）时补交 pending。
    pub fn remaining_from_final(&mut self, final_text: &str) -> String {
        if final_text.is_empty() {
            return self
                .take_pending()
                .iter()
                .map(|(_, span)| span.text.as_str())
                .collect();
        }
        let remaining = if final_text.starts_with(self.flushed_text.as_str()) {
            final_text[self.flushed_text.len()..].to_string()
        } else {
            self.trim_flushed_prefix_common(final_text)
        };
        self.flushed_text.push_str(&remaining);
        self.queued = self.entries.len();
        remaining
    }

    /// Final 剥去 `delivered_text` 前缀；空剩余返回 `None`。
    fn trim_delivered_prefix(&self, final_text: &str) -> Option<String> {
        if final_text.is_empty() {
            return None;
        }
        if final_text.starts_with(self.delivered_text.as_str()) {
            let delta = &final_text[self.delivered_text.len()..];
            return (!delta.is_empty()).then(|| delta.to_string());
        }
        // 兜底：最长公共字符前缀裁剪
        let common = final_text
            .chars()
            .zip(self.delivered_text.chars())
            .take_while(|(a, b)| a == b)
            .count();
        Some(self.slice_at_char(final_text, common))
            .filter(|delta| !delta.is_empty())
    }

    /// `flushed_text` 与 Final 无前缀关系时的兜底：按最长公共字符前缀裁剪。
    fn trim_flushed_prefix_common(&self, final_text: &str) -> String {
        let common = final_text
            .chars()
            .zip(self.flushed_text.chars())
            .take_while(|(a, b)| a == b)
            .count();
        self.slice_at_char(final_text, common)
    }

    /// 按字符序号切字节边界（UTF-8 安全）。
    fn slice_at_char(&self, text: &str, char_index: usize) -> String {
        let skip_bytes = text
            .char_indices()
            .nth(char_index)
            .map(|(i, _)| i)
            .unwrap_or(text.len());
        text[skip_bytes..].to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::DictationLedger;
    use crate::domain::stt::{AudioRange, DraftSpan};

    fn span(id: u64, start: u64, end: u64, text: &str) -> DraftSpan {
        DraftSpan::new(id, AudioRange::new(start, end), text, 1)
    }

    #[test]
    fn delta_grows_monotonically_with_seq() {
        let mut t = DictationLedger::new(1);
        assert_eq!(t.last_seq(), 0);
        assert_eq!(t.extract_delta(""), None);
        assert_eq!(
            t.extract_delta("第一句。"),
            Some((1, "第一句。".to_string()))
        );
        assert_eq!(t.extract_delta("第一句。"), None, "无新增不产段");
        assert_eq!(
            t.extract_delta("第一句。第二句。"),
            Some((2, "第二句。".to_string()))
        );
        assert_eq!(t.last_seq(), 2);
    }

    #[test]
    fn shrink_is_ignored() {
        let mut t = DictationLedger::new(1);
        assert_eq!(
            t.extract_delta("已交付的确认文本"),
            Some((1, "已交付的确认文本".to_string()))
        );
        // confirmed 回退（不应发生）：忽略，不产段、不回撤
        assert_eq!(t.extract_delta("已交"), None);
        assert_eq!(t.last_seq(), 1);
    }

    #[test]
    fn final_tail_trims_delivered_prefix() {
        let mut t = DictationLedger::new(1);
        t.extract_delta("第一句。第二句。");
        // Final = 全部 confirmed + 尾段定稿
        assert_eq!(
            t.extract_final_tail("第一句。第二句。最后一句。"),
            Some((2, "最后一句。".to_string()))
        );
        // Final 与已交付一致 → 无收尾段
        assert_eq!(t.extract_final_tail("第一句。第二句。最后一句。"), None);
    }

    #[test]
    fn final_tail_empty_and_fresh_cases() {
        let mut t = DictationLedger::new(1);
        assert_eq!(t.extract_final_tail(""), None, "空 Final 不产段");
        // 无任何增量交付：Final 全文即收尾段
        assert_eq!(
            t.extract_final_tail("只有一句。"),
            Some((1, "只有一句。".to_string()))
        );
    }

    #[test]
    fn final_tail_falls_back_to_common_prefix_on_mismatch() {
        let mut t = DictationLedger::new(1);
        t.extract_delta("前缀文本。");
        // Final 前缀与已交付不一致（异常路径）：按最长公共字符前缀（"前缀文"3 字符）裁剪
        assert_eq!(
            t.extract_final_tail("前缀文 prêt。尾巴"),
            Some((2, " prêt。尾巴".to_string()))
        );
    }

    #[test]
    fn final_tail_all_overlap_yields_none() {
        let mut t = DictationLedger::new(1);
        t.extract_delta("完全一致。");
        // 不以前缀开头但逐字符完全重合（异常路径）：裁剪后为空 → 不产段
        assert_eq!(t.extract_final_tail("完全一致。"), None);
    }

    #[test]
    fn multibyte_boundary_safe() {
        let mut t = DictationLedger::new(1);
        // 中文 + emoji 混合，确认按字符（而非字节）切分
        assert_eq!(t.extract_delta("你好🌍"), Some((1, "你好🌍".to_string())));
        assert_eq!(
            t.extract_delta("你好🌍再见🎯"),
            Some((2, "再见🎯".to_string()))
        );
    }

    #[test]
    fn typed_draft_spans_are_ordered_and_deduplicated_by_audio_identity() {
        let mut ledger = DictationLedger::new(1);
        assert!(ledger.accept_draft_span(span(11, 0, 80_000, "第一段。")));
        assert!(!ledger.accept_draft_span(span(11, 0, 80_000, "第一段。")), "重复 span 不得追加");

        assert!(
            !ledger.accept_draft_span(span(12, 79_000, 120_000, "重叠。")),
            "重叠音频不做文本合并"
        );

        assert!(ledger.accept_draft_span(span(12, 80_000, 160_000, "第二段。")));
        assert_eq!(ledger.last_seq(), 2);
        assert_eq!(ledger.pending_text(), "第一段。第二段。");
    }

    #[test]
    fn retention_window_releases_oldest_span() {
        // 通用窗口语义（retention=2 便于观察"窗口未满不冲刷"）：第 3 段
        // 定稿时最老一段可投递。生产配置为 G2 retention=0、Editor retention=1。
        let mut ledger = DictationLedger::new(2);
        assert!(ledger.accept_draft_span(span(1, 0, 80_000, "一。")));
        assert!(ledger.accept_draft_span(span(2, 80_000, 160_000, "二。")));
        assert!(ledger.queue_flushable().is_empty(), "窗口未满不投递");
        assert_eq!(ledger.pending_text(), "一。二。");

        assert!(ledger.accept_draft_span(span(3, 160_000, 240_000, "三。")));
        let flushed = ledger.queue_flushable();
        assert_eq!(flushed.len(), 1);
        assert_eq!(flushed[0].1.text, "一。");
        // 0.23.14：投递未 ack 前保持可见；ack 后才从 confirmed 窗口退场
        assert_eq!(
            ledger.pending_text(),
            "一。二。三。",
            "queued 未 ack 的段保持可见"
        );
        ledger.ack_delivered(Some(1));
        assert_eq!(ledger.pending_text(), "二。三。");
        assert_eq!(ledger.flushed_text(), "一。");
    }

    #[test]
    fn editor_retention_one_flushes_previous_on_next() {
        // Editor 语义：retention=1，第 2 段定稿时前一段写正文。
        // Editor 段经事件同步交付，投递即 ack（voice.rs 包装层调用
        // ack_delivered(None)），pending_text 行为与 0.23.13 一致。
        let mut ledger = DictationLedger::new(1);
        assert!(ledger.accept_draft_span(span(1, 0, 80_000, "第一句。")));
        assert!(ledger.queue_flushable().is_empty(), "单段停留在窗口");
        assert!(ledger.accept_draft_span(span(2, 80_000, 160_000, "第二句。")));
        let flushed = ledger.queue_flushable();
        assert_eq!(flushed.len(), 1);
        assert_eq!(flushed[0].1.text, "第一句。");
        ledger.ack_delivered(None);
        assert_eq!(ledger.pending_text(), "第二句。");
    }

    #[test]
    fn take_pending_flushes_all_at_terminal() {
        let mut ledger = DictationLedger::new(2);
        ledger.accept_draft_span(span(1, 0, 80_000, "一。"));
        ledger.accept_draft_span(span(2, 80_000, 160_000, "二。"));
        ledger.queue_flushable();
        let pending = ledger.take_pending();
        assert_eq!(pending.len(), 2);
        assert!(ledger.take_pending().is_empty(), "终态后无剩余");
        // 终态 job 交付成功 ack 前，段保持可见（deferred 补交窗口）
        assert_eq!(ledger.pending_text(), "一。二。");
        ledger.ack_delivered(None);
        assert!(ledger.pending_text().is_empty());
    }

    #[test]
    fn remaining_from_final_strips_flushed_prefix() {
        // 渐进上屏 1 段后 Final 全文到达：剩余 = pending + 尾段 + 未上报 span
        let mut ledger = DictationLedger::new(2);
        ledger.accept_draft_span(span(1, 0, 80_000, "一。"));
        ledger.accept_draft_span(span(2, 80_000, 160_000, "二。"));
        ledger.accept_draft_span(span(3, 160_000, 240_000, "三。"));
        ledger.queue_flushable(); // "一。" 已投递 worker（保序必达）

        let remaining = ledger.remaining_from_final("一。二。三。四。");
        assert_eq!(remaining, "二。三。四。");
        assert_eq!(ledger.flushed_text(), "一。二。三。四。");
        // 终态后重复调用无剩余
        assert_eq!(ledger.remaining_from_final("一。二。三。四。"), "");
    }

    #[test]
    fn remaining_from_final_empty_final_flushes_pending() {
        // finalize 失败/超时返回空串：已定稿 pending 仍要补交
        let mut ledger = DictationLedger::new(2);
        ledger.accept_draft_span(span(1, 0, 80_000, "一。"));
        ledger.accept_draft_span(span(2, 80_000, 160_000, "二。"));
        ledger.queue_flushable(); // retention=2 未满，无投递
        assert_eq!(ledger.remaining_from_final(""), "一。二。");
    }

    #[test]
    fn remaining_from_final_falls_back_to_common_prefix() {
        // 引擎整段重排（Final 不以已上屏为前缀）：按公共前缀裁剪，不重复
        let mut ledger = DictationLedger::new(1);
        ledger.accept_draft_span(span(1, 0, 80_000, "已上屏。"));
        ledger.take_pending(); // 全部冲刷
        // 公共前缀 = "已上"，剩余 = "重排。"
        assert_eq!(ledger.remaining_from_final("已重排。"), "重排。");
    }

    // ── 0.23.14 G2 注入 ack 双水位 ──

    /// G2 浮窗保留 queued 段直到注入 ack：投递后 confirmed 窗口不清空，
    /// ack 成功（含 deferred 后终态补交）才退场。
    #[test]
    fn g2_queued_draft_stays_visible_until_delivery_ack() {
        let mut ledger = DictationLedger::new(0);
        assert!(ledger.accept_draft_span(span(1, 0, 80_000, "第一句。")));
        let queued = ledger.queue_flushable();
        assert_eq!(queued.len(), 1);
        assert_eq!(
            ledger.pending_text(),
            "第一句。",
            "queued 未 ack 不得从浮窗退场"
        );
        ledger.ack_delivered(Some(1));
        assert!(ledger.pending_text().is_empty(), "ack 后才清空");
    }

    /// deferred（前台漂移/Unicode 失败挂起）不算已交付：不推进 ack，
    /// 段保持可见；终态补交按 queued 前缀裁剪——deferred 文本由同一
    /// worker 保序补交，不得因未 ack 而重复上屏。
    #[test]
    fn g2_deferred_flush_is_not_counted_as_delivered() {
        let mut ledger = DictationLedger::new(0);
        assert!(ledger.accept_draft_span(span(1, 0, 80_000, "已投递。")));
        ledger.queue_flushable();
        // worker 挂起：无 ack
        assert_eq!(ledger.pending_text(), "已投递。");

        // Final 到达：剩余剥去 queued 前缀（含 deferred），只补交尾段
        let remaining = ledger.remaining_from_final("已投递。尾段。");
        assert_eq!(remaining, "尾段。", "deferred 文本不因未 ack 重复上屏");
    }

    /// ack 按 seq 单调推进：迟到/重复 ack 不得回退已确认水位。
    #[test]
    fn ack_delivered_is_monotonic_and_bounded_by_queued() {
        let mut ledger = DictationLedger::new(0);
        ledger.accept_draft_span(span(1, 0, 80_000, "一。"));
        ledger.accept_draft_span(span(2, 80_000, 160_000, "二。"));
        ledger.accept_draft_span(span(3, 160_000, 240_000, "三。"));
        ledger.queue_flushable(); // 全部投递
        ledger.ack_delivered(Some(2));
        assert_eq!(ledger.pending_text(), "三。");
        // 迟到的低 seq ack 不得回退
        ledger.ack_delivered(Some(1));
        assert_eq!(ledger.pending_text(), "三。");
        // 终态全量 ack
        ledger.ack_delivered(None);
        assert!(ledger.pending_text().is_empty());
    }

    #[test]
    fn final_tail_after_pending_flush_keeps_continuity() {
        // Editor Final 路径：先 take_pending 再取尾段，seq 连续且不重叠
        let mut ledger = DictationLedger::new(1);
        ledger.accept_draft_span(span(1, 0, 80_000, "已经交付。"));
        let pending = ledger.take_pending();
        assert_eq!(pending.len(), 1);
        assert_eq!(
            ledger.extract_final_tail("已经交付。尾段。"),
            Some((2, "尾段。".to_string()))
        );
    }
}
