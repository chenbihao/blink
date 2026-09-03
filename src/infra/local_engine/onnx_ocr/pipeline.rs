//! ORT OCR pipeline 封装（0.22.8-C / 0.22.8-F）。
//!
//! 封装 `oar_ocr::OAROCR` 的构建与推理调用，隔离 `ort` crate API
//! 的细节，使 executor 模块可以测试时替换为 fake pipeline。
//!
//! ## 设计铁则
//!
//! - **工作线程上构建**：`OAROCRBuilder::build()` 调用
//!   `ort::Session::builder().commit_from_file()`，这是同步阻塞操作，
//!   必须在专用工作线程上执行，不阻塞 tokio runtime。
//! - **Send but not Sync**：`OAROCR` 持有 ORT Session，ORT 内部
//!   不是 `Sync`。因此 pipeline 只能在专用工作线程上使用。
//! - **PNG → RGB → predict → text**：输入是 PNG bytes，
//!   pipeline 负责解码为 `image::RgbImage` 后执行推理。
//!
//! ## 三层契约（0.22.8-F）
//!
//! `map_oarocr_to_ocr_result` 遵循三层分离：
//!
//! | 层 | 字段 | 来源 | 用途 |
//! |---|---|---|---|
//! | **文本真源** | `OcrResult.text` | `TextRegion.text`（原文不可改写） | 文本插入 / 复制 |
//! | **语义单元** | `OcrResult.words` + `char_ranges` | 每个 region 一个 `OcrWord` | 行级 grouping / 阅读顺序 |
//! | **字符定位** | `OcrResult.char_boxes` | `TextRegion.word_boxes`（逐字符框） | 图片 hit-test / 高亮 |
//!
//! **关键修正**：`oar_ocr` 的 `word_boxes` 实际是**逐字符框**（CJK 和 Latin
//! 均为字符级），不应被映射为 `OcrWord`。旧映射将每个字符框当作一个 word，
//! 导致 `join_words_intra_line_with_gaps` 在 Latin 字符间插入空格
//! （如 `PP-OCRv6` → `P P - O C R v 6`）。
//!
//! 新映射策略：
//! 1. `region.text` → `OcrResult.text`（按行 `\n` 拼接，**不改写原文**）
//! 2. 每个 region → 一个 `OcrWord`（语义级 region token）+ 一个 `OcrLine`
//! 3. `region.word_boxes` → `OcrCharBox`（字符级定位框，含全局 char range）
//! 4. `char_ranges` 在 word 级生成，指向 `OcrResult.text` 中的 Rust char 范围

use std::path::{Path, PathBuf};

use oar_ocr::processors::BoundingBox;

use crate::domain::capability::builtins::ocr_engine::{
    OcrCharBox, OcrLine, OcrRect, OcrResult, OcrWord,
};

/// OCR pipeline trait——可替换为 fake 实现（测试用）。
pub trait OcrPipeline: Send + 'static {
    /// 执行 OCR 识别。
    ///
    /// 输入：PNG bytes（来自截图/capability）。
    /// 输出：`OcrResult`（与 WinRT/PaddleOCR 后端格式一致）。
    fn recognize(&mut self, png_data: &[u8]) -> Result<OcrResult, PipelineError>;
}

/// pipeline 错误。
#[derive(Debug, thiserror::Error)]
pub enum PipelineError {
    #[error("图片解码失败: {0}")]
    Decode(String),
    #[error("OCR 推理失败: {0}")]
    Inference(String),
    #[error("Session 未构建")]
    NotBuilt,
}

/// 真实 ORT OCR pipeline——封装 `oar_ocr::OAROCR`。
pub struct OrtocrPipeline {
    /// OAROCR pipeline（持有 det + rec Session）。
    ocr: oar_ocr::oarocr::OAROCR,
}

