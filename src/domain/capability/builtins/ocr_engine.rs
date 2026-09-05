//! OCR Backend 抽象（0.11.7-c 引入，0.11.7-f 能力化，0.11.9-b word 级链路）。
//!
//! **架构**：
//! - `OcrBackend` trait — domain 侧 OCR 抽象（返回领域类型 `OcrResult`）
//! - `WindowsOcrBackendAdapter` — 包装 infra `PlatformOcrBackend`，做 raw → domain 映射
//! - `FakeOcrBackend` — 测试实现（返回预定义文本）
//! - `install_backend()` / `backend()` — 全局单例注入（对齐 ScreenshotBackend 模式）
//!
//! **0.14.7 W2**：WinRT 调用和原始 DTO 提取已迁至 `infra/platform/ocr/`。
//! 本文件只保留领域类型、智能拼接和 raw → domain 映射。
//!
//! **Windows.Media.Ocr 要求**：Windows 10 1809+，中文语言包已安装时自动识别中文。
//! 无中文语言包时仍可识别英文。
//!
//! **0.11.9-b word 级链路**：
//! - `OcrLine.Words()` → `OcrWord { text, rect, line_index }`（SDK 原生给的 word 级坐标）
//! - `OcrLine.bounding_rect` 真填（原为固定 `{0,0,0,0}`），用该行 words 的 union
//! - `OcrLine.word_indices` 指回 `OcrResult.words` flat 数组的 index 段
//! - `OcrResult.text` 走 `join_words_smart` 智能拼接（CJK↔CJK 无空格 / Latin↔Latin 有空格）
//!   替代 SDK `Text()` 的“每字夹空格”输出。前端“移除空格”按钮退化为兜底。

use std::sync::{Arc, OnceLock, RwLock};

use serde::Serialize;

/// OCR 识别结果
#[derive(Debug, Clone, Serialize)]
pub struct OcrResult {
    /// 智能拼接的完整文本（0.11.9-b 起用 `join_words_smart`,不再用 SDK `Text()`）
    pub text: String,
    /// 行级结构（含 word_indices 指回 words 数组）
    pub lines: Vec<OcrLine>,
    /// 词级 flat 数组（0.11.9-b 新增,前端 word 拖选 / word 高亮用）
    pub words: Vec<OcrWord>,
    /// SDK 检测到的文本旋转角度（度）；`None` 表示 SDK 未给或非旋转文本
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text_angle: Option<f64>,
    /// 每个 word 在 `text` 中的 Rust **字符**索引范围
    /// `{start, end}`（0.22.7 新增）。
    ///
    /// 这是 word → 全文 text 字符偏移的**单一真源**。前端从这里生成
    /// UTF-16 offset 供 textarea selection API 使用，不再自行复算空格/换行。
    /// `char_ranges[i]` 对应 `words[i]`，长度与 `words` 等长。
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub char_ranges: Vec<(usize, usize)>,
    /// 字符级选取框（0.22.8 新增）。
    ///
    /// `char_boxes` 与 `words` 语义分离：
    /// - `words` 是语义级选择单元（词/region），用于行级 grouping 和 char_ranges。
    /// - `char_boxes` 是字符级定位框，用于图片上的 hit-test、拖选和高亮。
    ///
    /// `char_start/char_end` 是相对于 `OcrResult.text` 的 Rust char index，
    /// 前端转换为 UTF-16 offset 后供 textarea selection API 使用。
    ///
    /// 兼容性：空数组（`Vec::new()`）表示无字符级选取框，前端回退到 `words`。
    /// WinRT / FakeOcrBackend 等不产生 char_boxes，序列化时省略（`skip_serializing_if`）。
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub char_boxes: Vec<OcrCharBox>,
    /// 本次实际使用的引擎（0.22.10 新增）。
    ///
    /// 由 capability 层从 RouteDecision 注入；直连 `backend()` 的路径为 `None`。
    /// 序列化缺省以保持旧消费者兼容。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backend_used: Option<crate::domain::ocr::config::OcrBackendKind>,
    /// auto 模式下回退 WinRT 的原因（0.22.10 新增）。
    ///
    /// 复用 `RouteDecision.fallback_reason`；未发生回退为 `None`。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backend_fallback_reason: Option<String>,
}

/// OCR 单行结果
#[derive(Debug, Clone, Serialize)]
pub struct OcrLine {
    pub text: String,
    /// 行包围盒（0.11.9-b 起用该行所有 words 的 union;旧版本为固定 `{0,0,0,0}`）
    #[serde(rename = "rect")]
    pub bounding_rect: OcrRect,
    /// 该行对应的 `OcrResult.words` 索引段（0.11.9-b 新增）
    pub word_indices: Vec<usize>,
}

/// OCR 单词结果（0.11.9-b 新增）
#[derive(Debug, Clone, Serialize)]
pub struct OcrWord {
    pub text: String,
    #[serde(rename = "rect")]
    pub bounding_rect: OcrRect,
    pub line_index: usize,
}

