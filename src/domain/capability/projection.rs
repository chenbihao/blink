//! Cap 协议投影规则（0.14 §三/§五）——manifest 投影配置 + JSONPath 取值工具。
//!
//! **核心思想**（§3.1）：插件只吐纯 `data`，投影规则（pointer / desc / actions）
//! 上移到 manifest 做代理。投影引擎用 `ProjectionRule` 把 `PluginRawResult` 投影成
//! `CapabilityResult`（轨道 A）。
//!
//! **双轨制**（§3.2.3）：
//! - 轨道 A（manifest 规范化）：简单返回（translate / IP / weather）→ 只返回 data，
//!   manifest 配 result_shape / items_pointer / item_actions
//! - 轨道 B（直接构造）：复杂返回（search_files 需格式化等）→ 直接吐完整 CapabilityResult
//!
//! **0.14.0**：定义 `ProjectionRule` 结构 + JSONPath 取值工具函数 + 单测。
//! **0.14.1**：实现 `project()` 投影引擎（四出口共用 canonical 投影）。
//! **0.17.10**：projection 职责收敛——`project()` 替换为 `normalize()`，不再读
//! pointer / item_pointer / item_desc_pointer（data 保留完整原始值）。展示字段
//! 挑选移到展示出口（`to_display_text`）用 projection 规则动态完成。AI 出口
//! 天然拿到完整 raw data。

use serde::{Deserialize, Serialize};

use super::result::{CapabilityResult, ItemAction, ItemResult};

/// manifest 投影规则——告诉投影引擎"怎么把 data 投影成 CapabilityResult"。
///
/// 对应 manifest 的 `result_shape` / `pointer` / `desc` / `desc_pointer` /
/// `items_pointer` / `item_pointer` / `item_desc_pointer` / `item_actions` 字段。
///
/// **0.17.10 职责收敛**：
/// - `normalize()`（invoke 链路）只读 `result_shape` / `items_pointer` / `item_actions`，
///   data 保留完整原始值，**不读** `pointer` / `item_pointer` / `item_desc_pointer`。
/// - `to_display_text()`（展示出口）读 `item_pointer` / `item_desc_pointer` 做展示投影。
/// - AI 出口（`to_rig_tool_result`）不读任何 pointer，直接拿完整 data。
///
/// manifest **不做格式化**——需要 `format_size(bytes)` 这种时，由能力单元自己在
/// data 里算好填进去（如 `size_display: "1.2 MB"`）。manifest 只负责"取哪个值"
/// 和"静态串"，不做计算。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ProjectionRule {
    /// 结果形态：text / items / blob / done。
    /// 缺失时由规范化引擎根据 data 类型推断。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result_shape: Option<ResultShape>,

    // ── text / blob 形态的投影规则（0.17.10: 展示出口用，normalize 不读） ──
    /// 主值从 data 哪里取（JSONPath）。`"$"` = data 整体。
    /// 例：weather 插件 data={city,temp,...}，pointer="$.temp" → 取 temp 作为主值。
    ///
    /// **0.17.10**：normalize 不读此字段（content = value_to_string(data)）。
    /// Text 形态不再支持 pointer 投影。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pointer: Option<String>,
    /// 静态 desc 字符串。
    ///
    /// **0.17.10**：normalize 不读此字段（desc = None）。Text 形态的展示出口
    /// `to_display_text` 直接用 content，也不读此字段。保留仅供向后兼容。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub desc: Option<String>,
    /// 动态 desc：从 data 取某字段作为 desc（JSONPath）。
    ///
    /// **0.17.10**：normalize 不读此字段。展示出口 `to_display_text` 对 Items
    /// 形态用 `item_desc_pointer`（非此字段），对 Text 形态不读任何 pointer。
    /// 保留仅供向后兼容。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub desc_pointer: Option<String>,

    // ── items 形态的投影规则 ────────────────────────────────────────────
    /// data 整体是数组时的 JSONPath（通常 `"$"` = data 整体）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub items_pointer: Option<String>,
    /// 每项的主值从该项的哪个字段取（JSONPath，相对于单项）。
    /// 例：IP 插件每项={ip,type}，item_pointer="$.ip" → 主值=ip 字段值。
    ///
    /// **0.17.10**：normalize 不读此字段（data 保留完整元素对象）。
    /// 展示出口 `to_display_text` 用此字段取主标题。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item_pointer: Option<String>,
    /// 每项的 desc 从该项的哪个字段取（JSONPath，相对于单项）。
    /// 例：IP 插件 item_desc_pointer="$.type" → desc="本地"/"公网"。
    ///
    /// **0.17.10**：normalize 不读此字段（desc = None）。
    /// 展示出口 `to_display_text` 用此字段取副标题。
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
/// 当前规范化引擎用 `jsonpath_query`（取首个匹配）处理 items_pointer，
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

