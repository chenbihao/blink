//! 同行聚合（视觉行合并 + 阅读顺序恢复）与智能拼接。
//!
//! 0.22 收尾：从 `ocr_engine.rs` 按职责拆出。
//!
//! 纯函数，不依赖 infra / tauri，可独立测试。

use super::types::{OcrLine, OcrRect, OcrResult, OcrWord};

// ── 布局归一化诊断统计 ─────────────────────────────────────────────────────
//
// 纯函数返回 `LayoutDiagnostics`，由 app/client 层记录 DEBUG 日志。
// 纯函数不依赖 tracing，保持 domain 层框架无关。

/// 布局归一化诊断统计。纯数据，不含 OCR 文本或敏感坐标。
#[derive(Debug, Clone, Default)]
#[allow(dead_code)] // 字段在 app/ocr_coordinator/mapping.rs 中通过 tracing::debug! 消费
pub struct LayoutDiagnostics {
    pub source_lines: usize,
    pub source_words: usize,
    pub native_word_boxes: usize,
    pub fallback_word_boxes: usize,
    pub grouped_lines: usize,
    pub merged_line_count: usize,
    pub output_text_chars: usize,
    pub layout_elapsed_ms: u64,
    // 判定统计
    pub rejected_y_center: usize,
    pub rejected_overlap: usize,
    pub rejected_height_ratio: usize,
    pub assigned_existing_line: usize,
    pub created_new_line: usize,
    pub large_horizontal_gaps: usize,
    pub inserted_extra_spaces: usize,
    pub inserted_blank_lines: usize,
}

// ── 同行聚合参数 ───────────────────────────────────────────────────────────

/// 同行判定的参数。所有阈值均为文字高度的比例，不使用固定像素。
///
/// - `Y_CENTER_RATIO`：元素 Y 中心距离 / 行参考高度 > 此值 → 不同行。
/// - `V_OVERLAP_MIN`：垂直重叠率（交集 / 较小高度）< 此值 → 不同行。
/// - `HEIGHT_RATIO_MAX`：两元素高度比超过此值 → 视为字号差异过大，可能不同行。
///   注意：放宽到 3.0 允许不同字号但基线相近的文本合理合并。
///
/// 注意：不再有 `X_GAP_RATIO_MAX` 否决同行。水平距离不再用于否决同行判定。
/// 大水平间距只会影响行内空格数（见 `join_words_intra_line_with_gaps`）。
const Y_CENTER_RATIO: f64 = 0.65;
const V_OVERLAP_MIN: f64 = 0.15;
const HEIGHT_RATIO_MAX: f64 = 3.0;

/// 垂直重叠率：两个 rect 在 Y 轴上交集高度 / 较小元素高度。
fn v_overlap_ratio(a: OcrRect, b: OcrRect) -> f64 {
    let overlap_top = a.y.max(b.y);
    let overlap_bot = a.bottom().min(b.bottom());
    let overlap = (overlap_bot - overlap_top).max(0) as f64;
    let min_h = a.height_f().min(b.height_f());
    if min_h <= 0.0 { 0.0 } else { overlap / min_h }
}

/// 行拒绝原因——用于诊断统计。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LineRejectReason {
    Accepted,
    YCenter,
    Overlap,
    HeightRatio,
    ZeroHeight,
    EmptyLine,
}

/// 计算若干 rect 的 union。返回 `None` 表示输入为空或全 zero。
pub(crate) fn rect_union(rects: impl Iterator<Item = OcrRect>) -> Option<OcrRect> {
    let mut it = rects.filter(|r| !r.is_zero());
    let first = it.next()?;
    let mut min_x = first.x;
    let mut min_y = first.y;
    let mut max_x = first.x.saturating_add(first.w as i32);
    let mut max_y = first.y.saturating_add(first.h as i32);
    for r in it {
        min_x = min_x.min(r.x);
        min_y = min_y.min(r.y);
        max_x = max_x.max(r.x.saturating_add(r.w as i32));
        max_y = max_y.max(r.y.saturating_add(r.h as i32));
    }
    Some(OcrRect {
        x: min_x,
        y: min_y,
        w: (max_x - min_x).max(0) as u32,
        h: (max_y - min_y).max(0) as u32,
    })
}