impl OrtocrPipeline {
    /// 在当前线程上构建 pipeline。
    ///
    /// **必须在专用工作线程上调用**——会阻塞加载 DLL 和模型文件。
    pub fn build(
        det_model: impl AsRef<Path>,
        rec_model: impl AsRef<Path>,
        dict_path: impl AsRef<Path>,
        dll_path: impl AsRef<Path>,
        intra_op: u32,
        inter_op: u32,
    ) -> Result<Self, PipelineError> {
        tracing::info!(
            det = %det_model.as_ref().display(),
            rec = %rec_model.as_ref().display(),
            dict = %dict_path.as_ref().display(),
            dll = %dll_path.as_ref().display(),
            "构建 OrtocrPipeline"
        );

        // 1. 初始化 ORT（加载 DLL）
        // ort::init_from 是进程级 OnceLock——首次调用加载 DLL，
        // 后续调用返回已加载的 builder（commit() 返回 false）。
        match ort::init_from(dll_path.as_ref()) {
            Ok(builder) => {
                let committed = builder.commit();
                tracing::debug!(committed, "ORT init_from + commit");
            }
            Err(e) => {
                return Err(PipelineError::Inference(format!("ORT DLL 加载失败: {e}")));
            }
        }

        // 2. 构建 OrtSessionConfig
        use oar_ocr::core::config::{
            OrtExecutionProvider, OrtGraphOptimizationLevel, OrtSessionConfig,
        };

        let session_config = OrtSessionConfig::new()
            .with_intra_threads(intra_op as usize)
            .with_optimization_level(OrtGraphOptimizationLevel::Level1)
            .with_execution_providers(vec![OrtExecutionProvider::CPU]);

        // inter_op 设置（oar-ocr API 可能不支持，预留）
        let _ = inter_op;

        // 3. 构建 OAROCR pipeline
        let ocr = oar_ocr::oarocr::OAROCRBuilder::new(
            det_model.as_ref().to_str().unwrap_or(""),
            rec_model.as_ref().to_str().unwrap_or(""),
            dict_path.as_ref().to_str().unwrap_or(""),
        )
        .ort_session(session_config)
        .return_word_box(true)
        .build()
        .map_err(|e| PipelineError::Inference(format!("OAROCR build 失败: {e}")))?;

        tracing::info!("OrtocrPipeline 构建成功");

        Ok(Self { ocr })
    }
}

impl OcrPipeline for OrtocrPipeline {
    fn recognize(&mut self, png_data: &[u8]) -> Result<OcrResult, PipelineError> {
        // 1. 解码 PNG → RgbImage
        let img = image::load_from_memory(png_data)
            .map_err(|e| PipelineError::Decode(format!("PNG 解码失败: {e}")))?
            .to_rgb8();

        // 2. 执行推理
        let results = self
            .ocr
            .predict(vec![img])
            .map_err(|e| PipelineError::Inference(format!("OCR predict 失败: {e}")))?;

        // 3. 映射为 OcrResult
        let result =
            results
                .into_iter()
                .next()
                .unwrap_or_else(|| oar_ocr::oarocr::result::OAROCRResult {
                    input_path: std::sync::Arc::from(""),
                    index: 0,
                    input_img: std::sync::Arc::new(image::ImageBuffer::new(1, 1)),
                    text_regions: vec![],
                    orientation_angle: None,
                    rectified_img: None,
                });

        map_oarocr_to_ocr_result(result)
    }
}

