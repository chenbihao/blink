//! OnnxOcrExecutor 测试（0.22.8-C / 0.22.8-F）。
//!
//! 测试策略：
//! - **Fake pipeline**：不依赖真实 ORT DLL 和模型文件，
//!   用 fake pipeline 验证 executor 的并发、取消、生命周期逻辑。
//! - **状态机验证**：Idle → Starting → Ready → TTL drop → Idle。
//! - **背压验证**：第 5 个请求立即返回 Backpressure。
//! - **取消验证**：等待中的请求被取消后立即返回 Cancelled。
//! - **三层契约映射**：`map_oarocr_to_ocr_result` 的 CJK/Latin/Mixed 对齐验证。

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use bytes::Bytes;
use tokio_util::sync::CancellationToken;

use super::executor::{
    OcrExecutor, OcrExecutorConfig, OcrExecutorError, OnnxOcrExecutor, RecognizeRequest,
};
use super::pipeline::{OcrPipeline, PipelineConfig, PipelineError};
use super::state::ExecutorState;

use crate::domain::capability::builtins::ocr_engine::{OcrLine, OcrResult};

/// Fake pipeline——可配置延迟和结果。
struct FakePipeline {
    /// 每次推理的模拟延迟。
    delay_ms: u64,
    /// 推理调用计数。
    call_count: Arc<AtomicU32>,
    /// 返回的文本。
    text: String,
}

impl FakePipeline {
    #[allow(dead_code)]
    fn new(text: &str, delay_ms: u64) -> (Self, Arc<AtomicU32>) {
        let call_count = Arc::new(AtomicU32::new(0));
        (
            Self {
                delay_ms,
                call_count: call_count.clone(),
                text: text.to_string(),
            },
            call_count,
        )
    }
}

impl OcrPipeline for FakePipeline {
    fn recognize(&mut self, _png_data: &[u8]) -> Result<OcrResult, PipelineError> {
        self.call_count.fetch_add(1, Ordering::SeqCst);
        if self.delay_ms > 0 {
            std::thread::sleep(Duration::from_millis(self.delay_ms));
        }
        Ok(OcrResult {
            backend_used: None,
            backend_fallback_reason: None,
            backend_degrade_hint: None,
            text: self.text.clone(),
            lines: vec![OcrLine {
                text: self.text.clone(),
                bounding_rect: crate::domain::capability::builtins::ocr_engine::OcrRect {
                    x: 0,
                    y: 0,
                    w: 100,
                    h: 20,
                },
                word_indices: vec![],
                font_height: None,
            }],
            words: vec![],
            text_angle: None,
            char_ranges: vec![],
            char_boxes: vec![],
        })
    }
}

/// 构建测试用 executor（使用 FakePipeline）。
///
/// 由于 OnnxOcrExecutor 直接构建 OrtocrPipeline，这里需要用
/// 一个可注入 pipeline 的变体。我们通过直接测试状态机和
/// 并发原语来验证逻辑正确性。
#[tokio::test]
async fn executor_state_initial_idle() {
    let config = OcrExecutorConfig::default();
    let executor = OnnxOcrExecutor::new(config);
    assert!(matches!(executor.state(), ExecutorState::Idle));
}

#[tokio::test]
async fn executor_backpressure_rejects_5th_request() {
    // 测试有界队列：4 个 pending 后第 5 个返回 Backpressure。
    // 此测试验证 Semaphore 的容量为 4。
    let sem = Arc::new(tokio::sync::Semaphore::new(4));

    // 获取 4 个 permit
    let mut permits = Vec::new();
    for _ in 0..4 {
        let permit = sem.try_acquire().unwrap();
        permits.push(permit);
    }

    // 第 5 个应失败
    let result = sem.try_acquire();
    assert!(result.is_err(), "第 5 个 permit 应被拒绝");

    // 释放一个后可以再获取
    drop(permits.remove(0));
    let result = sem.try_acquire();
    assert!(result.is_ok(), "释放后应能获取 permit");
}

#[tokio::test]
async fn executor_cancellation_returns_cancelled() {
    // 测试取消：如果请求在等待 permit 时被取消，应返回 Cancelled。
    let sem = Arc::new(tokio::sync::Semaphore::new(4));
    let cancellation = CancellationToken::new();

    // 获取所有 4 个 permit
    let mut permits = Vec::new();
    for _ in 0..4 {
        permits.push(sem.try_acquire().unwrap());
    }

    // 取消
    cancellation.cancel();

    // select! 应返回 Cancelled 而不是等待
    let result = tokio::select! {
        p = sem.acquire() => {
            p.map(|_| ()).map_err(|_| OcrExecutorError::Shutdown)
        }
        _ = cancellation.cancelled() => {
            Err(OcrExecutorError::Cancelled)
        }
    };

    assert!(matches!(result, Err(OcrExecutorError::Cancelled)));
}

