//! CLI 能力识别（0.13.6）——从 `--help` 输出生成 SKILL.md 模板。
//!
//! 纯文本解析，零 LLM 依赖。运行 `<cli> --help` 获取帮助文本，
//! 用启发式解析提取命令/子命令/参数，生成 SKILL.md 模板供用户 review 编辑。
//!
//! ## 解析规则
//!
//! - **子命令识别**：行首 2+ 空格 + 单词 + 2+ 空格 + 描述文本（`git` / `docker` / `kubectl` 风格）
//! - **选项识别**：行首 2+ 空格 + `-x, --xxx` 或 `--xxx` + 空格 + 描述（GNU getopt 风格）
//! - **Usage 行**：`Usage: ...` 行，提取为用法示例
//! - **描述**：首行非空文本（跳过 Usage 行）
//!
//! ## 与 0.14 的区别
//!
//! - 0.13.6 纯文本解析，零模型依赖，生成「半成品」模板
//! - 0.14 用 LLM 理解 `--help` 语义，生成更精准的 SKILL.md

use std::path::Path;

// ── 数据结构 ──────────────────────────────────────────────────────────────────

/// `--help` 文本解析结果。
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedHelp {
    /// 从首行非空文本提取的描述。
    pub description: String,
    /// 解析出的子命令列表。
    pub subcommands: Vec<CliCommand>,
    /// 解析出的全局选项。
    pub options: Vec<CliOption>,
    /// Usage 行（原始文本）。
    pub usage_line: Option<String>,
}

/// 一个 CLI 子命令。
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct CliCommand {
    pub name: String,
    pub description: String,
}

/// 一个 CLI 选项。
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct CliOption {
    /// 选项标志（如 `-v, --verbose`）。
    pub flags: String,
    /// 描述文本。
    pub description: String,
}

/// CLI 能力识别结果（供 Tauri command 返回）。
#[derive(Debug, Clone, serde::Serialize)]
pub struct CliRecognitionResult {
    pub tool_name: String,
    pub description: String,
    pub subcommands: Vec<CliCommand>,
    pub options: Vec<CliOption>,
    pub usage_line: Option<String>,
    /// 生成的 SKILL.md 全文。
    pub skill_md_content: String,
    /// 保存路径。
    pub saved_path: String,
    /// 来源 CLI 可执行文件路径（用于后续重新生成）。
    pub source_cli_path: String,
}

// ── 核心解析 ──────────────────────────────────────────────────────────────────

/// 解析 `--help` 文本，提取结构化信息。
///
/// 纯函数，可单测。不依赖外部资源。
pub fn parse_help_output(output: &str) -> ParsedHelp {
    let mut subcommands = Vec::new();
    let mut options = Vec::new();
    let mut usage_line = None;
    let mut description = String::new();

    for line in output.lines() {
        let trimmed = line.trim();

        // Usage 行
        if trimmed.starts_with("Usage:")
            || trimmed.starts_with("usage:")
            || trimmed.starts_with("USAGE:")
        {
            if usage_line.is_none() {
                usage_line = Some(trimmed.to_string());
            }
            continue;
        }

        // 首行非空文本作为描述（跳过 Usage 行和选项行）
        if description.is_empty()
            && !trimmed.is_empty()
            && !trimmed.starts_with('-')
            && !trimmed.starts_with("Usage")
            && !trimmed.starts_with("usage:")
        {
            description = trimmed.to_string();
        }

        // 子命令：行首 2+ 空格 + 单词 + 2+ 空格 + 描述
        if let Some(cmd) = parse_subcommand_line(line) {
            subcommands.push(cmd);
        }

        // 选项：行首 2+ 空格 + -x / --xxx
        if let Some(opt) = parse_option_line(line) {
            options.push(opt);
        }
    }

    ParsedHelp {
        description,
        subcommands,
        options,
        usage_line,
    }
}