/// 字符级选取框（0.22.8 新增）。
///
/// 用于图片上的 hit-test、拖选和高亮。`char_start/char_end` 是相对于
/// `OcrResult.text` 的 Rust char index 范围。
///
/// 与 `OcrWord` 的区别：
/// - `OcrWord` 是语义级 token（词/region），参与行级 grouping 和 `char_ranges`。
/// - `OcrCharBox` 是字符级定位框，不参与文本拼接，仅用于前端图片选取。
///
/// 来源：oar-ocr 的 `word_boxes` 实际是逐字符框，在 ONNX pipeline 中
/// 被映射为 `OcrCharBox` 而非伪装成 `OcrWord`。
#[derive(Debug, Clone, Serialize)]
pub struct OcrCharBox {
    pub text: String,
    #[serde(rename = "rect")]
    pub bounding_rect: OcrRect,
    pub line_index: usize,
    /// 该字符在 `OcrResult.text` 中的 Rust char 起始索引（含）。
    pub char_start: usize,
    /// 该字符在 `OcrResult.text` 中的 Rust char 结束索引（不含）。
    /// `char_end - char_start` 始终等于 `text.chars().count()`。
    pub char_end: usize,
}

/// 矩形坐标（物理像素）
#[derive(Debug, Clone, Copy, Serialize)]
pub struct OcrRect {
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
}

impl OcrRect {
    /// 求两 rect 的包围盒。任一 zero-sized rect 视作缺席不参与。
    #[allow(dead_code)] // 现只在 rect_union 里用
    fn is_zero(self) -> bool {
        self.w == 0 && self.h == 0
    }

    /// rect 右边界（x + w）。
    pub fn right(self) -> i32 {
        self.x.saturating_add(self.w as i32)
    }

    /// rect 下边界（y + h）。
    pub fn bottom(self) -> i32 {
        self.y.saturating_add(self.h as i32)
    }

    /// rect 垂直中心 Y 坐标。
    pub fn center_y(self) -> f64 {
        self.y as f64 + self.h as f64 / 2.0
    }

    /// rect 水平中心 X 坐标。
    pub fn center_x(self) -> f64 {
        self.x as f64 + self.w as f64 / 2.0
    }

    /// 元素高度（转为 f64 方便比例计算）。
    pub fn height_f(self) -> f64 {
        self.h as f64
    }
}

/// 计算若干 rect 的 union。返回 `None` 表示输入为空或全 zero。
fn rect_union(rects: impl Iterator<Item = OcrRect>) -> Option<OcrRect> {
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

// ── 同行聚合（视觉行合并 + 阅读顺序恢复） ─────────────────────────────────
//
// 问题背景：
// - WinRT OcrEngine 已按视觉行分组（OcrLine → Words），line_index 天然反映同行关系。
// - PaddleOCR 的 `extract_results` 把每个 rec_texts 检测元素视为独立"行"，
//   但同一视觉行可能被检测为多个独立框（尤其拉丁文连续文本），导致输出产生多余换行。
//
// 本模块提供一个**纯函数** `group_words_into_lines`，接收 flat 的 word 列表
// （每个 word 带 rect，不含 line_index 或 line_index 是检测元素的原始序号），
// 输出按视觉行分组后的新 line_index + 诊断统计。
//
// 算法：基于 Y 轴重叠、中心线距离、基线近似的真正行聚类 + 按 Y/X 恢复阅读顺序。
//
// 设计目标（0.22.7 升级）：
// 1. 不用固定像素阈值，用文字高度的比例做自适应阈值。
// 2. 不用水平距离直接否决"是否同行"——大水平间距只标记不拆行。
// 3. 元素与行的聚合几何/代表基线比较，而非只与上一个元素比较。
// 4. 行形成后按视觉 Y 顺序排序，行内按 X 顺序排序。
// 5. 阈值随文字高度缩放，兼容不同字号但基线相近的文本。
// 6. 多栏/表格保守策略：不能可靠识别多栏时优先保持视觉同行，用较大空格表达间隔。
// 7. 独立可测，不依赖 WinRT 或 PaddleOCR。WinRT 与 PaddleOCR 共用同一语义。

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
#[allow(dead_code)] // 0.22.7 测试便捷入口（_with_diag 的简化版），生产用 rebuild 路径
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

// ── 智能拼接（0.11.9-b） ──────────────────────────────────────────────────

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

/// 智能拼接 word 列表为完整文本（0.11.9-b，0.22.7 升级）。
///
/// **0.22.7 变更**：行内空格和行间换行/空行已由 `rebuild_with_line_grouping_and_diag`
/// 内部的 `join_words_intra_line_with_gaps` 和 `join_lines_into_text` 完成。
/// 本函数保留为兼容入口——直接从 `lines[i].text` 用 `\n` join，
/// 不再独立计算空格/换行。行内多空格和行间空行已在 line.text 拼接阶段处理。
///
/// 规则（由下游函数实现）：
/// - CJK ↔ CJK：紧邻不加空格，大间距加空格
/// - CJK ↔ Latin：紧邻不加空格
/// - Latin ↔ Latin：至少一个空格
/// - 大水平间距：映射为多个空格（上限 8）
/// - 大纵向间距：插入额外空行（上限 3）
/// - 行末换行 `\n`
///
/// 依赖 `words[i].line_index` 严格递增（0-based）。如果 words 为空但 lines 有内容
/// (SDK 只给 line 没给 word——不太可能但兜底)，退化为 line.text 用 `\n` join。
///
/// 独立可测（下方 `tests` 模块覆盖）。
#[allow(dead_code)] // 0.22.7 预留公共 API（domain::ocr 重导出），生产用 rebuild 路径
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

/// OCR 引擎错误
#[derive(Debug)]
#[allow(dead_code)]
pub enum OcrError {
    Engine(String),
    Decode(String),
    Unsupported,
}

impl std::fmt::Display for OcrError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OcrError::Engine(msg) => write!(f, "OCR 引擎错误: {msg}"),
            OcrError::Decode(msg) => write!(f, "图片解码错误: {msg}"),
            OcrError::Unsupported => write!(f, "当前平台不支持 OCR"),
        }
    }
}

