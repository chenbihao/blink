//! 固定可移植字段清单与双向投影（0.28.6，phase §3.5「固定范围不是整个 DB」）。
//!
//! **单一真源**：本文件定义哪些配置项可漫游。提取（`read_raw_local` →
//! `items_from_raw`）与合成（`build_apply_ops`）都从这里走，不遍历原始 KV。
//!
//! ## item key 命名空间
//!
//! | 形态 | 形式 | 例 |
//! |---|---|---|
//! | 分片标量字段 | `{kv}#{field}` | `app.appearance#theme` |
//! | 嵌套对象字段 | `{kv}#{field}.{sub}` | `app.chord#bindings.voice_input`、`app.ai#chat_config.auto_title` |
//! | id 列表条目 | `{kv}#{field}/{id}` | `app.weblinks#entries/gh`、`app.ai#providers/p1` |
//! | id 列表顺序 | `{kv}#{field}$order` | `app.ai#providers$order` |
//! | 插件分片字段 | `plugin:{pid}#{field}` | `plugin:builtin.translate#enabled` |
//! | 提示词库行 | `prompt_library#entry/{id}` | `prompt_library#entry/builtin.editor.tidy` |
//!
//! ## 固定范围（phase §3.5 表逐项落定）
//!
//! - 外观/交互/搜索偏好：appearance{theme,language,window_opacity}、app.search 全部、
//!   app.suggestion 全部、app.disable 三个列表。
//! - 快捷键与入口：app.hotkey 全部、app.chord（开关 + bindings.* 六键）。
//! - 插件：全部 `plugin:{id}` 分片的 enabled/settings/custom_triggers/
//!   disabled_default_triggers（revision 留本机做并发控制；settings 中的密钥在
//!   SQLite 本就只存空串占位，秘密实体在 CM，天然不漫游）。插件分片**删除不传播**
//!   ——卸载是本机安装状态，另一台保留设置为待应用（phase「未装插件先保留」）。
//! - AI 与用户配置资产：app.ai 标量 + chat_config.* + providers 按条目（**剔除
//!   secret_ref**——CM 引用不漫游，应用时保留本机已有引用，不清空本机密钥）；
//!   提示词库表按行（内置行 content 随版本提供不漫游，仅同步 name/覆盖正文/
//!   排序/隐藏）。
//! - 超链/自定义搜索：app.weblinks 按条目 id + 顺序；app.bookmarks 整分片。
//!
//! 留本机：auto_start/log_level/ai_http_body_log/onboarding_version、network、
//! stt:config、app.update、app.ai_permission、ai.capability_access、
//! screenshot:config、context:config、engine:*、clipboard:config、历史/剪贴板内容。
//!
//! ## 应用路由
//!
//! 合并结果不走直写 KV——`build_apply_ops` 把每个变化分片合成为既有
//! `set_config` 键的载荷（复用热键冲突/关键词冲突/网络校验与全部运行时副作用），
//! 由 bin 侧 `SyncLocalEnv::apply_ops` 逐个执行；失败转本机覆盖记录。

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use super::model::ItemMap;
use super::SyncError;

/// 提示词库伪分片 key（表存储，不在 config KV 中）。
pub const PROMPT_SHARD: &str = "prompt_library";
/// 提示词行 item 前缀。
const PROMPT_PREFIX: &str = "prompt_library#entry/";

/// 本机原始投影——冻结计划（CAS 预检）与合成应用操作的输入。
#[derive(Debug, Clone, Default)]
pub struct RawLocal {
    /// config KV 原始 JSON（9 个固定分片 + 全部 plugin:{id}）。
    pub shards: BTreeMap<String, Value>,
    /// 提示词库行投影（id → 行字段对象）。
    pub prompts: BTreeMap<String, Value>,
}

/// 单条应用操作（`build_apply_ops` 产出，bin 侧执行）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ApplyOp {
    /// 走既有 `set_config` 校验/副作用链。`value` 为该 key 的完整载荷。
    SetConfig {
        shard: String,
        key: String,
        value: Value,
        items: Vec<String>,
    },
    /// 插件分片——`plugin_config` 键 + engine.update_config（revision CAS）。
    PluginConfig {
        shard: String,
        plugin_id: String,
        enabled: bool,
        settings: Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        custom_triggers: Option<Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        disabled_default_triggers: Option<Value>,
        expected_revision: u64,
        items: Vec<String>,
    },
    /// 提示词库行落库（无 CAS；revision 自增）。
    PromptUpsert {
        shard: String,
        id: String,
        row: Value,
        item: String,
    },
    /// 提示词库行删除（内置条目会被存储层拒绝 → 转本机覆盖）。
    PromptDelete { shard: String, id: String, item: String },
}

/// 应用单条操作的结果（bin 侧回填，驱动本机覆盖记录）。
#[derive(Debug, Clone, PartialEq)]
pub struct AppliedItemOutcome {
    pub shard: String,
    pub items: Vec<String>,
    pub error: Option<String>,
}

