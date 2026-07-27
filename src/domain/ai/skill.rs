//! Skill 约定式（0.13.3）——SKILL.md 目录发现 + preamble 注入。
//!
//! Skill = 约定式目录包（SKILL.md + 可选资源），通过注入 preamble 增强 AI 的知识与行为。
//! 不直接执行，AI 读了之后用现有 tool/知识去执行。
//!
//! **核心区分**：Skill 注入 preamble（教 AI 怎么做），Tool 进 tool 池（让 AI 能做什么）。
//!
//! **目录发现**：扫描多个 agent 的 skill 目录（合并不去重，标注来源）：
//! - Blink 自身：`%APPDATA%\blink\skills\`
//! - Claude Code：`~/.claude/skills/`（`%USERPROFILE%\.claude\skills\`）
//! - ZCode：`~/.zcode/skills/`（`%USERPROFILE%\.zcode\skills\`）
//!
//! **渐进式披露**（控制 token 成本）：
//! - 阶段 1（常驻）：所有 Skill 的 `name + description` 摘要注入每个 preamble
//! - 阶段 2（按需）：命中触发条件的 Skill 的完整 SKILL.md 注入 preamble
//!
//! **触发判定**（阶段 1 → 阶段 2）：
//! - 关键词匹配：用户消息含 `triggers.keywords` 任一关键词
//! - 正则匹配：用户消息匹配 `triggers.patterns`
//! - 显式调用：`/skill <name>` 指令
//!
//! **SKILL.md 格式**（YAML frontmatter + Markdown body）：
//! ```markdown
//! ---
//! name: rust-debug-workflow
//! description: 调试 Rust 编译错误的标准流程
//! triggers:                          # Blink 可选扩展字段
//!   keywords: [cargo, rustc, compile error, E0xxx]
//!   patterns: ["error\\[E\\d+\\]"]
//! priority: normal                   # Blink 可选扩展字段
//! ---
//! # Rust 编译错误调试流程
//! ...
//! ```

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::RwLock;

use regex::Regex;

// ── SkillSource ──────────────────────────────────────────────────────────────

/// Skill 来源——标识 SKILL.md 来自哪个 agent 的目录。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SkillSource {
    /// Blink 自身目录 `%APPDATA%\blink\skills\`
    Blink,
    /// Claude Code 目录 `~/.claude/skills/`
    Claude,
    /// ZCode 目录 `~/.zcode/skills/`
    Zcode,
}

impl SkillSource {
    /// 返回来源的显示名（注入 preamble 的 `[source]` 标注用）。
    pub fn display_name(&self) -> &'static str {
        match self {
            Self::Blink => "blink",
            Self::Claude => "claude",
            Self::Zcode => "zcode",
        }
    }

    /// 返回该来源的 skill 目录路径（Windows 下 `%APPDATA%` / `%USERPROFILE%` 展开）。
    ///
    /// 目录不存在时仍返回路径（调用方自行 `exists()` 判断）。
    pub fn directory(&self) -> Option<PathBuf> {
        match self {
            Self::Blink => dirs_next::data_dir().map(|d| d.join("blink").join("skills")),
            Self::Claude => dirs_next::home_dir().map(|h| h.join(".claude").join("skills")),
            Self::Zcode => dirs_next::home_dir().map(|h| h.join(".zcode").join("skills")),
        }
    }

    /// 所有来源（按优先级排序：Blink → Claude → ZCode）。
    #[allow(dead_code)] // 供未来扩展使用
    pub fn all() -> &'static [SkillSource] {
        &[Self::Blink, Self::Claude, Self::Zcode]
    }
}

// ── SkillTriggers ────────────────────────────────────────────────────────────

/// Skill 触发条件（Blink 扩展字段，可选）。
///
/// 缺失 `triggers` 字段的 Skill 只能靠 `/skill` 显式激活。
#[derive(Debug, Clone, serde::Serialize)]
pub struct SkillTriggers {
    /// 关键词列表——用户消息含任一关键词即触发（不区分大小写）。
    pub keywords: Vec<String>,
    /// 正则模式列表——用户消息匹配任一模式即触发。
    pub patterns: Vec<String>,
}

// ── SkillSummary / SkillEntry ────────────────────────────────────────────────

