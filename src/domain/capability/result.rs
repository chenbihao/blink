//! Capability 统一返回类型（0.9.7 §3.2）。
//!
//! 四变体覆盖所有能力返回场景。`Serialize` 保证可投影成协议层 Value
//! （0.11 CLI stdout / MCP result）。
//!
//! **为什么只有四个变体**（不多不少）：
//! - 少了 `Blob` → 处理不了截图/音频
//! - 少了 `Items` → 处理不了列表（文件/历史）
//! - 少了 `Done` → 处理不了纯副作用（写剪贴板/打开）
//! - 多了（如 `File{path}`）→ 等于把消费形态焊进能力层，违背"返回类型消费方决定"

use serde::Serialize;
use serde_json::Value;

// ── rig 投影层（0.12.0 统一投影入口）──────────────────────────────────────
//
// **0.12.0 投影统一**：`to_rig_tool_result()` 是 CapabilityResult → rig ToolResultContent
// 的**唯一投影入口**。service.rs 旧的 `project_capability_result_to_tool_message` 已删除，
// Turn 2 回流改调本函数 + `rig_tool_result_to_text()` 提取文本。
//
// **Blob 摘要策略**（0.12.0 从 service.rs 迁入）：Blob 不喂原始字节（省 token），
// 返回人类可读摘要如 "已获取 image/png (1.2 MB)"。
// 未来 Agent 窗口若需多模态（喂图片给 vision 模型），可加 `to_rig_tool_result_raw()` 方法。