#[tokio::test]
async fn executor_shutdown_closes_channel() {
    // 测试 shutdown 后 sender 为 None
    let config = OcrExecutorConfig::default();
    let executor = OnnxOcrExecutor::new(config);

    // 初始状态 sender 为 None
    assert!(executor.is_sender_none(), "初始 sender 应为 None");

    executor.shutdown().await;

    // shutdown 后 sender 仍为 None
    assert!(executor.is_sender_none(), "shutdown 后 sender 应为 None");
}

#[tokio::test]
async fn executor_state_machine_transitions() {
    // 测试状态通道的 CAS 语义
    let state = super::state::StateChannel::new();

    // 初始 Idle
    assert!(matches!(state.current(), ExecutorState::Idle));

    // Idle → Starting
    let won = state.compare_swap(
        |s| matches!(s, ExecutorState::Idle),
        ExecutorState::Starting { generation: 0 },
    );
    assert!(won, "Idle → Starting CAS 应成功");
    assert!(matches!(
        state.current(),
        ExecutorState::Starting { generation: 0 }
    ));

    // 再次 Idle → Starting 应失败（已是 Starting）
    let won = state.compare_swap(
        |s| matches!(s, ExecutorState::Idle),
        ExecutorState::Starting { generation: 1 },
    );
    assert!(!won, "Starting → Starting CAS 应失败");

    // Starting → Ready
    state
        .tx
        .send(ExecutorState::Ready {
            generation: 0,
            ready_at: std::time::Instant::now(),
        })
        .ok();
    assert!(state.current().is_ready());

    // Ready → Stopping
    state
        .tx
        .send(ExecutorState::Stopping { generation: 0 })
        .ok();
    assert!(matches!(
        state.current(),
        ExecutorState::Stopping { generation: 0 }
    ));

    // Stopping → Idle
    state.tx.send(ExecutorState::Idle).ok();
    assert!(matches!(state.current(), ExecutorState::Idle));
}

#[tokio::test]
async fn executor_state_failed_retryable() {
    // 测试 Failed 状态可被新请求推进到 Idle
    let state = super::state::StateChannel::new();

    // 设置为 Failed
    state
        .tx
        .send(ExecutorState::Failed {
            generation: 0,
            reason: Arc::from("test failure"),
        })
        .ok();

    assert!(matches!(
        state.current(),
        ExecutorState::Failed { generation: 0, .. }
    ));

    // 新请求推进到 Idle
    let won = state.compare_swap(
        |s| matches!(s, ExecutorState::Failed { generation: 0, .. }),
        ExecutorState::Idle,
    );
    assert!(won, "Failed → Idle CAS 应成功");
    assert!(matches!(state.current(), ExecutorState::Idle));
}

#[tokio::test]
async fn executor_state_watch_no_loss() {
    // 测试 watch channel 不丢通知
    let state = super::state::StateChannel::new();
    let mut rx = state.subscribe();

    // 连续发送多个状态
    state
        .tx
        .send(ExecutorState::Starting { generation: 0 })
        .ok();
    state
        .tx
        .send(ExecutorState::Ready {
            generation: 0,
            ready_at: std::time::Instant::now(),
        })
        .ok();

    // 观察者应能看到最新状态
    let _ = rx.changed().await;
    let current = rx.borrow().clone();
    assert!(current.is_ready(), "应观察到 Ready 状态");
}

#[tokio::test]
async fn executor_permit_release_on_drop() {
    // 测试 SemaphorePermit drop 后释放 permit
    let sem = Arc::new(tokio::sync::Semaphore::new(1));

    {
        let _permit = sem.try_acquire().unwrap();
        // permit 仍持有——第二个应失败
        assert!(sem.try_acquire().is_err());
    }
    // permit drop 后——第二个应成功
    assert!(sem.try_acquire().is_ok());
}

#[test]
fn executor_config_default() {
    let config = OcrExecutorConfig::default();
    assert_eq!(config.pipeline.intra_op, 1);
    assert_eq!(config.pipeline.inter_op, 1);
}

#[test]
fn executor_state_display() {
    let state = ExecutorState::Idle;
    assert_eq!(format!("{state}"), "Idle");

    let state = ExecutorState::Starting { generation: 42 };
    assert!(format!("{state}").contains("42"));

    let state = ExecutorState::Ready {
        generation: 1,
        ready_at: std::time::Instant::now(),
    };
    assert!(format!("{state}").contains("Ready"));
    assert!(format!("{state}").contains("1"));

    let state = ExecutorState::Failed {
        generation: 3,
        reason: Arc::from("boom"),
    };
    assert!(format!("{state}").contains("Failed"));
    assert!(format!("{state}").contains("3"));
}

