//! 浏览器搜索引擎导入（0.26.21）：Chromium `Web Data` keywords 表行 → 超链导入候选。
//!
//! 纯函数模块：SQLite 读取在 app 层 command（`scan_browser_search_engines`），
//! 这里只做 TemplateURL → `{query}` 模板转换、过滤与触发词判重。fixture 取自
//! 本机 Chrome 真实 `keywords` 表样本（百度 `{inputEncoding}`、torrents 站
//! `{startPage?}`、iStock `&amp;`、starter pack chrome://、Google `{google:RLZ}`）。

use serde::Serialize;

use crate::config::shards::WebLinkEntry;
use crate::intent::KeywordOwner;

/// `keywords` 表行的最小投影（app 层 SELECT 只取这三列；usage_count 仅排序用）。
#[derive(Debug, Clone, PartialEq)]
pub struct KeywordRow {
    pub short_name: String,
    pub keyword: String,
    pub url: String,
}

/// 跳过原因（serde snake_case；前端按 `weblinks.import.skip.{reason}` 出文案）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ImportSkipReason {
    /// 非 http(s) 网页地址（chrome:// starter pack、空 URL 等）。
    NotHttp,
    /// TemplateURL 高级 token 残留（`{google:RLZ}` 等），转换后无法保证可打开。
    UnsupportedTemplate,
    /// 触发词无效（空 / 含空白 / 保留前缀 `ai`）。
    InvalidKeyword,
    /// 触发词已存在（超链表内或导入批内重复）。
    Duplicate,
    /// 触发词已被其他 engine / 插件占用。
    Taken,
}

/// 导入候选（scan 命令返回；确认导入由前端并入 `app.weblinks` 走既有保存链路，
/// 本模块不产生任何写路径）。
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ImportCandidate {
    pub name: String,
    pub keyword: String,
    /// 已转换为 `{query}` 占位符的 URL 模板；可为无占位符的直达地址。
    /// 跳过项保留原始 URL 供预览展示（转换失败时无模板可言）。
    pub url_template: String,
    /// None = 可导入；Some(原因) = 预览标灰跳过。
    pub skip: Option<ImportSkipReason>,
}

