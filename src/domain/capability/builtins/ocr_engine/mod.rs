//! OCR Backend 抽象（0.11.7-c 引入，0.11.7-f 能力化，0.11.9-b word 级链路）。
//!
//! **架构**：
//! - [`types`] — DTO、几何类型与结果类型（`OcrResult`, `OcrLine`, `OcrWord`, `OcrCharBox`, `OcrRect`, `OcrError`）
//! - [`layout`] — 同行聚合、行内拼接、结果重建与诊断统计
//! - [`backend`] — `OcrBackend` trait、`WindowsOcrBackendAdapter`、全局注入
//! - [`fake`] — 测试用 `FakeOcrBackend`
//!
//! **0.14.7 W2**：WinRT 调用和原始 DTO 提取已迁至 `infra/platform/ocr/`。
//! 本模块只保留领域类型、智能拼接和 raw → domain 映射。
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
//!
//! **0.22 收尾**：从单文件 `ocr_engine.rs` 拆为子模块，按职责分离类型、布局、后端与测试。

pub mod backend;
#[cfg(test)]
mod fake;
pub mod layout;
#[cfg(test)]
mod tests;
pub mod types;

// ── 公共 re-export（保持旧路径 `ocr_engine::*` 可用） ──────────────────────
#[cfg(test)] // ocr_image 测试经旧路径 `ocr_engine::install_backend` 注入 fake
pub use backend::install_backend;
#[allow(unused_imports)]
pub use backend::{OcrBackend, WindowsOcrBackendAdapter, backend};
#[cfg(test)] // ocr_image 测试经旧路径 `ocr_engine::FakeOcrBackend` 构造 fake
pub use fake::FakeOcrBackend;
#[allow(unused_imports)]
pub use layout::{
    LayoutDiagnostics, group_words_into_lines_with_diag, rebuild_with_line_grouping,
    rebuild_with_line_grouping_and_diag,
};
pub use types::{OcrCharBox, OcrError, OcrLine, OcrRect, OcrResult, OcrWord};
