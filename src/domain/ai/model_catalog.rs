//! 0.21.21: 内置精简模型目录（构建期嵌入）。
//!
//! 数据源：LiteLLM `model_prices_and_context_window.json` 精选主流条目。
//! 由 `cargo xtask models` 生成 `resources/model_context_windows.json`，
//! 运行时 `include_str!` 嵌入，零文件依赖。
//!
//! **铁则**：用户显式配置值优先；目录用于弹窗预填和运行时缺省识别。

use serde::Deserialize;

/// 目录中的单条模型记录。
#[derive(Debug, Clone, Deserialize)]
struct CatalogEntry {
    /// 模型 id 前缀（如 `gpt-4o-mini`）。
    prefix: String,
    /// 上下文窗口大小。
    context_window: u32,
}

/// 目录 JSON 的顶层结构。
#[derive(Debug, Deserialize)]
struct CatalogFile {
    /// 模型条目列表（已按前缀长度降序排列，确保更长的前缀先匹配）。
    models: Vec<CatalogEntry>,
}

/// 嵌入的精简模型目录 JSON。
const MODEL_CATALOG_JSON: &str = include_str!("../../../resources/model_context_windows.json");

/// 解析后的目录缓存——目录编译期固定，进程内解析一次即可。
static CATALOG: std::sync::OnceLock<Vec<CatalogEntry>> = std::sync::OnceLock::new();

/// 解析嵌入的模型目录。失败时返回空 Vec（静默降级——目录是优化项不是正确性依赖）。
fn parse_catalog() -> &'static [CatalogEntry] {
    CATALOG.get_or_init(|| {
        let catalog: CatalogFile = match serde_json::from_str(MODEL_CATALOG_JSON) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(error = %e, "model_catalog: 内置目录 JSON 解析失败，降级为空");
                return Vec::new();
            }
        };
        // 防御性排序：按前缀长度降序，确保更长的前缀先匹配。
        // JSON 文件已预排序，但此处再做一次保证正确性不依赖文件顺序。
        let mut models = catalog.models;
        models.sort_by_key(|b| std::cmp::Reverse(b.prefix.len()));
        models
    })
}

/// 按 model id 前缀匹配查找推荐的 context_window。
///
/// 前缀冲突取更长者（如 `gpt-4o-mini` 优先于 `gpt-4`）——
/// 目录已按前缀长度降序排列，取第一个匹配即可。
///
/// 返回 `None` 表示无匹配——调用方应 fallback 到档位估算或 32K。
///
/// **铁则**：只在模型未显式配置窗口时使用，绝不覆盖用户值。
pub fn lookup_context_window(model_id: &str) -> Option<u32> {
    // 聚合网关常返回 `provider/model`；目录只维护最后一段的标准模型 id。
    // ASCII 小写匹配兼容用户手填的大小写差异。
    let normalized = model_id
        .rsplit('/')
        .next()
        .unwrap_or(model_id)
        .to_ascii_lowercase();
    parse_catalog()
        .iter()
        .find(|entry| normalized.starts_with(&entry.prefix.to_ascii_lowercase()))
        .map(|entry| entry.context_window)
}

// ── 单测 ───────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lookup_gpt4o_mini_matches_longer_prefix() {
        // gpt-4o-mini 应匹配 gpt-4o-mini 而非 gpt-4o
        let result = lookup_context_window("gpt-4o-mini-2024-07-18");
        assert_eq!(result, Some(128000));
    }

    #[test]
    fn lookup_gpt4o_matches() {
        let result = lookup_context_window("gpt-4o-2024-08-06");
        assert_eq!(result, Some(128000));
    }

    #[test]
    fn lookup_gpt4_matches_base() {
        let result = lookup_context_window("gpt-4-0613");
        assert_eq!(result, Some(8192));
    }

    #[test]
    fn lookup_claude_sonnet_4_matches() {
        let result = lookup_context_window("claude-sonnet-4-20250514");
        assert_eq!(result, Some(200000));
    }

    #[test]
    fn lookup_deepseek_chat_matches() {
        let result = lookup_context_window("deepseek-chat");
        assert_eq!(result, Some(65536));
    }

    #[test]
    fn lookup_glm4_matches() {
        let result = lookup_context_window("glm-4-plus");
        assert_eq!(result, Some(128000));
    }

    #[test]
    fn lookup_unknown_model_returns_none() {
        let result = lookup_context_window("some-unknown-model");
        assert_eq!(result, None);
    }

    #[test]
    fn lookup_empty_id_returns_none() {
        let result = lookup_context_window("");
        assert_eq!(result, None);
    }

    #[test]
    fn lookup_is_case_insensitive() {
        let result = lookup_context_window("GPT-4o");
        assert_eq!(result, Some(128000));
    }

    #[test]
    fn lookup_strips_gateway_provider_prefix() {
        assert_eq!(lookup_context_window("deepseek/deepseek-chat"), Some(65536));
    }

    #[test]
    fn lookup_gemini_2_5_pro_matches() {
        let result = lookup_context_window("gemini-2.5-pro-preview-05-06");
        assert_eq!(result, Some(2000000));
    }

    #[test]
    fn lookup_qwen_plus_matches() {
        let result = lookup_context_window("qwen-plus");
        assert_eq!(result, Some(131072));
    }

    #[test]
    fn catalog_has_entries() {
        let catalog = parse_catalog();
        assert!(!catalog.is_empty(), "内置目录不应为空");
        assert!(
            catalog.len() >= 30,
            "内置目录应至少有 30 条目，实际 {}",
            catalog.len()
        );
    }

    #[test]
    fn catalog_sorted_by_prefix_length_desc() {
        // 验证目录已按前缀长度降序排列——这是 lookup 正确性的前提
        let catalog = parse_catalog();
        for i in 0..catalog.len().saturating_sub(1) {
            assert!(
                catalog[i].prefix.len() >= catalog[i + 1].prefix.len(),
                "目录未按前缀长度降序：[{}] '{}' ({}) < [{}] '{}' ({})",
                i,
                catalog[i].prefix,
                catalog[i].prefix.len(),
                i + 1,
                catalog[i + 1].prefix,
                catalog[i + 1].prefix.len()
            );
        }
    }
}
