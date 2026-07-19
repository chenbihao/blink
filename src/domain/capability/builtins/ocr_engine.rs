//! OCR Backend 抽象（0.11.7-c 引入，0.11.7-f 能力化，0.11.9-b word 级链路）。
//!
//! **命名注**：文件仍叫 `ocr_engine.rs`（避免大重命名），但**语义已改**——`OcrEngine`
//! trait → `OcrBackend` trait（对齐 `ScreenshotBackend` 命名）。旧名 `OcrEngine`
//! 作为类型别名保留避免破坏面。
//!
//! **架构**：
//! - `OcrBackend` trait — 可 mock 的 OCR 平台抽象
//! - `WindowsOcrBackend` — 生产实现（`Windows.Media.Ocr` WinRT API）
//! - `FakeOcrBackend` — 测试实现（返回预定义文本）
//! - `install_backend()` / `backend()` — 全局单例注入（对齐 ScreenshotBackend 模式）
//!
//! **Windows.Media.Ocr 要求**：Windows 10 1809+，中文语言包已安装时自动识别中文。
//! 无中文语言包时仍可识别英文。
//!
//! **0.11.9-b word 级链路**：
//! - `OcrLine.Words()` → `OcrWord { text, rect, line_index }`（SDK 原生给的 word 级坐标）
//! - `OcrLine.bounding_rect` 真填（原为固定 `{0,0,0,0}`），用该行 words 的 union
//! - `OcrLine.word_indices` 指回 `OcrResult.words` flat 数组的 index 段
//! - `OcrResult.text` 走 `join_words_smart` 智能拼接（CJK↔CJK 无空格 / Latin↔Latin 有空格）
//!   替代 SDK `Text()` 的"每字夹空格"输出。前端"移除空格"按钮退化为兜底。

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

