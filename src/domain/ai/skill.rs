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
use std::sync::atomic::{AtomicBool, Ordering};

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
    /// OpenCode 目录 `~/.opencode/skills/`
    Opencode,
    /// Codex 目录 `~/.codex/skills/`
    Codex,
}

impl SkillSource {
    /// 返回来源的显示名（注入 preamble 的 `[source]` 标注用）。
    pub fn display_name(&self) -> &'static str {
        match self {
            Self::Blink => "blink",
            Self::Claude => "claude",
            Self::Zcode => "zcode",
            Self::Opencode => "opencode",
            Self::Codex => "codex",
        }
    }

    /// 返回该来源的 skill 目录路径（Windows 下 `%APPDATA%` / `%USERPROFILE%` 展开）。
    ///
    /// 目录不存在时仍返回路径（调用方自行 `exists()` 判断）。
    pub fn directory(&self) -> Option<PathBuf> {
        match self {
            Self::Blink => Some(crate::infra::utils::paths::skills_global_dir()),
            Self::Claude => dirs_next::home_dir().map(|h| h.join(".claude").join("skills")),
            Self::Zcode => dirs_next::home_dir().map(|h| h.join(".zcode").join("skills")),
            Self::Opencode => dirs_next::home_dir().map(|h| h.join(".opencode").join("skills")),
            Self::Codex => dirs_next::home_dir().map(|h| h.join(".codex").join("skills")),
        }
    }

    /// 外部来源（除 Blink 外，供「从其他应用导入」功能枚举）。
    /// 按 id 字母序，与前端下拉保持一致。
    pub fn external_sources() -> &'static [SkillSource] {
        &[Self::Claude, Self::Codex, Self::Opencode, Self::Zcode]
    }

    /// 所有来源（按优先级排序：Blink → 其余）。
    #[allow(dead_code)] // 供未来扩展使用
    pub fn all() -> &'static [SkillSource] {
        &[
            Self::Blink,
            Self::Claude,
            Self::Codex,
            Self::Opencode,
            Self::Zcode,
        ]
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
    /// 预编译的正则模式（从 `triggers.patterns` 编译，scan 时一次性完成）。
    /// 无 triggers 或 patterns 为空时为空 Vec。避免 match_triggers 每次调用重新编译。
    #[serde(skip)]
    pub compiled_patterns: Vec<Regex>,
    /// SKILL.md body（frontmatter 之后的 Markdown 全文）。
    #[serde(skip)]
    pub full_content: String,
    /// 来源目录。
    pub source: SkillSource,
    /// SKILL.md 所在目录路径。
    #[allow(dead_code)] // 供未来扩展（如打开 skill 文件编辑）
    #[serde(skip)]
    pub dir_path: PathBuf,
    /// 来源 CLI 可执行文件路径（CLI 识别生成的 Skill 才有此字段）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_cli_path: Option<String>,
}

// ── SkillRegistry ────────────────────────────────────────────────────────────

/// Skill 注册表——内存结构，启动时扫描，可手动刷新。
///
/// 不进 DB——SKILL.md 是文件系统约定（各 agent 共享），进 DB 反而破坏"复用生态"语义。
pub struct SkillRegistry {
    skills: RwLock<Vec<SkillEntry>>,
    /// 0.13.6: 被用户禁用的 Skill 标识列表（格式：`name@source`）。
    disabled_skills: RwLock<std::collections::HashSet<String>>,
    /// 0.19.10: 运行时总开关。关闭时所有读取/触发入口立即旁路。
    enabled: AtomicBool,
}

impl SkillRegistry {
    pub fn new() -> Self {
        Self {
            skills: RwLock::new(Vec::new()),
            disabled_skills: RwLock::new(std::collections::HashSet::new()),
            enabled: AtomicBool::new(true),
        }
    }