// ── 固定分片清单 ─────────────────────────────────────────────────────────────

/// 固定 KV 分片（除 plugin:* 动态分片与提示词库表外）。
const FIXED_SHARDS: &[&str] = &[
    "app.appearance",
    "app.hotkey",
    "app.search",
    "app.suggestion",
    "app.chord",
    "app.disable",
    "app.weblinks",
    "app.bookmarks",
    "app.ai",
];

/// appearance 中可漫游的字段（其余 auto_start/log_level/ai_http_body_log/
/// onboarding_version 留本机）。
const APPEARANCE_PORTABLE: &[&str] = &["theme", "language", "window_opacity"];

/// 需要进入合并前「集合语义归一」（排序去重）的 item key 后缀。
fn is_set_item(key: &str) -> bool {
    key == "app.disable#disabled_builtin_actions"
        || key == "app.disable#disabled_context_bindings"
        || key == "app.disable#disabled_chord_actions"
        || key.ends_with("#disabled_default_triggers")
}

/// 集合语义归一：字符串数组排序去重后比较（disable 列表语义上是集合）。
pub(crate) fn normalize_set_value(value: &Value) -> Value {
    let Some(arr) = value.as_array() else {
        return value.clone();
    };
    let mut strings: Vec<String> = arr
        .iter()
        .filter_map(|v| v.as_str().map(str::to_string))
        .collect();
    strings.sort();
    strings.dedup();
    Value::Array(strings.into_iter().map(Value::String).collect())
}

/// item 归一（比较用）：集合 item 归一，其余原样。
pub(crate) fn normalize_item(key: &str, value: &Value) -> Value {
    if is_set_item(key) {
        normalize_set_value(value)
    } else {
        value.clone()
    }
}

// ── 读取本机原始投影 ─────────────────────────────────────────────────────────

/// 读取本机原始投影：固定分片 raw JSON（缺失按类型默认值）、全部 plugin:{id}
/// 分片、提示词库全表。损坏分片（JSON 解析失败）报错不静默默认——
/// 「损坏配置不被默认成空配置上传」。
pub async fn read_raw_local(pool: &sqlx::SqlitePool) -> Result<RawLocal, SyncError> {
    let mut raw = RawLocal::default();
    let all = blink_infra::data::history::get_all_config(pool).await;
    for &kv in FIXED_SHARDS {
        let value = match all.get(kv) {
            Some(s) => parse_shard(kv, s)?,
            None => default_shard(kv)?,
        };
        raw.shards.insert(kv.to_string(), value);
    }
    for (k, v) in &all {
        if k.starts_with("plugin:") {
            let value = parse_shard(k, v)?;
            if value.is_object() || value.is_null() {
                raw.shards.insert(k.clone(), value);
            }
        }
    }
    // 提示词库行投影。
    for row in blink_infra::data::prompt_library::list(pool, None)
        .await
        .map_err(|e| SyncError::Io {
            message: format!("读取提示词库失败: {e}"),
        })?
    {
        raw.prompts.insert(row.id.clone(), prompt_row_item(&row));
    }
    Ok(raw)
}

fn parse_shard(kv: &str, raw: &str) -> Result<Value, SyncError> {
    serde_json::from_str(raw).map_err(|e| SyncError::CorruptLocal {
        shard: format!("{kv}: {e}"),
    })
}

/// 分片 KV 缺失时的类型默认值（运行时读缺省分片同样得到默认）。
fn default_shard(kv: &str) -> Result<Value, SyncError> {
    use crate::config::ai_config::AIConfig;
    use crate::config::{
        AppearanceConfig, BookmarksConfig, ChordConfig, DisableConfig, HotkeyConfig, SearchConfig,
        SuggestionConfig, WebLinksConfig,
    };
    let value = match kv {
        "app.appearance" => serde_json::to_value(AppearanceConfig::default()),
        "app.hotkey" => serde_json::to_value(HotkeyConfig::default()),
        "app.search" => serde_json::to_value(SearchConfig::default()),
        "app.suggestion" => serde_json::to_value(SuggestionConfig::default()),
        "app.chord" => serde_json::to_value(ChordConfig::default()),
        "app.disable" => serde_json::to_value(DisableConfig::default()),
        "app.weblinks" => serde_json::to_value(WebLinksConfig::default()),
        "app.bookmarks" => serde_json::to_value(BookmarksConfig::default()),
        "app.ai" => serde_json::to_value(AIConfig::default()),
        _ => {
            return Ok(Value::Null);
        }
    };
    value.map_err(|e| SyncError::Unknown {
        message: format!("默认分片序列化失败: {e}"),
    })
}

