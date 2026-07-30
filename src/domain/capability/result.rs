//! Capability 统一返回类型（0.9.7 §3.2，0.14 重构协议分层）。
//!
//! 四变体覆盖所有能力返回场景。`Serialize` 保证可投影成协议层 Value
//! （0.11 CLI stdout / MCP result）。
//!
//! **0.14 重构**：
//! - `ItemResult` 从 `{ title, subtitle, payload, score }` 改为 `{ data, desc, actions }`
//!   ——删 `title`/`subtitle`/`score`（主窗口展示概念，不该在协议层），
//!   `data` 取代 `payload`（语义更明确："这就是给 AI 的纯净数据"），
//!   `desc` 取代 `subtitle`（可选副标题），`actions` 显式声明可用动作。
//! - `Text` / `Blob` 加 `desc: Option<String>`——可选描述信息。
//! - 主标题（旧 `title`）由前端从 `data` 派生（`derive_title()`）。
//! - Blob 摘要逻辑收进 `blob_summary()` 一个方法（消除 4 处重复）。

use serde::Serialize;
use serde_json::Value;

// ── rig 投影层（0.12.0 统一投影入口，0.14 适配新结构）──────────────────────
//
// **0.12.0 投影统一**：`to_rig_tool_result()` 是 CapabilityResult → rig ToolResultContent
// 的**唯一投影入口**。service.rs 旧的 `project_capability_result_to_tool_message` 已删除，
// Turn 2 回流改调本函数 + `rig_tool_result_to_text()` 提取文本。
//
// **0.14 适配**：Items 投影到 AI 时只序列化 `data`（纯净语义数据），
// 不含 `desc`（给人看的副标题）和 `actions`（给前端用的动作声明）。
// Blob 摘要改调 `blob_summary()` 统一方法。

impl CapabilityResult {
    /// 投影成 rig `ToolResultContent`——tool 结果喂回 LLM 的规范路径。
    ///
    /// **消费方**：
    /// - 主窗口 Turn 2 回流（经 `rig_tool_result_to_text()` 提取文本 → `ChatMessage::tool`）
    /// - 0.12.1+ 对话窗口 Agent tool loop（直接用 `Vec<ToolResultContent>`）
    pub fn to_rig_tool_result(&self) -> Vec<rig_core::completion::message::ToolResultContent> {
        use rig_core::completion::message::ToolResultContent;

        match self {
            CapabilityResult::Text { content, .. } => {
                vec![ToolResultContent::text(content)]
            }
            CapabilityResult::Items { items } => {
                // Items → 只序列化 data 喂 LLM（模型读纯 JSON 语义数据）
                vec![ToolResultContent::text(items_to_llm_json(items))]
            }
            CapabilityResult::Blob { .. } => {
                // Blob → 文本摘要（不喂原始字节省 token）
                vec![ToolResultContent::text(self.blob_summary())]
            }
            CapabilityResult::Done { summary } => {
                vec![ToolResultContent::text(summary)]
            }
        }
    }

    /// Blob 摘要——人类可读的尺寸描述（0.14 收进此方法，消除 4 处重复）。
    ///
    /// 例：`"已获取 image/png (1.2 MB)"`
    pub fn blob_summary(&self) -> String {
        match self {
            CapabilityResult::Blob { mime, bytes, .. } => {
                let size_kb = bytes.len() as f64 / 1024.0;
                let size_text = if size_kb >= 1024.0 {
                    format!("{:.1} MB", size_kb / 1024.0)
                } else {
                    format!("{:.1} KB", size_kb)
                };
                format!("已获取 {} ({})", mime, size_text)
            }
            _ => String::new(),
        }
    }

