//! 编辑器连续听写的段（segment）推导（0.23.3 §3.6）。
//!
//! 伪流式引擎在一次 STT session 内的 confirmed 文本单调增长（0.22.15
//! 事务化句尾：commit 才追加，rollback 不回退）。本 tracker 从累积的
//! confirmed 快照推导"新增段"并分配单调 seq；`finish_session` 产出的
//! Final 全文相对已交付前缀的剩余部分作为收尾段。
//!
//! 纯逻辑、框架无关：不依赖 tauri / tokio，单测覆盖重复、增长与
//! Final 与增量前缀不一致的兜底路径。

/// 编辑器听写段推导器。
///
/// 一次听写 epoch 一个实例；epoch 结束后整体丢弃（不跨 epoch 复用）。
#[derive(Debug, Default)]
pub struct EditorDictationTracker {
    /// 已交付 confirmed 文本（用于 Final 前缀精确裁剪）。
    delivered_text: String,
    /// 已交付的最大 seq；段号从 1 开始单调递增。
    last_seq: u64,
}

impl EditorDictationTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// 最后交付的 seq（0 = 尚无段）。
    pub fn last_seq(&self) -> u64 {
        self.last_seq
    }

    /// 从累积 confirmed 快照推导新增段。
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

    /// 从 Final 全文推导收尾段（confirmed + 尾段定稿）。
    ///
    /// 正常路径 Final 以已交付前缀开头，剩余部分即收尾段；前缀不一致
    /// （引擎后处理差异等异常）时退化为最长公共字符前缀裁剪，宁可多交
    /// 少交几个字符也不丢弃或重复整段。
    pub fn extract_final_delta(&mut self, final_text: &str) -> Option<(u64, String)> {
        if final_text.is_empty() {
            return None;
        }
        if final_text.starts_with(self.delivered_text.as_str()) {
            let delta = final_text[self.delivered_text.len()..].to_string();
            if delta.is_empty() {
                return None;
            }
            self.delivered_text.push_str(&delta);
            self.last_seq += 1;
            return Some((self.last_seq, delta));
        }
        // 兜底：最长公共字符前缀裁剪
        let common = final_text
            .chars()
            .zip(self.delivered_text.chars())
            .take_while(|(a, b)| a == b)
            .count();
        let skip_bytes = final_text
            .char_indices()
            .nth(common)
            .map(|(i, _)| i)
            .unwrap_or(final_text.len());
        let delta = final_text[skip_bytes..].to_string();
        if delta.is_empty() {
            return None;
        }
        self.delivered_text.push_str(&delta);
        self.last_seq += 1;
        Some((self.last_seq, delta))
    }
}

#[cfg(test)]
mod tests {
    use super::EditorDictationTracker;

    #[test]
    fn delta_grows_monotonically_with_seq() {
        let mut t = EditorDictationTracker::new();
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
        let mut t = EditorDictationTracker::new();
        assert_eq!(
            t.extract_delta("已交付的确认文本"),
            Some((1, "已交付的确认文本".to_string()))
        );
        // confirmed 回退（不应发生）：忽略，不产段、不回撤
        assert_eq!(t.extract_delta("已交"), None);
        assert_eq!(t.last_seq(), 1);
    }

    #[test]
    fn final_delta_trims_delivered_prefix() {
        let mut t = EditorDictationTracker::new();
        t.extract_delta("第一句。第二句。");
        // Final = 全部 confirmed + 尾段定稿
        assert_eq!(
            t.extract_final_delta("第一句。第二句。最后一句。"),
            Some((2, "最后一句。".to_string()))
        );
        // Final 与已交付一致 → 无收尾段
        assert_eq!(t.extract_final_delta("第一句。第二句。最后一句。"), None);
    }

    #[test]
    fn final_delta_empty_and_fresh_cases() {
        let mut t = EditorDictationTracker::new();
        assert_eq!(t.extract_final_delta(""), None, "空 Final 不产段");
        // 无任何增量交付：Final 全文即收尾段
        assert_eq!(
            t.extract_final_delta("只有一句。"),
            Some((1, "只有一句。".to_string()))
        );
    }

    #[test]
    fn final_delta_falls_back_to_common_prefix_on_mismatch() {
        let mut t = EditorDictationTracker::new();
        t.extract_delta("前缀文本。");
        // Final 前缀与已交付不一致（异常路径）：按最长公共字符前缀（"前缀文"3 字符）裁剪
        assert_eq!(
            t.extract_final_delta("前缀文 prêt。尾巴"),
            Some((2, " prêt。尾巴".to_string()))
        );
    }

    #[test]
    fn final_delta_all_overlap_yields_none() {
        let mut t = EditorDictationTracker::new();
        t.extract_delta("完全一致。");
        // 不以前缀开头但逐字符完全重合（异常路径）：裁剪后为空 → 不产段
        assert_eq!(t.extract_final_delta("完全一致。"), None);
    }

    #[test]
    fn multibyte_boundary_safe() {
        let mut t = EditorDictationTracker::new();
        // 中文 + emoji 混合，确认按字符（而非字节）切分
        assert_eq!(t.extract_delta("你好🌍"), Some((1, "你好🌍".to_string())));
        assert_eq!(
            t.extract_delta("你好🌍再见🎯"),
            Some((2, "再见🎯".to_string()))
        );
    }
}