/// 判定元素 `word` 是否属于已有视觉行 `line_rects`。
///
/// 与行中**所有**元素比较，只要与任一元素满足同行条件即可。
/// 这比只与行末元素比较更稳健——字号不同、检测框 Y 偏移、相邻行框交错时
/// 不会错误拆行或串行。
///
/// 返回 `(is_same, reject_reason)`。`reject_reason` 用于诊断统计。
fn is_word_in_line(word_rect: OcrRect, line_rects: &[OcrRect]) -> (bool, LineRejectReason) {
    let hw = word_rect.height_f();
    if hw <= 0.0 {
        return (false, LineRejectReason::ZeroHeight);
    }

    for &lr in line_rects {
        let lh = lr.height_f();
        if lh <= 0.0 {
            continue;
        }

        // 字号差异过大
        let height_ratio = hw.max(lh) / hw.min(lh);
        if height_ratio > HEIGHT_RATIO_MAX {
            continue; // 尝试行中下一个元素
        }

        let avg_h = (hw + lh) / 2.0;

        // Y 中心距离
        let dy = (word_rect.center_y() - lr.center_y()).abs();
        if dy > avg_h * Y_CENTER_RATIO {
            continue; // 尝试行中下一个元素
        }

        // 垂直重叠率
        if v_overlap_ratio(word_rect, lr) < V_OVERLAP_MIN {
            continue; // 尝试行中下一个元素
        }

        // 所有条件满足——同行
        return (true, LineRejectReason::Accepted);
    }

    // 与行中所有元素都不满足同行条件
    // 判断主要拒绝原因——以与行末元素（代表基线）的比较为准
    if let Some(&last) = line_rects.last() {
        let lh = last.height_f();
        if lh > 0.0 {
            let height_ratio = hw.max(lh) / hw.min(lh);
            if height_ratio > HEIGHT_RATIO_MAX {
                return (false, LineRejectReason::HeightRatio);
            }
            let avg_h = (hw + lh) / 2.0;
            let dy = (word_rect.center_y() - last.center_y()).abs();
            if dy > avg_h * Y_CENTER_RATIO {
                return (false, LineRejectReason::YCenter);
            }
            return (false, LineRejectReason::Overlap);
        }
    }
    (false, LineRejectReason::EmptyLine)
}

/// 把 flat word 列表按视觉行分组，返回每个 word 的新 line_index（0-based）
/// 和诊断统计。
///
/// 算法（真正的行聚类，0.22.7 升级）：
/// 1. 先按 (center_y, center_x) 排序，恢复从上到下、从左到右的阅读顺序。
/// 2. 顺序遍历，对每个 word 检查是否与当前行的**所有**元素满足同行条件。
///    同行 → 分配当前 line_id + 记录 `assigned_existing_line`。
///    不同行 → 新建行 + 记录 `created_new_line`。
/// 3. 水平距离不否决同行——大间距只标记 `large_horizontal_gaps`。
///
/// 输入 `words` 的 `line_index` 字段被忽略（它是待分组的原始数据）。
/// 输出 Vec<usize> 与输入等长，`output[i]` 是 `words[i]` 的新 line_index。
///
/// 独立可测——下方 `tests` 模块覆盖全部约定场景。
#[cfg(test)]
pub fn group_words_into_lines(words: &[OcrWord]) -> Vec<usize> {
    group_words_into_lines_with_diag(words).0
}