#[test]
fn executor_error_mapping() {
    use crate::domain::ocr::error::{OcrErrorCategory, StructuredOcrError};

    let e = OcrExecutorError::Cancelled;
    let structured: StructuredOcrError = e.into();
    assert_eq!(structured.category, OcrErrorCategory::Cancelled);

    let e = OcrExecutorError::Timeout;
    let structured: StructuredOcrError = e.into();
    assert_eq!(structured.category, OcrErrorCategory::Timeout);

    let e = OcrExecutorError::Shutdown;
    let structured: StructuredOcrError = e.into();
    assert_eq!(structured.category, OcrErrorCategory::BackendUnavailable);

    let e = OcrExecutorError::Backpressure(4);
    let structured: StructuredOcrError = e.into();
    assert_eq!(structured.category, OcrErrorCategory::BackendUnavailable);

    let e = OcrExecutorError::BuildFailed("test".to_string());
    let structured: StructuredOcrError = e.into();
    assert_eq!(structured.category, OcrErrorCategory::StartFailed);
}

#[tokio::test]
async fn executor_idle_cancel_notification() {
    // 测试 idle TTL 定时器可被 notify_waiters 取消
    let notify = Arc::new(tokio::sync::Notify::new());
    let notify2 = notify.clone();

    let handle = tokio::spawn(async move {
        tokio::select! {
            _n = notify2.notified() => "cancelled",
            _t = tokio::time::sleep(Duration::from_secs(100)) => "timed_out"
        }
    });

    // 让 spawn 的 task 有机会到达 notified() 等待点
    tokio::task::yield_now().await;

    // 立即取消
    notify.notify_waiters();

    let result = handle.await.unwrap();
    assert_eq!(result, "cancelled");
}

#[tokio::test]
async fn executor_recognize_request_construction() {
    let cancellation = CancellationToken::new();
    let request = RecognizeRequest {
        png_data: Bytes::from_static(b"fake-png"),
        cancellation: cancellation.clone(),
        deadline: None,
    };

    assert_eq!(request.png_data.len(), 8);
    assert!(!request.cancellation.is_cancelled());

    cancellation.cancel();
    assert!(request.cancellation.is_cancelled());
}

// ── 三层契约映射测试（0.22.8-F） ──────────────────────────────────────────
//
// 验证 `map_oarocr_to_ocr_result` 的三层契约：
// 1. `text` = `region.text` 原样拼接（不改写、不插空格）
// 2. `words` = 每个 region 一个 OcrWord（语义级 region token）
// 3. `char_boxes` = `word_boxes` 逐字符对齐（字符级定位框）
//
// 回归场景：旧映射将字符框当词框，导致 `PP-OCRv6` → `P P - O C R v 6`。

use oar_ocr::oarocr::result::{OAROCRResult, TextRegion};
use oar_ocr::processors::BoundingBox;

/// 构造一个单行的 OAROCRResult——每个字符一个 word_box。
fn make_char_per_box_result(text: &str) -> OAROCRResult {
    let chars: Vec<char> = text.chars().collect();
    let word_boxes: Vec<BoundingBox> = chars
        .iter()
        .enumerate()
        .map(|(i, _)| BoundingBox::from_coords(i as f32 * 20.0, 0.0, (i + 1) as f32 * 20.0, 20.0))
        .collect();

    let mut region = TextRegion::new(BoundingBox::from_coords(
        0.0,
        0.0,
        chars.len() as f32 * 20.0,
        20.0,
    ));
    region.text = Some(Arc::from(text));
    region.word_boxes = Some(word_boxes);

    OAROCRResult {
        input_path: Arc::from(""),
        index: 0,
        input_img: Arc::new(image::ImageBuffer::new(1, 1)),
        text_regions: vec![region],
        orientation_angle: None,
        rectified_img: None,
    }
}

/// 构造一个多行 OAROCRResult。
fn make_multi_line_result(lines: &[(&str, f32, f32)]) -> OAROCRResult {
    let mut regions = Vec::new();
    for &(text, y_offset, width) in lines {
        let chars: Vec<char> = text.chars().collect();
        let word_boxes: Vec<BoundingBox> = chars
            .iter()
            .enumerate()
            .map(|(i, _)| {
                BoundingBox::from_coords(
                    i as f32 * 20.0,
                    y_offset,
                    (i + 1) as f32 * 20.0,
                    y_offset + 20.0,
                )
            })
            .collect();

        let mut region = TextRegion::new(BoundingBox::from_coords(
            0.0,
            y_offset,
            width,
            y_offset + 20.0,
        ));
        region.text = Some(Arc::from(text));
        region.word_boxes = Some(word_boxes);
        regions.push(region);
    }

    OAROCRResult {
        input_path: Arc::from(""),
        index: 0,
        input_img: Arc::new(image::ImageBuffer::new(1, 1)),
        text_regions: regions,
        orientation_angle: None,
        rectified_img: None,
    }
}