/// Skill 摘要——阶段 1 注入 preamble 的精简信息（~50 token/skill）。
#[derive(Debug, Clone, serde::Serialize)]
pub struct SkillSummary {
    pub name: String,
    pub description: String,
    pub source: SkillSource,
    pub has_triggers: bool,
}

/// 一个已发现的 Skill 条目。
#[derive(Debug, Clone, serde::Serialize)]
pub struct SkillEntry {
    /// Skill 名称（frontmatter `name` 字段，必填）。
    pub name: String,
    /// 一句话描述（frontmatter `description` 字段，必填）。
    pub description: String,
    /// Blink 扩展字段，可选。缺失时只能靠 `/skill` 显式激活。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub triggers: Option<SkillTriggers>,
    /// SKILL.md body（frontmatter 之后的 Markdown 全文）。
    #[serde(skip)]
    pub full_content: String,
    /// 来源目录。
    pub source: SkillSource,
    /// SKILL.md 所在目录路径。
    #[allow(dead_code)] // 供未来扩展（如打开 skill 文件编辑）
    #[serde(skip)]
    pub dir_path: PathBuf,
}

// ── SkillRegistry ────────────────────────────────────────────────────────────

/// Skill 注册表——内存结构，启动时扫描，可手动刷新。
///
/// 不进 DB——SKILL.md 是文件系统约定（各 agent 共享），进 DB 反而破坏"复用生态"语义。
pub struct SkillRegistry {
    skills: RwLock<Vec<SkillEntry>>,
}

impl SkillRegistry {
    pub fn new() -> Self {
        Self {
            skills: RwLock::new(Vec::new()),
        }
    }

    /// 扫描所有启用的来源目录，解析 SKILL.md。
    ///
    /// 合并不去重——不同 agent 的同名 Skill 各自保留，标注来源。
    /// 扫描完成后替换内存中的 Skill 列表。
    pub fn scan(&self, enabled_sources: &[SkillSource]) {
        let mut entries = Vec::new();

        for &source in enabled_sources {
            let Some(dir) = source.directory() else {
                tracing::debug!(
                    source = source.display_name(),
                    "Skill 目录路径无法解析（dirs-next 返回 None），跳过"
                );
                continue;
            };
            if !dir.exists() {
                tracing::debug!(
                    source = source.display_name(),
                    dir = %dir.display(),
                    "Skill 目录不存在，跳过"
                );
                continue;
            }
            match scan_directory(&dir, source) {
                Ok(found) => {
                    tracing::info!(
                        source = source.display_name(),
                        count = found.len(),
                        dir = %dir.display(),
                        "Skill 扫描完成"
                    );
                    entries.extend(found);
                }
                Err(e) => {
                    tracing::warn!(
                        source = source.display_name(),
                        dir = %dir.display(),
                        %e,
                        "Skill 目录扫描失败"
                    );
                }
            }
        }

        let total = entries.len();
        *self
            .skills
            .write()
            .expect("skill registry lock poisoned") = entries;
        tracing::info!(total, "SkillRegistry: 扫描完成");
    }

    /// 返回所有已发现的 Skill 摘要（阶段 1 preamble 注入用）。
    pub fn summaries(&self) -> Vec<SkillSummary> {
        self.skills
            .read()
            .expect("skill registry lock poisoned")
            .iter()
            .map(|s| SkillSummary {
                name: s.name.clone(),
                description: s.description.clone(),
                source: s.source,
                has_triggers: s.triggers.is_some(),
            })
            .collect()
    }

    /// 返回所有已发现的 Skill 条目（设置页展示用）。
    pub fn all(&self) -> Vec<SkillEntry> {
        self.skills
            .read()
            .expect("skill registry lock poisoned")
            .clone()
    }