/// 将 `oar_ocr` 的 `OAROCRResult` 映射为 Blink 的 `OcrResult`。
///
/// ## 三层契约（0.22.8-F）
///
/// ### 1. 文本真源：`region.text`
///
/// `OcrResult.text` = 所有 region 的 `text` 用 `\n` 拼接。
/// **原文不可改写**——不执行 `split_whitespace`、不插入空格、不重排字符。
/// 这修复了旧映射将字符框当词框导致 `PP-OCRv6` 变成 `P P - O C R v 6` 的缺陷。
///
/// ### 2. 语义单元：`OcrWord` + `OcrLine`
///
/// 每个 `TextRegion` → 一个 `OcrWord`（region 级语义 token）+ 一个 `OcrLine`。
/// `OcrWord.text` = `region.text`（整行原文），`bounding_rect` = `region.bounding_box`。
/// `char_ranges[word_i]` = 该 region 在 `OcrResult.text` 中的 Rust char 范围。
///
/// ### 3. 字符定位：`OcrCharBox`
///
/// `region.word_boxes` 实际是**逐字符框**（oar-ocr 语义：CJK 和 Latin 均为字符级）。
/// 映射为 `OcrCharBox`（不伪装成 `OcrWord`），含全局 char range
/// （`char_start` / `char_end` 相对于 `OcrResult.text`）。
///
/// **降级**：当字符数 ≠ `word_boxes` 数量时，不生成 `char_boxes`，
/// 退化为纯 region 级映射（前端回退到 `words` hit-test）。
pub(super) fn map_oarocr_to_ocr_result(
    result: oar_ocr::oarocr::result::OAROCRResult,
) -> Result<OcrResult, PipelineError> {
    let mut text_parts: Vec<String> = Vec::new();
    let mut lines: Vec<OcrLine> = Vec::new();
    let mut words: Vec<OcrWord> = Vec::new();
    let mut char_boxes: Vec<OcrCharBox> = Vec::new();
    let mut char_ranges: Vec<(usize, usize)> = Vec::new();

    // 全局 char offset（Rust 字符索引，追踪在 full_text 中的位置）
    let mut global_offset = 0usize;

    for (line_idx, region) in result.text_regions.iter().enumerate() {
        if let Some(ref text) = region.text {
            let line_text = text.to_string();
            let line_char_count = line_text.chars().count();

            text_parts.push(line_text.clone());

            // ── 语义层：一个 region → 一个 OcrWord + 一个 OcrLine ──
            let word_start = global_offset;
            let word_end = global_offset + line_char_count;
            let word_idx = words.len();

            let line_rect = bbox_to_ocr_rect(&region.bounding_box);

            words.push(OcrWord {
                text: line_text.clone(),
                bounding_rect: line_rect,
                line_index: line_idx,
            });

            char_ranges.push((word_start, word_end));

            lines.push(OcrLine {
                text: line_text,
                bounding_rect: line_rect,
                word_indices: vec![word_idx],
            });

            // ── 字符层：word_boxes → OcrCharBox（逐字符对齐）──
            //
            // oar_ocr 的 word_boxes 语义：CJK 和 Latin 均为逐字符框。
            // 当字符数 == word_boxes 数量时，逐字符对齐生成 char_boxes。
            // 数量不一致时不生成 char_boxes（降级为纯 word 级）。
            if let Some(ref word_boxes) = region.word_boxes {
                let line_chars: Vec<char> = text.chars().collect();

                if line_chars.len() == word_boxes.len() {
                    // 逐字符对齐
                    for (char_index, (ch, wb)) in
                        line_chars.iter().zip(word_boxes.iter()).enumerate()
                    {
                        let char_len = ch.len_utf8();
                        let _ = char_len; // UTF-8 byte len not needed; we use char count
                        let char_start = global_offset + char_index;
                        char_boxes.push(OcrCharBox {
                            text: ch.to_string(),
                            bounding_rect: bbox_to_ocr_rect(wb),
                            line_index: line_idx,
                            char_start,
                            char_end: char_start + 1,
                        });
                    }
                } else {
                    // 数量不一致——记日志，不生成 char_boxes
                    tracing::warn!(
                        line_index = line_idx,
                        char_count = line_chars.len(),
                        word_box_count = word_boxes.len(),
                        "字符数与 word_boxes 数量不一致，跳过 char_boxes 生成"
                    );
                }
            }

            // 推进全局 offset：行文本 + 换行符
            global_offset += line_char_count;
            if line_idx < result.text_regions.len() - 1 {
                global_offset += 1; // \n
            }
        }
    }

    let full_text = text_parts.join("\n");
    let text_angle = result.orientation_angle.map(|a| a as f64);

    tracing::debug!(
        regions = result.text_regions.len(),
        lines = lines.len(),
        words = words.len(),
        char_boxes = char_boxes.len(),
        text_chars = full_text.chars().count(),
        "map_oarocr_to_ocr_result 完成"
    );

    Ok(OcrResult {
        text: full_text,
        lines,
        words,
        text_angle,
        char_ranges,
        char_boxes,
    })
}

/// 将 `BoundingBox` 转换为 `OcrRect`。
pub(super) fn bbox_to_ocr_rect(bbox: &BoundingBox) -> OcrRect {
    let x_min = bbox.x_min().max(0.0) as i32;
    let y_min = bbox.y_min().max(0.0) as i32;
    let x_max = bbox.x_max().max(0.0) as i32;
    let y_max = bbox.y_max().max(0.0) as i32;
    OcrRect {
        x: x_min,
        y: y_min,
        w: (x_max - x_min).max(0) as u32,
        h: (y_max - y_min).max(0) as u32,
    }
}

/// Pipeline 配置——传递给工作线程用于构建 Session。
#[derive(Debug, Clone)]
pub struct PipelineConfig {
    pub det_model: PathBuf,
    pub rec_model: PathBuf,
    pub dict_path: PathBuf,
    pub dll_path: PathBuf,
    pub intra_op: u32,
    pub inter_op: u32,
}