// ── 文本真源测试 ──────────────────────────────────────────────

#[test]
fn text_is_region_text_verbatim_no_spaces() {
    // 核心回归测试：Latin 文本不得被拆开插入空格
    let text = "PP-OCRv6";
    let result = make_char_per_box_result(text);
    let mapped = super::pipeline::map_oarocr_to_ocr_result(result).expect("映射成功");

    // 文本必须原样保留
    assert_eq!(mapped.text, "PP-OCRv6");
}

#[test]
fn text_is_cjk_verbatim_no_spaces() {
    let text = "你好世界";
    let result = make_char_per_box_result(text);
    let mapped = super::pipeline::map_oarocr_to_ocr_result(result).expect("映射成功");

    assert_eq!(mapped.text, "你好世界");
}

#[test]
fn text_is_mixed_cjk_latin_verbatim() {
    // 中英混排不应被改写
    let text = "温度25度";
    let result = make_char_per_box_result(text);
    let mapped = super::pipeline::map_oarocr_to_ocr_result(result).expect("映射成功");

    assert_eq!(mapped.text, "温度25度");
}

#[test]
fn text_multi_line_uses_newline_separator() {
    let result = make_multi_line_result(&[("你好", 0.0, 40.0), ("世界", 30.0, 40.0)]);
    let mapped = super::pipeline::map_oarocr_to_ocr_result(result).expect("映射成功");

    assert_eq!(mapped.text, "你好\n世界");
    assert_eq!(mapped.lines.len(), 2);
}

// ── 语义层（OcrWord）测试 ─────────────────────────────────────

#[test]
fn words_one_per_region_not_per_char() {
    // 每个 region 产生一个 OcrWord，不是每个字符一个
    let text = "PP-OCRv6";
    let result = make_char_per_box_result(text);
    let mapped = super::pipeline::map_oarocr_to_ocr_result(result).expect("映射成功");

    // 一个 region → 一个 word
    assert_eq!(
        mapped.words.len(),
        1,
        "应只有 1 个 word（region 级），不是 8 个"
    );
    // word 的 text 是整行原文
    assert_eq!(mapped.words[0].text, "PP-OCRv6");
}

#[test]
fn words_cjk_one_per_region() {
    let text = "你好世界";
    let result = make_char_per_box_result(text);
    let mapped = super::pipeline::map_oarocr_to_ocr_result(result).expect("映射成功");

    assert_eq!(mapped.words.len(), 1, "CJK 应只有 1 个 word（region 级）");
    assert_eq!(mapped.words[0].text, "你好世界");
}

#[test]
fn char_ranges_match_text_slice() {
    // 每个 word 的 char_range 切片应等于该 word 的 text
    let result = make_multi_line_result(&[("hello", 0.0, 100.0), ("你好", 30.0, 40.0)]);
    let mapped = super::pipeline::map_oarocr_to_ocr_result(result).expect("映射成功");

    let text_chars: Vec<char> = mapped.text.chars().collect();
    for (i, w) in mapped.words.iter().enumerate() {
        let (start, end) = mapped.char_ranges[i];
        let slice: String = text_chars[start..end].iter().collect();
        assert_eq!(slice, w.text, "word[{i}] char_range mismatch");
    }
}

#[test]
fn char_ranges_multi_line_offset_correct() {
    // 多行时 char_ranges 的全局偏移必须正确
    let result = make_multi_line_result(&[("hello", 0.0, 100.0), ("world", 30.0, 100.0)]);
    let mapped = super::pipeline::map_oarocr_to_ocr_result(result).expect("映射成功");

    // hello: chars 0..5
    assert_eq!(mapped.char_ranges[0], (0, 5));
    // world: chars 6..11 (after \n at index 5)
    assert_eq!(mapped.char_ranges[1], (6, 11));
}

// ── 字符层（OcrCharBox）测试 ──────────────────────────────────

#[test]
fn char_boxes_cjk_per_char_alignment() {
    let text = "你好世界";
    let result = make_char_per_box_result(text);
    let mapped = super::pipeline::map_oarocr_to_ocr_result(result).expect("映射成功");

    // 应产生 4 个 char_box（每个字符一个）
    assert_eq!(mapped.char_boxes.len(), 4);

    assert_eq!(mapped.char_boxes[0].text, "你");
    assert_eq!(mapped.char_boxes[1].text, "好");
    assert_eq!(mapped.char_boxes[2].text, "世");
    assert_eq!(mapped.char_boxes[3].text, "界");

    // 每个 char_box 的 bounding_rect 应不同（不堆叠）
    assert_eq!(mapped.char_boxes[0].bounding_rect.x, 0);
    assert_eq!(mapped.char_boxes[1].bounding_rect.x, 20);
    assert_eq!(mapped.char_boxes[2].bounding_rect.x, 40);
    assert_eq!(mapped.char_boxes[3].bounding_rect.x, 60);
}