/// 解析子命令行——行首 2+ 空格 + 单词 + 2+ 空格 + 描述文本。
///
/// 匹配 `git --help` 风格的子命令列表：
/// ```text
///   add        Add file contents to the index
///   commit     Record changes to the repository
/// ```
///
/// 不匹配：不以空格开头的行（不是子命令），或只有单词没有描述的行。
fn parse_subcommand_line(line: &str) -> Option<CliCommand> {
    // 必须以 2+ 空格开头
    if !line.starts_with("  ") {
        return None;
    }

    // 跳过以 - 开头的行（那是选项）
    let trimmed = line.trim_start();
    if trimmed.starts_with('-') {
        return None;
    }

    // 分割：单词 + 2+ 空格 + 描述
    let rest = &line[2..]; // 跳过开头的空格
    // 找到第一个非空格字符到下一个 2+ 空格的位置
    let name_end = rest.find(' ').unwrap_or(rest.len());
    if name_end == 0 || name_end == rest.len() {
        return None;
    }

    let name = rest[..name_end].trim();
    if name.is_empty() || name.contains(' ') {
        return None;
    }

    // 跳过 name 后的空格，找描述
    let after_name = &rest[name_end..];
    let desc_start = after_name
        .find(|c: char| c != ' ')
        .unwrap_or(after_name.len());
    if desc_start == after_name.len() {
        // 只有 name 没有描述——跳过
        return None;
    }

    let description = after_name[desc_start..].trim().to_string();
    if description.is_empty() {
        return None;
    }

    // 过滤掉明显不是子命令的内容（如纯数字、太长的 "name"）
    if name.chars().any(|c| c.is_whitespace()) || name.len() > 30 {
        return None;
    }

    Some(CliCommand {
        name: name.to_string(),
        description,
    })
}

/// 解析选项行——行首 2+ 空格 + `-x, --xxx` 或 `--xxx` + 空格 + 描述。
///
/// 匹配 GNU getopt 风格：
/// ```text
///   -v, --verbose    Increase output verbosity
///   -q, --quiet      Suppress output
///       --config FILE  Configuration file path
/// ```
fn parse_option_line(line: &str) -> Option<CliOption> {
    // 必须以 2+ 空格开头
    if !line.starts_with("  ") {
        return None;
    }

    let trimmed = line.trim_start();
    if !trimmed.starts_with('-') {
        return None;
    }

    // 提取 flags 部分（到 2+ 连续空格为止）
    // 找到 flags 和 description 的分界：2+ 连续空格
    let rest = trimmed;

    // 找到 flags 结束位置：第一个 2+ 空格序列
    let mut flags_end = rest.len();
    let mut in_flags = true;
    let chars: Vec<char> = rest.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == ' ' && i + 1 < chars.len() && chars[i + 1] == ' ' {
            // 2+ 空格——flags 在此结束
            if in_flags {
                flags_end = i;
                in_flags = false;
            }
        } else if chars[i] != ' ' {
            in_flags = true;
        }
        i += 1;
    }

    let flags = rest[..flags_end].trim().to_string();
    if flags.is_empty() || !flags.starts_with('-') {
        return None;
    }

    // 描述是 flags 之后的文本
    let desc_part = &rest[flags_end..];
    let description = desc_part.trim().to_string();

    Some(CliOption { flags, description })
}

// ── SKILL.md 生成 ────────────────────────────────────────────────────────────