/// `group_words_into_lines` 的诊断版本——返回 line_index 和统计。
pub fn group_words_into_lines_with_diag(words: &[OcrWord]) -> (Vec<usize>, LayoutDiagnostics) {
    let mut diag = LayoutDiagnostics {
        source_words: words.len(),
        ..Default::default()
    };

    if words.is_empty() {
        return (Vec::new(), diag);
    }

    // 按 (center_y, center_x) 排序恢复阅读顺序
    let mut indexed: Vec<(usize, f64, f64)> = words
        .iter()
        .enumerate()
        .map(|(i, w)| (i, w.bounding_rect.center_y(), w.bounding_rect.center_x()))
        .collect();
    indexed.sort_by(|a, b| {
        a.1.partial_cmp(&b.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.2.partial_cmp(&b.2).unwrap_or(std::cmp::Ordering::Equal))
    });

    let mut result = vec![0usize; words.len()];
    let mut current_line = 0usize;
    // 每行存储属于该行的元素 rect 列表
    let mut line_rects: Vec<Vec<OcrRect>> = Vec::new();
    line_rects.push(vec![words[indexed[0].0].bounding_rect]);
    result[indexed[0].0] = 0;
    diag.created_new_line = 1;

    for &(idx, _, _) in indexed.iter().skip(1) {
        let cur_rect = words[idx].bounding_rect;

        // 先取出 last_rect 用于大间距标记（避免与可变借用冲突）
        let last_rect = line_rects[current_line].last().copied();
        let (is_same, reason) = is_word_in_line(cur_rect, &line_rects[current_line]);

        if is_same {
            result[idx] = current_line;
            line_rects[current_line].push(cur_rect);
            diag.assigned_existing_line += 1;

            // 大水平间距标记（不否决同行）
            if let Some(lr) = last_rect {
                let gap_x = horizontal_gap(lr, cur_rect);
                let avg_h = (lr.height_f() + cur_rect.height_f()) / 2.0;
                if avg_h > 0.0 && gap_x as f64 > avg_h * 3.0 {
                    diag.large_horizontal_gaps += 1;
                }
            }
        } else {
            // 记录拒绝原因
            match reason {
                LineRejectReason::YCenter => diag.rejected_y_center += 1,
                LineRejectReason::Overlap => diag.rejected_overlap += 1,
                LineRejectReason::HeightRatio => diag.rejected_height_ratio += 1,
                _ => {}
            }

            current_line += 1;
            result[idx] = current_line;
            line_rects.push(vec![cur_rect]);
            diag.created_new_line += 1;
        }
    }

    diag.grouped_lines = current_line + 1;

    (result, diag)
}

/// 计算两个 rect 的水平间距（不重叠时为正数，重叠时为 0）。
fn horizontal_gap(a: OcrRect, b: OcrRect) -> i32 {
    let a_right = a.right();
    let b_right = b.right();
    if a_right <= b.x {
        b.x - a_right
    } else if b_right <= a.x {
        a.x - b_right
    } else {
        0
    }
}

/// 行内拼接结果——包含拼接文本和每个 word 的字符范围。
///
/// `char_ranges[i]` = `(start, end)` 表示第 i 个 word 在拼接后
/// `text` 中的 Rust **字符**索引范围（`text.chars().take(end).skip(start)`
/// 恰好得到该 word 的文本）。这是单一真源——前端从该范围生成
/// UTF-16 offset 供 textarea selection API 使用。
struct IntraLineResult {
    text: String,
    char_ranges: Vec<(usize, usize)>,
}

/// 估算单字符宽度——用于把横向 gap 映射为空格数。
///
/// 以 rect 宽度 / 文本字符数为估算，CJK 字符通常占一个全宽。
fn estimate_char_width(rect: OcrRect, text: &str) -> f64 {
    let char_count = text.chars().count().max(1);
    rect.w as f64 / char_count as f64
}

/// 估算行参考字符宽度——取行内所有 word 的中位数。
fn estimate_line_char_width(words: &[&OcrWord]) -> f64 {
    let mut widths: Vec<f64> = words
        .iter()
        .filter(|w| !w.text.is_empty())
        .map(|w| estimate_char_width(w.bounding_rect, &w.text))
        .filter(|&w| w > 0.0)
        .collect();
    if widths.is_empty() {
        return 10.0; // 兜底
    }
    widths.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    widths[widths.len() / 2]
}

