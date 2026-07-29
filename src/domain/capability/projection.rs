//! Cap 协议投影规则（0.14 §三/§五）——manifest 投影配置 + JSONPath 取值工具。
//!
//! **核心思想**（§3.1）：插件只吐纯 `data`，投影规则（pointer / desc / actions）
//! 上移到 manifest 做代理。投影引擎用 `ProjectionRule` 把 `PluginRawResult` 投影成
//! `CapabilityResult`（轨道 A）。
//!
//! **双轨制**（§3.2.3）：
//! - 轨道 A（manifest 投影）：简单返回（translate / IP / weather）→ 只返回 data，
//!   manifest 配 pointer/desc/actions
//! - 轨道 B（直接构造）：复杂返回（search_files 需格式化等）→ 直接吐完整 CapabilityResult
//!
//! **0.14.0**：定义 `ProjectionRule` 结构 + JSONPath 取值工具函数 + 单测。
//! **0.14.1**：实现 `project()` 投影引擎（四出口共用 canonical 投影）。

use serde::{Deserialize, Serialize};

use super::result::{CapabilityResult, ItemAction, ItemResult};

/// manifest 投影规则——告诉投影引擎"怎么把 data 投影成 CapabilityResult"。
///
/// 对应 manifest 的 `result_shape` / `pointer` / `desc` / `desc_pointer` /
/// `items_pointer` / `item_pointer` / `item_desc_pointer` / `item_actions` 字段。
///
/// **desc 三来源优先级**（§3.2.2）：
/// 1. `desc_pointer` 指定 data 某字段 → 取值作为 desc（动态）
/// 2. `desc` 静态字符串
/// 3. 都没有 → None → 不展示 desc
///
/// manifest **不做格式化**——需要 `format_size(bytes)` 这种时，由能力单元自己在
/// data 里算好填进去（如 `size_display: "1.2 MB"`）。manifest 只负责"取哪个值"
/// 和"静态串"，不做计算。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ProjectionRule {
    /// 结果形态：text / items / blob / done。
    /// 缺失时由投影引擎根据 data 类型推断（0.14.1 实现）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result_shape: Option<ResultShape>,

    // ── text / blob 形态的投影规则 ──────────────────────────────────────
    /// 主值从 data 哪里取（JSONPath）。`"$"` = data 整体。
    /// 例：weather 插件 data={city,temp,...}，pointer="$.temp" → 取 temp 作为主值。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pointer: Option<String>,
    /// 静态 desc 字符串。例：翻译插件配 `desc: "译文"`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub desc: Option<String>,
    /// 动态 desc：从 data 取某字段作为 desc（JSONPath）。
    /// 例：weather 插件配 `desc_pointer: "$.city"` → desc = 城市名。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub desc_pointer: Option<String>,

    // ── items 形态的投影规则 ────────────────────────────────────────────
    /// data 整体是数组时的 JSONPath（通常 `"$"` = data 整体）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub items_pointer: Option<String>,
    /// 每项的主值从该项的哪个字段取（JSONPath，相对于单项）。
    /// 例：IP 插件每项={ip,type}，item_pointer="$.ip" → 主值=ip 字段值。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item_pointer: Option<String>,
    /// 每项的 desc 从该项的哪个字段取（JSONPath，相对于单项）。
    /// 例：IP 插件 item_desc_pointer="$.type" → desc="本地"/"公网"。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item_desc_pointer: Option<String>,
    /// 每项支持的动作列表。
    /// 例：`["copy"]` → 每项配 Copy 动作。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub item_actions: Vec<ActionDef>,
}

/// 结果形态枚举——对应 manifest 的 `result_shape` 字段。
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ResultShape {
    /// 纯文本：翻译译文 / IP 查询结果。
    Text,
    /// 结构化列表：文件搜索 / IP 多层。
    Items,
    /// 二进制：截图 / 音频。
    Blob,
    /// 无返回值副作用：已写入 / 已打开。
    #[default]
    Done,
}