    /// 根据用户消息匹配触发的 Skill（阶段 2 preamble 注入用）。
    ///
    /// 匹配规则：
    /// - 关键词匹配：用户消息含 `triggers.keywords` 任一关键词（不区分大小写）
    /// - 正则匹配：用户消息匹配 `triggers.patterns`
    pub fn match_triggers(&self, message: &str) -> Vec<SkillEntry> {
        let skills = self
            .skills
            .read()
            .expect("skill registry lock poisoned");
        let msg_lower = message.to_lowercase();

        skills
            .iter()
            .filter(|s| {
                let Some(triggers) = &s.triggers else {
                    return false;
                };
                // 关键词匹配
                if triggers
                    .keywords
                    .iter()
                    .any(|kw| msg_lower.contains(&kw.to_lowercase()))
                {
                    return true;
                }
                // 正则匹配
                triggers.patterns.iter().any(|pat| {
                    match Regex::new(pat) {
                        Ok(re) => re.is_match(message),
                        Err(e) => {
                            tracing::warn!(pattern = %pat, %e, "Skill 正则编译失败，跳过");
                            false
                        }
                    }
                })
            })
            .cloned()
            .collect()
    }

    /// 按名称查找 Skill（`/skill` 显式激活用）。
    ///
    /// `source_filter` 用于同名消歧（如 `rust-debug-workflow@claude`）。
    pub fn find_by_name(
        &self,
        name: &str,
        source_filter: Option<SkillSource>,
    ) -> Option<SkillEntry> {
        self.skills
            .read()
            .expect("skill registry lock poisoned")
            .iter()
            .find(|s| s.name == name && source_filter.map_or(true, |src| s.source == src))
            .cloned()
    }

    /// 已发现的 Skill 数量。
    pub fn count(&self) -> usize {
        self.skills
            .read()
            .expect("skill registry lock poisoned")
            .len()
    }
}

impl Default for SkillRegistry {
    fn default() -> Self {
        Self::new()
    }
}

// ── 目录扫描 ──────────────────────────────────────────────────────────────────

/// 扫描单个目录下的所有 skill 子目录。
///
/// 每个子目录含 `SKILL.md` 即视为一个 Skill。非目录项和缺 `SKILL.md` 的子目录跳过。
fn scan_directory(dir: &Path, source: SkillSource) -> std::io::Result<Vec<SkillEntry>> {
    let mut entries = Vec::new();

    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();

        // 只扫描子目录（每个子目录是一个 skill）
        if !path.is_dir() {
            continue;
        }

        let skill_md = path.join("SKILL.md");
        if !skill_md.exists() {
            continue;
        }

        match std::fs::read_to_string(&skill_md) {
            Ok(content) => match parse_skill_md(&content, source, path.clone()) {
                Some(skill) => entries.push(skill),
                None => {
                    tracing::warn!(
                        path = %skill_md.display(),
                        "SKILL.md 解析失败（缺 name 或 description），跳过"
                    );
                }
            },
            Err(e) => {
                tracing::warn!(path = %skill_md.display(), %e, "读取 SKILL.md 失败");
            }
        }
    }

    Ok(entries)
}

// ── SKILL.md 解析 ────────────────────────────────────────────────────────────

/// 解析 SKILL.md 内容（YAML frontmatter + Markdown body）。
///
/// 返回 `None` 表示缺少必填字段（name 或 description）。
fn parse_skill_md(content: &str, source: SkillSource, dir_path: PathBuf) -> Option<SkillEntry> {
    let (frontmatter, body) = split_frontmatter(content);

    let name = frontmatter.get("name")?.trim().to_string();
    let description = frontmatter.get("description")?.trim().to_string();

    if name.is_empty() || description.is_empty() {
        return None;
    }

    let triggers = parse_triggers(&frontmatter);

    Some(SkillEntry {
        name,
        description,
        triggers,
        full_content: body,
        source,
        dir_path,
    })
}

/// 分离 YAML frontmatter 和 Markdown body。
///
/// 格式：
/// ```text
/// ---
/// key: value
/// ---
/// # Markdown content
/// ```
///
/// 无 frontmatter 时返回空 map + 全文作为 body。
fn split_frontmatter(content: &str) -> (HashMap<String, String>, String) {
    let mut map = HashMap::new();
    let trimmed = content.trim_start();

    if !trimmed.starts_with("---") {
        return (map, content.to_string());
    }

    // 跳过开头的 ---
    let rest = &trimmed[3..];
    // 跳过 --- 后的换行
    let rest = rest.strip_prefix(['\r', '\n']).unwrap_or(rest);

    // 找闭合的 ---（行首）
    if let Some(end) = find_closing_delimiter(rest) {
        let yaml_block = &rest[..end];
        // body 从闭合 --- 之后开始
        let after_delim = &rest[end..];
        // 跳过 --- 本身
        let after_delim = after_delim.strip_prefix("---").unwrap_or(after_delim);
        let body = after_delim.trim_start_matches(['\r', '\n']).to_string();

        parse_simple_yaml(yaml_block, &mut map);
        (map, body)
    } else {
        // 没有闭合 ---，当作无 frontmatter
        (map, content.to_string())
    }
}