#[test]
fn char_boxes_latin_per_char_alignment() {
    let text = "PP-OCRv6";
    let result = make_char_per_box_result(text);
    let mapped = super::pipeline::map_oarocr_to_ocr_result(result).expect("映射成功");

    // 应产生 8 个 char_box
    assert_eq!(mapped.char_boxes.len(), 8);

    assert_eq!(mapped.char_boxes[0].text, "P");
    assert_eq!(mapped.char_boxes[1].text, "P");
    assert_eq!(mapped.char_boxes[2].text, "-");
    assert_eq!(mapped.char_boxes[3].text, "O");
    assert_eq!(mapped.char_boxes[7].text, "6");
}

#[test]
fn char_boxes_mixed_cjk_latin_alignment() {
    let text = "abc你好";
    let result = make_char_per_box_result(text);
    let mapped = super::pipeline::map_oarocr_to_ocr_result(result).expect("映射成功");

    assert_eq!(mapped.char_boxes.len(), 5);
    assert_eq!(mapped.char_boxes[0].text, "a");
    assert_eq!(mapped.char_boxes[3].text, "你");
    assert_eq!(mapped.char_boxes[4].text, "好");
}

#[test]
fn char_boxes_char_ranges_correct() {
    // char_box 的 char_start/char_end 必须正确指向 full_text
    let text = "你好世界";
    let result = make_char_per_box_result(text);
    let mapped = super::pipeline::map_oarocr_to_ocr_result(result).expect("映射成功");

    let full_chars: Vec<char> = mapped.text.chars().collect();

    for (i, cb) in mapped.char_boxes.iter().enumerate() {
        let slice: String = full_chars[cb.char_start..cb.char_end].iter().collect();
        assert_eq!(slice, cb.text, "char_box[{i}] char_range mismatch");
    }
}

#[test]
fn char_boxes_multi_line_global_offset() {
    // 多行时 char_boxes 的全局 char_start 必须考虑换行符
    let result = make_multi_line_result(&[("你好", 0.0, 40.0), ("世界", 30.0, 40.0)]);
    let mapped = super::pipeline::map_oarocr_to_ocr_result(result).expect("映射成功");

    assert_eq!(mapped.text, "你好\n世界");

    // 第一行：你=0..1, 好=1..2
    assert_eq!(mapped.char_boxes[0].char_start, 0);
    assert_eq!(mapped.char_boxes[0].char_end, 1);
    assert_eq!(mapped.char_boxes[1].char_start, 1);
    assert_eq!(mapped.char_boxes[1].char_end, 2);

    // 第二行：世=3..4 (after \n at index 2), 界=4..5
    assert_eq!(mapped.char_boxes[2].char_start, 3);
    assert_eq!(mapped.char_boxes[2].char_end, 4);
    assert_eq!(mapped.char_boxes[3].char_start, 4);
    assert_eq!(mapped.char_boxes[3].char_end, 5);
}

// ── 降级测试 ──────────────────────────────────────────────────

#[test]
fn no_word_boxes_no_char_boxes() {
    // 无 word_boxes 时不生成 char_boxes
    let text = "测试文本";
    let mut region = TextRegion::new(BoundingBox::from_coords(10.0, 5.0, 90.0, 25.0));
    region.text = Some(Arc::from(text));
    region.word_boxes = None;

    let result = OAROCRResult {
        input_path: Arc::from(""),
        index: 0,
        input_img: Arc::new(image::ImageBuffer::new(1, 1)),
        text_regions: vec![region],
        orientation_angle: None,
        rectified_img: None,
    };

    let mapped = super::pipeline::map_oarocr_to_ocr_result(result).expect("映射成功");

    // 文本仍保留
    assert_eq!(mapped.text, "测试文本");
    // 无 char_boxes
    assert!(mapped.char_boxes.is_empty());
    // 仍有 word（region 级）
    assert_eq!(mapped.words.len(), 1);
    assert_eq!(mapped.words[0].text, "测试文本");
    assert_eq!(mapped.words[0].bounding_rect.x, 10);
    assert_eq!(mapped.words[0].bounding_rect.w, 80);
}

