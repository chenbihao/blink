//! rig 0.43 → 0.45 历史消息 JSON 兼容读取层（0.27.13）。
//!
//! rig 0.45 的 `completion::Message` serde 形状相对 0.43 有两处不兼容：
//! - **Reasoning 块**：0.43 是 `Sealed<Reasoning>`（`{"issuer","id","content":[…]}`
//!   多态 content 数组），0.45 是扁平 `{"text","redacted"}`；
//! - **CallId**：0.43 的 `ProviderCallId` 是 `{"call_id":"…","item_id":…}` 对象
//!   （`{"provider":{…}}` 包装），0.45 是纯字符串 `{"provider":"…"}`。
//!
//! 其余差异（assistant 消息的 `id`、ToolCall 的 `signature`/`additional_params`）
//! 在 0.45 侧是未知字段/宽容反序列化，无需处理。
//!
//! 策略（phase §6.3）：**旧库行不改写**，读取时先按 0.45 原生解析，失败再走
//! 本层的 JSON 形状升级后重试；两层都失败才报错，绝不静默跳行。签名/加密
//! reasoning 载荷与 dual-id 的 `item_id` 在 0.45 中无承载字段，升级时丢弃并
//! 打 debug 日志（跨版本重放本就只有文本语义可保）。

use rig_core::completion::Message;

/// 解析一列存储的 `messages.content` JSON 为 0.45 `Message`。
///
/// 兼容两种输入：0.45 原生格式、0.43 legacy 格式（自动升级形状）。
/// 两种格式都失败时返回带两层错误的 `Err`——调用方不得把它当空历史静默吞掉。
pub fn parse_message(json: &str) -> Result<Message, String> {
    match serde_json::from_str::<Message>(json) {
        Ok(message) => Ok(message),
        Err(native_error) => {
            let mut value: serde_json::Value = match serde_json::from_str(json) {
                Ok(value) => value,
                Err(_) => return Err(format!("消息 JSON 解析失败: {native_error}")),
            };
            let upgraded = upgrade_legacy_value(&mut value);
            match serde_json::from_value::<Message>(value) {
                Ok(message) => {
                    if upgraded {
                        tracing::debug!(
                            "message_compat: 已按 0.43 legacy 形状升级读取一条历史消息"
                        );
                    }
                    Ok(message)
                }
                Err(legacy_error) => Err(format!(
                    "消息反序列化失败（rig 0.45: {native_error}; 0.43 兼容: {legacy_error}）"
                )),
            }
        }
    }
}

/// 就地把 0.43 legacy 形状升级为 0.45 形状。返回是否发生改写。
fn upgrade_legacy_value(value: &mut serde_json::Value) -> bool {
    let mut changed = false;
    walk_value(value, &mut changed);
    changed
}

/// 深度遍历 JSON，识别并升级两种 legacy 形状（位置无关，兼容未来嵌套）。
fn walk_value(value: &mut serde_json::Value, changed: &mut bool) {
    match value {
        serde_json::Value::Object(map) => {
            if upgrade_call_id(map) {
                *changed = true;
                // call id 已是叶子，继续遍历其余字段
            }
            if upgrade_reasoning_block(map) {
                *changed = true;
                return; // reasoning 块整体已重写，无子结构可再遍历
            }
            for (_, child) in map.iter_mut() {
                walk_value(child, changed);
            }
        }
        serde_json::Value::Array(items) => {
            for item in items.iter_mut() {
                walk_value(item, changed);
            }
        }
        _ => {}
    }
}

/// 0.43 `CallId::Provider` 形状：`{"provider":{"call_id":"…","item_id":…}}`
/// → 0.45 `{"provider":"…"}`。`item_id`（OpenAI Responses dual-id）在 0.45
/// 无承载字段，丢弃。命中返回 true。
fn upgrade_call_id(map: &mut serde_json::Map<String, serde_json::Value>) -> bool {
    let Some(serde_json::Value::Object(inner)) = map.get("provider") else {
        return false;
    };
    let Some(serde_json::Value::String(call_id)) = inner.get("call_id") else {
        return false;
    };
    let had_item_id = inner.get("item_id").map_or(false, |v| !v.is_null());
    let call_id = call_id.clone();
    map.insert("provider".to_string(), serde_json::Value::String(call_id));
    if had_item_id {
        tracing::debug!(
            "message_compat: 丢弃 0.43 dual-id call 的 item_id（0.45 无承载字段）"
        );
    }
    true
}