/// 判定字符是否属于"CJK / 全角"表意族。走贴一起不加空格的规则。
///
/// 覆盖：中日韩汉字、日文假名、汉字扩展区、全角标点。走 char 分类避免
/// 依赖外部 unicode crate。
fn is_cjk_ish(c: char) -> bool {
    // CJK 统一表意文字 + 扩展 A + 扩展 B + 兼容
    matches!(
        c as u32,
        0x3400..=0x4DBF          // CJK Unified Ideographs Ext A
        | 0x4E00..=0x9FFF        // CJK Unified Ideographs
        | 0x20000..=0x2A6DF      // Ext B
        | 0xF900..=0xFAFF        // Compatibility
        | 0x3040..=0x309F        // Hiragana
        | 0x30A0..=0x30FF        // Katakana
        | 0xAC00..=0xD7AF        // Hangul Syllables
        | 0x3000..=0x303F        // CJK Symbols & Punctuation(全角括号/句号等)
        | 0xFF00..=0xFFEF        // Halfwidth & Fullwidth Forms
    )
}

/// 判定字符是否属于 word-continuous 的西文/数字族。相邻两 word 都是这种时中间加空格。
fn is_latin_word_char(c: char) -> bool {
    c.is_ascii_alphanumeric()
        || matches!(c as u32,
            0x00C0..=0x024F      // Latin-1 Supplement + Extended A/B(带音标欧洲字符)
            | 0x1E00..=0x1EFF    // Latin Extended Additional
        )
}

/// 判定 word 首字符/尾字符属于哪一族(CJK / Latin / 其它)。用于相邻 word 拼接决策。
enum WordKind {
    Cjk,
    Latin,
    Other, // 标点、符号、空 word 等；两侧都是 Other 时按 Latin 规则加空格
}

fn head_kind(s: &str) -> WordKind {
    match s.chars().next() {
        Some(c) if is_cjk_ish(c) => WordKind::Cjk,
        Some(c) if is_latin_word_char(c) => WordKind::Latin,
        _ => WordKind::Other,
    }
}

fn tail_kind(s: &str) -> WordKind {
    match s.chars().next_back() {
        Some(c) if is_cjk_ish(c) => WordKind::Cjk,
        Some(c) if is_latin_word_char(c) => WordKind::Latin,
        _ => WordKind::Other,
    }
}

