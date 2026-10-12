//! 丝滑小妙招（使用指南）command（0.25.28）。
//!
//! `docs/guide.md`（中文，真源）与 `docs/guide.en.md`（英文翻译）双文档：
//! GitHub 在线渲染 + 编译期 `include_str!` 嵌入，双端同源。内嵌版随二进制
//! 走，永远与安装版本的行为一致；在线版随 main 分支更新。前端按当前语言
//! 取对应正文，语言切换时重渲染。两份文档的章节结构对齐由 tests 兜底
//! （防单边增删章节后静默漂移）。文件存在性由 xtask release-check 的
//! EMBEDDED_RESOURCES 清单兜底校验（结构锚点见 tests）。

use serde::Serialize;

/// 使用指南中文版（编译期嵌入，真源 docs/guide.md）。
const GUIDE_MARKDOWN: &str = include_str!("../../../docs/guide.md");

/// 使用指南英文版（编译期嵌入，真源 docs/guide.en.md；中文版为真源，允许滞后）。
const GUIDE_MARKDOWN_EN: &str = include_str!("../../../docs/guide.en.md");

/// 中文在线版地址（仓库 docs/guide.md 的 blob 页）。
const GUIDE_ONLINE_URL: &str = concat!(
    env!("CARGO_PKG_REPOSITORY"),
    "/blob/main/docs/guide.md"
);

/// 英文在线版地址（仓库 docs/guide.en.md 的 blob 页）。
const GUIDE_ONLINE_URL_EN: &str = concat!(
    env!("CARGO_PKG_REPOSITORY"),
    "/blob/main/docs/guide.en.md"
);

/// 指南文档 DTO：双语正文 + 对应版本号 + 双语在线版链接。
#[derive(Serialize)]
pub struct GuideDoc {
    pub markdown: String,
    pub markdown_en: String,
    pub version: String,
    pub online_url: String,
    pub online_url_en: String,
}

/// 获取内嵌的使用指南（设置页「丝滑小妙招」tab 一次性拉取渲染，语言由前端定）。
///
/// 内容是编译期常量，无失败路径，不返回 Result。
#[tauri::command]
pub fn get_guide_markdown(app: tauri::AppHandle) -> GuideDoc {
    GuideDoc {
        markdown: GUIDE_MARKDOWN.to_string(),
        markdown_en: GUIDE_MARKDOWN_EN.to_string(),
        version: app.package_info().version.to_string(),
        online_url: GUIDE_ONLINE_URL.to_string(),
        online_url_en: GUIDE_ONLINE_URL_EN.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 产物正确性：指南必须是结构完整的 Markdown 文档（一级标题 + 章节标题
    /// + 快捷键总表锚点），防止误删/误清空后静默进安装包。
    #[test]
    fn guide_markdown_has_doc_structure() {
        assert!(
            GUIDE_MARKDOWN.starts_with("# Blink"),
            "指南应以一级标题「# Blink」开头"
        );
        assert!(
            GUIDE_MARKDOWN.contains("\n## "),
            "指南应包含章节标题（## ）"
        );
        assert!(
            GUIDE_MARKDOWN.contains("快捷键总表"),
            "指南应包含「快捷键总表」章节"
        );
        assert!(
            GUIDE_MARKDOWN.contains("你可能不知道"),
            "指南应包含「你可能不知道」速览章节"
        );
    }

    /// 英文版结构锚点：应用内按「\n## 1.」切片裁掉文档头，H1/编号章节/目录
    /// 缺失都会让内嵌渲染开天窗。
    #[test]
    fn guide_en_markdown_has_doc_structure() {
        assert!(
            GUIDE_MARKDOWN_EN.starts_with("# Blink"),
            "EN guide should start with an \"# Blink\" H1"
        );
        assert!(
            GUIDE_MARKDOWN_EN.contains("\n## Contents"),
            "EN guide should contain a \"## Contents\" section"
        );
        assert!(
            GUIDE_MARKDOWN_EN.contains("\n## 1. "),
            "EN guide should contain numbered \"## 1. \" sections"
        );
        assert!(
            GUIDE_MARKDOWN_EN.contains("Keyboard shortcuts reference"),
            "EN guide should contain the shortcuts reference section"
        );
    }

    /// 中英章节对齐：两份文档的 h2 总数与编号章节（## N.）的编号序列必须
    /// 一致（标题文本本就随语言不同，只比结构）。双语最大的维护风险是单边
    /// 增删章节导致结构漂移，这里给廉价报警。
    #[test]
    fn guide_zh_en_numbered_sections_align() {
        let h2_count = |md: &str| md.lines().filter(|l| l.starts_with("## ")).count();
        let numbers = |md: &str| -> Vec<u32> {
            md.lines()
                .filter_map(|l| l.strip_prefix("## "))
                .filter_map(|h| h.split('.').next()?.trim().parse().ok())
                .collect()
        };
        let (zh_total, en_total) = (h2_count(GUIDE_MARKDOWN), h2_count(GUIDE_MARKDOWN_EN));
        assert!(zh_total > 0, "中文版应包含章节标题");
        assert_eq!(zh_total, en_total, "中英文版 h2 章节总数应一致");
        let (zh_nums, en_nums) = (numbers(GUIDE_MARKDOWN), numbers(GUIDE_MARKDOWN_EN));
        assert!(!zh_nums.is_empty(), "中文版应包含编号章节");
        assert_eq!(
            zh_nums, en_nums,
            "中英文版的编号章节序列应一致（如 1~10）"
        );
    }

    /// 中文在线版链接应指向本仓库 docs/guide.md。
    #[test]
    fn guide_online_url_points_to_repo_docs() {
        assert!(GUIDE_ONLINE_URL.ends_with("/blob/main/docs/guide.md"));
        assert!(GUIDE_ONLINE_URL.starts_with("https://github.com/"));
    }

    /// 英文在线版链接应指向本仓库 docs/guide.en.md。
    #[test]
    fn guide_online_url_en_points_to_repo_docs() {
        assert!(GUIDE_ONLINE_URL_EN.ends_with("/blob/main/docs/guide.en.md"));
        assert!(GUIDE_ONLINE_URL_EN.starts_with("https://github.com/"));
    }
}