/// 提示词行 → item 值。内置行 content 随版本提供，不漫游。
fn prompt_row_item(row: &blink_infra::data::prompt_library::PromptLibraryRow) -> Value {
    let mut m = Map::new();
    m.insert("name".into(), Value::String(row.name.clone()));
    if let Some(over) = &row.override_content {
        m.insert("override_content".into(), Value::String(over.clone()));
    }
    m.insert("scope".into(), Value::String(row.scope.as_str().into()));
    m.insert("builtin".into(), Value::Bool(row.builtin));
    m.insert("sort_order".into(), Value::from(row.sort_order));
    m.insert("hidden".into(), Value::Bool(row.hidden));
    if !row.builtin {
        m.insert("content".into(), Value::String(row.content.clone()));
    }
    Value::Object(m)
}

// ── 原始投影 → item map（纯函数，提取与合成共用）────────────────────────────

/// 从原始投影提取 item map（含对 app.ai 剔除 secret_ref）。
pub fn items_from_raw(raw: &RawLocal) -> Result<ItemMap, SyncError> {
    let mut items = ItemMap::new();
    for (kv, value) in &raw.shards {
        let Some(obj) = value.as_object() else {
            continue;
        };
        match kv.as_str() {
            "app.appearance" => {
                for field in APPEARANCE_PORTABLE {
                    copy_field(&mut items, kv, obj, field);
                }
            }
            "app.chord" => {
                for (field, fv) in obj {
                    if field == "bindings" {
                        if let Some(bindings) = fv.as_object() {
                            for (sub, sv) in bindings {
                                items.insert(format!("{kv}#bindings.{sub}"), sv.clone());
                            }
                        }
                    } else {
                        items.insert(format!("{kv}#{field}"), fv.clone());
                    }
                }
            }
            "app.weblinks" => {
                copy_entries(&mut items, kv, "entries", obj.get("entries"));
                // seeded 是迁移标记，留本机。
            }
            "app.ai" => {
                for (field, fv) in obj {
                    match field.as_str() {
                        "providers" => {
                            copy_entries(&mut items, kv, "providers", obj.get("providers"));
                        }
                        "chat_config" => {
                            if let Some(chat) = fv.as_object() {
                                for (sub, sv) in chat {
                                    items.insert(format!("{kv}#chat_config.{sub}"), sv.clone());
                                }
                            }
                        }
                        _ => {
                            items.insert(format!("{kv}#{field}"), fv.clone());
                        }
                    }
                }
            }
            kv if kv.starts_with("plugin:") => {
                for field in ["enabled", "settings", "custom_triggers", "disabled_default_triggers"]
                {
                    copy_field(&mut items, kv, obj, field);
                }
            }
            // hotkey/search/suggestion/disable/bookmarks：整分片逐字段。
            _ => {
                for (field, fv) in obj {
                    items.insert(format!("{kv}#{field}"), fv.clone());
                }
            }
        }
    }
    // secret_ref 剔除（CM 引用不漫游）。
    let mut stripped = items
        .into_iter()
        .map(|(k, v)| (k, strip_provider_secret_ref(v)))
        .collect::<ItemMap>();
    for (id, row) in &raw.prompts {
        stripped.insert(format!("{PROMPT_PREFIX}{id}"), row.clone());
    }
    Ok(stripped)
}

fn copy_field(items: &mut ItemMap, kv: &str, obj: &Map<String, Value>, field: &str) {
    if let Some(v) = obj.get(field) {
        items.insert(format!("{kv}#{field}"), v.clone());
    }
}

/// id 列表 → 条目 + 顺序 items。
fn copy_entries(items: &mut ItemMap, kv: &str, field: &str, list: Option<&Value>) {
    let Some(arr) = list.and_then(Value::as_array) else {
        return;
    };
    let mut order = Vec::new();
    for entry in arr {
        let Some(id) = entry.get("id").and_then(Value::as_str) else {
            continue;
        };
        items.insert(format!("{kv}#{field}/{id}"), entry.clone());
        order.push(Value::String(id.to_string()));
    }
    items.insert(format!("{kv}#{field}$order"), Value::Array(order));
}

/// provider 条目 item 值剔除 secret_ref 字段（按结构判定——含 id/kind/models
/// 的对象即 provider 条目，不枚举 kind 值避免漂移；其余形态原样返回）。
fn strip_provider_secret_ref(value: Value) -> Value {
    let looks_like_provider = value.get("id").is_some()
        && value.get("kind").is_some()
        && value.get("models").is_some();
    if looks_like_provider && value.is_object() {
        let mut obj = value;
        if let Some(map) = obj.as_object_mut() {
            map.remove("secret_ref");
        }
        return obj;
    }
    value
}

/// 提取本机 item map（池读取 + 投影）。
pub async fn extract_local_items(pool: &sqlx::SqlitePool) -> Result<ItemMap, SyncError> {
    let raw = read_raw_local(pool).await?;
    items_from_raw(&raw)
}