/// OCR 后端 trait（domain 侧抽象，返回领域类型）。
#[async_trait::async_trait]
pub trait OcrBackend: Send + Sync {
    /// 识别 PNG 图片中的文字
    async fn recognize(&self, png_data: &[u8]) -> Result<OcrResult, OcrError>;

    /// 返回设备已安装的 OCR 语言 BCP-47 tag 列表（0.17.5 诊断用）。
    async fn available_languages(&self) -> Vec<String> {
        Vec::new()
    }

    /// 返回当前引擎使用的语言 tag（None = fallback；0.17.5 诊断用）。
    async fn engine_language(&self) -> Option<String> {
        None
    }
}

// ── 全局注入 ───────────────────────────────────────────────────────────────

static BACKEND: OnceLock<RwLock<Arc<dyn OcrBackend>>> = OnceLock::new();

/// 安装/替换 OCR backend（0.11.7-f）。
///
/// **调用时机**：`main.rs::setup` 里最早期。可重复调用替换 backend（测试用）。
#[allow(dead_code)] // 测试通过 install_backend 注入 Fake
pub fn install_backend(backend: Arc<dyn OcrBackend>) {
    match BACKEND.get() {
        Some(lock) => {
            if let Ok(mut w) = lock.write() {
                *w = backend;
            }
        }
        None => {
            let _ = BACKEND.set(RwLock::new(backend));
        }
    }
}

/// 获取当前 OCR backend。
///
/// **首次调用兜底**：自动包装 infra `PlatformOcrBackend` 为 domain `OcrBackend`。
pub fn backend() -> Arc<dyn OcrBackend> {
    let lock = BACKEND.get_or_init(|| {
        let default: Arc<dyn OcrBackend> = Arc::new(WindowsOcrBackendAdapter::new());
        RwLock::new(default)
    });
    lock.read().expect("OCR backend RwLock 中毒").clone()
}

// ── WindowsOcrBackendAdapter（0.14.7 W2）──────────────────────────────────

/// 包装 infra `PlatformOcrBackend`，将原始 DTO 映射为领域类型。
///
/// 智能拼接（`join_words_smart`）和 rect union 在此完成，不下沉到 infra。
pub struct WindowsOcrBackendAdapter {
    inner: Box<dyn crate::infra::platform::ocr::PlatformOcrBackend>,
}

impl WindowsOcrBackendAdapter {
    pub fn new() -> Self {
        Self {
            inner: crate::infra::platform::ocr::default_backend(),
        }
    }
}

impl Default for WindowsOcrBackendAdapter {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl OcrBackend for WindowsOcrBackendAdapter {
    async fn recognize(&self, png_data: &[u8]) -> Result<OcrResult, OcrError> {
        use crate::infra::platform::ocr::PlatformOcrError;

        let raw = self
            .inner
            .recognize_raw(png_data)
            .await
            .map_err(|e| match e {
                PlatformOcrError::Engine(msg) => OcrError::Engine(msg),
                PlatformOcrError::Decode(msg) => OcrError::Decode(msg),
                PlatformOcrError::Unsupported => OcrError::Unsupported,
            })?;

        Ok(map_raw_to_domain(raw))
    }

    async fn available_languages(&self) -> Vec<String> {
        self.inner.available_languages().await
    }