/// manifest 侧声明的动作类型——映射到 `ItemAction`。
///
/// 用独立 enum 而非直接引用 `ItemAction`，保持 manifest 解析层零业务依赖。
/// `pointer` 为 None 时默认取 item 的主值（由 `item_pointer` 指定）。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ActionDef {
    /// 动作类型：copy / open_file / open_url / reveal。
    #[serde(rename = "type")]
    pub kind: ActionKindDef,
    /// JSONPath 指定从 data 取哪个值。None = 取主值。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pointer: Option<String>,
}

/// manifest 侧动作类型声明。
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ActionKindDef {
    /// 复制。
    Copy,
    /// 打开文件。
    #[default]
    OpenFile,
    /// 打开 URL。
    OpenUrl,
    /// 资源管理器定位。
    Reveal,
}

// ── JSONPath 取值工具 ──────────────────────────────────────────────────────

/// 用 JSONPath 从 data 中取值（0.14 §3.2.1）。
///
/// 使用 `jsonpath-rust` crate（纯 Rust，~30KB）。
///
/// **实施验证项**（§8.4）：
/// - 中文 value 的取值（IP 插件 `type:"本地IP"` 这种）
/// - 嵌套对象 `$[*].ip` 数组通配
/// - pointer 缺失时的优雅降级（返回 None，不 panic）
pub fn jsonpath_query(data: &serde_json::Value, path: &str) -> Option<serde_json::Value> {
    use jsonpath_rust::JsonPath;

    let jp = JsonPath::<serde_json::Value>::try_from(path).ok()?;
    let result = jp.find(data);
    // find 返回 Value::Array(vec![...]) 或 Value::Null
    match result {
        serde_json::Value::Array(arr) if !arr.is_empty() => arr.into_iter().next(),
        _ => None,
    }
}

/// 用 JSONPath 从 data 中取值列表（数组场景）。
///
/// 例：`$[*]` 取数组所有元素，`$[*].ip` 取每项的 ip 字段。
///
/// 当前投影引擎用 `jsonpath_query`（取首个匹配）处理 items_pointer，
/// 本函数保留供未来投影场景（如 `$[*]` 展平）和测试使用。
#[allow(dead_code)]
pub fn jsonpath_query_all(data: &serde_json::Value, path: &str) -> Vec<serde_json::Value> {
    use jsonpath_rust::JsonPath;

    let Ok(jp) = JsonPath::<serde_json::Value>::try_from(path) else {
        return Vec::new();
    };
    let result = jp.find(data);
    // find 返回 Value::Array(vec![...]) 或 Value::Null
    match result {
        serde_json::Value::Array(arr) => arr,
        _ => Vec::new(),
    }
}

// ── 投影引擎（0.14.1）──────────────────────────────────────────────────────

/// 用 `ProjectionRule` 把纯 `data` 投影成 `CapabilityResult`（轨道 A，0.14.1）。
///
/// 四个出口（AI / 主窗口 / CLI / MCP）共用这一个投影引擎（§5.1）。
/// 插件只吐纯 `data`，投影规则在 manifest 的 `ProjectionRule` 里配置。
///
/// **错误处理**：本函数只处理成功路径（data 投影）。调用方应在调用前检查
/// `PluginRawResult.error`，有错时走错误路径，不调本函数。
///
/// **shape 推断**：`result_shape` 缺失时根据 data 类型推断——
/// Array → Items，String/Number → Text，其他 → Done。
///
/// **desc 三来源**（§3.2.2）：
/// 1. `desc_pointer` 指定 data 某字段 → 取值作为 desc（动态）
/// 2. `desc` 静态字符串
/// 3. 都没有 → None → 不展示 desc
pub fn project(data: &serde_json::Value, rule: &ProjectionRule) -> CapabilityResult {
    let shape = rule.result_shape.unwrap_or_else(|| infer_shape(data));

    match shape {
        ResultShape::Text => project_text(data, rule),
        ResultShape::Items => project_items(data, rule),
        ResultShape::Blob => project_blob(data, rule),
        ResultShape::Done => project_done(data, rule),
    }
}

/// 从 data 类型推断 `ResultShape`（manifest 未配 `result_shape` 时的兜底）。
fn infer_shape(data: &serde_json::Value) -> ResultShape {
    match data {
        serde_json::Value::Array(_) => ResultShape::Items,
        serde_json::Value::String(_) | serde_json::Value::Number(_) => ResultShape::Text,
        _ => ResultShape::Done,
    }
}

