//! OCR 领域 DTO、几何类型与结果类型。
//!
//! 0.22 收尾：从 `ocr_engine.rs` 按职责拆出，保持 re-export 路径不变。

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
    /// 显式 PaddleOCR 模式因环境未安装而降级 WinRT 时的用户提示。
    ///
    /// 由 capability 层注入（configured=PaddleOcr 且 selected=Windows 且成功），
    /// 前端 toast 直接展示；auto 模式降级属预期行为，不注入提示。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backend_degrade_hint: Option<String>,
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
    /// 字号参考高度（PP-OCR det 框 unclip 折减，翻译嵌图字号推导用）。
    ///
    /// PP-OCR 的 DBNet 检测框经 unclip 外扩后，`rect.h` 系统性大于实际字形
    /// 高度，直接按 rect.h 推导嵌图字号会偏大。此字段给出折减后的参考高度，
    /// **仅用于字号推导**——背景覆盖、hit-test 仍用 `rect`。
    /// `None` 表示无需折减（WinRT 词框 union 已紧贴字形），前端回退 rect.h。
    #[serde(rename = "font_h", skip_serializing_if = "Option::is_none")]
    pub font_height: Option<u32>,
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
    pub(crate) fn is_zero(self) -> bool {
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

/// OCR 引擎错误
#[derive(Debug)]
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
