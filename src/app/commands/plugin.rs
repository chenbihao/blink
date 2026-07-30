//! plugin 域命令（0.14.6 §2.4 从 commands.rs 拆分）。

use super::ai::cs_count_skills;
use super::stt::copy_dir_recursive;
use tauri::Manager;

/// 获取所有已加载插件的信息（设置页用）。已含 enabled + settings（0.5.1）。
#[tauri::command]
pub async fn get_plugins(app: tauri::AppHandle) -> Vec<serde_json::Value> {
    let engine = app.state::<std::sync::Arc<crate::domain::plugin::PluginEngine>>();
    // 读当前语言,供 manifest 配置文案按 locale 取值(设置页中英双语)
    let pool = &app.state::<crate::infra::data::DbPools>().config;
    let lang = crate::app::config::get_config(&pool).await.language;
    engine.list_plugins(&lang)
}

/// 列出所有已发现的 Skill 条目（设置页展示用）。
///
/// 0.13.6: 返回带 `disabled` 标记的 SkillEntryWithStatus，前端用此渲染复选框。
#[tauri::command]
pub async fn list_skills(
    app: tauri::AppHandle,
) -> Vec<crate::domain::ai::skill::SkillEntryWithStatus> {
    use tauri::Manager;
    let chat = app.try_state::<std::sync::Arc<crate::domain::ai::chat_service::ChatService>>();
    match chat {
        Some(cs) => cs.skill_registry().all_with_status(),
        None => Vec::new(),
    }
}

/// 手动刷新 Skill 注册表——重新扫描所有启用的来源目录。
///
/// 从当前 AIConfig 读取 `skill_config.enabled_sources()`，调用 `ChatService::refresh_skills`。
/// 设置页「刷新」按钮调用。
#[tauri::command]
pub async fn refresh_skills(app: tauri::AppHandle) -> Result<usize, String> {
    use tauri::Manager;
    let pools = app.state::<crate::infra::data::DbPools>();
    let ai_config =
        crate::app::config::ConfigStore::get::<crate::app::ai_config::AIConfig>(&pools.config)
            .await;

    if !ai_config.chat_config.skill_config.enabled {
        return Ok(0);
    }

    let sources = ai_config.chat_config.skill_config.enabled_sources();
    let chat = app
        .try_state::<std::sync::Arc<crate::domain::ai::chat_service::ChatService>>()
        .ok_or("ChatService 未初始化")?;
    chat.refresh_skills(&sources);
    // 0.13.6: 同步 disabled_skills
    chat.update_skill_disabled(ai_config.chat_config.skill_config.disabled_skills.clone());
    let count = cs_count_skills(&chat);
    tracing::info!(count, "Skill 注册表已手动刷新");
    Ok(count)
}

/// 打开指定来源的 Skill 目录（Explorer）。
///
/// 目录不存在时自动创建（方便用户放入 SKILL.md）。
/// `source` 参数："blink" / "claude" / "zcode"。
#[tauri::command]
pub async fn open_skill_dir(source: String) -> Result<(), String> {
    let skill_source = match source.as_str() {
        "blink" => crate::domain::ai::skill::SkillSource::Blink,
        "claude" => crate::domain::ai::skill::SkillSource::Claude,
        "zcode" => crate::domain::ai::skill::SkillSource::Zcode,
        _ => return Err(format!("未知的 Skill 来源: {source}")),
    };

    let dir = skill_source
        .directory()
        .ok_or_else(|| "无法解析 Skill 目录路径".to_string())?;

    // 目录不存在时先创建，避免 explorer 打开默认位置
    if !dir.exists() {
        std::fs::create_dir_all(&dir).map_err(|e| format!("创建 Skill 目录失败: {e}"))?;
    }

    std::process::Command::new("explorer.exe")
        .arg(&dir)
        .spawn()
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// 0.13.7: 枚举外部 Skill 来源（Claude / Codex / OpenCode / ZCode）。
///
/// 供设置页「导入 Skill」面板：下拉选应用 → 展示该应用目录下可导入的 skill 列表。
/// 返回每个来源的目录路径、是否存在、及其下已发现的 skill 概要（name/dir/description）。
#[tauri::command]
pub async fn list_external_skill_sources() -> Vec<crate::domain::ai::skill::ExternalSkillSourceInfo>
{
    crate::domain::ai::skill::list_external_sources()
}

/// 0.13.6: 导入 Skill 到 Blink 目录。
///
/// `source_path` = 源 SKILL.md 所在目录
/// `mode` = "symlink" | "copy"
/// 导入后在 `%APPDATA%\blink\skills\<name>\` 创建软链接或副本。
/// 软链接失败时自动降级为 Copy + 提示。
#[tauri::command]
pub async fn import_skill(source_path: String, mode: String) -> Result<String, String> {
    use crate::domain::ai::skill::SkillSource;

    let source_dir = std::path::Path::new(&source_path);
    if !source_dir.is_dir() {
        return Err(format!("源目录不存在: {source_path}"));
    }

    // 从目录名提取 Skill 名称
    let skill_name = source_dir
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or("无法从路径提取 Skill 名称")?;

    // 目标目录：%APPDATA%\blink\skills\<name>\
    let target_dir = SkillSource::Blink
        .directory()
        .ok_or("无法解析 Blink Skill 目录路径")?
        .join(skill_name);

    // 如果目标已存在，返回错误
    if target_dir.exists() {
        return Err(format!(
            "目标目录已存在: {}（请先删除或重命名）",
            target_dir.display()
        ));
    }

    // 确保父目录存在
    if let Some(parent) = target_dir.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("创建父目录失败: {e}"))?;
    }

    match mode.as_str() {
        "symlink" => {
            // 尝试创建符号链接
            match std::os::windows::fs::symlink_dir(&source_dir, &target_dir) {
                Ok(()) => {
                    tracing::info!(
                        skill = %skill_name,
                        source = %source_dir.display(),
                        target = %target_dir.display(),
                        "Skill 已通过软链接导入"
                    );
                    Ok(format!(
                        "Skill '{skill_name}' 已通过软链接导入到 Blink 目录。\n双向同步：源目录变更自动反映。"
                    ))
                }
                Err(e) => {
                    // 降级为 Copy
                    tracing::warn!(
                        error = %e,
                        "软链接创建失败，降级为 Copy"
                    );
                    copy_dir_recursive(source_dir, &target_dir)?;
                    Ok(format!(
                        "软链接创建失败（{e}）。已降级为复制。\n如需软链接，请在 Windows 设置 → 隐私和安全性 → 开发者选项 中开启开发者模式。\nSkill '{skill_name}' 已通过复制导入到 Blink 目录。"
                    ))
                }
            }
        }
        "copy" | _ => {
            copy_dir_recursive(source_dir, &target_dir)?;
            tracing::info!(
                skill = %skill_name,
                source = %source_dir.display(),
                target = %target_dir.display(),
                "Skill 已通过复制导入"
            );
            Ok(format!(
                "Skill '{skill_name}' 已通过复制导入到 Blink 目录。"
            ))
        }
    }
}

