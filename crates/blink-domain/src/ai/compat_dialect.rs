//! OpenAICompatible 兼容方言解析（0.27.14）——单一真源。
//!
//! rig 0.45 的 OpenAI 方言体系：`OpenAIConfig` 携带一个 `Dialect`，方言表达
//! 该端点的**兼容协议差异**（请求改写/回传字段/能力位），不改变端点本身。
//! 本仓首期只启用经验证的 DeepSeek/Moonshot 两个厂商方言（§5.14）：
//!
//! - **DeepSeek**（`wire::DEEPSEEK`）：思考中丢弃 forced tool choice
//!   （`finalize_deepseek`）；assistant 消息恒带 `reasoning_content` 回传字段
//!   （空串占位）；文本 content 拍平为字符串；不支持 `response_format`。
//! - **Moonshot**（`wire::MOONSHOT`）：拒绝强制指定单工具（本地 EncodeError）；
//!   `tool_choice=required` 降级为 `auto` 并追加引导消息；不支持 `response_format`。
//!
//! MiniMax/XiaomiMiMo/ZAI 当前主要是端点/身份常量，不作为方言接入目标。
//! DeepSeek `prompt_cache_hit_tokens` 由 rig 共享 `normalized_usage` 在**任何**
//! 方言下 lenient 读取（`Usage::cached_input_tokens`），不是方言独有收益。

use crate::config::ai_config::{CompatDialect, ProviderKind};

/// 解析后的兼容方言——`build_openai_client` 与 thinking 判定共用的最终结论。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolvedCompatDialect {
    /// 通用 OpenAI Chat Completions 路径（rig OPENAI dialect + 显式 Chat 路由）——
    /// 无 rewrite，现状行为。
    Generic,
    /// DeepSeek 方言（rig `wire::DEEPSEEK` quirks）。
    DeepSeek,
    /// Moonshot 方言（rig `wire::MOONSHOT` quirks）。
    Moonshot,
}

impl ResolvedCompatDialect {
    /// fingerprint 紧凑标识（进入 provider cache key，见 registry）。
    pub fn fingerprint_tag(self) -> &'static str {
        match self {
            Self::Generic => "g",
            Self::DeepSeek => "ds",
            Self::Moonshot => "ms",
        }
    }
}

/// 已验证的官方 hostname——Auto 模式精确匹配表（小写）。
/// 只列 rig 0.45 dialect 常量声明的官方端点；区域变体（moonshot.cn/ai）同表。
const DEEPSEEK_HOSTS: [&str; 1] = ["api.deepseek.com"];
const MOONSHOT_HOSTS: [&str; 2] = ["api.moonshot.cn", "api.moonshot.ai"];

/// 解析 base_url 的 host 部分（剥 scheme/userinfo/端口/路径）。
///
/// 手写实现（domain 层不依赖 `url` crate）：仅支持 `scheme://[userinfo@]host[:port]/…`
/// 常见形状；解析失败（无 scheme、格式怪异）返回 `None`，调用方按未知网关处理。
/// 返回小写 host（hostname 大小写不敏感）。
pub(crate) fn url_host(base_url: Option<&str>) -> Option<String> {
    let url = base_url?.trim();
    let (scheme, rest) = url.split_once("://")?;
    // 只认 http/https——其余 scheme（如 socks 占位）不是 AI 端点
    if !matches!(scheme.to_ascii_lowercase().as_str(), "http" | "https") {
        return None;
    }
    // 剥 path/query/fragment
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    // 剥 userinfo
    let host_port = authority.rsplit_once('@').map_or(authority, |(_, hp)| hp);
    // 剥端口（IPv6 字面量带方括号——形如 [::1]:8080）
    let host = if host_port.starts_with('[') {
        host_port
            .split_once(']')
            .map(|(h, _)| &h[1..])
            .unwrap_or(&host_port[1.min(host_port.len())..])
    } else {
        host_port.split(':').next().unwrap_or("")
    };
    let host = host.to_ascii_lowercase();
    (host.contains('.') || host == "localhost" || host.contains("::"))
        .then_some(host)
        .filter(|h| !h.is_empty())
}

/// 解析 provider 的最终兼容方言（单一真源，双调用链共用）。
///
/// - 非 `OpenAICompatible`：恒 `Generic`（方言概念不适用于其他协议）。
/// - `Generic` / 显式厂商：直接采纳用户意图。
/// - `Auto`：按 [`url_host`] 精确匹配官方 hostname 表；URL 解析失败、
///   host 未知、base_url 缺失 → `Generic`。**不做子串匹配**——
///   `https://api.deepseek.com.evil.com` 或 `https://my-deepseek-proxy.example`
///   一律通用路径，不给未知网关套方言 rewrite。
pub(crate) fn resolve_compat_dialect(
    kind: ProviderKind,
    setting: CompatDialect,
    base_url: Option<&str>,
) -> ResolvedCompatDialect {
    if kind != ProviderKind::OpenAICompatible {
        return ResolvedCompatDialect::Generic;
    }
    match setting {
        CompatDialect::Generic => ResolvedCompatDialect::Generic,
        CompatDialect::DeepSeek => ResolvedCompatDialect::DeepSeek,
        CompatDialect::Moonshot => ResolvedCompatDialect::Moonshot,
        CompatDialect::Auto => {
            let Some(host) = url_host(base_url) else {
                return ResolvedCompatDialect::Generic;
            };
            if DEEPSEEK_HOSTS.contains(&host.as_str()) {
                ResolvedCompatDialect::DeepSeek
            } else if MOONSHOT_HOSTS.contains(&host.as_str()) {
                ResolvedCompatDialect::Moonshot
            } else {
                ResolvedCompatDialect::Generic
            }
        }
    }
}