    /// 设置运行时总开关。调用方在重扫期间先关闭，完成后再开启，避免半更新可见。
    pub fn set_enabled(&self, enabled: bool) {
        self.enabled.store(enabled, Ordering::Release);
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::Acquire)
    }

    /// 清空当前进程已发现的 Skill；禁用总开关时调用。
    pub fn clear(&self) {
        self.skills
            .write()
            .expect("skill registry lock poisoned")
            .clear();
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
        *self.skills.write().expect("skill registry lock poisoned") = entries;
        tracing::info!(total, "SkillRegistry: 扫描完成");
    }

    /// 返回所有已发现的 Skill 摘要（阶段 1 preamble 注入用）。
    ///
    /// 0.13.6: 过滤被用户禁用的 Skill。
    pub fn summaries(&self) -> Vec<SkillSummary> {
        if !self.is_enabled() {
            return Vec::new();
        }
        let disabled = self.disabled_skills.read().expect("lock poisoned");
        self.skills
            .read()
            .expect("skill registry lock poisoned")
            .iter()
            .filter(|s| !disabled.contains(&skill_id(&s.name, s.source)))
            .map(|s| SkillSummary {
                name: s.name.clone(),
                description: s.description.clone(),
                source: s.source,
                has_triggers: s.triggers.is_some(),
            })
            .collect()
    }

    /// 返回所有已发现的 Skill 条目（设置页展示用）。
    #[allow(dead_code)] // 已被 all_with_status() 替代，保留供未来可能使用
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
    ///
    /// 0.13.6: 过滤被用户禁用的 Skill。
    pub fn match_triggers(&self, message: &str) -> Vec<SkillEntry> {
        if !self.is_enabled() {
            return Vec::new();
        }
        let disabled = self.disabled_skills.read().expect("lock poisoned");
        let skills = self.skills.read().expect("skill registry lock poisoned");
        let msg_lower = message.to_lowercase();

        skills
            .iter()
            .filter(|s| {
                // 0.13.6: 被禁用的 Skill 不触发
                if disabled.contains(&skill_id(&s.name, s.source)) {
                    return false;
                }
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
                // 正则匹配（使用预编译的模式）
                s.compiled_patterns.iter().any(|re| re.is_match(message))
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
        if !self.is_enabled() {
            return None;
        }
        let disabled = self.disabled_skills.read().expect("lock poisoned");
        self.skills
            .read()
            .expect("skill registry lock poisoned")
            .iter()
            .find(|s| {
                s.name == name
                    && source_filter.is_none_or(|src| s.source == src)
                    && !disabled.contains(&skill_id(&s.name, s.source))
            })
            .cloned()
    }

    /// 已发现的 Skill 数量。
    pub fn count(&self) -> usize {
        if !self.is_enabled() {
            return 0;
        }
        self.skills
            .read()
            .expect("skill registry lock poisoned")
            .len()
    }

    /// 0.13.6: 设置被禁用的 Skill 列表。
    ///
    /// `ids` 格式为 `name@source`（如 `"rust-debug@claude"`）。
    pub fn set_disabled_skills(&self, ids: Vec<String>) {
        *self.disabled_skills.write().expect("lock poisoned") = ids.into_iter().collect();
    }

    /// 0.13.6: 检查指定 Skill 是否被禁用。
    #[allow(dead_code)] // 目前仅测试使用，保留供未来可能需要
    pub fn is_disabled(&self, name: &str, source: SkillSource) -> bool {
        self.disabled_skills
            .read()
            .expect("lock poisoned")
            .contains(&skill_id(name, source))
    }

    /// 0.13.6: 返回所有 Skill 条目，附带 `disabled` 标记。
    ///
    /// 设置页展示用——前端用此标记渲染复选框状态。
    pub fn all_with_status(&self) -> Vec<SkillEntryWithStatus> {
        if !self.is_enabled() {
            return Vec::new();
        }
        let disabled = self.disabled_skills.read().expect("lock poisoned");
        self.skills
            .read()
            .expect("skill registry lock poisoned")
            .iter()
            .map(|s| SkillEntryWithStatus {
                dir: s.dir_path.display().to_string(),
                entry: s.clone(),
                disabled: disabled.contains(&skill_id(&s.name, s.source)),
            })
            .collect()
    }
}

impl Default for SkillRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// 0.13.6: Skill 标识——`name@source` 格式。
pub fn skill_id(name: &str, source: SkillSource) -> String {
    format!("{}@{}", name, source.display_name())
}

/// 0.13.6: 带 disabled 标记的 Skill 条目（设置页展示用）。
#[derive(Debug, Clone, serde::Serialize)]
pub struct SkillEntryWithStatus {
    #[serde(flatten)]
    pub entry: SkillEntry,
    pub disabled: bool,
    /// SKILL.md 所在目录路径（序列化为字符串，供前端展示/导入/打开目录用）。
    pub dir: String,
}

// ── 外部来源导入（0.13.7）─────────────────────────────────────────────────────

/// 外部来源的概要信息（供「从其他应用导入 Skill」面板展示）。
#[derive(Debug, Clone, serde::Serialize)]
pub struct ExternalSkillSourceInfo {
    /// 来源标识（claude / codex / opencode / zcode）。
    pub id: String,
    /// 显示名（Claude / Codex / OpenCode / ZCode）。
    pub label: String,
    /// 该来源的 skill 目录绝对路径（目录不存在时仍返回字符串）。
    pub dir: String,
    /// 目录是否存在。
    pub exists: bool,
    /// 该目录下已发现的 skill 概要列表。
    pub skills: Vec<ExternalSkillSummary>,
}

/// 外部来源下单个 skill 的概要（导入面板勾选用，只需 name + dir + description）。
#[derive(Debug, Clone, serde::Serialize)]
pub struct ExternalSkillSummary {
    pub name: String,
    pub description: String,
    /// SKILL.md 所在目录绝对路径——前端选中后作为 `import_skill` 的 `source_path`。
    pub dir: String,
}

/// 枚举所有外部来源（Claude / Codex / OpenCode / ZCode）的概要信息。
///
/// 供设置页「导入 Skill」面板：下拉选应用 → 展示该应用目录下可导入的 skill。
/// 目录不存在或读取失败时返回空 skills 列表，`exists=false`。
pub fn list_external_sources() -> Vec<ExternalSkillSourceInfo> {
    SkillSource::external_sources()
        .iter()
        .map(|&src| external_source_info(src))
        .collect()
}

/// 构建单个外部来源的概要信息。
fn external_source_info(source: SkillSource) -> ExternalSkillSourceInfo {
    let dir = match source.directory() {
        Some(d) => d,
        None => {
            return ExternalSkillSourceInfo {
                id: source.display_name().to_string(),
                label: source_label(source),
                dir: String::new(),
                exists: false,
                skills: Vec::new(),
            };
        }
    };
    let dir_str = dir.display().to_string();
    let exists = dir.exists();
    let skills = if exists {
        match scan_directory(&dir, source) {
            Ok(entries) => entries
                .into_iter()
                .map(|e| ExternalSkillSummary {
                    name: e.name,
                    description: e.description,
                    dir: e.dir_path.display().to_string(),
                })
                .collect(),
            Err(e) => {
                tracing::warn!(source = source.display_name(), %e, "扫描外部来源目录失败");
                Vec::new()
            }
        }
    } else {
        Vec::new()
    };
    ExternalSkillSourceInfo {
        id: source.display_name().to_string(),
        label: source_label(source),
        dir: dir_str,
        exists,
        skills,
    }
}

/// 来源显示名（首字母大写形式，用于 UI 展示）。
fn source_label(source: SkillSource) -> String {
    match source {
        SkillSource::Blink => "Blink".to_string(),
        SkillSource::Claude => "Claude".to_string(),
        SkillSource::Zcode => "ZCode".to_string(),
        SkillSource::Opencode => "OpenCode".to_string(),
        SkillSource::Codex => "Codex".to_string(),
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

    // 预编译正则模式（避免 match_triggers 每次调用重新编译）
    let compiled_patterns = triggers
        .as_ref()
        .map(|t| {
            t.patterns
                .iter()
                .filter_map(|pat| match Regex::new(pat) {
                    Ok(re) => Some(re),
                    Err(e) => {
                        tracing::warn!(pattern = %pat, %e, "Skill 正则编译失败，跳过");
                        None
                    }
                })
                .collect()
        })
        .unwrap_or_default();

    Some(SkillEntry {
        name,
        description,
        triggers,
        compiled_patterns,
        full_content: body,
        source,
        dir_path,
        source_cli_path: frontmatter.get("source_cli_path").map(|s| s.to_string()),
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
        if let Some(after_dashes) = after_newline.strip_prefix("---") {
            // 确认 --- 后面是行尾或换行（不是 ---something）
            if after_dashes.is_empty()
                || after_dashes.starts_with('\n')
                || after_dashes.starts_with('\r')
            {
                return Some(idx + 1);
            }
        }
    }
    // 也检查第一行（rest 本身以 --- 开头的情况）
    if let Some(after_dashes) = s.strip_prefix("---")
        && (after_dashes.is_empty()
            || after_dashes.starts_with('\n')
            || after_dashes.starts_with('\r'))
    {
        return Some(0);
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
    let patterns_str = map
        .get("triggers.patterns")
        .map(String::as_str)
        .unwrap_or("");

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
            .map(clean_yaml_value)
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
        map.insert(
            "triggers.keywords".to_string(),
            "[cargo, rustc]".to_string(),
        );
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
        assert_eq!(
            skill.triggers.as_ref().unwrap().keywords,
            vec!["cargo", "E0"]
        );
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
    fn runtime_switch_immediately_bypasses_and_can_restore_registry() {
        let registry = SkillRegistry::new();
        let skill = parse_skill_md(
            "---\nname: runtime-switch\ndescription: Runtime switch test\ntriggers:\n  keywords: [activate-me]\n---\nBody",
            SkillSource::Blink,
            PathBuf::from("/tmp/runtime-switch"),
        )
        .unwrap();
        *registry.skills.write().unwrap() = vec![skill];

        assert_eq!(registry.count(), 1);
        assert_eq!(registry.match_triggers("activate-me").len(), 1);
        assert!(registry.find_by_name("runtime-switch", None).is_some());

        registry.set_enabled(false);
        assert_eq!(registry.count(), 0);
        assert!(registry.summaries().is_empty());
        assert!(registry.match_triggers("activate-me").is_empty());
        assert!(registry.find_by_name("runtime-switch", None).is_none());

        registry.set_enabled(true);
        assert_eq!(registry.count(), 1);
        assert_eq!(registry.match_triggers("activate-me").len(), 1);

        registry.clear();
        assert_eq!(registry.count(), 0);
    }

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
            compiled_patterns: Vec::new(),
            full_content: "# Rust Debug".to_string(),
            source: SkillSource::Blink,
            dir_path: PathBuf::from("/tmp"),
            source_cli_path: None,
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
            compiled_patterns: Vec::new(),
            full_content: String::new(),
            source: SkillSource::Blink,
            dir_path: PathBuf::from("/tmp"),
            source_cli_path: None,
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
            compiled_patterns: vec![Regex::new(r"error\[E\d+\]").unwrap()],
            full_content: String::new(),
            source: SkillSource::Claude,
            dir_path: PathBuf::from("/tmp"),
            source_cli_path: None,
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
            compiled_patterns: Vec::new(),
            full_content: String::new(),
            source: SkillSource::Blink,
            dir_path: PathBuf::from("/tmp"),
            source_cli_path: None,
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
                compiled_patterns: Vec::new(),
                full_content: String::new(),
                source: SkillSource::Blink,
                dir_path: PathBuf::from("/tmp"),
                source_cli_path: None,
            },
            SkillEntry {
                name: "skill-b".to_string(),
                description: "B".to_string(),
                triggers: Some(SkillTriggers {
                    keywords: vec![],
                    patterns: vec![r"error\[E".to_string()],
                }),
                compiled_patterns: vec![Regex::new(r"error\[E").unwrap()],
                full_content: String::new(),
                source: SkillSource::Claude,
                dir_path: PathBuf::from("/tmp"),
                source_cli_path: None,
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
            compiled_patterns: Vec::new(),
            full_content: String::new(),
            source: SkillSource::Blink,
            dir_path: PathBuf::from("/tmp"),
            source_cli_path: None,
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
                compiled_patterns: Vec::new(),
                full_content: String::new(),
                source: SkillSource::Blink,
                dir_path: PathBuf::from("/tmp"),
                source_cli_path: None,
            },
            SkillEntry {
                name: "same-name".to_string(),
                description: "Claude version".to_string(),
                triggers: None,
                compiled_patterns: Vec::new(),
                full_content: String::new(),
                source: SkillSource::Claude,
                dir_path: PathBuf::from("/tmp"),
                source_cli_path: None,
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
        assert_eq!(SkillSource::Opencode.display_name(), "opencode");
        assert_eq!(SkillSource::Codex.display_name(), "codex");
    }

    #[test]
    fn skill_source_all_returns_ordered() {
        // all() 按 Blink 优先 + 其余字母序
        let sources = SkillSource::all();
        assert_eq!(sources[0], SkillSource::Blink);
        assert_eq!(sources[1], SkillSource::Claude);
        assert_eq!(sources[2], SkillSource::Codex);
        assert_eq!(sources[3], SkillSource::Opencode);
        assert_eq!(sources[4], SkillSource::Zcode);
    }

    #[test]
    fn skill_source_external_sources_excludes_blink() {
        // external_sources() 不含 Blink，供「从其他应用导入」面板枚举
        let sources = SkillSource::external_sources();
        assert!(!sources.contains(&SkillSource::Blink));
        assert!(sources.contains(&SkillSource::Claude));
        assert!(sources.contains(&SkillSource::Codex));
        assert!(sources.contains(&SkillSource::Opencode));
        assert!(sources.contains(&SkillSource::Zcode));
    }

    #[test]
    fn skill_source_directory_maps_to_expected_path() {
        // 外部来源目录都在 home 下（Claude/Codex/OpenCode/ZCode）
        for &src in SkillSource::external_sources() {
            let dir = src.directory().expect("外部来源应有目录");
            let parent_name = dir
                .parent()
                .and_then(|p| p.file_name())
                .and_then(|n| n.to_str())
                .unwrap_or("");
            // 父目录应为 ~/.claude / ~/.codex / ~/.opencode / ~/.zcode
            assert!(
                parent_name.starts_with('.'),
                "外部来源 {} 的父目录应为隐藏目录，实际: {}",
                src.display_name(),
                parent_name
            );
        }
    }

    // ── 0.13.6: disabled_skills 过滤 ──

    #[test]
    fn disabled_skills_excluded_from_summaries() {
        let registry = SkillRegistry::new();
        *registry.skills.write().unwrap() = vec![
            SkillEntry {
                name: "active".to_string(),
                description: "Active skill".to_string(),
                triggers: Some(SkillTriggers {
                    keywords: vec!["cargo".to_string()],
                    patterns: vec![],
                }),
                compiled_patterns: Vec::new(),
                full_content: String::new(),
                source: SkillSource::Blink,
                dir_path: PathBuf::from("/tmp/active"),
                source_cli_path: None,
            },
            SkillEntry {
                name: "disabled".to_string(),
                description: "Disabled skill".to_string(),
                triggers: Some(SkillTriggers {
                    keywords: vec!["cargo".to_string()],
                    patterns: vec![],
                }),
                compiled_patterns: Vec::new(),
                full_content: String::new(),
                source: SkillSource::Claude,
                dir_path: PathBuf::from("/tmp/disabled"),
                source_cli_path: None,
            },
        ];
        registry.set_disabled_skills(vec!["disabled@claude".to_string()]);

        let summaries = registry.summaries();
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].name, "active");
    }

    #[test]
    fn disabled_skills_excluded_from_match_triggers() {
        let registry = SkillRegistry::new();
        *registry.skills.write().unwrap() = vec![
            SkillEntry {
                name: "skill-a".to_string(),
                description: "A".to_string(),
                triggers: Some(SkillTriggers {
                    keywords: vec!["cargo".to_string()],
                    patterns: vec![],
                }),
                compiled_patterns: Vec::new(),
                full_content: String::new(),
                source: SkillSource::Blink,
                dir_path: PathBuf::from("/tmp/a"),
                source_cli_path: None,
            },
            SkillEntry {
                name: "skill-b".to_string(),
                description: "B".to_string(),
                triggers: Some(SkillTriggers {
                    keywords: vec!["cargo".to_string()],
                    patterns: vec![],
                }),
                compiled_patterns: Vec::new(),
                full_content: String::new(),
                source: SkillSource::Claude,
                dir_path: PathBuf::from("/tmp/b"),
                source_cli_path: None,
            },
        ];
        registry.set_disabled_skills(vec!["skill-b@claude".to_string()]);

        let matched = registry.match_triggers("cargo build");
        assert_eq!(matched.len(), 1);
        assert_eq!(matched[0].name, "skill-a");
    }

    #[test]
    fn disabled_skills_re_enable() {
        let registry = SkillRegistry::new();
        *registry.skills.write().unwrap() = vec![SkillEntry {
            name: "test".to_string(),
            description: "Test".to_string(),
            triggers: Some(SkillTriggers {
                keywords: vec!["cargo".to_string()],
                patterns: vec![],
            }),
            compiled_patterns: Vec::new(),
            full_content: String::new(),
            source: SkillSource::Blink,
            dir_path: PathBuf::from("/tmp"),
            source_cli_path: None,
        }];

        // Disable
        registry.set_disabled_skills(vec!["test@blink".to_string()]);
        assert!(registry.is_disabled("test", SkillSource::Blink));
        assert!(registry.find_by_name("test", None).is_none());
        assert!(registry.match_triggers("cargo").is_empty());

        // Re-enable
        registry.set_disabled_skills(vec![]);
        assert!(!registry.is_disabled("test", SkillSource::Blink));
        assert_eq!(registry.match_triggers("cargo").len(), 1);
    }

    #[test]
    fn all_with_status_includes_disabled_flag_and_dir() {
        let registry = SkillRegistry::new();
        *registry.skills.write().unwrap() = vec![
            SkillEntry {
                name: "active".to_string(),
                description: "Active".to_string(),
                triggers: None,
                compiled_patterns: Vec::new(),
                full_content: String::new(),
                source: SkillSource::Blink,
                dir_path: PathBuf::from("/tmp/active"),
                source_cli_path: None,
            },
            SkillEntry {
                name: "inactive".to_string(),
                description: "Inactive".to_string(),
                triggers: None,
                compiled_patterns: Vec::new(),
                full_content: String::new(),
                source: SkillSource::Claude,
                dir_path: PathBuf::from("/tmp/inactive"),
                source_cli_path: None,
            },
        ];
        registry.set_disabled_skills(vec!["inactive@claude".to_string()]);

        let entries = registry.all_with_status();
        assert_eq!(entries.len(), 2);

        let active = entries.iter().find(|e| e.entry.name == "active").unwrap();
        assert!(!active.disabled);
        assert_eq!(active.dir, "/tmp/active");

        let inactive = entries.iter().find(|e| e.entry.name == "inactive").unwrap();
        assert!(inactive.disabled);
        assert_eq!(inactive.dir, "/tmp/inactive");
    }

    #[test]
    fn skill_id_format() {
        assert_eq!(skill_id("test", SkillSource::Blink), "test@blink");
        assert_eq!(skill_id("rust", SkillSource::Claude), "rust@claude");
        assert_eq!(skill_id("z", SkillSource::Zcode), "z@zcode");
    }

    // ── P4: Skill 从 --help 输出到 preamble 注入闭环 ──

    /// 验证完整链路：
    /// 1. blink --help 输出 → parse_help_output() → ParsedHelp
    /// 2. generate_skill_md() → SKILL.md 文本
    /// 3. parse_skill_md() → SkillEntry（能解析回来）
    /// 4. SkillRegistry → summaries() / match_triggers() 可用
    #[test]
    fn skill_help_to_preamble_full_roundtrip() {
        use crate::domain::ai::cli_recognizer::{generate_skill_md, parse_help_output};

        // 1. 模拟 blink --help 输出（clap 生成格式）
        let help_text = r#"Blink — Windows 全局快捷入口 (CLI 模式)

Usage: blink [COMMAND]

Commands:
  mcp-server    作为 MCP server 运行（stdio 模式，供外部 MCP client 连接）
  search        搜索应用
  run           调用任意 Capability
  capabilities  列出所有可用 Capability
  config        读写配置
  chat          终端对话模式（基础实现）
  help          Print this message or the help of the given subcommand(s)

Options:
  -h, --help     Print help
  -V, --version  Print version
"#;

        // 2. 解析 help 输出
        let parsed = parse_help_output(help_text);
        assert!(!parsed.subcommands.is_empty(), "应解析出子命令");
        assert!(parsed.subcommands.iter().any(|c| c.name == "mcp-server"));
        assert!(parsed.subcommands.iter().any(|c| c.name == "search"));
        assert!(parsed.subcommands.iter().any(|c| c.name == "chat"));

        // 3. 生成 SKILL.md
        let skill_md = generate_skill_md(&parsed, "blink", None);
        assert!(skill_md.contains("name: blink-cli"));
        assert!(skill_md.contains("keywords: [blink, mcp-server, search"));
        assert!(skill_md.contains("# Blink 命令行工具"));
        assert!(skill_md.contains("mcp-server"));

        // 4. 解析回 SkillEntry
        let skill = parse_skill_md(
            &skill_md,
            SkillSource::Blink,
            std::path::PathBuf::from("/tmp/blink"),
        )
        .expect("生成的 SKILL.md 应能被解析回来");

        assert_eq!(skill.name, "blink-cli");
        assert!(
            skill.triggers.is_some(),
            "应有 triggers（keywords 来自子命令名）"
        );
        let triggers = skill.triggers.as_ref().unwrap();
        assert!(triggers.keywords.contains(&"blink".to_string()));
        assert!(triggers.keywords.contains(&"mcp-server".to_string()));

        // 5. 注入 SkillRegistry → 验证 summaries() 和 match_triggers()
        let registry = SkillRegistry::new();
        *registry.skills.write().unwrap() = vec![skill.clone()];

        let summaries = registry.summaries();
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].name, "blink-cli");
        assert!(summaries[0].has_triggers);

        // 发消息包含 "blink" 关键词 → 应触发
        let matched = registry.match_triggers("帮我用 blink 搜索应用");
        assert_eq!(matched.len(), 1);
        assert_eq!(matched[0].name, "blink-cli");
    }

    /// 验证 SKILL.md 的 disabled 过滤在闭环中也生效。
    #[test]
    fn skill_help_roundtrip_with_disabled_filter() {
        use crate::domain::ai::cli_recognizer::{generate_skill_md, parse_help_output};

        let help = "A tool\nUsage: mytool\n  run    Run something";
        let parsed = parse_help_output(help);
        let skill_md = generate_skill_md(&parsed, "mytool", None);

        let skill = parse_skill_md(
            &skill_md,
            SkillSource::Blink,
            std::path::PathBuf::from("/tmp"),
        )
        .expect("should parse");

        let registry = SkillRegistry::new();
        *registry.skills.write().unwrap() = vec![skill];

        // 禁用后 summaries 不包含
        registry.set_disabled_skills(vec![skill_id("mytool-cli", SkillSource::Blink)]);
        assert!(registry.summaries().is_empty());
        assert!(registry.match_triggers("mytool run").is_empty());
    }

    // ── 真正的 blink --help → SKILL.md → SkillRegistry → preamble 闭环 ──

    /// 用 **实际的** clap Cli::command().render_help() 输出（而非模拟文本），
    /// 走完整链路：help 文本 → parse_help_output → generate_skill_md →
    /// parse_skill_md → SkillRegistry → summaries + match_triggers。
    ///
    /// 这是用户要求的闭环：blink 自己的 help 命令 → skill 探测 → 转化为 skill。
    #[test]
    fn skill_e2e_real_blink_help_to_preamble() {
        use crate::cli::Cli;
        use crate::domain::ai::cli_recognizer::{generate_skill_md, parse_help_output};
        use clap::CommandFactory;

        // 1. 获取真正的 blink --help 输出（clap 生成，不是模拟文本）
        let help_text = Cli::command().render_help().to_string();
        assert!(!help_text.is_empty(), "blink --help 应有输出");
        // clap 生成的 help 应包含 blink 自身的描述
        assert!(
            help_text.contains("blink") || help_text.contains("Blink"),
            "help 文本应包含 blink 名称: {help_text}"
        );

        // 2. 解析 help 输出
        let parsed = parse_help_output(&help_text);
        // clap 的 help 格式应该能解析出子命令（mcp-server / search / run 等）
        assert!(
            !parsed.subcommands.is_empty(),
            "应从 blink --help 解析出子命令, got: {:?}",
            parsed.subcommands
        );
        // 确认关键子命令被解析出来
        let sub_names: Vec<&str> = parsed.subcommands.iter().map(|c| c.name.as_str()).collect();
        assert!(
            sub_names.contains(&"mcp-server"),
            "应包含 mcp-server 子命令, got: {sub_names:?}"
        );
        assert!(
            sub_names.contains(&"search"),
            "应包含 search 子命令, got: {sub_names:?}"
        );
        assert!(
            sub_names.contains(&"capabilities"),
            "应包含 capabilities 子命令, got: {sub_names:?}"
        );

        // 3. 生成 SKILL.md
        let skill_md = generate_skill_md(&parsed, "blink", None);
        assert!(skill_md.contains("name: blink-cli"));
        assert!(skill_md.contains("# Blink 命令行工具"));
        // keywords 应包含子命令名
        assert!(skill_md.contains("mcp-server"));
        assert!(skill_md.contains("search"));

        // 4. 解析回 SkillEntry（验证生成的 SKILL.md 格式正确）
        let skill = parse_skill_md(
            &skill_md,
            SkillSource::Blink,
            std::path::PathBuf::from("/tmp/blink"),
        )
        .expect("生成的 SKILL.md 应能被 parse_skill_md 解析回来");

        assert_eq!(skill.name, "blink-cli");
        assert!(
            skill.triggers.is_some(),
            "应有 triggers（keywords 来自子命令名）"
        );
        let triggers = skill.triggers.as_ref().unwrap();
        assert!(triggers.keywords.contains(&"blink".to_string()));
        assert!(triggers.keywords.contains(&"mcp-server".to_string()));
        assert!(triggers.keywords.contains(&"search".to_string()));

        // 5. 注入 SkillRegistry → 验证 preamble 链路可用
        let registry = SkillRegistry::new();
        *registry.skills.write().unwrap() = vec![skill.clone()];

        let summaries = registry.summaries();
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].name, "blink-cli");
        assert!(summaries[0].has_triggers);

        // 6. 用用户消息触发——消息包含 "blink" 关键词
        let matched = registry.match_triggers("帮我用 blink 搜索应用");
        assert_eq!(matched.len(), 1);
        assert_eq!(matched[0].name, "blink-cli");
        assert_eq!(matched[0].source, SkillSource::Blink);

        // 7. 验证触发的 skill 可注入 preamble
        use crate::domain::ai::prompt::chat_system_prompt_with_skills;
        let prompt = chat_system_prompt_with_skills(None, &summaries, &matched);
        assert!(prompt.contains("blink-cli"), "preamble 应包含 skill name");
        assert!(prompt.contains("可用技能"), "应有技能摘要段");
        assert!(
            prompt.contains("已激活技能详情"),
            "应有已激活技能详情段（触发的 skill 全文）"
        );

        tracing::info!(
            keywords = ?triggers.keywords,
            subcommands = parsed.subcommands.len(),
            "Skill e2e: blink --help → SKILL.md → preamble 闭环验证通过"
        );
    }
}
