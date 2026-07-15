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

// ── rig 投影层（0.10 multi-turn 预备，0.9.7 仅定义不消费）─────────────────

/// rig ToolResultContent 投影——把 `CapabilityResult` 转成 rig 的 tool result 格式。
///
/// **0.10 Agent 窗口 multi-turn 流程**消费此函数：AI tool_call → Capability invoke →
/// 结果投影成 `ToolResultContent` → 喂回 LLM 做下一轮 completion。
///
/// **0.9.7 仅定义**，当前单轮流程走前端投影（`capability_result_to_entries`），
/// 不走 multi-turn。保证 0.10 接 Agent 窗口时"投影层已就绪"。
///
/// **Blob 投影策略**：用 `DocumentSourceKind::Raw`（原始字节），不 base64 编码——
/// provider 适配层（rig 内部）负责按 vendor 要求转 base64/raw，能力层不关心。
impl CapabilityResult {
    #[allow(dead_code)] // 0.10 multi-turn 消费；0.9.7 单轮流程走前端投影
    pub fn to_rig_tool_result(&self) -> Vec<rig_core::completion::message::ToolResultContent> {
        use rig_core::completion::message::{
            DocumentSourceKind, Image, ImageMediaType, MimeType, ToolResultContent,
        };

        match self {
            CapabilityResult::Text { content } => {
                vec![ToolResultContent::text(content)]
            }
            CapabilityResult::Items { items } => {
                // Items → 序列化 JSON 文本喂 LLM（模型读 JSON 上下文）
                let json = serde_json::to_string(items).unwrap_or_else(|_| "[]".to_string());
                vec![ToolResultContent::text(json)]
            }
            CapabilityResult::Blob { mime, bytes } => {
                // Blob → Image（Raw 字节，provider 适配层负责 base64 编码）
                let media_type = ImageMediaType::from_mime_type(mime);
                vec![ToolResultContent::Image(Image {
                    data: DocumentSourceKind::Raw(bytes.clone()),
                    media_type,
                    detail: None,
                    additional_params: None,
                })]
            }
            CapabilityResult::Done { summary } => {
                vec![ToolResultContent::text(summary)]
            }
        }
    }
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

    #[test]
    fn rig_projection_blob_produces_image() {
        use rig_core::completion::message::{DocumentSourceKind, MimeType, ToolResultContent};
        let r = CapabilityResult::Blob {
            mime: "image/png".into(),
            bytes: vec![0x89, 0x50, 0x4E, 0x47],
        };
        let contents = r.to_rig_tool_result();
        assert_eq!(contents.len(), 1);
        if let ToolResultContent::Image(img) = &contents[0] {
            // Blob → Raw 字节（不 base64 编码）
            assert!(matches!(img.data, DocumentSourceKind::Raw(_)));
            assert_eq!(img.media_type.as_ref().unwrap().to_mime_type(), "image/png");
        } else {
            panic!("Blob should project to Image");
        }
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
