//! 主窗口列表偏好与一次性迁移；运行时只消费 app.search。
use blink_infra::data::config::{compare_set_configs, config_value};
use serde_json::{Value, json};
use sqlx::SqlitePool;

use super::{ConfigStore, SearchConfig, WheelMode};

pub async fn update_wheel_mode(
    pool: &SqlitePool,
    mode: WheelMode,
    expected: Option<WheelMode>,
) -> Result<(), String> {
    ConfigStore::update::<SearchConfig>(pool, |cfg| {
        if expected.is_some_and(|old| old != cfg.wheel_mode) {
            return Err("config_conflict: 主窗口滚轮行为已变化，请重新读取后重试".into());
        }
        cfg.wheel_mode = mode;
        Ok(())
    })
    .await
    .map(|_| ())
}

fn object(raw: Option<&str>, key: &str) -> Result<Value, String> {
    let value: Value = raw
        .map(serde_json::from_str)
        .transpose()
        .map_err(|e| format!("配置 {key} 无效，未迁移: {e}"))?
        .unwrap_or(json!({}));
    if !value.is_object() {
        return Err(format!("配置 {key} 必须为对象，未迁移"));
    }
    Ok(value)
}

/// 必须在初始化时、分片默认值落库之前执行。双分片 CAS 保证并发与事务安全。
pub async fn migrate_wheel_mode(pool: &SqlitePool) -> Result<(), String> {
    for _ in 0..16 {
        let search_raw = config_value(pool, "app.search")
            .await
            .map_err(|e| e.to_string())?;
        let clip_raw = config_value(pool, "clipboard:config")
            .await
            .map_err(|e| e.to_string())?;
        let mut search = object(search_raw.as_deref(), "app.search")?;
        let mut clip = object(clip_raw.as_deref(), "clipboard:config")?;
        // 除旧滚轮字段外，损坏的已知配置也不得通过迁移写回。
        let old_mode = clip.as_object_mut().unwrap().remove("wheel_mode");
        serde_json::from_value::<blink_infra::data::clipboard::ClipboardConfig>(clip.clone())
            .map_err(|e| format!("剪贴板配置无效，未迁移: {e}"))?;
        serde_json::from_value::<SearchConfig>(search.clone())
            .map_err(|e| format!("主窗口配置无效，未迁移: {e}"))?;
        if search.get("wheel_mode").is_some() && old_mode.is_none() {
            return Ok(());
        }
        if search_raw.is_none() && clip_raw.is_none() {
            return Ok(());
        }
        let fallback = old_mode
            .as_ref()
            .filter(|v| *v == "step" || *v == "page")
            .cloned();
        if old_mode.is_some() && fallback.is_none() {
            tracing::warn!("旧剪贴板滚轮偏好无效，将移除；主窗口缺省时使用逐条模式");
        }
        if search.get("wheel_mode").is_none() {
            search["wheel_mode"] = fallback.unwrap_or(json!("step"));
        }
        // 两个基线都检查；即使其中一份内容未变，也不能漏掉并发写入。
        let mut writes = vec![("app.search".into(), search_raw, search.to_string())];
        writes.push(("clipboard:config".into(), clip_raw, clip.to_string()));
        if compare_set_configs(pool, &writes)
            .await
            .map_err(|e| e.to_string())?
        {
            tracing::info!(wheel_mode = %search["wheel_mode"], "滚轮偏好已迁移至主窗口 app.search");
            return Ok(());
        }
    }
    Err("config_conflict: 滚轮偏好迁移遇到并发更新，请重试".into())
}

#[cfg(test)]
mod tests;