/// Text 形态投影。
///
/// - `pointer` 取主值（缺失则用 data 整体）
/// - `desc` / `desc_pointer` 按 §3.2.2 优先级解析
fn project_text(data: &serde_json::Value, rule: &ProjectionRule) -> CapabilityResult {
    let content = rule
        .pointer
        .as_deref()
        .and_then(|p| jsonpath_query(data, p))
        .unwrap_or_else(|| data.clone());

    let content_str = value_to_string(&content);
    let desc = resolve_desc(data, rule);

    CapabilityResult::Text {
        content: content_str,
        desc,
    }
}

/// Items 形态投影。
///
/// - `items_pointer` 取数组（缺失则用 data 整体，要求是数组）
/// - 每项：`item_pointer` 取主值 → `data`，`item_desc_pointer` 取 desc，
///   `item_actions` 映射为 `Vec<ItemAction>`
fn project_items(data: &serde_json::Value, rule: &ProjectionRule) -> CapabilityResult {
    let array = rule
        .items_pointer
        .as_deref()
        .and_then(|p| jsonpath_query(data, p))
        .unwrap_or_else(|| data.clone());

    let elements: Vec<serde_json::Value> = match &array {
        serde_json::Value::Array(arr) => arr.clone(),
        // 非数组 → 当作单元素列表兜底
        other => vec![other.clone()],
    };

    let items: Vec<ItemResult> = elements
        .iter()
        .map(|elem| {
            let item_data = rule
                .item_pointer
                .as_deref()
                .and_then(|p| jsonpath_query(elem, p))
                .unwrap_or_else(|| elem.clone());

            let desc = rule
                .item_desc_pointer
                .as_deref()
                .and_then(|p| jsonpath_query(elem, p))
                .map(|v| value_to_string(&v));

            let actions = rule
                .item_actions
                .iter()
                .map(action_def_to_item_action)
                .collect();

            ItemResult {
                data: item_data,
                desc,
                actions,
            }
        })
        .collect();

    CapabilityResult::Items { items }
}

/// Blob 形态投影。
///
/// 插件通过 JSONL 不传二进制——Blob 结果走轨道 B（builtin 直接构造
/// `CapabilityResult::Blob`）。如果插件配了 blob shape，把 data 当描述文本，
/// 返回 `Done` 兜底（避免构造空 bytes 的假 Blob）。
fn project_blob(data: &serde_json::Value, rule: &ProjectionRule) -> CapabilityResult {
    let desc = resolve_desc(data, rule);
    let summary = match data {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Null => desc.unwrap_or_else(|| "已完成".into()),
        _ => data.to_string(),
    };
    CapabilityResult::Done { summary }
}

/// Done 形态投影。
///
/// `summary` 从 data 取（字符串直接用，Null 用默认文案，其他 JSON 串兜底）。
fn project_done(data: &serde_json::Value, _rule: &ProjectionRule) -> CapabilityResult {
    let summary = match data {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Null => "已完成".to_string(),
        _ => data.to_string(),
    };
    CapabilityResult::Done { summary }
}

/// desc 三来源优先级解析（§3.2.2）。
///
/// 1. `desc_pointer` → 从 data 动态取值
/// 2. `desc` → 静态字符串
/// 3. 都没有 → None
fn resolve_desc(data: &serde_json::Value, rule: &ProjectionRule) -> Option<String> {
    if let Some(path) = &rule.desc_pointer {
        if let Some(val) = jsonpath_query(data, path) {
            return Some(value_to_string(&val));
        }
    }
    rule.desc.clone()
}

/// `serde_json::Value` → 可读字符串（String 原样，其他 `to_string`）。
fn value_to_string(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        _ => v.to_string(),
    }
}

