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
/// - word flat 数组构建 + line_index 回填
/// - line bounding_rect = 该行 words 的 union
/// - `join_words_smart` 智能拼接全文
fn map_raw_to_domain(raw: crate::infra::platform::ocr::RawOcrResult) -> OcrResult {
    let mut words: Vec<OcrWord> = Vec::new();
    let mut lines: Vec<OcrLine> = Vec::new();

    for (line_idx, raw_line) in raw.lines.iter().enumerate() {
        let mut line_word_indices: Vec<usize> = Vec::new();

        for raw_word in &raw_line.words {
            let rect = OcrRect {
                x: raw_word.rect.x.round() as i32,
                y: raw_word.rect.y.round() as i32,
                w: raw_word.rect.width.round().max(0.0) as u32,
                h: raw_word.rect.height.round().max(0.0) as u32,
            };
            let idx = words.len();
            words.push(OcrWord {
                text: raw_word.text.clone(),
                bounding_rect: rect,
                line_index: line_idx,
            });
            line_word_indices.push(idx);
        }

        let line_rect = rect_union(
            line_word_indices
                .iter()
                .map(|&idx| words[idx].bounding_rect),
        )
        .unwrap_or(OcrRect {
            x: 0,
            y: 0,
            w: 0,
            h: 0,
        });

        lines.push(OcrLine {
            text: raw_line.text.clone(),
            bounding_rect: line_rect,
            word_indices: line_word_indices,
        });
    }

    let text = join_words_smart(&words, &lines);

    OcrResult {
        text,
        lines,
        words,
        text_angle: raw.text_angle,
    }
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
            text: self.text.clone(),
            lines: self.lines.clone(),
            words: self.words.clone(),
            text_angle: None,
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

    // ── join_words_smart（0.11.9-b） ─────────────────────────────────

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
}