/// 便捷入口——直接从 `ProviderEntry` 解析（bin 侧 commands 与 domain 内调用方共用）。
pub fn resolve_for_entry(entry: &crate::config::ai_config::ProviderEntry) -> ResolvedCompatDialect {
    resolve_compat_dialect(entry.kind, entry.compat_dialect, entry.base_url.as_deref())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolved(setting: CompatDialect, base_url: Option<&str>) -> ResolvedCompatDialect {
        resolve_compat_dialect(ProviderKind::OpenAICompatible, setting, base_url)
    }

    #[test]
    fn auto_matches_official_deepseek_hostnames_exactly() {
        assert_eq!(
            resolved(CompatDialect::Auto, Some("https://api.deepseek.com/v1")),
            ResolvedCompatDialect::DeepSeek
        );
        assert_eq!(
            resolved(CompatDialect::Auto, Some("https://api.deepseek.com")),
            ResolvedCompatDialect::DeepSeek
        );
        // 带端口/尾斜杠/query 均能剥出 host
        assert_eq!(
            resolved(CompatDialect::Auto, Some("http://api.deepseek.com:8443/v1/?x=1")),
            ResolvedCompatDialect::DeepSeek
        );
    }

    #[test]
    fn auto_matches_official_moonshot_hostnames_both_regions() {
        for url in ["https://api.moonshot.cn/v1", "https://api.moonshot.ai/v1"] {
            assert_eq!(
                resolved(CompatDialect::Auto, Some(url)),
                ResolvedCompatDialect::Moonshot,
                "官方端点 {url} 应精确命中 Moonshot"
            );
        }
    }

    #[test]
    fn auto_never_matches_by_substring_or_model_name() {
        // 铁则：不用 URL 子串、显示名或模型名给未知网关套方言
        for url in [
            "https://api.deepseek.com.evil.com/v1",
            "https://deepseek-proxy.example.com/v1",
            "https://api.moonshot.cn.mirror.io/v1",
            "https://my-gateway.local/v1",
        ] {
            assert_eq!(
                resolved(CompatDialect::Auto, Some(url)),
                ResolvedCompatDialect::Generic,
                "未知网关 {url} 必须走通用路径"
            );
        }
        // base_url 缺失 / 非 http scheme / 解析失败
        assert_eq!(resolved(CompatDialect::Auto, None), ResolvedCompatDialect::Generic);
        assert_eq!(
            resolved(CompatDialect::Auto, Some("ftp://api.deepseek.com")),
            ResolvedCompatDialect::Generic
        );
        assert_eq!(
            resolved(CompatDialect::Auto, Some("not-a-url")),
            ResolvedCompatDialect::Generic
        );
    }

    #[test]
    fn explicit_overrides_win_and_generic_disables_rewrite() {
        // 自建代理显式覆盖——即便 host 非官方
        assert_eq!(
            resolved(CompatDialect::DeepSeek, Some("https://proxy.mycompany.com/v1")),
            ResolvedCompatDialect::DeepSeek
        );
        // 显式通用——即便 host 命中官方端点也不套方言
        assert_eq!(
            resolved(CompatDialect::Generic, Some("https://api.deepseek.com/v1")),
            ResolvedCompatDialect::Generic
        );
    }

    #[test]
    fn non_openai_compatible_kinds_are_always_generic() {
        for kind in [
            ProviderKind::AnthropicMessages,
            ProviderKind::GeminiGenerateContent,
            ProviderKind::OllamaHttp,
        ] {
            assert_eq!(
                resolve_compat_dialect(kind, CompatDialect::DeepSeek, Some("https://api.deepseek.com")),
                ResolvedCompatDialect::Generic,
                "方言仅对 OpenAICompatible 生效"
            );
        }
    }

    #[test]
    fn url_host_extracts_host_from_common_shapes() {
        assert_eq!(
            url_host(Some("https://api.openai.com/v1")).as_deref(),
            Some("api.openai.com")
        );
        // userinfo + 端口
        assert_eq!(
            url_host(Some("http://user:pass@Host.Example.com:8080/path")).as_deref(),
            Some("host.example.com")
        );
        // IPv6 loopback（带方括号）
        assert_eq!(
            url_host(Some("http://[::1]:11434/api")).as_deref(),
            Some("::1")
        );
        assert_eq!(url_host(Some("http://localhost:11434")).as_deref(), Some("localhost"));
        assert_eq!(url_host(Some("::1")).as_deref(), None, "无 scheme 不解析");
        assert_eq!(url_host(None), None);
    }

    #[test]
    fn fingerprint_tags_are_distinct_and_compact() {
        let tags = [
            ResolvedCompatDialect::Generic.fingerprint_tag(),
            ResolvedCompatDialect::DeepSeek.fingerprint_tag(),
            ResolvedCompatDialect::Moonshot.fingerprint_tag(),
        ];
        for i in 0..tags.len() {
            for j in i + 1..tags.len() {
                assert_ne!(tags[i], tags[j], "fingerprint tag 必须互异");
            }
        }
    }
}