/// 行内 word 文本拼接——根据横向 gap 和字符宽度映射多空格（0.22.7 升级）。
///
/// 规则：
/// - CJK ↔ CJK：不加空格（紧邻时）
/// - Latin ↔ Latin：至少一个空格
/// - CJK ↔ Latin：不加空格
/// - 大水平间距：根据 gap / char_width 比值映射为多个空格
/// - 空格上限：`MAX_SPACES` = 8，避免生成数百个空格
///
/// 同时返回每个 word 在拼接文本中的 Rust 字符范围 `{start, end}`，
/// 作为单一真源供前端生成 UTF-16 offset。
fn join_words_intra_line_with_gaps(
    words: &[&OcrWord],
    diag: &mut LayoutDiagnostics,
) -> IntraLineResult {
    if words.is_empty() {
        return IntraLineResult {
            text: String::new(),
            char_ranges: Vec::new(),
        };
    }

    const MAX_SPACES: usize = 8;
    let char_width = estimate_line_char_width(words);

    let mut text = String::new();
    let mut char_ranges: Vec<(usize, usize)> = Vec::with_capacity(words.len());
    let mut prev_tail: Option<WordKind> = None;
    let mut prev_rect: Option<OcrRect> = None;

    for w in words {
        if w.text.is_empty() {
            char_ranges.push((text.chars().count(), text.chars().count()));
            continue;
        }

        // 计算空格
        if let (Some(tk), Some(pr)) = (&prev_tail, prev_rect) {
            let hk = head_kind(&w.text);
            let gap = horizontal_gap(pr, w.bounding_rect).max(0) as f64;

            // 基本空格规则
            let base_space = match (tk, &hk) {
                (WordKind::Cjk, WordKind::Cjk) => {
                    // CJK 紧邻：gap 很小时不加空格
                    if gap > 0.0 && gap > char_width * 0.5 {
                        1
                    } else {
                        0
                    }
                }
                (WordKind::Cjk, _) | (_, WordKind::Cjk) => {
                    // CJK ↔ Latin：紧邻不加空格，较大间距加一个
                    if gap > char_width * 2.0 { 1 } else { 0 }
                }
                _ => {
                    // Latin/Latin, Latin/Other, Other/Other：至少一个
                    1
                }
            };

            // 大间距转多空格
            if gap > 0.0 && char_width > 0.0 {
                let gap_spaces = (gap / char_width).round() as usize;
                if gap_spaces > 1 {
                    let extra = gap_spaces.min(MAX_SPACES).max(base_space);
                    if extra > base_space {
                        diag.inserted_extra_spaces += extra - base_space;
                    }
                    for _ in 0..extra {
                        text.push(' ');
                    }
                } else {
                    for _ in 0..base_space {
                        text.push(' ');
                    }
                }
            } else {
                for _ in 0..base_space {
                    text.push(' ');
                }
            }
        }

        let start = text.chars().count();
        text.push_str(&w.text);
        let end = text.chars().count();
        char_ranges.push((start, end));

        prev_tail = Some(tail_kind(&w.text));
        prev_rect = Some(w.bounding_rect);
    }

    IntraLineResult { text, char_ranges }
}

/// 根据新 line_index 重新构建 lines / words / text + char_ranges。
///
/// - words 按新 line_index 重新排列（line 内按 X 排序）。
/// - lines 的 text 从 word text 拼接得到（走 `join_words_intra_line_with_gaps`）。
/// - lines 的 rect = 该行 words 的 union。
/// - lines 的 word_indices 指回重排后的 flat words 数组。
/// - 全文 text 走 `join_words_smart_with_gaps`，行间根据纵向 gap 决定换行/空行。
///
/// 这是对 `map_raw_to_domain` 和 PaddleOCR `extract_results` 的共享语义。
pub fn rebuild_with_line_grouping(words: Vec<OcrWord>, text_angle: Option<f64>) -> OcrResult {
    rebuild_with_line_grouping_and_diag(words, text_angle).0
}