/// 从解析结果生成 SKILL.md 模板。
///
/// 生成的内容含 YAML frontmatter（name / description / source_cli_path / triggers / priority）
/// + Markdown body（用法 / 子命令表 / 选项表）。
///
/// `source_cli_path` 非空时写入 frontmatter，用于后续重新生成。
pub fn generate_skill_md(
    parsed: &ParsedHelp,
    tool_name: &str,
    source_cli_path: Option<&str>,
) -> String {
    let mut md = String::new();

    // frontmatter
    md.push_str("---\n");
    md.push_str(&format!("name: {}-cli\n", tool_name));
    md.push_str(&format!(
        "description: {}\n",
        parsed.description.replace('\n', " ")
    ));
    if let Some(path) = source_cli_path {
        md.push_str(&format!("source_cli_path: \"{}\"\n", path));
    }
    md.push_str("triggers:\n");
    // 关键词 = 子命令名 + 工具名
    let keywords: Vec<&str> = std::iter::once(tool_name)
        .chain(parsed.subcommands.iter().map(|c| c.name.as_str()))
        .take(20) // 限制关键词数量
        .collect();
    md.push_str(&format!(
        "  keywords: [{}]\n",
        keywords
            .iter()
            .map(|k| k.to_string())
            .collect::<Vec<_>>()
            .join(", ")
    ));
    md.push_str("priority: normal\n");
    md.push_str("---\n\n");

    // body
    md.push_str(&format!("# {} 命令行工具\n\n", capitalize(tool_name)));
    md.push_str("> ⚠️ 此 Skill 由 Blink CLI 能力识别自动生成，请检查并编辑后使用。\n\n");

    // 用法
    if let Some(usage) = &parsed.usage_line {
        md.push_str("## 用法\n\n```\n");
        md.push_str(usage);
        md.push_str("\n```\n\n");
    }

    // 子命令
    if !parsed.subcommands.is_empty() {
        md.push_str("## 子命令\n\n");
        md.push_str("| 命令 | 描述 |\n");
        md.push_str("|---|---|\n");
        for cmd in &parsed.subcommands {
            md.push_str(&format!(
                "| {} | {} |\n",
                cmd.name,
                cmd.description.replace('|', "\\|")
            ));
        }
        md.push('\n');
    }

    // 选项
    if !parsed.options.is_empty() {
        md.push_str("## 全局选项\n\n");
        md.push_str("| 选项 | 描述 |\n");
        md.push_str("|---|---|\n");
        for opt in &parsed.options {
            md.push_str(&format!(
                "| {} | {} |\n",
                opt.flags,
                opt.description.replace('|', "\\|")
            ));
        }
    }

    md
}

/// 首字母大写。
fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) => c.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

// ── 完整识别流程 ──────────────────────────────────────────────────────────────

/// 构建运行 CLI 命令的 `tokio::process::Command`。
///
/// `.cmd`/`.bat` 文件需要通过 `cmd.exe /C` 执行（CreateProcessW 不直接支持）。
fn build_cli_command(cli_path: &str) -> tokio::process::Command {
    let path = Path::new(cli_path);
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();

    if ext == "cmd" || ext == "bat" {
        let mut cmd = tokio::process::Command::new("cmd.exe");
        cmd.arg("/C").arg(cli_path);
        cmd
    } else {
        tokio::process::Command::new(cli_path)
    }
}