/// item key → 归属分片（plugin:* 动态 / prompt_library / 固定 kv）。
pub fn shard_of_item(key: &str) -> String {
    if let Some(rest) = key.strip_prefix(PROMPT_PREFIX) {
        let _ = rest;
        PROMPT_SHARD.to_string()
    } else if let Some(pos) = key.find('#') {
        key[..pos].to_string()
    } else {
        key.to_string()
    }
}

// ── 合并结果 → 应用操作 ─────────────────────────────────────────────────────

/// 把合并 item map 合成为应用操作序列。只对「相对本机发生变化的分片」产出。
/// `effective_local` 是参与比较的本机视角（覆盖替换后的 L 视图）——本机覆盖
/// 记录里的 item 已被基线值替换，天然不会重复应用失败的值。
pub fn build_apply_ops(
    merged: &ItemMap,
    raw: &RawLocal,
    effective_local: &ItemMap,
) -> Result<Vec<ApplyOp>, SyncError> {
    let local = effective_local;
    // 变化分片集合。
    let mut changed: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut keys = merged.keys().collect::<Vec<_>>();
    keys.extend(local.keys());
    keys.sort();
    keys.dedup();
    for key in keys {
        let m = merged.get(key);
        let l = local.get(key);
        let equal = match (m, l) {
            (Some(mv), Some(lv)) => {
                normalize_item(key, mv) == normalize_item(key, lv)
            }
            (None, None) => true,
            _ => false,
        };
        if !equal {
            changed
                .entry(shard_of_item(key))
                .or_default()
                .push(key.clone());
        }
    }

    let mut ops = Vec::new();
    for (shard, items) in &changed {
        match shard.as_str() {
            PROMPT_SHARD => build_prompt_ops(&items, merged, raw, &mut ops)?,
            kv if kv.starts_with("plugin:") => {
                build_plugin_op(kv, items, merged, raw, &mut ops)?
            }
            "app.appearance" | "app.search" => {
                build_general_ops(merged, raw, &items, &mut ops)
            }
            "app.suggestion" => build_suggestion_op(merged, raw, items, &mut ops),
            "app.hotkey" => build_whole_op("hotkey", "app.hotkey", merged, raw, items, &mut ops),
            "app.chord" => build_chord_ops(merged, raw, items, &mut ops),
            "app.disable" => build_disable_ops(merged, raw, items, &mut ops),
            "app.weblinks" => build_weblinks_op(merged, raw, items, &mut ops),
            "app.bookmarks" => build_whole_op(
                "bookmarks",
                "app.bookmarks",
                merged,
                raw,
                items,
                &mut ops,
            ),
            "app.ai" => build_ai_op(merged, raw, items, &mut ops),
            other => {
                return Err(SyncError::Unknown {
                    message: format!("未知分片 {other} 进入应用合成"),
                })
            }
        }
    }
    Ok(ops)
}

/// 将 items 中属于 `kv` 分片的标量/嵌套字段写回 raw 对象副本
/// （仅覆盖出现的 item；标量缺失不删除本机字段——不靠缺字段猜删除）。
fn overlay_shard_object(kv: &str, merged: &ItemMap, raw: &RawLocal) -> Value {
    let mut obj = raw
        .shards
        .get(kv)
        .cloned()
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default();
    let prefix = format!("{kv}#");
    for (key, value) in merged {
        if let Some(rest) = key.strip_prefix(&prefix) {
            set_nested(&mut obj, rest, value.clone());
        }
    }
    Value::Object(obj)
}

fn set_nested(obj: &mut Map<String, Value>, path: &str, value: Value) {
    match path.split_once('.') {
        None => {
            obj.insert(path.to_string(), value);
        }
        Some((head, tail)) => {
            let entry = obj
                .entry(head.to_string())
                .or_insert_with(|| Value::Object(Map::new()));
            if !entry.is_object() {
                *entry = Value::Object(Map::new());
            }
            if let Some(map) = entry.as_object_mut() {
                set_nested(map, tail, value);
            }
        }
    }
}

/// 整分片单 key 应用（hotkey/bookmarks：对象即载荷）。
fn build_whole_op(
    set_key: &str,
    kv: &str,
    merged: &ItemMap,
    raw: &RawLocal,
    items: &[String],
    ops: &mut Vec<ApplyOp>,
) {
    let value = overlay_shard_object(kv, merged, raw);
    ops.push(ApplyOp::SetConfig {
        shard: kv.to_string(),
        key: set_key.to_string(),
        value,
        items: items.to_vec(),
    });
}