/// `rebuild_with_line_grouping` 的诊断版本——返回 `OcrResult` 和 `LayoutDiagnostics`。
pub fn rebuild_with_line_grouping_and_diag(
    mut words: Vec<OcrWord>,
    text_angle: Option<f64>,
) -> (OcrResult, LayoutDiagnostics) {
    let start = std::time::Instant::now();
    let mut diag = LayoutDiagnostics {
        source_words: words.len(),
        ..Default::default()
    };

    if words.is_empty() {
        return (
            OcrResult {
                backend_used: None,
                backend_fallback_reason: None,
                backend_degrade_hint: None,
                text: String::new(),
                lines: Vec::new(),
                words: Vec::new(),
                text_angle,
                char_ranges: Vec::new(),
                char_boxes: Vec::new(),
            },
            diag,
        );
    }

    // 1. 分组（带诊断）
    let (new_line_indices, group_diag) = group_words_into_lines_with_diag(&words);
    diag.rejected_y_center = group_diag.rejected_y_center;
    diag.rejected_overlap = group_diag.rejected_overlap;
    diag.rejected_height_ratio = group_diag.rejected_height_ratio;
    diag.assigned_existing_line = group_diag.assigned_existing_line;
    diag.created_new_line = group_diag.created_new_line;
    diag.large_horizontal_gaps = group_diag.large_horizontal_gaps;
    diag.grouped_lines = group_diag.grouped_lines;

    // 2. 回填新 line_index
    for (i, w) in words.iter_mut().enumerate() {
        w.line_index = new_line_indices[i];
    }

    // 3. 按 (line_index, center_x) 重排 words——恢复行内 X 顺序
    words.sort_by(|a, b| {
        a.line_index.cmp(&b.line_index).then(
            a.bounding_rect
                .center_x()
                .partial_cmp(&b.bounding_rect.center_x())
                .unwrap_or(std::cmp::Ordering::Equal),
        )
    });

    // 4. 重新分配 flat 数组中的连续 index + 构建 lines
    let mut lines: Vec<OcrLine> = Vec::new();
    let mut current_line_idx = 0usize;
    let mut current_word_indices: Vec<usize> = Vec::new();
    let mut current_rects: Vec<OcrRect> = Vec::new();

    // 先把 line_index 重新连续化
    let mut compact_map: Vec<usize> = vec![0; words.len()];
    let mut compact_idx = 0usize;
    for i in 0..words.len() {
        if i > 0 && words[i].line_index != words[i - 1].line_index {
            compact_idx += 1;
        }
        compact_map[i] = compact_idx;
    }
    for (i, w) in words.iter_mut().enumerate() {
        w.line_index = compact_map[i];
    }

    for (flat_idx, w) in words.iter().enumerate() {
        if w.line_index != current_line_idx {
            // flush 当前行
            let line_rect = rect_union(current_rects.iter().copied()).unwrap_or(OcrRect {
                x: 0,
                y: 0,
                w: 0,
                h: 0,
            });
            lines.push(OcrLine {
                text: String::new(),
                bounding_rect: line_rect,
                word_indices: current_word_indices.clone(),
            });
            current_word_indices.clear();
            current_rects.clear();
            current_line_idx = w.line_index;
        }
        current_word_indices.push(flat_idx);
        current_rects.push(w.bounding_rect);
    }
    // flush 最后一行
    if !current_word_indices.is_empty() {
        let line_rect = rect_union(current_rects.iter().copied()).unwrap_or(OcrRect {
            x: 0,
            y: 0,
            w: 0,
            h: 0,
        });
        lines.push(OcrLine {
            text: String::new(),
            bounding_rect: line_rect,
            word_indices: current_word_indices.clone(),
        });
    }

    // 5. 填 line.text（从 words 拼接 + 记录行内 char_ranges）
    let mut line_char_ranges: Vec<Vec<(usize, usize)>> = Vec::with_capacity(lines.len());

    for line in lines.iter_mut() {
        let line_words: Vec<&OcrWord> = line.word_indices.iter().map(|&i| &words[i]).collect();
        let result = join_words_intra_line_with_gaps(&line_words, &mut diag);
        line.text = result.text.clone();
        line_char_ranges.push(result.char_ranges);
    }

    // 6. 全文拼接——行间根据纵向 gap 决定换行/空行，同时偏移 char_ranges
    let (text, full_char_ranges) =
        join_lines_into_text(&lines, &words, &line_char_ranges, &mut diag);

    diag.merged_line_count = lines.len();
    diag.output_text_chars = text.chars().count();

    diag.layout_elapsed_ms = start.elapsed().as_millis() as u64;

    (
        OcrResult {
            backend_used: None,
            backend_fallback_reason: None,
            backend_degrade_hint: None,
            text,
            lines,
            words,
            text_angle,
            char_ranges: full_char_ranges,
            char_boxes: Vec::new(),
        },
        diag,
    )
}