/// 执行 CLI 能力识别完整流程。
///
/// 1. 运行 `<cli> --help`，捕获 stdout + stderr
/// 2. 解析输出
/// 3. 生成 SKILL.md（含 `source_cli_path` frontmatter 字段）
/// 4. 保存到 `%APPDATA%\blink\skills\<tool-name>\SKILL.md`
///
/// 返回识别结果（含生成内容 + 保存路径 + 来源 CLI 路径）。
pub async fn recognize_cli(cli_path: &str) -> Result<CliRecognitionResult, String> {
    let path = Path::new(cli_path);
    if !path.exists() {
        return Err(format!("CLI 路径不存在: {cli_path}"));
    }

    // 0.13.7: 排除识别 Blink 自身。
    // Blink 启用了单实例（tauri_plugin_single_instance），当 GUI 已在运行时，
    // 再次启动 blink.exe（即使带 --help）的子进程会被单实例插件拦截并转交给
    // 已运行实例（从而唤起主窗口），子进程自身无 stdout 输出，
    // 导致「未能获取 --help 输出」的误导性错误。
    // 直接拒绝，给出清晰提示，避免唤起主窗口。
    let self_exe = std::env::current_exe()
        .ok()
        .and_then(|p| std::fs::canonicalize(p).ok());
    let target_canon = std::fs::canonicalize(path).ok();
    if let (Some(self_e), Some(tgt)) = (self_exe.as_ref(), target_canon.as_ref()) {
        if self_e == tgt {
            return Err(
                "不支持识别 Blink 自身（会触发单实例冲突并唤起主窗口）。请选择其他 CLI 工具。"
                    .to_string(),
            );
        }
    }

    // 从文件名提取工具名
    let tool_name = path
        .file_stem()
        .and_then(|n| n.to_str())
        .ok_or("无法从路径提取工具名")?;

    // 运行 --help（.cmd/.bat 通过 cmd.exe /C 执行）
    let output = build_cli_command(cli_path)
        .arg("--help")
        .output()
        .await
        .map_err(|e| format!("运行 --help 失败: {e}"))?;

    // stdout + stderr 合并（部分 CLI 把 help 输出到 stderr）
    let mut help_text = String::from_utf8_lossy(&output.stdout).to_string();
    if output.stdout.is_empty() || !output.status.success() {
        help_text.push_str(&String::from_utf8_lossy(&output.stderr));
    }

    // 如果 --help 退出码非 0 且输出为空，尝试 help 子命令
    if help_text.trim().is_empty() {
        let alt_output = build_cli_command(cli_path)
            .arg("help")
            .output()
            .await
            .map_err(|e| format!("运行 help 失败: {e}"))?;
        help_text = String::from_utf8_lossy(&alt_output.stdout).to_string();
        help_text.push_str(&String::from_utf8_lossy(&alt_output.stderr));
    }

    if help_text.trim().is_empty() {
        return Err("未能获取 --help 输出".to_string());
    }

    // 解析
    let parsed = parse_help_output(&help_text);

    // 生成 SKILL.md（含 source_cli_path）
    let skill_md_content = generate_skill_md(&parsed, tool_name, Some(cli_path));

    // 保存
    let skill_dir = crate::infra::utils::paths::skills_global_dir().join(tool_name);
    std::fs::create_dir_all(&skill_dir).map_err(|e| format!("创建 Skill 目录失败: {e}"))?;
    let skill_md_path = skill_dir.join("SKILL.md");
    std::fs::write(&skill_md_path, &skill_md_content)
        .map_err(|e| format!("写入 SKILL.md 失败: {e}"))?;

    tracing::info!(
        tool = %tool_name,
        subcommands = parsed.subcommands.len(),
        options = parsed.options.len(),
        saved = %skill_md_path.display(),
        source_cli = %cli_path,
        "CLI 能力识别完成"
    );

    Ok(CliRecognitionResult {
        tool_name: tool_name.to_string(),
        description: parsed.description.clone(),
        subcommands: parsed.subcommands.clone(),
        options: parsed.options.clone(),
        usage_line: parsed.usage_line.clone(),
        skill_md_content,
        saved_path: skill_md_path.display().to_string(),
        source_cli_path: cli_path.to_string(),
    })
}