/// 智能拼接 word 列表为完整文本（0.11.9-b）。
///
/// 规则：
/// - CJK ↔ CJK：不加空格（"你好" + "世界" → "你好世界"）
/// - CJK ↔ Latin/Digit：不加空格（"温度" + "25" → "温度25"）
/// - Latin/Digit ↔ Latin/Digit：加一个空格（"hello" + "world" → "hello world"）
/// - 其它（含标点） ↔ 任意：按 Latin 规则处理（保守地加空格,除非另一侧是 CJK）
/// - 同一行内按 word 顺序拼；行末换行 `\n`
///
/// 依赖 `words[i].line_index` 严格递增（0-based）。如果 words 为空但 lines 有内容
/// (SDK 只给 line 没给 word——不太可能但兜底)，退化为 line.text 用 `\n` join。
///
/// 独立可测（下方 `tests` 模块覆盖）。
pub fn join_words_smart(words: &[OcrWord], lines: &[OcrLine]) -> String {
    // 兜底：words 为空 → 用 lines.text
    if words.is_empty() {
        return lines
            .iter()
            .map(|l| l.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
    }

    let mut out = String::new();
    let mut prev_line: Option<usize> = None;
    let mut prev_tail: Option<WordKind> = None;

    for w in words {
        if w.text.is_empty() {
            continue;
        }
        // 换行
        if let Some(pl) = prev_line
            && pl != w.line_index
        {
            out.push('\n');
            prev_tail = None; // 行首,不需要前置空格
        }
        // 词间空格
        if let Some(tk) = &prev_tail {
            let hk = head_kind(&w.text);
            let need_space = match (tk, &hk) {
                (WordKind::Cjk, _) | (_, WordKind::Cjk) => false,
                _ => true, // Latin/Latin, Latin/Other, Other/Latin, Other/Other → 加
            };
            if need_space {
                out.push(' ');
            }
        }
        out.push_str(&w.text);
        prev_tail = Some(tail_kind(&w.text));
        prev_line = Some(w.line_index);
    }

    out
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

/// OCR 后端 trait（0.11.7-f 重命名自 `OcrEngine`）。
#[async_trait::async_trait]
pub trait OcrBackend: Send + Sync {
    /// 识别 PNG 图片中的文字
    async fn recognize(&self, png_data: &[u8]) -> Result<OcrResult, OcrError>;
}

/// 旧类型别名（供 0.11.7-c 遗留代码使用；新代码用 `OcrBackend`）。
#[allow(dead_code)]
pub type OcrEngine = dyn OcrBackend;

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

/// 获取当前 OCR backend（0.11.7-f）。
///
/// **首次调用兜底**：Windows 平台自动装 `WindowsOcrBackend`。
pub fn backend() -> Arc<dyn OcrBackend> {
    let lock = BACKEND.get_or_init(|| {
        #[cfg(target_os = "windows")]
        let default: Arc<dyn OcrBackend> = Arc::new(WindowsOcrBackend);
        #[cfg(not(target_os = "windows"))]
        let default: Arc<dyn OcrBackend> = Arc::new(WindowsOcrBackend); // fallback 也是 stub
        RwLock::new(default)
    });
    lock.read().expect("OCR backend RwLock 中毒").clone()
}

/// **兼容层**：旧调用者拿全局单例（0.11.7-c）。
///
/// 新代码走 `backend()` 拿 `Arc<dyn OcrBackend>`。
#[allow(dead_code)]
pub fn get_ocr_engine() -> Arc<dyn OcrBackend> {
    backend()
}

// ── WindowsOcrBackend 实现 ──────────────────────────────────

/// Windows.Media.Ocr 实现的 OCR 后端。
///
/// 内部使用 WinRT API，通过 `windows-rs` 绑定调用。
#[cfg(target_os = "windows")]
pub struct WindowsOcrBackend;

#[cfg(target_os = "windows")]
#[async_trait::async_trait]
impl OcrBackend for WindowsOcrBackend {
    #[allow(unused_qualifications)]
    async fn recognize(&self, png_data: &[u8]) -> Result<OcrResult, OcrError> {
        use windows::Graphics::Imaging::{BitmapDecoder, BitmapPixelFormat, SoftwareBitmap};
        use windows::Media::Ocr::OcrEngine as WinRtOcrEngine;
        use windows::Storage::Streams::{DataWriter, InMemoryRandomAccessStream};

        // 1. 创建 InMemoryRandomAccessStream 并写入 PNG 字节
        let stream = InMemoryRandomAccessStream::new()
            .map_err(|e| OcrError::Engine(format!("创建流失败: {e}")))?;

        let writer = DataWriter::CreateDataWriter(&stream)
            .map_err(|e| OcrError::Engine(format!("创建 DataWriter 失败: {e}")))?;

        writer
            .WriteBytes(png_data)
            .map_err(|e| OcrError::Engine(format!("写入流失败: {e}")))?;

        let _store_result = writer
            .StoreAsync()
            .map_err(|e| OcrError::Engine(format!("StoreAsync 失败: {e}")))?
            .await
            .map_err(|e| OcrError::Engine(format!("StoreAsync await 失败: {e}")))?;

        stream
            .Seek(0)
            .map_err(|e| OcrError::Engine(format!("Seek 失败: {e}")))?;

        let decoder = BitmapDecoder::CreateAsync(&stream)
            .map_err(|e| OcrError::Engine(format!("创建 BitmapDecoder 失败: {e}")))?
            .await
            .map_err(|e| OcrError::Engine(format!("BitmapDecoder await 失败: {e}")))?;

        let software_bitmap = decoder
            .GetSoftwareBitmapAsync()
            .map_err(|e| OcrError::Engine(format!("GetSoftwareBitmap 失败: {e}")))?
            .await
            .map_err(|e| OcrError::Engine(format!("GetSoftwareBitmap await 失败: {e}")))?;

        let bgra_bitmap = SoftwareBitmap::Convert(&software_bitmap, BitmapPixelFormat::Bgra8)
            .map_err(|e| OcrError::Engine(format!("转换 BGRA8 失败: {e}")))?;

        let ocr_engine = WinRtOcrEngine::TryCreateFromUserProfileLanguages()
            .map_err(|e| OcrError::Engine(format!("创建 OcrEngine 失败: {e}")))?;

        let ocr_result = ocr_engine
            .RecognizeAsync(&bgra_bitmap)
            .map_err(|e| OcrError::Engine(format!("RecognizeAsync 失败: {e}")))?
            .await
            .map_err(|e| OcrError::Engine(format!("等待识别完成失败: {e}")))?;

        // 0.11.9-b：读 word 级数据代替直接拿 Text()。
        // SDK Text() 会在 CJK 字符之间插空格,前端只能靠正则强清(副作用:英文也丢词间空格)。
        // 走 words + join_words_smart 从根上拼对。
        let text_angle: Option<f64> = ocr_result
            .TextAngle()
            .ok()
            .and_then(|opt| opt.Value().ok())
            .map(|d| d as f64);

        let mut words: Vec<OcrWord> = Vec::new();
        let mut lines: Vec<OcrLine> = Vec::new();

        if let Ok(lines_raw) = ocr_result.Lines() {
            let line_count = lines_raw.Size().unwrap_or(0);
            for i in 0..line_count {
                let Ok(line) = lines_raw.GetAt(i) else {
                    continue;
                };
                let line_text = line.Text().unwrap_or_default().to_string();
                let mut line_word_indices: Vec<usize> = Vec::new();

                if let Ok(words_raw) = line.Words() {
                    let word_count = words_raw.Size().unwrap_or(0);
                    for j in 0..word_count {
                        let Ok(w) = words_raw.GetAt(j) else {
                            continue;
                        };
                        let text = w.Text().unwrap_or_default().to_string();
                        if text.is_empty() {
                            continue;
                        }
                        let rect = w
                            .BoundingRect()
                            .map(|r| OcrRect {
                                x: r.X.round() as i32,
                                y: r.Y.round() as i32,
                                w: r.Width.round().max(0.0) as u32,
                                h: r.Height.round().max(0.0) as u32,
                            })
                            .unwrap_or(OcrRect { x: 0, y: 0, w: 0, h: 0 });
                        let idx = words.len();
                        words.push(OcrWord {
                            text,
                            bounding_rect: rect,
                            line_index: i as usize,
                        });
                        line_word_indices.push(idx);
                    }
                }

                let line_rect = rect_union(
                    line_word_indices
                        .iter()
                        .map(|&idx| words[idx].bounding_rect),
                )
                .unwrap_or(OcrRect { x: 0, y: 0, w: 0, h: 0 });

                // 跳过空行(SDK 偶尔给空 Line + 空 Words),不进 lines
                if !line_text.is_empty() || !line_word_indices.is_empty() {
                    lines.push(OcrLine {
                        text: line_text,
                        bounding_rect: line_rect,
                        word_indices: line_word_indices,
                    });
                }
            }
        }

        let text = join_words_smart(&words, &lines);

        Ok(OcrResult {
            text,
            lines,
            words,
            text_angle,
        })
    }
}

/// 非 Windows 平台回退
#[cfg(not(target_os = "windows"))]
pub struct WindowsOcrBackend;

#[cfg(not(target_os = "windows"))]
#[async_trait::async_trait]
impl OcrBackend for WindowsOcrBackend {
    async fn recognize(&self, _png_data: &[u8]) -> Result<OcrResult, OcrError> {
        Err(OcrError::Unsupported)
    }
}

// ── FakeOcrBackend（测试用） ───────────────────────────────────────────────

/// 测试用假 OCR 后端。构造时配置固定返回值。
///
/// 0.11.9-b：支持配置 `words` 让 Capability 层测试 word 级链路。
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct FakeOcrBackend {
    text: String,
    lines: Vec<OcrLine>,
    words: Vec<OcrWord>,
    err: Option<String>,
}

#[allow(dead_code)]
impl FakeOcrBackend {
    pub fn returning(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            lines: Vec::new(),
            words: Vec::new(),
            err: None,
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
        }
    }
}

#[async_trait::async_trait]
impl OcrBackend for FakeOcrBackend {
    async fn recognize(&self, _png_data: &[u8]) -> Result<OcrResult, OcrError> {
        if let Some(msg) = &self.err {
            return Err(OcrError::Engine(msg.clone()));
        }
        Ok(OcrResult {
            text: self.text.clone(),
            lines: self.lines.clone(),
            words: self.words.clone(),
            text_angle: None,
        })
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

    // ── join_words_smart（0.11.9-b） ─────────────────────────────────

    fn w(text: &str, line: usize) -> OcrWord {
        OcrWord {
            text: text.into(),
            bounding_rect: OcrRect { x: 0, y: 0, w: 0, h: 0 },
            line_index: line,
        }
    }

    #[test]
    fn join_pure_cjk_no_spaces() {
        // "你好" "世界" 在同一行 → "你好世界"(SDK 会把每字当 word,不能夹空格)
        let words = vec![w("你", 0), w("好", 0), w("世", 0), w("界", 0)];
        assert_eq!(join_words_smart(&words, &[]), "你好世界");
    }

    #[test]
    fn join_pure_latin_has_single_space() {
        let words = vec![w("hello", 0), w("world", 0)];
        assert_eq!(join_words_smart(&words, &[]), "hello world");
    }

    #[test]
    fn join_cjk_latin_no_space() {
        // 中英混排应贴一起("温度 25 度" → "温度25度")——用户 OCR 中文页面时更符合直觉
        let words = vec![w("温度", 0), w("25", 0), w("度", 0)];
        assert_eq!(join_words_smart(&words, &[]), "温度25度");
    }

    #[test]
    fn join_multiline_uses_newline() {
        let words = vec![w("first", 0), w("line", 0), w("第二", 1), w("行", 1)];
        assert_eq!(join_words_smart(&words, &[]), "first line\n第二行");
    }

    #[test]
    fn join_ascii_punct_between_latin_gets_space() {
        // "Hello , world" —— SDK 通常把标点当独立 word,现规则:标点走 Other,与
        // Latin 相邻加空格;下游需要"贴标点"体验时前端可再兜底(不属核心链路)
        let words = vec![w("Hello", 0), w(",", 0), w("world", 0)];
        assert_eq!(join_words_smart(&words, &[]), "Hello , world");
    }

    #[test]
    fn join_cjk_and_punct_no_space() {
        // "你好,世界" —— CJK 侧一定不加空格,不论对面是标点还是字
        let words = vec![w("你好", 0), w(",", 0), w("世界", 0)];
        assert_eq!(join_words_smart(&words, &[]), "你好,世界");
    }

    #[test]
    fn join_empty_words_falls_back_to_lines_text() {
        let lines = vec![
            OcrLine {
                text: "fallback line 1".into(),
                bounding_rect: OcrRect { x: 0, y: 0, w: 0, h: 0 },
                word_indices: vec![],
            },
            OcrLine {
                text: "fallback line 2".into(),
                bounding_rect: OcrRect { x: 0, y: 0, w: 0, h: 0 },
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
        // 空 word text 不影响相邻 word 拼接规则(比如两个 latin word 之间夹一个空 word 也应加空格)
        let words = vec![w("hello", 0), w("", 0), w("world", 0)];
        assert_eq!(join_words_smart(&words, &[]), "hello world");
    }

    #[test]
    fn join_line_boundary_resets_leading_space() {
        // 行首不该有前置空格(第一 word 是 latin 也不加)
        let words = vec![w("末尾", 0), w("hello", 1)];
        assert_eq!(join_words_smart(&words, &[]), "末尾\nhello");
    }

    // ── rect_union 边界 ─────────────────────────────────

    #[test]
    fn rect_union_returns_none_when_all_zero() {
        let empties = [OcrRect { x: 0, y: 0, w: 0, h: 0 }; 3];
        assert!(rect_union(empties.into_iter()).is_none());
    }

    #[test]
    fn rect_union_computes_bounding_box() {
        let rects = vec![
            OcrRect { x: 10, y: 20, w: 30, h: 40 },  // 右下 (40, 60)
            OcrRect { x: 50, y: 5,  w: 20, h: 10 },  // 右下 (70, 15)
        ];
        let u = rect_union(rects.into_iter()).unwrap();
        assert_eq!(u.x, 10);
        assert_eq!(u.y, 5);
        assert_eq!(u.w, 60);  // 70 - 10
        assert_eq!(u.h, 55);  // 60 - 5
    }

    #[test]
    fn rect_union_skips_zero_rects() {
        let rects = vec![
            OcrRect { x: 0, y: 0, w: 0, h: 0 },
            OcrRect { x: 10, y: 10, w: 20, h: 20 },
        ];
        let u = rect_union(rects.into_iter()).unwrap();
        assert_eq!(u.x, 10);
        assert_eq!(u.w, 20);
    }
}