/// `ActionDef` → `ItemAction` 映射。
fn action_def_to_item_action(def: &ActionDef) -> ItemAction {
    match def.kind {
        ActionKindDef::Copy => ItemAction::Copy {
            pointer: def.pointer.clone(),
        },
        ActionKindDef::OpenFile => ItemAction::OpenFile {
            pointer: def.pointer.clone(),
        },
        ActionKindDef::OpenUrl => ItemAction::OpenUrl {
            pointer: def.pointer.clone(),
        },
        ActionKindDef::Reveal => ItemAction::Reveal {
            pointer: def.pointer.clone(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ── ProjectionRule 结构测试 ──────────────────────────────────────────

    #[test]
    fn projection_rule_defaults_to_empty() {
        let rule = ProjectionRule::default();
        assert!(rule.result_shape.is_none());
        assert!(rule.pointer.is_none());
        assert!(rule.desc.is_none());
        assert!(rule.desc_pointer.is_none());
        assert!(rule.items_pointer.is_none());
        assert!(rule.item_pointer.is_none());
        assert!(rule.item_desc_pointer.is_none());
        assert!(rule.item_actions.is_empty());
    }

    #[test]
    fn projection_rule_text_shape_parses() {
        let json = r#"{
            "result_shape": "text",
            "pointer": "$",
            "desc": "译文"
        }"#;
        let rule: ProjectionRule = serde_json::from_str(json).unwrap();
        assert_eq!(rule.result_shape, Some(ResultShape::Text));
        assert_eq!(rule.pointer.as_deref(), Some("$"));
        assert_eq!(rule.desc.as_deref(), Some("译文"));
    }

    #[test]
    fn projection_rule_items_shape_parses() {
        let json = r#"{
            "result_shape": "items",
            "items_pointer": "$",
            "item_pointer": "$.ip",
            "item_desc_pointer": "$.type",
            "item_actions": [{"type": "copy"}]
        }"#;
        let rule: ProjectionRule = serde_json::from_str(json).unwrap();
        assert_eq!(rule.result_shape, Some(ResultShape::Items));
        assert_eq!(rule.items_pointer.as_deref(), Some("$"));
        assert_eq!(rule.item_pointer.as_deref(), Some("$.ip"));
        assert_eq!(rule.item_desc_pointer.as_deref(), Some("$.type"));
        assert_eq!(rule.item_actions.len(), 1);
        assert_eq!(rule.item_actions[0].kind, ActionKindDef::Copy);
        assert!(rule.item_actions[0].pointer.is_none());
    }

    #[test]
    fn projection_rule_desc_pointer_parses() {
        let json = r#"{
            "result_shape": "text",
            "pointer": "$.temp",
            "desc_pointer": "$.city"
        }"#;
        let rule: ProjectionRule = serde_json::from_str(json).unwrap();
        assert_eq!(rule.pointer.as_deref(), Some("$.temp"));
        assert_eq!(rule.desc_pointer.as_deref(), Some("$.city"));
        assert!(rule.desc.is_none());
    }

    #[test]
    fn projection_rule_blob_shape_parses() {
        let json = r#"{"result_shape": "blob"}"#;
        let rule: ProjectionRule = serde_json::from_str(json).unwrap();
        assert_eq!(rule.result_shape, Some(ResultShape::Blob));
    }

    #[test]
    fn projection_rule_done_shape_parses() {
        let json = r#"{"result_shape": "done"}"#;
        let rule: ProjectionRule = serde_json::from_str(json).unwrap();
        assert_eq!(rule.result_shape, Some(ResultShape::Done));
    }

    #[test]
    fn projection_rule_serializes_roundtrip() {
        let rule = ProjectionRule {
            result_shape: Some(ResultShape::Items),
            pointer: None,
            desc: None,
            desc_pointer: None,
            items_pointer: Some("$".into()),
            item_pointer: Some("$.ip".into()),
            item_desc_pointer: Some("$.type".into()),
            item_actions: vec![ActionDef {
                kind: ActionKindDef::Copy,
                pointer: None,
            }],
        };
        let json = serde_json::to_string(&rule).unwrap();
        let restored: ProjectionRule = serde_json::from_str(&json).unwrap();
        assert_eq!(rule, restored);
    }

    #[test]
    fn action_def_with_pointer_parses() {
        let json = r#"{"type": "open_file", "pointer": "$.path"}"#;
        let def: ActionDef = serde_json::from_str(json).unwrap();
        assert_eq!(def.kind, ActionKindDef::OpenFile);
        assert_eq!(def.pointer.as_deref(), Some("$.path"));
    }

    #[test]
    fn action_def_defaults_to_open_file() {
        let json = r#"{"type": "copy"}"#;
        let def: ActionDef = serde_json::from_str(json).unwrap();
        assert_eq!(def.kind, ActionKindDef::Copy);
        assert!(def.pointer.is_none());
    }

    // ── JSONPath 取值测试（§8.4 实施验证项）──────────────────────────────

    #[test]
    fn jsonpath_query_single_value() {
        let data = json!({ "ip": "192.168.1.1", "type": "本地" });
        let result = jsonpath_query(&data, "$.ip");
        assert_eq!(result, Some(json!("192.168.1.1")));
    }

    #[test]
    fn jsonpath_query_chinese_value() {
        // 中文 value 取值（IP 插件 type:"本地IP" 这种）
        let data = json!({ "type": "本地IP", "ip": "127.0.0.1" });
        let result = jsonpath_query(&data, "$.type");
        assert_eq!(result, Some(json!("本地IP")));
    }

    #[test]
    fn jsonpath_query_root_returns_whole_data() {
        let data = json!("纯文本翻译结果");
        let result = jsonpath_query(&data, "$");
        assert_eq!(result, Some(json!("纯文本翻译结果")));
    }

    #[test]
    fn jsonpath_query_missing_path_returns_none() {
        // pointer 缺失时优雅降级（返回 None，不 panic）
        let data = json!({ "ip": "1.2.3.4" });
        let result = jsonpath_query(&data, "$.nonexistent");
        assert_eq!(result, None);
    }

    #[test]
    fn jsonpath_query_all_array_elements() {
        // $[*] 取数组所有元素
        let data = json!([
            { "ip": "192.168.1.1", "type": "本地" },
            { "ip": "8.8.8.8", "type": "公网" }
        ]);
        let results = jsonpath_query_all(&data, "$[*]");
        assert_eq!(results.len(), 2);
        assert_eq!(results[0]["ip"], "192.168.1.1");
        assert_eq!(results[1]["ip"], "8.8.8.8");
    }

    #[test]
    fn jsonpath_query_all_nested_field() {
        // $[*].ip 取每项的 ip 字段
        let data = json!([
            { "ip": "192.168.1.1", "type": "本地" },
            { "ip": "8.8.8.8", "type": "公网" }
        ]);
        let results = jsonpath_query_all(&data, "$[*].ip");
        assert_eq!(results.len(), 2);
        assert_eq!(results[0], "192.168.1.1");
        assert_eq!(results[1], "8.8.8.8");
    }

    #[test]
    fn jsonpath_query_all_empty_array() {
        let data = json!([]);
        let results = jsonpath_query_all(&data, "$[*]");
        assert!(results.is_empty());
    }

    #[test]
    fn jsonpath_query_all_invalid_path_returns_empty() {
        let data = json!({ "foo": "bar" });
        let results = jsonpath_query_all(&data, "$.nonexistent");
        assert!(results.is_empty());
    }

    #[test]
    fn jsonpath_query_all_chinese_in_array() {
        // 数组中包含中文的取值
        let data = json!([
            { "type": "本地", "ip": "127.0.0.1" },
            { "type": "公网", "ip": "1.2.3.4" }
        ]);
        let results = jsonpath_query_all(&data, "$[*].type");
        assert_eq!(results.len(), 2);
        assert_eq!(results[0], "本地");
        assert_eq!(results[1], "公网");
    }

    // ── project() 投影引擎测试（4 shape × 3 desc 来源）──────────────────

    // ── Text shape ──────────────────────────────────────────────────────

    /// Text + desc=静态字符串。翻译插件场景：data="你好"，desc="译文"。
    #[test]
    fn project_text_with_static_desc() {
        let data = json!("你好");
        let rule = ProjectionRule {
            result_shape: Some(ResultShape::Text),
            pointer: Some("$".into()),
            desc: Some("译文".into()),
            ..Default::default()
        };
        let result = project(&data, &rule);
        match result {
            CapabilityResult::Text { content, desc } => {
                assert_eq!(content, "你好");
                assert_eq!(desc.as_deref(), Some("译文"));
            }
            _ => panic!("应是 Text"),
        }
    }

    /// Text + desc_pointer=动态取值。天气插件场景：data={city,temp}，desc_pointer="$.city"。
    #[test]
    fn project_text_with_dynamic_desc() {
        let data = json!({ "city": "北京", "temp": 25, "condition": "晴" });
        let rule = ProjectionRule {
            result_shape: Some(ResultShape::Text),
            pointer: Some("$.temp".into()),
            desc_pointer: Some("$.city".into()),
            ..Default::default()
        };
        let result = project(&data, &rule);
        match result {
            CapabilityResult::Text { content, desc } => {
                assert_eq!(content, "25");
                assert_eq!(desc.as_deref(), Some("北京"));
            }
            _ => panic!("应是 Text"),
        }
    }

    /// Text + 无 desc（desc 和 desc_pointer 都缺失）。
    #[test]
    fn project_text_without_desc() {
        let data = json!("纯文本结果");
        let rule = ProjectionRule {
            result_shape: Some(ResultShape::Text),
            pointer: Some("$".into()),
            ..Default::default()
        };
        let result = project(&data, &rule);
        match result {
            CapabilityResult::Text { content, desc } => {
                assert_eq!(content, "纯文本结果");
                assert!(desc.is_none());
            }
            _ => panic!("应是 Text"),
        }
    }

    /// Text + desc_pointer 优先于 desc（两者都有时，desc_pointer 胜出）。
    #[test]
    fn project_text_desc_pointer_takes_priority_over_static_desc() {
        let data = json!({ "value": "实际值", "label": "动态标签" });
        let rule = ProjectionRule {
            result_shape: Some(ResultShape::Text),
            pointer: Some("$.value".into()),
            desc: Some("静态desc".into()),
            desc_pointer: Some("$.label".into()),
            ..Default::default()
        };
        let result = project(&data, &rule);
        match result {
            CapabilityResult::Text { content, desc } => {
                assert_eq!(content, "实际值");
                // desc_pointer 优先
                assert_eq!(desc.as_deref(), Some("动态标签"));
            }
            _ => panic!("应是 Text"),
        }
    }

    /// Text + desc_pointer 路径不存在 → 回退到静态 desc。
    #[test]
    fn project_text_desc_pointer_missing_falls_back_to_static_desc() {
        let data = json!({ "value": "v" });
        let rule = ProjectionRule {
            result_shape: Some(ResultShape::Text),
            pointer: Some("$.value".into()),
            desc: Some("静态desc".into()),
            desc_pointer: Some("$.nonexistent".into()),
            ..Default::default()
        };
        let result = project(&data, &rule);
        match result {
            CapabilityResult::Text { content, desc } => {
                assert_eq!(content, "v");
                // desc_pointer 取不到 → 回退到静态 desc
                assert_eq!(desc.as_deref(), Some("静态desc"));
            }
            _ => panic!("应是 Text"),
        }
    }

    /// Text + pointer 缺失 → 用 data 整体作为 content。
    #[test]
    fn project_text_without_pointer_uses_data_as_content() {
        let data = json!("直接文本");
        let rule = ProjectionRule {
            result_shape: Some(ResultShape::Text),
            ..Default::default()
        };
        let result = project(&data, &rule);
        match result {
            CapabilityResult::Text { content, .. } => {
                assert_eq!(content, "直接文本");
            }
            _ => panic!("应是 Text"),
        }
    }

    // ── Items shape ─────────────────────────────────────────────────────

    /// Items + item_desc_pointer=动态取值。IP 插件场景：
    /// data=[{ip,type},...]，item_pointer="$.ip"，item_desc_pointer="$.type"。
    #[test]
    fn project_items_with_dynamic_desc() {
        let data = json!([
            { "ip": "192.168.1.1", "type": "本地" },
            { "ip": "8.8.8.8", "type": "公网" }
        ]);
        let rule = ProjectionRule {
            result_shape: Some(ResultShape::Items),
            items_pointer: Some("$".into()),
            item_pointer: Some("$.ip".into()),
            item_desc_pointer: Some("$.type".into()),
            item_actions: vec![ActionDef {
                kind: ActionKindDef::Copy,
                pointer: None,
            }],
            ..Default::default()
        };
        let result = project(&data, &rule);
        match result {
            CapabilityResult::Items { items } => {
                assert_eq!(items.len(), 2);
                assert_eq!(items[0].data, "192.168.1.1");
                assert_eq!(items[0].desc.as_deref(), Some("本地"));
                assert_eq!(items[0].actions.len(), 1);
                assert!(matches!(items[0].actions[0], ItemAction::Copy { .. }));
                assert_eq!(items[1].data, "8.8.8.8");
                assert_eq!(items[1].desc.as_deref(), Some("公网"));
            }
            _ => panic!("应是 Items"),
        }
    }

    /// Items + 无 item_desc_pointer → 每项 desc=None。
    #[test]
    fn project_items_without_desc() {
        let data = json!(["item1", "item2"]);
        let rule = ProjectionRule {
            result_shape: Some(ResultShape::Items),
            items_pointer: Some("$".into()),
            ..Default::default()
        };
        let result = project(&data, &rule);
        match result {
            CapabilityResult::Items { items } => {
                assert_eq!(items.len(), 2);
                assert_eq!(items[0].data, "item1");
                assert!(items[0].desc.is_none());
                assert!(items[0].actions.is_empty());
            }
            _ => panic!("应是 Items"),
        }
    }

    /// Items + 多 action（open_file + copy）。
    #[test]
    fn project_items_with_multiple_actions() {
        let data = json!([
            { "path": "C:\\file.txt", "name": "file.txt" }
        ]);
        let rule = ProjectionRule {
            result_shape: Some(ResultShape::Items),
            items_pointer: Some("$".into()),
            item_pointer: Some("$.path".into()),
            item_actions: vec![
                ActionDef {
                    kind: ActionKindDef::OpenFile,
                    pointer: None,
                },
                ActionDef {
                    kind: ActionKindDef::Copy,
                    pointer: None,
                },
            ],
            ..Default::default()
        };
        let result = project(&data, &rule);
        match result {
            CapabilityResult::Items { items } => {
                assert_eq!(items.len(), 1);
                assert_eq!(items[0].data, "C:\\file.txt");
                assert_eq!(items[0].actions.len(), 2);
                assert!(matches!(items[0].actions[0], ItemAction::OpenFile { .. }));
                assert!(matches!(items[0].actions[1], ItemAction::Copy { .. }));
            }
            _ => panic!("应是 Items"),
        }
    }

    /// Items + item_pointer 缺失 → 用每项整体作为 data。
    #[test]
    fn project_items_without_item_pointer_uses_whole_element() {
        let data = json!([{ "name": "a" }, { "name": "b" }]);
        let rule = ProjectionRule {
            result_shape: Some(ResultShape::Items),
            items_pointer: Some("$".into()),
            ..Default::default()
        };
        let result = project(&data, &rule);
        match result {
            CapabilityResult::Items { items } => {
                assert_eq!(items.len(), 2);
                assert_eq!(items[0].data["name"], "a");
                assert_eq!(items[1].data["name"], "b");
            }
            _ => panic!("应是 Items"),
        }
    }

    /// Items + 中文 desc（item_desc_pointer 取中文值）。
    #[test]
    fn project_items_chinese_desc() {
        let data = json!([
            { "ip": "127.0.0.1", "type": "本地IP" },
        ]);
        let rule = ProjectionRule {
            result_shape: Some(ResultShape::Items),
            items_pointer: Some("$".into()),
            item_pointer: Some("$.ip".into()),
            item_desc_pointer: Some("$.type".into()),
            item_actions: vec![ActionDef {
                kind: ActionKindDef::Copy,
                pointer: None,
            }],
            ..Default::default()
        };
        let result = project(&data, &rule);
        match result {
            CapabilityResult::Items { items } => {
                assert_eq!(items[0].data, "127.0.0.1");
                assert_eq!(items[0].desc.as_deref(), Some("本地IP"));
            }
            _ => panic!("应是 Items"),
        }
    }

    /// Items + 空数组 → 空 items 列表。
    #[test]
    fn project_items_empty_array() {
        let data = json!([]);
        let rule = ProjectionRule {
            result_shape: Some(ResultShape::Items),
            items_pointer: Some("$".into()),
            ..Default::default()
        };
        let result = project(&data, &rule);
        match result {
            CapabilityResult::Items { items } => {
                assert!(items.is_empty());
            }
            _ => panic!("应是 Items"),
        }
    }

    // ── Done shape ──────────────────────────────────────────────────────

    /// Done + 字符串 data → summary=data。
    #[test]
    fn project_done_with_string_data() {
        let data = json!("已写入剪贴板");
        let rule = ProjectionRule {
            result_shape: Some(ResultShape::Done),
            ..Default::default()
        };
        let result = project(&data, &rule);
        match result {
            CapabilityResult::Done { summary } => {
                assert_eq!(summary, "已写入剪贴板");
            }
            _ => panic!("应是 Done"),
        }
    }

    /// Done + Null data → summary="已完成"（默认文案）。
    #[test]
    fn project_done_with_null_data() {
        let data = json!(null);
        let rule = ProjectionRule {
            result_shape: Some(ResultShape::Done),
            ..Default::default()
        };
        let result = project(&data, &rule);
        match result {
            CapabilityResult::Done { summary } => {
                assert_eq!(summary, "已完成");
            }
            _ => panic!("应是 Done"),
        }
    }

    /// Done + 对象 data → summary=JSON 串兜底。
    #[test]
    fn project_done_with_object_data() {
        let data = json!({ "status": "ok" });
        let rule = ProjectionRule {
            result_shape: Some(ResultShape::Done),
            ..Default::default()
        };
        let result = project(&data, &rule);
        match result {
            CapabilityResult::Done { summary } => {
                assert!(summary.contains("ok"));
            }
            _ => panic!("应是 Done"),
        }
    }

    // ── Blob shape ──────────────────────────────────────────────────────

    /// Blob shape → 兜底返回 Done（插件不传二进制，走轨道 B）。
    #[test]
    fn project_blob_falls_back_to_done() {
        let data = json!("截图描述");
        let rule = ProjectionRule {
            result_shape: Some(ResultShape::Blob),
            desc: Some("截图".into()),
            ..Default::default()
        };
        let result = project(&data, &rule);
        match result {
            CapabilityResult::Done { summary } => {
                assert_eq!(summary, "截图描述");
            }
            _ => panic!("Blob shape 应兜底为 Done"),
        }
    }

    /// Blob shape + Null data + desc → summary=desc。
    #[test]
    fn project_blob_null_data_uses_desc_as_summary() {
        let data = json!(null);
        let rule = ProjectionRule {
            result_shape: Some(ResultShape::Blob),
            desc: Some("截图完成".into()),
            ..Default::default()
        };
        let result = project(&data, &rule);
        match result {
            CapabilityResult::Done { summary } => {
                assert_eq!(summary, "截图完成");
            }
            _ => panic!("应兜底为 Done"),
        }
    }

    // ── shape 推断（result_shape 缺失）──────────────────────────────────

    /// data 是字符串 → 推断为 Text。
    #[test]
    fn project_infers_text_from_string_data() {
        let data = json!("自动推断文本");
        let rule = ProjectionRule::default(); // 无 result_shape
        let result = project(&data, &rule);
        match result {
            CapabilityResult::Text { content, .. } => {
                assert_eq!(content, "自动推断文本");
            }
            _ => panic!("字符串 data 应推断为 Text"),
        }
    }

    /// data 是数组 → 推断为 Items。
    #[test]
    fn project_infers_items_from_array_data() {
        let data = json!(["a", "b"]);
        let rule = ProjectionRule::default();
        let result = project(&data, &rule);
        match result {
            CapabilityResult::Items { items } => {
                assert_eq!(items.len(), 2);
                assert_eq!(items[0].data, "a");
            }
            _ => panic!("数组 data 应推断为 Items"),
        }
    }

    /// data 是 Null → 推断为 Done。
    #[test]
    fn project_infers_done_from_null_data() {
        let data = json!(null);
        let rule = ProjectionRule::default();
        let result = project(&data, &rule);
        match result {
            CapabilityResult::Done { summary } => {
                assert_eq!(summary, "已完成");
            }
            _ => panic!("Null data 应推断为 Done"),
        }
    }

    /// data 是数字 → 推断为 Text。
    #[test]
    fn project_infers_text_from_number_data() {
        let data = json!(42);
        let rule = ProjectionRule::default();
        let result = project(&data, &rule);
        match result {
            CapabilityResult::Text { content, .. } => {
                assert_eq!(content, "42");
            }
            _ => panic!("数字 data 应推断为 Text"),
        }
    }
}