impl CapabilityResult {
    /// 投影成 rig `ToolResultContent`——tool 结果喂回 LLM 的规范路径。
    ///
    /// **消费方**：
    /// - 主窗口 Turn 2 回流（经 `rig_tool_result_to_text()` 提取文本 → `ChatMessage::tool`）
    /// - 0.12.1+ 对话窗口 Agent tool loop（直接用 `Vec<ToolResultContent>`）
    pub fn to_rig_tool_result(&self) -> Vec<rig_core::completion::message::ToolResultContent> {
        use rig_core::completion::message::ToolResultContent;

        match self {
            CapabilityResult::Text { content } => {
                vec![ToolResultContent::text(content)]
            }
            CapabilityResult::Items { items } => {
                // Items → 序列化 JSON 文本喂 LLM（模型读 JSON 上下文）
                vec![ToolResultContent::text(items_to_llm_json(items))]
            }
            CapabilityResult::Blob { mime, bytes } => {
                // Blob → 文本摘要（不喂原始字节省 token）
                let size_kb = bytes.len() as f64 / 1024.0;
                let size_text = if size_kb >= 1024.0 {
                    format!("{:.1} MB", size_kb / 1024.0)
                } else {
                    format!("{:.1} KB", size_kb)
                };
                vec![ToolResultContent::text(format!(
                    "已获取 {} ({})",
                    mime, size_text
                ))]
            }
            CapabilityResult::Done { summary } => {
                vec![ToolResultContent::text(summary)]
            }
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

/// 把 `ItemResult` 列表投影成喂 LLM 的 JSON 文本（0.13.7）。
///
/// **为何不用 `serde_json::to_string(items)`**：`ItemResult.score` 是给主窗口排序用的
/// 归一化分数（0.0..=1.0），对 LLM 是纯噪音——模型不需要知道某个文件匹配分是 0.87，
/// 反而可能误导（如"分低的更相关？"）。投影到 AI 时剔除该字段，只保留
/// `title` / `subtitle` / `payload`（语义信息）。
///
/// `subtitle` 为 None 时通过 `skip_serializing_if` 自然省略。
/// 失败兜底返回 `"[]"`（与旧行为一致）。
pub fn items_to_llm_json(items: &[ItemResult]) -> String {
    // 手动投影成 JSON 数组（剔除 score），避免为 LLM 投影单独定义一个 SkipScoreItem 类型。
    #[derive(serde::Serialize)]
    struct LlmItem<'a> {
        title: &'a str,
        #[serde(skip_serializing_if = "Option::is_none")]
        subtitle: &'a Option<String>,
        /// payload 原样透传——它是语义载体（path/text/任意 JSON），AI 读它做推理。
        payload: &'a Value,
    }
    let llm_items: Vec<_> = items
        .iter()
        .map(|i| LlmItem {
            title: &i.title,
            subtitle: &i.subtitle,
            payload: &i.payload,
        })
        .collect();
    serde_json::to_string(&llm_items).unwrap_or_else(|_| "[]".to_string())
}

/// 原子能力的统一返回——四种形态覆盖所有场景。
///
/// **消费方投影**（AI lane / 前端协议层职责，不在 Capability 层）：
/// - `Text` → 前端 Copy 条目 / AI 当文本上下文
/// - `Items` → 前端渲染条目（用户选）/ AI 读 JSON
/// - `Blob` → 字节+mime，投影方式由消费方决定（base64/raw/file_url）
/// - `Done` → "✓ 已执行" 展示
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CapabilityResult {
    /// 纯文本：OCR / IP 查询 / 总结。
    /// → 前端 Copy、AI 当文本上下文。
    Text { content: String },

    /// 结构化列表：文件搜索 / 剪贴板历史 / 进程列表。
    /// → 前端渲染条目、AI 读 JSON。
    Items { items: Vec<ItemResult> },

    /// 二进制：截图 / 音频 / 文件内容。
    /// → 字节+mime，投影方式由消费方决定（base64/raw/file_url）。
    ///
    /// **不在能力层决定返回形态**（文件还是流还是 base64）——那是消费方约束。
    /// rig 0.39 的 `DocumentSourceKind`（`Url/Base64/FileId/Raw/String`）印证此设计。
    ///
    /// **已知性能点**：截图 ~14MB clone 不便宜。0.9.7 先 clone 跑通；
    /// 若热路径出现，改 `Arc<Vec<u8>>` 或投影层避免深拷贝。
    Blob { mime: String, bytes: Vec<u8> },

    /// 无返回值副作用：已写入 / 已打开 / 已锁定。
    /// → 携带人类可读 summary，AI lane 展示"✓ 已执行"。
    Done { summary: String },
}

/// 结构化列表的单项（`CapabilityResult::Items` 的元素）。
///
/// `payload` 既给前端执行（path/text），又给 AI 读（任意 JSON）——
/// 一个返回，两种消费（主窗口回流 / AI 读上下文 / CLI stdout 各取所需）。
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ItemResult {
    /// 主行显示文本。
    pub title: String,
    /// 副行显示文本（路径/提示）。可选。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subtitle: Option<String>,
    /// 结构化 payload——既给前端执行（path/text），又给 AI 读（任意 JSON）。
    pub payload: Value,
    /// 归一化分数 0.0..=1.0（有序则给分，无序 None）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub score: Option<f32>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn text_serializes_to_tagged_json() {
        let r = CapabilityResult::Text {
            content: "你好".into(),
        };
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(v["kind"], "text");
        assert_eq!(v["content"], "你好");
    }

    #[test]
    fn items_serializes_with_array() {
        let r = CapabilityResult::Items {
            items: vec![ItemResult {
                title: "report.pdf".into(),
                subtitle: Some("C:\\docs".into()),
                payload: json!({ "path": "C:\\docs\\report.pdf" }),
                score: Some(0.9),
            }],
        };
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(v["kind"], "items");
        assert_eq!(v["items"][0]["title"], "report.pdf");
        assert_eq!(v["items"][0]["payload"]["path"], "C:\\docs\\report.pdf");
    }

    #[test]
    fn blob_serializes_with_mime_and_bytes() {
        let r = CapabilityResult::Blob {
            mime: "image/png".into(),
            bytes: vec![0x89, 0x50, 0x4E, 0x47],
        };
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(v["kind"], "blob");
        assert_eq!(v["mime"], "image/png");
        // bytes 序列化为 JSON 数组
        assert_eq!(v["bytes"][0], 0x89);
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
    /// 这是 0.11 CLI/MCP 派生的前提——签名纯 JSON 进出。
    #[test]
    fn all_variants_roundtrip_through_json() {
        let variants: Vec<CapabilityResult> = vec![
            CapabilityResult::Text {
                content: "a".into(),
            },
            CapabilityResult::Items { items: vec![] },
            CapabilityResult::Blob {
                mime: "x".into(),
                bytes: vec![],
            },
            CapabilityResult::Done {
                summary: "s".into(),
            },
        ];
        for v in &variants {
            let json = serde_json::to_string(v).unwrap();
            // round-trip 回 Value 验证是合法 JSON（协议层能消费）
            let _: Value = serde_json::from_str(&json).unwrap();
        }
    }

    #[test]
    fn item_result_skips_none_fields() {
        let item = ItemResult {
            title: "bare".into(),
            subtitle: None,
            payload: json!({}),
            score: None,
        };
        let v = serde_json::to_value(&item).unwrap();
        // None 字段不出现（skip_serializing_if）
        assert!(v.get("subtitle").is_none());
        assert!(v.get("score").is_none());
        assert_eq!(v["title"], "bare");
    }

    // ── to_rig_tool_result 投影测试（0.9.7 Step 4）─────────────────────────

    #[test]
    fn rig_projection_text_produces_text_content() {
        use rig_core::completion::message::ToolResultContent;
        let r = CapabilityResult::Text {
            content: "hello".into(),
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
                title: "file.txt".into(),
                subtitle: None,
                payload: json!({ "path": "C:\\file.txt" }),
                score: Some(0.8),
            }],
        };
        let contents = r.to_rig_tool_result();
        assert_eq!(contents.len(), 1);
        // Items → 序列化成 JSON 文本喂 LLM
        if let ToolResultContent::Text(t) = &contents[0] {
            assert!(t.text().contains("file.txt"));
            assert!(t.text().contains("C:\\\\file.txt"));
        } else {
            panic!("Items should project to Text");
        }
    }