/// appearance#theme + app.search → general_config / language / window_opacity / wheel_mode。
fn build_general_ops(merged: &ItemMap, raw: &RawLocal, items: &[String], ops: &mut Vec<ApplyOp>) {
    let appearance = overlay_shard_object("app.appearance", merged, raw);
    let search = overlay_shard_object("app.search", merged, raw);
    let need_general = items.iter().any(|k| {
        matches!(
            k.as_str(),
            "app.appearance#theme"
                | "app.search#search_history_enabled"
                | "app.search#search_history_days"
                | "app.search#max_results"
                | "app.search#page_size"
        )
    });
    if need_general {
        let value = serde_json::json!({
            "theme": appearance.get("theme").cloned().unwrap_or(Value::String("auto".into())),
            "search_history_enabled": search.get("search_history_enabled").cloned().unwrap_or(Value::Bool(true)),
            "search_history_days": search.get("search_history_days").cloned().unwrap_or(Value::from(30)),
            "max_results": search.get("max_results").cloned().unwrap_or(Value::from(50)),
            "page_size": search.get("page_size").cloned().unwrap_or(Value::from(9)),
        });
        let affected: Vec<String> = items
            .iter()
            .filter(|k| k.starts_with("app.appearance#theme") || k.starts_with("app.search#"))
            .cloned()
            .collect();
        ops.push(ApplyOp::SetConfig {
            shard: "app.search".into(),
            key: "general_config".into(),
            value,
            items: affected,
        });
    }
    push_scalar_op(ops, items, "language", "app.appearance#language", appearance.get("language"));
    push_scalar_op(
        ops,
        items,
        "window_opacity",
        "app.appearance#window_opacity",
        appearance.get("window_opacity"),
    );
    if items.iter().any(|k| k.as_str() == "app.search#wheel_mode") {
        if let Some(v) = merged.get("app.search#wheel_mode") {
            ops.push(ApplyOp::SetConfig {
                shard: "app.search".into(),
                key: "wheel_mode".into(),
                value: v.clone(),
                items: vec!["app.search#wheel_mode".into()],
            });
        }
    }
}

fn push_scalar_op(
    ops: &mut Vec<ApplyOp>,
    items: &[String],
    key: &str,
    item_key: &str,
    value: Option<&Value>,
) {
    if items.iter().any(|k| k == item_key) && let Some(v) = value {
        ops.push(ApplyOp::SetConfig {
            shard: "app.appearance".into(),
            key: key.to_string(),
            value: v.clone(),
            items: vec![item_key.to_string()],
        });
    }
}

/// app.suggestion → autosuggest（全 11 字段，camelCase 载荷）。
fn build_suggestion_op(merged: &ItemMap, raw: &RawLocal, items: &[String], ops: &mut Vec<ApplyOp>) {
    let obj = overlay_shard_object("app.suggestion", merged, raw);
    fn f(obj: &Value, key: &str) -> Value {
        obj.get(key).cloned().unwrap_or(Value::Null)
    }
    let value = serde_json::json!({
        "enabled": f(&obj, "autosuggest_enabled"),
        "minScore": f(&obj, "autosuggest_min_score"),
        "tabKey": f(&obj, "autosuggest_tab_key"),
        "completionEnabled": f(&obj, "completion_enabled"),
        "contextSuggestionEnabled": f(&obj, "context_suggestion_enabled"),
        "aiSuggestionEnabled": f(&obj, "ai_suggestion_enabled"),
        "secondaryEnabled": f(&obj, "secondary_enabled"),
        "suppressRepeated": f(&obj, "suppress_repeated"),
        "proactiveEnabled": f(&obj, "proactive_enabled"),
        "emptyQueryTopn": f(&obj, "empty_query_topn"),
        "secondaryMinRank": f(&obj, "secondary_min_rank"),
    });
    ops.push(ApplyOp::SetConfig {
        shard: "app.suggestion".into(),
        key: "autosuggest".into(),
        value,
        items: items.to_vec(),
    });
}

/// app.chord → chord_toggles（camelCase）+ chord_bindings。
fn build_chord_ops(merged: &ItemMap, raw: &RawLocal, items: &[String], ops: &mut Vec<ApplyOp>) {
    let obj = overlay_shard_object("app.chord", merged, raw);
    let toggles: Vec<&String> = items
        .iter()
        .filter(|k| k.as_str() == "app.chord#chord_enabled"
            || k.as_str() == "app.chord#chord_hint_visible")
        .collect();
    if !toggles.is_empty() {
        ops.push(ApplyOp::SetConfig {
            shard: "app.chord".into(),
            key: "chord_toggles".into(),
            value: serde_json::json!({
                "chordEnabled": obj.get("chord_enabled").cloned().unwrap_or(Value::Bool(true)),
                "chordHintVisible": obj.get("chord_hint_visible").cloned().unwrap_or(Value::Bool(true)),
            }),
            items: toggles.into_iter().cloned().collect(),
        });
    }
    let binding_items: Vec<String> = items
        .iter()
        .filter(|k| k.starts_with("app.chord#bindings."))
        .cloned()
        .collect();
    if !binding_items.is_empty() {
        let bindings = obj
            .get("bindings")
            .cloned()
            .unwrap_or_else(|| Value::Object(Map::new()));
        ops.push(ApplyOp::SetConfig {
            shard: "app.chord".into(),
            key: "chord_bindings".into(),
            value: bindings,
            items: binding_items,
        });
    }
}