/// XML 实体反转义（单层：`&amp;` 最后替换，`&amp;lt;` 结果为 `&lt;` 不二次展开）。
/// Chromium 写入 OpenSearch 发现的 URL 时按 XML 转义（实测 iStock 行含 `&amp;`）。
fn unescape_xml_entities(raw: &str) -> String {
    raw.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

/// 删除 `{token?}` 可选 token（TemplateURL 语义：参数可省略则整段移除；
/// 实测 torrents 站 `page={startPage?}`）。模板无嵌套花括号，线性扫描即可。
fn strip_optional_tokens(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out = String::with_capacity(raw.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'{' {
            // 找同层下一个 '}'（'{' 先出现视为异常 token，原样保留交残留检查报错）
            if let Some(end) = raw[i + 1..].find('}').map(|p| i + 1 + p) {
                let token = &raw[i + 1..end];
                let next_brace = raw[i + 1..end].find('{');
                if next_brace.is_none() && token.ends_with('?') {
                    i = end + 1; // 整段（含花括号）丢弃
                    continue;
                }
            }
        }
        // 安全推进：按字符边界逐字符追加（string_slice lint 禁止裸字节切片）
        let ch = raw[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// Chromium TemplateURL → blink `{query}` 模板。
///
/// 转换步骤（顺序敏感）：XML 实体反转义 → `{google:baseURL}` 展开 →
/// `{token?}` 可选 token 删除 → `{inputEncoding}` → `utf-8` →
/// `{searchTerms}` → `{query}`。残留花括号（除 `{query}` 外）即高级 token
/// （Google 默认引擎的 `{google:RLZ}` 等），返回 Err 交预览标注，不静默丢弃。
pub fn convert_template_url(raw: &str) -> Result<String, ImportSkipReason> {
    if raw.trim().is_empty() {
        return Err(ImportSkipReason::NotHttp);
    }
    let converted = unescape_xml_entities(raw)
        .replace("{google:baseURL}", "https://www.google.com/");
    let converted = strip_optional_tokens(&converted)
        .replace("{inputEncoding}", "utf-8")
        .replace("{searchTerms}", "{query}");
    let converted = converted.trim().to_string();
    // 残留 token 检查：{query} 是唯一合法花括号；其余（host 或 query 中的
    // 高级 token）无法保证渲染后可打开。
    let residual = converted.replace("{query}", "");
    if residual.contains('{') || residual.contains('}') {
        return Err(ImportSkipReason::UnsupportedTemplate);
    }
    let url = reqwest::Url::parse(&converted).map_err(|_| ImportSkipReason::UnsupportedTemplate)?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(ImportSkipReason::NotHttp);
    }
    if url.host_str().map_or(true, |h| h.is_empty()) {
        return Err(ImportSkipReason::UnsupportedTemplate);
    }
    Ok(converted)
}

/// 批量转换 + 过滤 + 判重。
///
/// 判重以 keyword（大小写不敏感）为键：既有超链表全量（含禁用条目——开关
/// 切换后即暴露，与保存校验同口径）先入 seen，再逐行检查。行序即候选序
/// （app 层按 usage_count 降序查询，最常用的排前、优先可导入）。
pub fn import_candidates(
    rows: &[KeywordRow],
    existing: &[WebLinkEntry],
    owner_of: &dyn Fn(&str) -> Option<KeywordOwner>,
) -> Vec<ImportCandidate> {
    let mut seen: Vec<String> = existing
        .iter()
        .map(|e| e.keyword.trim().to_lowercase())
        .filter(|k| !k.is_empty())
        .collect();
    rows.iter()
        .map(|row| {
            let keyword = row.keyword.trim();
            let name = {
                let n = row.short_name.trim();
                if n.is_empty() { keyword.to_string() } else { n.to_string() }
            };
            // URL 先行：不可导入的模板与触发词状态无关，预览按模板原因标注。
            let template = match convert_template_url(&row.url) {
                Ok(t) => t,
                Err(reason) => {
                    return ImportCandidate {
                        name,
                        keyword: keyword.to_string(),
                        url_template: row.url.clone(),
                        skip: Some(reason),
                    }
                }
            };
            let invalid = keyword.is_empty()
                || keyword.chars().any(char::is_whitespace)
                || keyword.eq_ignore_ascii_case("ai");
            if invalid {
                return ImportCandidate {
                    name,
                    keyword: keyword.to_string(),
                    url_template: template,
                    skip: Some(ImportSkipReason::InvalidKeyword),
                };
            }
            if owner_of(keyword).is_some() {
                return ImportCandidate {
                    name,
                    keyword: keyword.to_string(),
                    url_template: template,
                    skip: Some(ImportSkipReason::Taken),
                };
            }
            let lower = keyword.to_lowercase();
            if seen.iter().any(|s| *s == lower) {
                return ImportCandidate {
                    name,
                    keyword: keyword.to_string(),
                    url_template: template,
                    skip: Some(ImportSkipReason::Duplicate),
                };
            }
            seen.push(lower);
            ImportCandidate {
                name,
                keyword: keyword.to_string(),
                url_template: template,
                skip: None,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── 本机 Chrome keywords 表真实样本（2026-10 实测） ─────────────────────

    #[test]
    fn converts_baidu_input_encoding() {
        assert_eq!(
            convert_template_url("https://www.baidu.com/#ie={inputEncoding}&wd={searchTerms}"),
            Ok("https://www.baidu.com/#ie=utf-8&wd={query}".to_string())
        );
    }

    #[test]
    fn converts_plain_search_terms() {
        assert_eq!(
            convert_template_url("https://www.bing.com/search?q={searchTerms}"),
            Ok("https://www.bing.com/search?q={query}".to_string())
        );
        assert_eq!(
            convert_template_url("https://linux.do/search?q={searchTerms}"),
            Ok("https://linux.do/search?q={query}".to_string())
        );
    }

    #[test]
    fn strips_optional_and_unescapes_entities() {
        // torrents 站可选分页 token + iStock XML 实体（http 也合法）
        assert_eq!(
            convert_template_url("https://rousi.zip/torrents.php?search={searchTerms}&page={startPage?}"),
            Ok("https://rousi.zip/torrents.php?search={query}&page=".to_string())
        );
        assert_eq!(
            convert_template_url(
                "http://www.istockphoto.com/hk/search/2/image?family=creative&amp;src=opensearch&amp;phrase={searchTerms}"
            ),
            Ok("http://www.istockphoto.com/hk/search/2/image?family=creative&src=opensearch&phrase={query}".to_string())
        );
    }

    #[test]
    fn expands_google_base_url_but_rlz_residual_rejected() {
        // Google 默认引擎：baseURL 可展开，但 {google:RLZ} 等高级 token 残留 → 不可导入
        // （种子表已含 Google，此行跳过无损失）。
        assert_eq!(
            convert_template_url(
                "{google:baseURL}search?q={searchTerms}&{google:RLZ}{google:assistedQueryStats}"
            ),
            Err(ImportSkipReason::UnsupportedTemplate)
        );
    }

    #[test]
    fn rejects_chrome_internal_and_empty() {
        assert_eq!(
            convert_template_url("chrome://bookmarks/?q={searchTerms}"),
            Err(ImportSkipReason::NotHttp)
        );
        assert_eq!(convert_template_url(""), Err(ImportSkipReason::NotHttp));
        assert_eq!(convert_template_url("   "), Err(ImportSkipReason::NotHttp));
    }

    #[test]
    fn direct_url_without_placeholder_is_importable() {
        assert_eq!(
            convert_template_url("https://example.com/portal"),
            Ok("https://example.com/portal".to_string())
        );
    }

    // ── import_candidates：过滤 + 判重 ─────────────────────────────────────

    fn row(name: &str, keyword: &str, url: &str) -> KeywordRow {
        KeywordRow {
            short_name: name.to_string(),
            keyword: keyword.to_string(),
            url: url.to_string(),
        }
    }

    fn existing_entry(keyword: &str) -> WebLinkEntry {
        WebLinkEntry {
            id: format!("e-{keyword}"),
            name: keyword.to_string(),
            keyword: keyword.to_string(),
            home_url: String::new(),
            url_template: "https://existing.test/s?q={query}".to_string(),
            enabled: true,
        }
    }

    #[test]
    fn marks_importable_and_skip_reasons() {
        let rows = [
            row("百度", "baidu.com", "https://www.baidu.com/#ie={inputEncoding}&wd={searchTerms}"),
            row("书签", "@书签", "chrome://bookmarks/?q={searchTerms}"),
            row("Google", "google", "{google:baseURL}search?q={searchTerms}&{google:RLZ}"),
        ];
        let out = import_candidates(&rows, &[], &|_| None);
        assert_eq!(out[0].skip, None);
        assert_eq!(out[0].name, "百度");
        assert_eq!(out[0].url_template, "https://www.baidu.com/#ie=utf-8&wd={query}");
        assert_eq!(out[1].skip, Some(ImportSkipReason::NotHttp));
        assert_eq!(out[2].skip, Some(ImportSkipReason::UnsupportedTemplate));
    }

    #[test]
    fn duplicate_against_table_batch_and_taken() {
        let rows = [
            row("LINUX DO", "linux.do", "https://linux.do/search?q={searchTerms}"),
            row("LINUX DO 2", "LINUX.DO", "https://linux.do/search?q={searchTerms}"),
            row("剪贴板冲突", "clip", "https://clip.test/?q={searchTerms}"),
            row("AI 保留", "ai", "https://ai.test/?q={searchTerms}"),
        ];
        let existing = [existing_entry("linux.do")];
        let out = import_candidates(
            &rows,
            &existing,
            &|kw| (kw == "clip").then(|| KeywordOwner::Engine("clipboard".to_string())),
        );
        assert_eq!(out[0].skip, Some(ImportSkipReason::Duplicate), "表内已存在");
        assert_eq!(
            out[1].skip,
            Some(ImportSkipReason::Duplicate),
            "批内大小写不敏感重复"
        );
        assert_eq!(out[2].skip, Some(ImportSkipReason::Taken), "被 engine 占用");
        assert_eq!(out[3].skip, Some(ImportSkipReason::InvalidKeyword), "保留前缀 ai");
    }

    #[test]
    fn same_url_different_keyword_both_importable() {
        // 实测本机 Gumroad×2：Chrome 允许同 URL 不同 keyword 并存，blink 按 keyword 判重。
        let rows = [
            row("Gumroad", "a.gumroad.com", "https://gumroad.com/discover?query={searchTerms}"),
            row("Gumroad", "b.gumroad.com", "https://gumroad.com/discover?query={searchTerms}"),
        ];
        let out = import_candidates(&rows, &[], &|_| None);
        assert_eq!(out[0].skip, None);
        assert_eq!(out[1].skip, None);
    }

    #[test]
    fn empty_short_name_falls_back_to_keyword() {
        let rows = [row("", "z.ai", "https://chat.z.ai/?q={searchTerms}")];
        let out = import_candidates(&rows, &[], &|_| None);
        assert_eq!(out[0].name, "z.ai");
        assert_eq!(out[0].skip, None);
    }

    #[test]
    fn unsupported_url_skips_before_keyword_checks() {
        // URL 模板不可导入时按模板原因标注（与触发词占用状态无关，预览信息更准确）。
        let rows = [row("内部", "clip", "chrome://tabs/?q={searchTerms}")];
        let out = import_candidates(
            &rows,
            &[],
            &|kw| (kw == "clip").then(|| KeywordOwner::Plugin("p".to_string())),
        );
        assert_eq!(out[0].skip, Some(ImportSkipReason::NotHttp));
    }
}