    async fn engine_language(&self) -> Option<String> {
        self.inner.engine_language().await
    }
}

/// 将 infra 原始 DTO 映射为领域类型。
///
/// 负责：
/// - 浮点 rect → 整数 rect（四舍五入）
/// - word flat 数组构建（丢弃 SDK 的 line 分组，统一走 `rebuild_with_line_grouping` 重新分组）
/// - 同行聚合 + 阅读顺序恢复（`rebuild_with_line_grouping`）
/// - `join_words_smart` 智能拼接全文
///
/// WinRT SDK 虽然已按视觉行分组，但走同一套同行聚合纯函数可保证
/// WinRT 与 PaddleOCR 语义一致，且能修正 SDK 偶尔的行拆分问题。
fn map_raw_to_domain(raw: crate::infra::platform::ocr::RawOcrResult) -> OcrResult {
    let mut words: Vec<OcrWord> = Vec::new();

    for raw_line in &raw.lines {
        for raw_word in &raw_line.words {
            let rect = OcrRect {
                x: raw_word.rect.x.round() as i32,
                y: raw_word.rect.y.round() as i32,
                w: raw_word.rect.width.round().max(0.0) as u32,
                h: raw_word.rect.height.round().max(0.0) as u32,
            };
            words.push(OcrWord {
                text: raw_word.text.clone(),
                bounding_rect: rect,
                line_index: 0, // 会被 rebuild_with_line_grouping 覆盖
            });
        }
    }

    rebuild_with_line_grouping(words, raw.text_angle)
}

// ── FakeOcrBackend（测试用） ───────────────────────────────────────────────

/// 测试用假 OCR 后端。构造时配置固定返回值。
///
/// 0.11.9-b：支持配置 `words` 让 Capability 层测试 word 级链路。
/// 0.17.5：支持配置 `available_langs` / `engine_lang` 测试诊断面板。
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct FakeOcrBackend {
    text: String,
    lines: Vec<OcrLine>,
    words: Vec<OcrWord>,
    err: Option<String>,
    available_langs: Vec<String>,
    engine_lang: Option<String>,
}

#[allow(dead_code)]
impl FakeOcrBackend {
    pub fn returning(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            lines: Vec::new(),
            words: Vec::new(),
            err: None,
            available_langs: Vec::new(),
            engine_lang: None,
        }
    }

    pub fn with_lines(mut self, lines: Vec<OcrLine>) -> Self {
        self.lines = lines;
        self
    }

    pub fn with_words(mut self, words: Vec<OcrWord>) -> Self {
        self.words = words;
        self
    }

    pub fn failing(msg: impl Into<String>) -> Self {
        Self {
            text: String::new(),
            lines: Vec::new(),
            words: Vec::new(),
            err: Some(msg.into()),
            available_langs: Vec::new(),
            engine_lang: None,
        }
    }

    /// 配置诊断返回值（0.17.5）。
    pub fn with_available_langs(mut self, langs: Vec<String>) -> Self {
        self.available_langs = langs;
        self
    }

    pub fn with_engine_lang(mut self, lang: Option<String>) -> Self {
        self.engine_lang = lang;
        self
    }
}

#[async_trait::async_trait]
impl OcrBackend for FakeOcrBackend {
    async fn recognize(&self, _png_data: &[u8]) -> Result<OcrResult, OcrError> {
        if let Some(msg) = &self.err {
            return Err(OcrError::Engine(msg.clone()));
        }
        Ok(OcrResult {
            backend_used: None,
            backend_fallback_reason: None,
            text: self.text.clone(),
            lines: self.lines.clone(),
            words: self.words.clone(),
            text_angle: None,
            char_ranges: Vec::new(),
            char_boxes: Vec::new(),
        })
    }

    async fn available_languages(&self) -> Vec<String> {
        self.available_langs.clone()
    }

    async fn engine_language(&self) -> Option<String> {
        self.engine_lang.clone()
    }
}