/// app.disable → 三个列表 key。
fn build_disable_ops(merged: &ItemMap, raw: &RawLocal, items: &[String], ops: &mut Vec<ApplyOp>) {
    let obj = overlay_shard_object("app.disable", merged, raw);
    for (field, key) in [
        ("disabled_builtin_actions", "disabled_builtin_actions"),
        ("disabled_context_bindings", "disabled_context_bindings"),
        ("disabled_chord_actions", "disabled_chord_actions"),
    ] {
        let item_key = format!("app.disable#{field}");
        if items.iter().any(|k| *k == item_key) && let Some(v) = obj.get(field) {
            ops.push(ApplyOp::SetConfig {
                shard: "app.disable".into(),
                key: key.to_string(),
                value: v.clone(),
                items: vec![item_key],
            });
        }
    }
}

/// app.weblinks → weblinks（条目按合并重建；seeded 保留本机值）。
fn build_weblinks_op(merged: &ItemMap, raw: &RawLocal, items: &[String], ops: &mut Vec<ApplyOp>) {
    let entries = rebuild_entries(merged, raw, "app.weblinks", "entries");
    let seeded = raw
        .shards
        .get("app.weblinks")
        .and_then(|v| v.get("seeded"))
        .cloned()
        .unwrap_or(Value::Bool(false));
    ops.push(ApplyOp::SetConfig {
        shard: "app.weblinks".into(),
        key: "weblinks".into(),
        value: serde_json::json!({ "entries": entries, "seeded": seeded }),
        items: items.to_vec(),
    });
}

/// id 列表重建：合并条目 item + 顺序 item；缺失顺序退化为本机顺序 + 新增追加。
fn rebuild_entries(merged: &ItemMap, raw: &RawLocal, kv: &str, field: &str) -> Value {
    let prefix = format!("{kv}#{field}/");
    let order_key = format!("{kv}#{field}$order");
    let mut entries: Vec<Value> = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    let local_order: Vec<String> = raw
        .shards
        .get(kv)
        .and_then(|v| v.get(field))
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|e| e.get("id").and_then(Value::as_str).map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let order: Vec<String> = merged
        .get(&order_key)
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .filter(|o: &Vec<String>| !o.is_empty())
        .unwrap_or_else(|| local_order.clone());
    for id in &order {
        if let Some(entry) = merged.get(&format!("{prefix}{id}")) {
            entries.push(entry.clone());
            seen.push(id.clone());
        }
    }
    // 顺序里缺席但 item 存在的条目（防御：顺序 item 丢失时按 id 序补齐）。
    for (key, entry) in merged.range(prefix.clone()..) {
        if !key.starts_with(&prefix) {
            break;
        }
        let id = &key[prefix.len()..];
        if !seen.iter().any(|s| s == id) {
            entries.push(entry.clone());
        }
    }
    Value::Array(entries)
}

/// app.ai → ai_config（providers 重建 + 本机 secret_ref 回填）。
fn build_ai_op(merged: &ItemMap, raw: &RawLocal, items: &[String], ops: &mut Vec<ApplyOp>) {
    let mut obj = overlay_shard_object("app.ai", merged, raw);
    let local_refs: BTreeMap<String, Value> = raw
        .shards
        .get("app.ai")
        .and_then(|v| v.get("providers"))
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|p| {
                    let id = p.get("id")?.as_str()?;
                    let r = p.get("secret_ref").cloned()?;
                    Some((id.to_string(), r))
                })
                .collect()
        })
        .unwrap_or_default();
    let mut providers = rebuild_entries(merged, raw, "app.ai", "providers");
    if let Some(arr) = providers.as_array_mut() {
        for provider in arr {
            let secret = provider
                .get("id")
                .and_then(Value::as_str)
                .and_then(|id| local_refs.get(id).cloned())
                .unwrap_or(Value::String(String::new()));
            if let Some(map) = provider.as_object_mut() {
                map.insert("secret_ref".to_string(), secret);
            }
        }
    }
    if let Some(map) = obj.as_object_mut() {
        map.insert("providers".to_string(), providers);
    }
    ops.push(ApplyOp::SetConfig {
        shard: "app.ai".into(),
        key: "ai_config".into(),
        value: obj,
        items: items.to_vec(),
    });
}