    /// 投影成人类可读文本——CLI / 审计日志共用（0.14.1 收敛）。
    ///
    /// 与 `to_rig_tool_result()` 的区别：此方法读 `data` + `desc`（给人看），
    /// 后者只读 `data`（给 AI 看，不含 desc）。
    ///
    /// - `Text` → content
    /// - `Items` → 编号列表（`derive_title` 派生主标题 + desc 副标题）
    /// - `Blob` → `blob_summary()`
    /// - `Done` → `✓ {summary}`
    pub fn to_display_text(&self) -> String {
        match self {
            CapabilityResult::Text { content, .. } => content.clone(),
            CapabilityResult::Items { items } => {
                if items.is_empty() {
                    "（无结果）".to_string()
                } else {
                    items
                        .iter()
                        .enumerate()
                        .map(|(i, item)| {
                            let title = derive_title(&item.data);
                            match &item.desc {
                                Some(d) if !d.is_empty() => {
                                    format!("{}. {} — {}", i + 1, title, d)
                                }
                                _ => format!("{}. {}", i + 1, title),
                            }
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                }
            }
            CapabilityResult::Blob { .. } => self.blob_summary(),
            CapabilityResult::Done { summary } => format!("✓ {summary}"),
        }
    }
}

/// 把 `Vec<ToolResultContent>` 提取成纯文本——主窗口 Turn 2 回流用。
///
/// `ToolResultContent` 有 Text / Image 两变体；主窗口是文本模型，只取 Text。
/// 对话窗口（0.12.1+）可直接用 `Vec<ToolResultContent>` 喂多模态模型。
pub fn rig_tool_result_to_text(
    contents: &[rig_core::completion::message::ToolResultContent],
) -> String {
    use rig_core::completion::message::ToolResultContent;
    contents
        .iter()
        .filter_map(|c| match c {
            ToolResultContent::Text(t) => Some(t.text().to_string()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// 把 `ItemResult` 列表投影成喂 LLM 的 JSON 文本（0.14 重构）。
///
/// **0.14 变化**：只序列化 `data`（纯净语义数据）。`desc` 是给人看的副标题，
/// `actions` 是给前端用的动作声明——对 AI 都是噪音，不喂。
///
/// 失败兜底返回 `"[]"`。
pub fn items_to_llm_json(items: &[ItemResult]) -> String {
    let data_items: Vec<&Value> = items.iter().map(|i| &i.data).collect();
    serde_json::to_string(&data_items).unwrap_or_else(|_| "[]".to_string())
}

/// 从 `data` 派生主标题（0.14 §5.2 fallback 派生）。
///
/// 规则：
/// - 纯字符串 → 直接用
/// - 纯数字 → to_string
/// - 对象 → 优先取 `name` / `title` 字段；都没有则 JSON 串兜底
/// - 其他 → JSON 串兜底
///
/// **复杂对象主标题**（如 search_files 的文件名）走轨道 B——builtin 直接构造好
/// ItemResult，data 里带 `name` 字段，此函数从约定字段取。
pub fn derive_title(data: &Value) -> String {
    match data {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        Value::Object(map) => {
            // 优先取 name / title 字段（builtin capability 约定）
            map.get("name")
                .or_else(|| map.get("title"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .unwrap_or_else(|| data.to_string())
        }
        _ => data.to_string(),
    }
}

/// 原子能力的统一返回——四种形态覆盖所有场景（0.14 重构字段语义）。
///
/// **消费方投影**（AI lane / 前端协议层职责，不在 Capability 层）：
/// - `Text` → 前端 Copy 条目 / AI 当文本上下文
/// - `Items` → 前端渲染条目（用户选）/ AI 读 data JSON
/// - `Blob` → 字节+mime，投影方式由消费方决定（base64/raw/file_url）
/// - `Done` → "✓ 已执行" 展示
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CapabilityResult {
    /// 纯文本：翻译译文 / IP 查询 / OCR 结果。
    /// → 前端 Copy、AI 当文本上下文。
    /// `desc` 可选描述（如"译文"），AI 不读此字段。
    Text {
        content: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        desc: Option<String>,
    },

    /// 结构化列表：文件搜索 / 剪贴板历史 / 进程列表。
    /// → 前端渲染条目、AI 读 data JSON。
    Items { items: Vec<ItemResult> },

    /// 二进制：截图 / 音频 / 文件内容。
    /// → 字节+mime，投影方式由消费方决定（base64/raw/file_url）。
    ///
    /// **不在能力层决定返回形态**（文件还是流还是 base64）——那是消费方约束。
    /// rig 0.39 的 `DocumentSourceKind`（`Url/Base64/FileId/Raw/String`）印证此设计。
    ///
    /// **已知性能点**：截图 ~14MB clone 不便宜。0.9.7 先 clone 跑通；
    /// 若热路径出现，改 `Arc<Vec<u8>>` 或投影层避免深拷贝。
    /// `desc` 可选描述，AI 不读此字段。
    Blob {
        mime: String,
        bytes: Vec<u8>,
        #[serde(skip_serializing_if = "Option::is_none")]
        desc: Option<String>,
    },

    /// 无返回值副作用：已写入 / 已打开 / 已锁定。
    /// → 携带人类可读 summary，AI lane 展示"✓ 已执行"。
    Done { summary: String },
}

/// 结构化列表的单项（`CapabilityResult::Items` 的元素）——0.14 重构。
///
/// **对比旧 `ItemResult` 的关键变化**：
/// - 删 `title` / `subtitle` / `score`——这些是主窗口展示概念，不该在协议层
/// - `data` 取代 `payload`，语义更明确（"这就是给 AI 的纯净数据"）
/// - `desc` 取代 `subtitle`，且明确是"可选副标题"
/// - `actions` 显式声明，AI/前端看到就知道这个 item 能干什么
/// - 主标题（旧 `title`）由前端从 `data` 派生（`derive_title()`）
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ItemResult {
    /// 给 AI/CLI/MCP 的语义数据（自解释）。AI 直接读这个。
    pub data: Value,
    /// 给主窗口展示的可选副标题（替代旧 subtitle）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub desc: Option<String>,
    /// 可执行动作声明。空数组 = 纯展示项。
    /// 主窗口约定：回车执行 `actions[0]`，右键展开全部。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub actions: Vec<ItemAction>,
}

/// Item 的可执行动作声明（0.14）。
///
/// `pointer` 为 None 时默认取 item 的主值（由 manifest 的 `item_pointer` 指定）。
/// 主窗口交互约定：`actions[0]` = 回车行为，右键展开全部。
#[allow(dead_code)] // ItemAction 变体由 ItemResult.actions 消费（主窗口前端渲染用，0.14.4 适配）
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ItemAction {
    /// 复制。`pointer` 指定从 data 取哪个值复制。
    Copy {
        #[serde(skip_serializing_if = "Option::is_none")]
        pointer: Option<String>,
    },
    /// 打开文件。`pointer` 指定从 data 取哪个路径。
    OpenFile {
        #[serde(skip_serializing_if = "Option::is_none")]
        pointer: Option<String>,
    },
    /// 打开 URL。`pointer` 指定从 data 取哪个 URL。
    OpenUrl {
        #[serde(skip_serializing_if = "Option::is_none")]
        pointer: Option<String>,
    },
    /// 资源管理器定位。`pointer` 指定从 data 取哪个路径。
    Reveal {
        #[serde(skip_serializing_if = "Option::is_none")]
        pointer: Option<String>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ── CapabilityResult 序列化测试 ──────────────────────────────────────

    #[test]
    fn text_serializes_to_tagged_json() {
        let r = CapabilityResult::Text {
            content: "你好".into(),
            desc: None,
        };
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(v["kind"], "text");
        assert_eq!(v["content"], "你好");
        // desc=None 时 skip_serializing_if 生效
        assert!(v.get("desc").is_none());
    }

    #[test]
    fn text_with_desc_serializes_desc() {
        let r = CapabilityResult::Text {
            content: "你好".into(),
            desc: Some("译文".into()),
        };
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(v["desc"], "译文");
    }

    #[test]
    fn items_serializes_with_array() {
        let r = CapabilityResult::Items {
            items: vec![ItemResult {
                data: json!({ "path": "C:\\docs\\report.pdf", "name": "report.pdf" }),
                desc: Some("C:\\docs".into()),
                actions: vec![ItemAction::OpenFile {
                    pointer: Some("$.path".into()),
                }],
            }],
        };
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(v["kind"], "items");
        assert_eq!(v["items"][0]["data"]["path"], "C:\\docs\\report.pdf");
        assert_eq!(v["items"][0]["data"]["name"], "report.pdf");
        assert_eq!(v["items"][0]["desc"], "C:\\docs");
        assert_eq!(v["items"][0]["actions"][0]["type"], "open_file");
    }

    #[test]
    fn blob_serializes_with_mime_and_bytes() {
        let r = CapabilityResult::Blob {
            mime: "image/png".into(),
            bytes: vec![0x89, 0x50, 0x4E, 0x47],
            desc: None,
        };
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(v["kind"], "blob");
        assert_eq!(v["mime"], "image/png");
        assert_eq!(v["bytes"][0], 0x89);
        assert!(v.get("desc").is_none());
    }

    #[test]
    fn done_serializes_with_summary() {
        let r = CapabilityResult::Done {
            summary: "已写入剪贴板".into(),
        };
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(v["kind"], "done");
        assert_eq!(v["summary"], "已写入剪贴板");
    }

    /// 协议层投影可行性：所有变体都能 round-trip 成纯 JSON。
    #[test]
    fn all_variants_roundtrip_through_json() {
        let variants: Vec<CapabilityResult> = vec![
            CapabilityResult::Text {
                content: "a".into(),
                desc: None,
            },
            CapabilityResult::Items { items: vec![] },
            CapabilityResult::Blob {
                mime: "x".into(),
                bytes: vec![],
                desc: None,
            },
            CapabilityResult::Done {
                summary: "s".into(),
            },
        ];
        for v in &variants {
            let json = serde_json::to_string(v).unwrap();
            let _: Value = serde_json::from_str(&json).unwrap();
        }
    }

    // ── ItemResult 序列化测试 ────────────────────────────────────────────

    #[test]
    fn item_result_skips_none_and_empty_fields() {
        let item = ItemResult {
            data: json!("bare"),
            desc: None,
            actions: vec![],
        };
        let v = serde_json::to_value(&item).unwrap();
        // None desc 不出现
        assert!(v.get("desc").is_none());
        // 空 actions 不出现
        assert!(v.get("actions").is_none());
        assert_eq!(v["data"], "bare");
    }

    // ── ItemAction 测试 ──────────────────────────────────────────────────

    #[test]
    fn item_action_copy_serializes() {
        let action = ItemAction::Copy { pointer: None };
        let v = serde_json::to_value(&action).unwrap();
        assert_eq!(v["type"], "copy");
        assert!(v.get("pointer").is_none());
    }

    #[test]
    fn item_action_open_file_with_pointer_serializes() {
        let action = ItemAction::OpenFile {
            pointer: Some("$.path".into()),
        };
        let v = serde_json::to_value(&action).unwrap();
        assert_eq!(v["type"], "open_file");
        assert_eq!(v["pointer"], "$.path");
    }

    #[test]
    fn item_action_open_url_serializes() {
        let action = ItemAction::OpenUrl {
            pointer: Some("$.url".into()),
        };
        let v = serde_json::to_value(&action).unwrap();
        assert_eq!(v["type"], "open_url");
    }

    #[test]
    fn item_action_reveal_serializes() {
        let action = ItemAction::Reveal { pointer: None };
        let v = serde_json::to_value(&action).unwrap();
        assert_eq!(v["type"], "reveal");
    }

    // ── derive_title 测试 ────────────────────────────────────────────────

    #[test]
    fn derive_title_string_returns_as_is() {
        assert_eq!(derive_title(&json!("hello")), "hello");
    }

    #[test]
    fn derive_title_number_to_string() {
        assert_eq!(derive_title(&json!(42)), "42");
    }

    #[test]
    fn derive_title_object_with_name_field() {
        let data = json!({ "name": "report.pdf", "path": "C:\\docs\\report.pdf" });
        assert_eq!(derive_title(&data), "report.pdf");
    }

    #[test]
    fn derive_title_object_with_title_field() {
        let data = json!({ "title": "My Title", "other": "x" });
        assert_eq!(derive_title(&data), "My Title");
    }

    #[test]
    fn derive_title_object_without_name_falls_back_to_json() {
        let data = json!({ "foo": "bar" });
        assert_eq!(derive_title(&data), data.to_string());
    }

    #[test]
    fn derive_title_null_to_string() {
        assert_eq!(derive_title(&Value::Null), "null");
    }

    // ── blob_summary 测试 ────────────────────────────────────────────────

    #[test]
    fn blob_summary_kb_range() {
        let r = CapabilityResult::Blob {
            mime: "image/png".into(),
            bytes: vec![0u8; 2048], // 2 KB
            desc: None,
        };
        let summary = r.blob_summary();
        assert!(summary.contains("image/png"));
        assert!(summary.contains("2.0 KB"));
    }

    #[test]
    fn blob_summary_mb_range() {
        let r = CapabilityResult::Blob {
            mime: "image/png".into(),
            bytes: vec![0u8; 2 * 1024 * 1024], // 2 MB
            desc: None,
        };
        let summary = r.blob_summary();
        assert!(summary.contains("2.0 MB"));
        assert!(!summary.contains("KB"));
    }

    #[test]
    fn blob_summary_non_blob_returns_empty() {
        let r = CapabilityResult::Text {
            content: "hello".into(),
            desc: None,
        };
        assert_eq!(r.blob_summary(), "");
    }

    // ── to_rig_tool_result 投影测试 ──────────────────────────────────────

    #[test]
    fn rig_projection_text_produces_text_content() {
        use rig_core::completion::message::ToolResultContent;
        let r = CapabilityResult::Text {
            content: "hello".into(),
            desc: None,
        };
        let contents = r.to_rig_tool_result();
        assert_eq!(contents.len(), 1);
        assert!(matches!(contents[0], ToolResultContent::Text(_)));
    }

    #[test]
    fn rig_projection_items_produces_json_text() {
        use rig_core::completion::message::ToolResultContent;
        let r = CapabilityResult::Items {
            items: vec![ItemResult {
                data: json!({ "path": "C:\\file.txt", "name": "file.txt" }),
                desc: None,
                actions: vec![ItemAction::OpenFile { pointer: None }],
            }],
        };
        let contents = r.to_rig_tool_result();
        assert_eq!(contents.len(), 1);
        if let ToolResultContent::Text(t) = &contents[0] {
            // AI 只读 data，不含 desc / actions
            assert!(t.text().contains("file.txt"));
            assert!(t.text().contains("C:\\\\file.txt"));
            assert!(!t.text().contains("actions"));
        } else {
            panic!("Items should project to Text");
        }
    }

    /// 0.14: items_to_llm_json 只序列化 data，不含 desc / actions。
    #[test]
    fn items_to_llm_json_only_contains_data() {
        let items = vec![
            ItemResult {
                data: json!({ "ip": "192.168.1.1", "type": "本地" }),
                desc: Some("本地IP".into()),
                actions: vec![ItemAction::Copy { pointer: None }],
            },
            ItemResult {
                data: json!("纯文本结果"),
                desc: None,
                actions: vec![],
            },
        ];
        let json_text = super::items_to_llm_json(&items);
        // data 内容保留
        assert!(json_text.contains("192.168.1.1"));
        assert!(json_text.contains("纯文本结果"));
        // desc / actions 不出现
        assert!(
            !json_text.contains("desc"),
            "items_to_llm_json 不应包含 desc: {json_text}"
        );
        assert!(
            !json_text.contains("actions"),
            "items_to_llm_json 不应包含 actions: {json_text}"
        );
    }

    #[test]
    fn items_to_llm_json_empty_returns_empty_array() {
        let json_text = super::items_to_llm_json(&[]);
        assert_eq!(json_text, "[]");
    }

    #[test]
    fn rig_projection_blob_produces_text_summary() {
        use rig_core::completion::message::ToolResultContent;
        let r = CapabilityResult::Blob {
            mime: "image/png".into(),
            bytes: vec![0x89, 0x50, 0x4E, 0x47],
            desc: None,
        };
        let contents = r.to_rig_tool_result();
        assert_eq!(contents.len(), 1);
        if let ToolResultContent::Text(t) = &contents[0] {
            assert!(t.text().contains("image/png"));
            assert!(t.text().contains("KB"));
        } else {
            panic!("Blob should project to Text summary");
        }
    }

    #[test]
    fn rig_tool_result_to_text_extracts_text() {
        let r = CapabilityResult::Text {
            content: "hello world".into(),
            desc: None,
        };
        let contents = r.to_rig_tool_result();
        let text = super::rig_tool_result_to_text(&contents);
        assert_eq!(text, "hello world");
    }

    #[test]
    fn rig_tool_result_to_text_handles_blob_summary() {
        let r = CapabilityResult::Blob {
            mime: "image/png".into(),
            bytes: vec![0u8; 2048],
            desc: None,
        };
        let contents = r.to_rig_tool_result();
        let text = super::rig_tool_result_to_text(&contents);
        assert!(text.contains("image/png"));
        assert!(text.contains("2.0 KB"));
    }

    #[test]
    fn rig_projection_done_produces_text() {
        use rig_core::completion::message::ToolResultContent;
        let r = CapabilityResult::Done {
            summary: "已写入".into(),
        };
        let contents = r.to_rig_tool_result();
        assert_eq!(contents.len(), 1);
        if let ToolResultContent::Text(t) = &contents[0] {
            assert_eq!(t.text(), "已写入");
        } else {
            panic!("Done should project to Text");
        }
    }

    // ── to_display_text 测试（0.14.1 CLI canonical 投影）─────────────────

    #[test]
    fn display_text_text_returns_content() {
        let r = CapabilityResult::Text {
            content: "你好世界".into(),
            desc: None,
        };
        assert_eq!(r.to_display_text(), "你好世界");
    }

    #[test]
    fn display_text_done_returns_summary_with_prefix() {
        let r = CapabilityResult::Done {
            summary: "已写入剪贴板".into(),
        };
        assert_eq!(r.to_display_text(), "✓ 已写入剪贴板");
    }

    #[test]
    fn display_text_blob_returns_summary() {
        let r = CapabilityResult::Blob {
            mime: "image/png".into(),
            bytes: vec![0u8; 2048],
            desc: None,
        };
        let text = r.to_display_text();
        assert!(text.contains("image/png"));
        assert!(text.contains("2.0 KB"));
    }

    #[test]
    fn display_text_items_numbered_list() {
        let r = CapabilityResult::Items {
            items: vec![
                ItemResult {
                    data: json!({ "name": "file1.txt" }),
                    desc: Some("文档".into()),
                    actions: vec![],
                },
                ItemResult {
                    data: json!({ "name": "file2.txt" }),
                    desc: None,
                    actions: vec![],
                },
            ],
        };
        let text = r.to_display_text();
        assert!(text.contains("1. file1.txt — 文档"));
        assert!(text.contains("2. file2.txt"));
    }

    #[test]
    fn display_text_empty_items_returns_placeholder() {
        let r = CapabilityResult::Items { items: vec![] };
        assert_eq!(r.to_display_text(), "（无结果）");
    }
}