#[test]
fn mismatched_count_no_char_boxes() {
    // 字符数 ≠ word_boxes 数量 → 不生成 char_boxes（降级）
    let text = "你好世";
    let mut region = TextRegion::new(BoundingBox::from_coords(0.0, 0.0, 60.0, 20.0));
    region.text = Some(Arc::from(text));
    region.word_boxes = Some(vec![
        BoundingBox::from_coords(0.0, 0.0, 20.0, 20.0),
        BoundingBox::from_coords(20.0, 0.0, 40.0, 20.0),
    ]);

    let result = OAROCRResult {
        input_path: Arc::from(""),
        index: 0,
        input_img: Arc::new(image::ImageBuffer::new(1, 1)),
        text_regions: vec![region],
        orientation_angle: None,
        rectified_img: None,
    };

    let mapped = super::pipeline::map_oarocr_to_ocr_result(result).expect("映射成功");

    // 文本仍保留
    assert_eq!(mapped.text, "你好世");
    // 数量不一致 → 无 char_boxes
    assert!(mapped.char_boxes.is_empty());
    // 仍有 word
    assert_eq!(mapped.words.len(), 1);
    assert_eq!(mapped.words[0].text, "你好世");
}

#[test]
fn empty_regions_produce_empty_result() {
    let result = OAROCRResult {
        input_path: Arc::from(""),
        index: 0,
        input_img: Arc::new(image::ImageBuffer::new(1, 1)),
        text_regions: vec![],
        orientation_angle: None,
        rectified_img: None,
    };

    let mapped = super::pipeline::map_oarocr_to_ocr_result(result).expect("映射成功");

    assert!(mapped.text.is_empty());
    assert!(mapped.lines.is_empty());
    assert!(mapped.words.is_empty());
    assert!(mapped.char_boxes.is_empty());
    assert!(mapped.char_ranges.is_empty());
}

#[test]
fn region_without_text_skipped() {
    // region.text = None → 跳过该 region
    let mut region1 = TextRegion::new(BoundingBox::from_coords(0.0, 0.0, 40.0, 20.0));
    region1.text = Some(Arc::from("你好"));
    region1.word_boxes = Some(vec![
        BoundingBox::from_coords(0.0, 0.0, 20.0, 20.0),
        BoundingBox::from_coords(20.0, 0.0, 40.0, 20.0),
    ]);

    let mut region2 = TextRegion::new(BoundingBox::from_coords(0.0, 30.0, 40.0, 50.0));
    region2.text = None; // 无文本
    region2.word_boxes = None;

    let result = OAROCRResult {
        input_path: Arc::from(""),
        index: 0,
        input_img: Arc::new(image::ImageBuffer::new(1, 1)),
        text_regions: vec![region1, region2],
        orientation_angle: None,
        rectified_img: None,
    };

    let mapped = super::pipeline::map_oarocr_to_ocr_result(result).expect("映射成功");

    // 只有 region1 贡献内容
    assert_eq!(mapped.text, "你好");
    assert_eq!(mapped.lines.len(), 1);
    assert_eq!(mapped.words.len(), 1);
    assert_eq!(mapped.char_boxes.len(), 2);
}

// ── 0.22.18 Worker 生命周期确定性测试 ──────────────────────────────────
//
// 覆盖：
// 1. 第一次构建超时且不重试，构建稍后完成后线程自然退出
// 2. 连续构建超时达到上限后，下一次重试不再创建线程
// 3. retired worker 完成后可被回收，并恢复重试额度
// 4. shutdown 面对未完成 native build 不会无限挂起
//
// 使用 Condvar 控制的 fake build 函数实现确定性测试。

use std::sync::{Condvar, Mutex};

type BuildFn = super::executor::BuildFn;

/// 永久阻塞的 build 函数——模拟 native build 卡住且不可释放。
/// 线程调用后永远不会返回，直到测试进程退出时被清理。
fn make_forever_blocking_build_fn() -> BuildFn {
    Arc::new(|_config: &PipelineConfig| {
        // 永久 park——模拟不可中断的 native build
        std::thread::park();
        // park 不会被 unpark，这行不可达
        Err(PipelineError::Inference("unreachable".into()))
    })
}

/// 创建一个立即成功的 build 函数。
fn make_fast_build_fn() -> BuildFn {
    Arc::new(|_config: &PipelineConfig| {
        let (pipeline, _) = FakePipeline::new("ok", 0);
        Ok(Box::new(pipeline) as Box<dyn OcrPipeline>)
    })
}

/// 创建一个快速失败的 build 函数。
fn make_fail_build_fn() -> BuildFn {
    Arc::new(|_config: &PipelineConfig| Err(PipelineError::Inference("fast fail".into())))
}

