//! 提示词库命令（0.27.5，phase §3.7 / §5.5 / §8.1）。
//!
//! 命令注册与权限校验都在本层：scope/长度/builtin/expected_revision 由后端
//! 强制（§5.5.2），任何变更广播 `blink://prompt-library-changed`（payload 含
//! scope），设置页/Chat/Editor 订阅刷新缓存。种子落库在 main.rs 启动时执行
//! （domain 目录 → infra 行存储）。

use blink_infra::data::prompt_library::{PromptScope, PromptStoreError};
use tauri::{Emitter, Manager};

use crate::domain::event_names::EventNames;
use crate::domain::prompt_library;

/// 行投影（前端消费形状；含有效内容解析结果与覆盖标记）。
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptEntryView {
    id: String,
    name: String,
    /// 内置=随版本默认内容；自建=用户正文。
    content: String,
    /// 内置条目的用户覆盖（None=未覆盖）。
    override_content: Option<String>,
    /// 有效内容（内置未覆盖=默认、已覆盖=override、自建=content）。
    effective_content: String,
    scope: PromptScope,
    builtin: bool,
    hidden: bool,
    sort_order: i64,
    revision: i64,
}

fn to_view(row: &blink_infra::data::prompt_library::PromptLibraryRow) -> PromptEntryView {
    PromptEntryView {
        id: row.id.clone(),
        name: row.name.clone(),
        content: row.content.clone(),
        override_content: row.override_content.clone(),
        effective_content: prompt_library::effective_content_of_row(row).to_string(),
        scope: row.scope,
        builtin: row.builtin,
        hidden: row.hidden,
        sort_order: row.sort_order,
        revision: row.revision,
    }
}

/// 存储错误 → CommandError code（前端按 code 分类展示冲突/锁内置/无效）。
fn map_store_error(e: PromptStoreError) -> crate::app::command_error::CommandError {
    let code = match &e {
        PromptStoreError::NotFound => "prompt_not_found",
        PromptStoreError::Conflict => "prompt_conflict",
        PromptStoreError::BuiltinLocked => "prompt_builtin_locked",
        PromptStoreError::NotBuiltin => "prompt_not_builtin",
        PromptStoreError::Db(_) => "prompt_store_failed",
    };
    crate::app::command_error::CommandError::new(code, e.to_string(), false)
}

fn pool(app: &tauri::AppHandle) -> sqlx::SqlitePool {
    app.state::<crate::infra::data::DbPools>().ai.clone()
}

/// 广播变更（payload 含 scope，订阅方按需刷新）。
fn emit_changed(app: &tauri::AppHandle, scope: PromptScope) {
    if let Err(e) = app.emit(
        EventNames::PROMPT_LIBRARY_CHANGED,
        serde_json::json!({ "scope": scope.as_str() }),
    ) {
        tracing::debug!(%e, "emit prompt-library-changed 失败");
    }
}

/// 列出条目（`scope` 为 None 时全量；隐藏条目也返回，由前端标注）。
#[tauri::command]
pub async fn prompt_library_list(
    app: tauri::AppHandle,
    scope: Option<String>,
) -> Result<Vec<PromptEntryView>, crate::app::command_error::CommandError> {
    let scope = scope
        .map(|s| PromptScope::parse(&s).map_err(|e| {
            crate::app::command_error::CommandError::new("prompt_invalid", e, false)
        }))
        .transpose()?;
    let rows = blink_infra::data::prompt_library::list(&pool(&app), scope)
        .await
        .map_err(map_store_error)?;
    Ok(rows.iter().map(to_view).collect())
}

/// upsert 载荷。
#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptUpsertInput {
    /// None = 新建自建条目（id 由后端生成 UUID）；Some = 更新既有条目。
    pub id: Option<String>,
    pub name: String,
    /// 自建条目的正文（内置条目忽略该字段，content 由目录种子升级）。
    pub content: Option<String>,
    /// 内置条目的用户覆盖；Some(None) = 清除覆盖（等价 reset）。
    #[serde(default)]
    pub override_content: Option<Option<String>>,
    /// 新建条目的作用域（更新时忽略——scope 不可变，§3.7）。
    pub scope: Option<String>,
    #[serde(default)]
    pub hidden: Option<bool>,
    #[serde(default)]
    pub sort_order: Option<i64>,
    /// 乐观并发版本（更新必填；新建忽略）。
    pub expected_revision: Option<i64>,
}