/// 找到闭合 `---` 的位置（行首）。
///
/// 在 frontmatter 内部，`---` 必须在行首才被视为闭合分隔符。
fn find_closing_delimiter(s: &str) -> Option<usize> {
    for (idx, line) in s.match_indices('\n') {
        let _ = line; // line text
        // 检查这一行之后是否以 --- 开头
        let after_newline = &s[idx + 1..];
        if after_newline.starts_with("---") {
            // 确认 --- 后面是行尾或换行（不是 ---something）
            let after_dashes = &after_newline[3..];
            if after_dashes.is_empty()
                || after_dashes.starts_with('\n')
                || after_dashes.starts_with('\r')
            {
                return Some(idx + 1);
            }
        }
    }
    // 也检查第一行（rest 本身以 --- 开头的情况）
    if s.starts_with("---") {
        let after_dashes = &s[3..];
        if after_dashes.is_empty()
            || after_dashes.starts_with('\n')
            || after_dashes.starts_with('\r')
        {
            return Some(0);
        }
    }
    None
}

/// 简单 YAML 解析——只处理 `key: value` 和 `key: [a, b, c]` 格式。
///
/// **不依赖 serde_yaml**——SKILL.md frontmatter 字段简单，手写解析器足够，
/// 且避免引入 ~500KB 依赖。嵌套的 `triggers` 块用 `triggers.<subkey>` 前缀平铺。
fn parse_simple_yaml(yaml: &str, map: &mut HashMap<String, String>) {
    let mut in_triggers = false;

    for line in yaml.lines() {
        let trimmed = line.trim();

        // 空行和注释跳过
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }

        // 检测 triggers 块开始
        if trimmed == "triggers:" {
            in_triggers = true;
            continue;
        }

        // 新的一级 key 出现（不以空格/制表符开头），退出 triggers 块
        if !line.starts_with(' ') && !line.starts_with('\t') {
            in_triggers = false;
        }

        // 解析 key: value
        if let Some((key, value)) = trimmed.split_once(':') {
            let key = key.trim();
            let value = value.trim();

            if key.is_empty() {
                continue;
            }

            let full_key = if in_triggers {
                format!("triggers.{key}")
            } else {
                key.to_string()
            };

            let cleaned = clean_yaml_value(value);
            if !cleaned.is_empty() {
                map.insert(full_key, cleaned);
            }
        }
    }
}

/// 清理 YAML 值——去掉首尾引号。
fn clean_yaml_value(value: &str) -> String {
    let v = value.trim();
    // 去掉双引号或单引号
    if v.len() >= 2 {
        let first = v.chars().next().unwrap();
        let last = v.chars().last().unwrap();
        if (first == '"' && last == '"') || (first == '\'' && last == '\'') {
            return v[1..v.len() - 1].to_string();
        }
    }
    v.to_string()
}

/// 从 frontmatter 解析 triggers 结构。
fn parse_triggers(map: &HashMap<String, String>) -> Option<SkillTriggers> {
    let keywords_str = map.get("triggers.keywords")?;
    let patterns_str = map.get("triggers.patterns").map(String::as_str).unwrap_or("");

    let keywords = parse_yaml_list(keywords_str);
    let patterns = parse_yaml_list(patterns_str);

    if keywords.is_empty() && patterns.is_empty() {
        return None;
    }

    Some(SkillTriggers { keywords, patterns })
}

/// 解析 YAML 列表值——支持 `[a, b, c]` 内联格式。
fn parse_yaml_list(value: &str) -> Vec<String> {
    let v = value.trim();
    if v.is_empty() {
        return Vec::new();
    }

    // [a, b, c] 内联格式
    if v.starts_with('[') && v.ends_with(']') {
        let inner = &v[1..v.len() - 1];
        inner
            .split(',')
            .map(|s| clean_yaml_value(s))
            .filter(|s| !s.is_empty())
            .collect()
    } else {
        // 单个值
        let cleaned = clean_yaml_value(v);
        if cleaned.is_empty() {
            Vec::new()
        } else {
            vec![cleaned]
        }
    }
}