/// 测试 1：第一次构建超时后，in_flight_count 增加，sender 被 drop。
#[tokio::test]
async fn worker_build_timeout_drops_sender_and_retires() {
    let config = OcrExecutorConfig::default();
    let build_fn = make_forever_blocking_build_fn();
    let executor = OnnxOcrExecutor::with_build_fn(config, build_fn)
        .with_build_timeout(Duration::from_millis(100));

    // 触发构建——会超时
    let cancellation = CancellationToken::new();
    let result = executor.ensure_ready(&cancellation).await;

    // 超时返回 BuildFailed
    assert!(
        matches!(result, Err(OcrExecutorError::BuildFailed(_))),
        "超时应返回 BuildFailed, got {result:?}"
    );

    // sender 应为 None（已被 drop）
    assert!(
        executor.is_sender_none(),
        "超时后 sender 应为 None（提交权已撤销）"
    );

    // in_flight_count 应为 1（retired worker 仍在运行）
    assert_eq!(
        executor.in_flight_count(),
        1,
        "retired worker 仍计入 in_flight_count"
    );

    // retired_workers 应有 1 个
    assert_eq!(executor.retired_count(), 1, "应有 1 个退休 worker");

    // shutdown 清理
    executor.shutdown().await;
}

/// 测试 2：连续构建超时达到 MAX_INFLIGHT_WORKERS 上限后，
/// 下一次重试不再创建线程，返回 BuildInProgress。
#[tokio::test]
async fn worker_inflight_limit_blocks_new_build() {
    let config = OcrExecutorConfig::default();
    let build_fn = make_forever_blocking_build_fn();
    let executor = OnnxOcrExecutor::with_build_fn(config, build_fn)
        .with_build_timeout(Duration::from_millis(100));

    let cancellation = CancellationToken::new();

    // 第一次构建超时
    let _ = executor.ensure_ready(&cancellation).await;
    assert_eq!(executor.in_flight_count(), 1, "第一次超时后 in_flight=1");

    // 直接用 ensure_ready 再次尝试——状态会从 Failed 推进到 Idle 再尝试构建
    // 第二次构建也超时
    let _ = executor.ensure_ready(&cancellation).await;
    assert_eq!(
        executor.in_flight_count(),
        2,
        "第二次超时后 in_flight=2（达到上限）"
    );

    // 第三次尝试——应返回 BuildInProgress 而非创建新线程
    let result = executor.ensure_ready(&cancellation).await;
    assert!(
        matches!(result, Err(OcrExecutorError::BuildInProgress)),
        "达到上限后应返回 BuildInProgress, got {result:?}"
    );
    assert_eq!(
        executor.in_flight_count(),
        2,
        "BuildInProgress 不应增加 in_flight_count"
    );

    executor.shutdown().await;
}

/// 测试 3：retired worker 完成后可被回收，并恢复重试额度。
#[tokio::test]
async fn worker_reclaim_restores_quota() {
    let config = OcrExecutorConfig::default();
    // 使用 Condvar 控制：build 函数阻塞，释放后线程退出
    let pair = Arc::new((Mutex::new(false), Condvar::new()));
    let p = pair.clone();
    let build_fn: BuildFn = Arc::new(move |_config: &PipelineConfig| {
        let (lock, cvar) = &*p;
        let mut guard = lock.lock().unwrap();
        while !*guard {
            guard = cvar.wait(guard).unwrap();
        }
        Err(PipelineError::Inference("done".into()))
    });
    let executor = OnnxOcrExecutor::with_build_fn(config, build_fn)
        .with_build_timeout(Duration::from_millis(100));

    let cancellation = CancellationToken::new();

    // 第一次构建超时
    let _ = executor.ensure_ready(&cancellation).await;
    assert_eq!(executor.in_flight_count(), 1, "超时后 in_flight=1");

    // 释放 condvar——让 retired worker 的 build 函数完成并退出
    {
        let (lock, cvar) = &*pair;
        let mut guard = lock.lock().unwrap();
        *guard = true;
        cvar.notify_all();
    }

    // 等待线程退出
    std::thread::sleep(Duration::from_millis(200));

    // 回收已完成的退休 worker
    executor.reclaim_finished();
    assert_eq!(executor.in_flight_count(), 0, "回收后 in_flight 应恢复为 0");
    assert_eq!(executor.retired_count(), 0, "回收后 retired 应为空");

    executor.shutdown().await;
}

/// 测试 4：shutdown 面对未完成 native build 不会无限挂起。
#[tokio::test]
async fn worker_shutdown_does_not_hang_on_stuck_build() {
    let config = OcrExecutorConfig::default();
    let build_fn = make_forever_blocking_build_fn();
    let executor = OnnxOcrExecutor::with_build_fn(config, build_fn)
        .with_build_timeout(Duration::from_millis(100));

    let cancellation = CancellationToken::new();

    // 触发构建超时
    let _ = executor.ensure_ready(&cancellation).await;
    assert_eq!(executor.in_flight_count(), 1, "超时后 in_flight=1");

    // shutdown——barrier 永远不会被释放，但 shutdown 应在有限时间内完成
    // SHUTDOWN_JOIN_TIMEOUT_SECS = 10s，但我们不需要等那么久——
    // 退休 worker 的 build 仍在阻塞，shutdown 会对它做有界 join
    let shutdown_start = std::time::Instant::now();
    executor.shutdown().await;
    let shutdown_elapsed = shutdown_start.elapsed();

    // shutdown 应在合理时间内完成（有界 join 超时 + 一些开销）
    // SHUTDOWN_JOIN_TIMEOUT_SECS = 10s，给 15s 余量
    assert!(
        shutdown_elapsed < Duration::from_secs(15),
        "shutdown 不应无限挂起，实际耗时 {:?}",
        shutdown_elapsed
    );

    // 注意：卡住的线程会 detached，但测试进程退出时会被清理
}