// ── 测试 ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn fake_backend_returns_configured_text() {
        let backend = FakeOcrBackend::returning("Hello 世界");
        let result = backend.recognize(&[]).await.unwrap();
        assert_eq!(result.text, "Hello 世界");
        assert!(result.lines.is_empty());
    }

    #[tokio::test]
    async fn fake_backend_returns_configured_error() {
        let backend = FakeOcrBackend::failing("模拟错误");
        let err = backend.recognize(&[]).await.unwrap_err();
        assert!(matches!(err, OcrError::Engine(msg) if msg == "模拟错误"));
    }

    #[tokio::test]
    async fn install_backend_replaces_global() {
        install_backend(Arc::new(FakeOcrBackend::returning("test-injection")));
        let b = backend();
        let result = b.recognize(&[]).await.unwrap();
        assert_eq!(result.text, "test-injection");
    }

    // ── join_words_smart（0.11.9-b → 0.22.7 升级） ──────────────
    //
    // 0.22.7 后 join_words_smart 只用 \n join lines[].text，
    // 不再从 words 自行拼接。测试改为走 rebuild_with_line_grouping
    // 来验证端到端的行内拼接 + 行间换行。

    #[cfg(test)]
    #[allow(dead_code)] // 0.22.7 遗留测试辅助；测试改走 rebuild_with_line_grouping 后保留供后续回归
    fn w(text: &str, line: usize) -> OcrWord {
        OcrWord {
            text: text.into(),
            bounding_rect: OcrRect {
                x: 0,
                y: 0,
                w: 0,
                h: 0,
            },
            line_index: line,
        }
    }

    #[test]
    fn join_pure_cjk_no_spaces() {
        // "你好" "世界" 在同一行 → "你好世界"
        // 走 rebuild_with_line_grouping 验证行内拼接规则
        let words = vec![
            wr("你", 10, 20, 30, 30, 0),
            wr("好", 45, 20, 30, 30, 0),
            wr("世", 80, 20, 30, 30, 0),
            wr("界", 115, 20, 30, 30, 0),
        ];
        let result = rebuild_with_line_grouping(words, None);
        assert_eq!(result.text, "你好世界");
    }

    #[test]
    fn join_pure_latin_has_single_space() {
        let words = vec![
            wr("hello", 10, 20, 60, 30, 0),
            wr("world", 75, 20, 60, 30, 0),
        ];
        let result = rebuild_with_line_grouping(words, None);
        assert_eq!(result.text, "hello world");
    }

    #[test]
    fn join_cjk_latin_no_space() {
        // 中英混排应贴一起("温度 25 度" → "温度25度")
        let words = vec![
            wr("温度", 10, 20, 60, 30, 0),
            wr("25", 75, 20, 30, 30, 0),
            wr("度", 110, 20, 30, 30, 0),
        ];
        let result = rebuild_with_line_grouping(words, None);
        assert_eq!(result.text, "温度25度");
    }

    #[test]
    fn join_multiline_uses_newline() {
        let words = vec![
            wr("first", 10, 20, 60, 30, 0),
            wr("line", 75, 20, 60, 30, 0),
            wr("第二", 10, 60, 60, 30, 1),
            wr("行", 75, 60, 30, 30, 1),
        ];
        let result = rebuild_with_line_grouping(words, None);
        assert_eq!(result.text, "first line\n第二行");
    }

    #[test]
    fn join_ascii_punct_between_latin_gets_space() {
        // 标点走 Other,与 Latin 相邻加空格
        let words = vec![
            wr("Hello", 10, 20, 60, 30, 0),
            wr(",", 75, 20, 10, 30, 0),
            wr("world", 90, 20, 60, 30, 0),
        ];
        let result = rebuild_with_line_grouping(words, None);
        assert_eq!(result.text, "Hello , world");
    }

    #[test]
    fn join_cjk_and_punct_no_space() {
        // CJK 侧一定不加空格
        let words = vec![
            wr("你好", 10, 20, 60, 30, 0),
            wr(",", 75, 20, 10, 30, 0),
            wr("世界", 90, 20, 60, 30, 0),
        ];
        let result = rebuild_with_line_grouping(words, None);
        assert_eq!(result.text, "你好,世界");
    }

    #[test]
    fn join_empty_words_falls_back_to_lines_text() {
        // join_words_smart 兑底：words 为空 → 用 lines.text
        let lines = vec![
            OcrLine {
                text: "fallback line 1".into(),
                bounding_rect: OcrRect {
                    x: 0,
                    y: 0,
                    w: 0,
                    h: 0,
                },
                word_indices: vec![],
            },
            OcrLine {
                text: "fallback line 2".into(),
                bounding_rect: OcrRect {
                    x: 0,
                    y: 0,
                    w: 0,
                    h: 0,
                },
                word_indices: vec![],
            },
        ];
        assert_eq!(
            join_words_smart(&[], &lines),
            "fallback line 1\nfallback line 2"
        );
    }

    #[test]
    fn join_skips_empty_word_text() {
        // 空 word text 不影响相邻 word 拼接规则
        let words = vec![
            wr("hello", 10, 20, 60, 30, 0),
            wr("", 75, 20, 0, 30, 0),
            wr("world", 80, 20, 60, 30, 0),
        ];
        let result = rebuild_with_line_grouping(words, None);
        assert_eq!(result.text, "hello world");
    }

    #[test]
    fn join_line_boundary_resets_leading_space() {
        // 行首不该有前置空格
        let words = vec![
            wr("末尾", 10, 20, 60, 30, 0),
            wr("hello", 10, 60, 60, 30, 1),
        ];
        let result = rebuild_with_line_grouping(words, None);
        assert_eq!(result.text, "末尾\nhello");
    }

    // ── rect_union 边界 ─────────────────────────────────

    #[test]
    fn rect_union_returns_none_when_all_zero() {
        let empties = [OcrRect {
            x: 0,
            y: 0,
            w: 0,
            h: 0,
        }; 3];
        assert!(rect_union(empties.into_iter()).is_none());
    }

    #[test]
    fn rect_union_computes_bounding_box() {
        let rects = vec![
            OcrRect {
                x: 10,
                y: 20,
                w: 30,
                h: 40,
            }, // 右下 (40, 60)
            OcrRect {
                x: 50,
                y: 5,
                w: 20,
                h: 10,
            }, // 右下 (70, 15)
        ];
        let u = rect_union(rects.into_iter()).unwrap();
        assert_eq!(u.x, 10);
        assert_eq!(u.y, 5);
        assert_eq!(u.w, 60); // 70 - 10
        assert_eq!(u.h, 55); // 60 - 5
    }

    #[test]
    fn rect_union_skips_zero_rects() {
        let rects = vec![
            OcrRect {
                x: 0,
                y: 0,
                w: 0,
                h: 0,
            },
            OcrRect {
                x: 10,
                y: 10,
                w: 20,
                h: 20,
            },
        ];
        let u = rect_union(rects.into_iter()).unwrap();
        assert_eq!(u.x, 10);
        assert_eq!(u.w, 20);
    }

    // ── map_raw_to_domain（0.14.7 W2）──────────────────────────────

    #[test]
    fn map_raw_to_domain_converts_rects_and_joins_words() {
        use crate::infra::platform::ocr::{RawOcrLine, RawOcrRect, RawOcrResult, RawOcrWord};

        let raw = RawOcrResult {
            lines: vec![RawOcrLine {
                text: "你好 world".into(),
                words: vec![
                    RawOcrWord {
                        text: "你".into(),
                        rect: RawOcrRect {
                            x: 10.4,
                            y: 20.6,
                            width: 30.0,
                            height: 40.0,
                        },
                    },
                    RawOcrWord {
                        text: "好".into(),
                        rect: RawOcrRect {
                            x: 50.0,
                            y: 20.0,
                            width: 30.0,
                            height: 40.0,
                        },
                    },
                    RawOcrWord {
                        text: "world".into(),
                        rect: RawOcrRect {
                            x: 90.0,
                            y: 20.0,
                            width: 50.0,
                            height: 40.0,
                        },
                    },
                ],
            }],
            text_angle: Some(90.0),
        };

        let result = map_raw_to_domain(raw);

        // 智能拼接：CJK↔CJK 无空格，CJK↔Latin 无空格
        assert_eq!(result.text, "你好world");
        assert_eq!(result.words.len(), 3);
        assert_eq!(result.lines.len(), 1);

        // rect 四舍五入
        assert_eq!(result.words[0].bounding_rect.x, 10);
        assert_eq!(result.words[0].bounding_rect.y, 21);

        // line rect = words union
        let line_rect = result.lines[0].bounding_rect;
        assert_eq!(line_rect.x, 10);
        assert_eq!(line_rect.w, 130); // 90+50 - 10

        // text_angle 透传
        assert_eq!(result.text_angle, Some(90.0));

        // word_indices 指回 flat 数组
        assert_eq!(result.lines[0].word_indices, vec![0, 1, 2]);
    }

    // ── 同行聚合（group_words_into_lines / rebuild_with_line_grouping） ──

    /// 构造 word 的辅助函数，带真实 rect。
    fn wr(text: &str, x: i32, y: i32, w: u32, h: u32, line: usize) -> OcrWord {
        OcrWord {
            text: text.into(),
            bounding_rect: OcrRect { x, y, w, h },
            line_index: line,
        }
    }

    #[test]
    fn group_same_line_adjacent_boxes_merge() {
        // 同一行两个相邻框（水平间距 = 5px，高度 = 30px）→ 合并
        let words = vec![
            wr("hello", 10, 20, 60, 30, 0),
            wr("world", 75, 20, 60, 30, 1), // 原始 line_index 不同
        ];
        let groups = group_words_into_lines(&words);
        assert_eq!(groups, vec![0, 0]); // 合并到同一行
    }

    #[test]
    fn group_different_lines_do_not_merge() {
        // 上下两行（Y 中心距 = 45px，高度 = 30px，avg_h=30, 45 > 30*0.6=18）→ 不合并
        let words = vec![
            wr("first", 10, 10, 60, 30, 0),
            wr("second", 10, 55, 60, 30, 1),
        ];
        let groups = group_words_into_lines(&words);
        assert_eq!(groups, vec![0, 1]);
    }

    #[test]
    fn group_different_font_size_but_similar_baseline() {
        // 字号差异大但基线相近：高度 30 vs 高度 80（ratio=2.67 < 3.0）→ 合并
        // 0.22.7：HEIGHT_RATIO_MAX 放宽到 3.0，允许不同字号但基线相近的文本合并
        let words = vec![
            wr("small", 10, 20, 60, 30, 0),
            wr("BIG", 80, 10, 120, 80, 1),
        ];
        let groups = group_words_into_lines(&words);
        assert_eq!(groups, vec![0, 0]); // 合并到同一行
    }

    #[test]
    fn group_horizontal_distance_no_longer_splits() {
        // 0.22.7：水平距离不再否决同行。同 Y 高度但水平间距很大 → 仍合并
        // 大间距会在 join_words_intra_line_with_gaps 中映射为多空格
        let words = vec![
            wr("left", 10, 20, 60, 30, 0),
            wr("right", 280, 20, 60, 30, 1),
        ];
        let groups = group_words_into_lines(&words);
        assert_eq!(groups, vec![0, 0]); // 合并到同一行
    }

    #[test]
    fn group_shuffled_input_recovers_reading_order() {
        // 输入顺序混乱，应按 Y/X 恢复阅读顺序
        // 行0: "B" "A"（Y=20, X 分别 60 和 10）
        // 行1: "D" "C"（Y=60, X 分别 60 和 10）
        let words = vec![
            wr("D", 60, 60, 50, 30, 0), // Y=60 X=60
            wr("A", 10, 20, 50, 30, 1), // Y=20 X=10
            wr("C", 10, 60, 50, 30, 2), // Y=60 X=10
            wr("B", 60, 20, 50, 30, 3), // Y=20 X=60
        ];
        let groups = group_words_into_lines(&words);
        // A,B 同行0；C,D 同行1
        // groups[i] 对应 words[i]
        assert_eq!(groups[0], 1); // D → 行1
        assert_eq!(groups[1], 0); // A → 行0
        assert_eq!(groups[2], 1); // C → 行1
        assert_eq!(groups[3], 0); // B → 行0
    }

    #[test]
    fn rebuild_bidirectional_consistency() {
        // 合并后 word_indices 与 line_index 双向一致
        let words = vec![
            wr("hello", 10, 20, 60, 30, 0),
            wr("world", 75, 20, 60, 30, 0),
            wr("foo", 10, 60, 60, 30, 1),
            wr("bar", 75, 60, 60, 30, 1),
        ];
        let result = rebuild_with_line_grouping(words, None);

        // 校验双向一致
        for (line_idx, line) in result.lines.iter().enumerate() {
            for &word_idx in &line.word_indices {
                assert_eq!(
                    result.words[word_idx].line_index, line_idx,
                    "双向一致失败：word[{word_idx}].line_index={} 但被 line[{line_idx}] 引用",
                    result.words[word_idx].line_index
                );
            }
        }
        // 每个 word 被恰好引用一次
        let mut ref_count = vec![0u32; result.words.len()];
        for line in &result.lines {
            for &idx in &line.word_indices {
                ref_count[idx] += 1;
            }
        }
        for (idx, &count) in ref_count.iter().enumerate() {
            assert_eq!(count, 1, "word[{idx}] 被引用 {count} 次（应恰好 1 次）");
        }
    }

    #[test]
    fn rebuild_cjk_text() {
        // CJK 同行：不加空格
        let words = vec![
            wr("你", 10, 20, 30, 30, 0),
            wr("好", 45, 20, 30, 30, 0),
            wr("世", 80, 20, 30, 30, 0),
            wr("界", 115, 20, 30, 30, 0),
        ];
        let result = rebuild_with_line_grouping(words, None);
        assert_eq!(result.text, "你好世界");
        assert_eq!(result.lines.len(), 1);
    }

    #[test]
    fn rebuild_latin_text() {
        // Latin 同行：加空格
        let words = vec![
            wr("hello", 10, 20, 60, 30, 0),
            wr("world", 75, 20, 60, 30, 0),
        ];
        let result = rebuild_with_line_grouping(words, None);
        assert_eq!(result.text, "hello world");
        assert_eq!(result.lines.len(), 1);
    }

    #[test]
    fn rebuild_mixed_cjk_latin_text() {
        // 中英混排：CJK↔Latin 不加空格
        let words = vec![
            wr("温度", 10, 20, 60, 30, 0),
            wr("25", 75, 20, 30, 30, 0),
            wr("度", 110, 20, 30, 30, 0),
        ];
        let result = rebuild_with_line_grouping(words, None);
        assert_eq!(result.text, "温度25度");
        assert_eq!(result.lines.len(), 1);
    }

    #[test]
    fn rebuild_multiple_lines() {
        // 上下两行各自同行
        let words = vec![
            wr("hello", 10, 20, 60, 30, 0),
            wr("world", 75, 20, 60, 30, 0),
            wr("你好", 10, 60, 60, 30, 1),
            wr("世界", 75, 60, 60, 30, 1),
        ];
        let result = rebuild_with_line_grouping(words, None);
        assert_eq!(result.text, "hello world\n你好世界");
        assert_eq!(result.lines.len(), 2);
        assert_eq!(result.lines[0].word_indices, vec![0, 1]);
        assert_eq!(result.lines[1].word_indices, vec![2, 3]);
    }

    #[test]
    fn rebuild_line_rect_is_union() {
        // line rect = 该行所有 words 的 union
        let words = vec![
            wr("A", 10, 20, 30, 30, 0),
            wr("B", 50, 20, 30, 30, 0),
            wr("C", 90, 20, 30, 30, 0),
        ];
        let result = rebuild_with_line_grouping(words, None);
        let line_rect = result.lines[0].bounding_rect;
        assert_eq!(line_rect.x, 10);
        assert_eq!(line_rect.y, 20);
        assert_eq!(line_rect.w, 110); // 90+30 - 10
        assert_eq!(line_rect.h, 30);
    }

    #[test]
    fn rebuild_empty_words() {
        let result = rebuild_with_line_grouping(vec![], None);
        assert_eq!(result.text, "");
        assert!(result.lines.is_empty());
        assert!(result.words.is_empty());
    }

    #[test]
    fn rebuild_single_word() {
        let words = vec![wr("alone", 10, 20, 60, 30, 0)];
        let result = rebuild_with_line_grouping(words, None);
        assert_eq!(result.text, "alone");
        assert_eq!(result.lines.len(), 1);
        assert_eq!(result.words.len(), 1);
    }

    #[test]
    fn rebuild_text_angle_preserved() {
        let words = vec![wr("test", 10, 20, 60, 30, 0)];
        let result = rebuild_with_line_grouping(words, Some(90.0));
        assert_eq!(result.text_angle, Some(90.0));
    }

    #[test]
    fn rebuild_words_sorted_by_x_within_line() {
        // 行内 X 顺序：输入 B 在 A 前面，输出应按 X 排序 A, B
        let words = vec![
            wr("B", 60, 20, 30, 30, 0), // X=60
            wr("A", 10, 20, 30, 30, 1), // X=10
        ];
        let result = rebuild_with_line_grouping(words, None);
        // 同行，X 排序后 A 在前
        assert_eq!(result.words[0].text, "A");
        assert_eq!(result.words[1].text, "B");
        assert_eq!(result.text, "A B");
    }

    // ── char_ranges（0.22.7 新增） ──────────────────────────────────────

    #[test]
    fn char_ranges_basic_single_line() {
        // 单行 Latin：hello + " " + world
        let words = vec![
            wr("hello", 10, 20, 60, 30, 0),
            wr("world", 75, 20, 60, 30, 0),
        ];
        let result = rebuild_with_line_grouping(words, None);
        assert_eq!(result.text, "hello world");
        assert_eq!(result.char_ranges.len(), 2);
        // hello: chars 0..5
        assert_eq!(result.char_ranges[0], (0, 5));
        // world: chars 6..11 (after space)
        assert_eq!(result.char_ranges[1], (6, 11));
    }

    #[test]
    fn char_ranges_cjk_single_line() {
        // CJK 不加空格
        let words = vec![wr("你好", 10, 20, 60, 30, 0), wr("世界", 75, 20, 60, 30, 0)];
        let result = rebuild_with_line_grouping(words, None);
        assert_eq!(result.text, "你好世界");
        assert_eq!(result.char_ranges.len(), 2);
        assert_eq!(result.char_ranges[0], (0, 2));
        assert_eq!(result.char_ranges[1], (2, 4));
    }

    #[test]
    fn char_ranges_multi_line() {
        // 两行：行间换行偏移 char_ranges
        let words = vec![
            wr("hello", 10, 20, 60, 30, 0),
            wr("world", 75, 20, 60, 30, 0),
            wr("你好", 10, 60, 60, 30, 1),
            wr("世界", 75, 60, 60, 30, 1),
        ];
        let result = rebuild_with_line_grouping(words, None);
        assert_eq!(result.text, "hello world\n你好世界");
        assert_eq!(result.char_ranges.len(), 4);
        // hello: 0..5, world: 6..11
        assert_eq!(result.char_ranges[0], (0, 5));
        assert_eq!(result.char_ranges[1], (6, 11));
        // 你好: 12..14 (after \n at index 11)
        assert_eq!(result.char_ranges[2], (12, 14));
        // 世界: 14..16
        assert_eq!(result.char_ranges[3], (14, 16));
    }

    #[test]
    fn char_ranges_match_text_slice() {
        // 每个 word 的 char_range 切片应该恰好等于该 word 的 text
        let words = vec![
            wr("温度", 10, 20, 60, 30, 0),
            wr("25", 75, 20, 30, 30, 0),
            wr("度", 110, 20, 30, 30, 0),
        ];
        let result = rebuild_with_line_grouping(words, None);
        let text_chars: Vec<char> = result.text.chars().collect();
        for (i, w) in result.words.iter().enumerate() {
            let (start, end) = result.char_ranges[i];
            let slice: String = text_chars[start..end].iter().collect();
            assert_eq!(slice, w.text, "word[{i}] char_range mismatch");
        }
    }

    // ── 多空格映射（0.22.7） ─────────────────────────────────────────

    #[test]
    fn large_horizontal_gap_produces_multiple_spaces() {
        // 同行但水平间距很大 → 映射为多空格（上限 8）
        let words = vec![
            wr("left", 10, 20, 60, 30, 0),
            wr("right", 400, 20, 60, 30, 0), // gap = 330px, char_width ≈ 15, gap/char_width ≈ 22
        ];
        let result = rebuild_with_line_grouping(words, None);
        // 应有多个空格（至少 2，上限 8）
        let space_count = result.text.chars().filter(|&c| c == ' ').count();
        assert!(
            (2..=8).contains(&space_count),
            "expected 2-8 spaces, got {space_count}: text='{:?}'",
            result.text
        );
    }

    // ── 空行映射（0.22.7） ─────────────────────────────────────────────

    #[test]
    fn large_vertical_gap_inserts_blank_lines() {
        // 行间纵向 gap 明显大于行高 → 插入额外空行（上限 3）
        let words = vec![
            wr("line1", 10, 10, 60, 30, 0),
            wr("line2", 10, 200, 60, 30, 1), // v_gap = 200 - 40 = 160, avg_h = 30, 160 > 30*1.5=45
        ];
        let result = rebuild_with_line_grouping(words, None);
        // 应有额外空行
        let newline_count = result.text.chars().filter(|&c| c == '\n').count();
        assert!(
            newline_count >= 2,
            "expected at least 2 newlines (1 + blank), got {newline_count}: text='{:?}'",
            result.text
        );
    }

    // ── LayoutDiagnostics（0.22.7） ─────────────────────────────────────

    #[test]
    fn diagnostics_populated_correctly() {
        let words = vec![
            wr("hello", 10, 20, 60, 30, 0),
            wr("world", 75, 20, 60, 30, 0),
            wr("foo", 10, 60, 60, 30, 1),
        ];
        let (result, diag) = rebuild_with_line_grouping_and_diag(words, None);
        assert_eq!(diag.source_words, 3);
        assert_eq!(diag.merged_line_count, 2);
        assert_eq!(diag.output_text_chars, result.text.chars().count());
        assert!(diag.created_new_line >= 2); // 至少 2 行
        assert!(diag.assigned_existing_line >= 1); // 至少 1 个被分配到已有行
    }
}