// ── 规范化引擎（0.17.10 收敛）──────────────────────────────────────────────

/// 把 raw data 规范化为 `CapabilityResult`（0.17.10 收敛）。
///
/// 与旧 `project` 的关键区别：**不读** `pointer` / `item_pointer` / `item_desc_pointer`，
/// data 保留完整原始值。展示字段挑选由展示出口（`to_display_text`）用 projection
/// 规则动态完成。
///
/// **保留读**：`result_shape`（形态）、`items_pointer`（数组根）、`item_actions`（动作声明）。
///
/// **规范化规则**：
/// | result_shape | raw data 形态 | 规范化结果 |
/// |---|---|---|
/// | Items | 数组 | `Items{ items: [ItemResult{ data: 完整元素, desc: None }] }` |
/// | Items | 非数组 | 兜底为单元素列表 |
/// | Text | 任意 | `Text{ content: value_to_string(data), desc: None }` |
/// | Done | 任意 | `Done{ summary: value_to_string(data) }` |
/// | Blob | 任意 | `Done{ summary }` 兜底（插件通过 JSONL 不传二进制） |
pub fn normalize(data: &serde_json::Value, rule: &ProjectionRule) -> CapabilityResult {
    let shape = rule.result_shape.unwrap_or_else(|| infer_shape(data));

    match shape {
        ResultShape::Text => CapabilityResult::Text {
            content: value_to_string(data),
            desc: None,
        },
        ResultShape::Items => {
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
                .map(|elem| ItemResult {
                    data: elem.clone(), // 完整元素，不挑字段
                    desc: None,         // 展示出口动态投影
                    actions: rule
                        .item_actions
                        .iter()
                        .map(action_def_to_item_action)
                        .collect(),
                })
                .collect();

            CapabilityResult::Items { items }
        }
        ResultShape::Blob => {
            // 插件通过 JSONL 不传二进制——Blob 结果走轨道 B（builtin 直接构造）。
            // 如果插件配了 blob shape，把 data 当描述文本，返回 Done 兜底。
            let summary = match data {
                serde_json::Value::String(s) => s.clone(),
                serde_json::Value::Null => "已完成".to_string(),
                _ => data.to_string(),
            };
            CapabilityResult::Done { summary }
        }
        ResultShape::Done => {
            let summary = match data {
                serde_json::Value::String(s) => s.clone(),
                serde_json::Value::Null => "已完成".to_string(),
                _ => data.to_string(),
            };
            CapabilityResult::Done { summary }
        }
    }
}

// ── 工具函数 ───────────────────────────────────────────────────────────────

/// 从 data 类型推断 `ResultShape`（manifest 未配 `result_shape` 时的兜底）。
fn infer_shape(data: &serde_json::Value) -> ResultShape {
    match data {
        serde_json::Value::Array(_) => ResultShape::Items,
        serde_json::Value::String(_) | serde_json::Value::Number(_) => ResultShape::Text,
        _ => ResultShape::Done,
    }
}