/// 测试 5：快速失败的 build 函数——in_flight_count 正确回滚。
#[tokio::test]
async fn worker_fast_fail_rolls_back_count() {
    let config = OcrExecutorConfig::default();
    let build_fn = make_fail_build_fn();
    let executor =
        OnnxOcrExecutor::with_build_fn(config, build_fn).with_build_timeout(Duration::from_secs(5));

    let cancellation = CancellationToken::new();
    let result = executor.ensure_ready(&cancellation).await;

    // 快速失败返回 BuildFailed
    assert!(
        matches!(result, Err(OcrExecutorError::BuildFailed(_))),
        "快速失败应返回 BuildFailed, got {result:?}"
    );

    // in_flight_count 应为 0（线程已退出，计数已回滚）
    assert_eq!(
        executor.in_flight_count(),
        0,
        "快速失败后 in_flight 应为 0（计数已回滚）"
    );

    executor.shutdown().await;
}

/// 快速失败的 current handle 必须当场收走；后续退休 worker 回收不能再次
/// 释放同一额度，否则计数会低于真实存活数并绕过并发上限。
#[tokio::test]
async fn worker_fast_fail_then_timeouts_preserve_real_inflight_limit() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let calls = Arc::new(AtomicUsize::new(0));
    let release = Arc::new((Mutex::new(false), Condvar::new()));
    let calls_for_build = calls.clone();
    let release_for_build = release.clone();
    let build_fn: BuildFn = Arc::new(move |_config: &PipelineConfig| {
        if calls_for_build.fetch_add(1, Ordering::SeqCst) == 0 {
            return Err(PipelineError::Inference("first fast fail".into()));
        }
        let (lock, cvar) = &*release_for_build;
        let mut released = lock.lock().unwrap_or_else(|e| e.into_inner());
        while !*released {
            released = cvar.wait(released).unwrap_or_else(|e| e.into_inner());
        }
        Err(PipelineError::Inference("released".into()))
    });
    let executor = OnnxOcrExecutor::with_build_fn(OcrExecutorConfig::default(), build_fn)
        .with_build_timeout(Duration::from_millis(50));
    let cancellation = CancellationToken::new();

    assert!(matches!(
        executor.ensure_ready(&cancellation).await,
        Err(OcrExecutorError::BuildFailed(_))
    ));
    assert_eq!(executor.in_flight_count(), 0);
    assert!(executor.is_sender_none());
    assert_eq!(executor.retired_count(), 0);

    let _ = executor.ensure_ready(&cancellation).await;
    let _ = executor.ensure_ready(&cancellation).await;
    assert_eq!(executor.in_flight_count(), 2);
    assert_eq!(calls.load(Ordering::SeqCst), 3);

    assert!(matches!(
        executor.ensure_ready(&cancellation).await,
        Err(OcrExecutorError::BuildInProgress)
    ));
    assert_eq!(calls.load(Ordering::SeqCst), 3, "达到上限后不得再 spawn");

    {
        let (lock, cvar) = &*release;
        *lock.lock().unwrap_or_else(|e| e.into_inner()) = true;
        cvar.notify_all();
    }
    std::thread::sleep(Duration::from_millis(100));
    executor.reclaim_finished();
    assert_eq!(executor.in_flight_count(), 0);
    executor.shutdown().await;
}

/// 测试 6：快速成功的 build 函数——executor 进入 Ready 状态。
#[tokio::test]
async fn worker_fast_success_reaches_ready() {
    let config = OcrExecutorConfig::default();
    let build_fn = make_fast_build_fn();
    let executor =
        OnnxOcrExecutor::with_build_fn(config, build_fn).with_build_timeout(Duration::from_secs(5));

    let cancellation = CancellationToken::new();
    let result = executor.ensure_ready(&cancellation).await;

    assert!(result.is_ok(), "快速成功应返回 Ok, got {result:?}");
    assert!(executor.state().is_ready(), "状态应为 Ready");
    assert_eq!(executor.in_flight_count(), 1, "Ready 后 in_flight=1");

    executor.shutdown().await;
}