/// 0.43 reasoning 块形状：`{"type":"reasoning","issuer":…,"id":…,"content":[…]}`
/// → 0.45 `{"type":"reasoning","text":…,"redacted":…}`。
///
/// 0.43 `ReasoningContent` 变体（tag=type, content=content, snake_case）：
/// - `text{text,signature?}` → 计入 text
/// - `summary(String)` → 计入 text（换行拼接）
/// - `encrypted(String)` / `redacted{data}` → redacted=true（载荷无承载字段）
fn upgrade_reasoning_block(map: &mut serde_json::Map<String, serde_json::Value>) -> bool {
    if map.get("type").and_then(|v| v.as_str()) != Some("reasoning") {
        return false;
    }
    let Some(serde_json::Value::Array(contents)) = map.get("content") else {
        return false;
    };
    let mut text_parts: Vec<String> = Vec::new();
    let mut redacted = false;
    let mut dropped_payload = false;
    for content in contents {
        let Some(serde_json::Value::Object(item)) = Some(content) else {
            continue;
        };
        match item.get("type").and_then(|v| v.as_str()) {
            Some("text") => {
                if let Some(text) = item
                    .get("content")
                    .and_then(|c| c.get("text"))
                    .and_then(|t| t.as_str())
                {
                    text_parts.push(text.to_string());
                }
            }
            Some("summary") => {
                if let Some(text) = item.get("content").and_then(|c| c.as_str()) {
                    if !text.is_empty() {
                        text_parts.push(text.to_string());
                    }
                }
            }
            Some("encrypted") | Some("redacted") => {
                redacted = true;
                dropped_payload = true;
            }
            _ => {}
        }
    }
    let mut upgraded = serde_json::Map::new();
    upgraded.insert(
        "type".to_string(),
        serde_json::Value::String("reasoning".to_string()),
    );
    upgraded.insert(
        "text".to_string(),
        serde_json::Value::String(text_parts.join("\n")),
    );
    if redacted {
        upgraded.insert("redacted".to_string(), serde_json::Value::Bool(true));
    }
    if dropped_payload {
        tracing::debug!(
            "message_compat: 0.43 加密/redacted reasoning 载荷无 0.45 承载字段，保留 redacted 标记"
        );
    }
    *map = upgraded;
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_043_text_user_and_assistant() {
        let user = r#"{"role":"user","content":[{"type":"text","text":"hello"}]}"#;
        let msg = parse_message(user).expect("0.43 user 文本行应可读");
        assert!(matches!(msg, Message::User { .. }));

        // 0.43 assistant：结构变体 {id, content}；0.45 为 AssistantMessage（id 为未知字段被忽略）
        let assistant = r#"{"role":"assistant","id":null,"content":[{"type":"text","text":"hi"}]}"#;
        let msg = parse_message(assistant).expect("0.43 assistant 文本行应可读");
        match msg {
            Message::Assistant(turn) => {
                assert_eq!(turn.content.len(), 1);
            }
            other => panic!("应为 assistant，实际 {other:?}"),
        }
    }

    /// 0.43 reasoning 块（Sealed 包装 + 多态 content 数组）。
    #[test]
    fn parses_043_reasoning_block() {
        let json = r#"{"role":"assistant","id":"msg_1","content":[
            {"type":"reasoning","issuer":"openai","id":null,"content":[
                {"type":"text","content":{"text":"思考中","signature":"sig123"}},
                {"type":"summary","content":"摘要"}
            ]},
            {"type":"text","text":"答案"}
        ]}"#;
        let msg = parse_message(json).expect("0.43 reasoning 行应可读");
        let Message::Assistant(turn) = msg else {
            panic!("应为 assistant");
        };
        let mut reasoning_text = None;
        for block in &turn.content {
            if let rig_core::completion::AssistantContent::Reasoning(r) = block {
                reasoning_text = Some(r.text.clone());
                assert!(!r.redacted);
            }
        }
        assert_eq!(reasoning_text.as_deref(), Some("思考中\n摘要"));
    }

    /// 0.43 redacted reasoning：载荷丢弃但 redacted 标记保留。
    #[test]
    fn parses_043_redacted_reasoning() {
        let json = r#"{"role":"assistant","content":[
            {"type":"reasoning","issuer":"anthropic","content":[
                {"type":"redacted","content":{"data":"opaque-bytes"}}
            ]}
        ]}"#;
        let msg = parse_message(json).expect("0.43 redacted reasoning 应可读");
        let Message::Assistant(turn) = msg else {
            panic!("应为 assistant");
        };
        match &turn.content[0] {
            rig_core::completion::AssistantContent::Reasoning(r) => {
                assert!(r.redacted);
                assert!(r.text.is_empty());
            }
            other => panic!("应为 reasoning 块，实际 {other:?}"),
        }
    }

    /// 0.43 tool_call（ProviderCallId 对象）与 tool_result（call 为 ProviderCallId
    /// 对象）。serde tag 是 lowercase 变体名（`toolcall`/`toolresult`，无下划线），
    /// 0.43/0.45 两版一致——块形状原生兼容，仅 CallId 需要兼容升级。
    #[test]
    fn parses_043_tool_call_and_result_roundtrip() {
        let assistant = r#"{"role":"assistant","content":[
            {"type":"toolcall","id":{"provider":{"call_id":"call_abc","item_id":"fc_abc"}},"function":{"name":"search","arguments":{"q":"blink"}},"signature":"sig"}
        ]}"#;
        let msg = parse_message(assistant).expect("0.43 toolcall 行应可读");
        let Message::Assistant(turn) = msg else {
            panic!("应为 assistant");
        };
        match &turn.content[0] {
            rig_core::completion::AssistantContent::ToolCall(call) => {
                assert_eq!(call.id.to_string(), "call_abc");
                assert_eq!(call.function.name.as_str(), "search");
            }
            other => panic!("应为 toolcall 块，实际 {other:?}"),
        }

        let user_result = r#"{"role":"user","content":[
            {"type":"toolresult","call":{"provider":{"call_id":"call_abc"}},"name":"search","content":[{"type":"text","text":"结果"}]}
        ]}"#;
        let msg = parse_message(user_result).expect("0.43 toolresult 行应可读");
        let Message::User { content } = msg else {
            panic!("应为 user");
        };
        match &content[0] {
            rig_core::message::UserContent::ToolResult(result) => {
                assert_eq!(result.call.to_string(), "call_abc");
                assert_eq!(result.name.as_str(), "search");
                assert!(!result.is_error);
            }
            other => panic!("应为 toolresult，实际 {other:?}"),
        }
    }

    /// 0.45 原生行不经过升级路径（直接命中快路径）。
    #[test]
    fn parses_native_045_directly() {
        let json = r#"{"role":"assistant","content":[{"type":"reasoning","text":"new style","redacted":false}]}"#;
        let msg = parse_message(json).expect("0.45 原生行应可读");
        let Message::Assistant(turn) = msg else {
            panic!("应为 assistant");
        };
        match &turn.content[0] {
            rig_core::completion::AssistantContent::Reasoning(r) => {
                assert_eq!(r.text, "new style");
            }
            other => panic!("应为 reasoning，实际 {other:?}"),
        }
    }

    /// 两层都失败：错误信息包含两侧原因，不静默。
    #[test]
    fn rejects_garbage_with_both_errors() {
        let err = parse_message("{not json").unwrap_err();
        assert!(err.contains("解析失败"), "错误应说明解析失败：{err}");
        let err = parse_message(r#"{"role":"alien","content":[]}"#).unwrap_err();
        assert!(err.contains("0.43 兼容"), "错误应包含兼容层尝试：{err}");
    }
}
