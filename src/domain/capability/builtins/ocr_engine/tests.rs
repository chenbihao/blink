//! OCR engine 单元测试。
//!
//! 0.22 收尾：从 `ocr_engine.rs` 按职责拆出。

use super::layout::*;
use super::types::*;
use super::*;

#[tokio::test]
async fn fake_backend_returns_configured_text() {
    let backend = fake::FakeOcrBackend::returning("Hello 世界");
    let result = backend.recognize(&[]).await.unwrap();
    assert_eq!(result.text, "Hello 世界");
    assert!(result.lines.is_empty());
}

#[tokio::test]
async fn fake_backend_returns_configured_error() {
    let backend = fake::FakeOcrBackend::failing("模拟错误");
    let err = backend.recognize(&[]).await.unwrap_err();
    assert!(matches!(err, OcrError::Engine(msg) if msg == "模拟错误"));
}

#[tokio::test]
async fn install_backend_replaces_global() {
    backend::install_backend(std::sync::Arc::new(fake::FakeOcrBackend::returning(
        "test-injection",
    )));
    let b = backend::backend();
    let result = b.recognize(&[]).await.unwrap();
    assert_eq!(result.text, "test-injection");
}

// ── join_words_smart（0.11.9-b → 0.22.7 升级） ──────────────
//
// 0.22.7 后 join_words_smart 只用 \n join lines[].text，
// 不再从 words 自行拼接。测试改为走 rebuild_with_line_grouping
// 来验证端到端的行内拼接 + 行间换行。

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
            font_height: None,
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
            font_height: None,
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
    assert!(super::layout::rect_union(empties.into_iter()).is_none());
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
    let u = super::layout::rect_union(rects.into_iter()).unwrap();
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
    let u = super::layout::rect_union(rects.into_iter()).unwrap();
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

    let result = backend::map_raw_to_domain(raw);

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