/// `serde_json::Value` → 可读字符串（String 原样，其他 `to_string`）。
pub(super) fn value_to_string(v: &serde_json::Value) -> String {
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

    // ── normalize() 规范化引擎测试（0.17.10）─────────────────────────────

    // ── Text shape ──────────────────────────────────────────────────────

    /// Text + 字符串 data → content = value_to_string(data)，desc = None。
    /// 翻译插件场景：data="你好"，normalize 不挑字段。
    #[test]
    fn normalize_text_string_data() {
        let data = json!("你好");
        let rule = ProjectionRule {
            result_shape: Some(ResultShape::Text),
            pointer: Some("$".into()),
            ..Default::default()
        };
        let result = normalize(&data, &rule);
        match result {
            CapabilityResult::Text { content, desc } => {
                // normalize 不读 pointer，content = value_to_string("你好") = "你好"
                assert_eq!(content, "你好");
                // normalize 不设 desc
                assert!(desc.is_none());
            }
            _ => panic!("应是 Text"),
        }
    }

    /// Text + 对象 data → content = JSON 串（normalize 不读 pointer）。
    /// 验证 normalize 不挑字段：pointer="$.temp" 被忽略。
    #[test]
    fn normalize_text_object_ignores_pointer() {
        let data = json!({ "city": "北京", "temp": 25 });
        let rule = ProjectionRule {
            result_shape: Some(ResultShape::Text),
            pointer: Some("$.temp".into()), // normalize 应忽略此字段
            desc: Some("译文".into()),      // normalize 应忽略此字段
            ..Default::default()
        };
        let result = normalize(&data, &rule);
        match result {
            CapabilityResult::Text { content, desc } => {
                // content = value_to_string(data) = JSON 串
                assert!(content.contains("北京"));
                assert!(content.contains("25"));
                // desc = None（normalize 不读 desc / desc_pointer）
                assert!(desc.is_none());
            }
            _ => panic!("应是 Text"),
        }
    }

    /// Text + 数字 data → content = "42"。
    #[test]
    fn normalize_text_number_data() {
        let data = json!(42);
        let rule = ProjectionRule {
            result_shape: Some(ResultShape::Text),
            ..Default::default()
        };
        let result = normalize(&data, &rule);
        match result {
            CapabilityResult::Text { content, .. } => {
                assert_eq!(content, "42");
            }
            _ => panic!("应是 Text"),
        }
    }

    // ── Items shape ─────────────────────────────────────────────────────

    /// Items + 数组 data → 每项 data 保留完整对象，desc = None。
    /// IP 插件场景：data=[{ip,type},...]，item_pointer="$.ip" 被忽略。
    #[test]
    fn normalize_items_preserves_complete_objects() {
        let data = json!([
            { "ip": "192.168.1.1", "type": "本地" },
            { "ip": "8.8.8.8", "type": "公网" }
        ]);
        let rule = ProjectionRule {
            result_shape: Some(ResultShape::Items),
            items_pointer: Some("$".into()),
            item_pointer: Some("$.ip".into()), // normalize 应忽略
            item_desc_pointer: Some("$.type".into()), // normalize 应忽略
            item_actions: vec![ActionDef {
                kind: ActionKindDef::Copy,
                pointer: None,
            }],
            ..Default::default()
        };
        let result = normalize(&data, &rule);
        match result {
            CapabilityResult::Items { items } => {
                assert_eq!(items.len(), 2);
                // data 保留完整对象，不挑字段
                assert_eq!(items[0].data["ip"], "192.168.1.1");
                assert_eq!(items[0].data["type"], "本地");
                // desc = None（展示出口动态投影）
                assert!(items[0].desc.is_none());
                // actions 仍从 item_actions 映射
                assert_eq!(items[0].actions.len(), 1);
                assert!(matches!(items[0].actions[0], ItemAction::Copy { .. }));
                // 第二项也保留完整对象
                assert_eq!(items[1].data["ip"], "8.8.8.8");
                assert_eq!(items[1].data["type"], "公网");
            }
            _ => panic!("应是 Items"),
        }
    }

    /// Items + 字符串数组 → 每项 data = 完整字符串。
    #[test]
    fn normalize_items_string_array() {
        let data = json!(["item1", "item2"]);
        let rule = ProjectionRule {
            result_shape: Some(ResultShape::Items),
            items_pointer: Some("$".into()),
            ..Default::default()
        };
        let result = normalize(&data, &rule);
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
    fn normalize_items_with_multiple_actions() {
        let data = json!([
            { "path": "C:\\file.txt", "name": "file.txt" }
        ]);
        let rule = ProjectionRule {
            result_shape: Some(ResultShape::Items),
            items_pointer: Some("$".into()),
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
        let result = normalize(&data, &rule);
        match result {
            CapabilityResult::Items { items } => {
                assert_eq!(items.len(), 1);
                // data 保留完整对象
                assert_eq!(items[0].data["path"], "C:\\file.txt");
                assert_eq!(items[0].data["name"], "file.txt");
                assert_eq!(items[0].actions.len(), 2);
                assert!(matches!(items[0].actions[0], ItemAction::OpenFile { .. }));
                assert!(matches!(items[0].actions[1], ItemAction::Copy { .. }));
            }
            _ => panic!("应是 Items"),
        }
    }

    /// Items + 空数组 → 空 items 列表。
    #[test]
    fn normalize_items_empty_array() {
        let data = json!([]);
        let rule = ProjectionRule {
            result_shape: Some(ResultShape::Items),
            items_pointer: Some("$".into()),
            ..Default::default()
        };
        let result = normalize(&data, &rule);
        match result {
            CapabilityResult::Items { items } => {
                assert!(items.is_empty());
            }
            _ => panic!("应是 Items"),
        }
    }

    /// Items + 非数组 data（对象）→ 兜底为单元素列表。
    #[test]
    fn normalize_items_non_array_wraps_as_single() {
        let data = json!({ "name": "single" });
        let rule = ProjectionRule {
            result_shape: Some(ResultShape::Items),
            ..Default::default()
        };
        let result = normalize(&data, &rule);
        match result {
            CapabilityResult::Items { items } => {
                assert_eq!(items.len(), 1);
                assert_eq!(items[0].data["name"], "single");
            }
            _ => panic!("应是 Items"),
        }
    }

    /// Items + 中文 data（完整对象保留中文字段）。
    #[test]
    fn normalize_items_chinese_data() {
        let data = json!([
            { "ip": "127.0.0.1", "type": "本地IP" },
        ]);
        let rule = ProjectionRule {
            result_shape: Some(ResultShape::Items),
            items_pointer: Some("$".into()),
            item_pointer: Some("$.ip".into()),
            item_desc_pointer: Some("$.type".into()),
            ..Default::default()
        };
        let result = normalize(&data, &rule);
        match result {
            CapabilityResult::Items { items } => {
                // data 保留完整对象
                assert_eq!(items[0].data["ip"], "127.0.0.1");
                assert_eq!(items[0].data["type"], "本地IP");
                // desc = None
                assert!(items[0].desc.is_none());
            }
            _ => panic!("应是 Items"),
        }
    }

    // ── Done shape ──────────────────────────────────────────────────────

    /// Done + 字符串 data → summary=data。
    #[test]
    fn normalize_done_with_string_data() {
        let data = json!("已写入剪贴板");
        let rule = ProjectionRule {
            result_shape: Some(ResultShape::Done),
            ..Default::default()
        };
        let result = normalize(&data, &rule);
        match result {
            CapabilityResult::Done { summary } => {
                assert_eq!(summary, "已写入剪贴板");
            }
            _ => panic!("应是 Done"),
        }
    }

    /// Done + Null data → summary="已完成"（默认文案）。
    #[test]
    fn normalize_done_with_null_data() {
        let data = json!(null);
        let rule = ProjectionRule {
            result_shape: Some(ResultShape::Done),
            ..Default::default()
        };
        let result = normalize(&data, &rule);
        match result {
            CapabilityResult::Done { summary } => {
                assert_eq!(summary, "已完成");
            }
            _ => panic!("应是 Done"),
        }
    }

    /// Done + 对象 data → summary=JSON 串兜底。
    #[test]
    fn normalize_done_with_object_data() {
        let data = json!({ "status": "ok" });
        let rule = ProjectionRule {
            result_shape: Some(ResultShape::Done),
            ..Default::default()
        };
        let result = normalize(&data, &rule);
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
    fn normalize_blob_falls_back_to_done() {
        let data = json!("截图描述");
        let rule = ProjectionRule {
            result_shape: Some(ResultShape::Blob),
            ..Default::default()
        };
        let result = normalize(&data, &rule);
        match result {
            CapabilityResult::Done { summary } => {
                assert_eq!(summary, "截图描述");
            }
            _ => panic!("Blob shape 应兜底为 Done"),
        }
    }

    /// Blob shape + Null data → summary="已完成"。
    #[test]
    fn normalize_blob_null_data_uses_default() {
        let data = json!(null);
        let rule = ProjectionRule {
            result_shape: Some(ResultShape::Blob),
            ..Default::default()
        };
        let result = normalize(&data, &rule);
        match result {
            CapabilityResult::Done { summary } => {
                assert_eq!(summary, "已完成");
            }
            _ => panic!("应兜底为 Done"),
        }
    }

    // ── shape 推断（result_shape 缺失）──────────────────────────────────

    /// data 是字符串 → 推断为 Text。
    #[test]
    fn normalize_infers_text_from_string_data() {
        let data = json!("自动推断文本");
        let rule = ProjectionRule::default(); // 无 result_shape
        let result = normalize(&data, &rule);
        match result {
            CapabilityResult::Text { content, .. } => {
                assert_eq!(content, "自动推断文本");
            }
            _ => panic!("字符串 data 应推断为 Text"),
        }
    }

    /// data 是数组 → 推断为 Items。
    #[test]
    fn normalize_infers_items_from_array_data() {
        let data = json!(["a", "b"]);
        let rule = ProjectionRule::default();
        let result = normalize(&data, &rule);
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
    fn normalize_infers_done_from_null_data() {
        let data = json!(null);
        let rule = ProjectionRule::default();
        let result = normalize(&data, &rule);
        match result {
            CapabilityResult::Done { summary } => {
                assert_eq!(summary, "已完成");
            }
            _ => panic!("Null data 应推断为 Done"),
        }
    }

    /// data 是数字 → 推断为 Text。
    #[test]
    fn normalize_infers_text_from_number_data() {
        let data = json!(42);
        let rule = ProjectionRule::default();
        let result = normalize(&data, &rule);
        match result {
            CapabilityResult::Text { content, .. } => {
                assert_eq!(content, "42");
            }
            _ => panic!("数字 data 应推断为 Text"),
        }
    }
}