/// 新建 / 更新条目。
///
/// 校验顺序（§5.5.2）：字段校验（名称/正文长度、scope）→ scope 不可变 →
/// builtin 权限（内置只允许 name/override/hidden/sort）→ revision CAS →
/// 广播。冲突返回 `prompt_conflict`，前端提示重新载入。
#[tauri::command]
pub async fn prompt_library_upsert(
    app: tauri::AppHandle,
    entry: PromptUpsertInput,
) -> Result<PromptEntryView, crate::app::command_error::CommandError> {
    let invalid = |message: String| {
        crate::app::command_error::CommandError::new("prompt_invalid", message, false)
    };

    // ── 新建自建条目 ──
    let Some(id) = entry.id.clone() else {
        let scope = entry
            .scope
            .as_deref()
            .map(PromptScope::parse)
            .transpose()
            .map_err(invalid)?
            .ok_or_else(|| invalid("新建条目必须指定作用域".into()))?;
        let content = entry.content.clone().unwrap_or_default();
        prompt_library::validate_entry(&entry.name, &content).map_err(invalid)?;
        let id = new_custom_id();
        let row = blink_infra::data::prompt_library::insert(
            &pool(&app),
            &id,
            entry.name.trim(),
            content.trim(),
            scope,
            entry.sort_order.unwrap_or(1000),
        )
        .await
        .map_err(map_store_error)?;
        tracing::info!(%id, scope = scope.as_str(), "提示词库：新建自建条目");
        emit_changed(&app, scope);
        return Ok(to_view(&row));
    };

    // ── 更新既有条目 ──
    let existing = blink_infra::data::prompt_library::get(&pool(&app), &id)
        .await
        .map_err(map_store_error)?;
    if let Some(requested) = entry.scope.as_deref()
        && requested != existing.scope.as_str()
    {
        return Err(invalid("作用域不可修改".into()));
    }

    // 内置条目：override/未覆盖默认值参与校验；自建：content 参与校验
    let effective_for_validation = if existing.builtin {
        match entry.override_content.as_ref() {
            Some(Some(r#override)) => r#override.clone(),
            Some(None) => existing.content.clone(),
            None => prompt_library::effective_content_of_row(&existing).to_string(),
        }
    } else {
        entry.content.clone().unwrap_or(existing.content.clone())
    };
    prompt_library::validate_entry(&entry.name, &effective_for_validation).map_err(invalid)?;

    // 自建条目不允许走 override 字段（存储层虽不存，也拒绝防误解）
    if !existing.builtin && entry.override_content.is_some() {
        return Err(invalid("自建条目没有覆盖概念，请直接修改正文".into()));
    }
    // 内置条目不允许改 content（存储层同样强制）
    if existing.builtin && entry.content.is_some() {
        return Err(invalid("内置条目的默认内容随版本升级，不能直接修改".into()));
    }

    let expected = entry
        .expected_revision
        .ok_or_else(|| invalid("更新必须携带版本号（expectedRevision）".into()))?;
    let row = blink_infra::data::prompt_library::update(
        &pool(&app),
        &id,
        entry.name.trim(),
        entry.content.as_deref().map(str::trim),
        entry
            .override_content
            .as_ref()
            .map(|value| value.as_deref().map(str::trim)),
        entry.hidden,
        entry.sort_order,
        Some(expected),
    )
    .await
    .map_err(map_store_error)?;
    tracing::info!(%id, builtin = row.builtin, "提示词库：条目已更新");
    emit_changed(&app, row.scope);
    Ok(to_view(&row))
}

/// 删除条目（内置由存储层强制拒绝）。
#[tauri::command]
pub async fn prompt_library_delete(
    app: tauri::AppHandle,
    id: String,
) -> Result<(), crate::app::command_error::CommandError> {
    let existing = blink_infra::data::prompt_library::get(&pool(&app), &id)
        .await
        .map_err(map_store_error)?;
    blink_infra::data::prompt_library::delete(&pool(&app), &id)
        .await
        .map_err(map_store_error)?;
    tracing::info!(%id, "提示词库：自建条目已删除");
    emit_changed(&app, existing.scope);
    Ok(())
}

/// 恢复默认：清除内置条目的用户覆盖。
#[tauri::command]
pub async fn prompt_library_reset(
    app: tauri::AppHandle,
    id: String,
) -> Result<PromptEntryView, crate::app::command_error::CommandError> {
    let row = blink_infra::data::prompt_library::reset_override(&pool(&app), &id)
        .await
        .map_err(map_store_error)?;
    tracing::info!(%id, "提示词库：内置条目已恢复默认");
    emit_changed(&app, row.scope);
    Ok(to_view(&row))
}

/// 按给定 id 顺序重排（前端拖拽/上移下移产出全量顺序）。
#[tauri::command]
pub async fn prompt_library_reorder(
    app: tauri::AppHandle,
    ordered_ids: Vec<String>,
) -> Result<(), crate::app::command_error::CommandError> {
    blink_infra::data::prompt_library::reorder(&pool(&app), &ordered_ids)
        .await
        .map_err(map_store_error)?;
    tracing::info!(count = ordered_ids.len(), "提示词库：顺序已更新");
    // 顺序影响全部作用域列表，广播两域（订阅方幂等刷新）
    for scope in [PromptScope::Chat, PromptScope::Editor] {
        emit_changed(&app, scope);
    }
    Ok(())
}

/// 自建条目 id 生成（时间戳 + 进程内原子序号；与 sticky generate_id 同惯例）。
fn new_custom_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    format!("custom_{timestamp}_{seq}")
}

/// 启动种子：内置目录落库（main.rs 调用；幂等，§3.7）。
pub async fn seed_builtin_prompts(pool: &sqlx::SqlitePool) {
    let entries: Vec<_> = prompt_library::BUILTIN_CATALOG
        .iter()
        .map(|e| blink_infra::data::prompt_library::SeedEntry {
            id: e.id,
            name: e.default_name,
            content: e.content,
            scope: e.scope,
            sort_order: e.sort_order,
        })
        .collect();
    if let Err(e) = blink_infra::data::prompt_library::seed_builtins(pool, &entries).await {
        tracing::warn!(%e, "提示词库内置种子失败（内置预设将不可用，重启自动重试）");
    }
}