/// plugin:{pid} → plugin_config（revision CAS 用本机当前值）。
fn build_plugin_op(
    kv: &str,
    items: &[String],
    merged: &ItemMap,
    raw: &RawLocal,
    ops: &mut Vec<ApplyOp>,
) -> Result<(), SyncError> {
    let plugin_id = kv
        .strip_prefix("plugin:")
        .ok_or_else(|| SyncError::Unknown {
            message: format!("非法插件分片 key: {kv}"),
        })?
        .to_string();
    let prefix = format!("{kv}#");
    let local = raw.shards.get(kv);
    let enabled = merged
        .get(&format!("{prefix}enabled"))
        .cloned()
        .or_else(|| local.and_then(|v| v.get("enabled").cloned()))
        .unwrap_or(Value::Bool(true));
    let settings = merged
        .get(&format!("{prefix}settings"))
        .cloned()
        .or_else(|| local.and_then(|v| v.get("settings").cloned()))
        .unwrap_or(Value::Null);
    let has_custom = items.iter().any(|k| k.ends_with("#custom_triggers"));
    let has_disabled = items.iter().any(|k| k.ends_with("#disabled_default_triggers"));
    let expected_revision = local
        .and_then(|v| v.get("revision"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    ops.push(ApplyOp::PluginConfig {
        shard: kv.to_string(),
        plugin_id,
        enabled: enabled.as_bool().unwrap_or(true),
        settings,
        custom_triggers: has_custom
            .then(|| merged.get(&format!("{prefix}custom_triggers")).cloned())
            .flatten(),
        disabled_default_triggers: has_disabled
            .then(|| {
                merged
                    .get(&format!("{prefix}disabled_default_triggers"))
                    .cloned()
            })
            .flatten(),
        expected_revision,
        items: items.to_vec(),
    });
    Ok(())
}

/// prompt_library 行增删。
fn build_prompt_ops(
    items: &[String],
    merged: &ItemMap,
    _raw: &RawLocal,
    ops: &mut Vec<ApplyOp>,
) -> Result<(), SyncError> {
    for item in items {
        let id = item
            .strip_prefix(PROMPT_PREFIX)
            .ok_or_else(|| SyncError::Unknown {
                message: format!("非法提示词 item: {item}"),
            })?;
        match merged.get(item) {
            Some(row) => {
                // 校验 name（domain 提示词库规则）；失败在应用层转为覆盖记录。
                ops.push(ApplyOp::PromptUpsert {
                    shard: PROMPT_SHARD.to_string(),
                    id: id.to_string(),
                    row: row.clone(),
                    item: item.clone(),
                });
            }
            None => {
                // 内置条目锁定由存储层删除拒绝兜底，失败转本机覆盖记录。
                ops.push(ApplyOp::PromptDelete {
                    shard: PROMPT_SHARD.to_string(),
                    id: id.to_string(),
                    item: item.clone(),
                });
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn raw_with(shards: Vec<(&str, Value)>, prompts: Vec<(&str, Value)>) -> RawLocal {
        RawLocal {
            shards: shards
                .into_iter()
                .map(|(k, v)| (k.to_string(), v))
                .collect(),
            prompts: prompts
                .into_iter()
                .map(|(k, v)| (k.to_string(), v))
                .collect(),
        }
    }

    #[test]
    fn items_from_raw_strips_secret_ref_and_keeps_local_only_out() {
        let raw = raw_with(
            vec![
                (
                    "app.appearance",
                    serde_json::json!({"theme":"dark","language":"zh","window_opacity":0.9,
                        "auto_start":true,"log_level":"debug","onboarding_version":2}),
                ),
                (
                    "app.ai",
                    serde_json::json!({"enabled":true,"providers":[
                        {"id":"p1","kind":"openai_compatible","secret_ref":"blink/p1/key",
                         "models":[{"id":"m1","display_name":"M","enabled":true}]}],
                         "tier_main":{"provider_id":"p1","model_id":"m1"},
                         "chat_config":{"auto_title":true,"skill_config":{"enabled":true}}}),
                ),
            ],
            vec![(
                "builtin.editor.tidy",
                serde_json::json!({"name":"整理","scope":"editor","builtin":true,"sort_order":1,"hidden":false}),
            )],
        );
        let items = items_from_raw(&raw).unwrap();
        // appearance：只有三个可移植字段进快照。
        assert_eq!(items.get("app.appearance#theme"), Some(&json!("dark")));
        assert!(items.get("app.appearance#auto_start").is_none());
        // ai：provider 条目剔除 secret_ref，模型/档位保留。
        let p1 = items.get("app.ai#providers/p1").unwrap();
        assert!(p1.get("secret_ref").is_none());
        assert_eq!(p1.get("models").unwrap().as_array().unwrap().len(), 1);
        assert_eq!(
            items.get("app.ai#providers$order"),
            Some(&json!(["p1"]))
        );
        assert_eq!(
            items.get("app.ai#chat_config.auto_title"),
            Some(&json!(true))
        );
        // 提示词行进入。
        assert!(items.contains_key("prompt_library#entry/builtin.editor.tidy"));
    }

    #[test]
    fn shard_of_item_variants() {
        assert_eq!(shard_of_item("app.appearance#theme"), "app.appearance");
        assert_eq!(shard_of_item("app.chord#bindings.chat"), "app.chord");
        assert_eq!(
            shard_of_item("plugin:builtin.translate#enabled"),
            "plugin:builtin.translate"
        );
        assert_eq!(
            shard_of_item("prompt_library#entry/x"),
            PROMPT_SHARD
        );
    }

    #[test]
    fn normalize_set_item_sorts_and_dedups() {
        let v = json!(["b", "a", "b"]);
        assert_eq!(normalize_item("app.disable#disabled_builtin_actions", &v), json!(["a", "b"]));
        // 非集合 item 原样。
        assert_eq!(normalize_item("app.appearance#theme", &v), v);
    }

    #[test]
    fn build_apply_ops_routes_changed_shards() {
        let raw = raw_with(
            vec![
                (
                    "app.appearance",
                    json!({"theme":"auto","language":"zh","window_opacity":1.0,"auto_start":false}),
                ),
                (
                    "app.hotkey",
                    json!({"modifiers":["alt"],"key":" ","display":"Alt+Space","tap_threshold":300,"grace_period":500}),
                ),
                (
                    "app.weblinks",
                    json!({"entries":[{"id":"gh","name":"GitHub","keyword":"gh","url_template":"u","enabled":true}],"seeded":true}),
                ),
                (
                    "plugin:builtin.translate",
                    json!({"revision":7,"enabled":true,"settings":{"a":1},"custom_triggers":[],"disabled_default_triggers":[]}),
                ),
            ],
            vec![],
        );
        let mut merged = items_from_raw(&raw).unwrap();
        merged.insert("app.appearance#theme".into(), json!("dark"));
        merged.insert("app.hotkey#key".into(), json!("k"));
        merged.remove("app.weblinks#entries/gh");
        merged.insert("app.weblinks#entries$order".into(), json!([]));
        merged.insert("plugin:builtin.translate#enabled".into(), json!(false));

        let ops = build_apply_ops(&merged, &raw, &items_from_raw(&raw).unwrap()).unwrap();
        let keys: Vec<(&str, String)> = ops
            .iter()
            .map(|op| match op {
                ApplyOp::SetConfig { key, .. } => ("set", key.clone()),
                ApplyOp::PluginConfig { plugin_id, .. } => ("plugin", plugin_id.clone()),
                ApplyOp::PromptUpsert { .. } => ("prompt_upsert", String::new()),
                ApplyOp::PromptDelete { .. } => ("prompt_del", String::new()),
            })
            .collect();
        assert!(keys.contains(&("set", "general_config".to_string())));
        assert!(keys.contains(&("set", "hotkey".to_string())));
        assert!(keys.contains(&("set", "weblinks".to_string())));
        assert!(keys.contains(&("plugin", "builtin.translate".to_string())));
        // hotkey 载荷 = 本机对象 + 合并覆盖。
        for op in &ops {
            if let ApplyOp::SetConfig { key, value, .. } = op {
                if key == "hotkey" {
                    assert_eq!(value.get("key"), Some(&json!("k")));
                    assert_eq!(value.get("display"), Some(&json!("Alt+Space")));
                }
                if key == "weblinks" {
                    // 远端删除条目 → 条目移除；seeded 保留本机值。
                    assert_eq!(value.get("entries").unwrap().as_array().unwrap().len(), 0);
                    assert_eq!(value.get("seeded"), Some(&json!(true)));
                }
            }
            if let ApplyOp::PluginConfig {
                plugin_id,
                expected_revision,
                settings,
                ..
            } = op
            {
                if plugin_id == "builtin.translate" {
                    assert_eq!(*expected_revision, 7);
                    assert_eq!(settings.get("a"), Some(&json!(1)));
                }
            }
        }
    }

    #[test]
    fn build_apply_ops_reinjects_local_secret_ref() {
        let raw = raw_with(
            vec![(
                "app.ai",
                json!({"enabled":false,"providers":[
                    {"id":"p1","kind":"openai_compatible","secret_ref":"blink/p1/key","models":[]}]}),
            )],
            vec![],
        );
        let mut merged = items_from_raw(&raw).unwrap();
        merged.insert("app.ai#enabled".into(), json!(true));
        let ops = build_apply_ops(&merged, &raw, &items_from_raw(&raw).unwrap()).unwrap();
        for op in ops {
            if let ApplyOp::SetConfig { key, value, .. } = op {
                if key == "ai_config" {
                    let arr = value.get("providers").unwrap().as_array().unwrap();
                    // 快照条目无 secret_ref，应用时回填本机引用——不空串清空。
                    assert_eq!(
                        arr[0].get("secret_ref"),
                        Some(&json!("blink/p1/key"))
                    );
                }
            }
        }
    }
}