/// 行间拼接——根据纵向 gap 和行高决定换行或额外空行。
///
/// 规则：
/// - 相邻行：一个换行 `\n`
/// - 纵向 gap 明显大于行高（> 1.5x）：插入额外空行（`\n\n`）
/// - 空行上限：`MAX_BLANK_LINES` = 3，避免极大空白生成大量换行
///
/// 同时把行内 char_ranges 转换为全文 char_ranges。
fn join_lines_into_text(
    lines: &[OcrLine],
    words: &[OcrWord],
    line_char_ranges: &[Vec<(usize, usize)>],
    diag: &mut LayoutDiagnostics,
) -> (String, Vec<(usize, usize)>) {
    const MAX_BLANK_LINES: usize = 3;

    if lines.is_empty() {
        return (String::new(), Vec::new());
    }

    let mut text = String::new();
    let mut full_ranges: Vec<(usize, usize)> = vec![(0, 0); words.len()];

    for (line_idx, line) in lines.iter().enumerate() {
        if line_idx > 0 {
            // 计算与前一行之间的纵向 gap
            let prev_line = &lines[line_idx - 1];
            let prev_bottom = prev_line.bounding_rect.bottom();
            let cur_top = line.bounding_rect.y;
            let v_gap = (cur_top - prev_bottom).max(0) as f64;

            // 典型行高估算
            let prev_h = prev_line.bounding_rect.height_f();
            let cur_h = line.bounding_rect.height_f();
            let avg_h = if prev_h > 0.0 && cur_h > 0.0 {
                (prev_h + cur_h) / 2.0
            } else if cur_h > 0.0 {
                cur_h
            } else {
                prev_h
            };

            text.push('\n');

            if avg_h > 0.0 && v_gap > avg_h * 1.5 {
                let blank_lines = ((v_gap / avg_h).round() as usize)
                    .saturating_sub(1)
                    .min(MAX_BLANK_LINES);
                for _ in 0..blank_lines {
                    text.push('\n');
                }
                if blank_lines > 0 {
                    diag.inserted_blank_lines += blank_lines;
                }
            }
        }

        // 拼接行文本 + 偏移 char_ranges
        let line_start = text.chars().count();
        text.push_str(&line.text);
        let line_end = text.chars().count();

        // 把行内 char_ranges 偏移为全文 char_ranges
        for (pos_in_line, &word_flat_idx) in line.word_indices.iter().enumerate() {
            if pos_in_line < line_char_ranges[line_idx].len() {
                let (s, e) = line_char_ranges[line_idx][pos_in_line];
                full_ranges[word_flat_idx] = (line_start + s, line_start + e);
            }
        }
        let _ = line_end;
    }

    (text, full_ranges)
}

/// 智能拼接 word 列表为完整文本（0.11.9-b，0.22.7 升级）。
///
/// **0.22.7 变更**：行内空格和行间换行/空行已由 `rebuild_with_line_grouping_and_diag`
/// 内部的 `join_words_intra_line_with_gaps` 和 `join_lines_into_text` 完成。
/// 本函数保留为兼容入口——直接从 `lines[i].text` 用 `\n` join，
/// 不再独立计算空格/换行。行内多空格和行间空行已在 line.text 拼接阶段处理。
///
/// 依赖 `words[i].line_index` 严格递增（0-based）。如果 words 为空但 lines 有内容
/// (SDK 只给 line 没给 word——不太可能但兜底)，退化为 line.text 用 `\n` join。
///
/// 独立可测（下方 `tests` 模块覆盖）。
#[cfg(test)]
pub fn join_words_smart(words: &[OcrWord], lines: &[OcrLine]) -> String {
    // 兜底：words 为空 → 用 lines.text
    if words.is_empty() {
        return lines
            .iter()
            .map(|l| l.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
    }

    // 0.22.7：line.text 已由 join_words_intra_line_with_gaps 拼好，
    // 行间换行/空行逻辑已在 rebuild_with_line_grouping_and_diag 中完成。
    // 此函数只需用 \n join lines.text 即可。
    // 如果调用方没走 rebuild（直接构造），则退化为简单换行 join。
    lines
        .iter()
        .map(|l| l.text.as_str())
        .collect::<Vec<_>>()
        .join("\n")
}