// ── 单测 ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── parse_help_output ──

    #[test]
    fn parse_git_help_sample() {
        let help = r#"git is a DevOps tool used for source code management.

Usage: git [--version] [--help] [-C <path>] [-c <name>=<value>]

These are common Git commands used in various situations:

  add        Add file contents to the index
  commit     Record changes to the repository
  push       Update remote refs along with associated objects
  clone      Clone a repository into a new directory
  init       Create an empty Git repository or reinitialize an existing one
"#;
        let parsed = parse_help_output(help);
        assert_eq!(
            parsed.description,
            "git is a DevOps tool used for source code management."
        );
        assert!(parsed.usage_line.is_some());
        assert!(parsed.usage_line.as_ref().unwrap().contains("Usage: git"));

        assert_eq!(parsed.subcommands.len(), 5);
        assert_eq!(parsed.subcommands[0].name, "add");
        assert_eq!(
            parsed.subcommands[0].description,
            "Add file contents to the index"
        );
        assert_eq!(parsed.subcommands[1].name, "commit");
        assert_eq!(parsed.subcommands[2].name, "push");
        assert_eq!(parsed.subcommands[3].name, "clone");
        assert_eq!(parsed.subcommands[4].name, "init");
    }

    #[test]
    fn parse_docker_help_sample() {
        let help = r#"Usage:  docker [OPTIONS] COMMAND

A self-sufficient runtime for containers

Common Commands:
  run         Create and run a new container from an image
  exec        Run a command in a running container
  ps          List containers
  build       Build an image from a Dockerfile
  pull        Download an image from a registry

Options:
  -v, --version          Print version information and quit
  -H, --host list        Daemon socket(s) to connect to
      --config string    Location of client config files (default "~/.docker")
"#;
        let parsed = parse_help_output(help);
        assert!(parsed.usage_line.is_some());
        assert!(
            parsed
                .usage_line
                .as_ref()
                .unwrap()
                .contains("Usage:  docker")
        );

        assert!(parsed.subcommands.len() >= 5);
        assert_eq!(parsed.subcommands[0].name, "run");
        assert_eq!(
            parsed.subcommands[0].description,
            "Create and run a new container from an image"
        );

        assert!(parsed.options.len() >= 3);
        assert_eq!(parsed.options[0].flags, "-v, --version");
        assert_eq!(
            parsed.options[0].description,
            "Print version information and quit"
        );
        assert_eq!(parsed.options[2].flags, "--config string");
        assert_eq!(
            parsed.options[2].description,
            "Location of client config files (default \"~/.docker\")"
        );
    }

    #[test]
    fn parse_empty_output() {
        let parsed = parse_help_output("");
        assert!(parsed.description.is_empty());
        assert!(parsed.subcommands.is_empty());
        assert!(parsed.options.is_empty());
        assert!(parsed.usage_line.is_none());
    }

    #[test]
    fn parse_no_subcommands_no_options() {
        let help = "A simple tool\nUsage: simple\nThat's it.";
        let parsed = parse_help_output(help);
        assert_eq!(parsed.description, "A simple tool");
        assert!(parsed.subcommands.is_empty());
        assert_eq!(parsed.usage_line.as_deref(), Some("Usage: simple"));
    }

    #[test]
    fn parse_case_insensitive_usage() {
        let help = "usage: mytool [options]\nMy tool description";
        let parsed = parse_help_output(help);
        assert_eq!(
            parsed.usage_line.as_deref(),
            Some("usage: mytool [options]")
        );
    }

    // ── parse_subcommand_line ──

    #[test]
    fn subcommand_basic() {
        let cmd = parse_subcommand_line("  add        Add file contents to the index").unwrap();
        assert_eq!(cmd.name, "add");
        assert_eq!(cmd.description, "Add file contents to the index");
    }

    #[test]
    fn subcommand_single_word_no_desc() {
        // 只有 name 没有描述 → None
        assert!(parse_subcommand_line("  add").is_none());
    }

    #[test]
    fn subcommand_not_indented() {
        // 不以 2+ 空格开头 → None
        assert!(parse_subcommand_line("add  Add stuff").is_none());
    }

    #[test]
    fn subcommand_starts_with_dash() {
        // 以 - 开头是选项行 → None
        assert!(parse_subcommand_line("  -v, --verbose  Verbose output").is_none());
    }

    // ── parse_option_line ──

    #[test]
    fn option_short_and_long() {
        let opt = parse_option_line("  -v, --verbose    Increase output verbosity").unwrap();
        assert_eq!(opt.flags, "-v, --verbose");
        assert_eq!(opt.description, "Increase output verbosity");
    }

    #[test]
    fn option_long_only() {
        let opt = parse_option_line("      --config FILE  Configuration file path").unwrap();
        assert_eq!(opt.flags, "--config FILE");
        assert_eq!(opt.description, "Configuration file path");
    }

    #[test]
    fn option_not_indented() {
        assert!(parse_option_line("-v  Verbose").is_none());
    }

    #[test]
    fn option_no_description() {
        let opt = parse_option_line("  --verbose").unwrap();
        assert_eq!(opt.flags, "--verbose");
        assert!(opt.description.is_empty());
    }

    // ── generate_skill_md ──

    #[test]
    fn generate_skill_md_has_frontmatter() {
        let parsed = ParsedHelp {
            description: "Git version control".to_string(),
            subcommands: vec![CliCommand {
                name: "add".to_string(),
                description: "Add files".to_string(),
            }],
            options: vec![CliOption {
                flags: "-v, --verbose".to_string(),
                description: "Verbose".to_string(),
            }],
            usage_line: Some("Usage: git [options]".to_string()),
        };
        let md = generate_skill_md(&parsed, "git", None);
        assert!(md.starts_with("---\n"));
        assert!(md.contains("name: git-cli"));
        assert!(md.contains("description: Git version control"));
        assert!(md.contains("keywords: [git, add]"));
        assert!(md.contains("priority: normal"));
        assert!(md.contains("# Git 命令行工具"));
        assert!(md.contains("⚠️ 此 Skill 由 Blink CLI 能力识别自动生成"));
        assert!(md.contains("Usage: git [options]"));
        assert!(md.contains("## 子命令"));
        assert!(md.contains("| add | Add files |"));
        assert!(md.contains("## 全局选项"));
        assert!(md.contains("| -v, --verbose | Verbose |"));
    }

    #[test]
    fn generate_skill_md_empty_subcommands() {
        let parsed = ParsedHelp {
            description: "Simple tool".to_string(),
            subcommands: vec![],
            options: vec![],
            usage_line: None,
        };
        let md = generate_skill_md(&parsed, "simple", None);
        assert!(md.contains("name: simple-cli"));
        assert!(!md.contains("## 子命令"));
        assert!(!md.contains("## 全局选项"));
        assert!(!md.contains("## 用法"));
    }

    #[test]
    fn generate_skill_md_escapes_pipe_in_table() {
        let parsed = ParsedHelp {
            description: "Test".to_string(),
            subcommands: vec![CliCommand {
                name: "add".to_string(),
                description: "Add | remove files".to_string(),
            }],
            options: vec![],
            usage_line: None,
        };
        let md = generate_skill_md(&parsed, "test", None);
        assert!(md.contains("Add \\| remove files"));
    }

    #[test]
    fn capitalize_first_letter() {
        assert_eq!(capitalize("git"), "Git");
        assert_eq!(capitalize("docker"), "Docker");
        assert_eq!(capitalize(""), "");
    }

    // ── recognize_cli 自检（排除 Blink 自身）──

    #[tokio::test]
    async fn recognize_cli_rejects_blink_itself() {
        // 传入当前进程自身的 exe 路径，应被自检拦截，返回明确错误而非启动子进程。
        // 这避免了「识别 blink.exe 时被单实例插件拦截 → 唤起主窗口 + 无 stdout」的问题。
        let self_exe = std::env::current_exe().expect("无法获取当前 exe 路径");
        let result = recognize_cli(&self_exe.to_string_lossy()).await;
        assert!(result.is_err(), "识别 Blink 自身应返回错误");
        let err = result.unwrap_err();
        assert!(
            err.contains("Blink 自身") || err.contains("单实例"),
            "错误信息应说明是 Blink 自身/单实例冲突，实际: {err}"
        );
    }

    #[tokio::test]
    async fn recognize_cli_nonexistent_path() {
        let result = recognize_cli("Z:\\nonexistent\\path\\notreal.exe").await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("不存在"));
    }
}