    /// 0.13.7: score 是主窗口排序用的归一化分数，对 LLM 是噪音，投影到 AI 时剔除。
    #[test]
    fn items_to_llm_json_strips_score_field() {
        let items = vec![
            ItemResult {
                title: "file_a.txt".into(),
                subtitle: Some("C:\\dir".into()),
                payload: json!({ "path": "C:\\dir\\file_a.txt" }),
                score: Some(0.9),
            },
            ItemResult {
                title: "file_b.txt".into(),
                subtitle: None,
                payload: json!({}),
                score: None,
            },
        ];
        let json_text = super::items_to_llm_json(&items);
        // score 必须不出现
        assert!(
            !json_text.contains("score"),
            "items_to_llm_json 不应包含 score 字段: {json_text}"
        );
        // 语义字段保留
        assert!(json_text.contains("file_a.txt"));
        assert!(json_text.contains("file_b.txt"));
        assert!(json_text.contains("C:\\\\dir"));
    }

    #[test]
    fn items_to_llm_json_empty_returns_empty_array() {
        let json_text = super::items_to_llm_json(&[]);
        assert_eq!(json_text, "[]");
    }

    #[test]
    fn rig_projection_blob_produces_text_summary() {
        // 0.12.0: Blob → 文本摘要（不喂原始字节省 token）
        use rig_core::completion::message::ToolResultContent;
        let r = CapabilityResult::Blob {
            mime: "image/png".into(),
            bytes: vec![0x89, 0x50, 0x4E, 0x47],
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
        };
        let contents = r.to_rig_tool_result();
        let text = super::rig_tool_result_to_text(&contents);
        assert_eq!(text, "hello world");
    }

    #[test]
    fn rig_tool_result_to_text_handles_blob_summary() {
        let r = CapabilityResult::Blob {
            mime: "image/png".into(),
            bytes: vec![0u8; 2048], // 2 KB
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
}