// ── /skill 指令解析 ──────────────────────────────────────────────────────────

/// `/skill` 指令解析结果。
#[derive(Debug, Clone, PartialEq)]
pub struct SkillCommand {
    /// Skill 名称。
    pub name: String,
    /// 来源消歧（`@claude` 后缀），None 表示不限来源。
    pub source: Option<SkillSource>,
    /// 去掉 `/skill xxx` 后的剩余消息（可能是空字符串）。
    pub remaining_message: String,
}

/// 解析 `/skill` 指令。
///
/// 支持格式：
/// - `/skill rust-debug-workflow` — 按名称激活
/// - `/skill rust-debug-workflow@claude` — 带来源消歧
/// - `/skill rust-debug-workflow 帮我调试这个错误` — 激活后附带消息
///
/// 返回 `None` 表示不是 `/skill` 指令。
pub fn parse_skill_command(message: &str) -> Option<SkillCommand> {
    let trimmed = message.trim();
    if !trimmed.to_lowercase().starts_with("/skill ") {
        return None;
    }

    // 去掉 /skill 前缀
    let rest = trimmed[7..].trim();
    if rest.is_empty() {
        return None;
    }

    // 分离 skill 名称和剩余消息（第一个空格分隔）
    let (name_part, remaining) = match rest.split_once(char::is_whitespace) {
        Some((name, msg)) => (name, msg.trim().to_string()),
        None => (rest, String::new()),
    };

    // 解析 @source 消歧后缀
    let (name, source) = if let Some(at_pos) = name_part.find('@') {
        let name = name_part[..at_pos].to_string();
        let source_str = &name_part[at_pos + 1..];
        let source = match source_str.to_lowercase().as_str() {
            "blink" => Some(SkillSource::Blink),
            "claude" => Some(SkillSource::Claude),
            "zcode" => Some(SkillSource::Zcode),
            _ => {
                tracing::warn!(source = %source_str, "未知的 skill 来源后缀，忽略");
                None
            }
        };
        (name, source)
    } else {
        (name_part.to_string(), None)
    };

    if name.is_empty() {
        return None;
    }

    Some(SkillCommand {
        name,
        source,
        remaining_message: remaining,
    })
}

