//! blink CLI help → skill 探测 e2e 测试（0.25.4 自 domain::ai::skill 迁入 bin）。
//!
//! 用**真实的** clap `Cli::command().render_help()` 输出走完整 skill 链路——
//! 测试对象是 bin 自己的 CLI 定义，天然属于 bin 层（domain crate 无法引用 cli）。

#[cfg(test)]
mod tests {
    use clap::CommandFactory;

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
            let skill = crate::domain::ai::skill::parse_skill_md(
                &skill_md,
                crate::domain::ai::skill::SkillSource::Blink,
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
            let registry = crate::domain::ai::skill::SkillRegistry::new();
            registry.inject_entries(vec![skill.clone()]);

            let summaries = registry.summaries();
            assert_eq!(summaries.len(), 1);
            assert_eq!(summaries[0].name, "blink-cli");
            assert!(summaries[0].has_triggers);

            // 6. 用用户消息触发——消息包含 "blink" 关键词
            let matched = registry.match_triggers("帮我用 blink 搜索应用");
            assert_eq!(matched.len(), 1);
            assert_eq!(matched[0].name, "blink-cli");
            assert_eq!(matched[0].source, crate::domain::ai::skill::SkillSource::Blink);

            // 7. 验证触发的 skill 可注入 preamble
            use crate::domain::ai::prompt::ToolSourceSummary;
            use crate::domain::ai::prompt::chat_system_prompt_with_skills;
            let prompt = chat_system_prompt_with_skills(
                None,
                &summaries,
                &matched,
                &ToolSourceSummary::default(),
            );
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