/// 0.13.6: 设置单个 Skill 的启用/禁用状态。
///
/// `skill_id` 格式：`name@source`（如 `"rust-debug@claude"`）。
/// 更新 AIConfig.skill_config.disabled_skills 并同步到运行时 SkillRegistry。
#[tauri::command]
pub async fn set_skill_enabled(
    app: tauri::AppHandle,
    skill_id: String,
    enabled: bool,
) -> Result<(), String> {
    use tauri::Manager;
    let pools = app.state::<crate::infra::data::DbPools>();
    let mut ai_config =
        crate::app::config::ConfigStore::get::<crate::app::ai_config::AIConfig>(&pools.config)
            .await;

    // 更新 disabled_skills
    let mut disabled = ai_config.chat_config.skill_config.disabled_skills.clone();
    if enabled {
        disabled.retain(|id| id != &skill_id);
    } else if !disabled.contains(&skill_id) {
        disabled.push(skill_id);
    }
    ai_config.chat_config.skill_config.disabled_skills = disabled;

    // 持久化
    crate::app::config::ConfigStore::set(&pools.config, &ai_config)
        .await
        .map_err(|e| e.to_string())?;

    // 同步到运行时
    if let Some(chat) =
        app.try_state::<std::sync::Arc<crate::domain::ai::chat_service::ChatService>>()
    {
        chat.update_skill_disabled(ai_config.chat_config.skill_config.disabled_skills.clone());
    }

    Ok(())
}

/// 保存编辑后的 SKILL.md 内容到指定 skill 目录。
#[tauri::command]
pub async fn save_skill_md(skill_dir: String, content: String) -> Result<(), String> {
    let dir = std::path::Path::new(&skill_dir);
    if !dir.exists() {
        std::fs::create_dir_all(dir).map_err(|e| format!("创建 Skill 目录失败: {e}"))?;
    }
    let skill_md_path = dir.join("SKILL.md");
    std::fs::write(&skill_md_path, &content).map_err(|e| format!("写入 SKILL.md 失败: {e}"))?;
    tracing::info!(path = %skill_md_path.display(), "SKILL.md 已保存");
    Ok(())
}

/// 读取指定 skill 目录中的 SKILL.md 内容（编辑用）。
#[tauri::command]
pub async fn get_skill_content(skill_dir: String) -> Result<String, String> {
    let skill_md_path = std::path::Path::new(&skill_dir).join("SKILL.md");
    if !skill_md_path.exists() {
        return Err("SKILL.md 不存在".to_string());
    }
    std::fs::read_to_string(&skill_md_path).map_err(|e| format!("读取 SKILL.md 失败: {e}"))
}

/// 删除指定 skill 目录（包含 SKILL.md 及同目录资源）。
#[tauri::command]
pub async fn delete_skill(skill_dir: String) -> Result<(), String> {
    let dir = std::path::Path::new(&skill_dir);
    if !dir.exists() {
        return Ok(()); // 已删除，幂等
    }
    std::fs::remove_dir_all(dir).map_err(|e| format!("删除 Skill 目录失败: {e}"))?;
    tracing::info!(dir = %dir.display(), "Skill 目录已删除");
    Ok(())
}