// ── 单测 ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── split_frontmatter ──

    #[test]
    fn split_frontmatter_standard() {
        let content = "---\nname: test\ndescription: A test skill\n---\n# Test\nBody text";
        let (map, body) = split_frontmatter(content);
        assert_eq!(map.get("name").unwrap(), "test");
        assert_eq!(map.get("description").unwrap(), "A test skill");
        assert!(body.starts_with("# Test"));
    }

    #[test]
    fn split_frontmatter_no_frontmatter() {
        let content = "# Just markdown\nNo frontmatter";
        let (map, body) = split_frontmatter(content);
        assert!(map.is_empty());
        assert_eq!(body, content);
    }

    #[test]
    fn split_frontmatter_with_triggers() {
        // patterns 使用简单值（非引号），验证 triggers 块正确解析
        let content = "---\nname: rust-debug\ndescription: Debug Rust\ntriggers:\n  keywords: [cargo, rustc]\n  patterns: error\\[E\\d+\\]\n---\n# Body";
        let (map, body) = split_frontmatter(content);
        assert_eq!(map.get("name").unwrap(), "rust-debug");
        assert_eq!(map.get("triggers.keywords").unwrap(), "[cargo, rustc]");
        assert_eq!(map.get("triggers.patterns").unwrap(), r"error\[E\d+\]");
        assert!(body.starts_with("# Body"));
    }

    #[test]
    fn split_frontmatter_crlf_line_endings() {
        let content = "---\r\nname: test\r\ndescription: desc\r\n---\r\n# Body";
        let (map, body) = split_frontmatter(content);
        assert_eq!(map.get("name").unwrap(), "test");
        assert_eq!(map.get("description").unwrap(), "desc");
        assert!(body.starts_with("# Body"));
    }

    // ── parse_simple_yaml ──

    #[test]
    fn parse_yaml_simple_key_value() {
        let mut map = HashMap::new();
        parse_simple_yaml("name: test\ndescription: hello", &mut map);
        assert_eq!(map.get("name").unwrap(), "test");
        assert_eq!(map.get("description").unwrap(), "hello");
    }

    #[test]
    fn parse_yaml_quoted_values() {
        let mut map = HashMap::new();
        parse_simple_yaml(r#"name: "test skill""#, &mut map);
        assert_eq!(map.get("name").unwrap(), "test skill");
    }

    #[test]
    fn parse_yaml_triggers_block() {
        let mut map = HashMap::new();
        parse_simple_yaml(
            "name: test\ntriggers:\n  keywords: [a, b, c]\n  patterns: x.*",
            &mut map,
        );
        assert_eq!(map.get("triggers.keywords").unwrap(), "[a, b, c]");
        assert_eq!(map.get("triggers.patterns").unwrap(), "x.*");
    }

    // ── parse_yaml_list ──

    #[test]
    fn parse_list_inline() {
        let list = parse_yaml_list("[cargo, rustc, compile error]");
        assert_eq!(list, vec!["cargo", "rustc", "compile error"]);
    }

    #[test]
    fn parse_list_single_value() {
        let list = parse_yaml_list("cargo");
        assert_eq!(list, vec!["cargo"]);
    }

    #[test]
    fn parse_list_empty() {
        assert!(parse_yaml_list("").is_empty());
        assert!(parse_yaml_list("[]").is_empty());
    }

    #[test]
    fn parse_list_quoted_items() {
        let list = parse_yaml_list(r#"["error\[E\d+\]", "warning.*"]"#);
        assert_eq!(list.len(), 2);
        assert_eq!(list[0], r"error\[E\d+\]");
        assert_eq!(list[1], "warning.*");
    }

    // ── parse_triggers ──

    #[test]
    fn parse_triggers_both_keywords_and_patterns() {
        let mut map = HashMap::new();
        map.insert("triggers.keywords".to_string(), "[cargo, rustc]".to_string());
        map.insert("triggers.patterns".to_string(), r"error\[\d+\]".to_string());
        let triggers = parse_triggers(&map).unwrap();
        assert_eq!(triggers.keywords, vec!["cargo", "rustc"]);
        assert_eq!(triggers.patterns, vec![r"error\[\d+\]"]);
    }

    #[test]
    fn parse_triggers_only_keywords() {
        let mut map = HashMap::new();
        map.insert("triggers.keywords".to_string(), "[cargo]".to_string());
        let triggers = parse_triggers(&map).unwrap();
        assert_eq!(triggers.keywords, vec!["cargo"]);
        assert!(triggers.patterns.is_empty());
    }

    #[test]
    fn parse_triggers_none_when_no_triggers_key() {
        let map = HashMap::new();
        assert!(parse_triggers(&map).is_none());
    }

    #[test]
    fn parse_triggers_none_when_both_empty() {
        let mut map = HashMap::new();
        map.insert("triggers.keywords".to_string(), "[]".to_string());
        assert!(parse_triggers(&map).is_none());
    }

    // ── parse_skill_md ──

    #[test]
    fn parse_skill_md_full() {
        let content = "---\nname: rust-debug\ndescription: Debug Rust errors\ntriggers:\n  keywords: [cargo, E0]\n---\n# Rust Debug\n\nStep 1: Read error";
        let skill = parse_skill_md(content, SkillSource::Blink, PathBuf::from("/tmp/skill"))
            .expect("should parse");
        assert_eq!(skill.name, "rust-debug");
        assert_eq!(skill.description, "Debug Rust errors");
        assert!(skill.triggers.is_some());
        assert_eq!(skill.triggers.as_ref().unwrap().keywords, vec!["cargo", "E0"]);
        assert!(skill.full_content.contains("Step 1"));
        assert_eq!(skill.source, SkillSource::Blink);
    }

    #[test]
    fn parse_skill_md_no_triggers() {
        let content = "---\nname: simple\ndescription: A simple skill\n---\n# Simple\nBody";
        let skill = parse_skill_md(content, SkillSource::Claude, PathBuf::from("/tmp/s"))
            .expect("should parse");
        assert_eq!(skill.name, "simple");
        assert!(skill.triggers.is_none());
    }

    #[test]
    fn parse_skill_md_missing_name_returns_none() {
        let content = "---\ndescription: No name\n---\nBody";
        assert!(parse_skill_md(content, SkillSource::Blink, PathBuf::from("/tmp")).is_none());
    }

    #[test]
    fn parse_skill_md_missing_description_returns_none() {
        let content = "---\nname: test\n---\nBody";
        assert!(parse_skill_md(content, SkillSource::Blink, PathBuf::from("/tmp")).is_none());
    }

    #[test]
    fn parse_skill_md_no_frontmatter_returns_none() {
        // 无 frontmatter → 无 name/description → None
        let content = "# Just a markdown\nNo frontmatter";
        assert!(parse_skill_md(content, SkillSource::Blink, PathBuf::from("/tmp")).is_none());
    }

    // ── SkillRegistry::match_triggers ──

    #[test]
    fn registry_match_keyword_trigger() {
        let registry = SkillRegistry::new();
        let skill = SkillEntry {
            name: "rust-debug".to_string(),
            description: "Debug Rust".to_string(),
            triggers: Some(SkillTriggers {
                keywords: vec!["cargo".to_string(), "rustc".to_string()],
                patterns: vec![],
            }),
            full_content: "# Rust Debug".to_string(),
            source: SkillSource::Blink,
            dir_path: PathBuf::from("/tmp"),
        };
        *registry.skills.write().unwrap() = vec![skill];

        let matched = registry.match_triggers("cargo build failed");
        assert_eq!(matched.len(), 1);
        assert_eq!(matched[0].name, "rust-debug");
    }

    #[test]
    fn registry_match_keyword_case_insensitive() {
        let registry = SkillRegistry::new();
        *registry.skills.write().unwrap() = vec![SkillEntry {
            name: "test".to_string(),
            description: "test".to_string(),
            triggers: Some(SkillTriggers {
                keywords: vec!["Cargo".to_string()],
                patterns: vec![],
            }),
            full_content: String::new(),
            source: SkillSource::Blink,
            dir_path: PathBuf::from("/tmp"),
        }];

        let matched = registry.match_triggers("running cargo build");
        assert_eq!(matched.len(), 1);
    }

    #[test]
    fn registry_match_pattern_trigger() {
        let registry = SkillRegistry::new();
        *registry.skills.write().unwrap() = vec![SkillEntry {
            name: "error-handler".to_string(),
            description: "Handle errors".to_string(),
            triggers: Some(SkillTriggers {
                keywords: vec![],
                patterns: vec![r"error\[E\d+\]".to_string()],
            }),
            full_content: String::new(),
            source: SkillSource::Claude,
            dir_path: PathBuf::from("/tmp"),
        }];

        let matched = registry.match_triggers("error[E0308]: mismatched types");
        assert_eq!(matched.len(), 1);

        let no_match = registry.match_triggers("no errors here");
        assert!(no_match.is_empty());
    }

    #[test]
    fn registry_no_triggers_no_match() {
        let registry = SkillRegistry::new();
        *registry.skills.write().unwrap() = vec![SkillEntry {
            name: "manual-only".to_string(),
            description: "Manual activation only".to_string(),
            triggers: None,
            full_content: String::new(),
            source: SkillSource::Blink,
            dir_path: PathBuf::from("/tmp"),
        }];

        let matched = registry.match_triggers("anything");
        assert!(matched.is_empty());
    }

    #[test]
    fn registry_multiple_matches() {
        let registry = SkillRegistry::new();
        *registry.skills.write().unwrap() = vec![
            SkillEntry {
                name: "skill-a".to_string(),
                description: "A".to_string(),
                triggers: Some(SkillTriggers {
                    keywords: vec!["cargo".to_string()],
                    patterns: vec![],
                }),
                full_content: String::new(),
                source: SkillSource::Blink,
                dir_path: PathBuf::from("/tmp"),
            },
            SkillEntry {
                name: "skill-b".to_string(),
                description: "B".to_string(),
                triggers: Some(SkillTriggers {
                    keywords: vec![],
                    patterns: vec![r"error\[E".to_string()],
                }),
                full_content: String::new(),
                source: SkillSource::Claude,
                dir_path: PathBuf::from("/tmp"),
            },
        ];

        // 消息同时命中两个 skill
        let matched = registry.match_triggers("cargo error[E0308]");
        assert_eq!(matched.len(), 2);
    }

    // ── SkillRegistry::find_by_name ──

    #[test]
    fn find_by_name_exact() {
        let registry = SkillRegistry::new();
        *registry.skills.write().unwrap() = vec![SkillEntry {
            name: "rust-debug".to_string(),
            description: "Debug".to_string(),
            triggers: None,
            full_content: String::new(),
            source: SkillSource::Blink,
            dir_path: PathBuf::from("/tmp"),
        }];

        assert!(registry.find_by_name("rust-debug", None).is_some());
        assert!(registry.find_by_name("other", None).is_none());
    }

    #[test]
    fn find_by_name_with_source_filter() {
        let registry = SkillRegistry::new();
        *registry.skills.write().unwrap() = vec![
            SkillEntry {
                name: "same-name".to_string(),
                description: "Blink version".to_string(),
                triggers: None,
                full_content: String::new(),
                source: SkillSource::Blink,
                dir_path: PathBuf::from("/tmp"),
            },
            SkillEntry {
                name: "same-name".to_string(),
                description: "Claude version".to_string(),
                triggers: None,
                full_content: String::new(),
                source: SkillSource::Claude,
                dir_path: PathBuf::from("/tmp"),
            },
        ];

        // 不带 source → 返回第一个匹配
        let found = registry.find_by_name("same-name", None).unwrap();
        assert_eq!(found.source, SkillSource::Blink);

        // 带 source → 返回指定来源
        let found = registry
            .find_by_name("same-name", Some(SkillSource::Claude))
            .unwrap();
        assert_eq!(found.source, SkillSource::Claude);
        assert_eq!(found.description, "Claude version");
    }

    // ── parse_skill_command ──

    #[test]
    fn parse_skill_command_basic() {
        let cmd = parse_skill_command("/skill rust-debug").unwrap();
        assert_eq!(cmd.name, "rust-debug");
        assert!(cmd.source.is_none());
        assert!(cmd.remaining_message.is_empty());
    }

    #[test]
    fn parse_skill_command_with_source() {
        let cmd = parse_skill_command("/skill rust-debug@claude").unwrap();
        assert_eq!(cmd.name, "rust-debug");
        assert_eq!(cmd.source, Some(SkillSource::Claude));
    }

    #[test]
    fn parse_skill_command_with_message() {
        let cmd = parse_skill_command("/skill rust-debug help me fix this error").unwrap();
        assert_eq!(cmd.name, "rust-debug");
        assert_eq!(cmd.remaining_message, "help me fix this error");
    }

    #[test]
    fn parse_skill_command_case_insensitive_prefix() {
        let cmd = parse_skill_command("/SKILL rust-debug").unwrap();
        assert_eq!(cmd.name, "rust-debug");
    }

    #[test]
    fn parse_skill_command_not_a_command() {
        assert!(parse_skill_command("hello world").is_none());
        assert!(parse_skill_command("/other command").is_none());
    }

    #[test]
    fn parse_skill_command_empty_name() {
        assert!(parse_skill_command("/skill ").is_none());
        assert!(parse_skill_command("/skill").is_none());
    }

    #[test]
    fn parse_skill_command_unknown_source_ignored() {
        let cmd = parse_skill_command("/skill test@unknown").unwrap();
        assert_eq!(cmd.name, "test");
        assert!(cmd.source.is_none());
    }

    // ── SkillSource ──

    #[test]
    fn skill_source_display_name() {
        assert_eq!(SkillSource::Blink.display_name(), "blink");
        assert_eq!(SkillSource::Claude.display_name(), "claude");
        assert_eq!(SkillSource::Zcode.display_name(), "zcode");
    }

    #[test]
    fn skill_source_all_returns_ordered() {
        let sources = SkillSource::all();
        assert_eq!(sources[0], SkillSource::Blink);
        assert_eq!(sources[1], SkillSource::Claude);
        assert_eq!(sources[2], SkillSource::Zcode);
    }
}
