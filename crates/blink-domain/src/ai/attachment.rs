//! 聊天附件协议——kind 封闭、逐项校验、文本注入纯函数（0.27.9）。
//!
//! **边界**（phase 0.27 §3.3/§8.2）：
//! - 附件带 kind 的封闭类型（audio/text_file/image），各自携带 resource_ref
//!   与展示名；前端任意 path 不是授权凭据。
//! - wire 校验失败**逐项报错**，禁止静默丢项（取代 0.23.12 音频时代的
//!   `sanitize_attachments` 过滤式清洗）。
//! - 文本读取默认值：单文件硬上限 256 KiB；≤4000 估算 tokens 全文注入，
//!   超过截断为 ≤2000 的 UTF-8 安全前缀并附结构化截断说明；最多 8 项、
//!   文本合计 ≤8000 估算 tokens（当前模型整次请求预算由 context_status 侧校验）。
//!
//! 历史徽标（`AttachmentBadge`）只落展示名与种类，兼容旧音频纯字符串记录。

use crate::ai::token_budget::{estimate_text_tokens, utf8_safe_token_prefix};

/// 附件种类——wire 封闭枚举（§8.2）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttachmentKind {
    /// 本地音频（沿用 0.23.12 会话音频引用通道）。
    Audio,
    /// UTF-8 文本文件（注入正文 + `read_attachment_text` 续读）。
    TextFile,
    /// 图片（Vision 输入，0.27.10）。
    Image,
}

impl AttachmentKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Audio => "audio",
            Self::TextFile => "text_file",
            Self::Image => "image",
        }
    }
}

impl std::fmt::Display for AttachmentKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// `chat_prompt` 附件 wire 载荷（§8.2 `{ kind, ref, display }`）。
///
/// `ref` 是短期 bearer 句柄（`rref_` 前缀），仅注入本轮模型输入；
/// 不进入用户可见正文、标题输入或持久化历史（历史只落 `AttachmentBadge`）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct ChatAttachmentInput {
    pub kind: AttachmentKind,
    /// resource_ref token（wire 字段名 `ref`；Rust 侧 `r#ref`）。
    pub r#ref: String,
    pub display: String,
}

/// 历史徽标——只含展示名与种类（§3.3：非授权徽标）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AttachmentBadge {
    pub kind: AttachmentKind,
    pub name: String,
}

/// `open_chat` 入口侧草稿事务请求（0.27.9 §8.2 schema 扩展）。
///
/// 文件建议流转、剪贴板发送、「问 AI」三个入口统一经此扩展进入 chat，
/// 不另造前端直连执行链（§A3.5 多入口单执行语义）。ref 已由可信签发
/// 入口（picker / 选中项确认 / 剪贴板历史读取链）签发；正文与附件追加
/// 草稿、不自动发送。字段名与 capability schema 一致（snake_case）。
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct OpenChatDraft {
    /// 预填正文（追加当前草稿语义，不自动发送）。
    #[serde(default)]
    pub prefill: Option<String>,
    /// 附件（与 ChatAttachmentInput 同形）。
    #[serde(default)]
    pub attachments: Vec<ChatAttachmentInput>,
    /// 是否先新建对话再预填。
    #[serde(default)]
    pub new_conversation: bool,
}

/// 单轮允许的最大附件数（§3.3）。
pub const MAX_CHAT_ATTACHMENTS: usize = 8;

/// 单个文本附件硬上限（字节）——超过在 picker/预检阶段明确拒绝。
pub const TEXT_FILE_MAX_BYTES: u64 = 256 * 1024;

/// ≤ 此估算 tokens 全文注入。
pub const TEXT_FULL_TOKEN_LIMIT: usize = 4_000;

/// 超限后截断到的 UTF-8 安全前缀预算。
pub const TEXT_TRUNCATED_TOKEN_LIMIT: usize = 2_000;

/// 本轮全部文本附件合计估算 tokens 上限（超额需用户调整，不静默丢项）。
pub const TEXT_TOTAL_TOKEN_LIMIT: usize = 8_000;

/// 附件 wire 校验错误——逐项（index + 展示名 + 原因），不静默丢项。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachmentInvalid {
    pub index: usize,
    pub display: String,
    pub reason: AttachmentInvalidReason,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachmentInvalidReason {
    /// 非 `rref_` 形态或超长（≤128）。
    BadRef,
    /// ref 为空/空白。
    EmptyRef,
    /// display 去控制字符后为空。
    EmptyDisplay,
    /// 附件总数超过 `MAX_CHAT_ATTACHMENTS`。
    TooMany,
}

impl AttachmentInvalidReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::BadRef => "bad_ref",
            Self::EmptyRef => "empty_ref",
            Self::EmptyDisplay => "empty_display",
            Self::TooMany => "too_many",
        }
    }
}

/// wire 输入清洗 + 逐项校验：返回 (有效项, 无效项明细)。
///
/// 与旧 `sanitize_attachments` 的关键差异——无效项不再被静默过滤，
/// 而是返回给调用方逐项反馈（§5.9.2「无效/超限逐项报错，禁止静默丢项」）。
/// 超过 `MAX_CHAT_ATTACHMENTS` 的尾部条目按 `TooMany` 逐项报错，不截断。
pub fn validate_inputs(inputs: Vec<ChatAttachmentInput>) -> (Vec<ChatAttachmentInput>, Vec<AttachmentInvalid>) {
    let mut valid = Vec::with_capacity(inputs.len());
    let mut invalid = Vec::new();
    for (index, mut input) in inputs.into_iter().enumerate() {
        let display = sanitize_display(&input.display);
        let r#ref = input.r#ref.trim().to_string();
        let reason = if index >= MAX_CHAT_ATTACHMENTS {
            Some(AttachmentInvalidReason::TooMany)
        } else if r#ref.is_empty() {
            Some(AttachmentInvalidReason::EmptyRef)
        } else if !r#ref.starts_with("rref_") || r#ref.len() > 128 {
            Some(AttachmentInvalidReason::BadRef)
        } else if display.is_empty() {
            Some(AttachmentInvalidReason::EmptyDisplay)
        } else {
            None
        };
        if let Some(reason) = reason {
            invalid.push(AttachmentInvalid {
                index,
                display,
                reason,
            });
            continue;
        }
        input.r#ref = r#ref;
        input.display = display;
        valid.push(input);
    }
    (valid, invalid)
}

/// 展示名清洗：去控制字符、≤160 字符（对齐音频附件先例）。
pub fn sanitize_display(display: &str) -> String {
    display
        .chars()
        .filter(|c| !c.is_control())
        .take(160)
        .collect()
}

// ── 文本附件纯函数管道 ──────────────────────────────────────────────────────

/// 文本探测状态（与 read_text_file probe 同口径，输入为首 8 KiB 采样）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextProbeStatus {
    /// 空文件。
    Empty,
    /// 含 NUL——按二进制拒绝。
    Binary,
    /// 非 UTF-8（存在非法字节序列）。
    NonUtf8,
    /// UTF-8（含 BOM）。
    Utf8,
}

/// 探测采样可能恰好截断一个多字节字符；仅把「确定存在非法字节」判为非 UTF-8
/// （与 read_text_file::is_valid_utf8_probe 同语义）。
pub fn classify_text_probe(sample: &[u8]) -> TextProbeStatus {
    let without_bom = sample.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(sample);
    if sample.is_empty() {
        TextProbeStatus::Empty
    } else if sample.contains(&0) {
        TextProbeStatus::Binary
    } else if !matches!(std::str::from_utf8(without_bom), Ok(_)) {
        // 与 read_text_file 一致：尾部截断的多字节序列不算非法
        match std::str::from_utf8(without_bom) {
            Err(e) if e.error_len().is_none() => TextProbeStatus::Utf8,
            _ => TextProbeStatus::NonUtf8,
        }
    } else {
        TextProbeStatus::Utf8
    }
}

/// 文本附件注入结果——模型输入里的正文切片与截断元数据。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextInjection {
    /// 实际注入的文本（全文或 UTF-8 安全前缀）。
    pub text: String,
    /// 注入文本的估算 tokens。
    pub token_est: usize,
    /// 是否发生了截断。
    pub truncated: bool,
    /// 原文完整估算 tokens（截断时供说明）。
    pub original_token_est: usize,
}

/// 构建文本注入：≤`TEXT_FULL_TOKEN_LIMIT` 全文；超过截断为
/// ≤`TEXT_TRUNCATED_TOKEN_LIMIT` 的 UTF-8 安全前缀（§3.3）。
pub fn build_text_injection(full_text: &str) -> TextInjection {
    let original_token_est = estimate_text_tokens(full_text);
    if original_token_est <= TEXT_FULL_TOKEN_LIMIT {
        return TextInjection {
            text: full_text.to_string(),
            token_est: original_token_est,
            truncated: false,
            original_token_est,
        };
    }
    let prefix = utf8_safe_token_prefix(full_text, TEXT_TRUNCATED_TOKEN_LIMIT);
    let token_est = estimate_text_tokens(&prefix);
    TextInjection {
        text: prefix,
        token_est,
        truncated: true,
        original_token_est,
    }
}

/// 校验本轮文本附件合计预算（§3.3：文本合计默认 ≤8000 tokens）。
/// 返回超出额度（0 = 通过）。
pub fn text_total_overrun(injections: &[TextInjection]) -> usize {
    let total: usize = injections.iter().map(|i| i.token_est).sum();
    total.saturating_sub(TEXT_TOTAL_TOKEN_LIMIT)
}

// ── 模型输入组装 ─────────────────────────────────────────────────────────────

/// 已解引用的附件（发送时快照）——模型输入组装的唯一输入。
#[derive(Debug, Clone)]
pub enum ResolvedAttachment {
    Audio {
        r#ref: String,
        display: String,
    },
    TextFile {
        r#ref: String,
        display: String,
        injection: TextInjection,
    },
    Image {
        r#ref: String,
        display: String,
    },
}

impl ResolvedAttachment {
    pub fn r#ref(&self) -> &str {
        match self {
            Self::Audio { r#ref, .. } | Self::TextFile { r#ref, .. } | Self::Image { r#ref, .. } => {
                r#ref
            }
        }
    }

    pub fn display(&self) -> &str {
        match self {
            Self::Audio { display, .. }
            | Self::TextFile { display, .. }
            | Self::Image { display, .. } => display,
        }
    }

    pub fn kind(&self) -> AttachmentKind {
        match self {
            Self::Audio { .. } => AttachmentKind::Audio,
            Self::TextFile { .. } => AttachmentKind::TextFile,
            Self::Image { .. } => AttachmentKind::Image,
        }
    }
}

/// 附件上下文块——本轮模型输入的技术段（§3.3「附件仅注入明确附加的请求」）。
///
/// - 音频：ref + 转写提示（0.23.12 语义平移）。
/// - 文本：正文内容 + 范围/截断说明（用户可让模型 `read_attachment_text` 续读）。
/// - 图片：仅列引用（内容经 provider 适配层以多模态 part 注入，0.27.10）。
fn attachment_block_for(resolved: &ResolvedAttachment, index: usize) -> String {
    match resolved {
        ResolvedAttachment::Audio { r#ref, display } => format!(
            "[音频附件 {n}] file=\"{file}\" audio_ref=\"{r}\"",
            n = index + 1,
            file = display.replace('"', "'"),
            r = r#ref,
        ),
        ResolvedAttachment::TextFile {
            r#ref,
            display,
            injection,
        } => {
            let mut block = format!(
                "[文本附件 {n}] file=\"{file}\" resource_ref=\"{r}\"\n",
                n = index + 1,
                file = display.replace('"', "'"),
                r = r#ref,
            );
            if injection.truncated {
                block.push_str(&format!(
                    "（原文约 {orig} tokens 超过单文件注入上限，以下仅注入前 {est} tokens；\
                     可用 read_attachment_text 以 resource_ref 续读后续内容）\n",
                    orig = injection.original_token_est,
                    est = injection.token_est,
                ));
            }
            block.push_str("----- 文件内容开始 -----\n");
            block.push_str(&injection.text);
            block.push_str("\n----- 文件内容结束 -----");
            block
        }
        ResolvedAttachment::Image { r#ref, display } => format!(
            "[图片附件 {n}] file=\"{file}\" resource_ref=\"{r}\"（图片内容随本消息以多模态输入提供）",
            n = index + 1,
            file = display.replace('"', "'"),
            r = r#ref,
        ),
    }
}

/// 组装全部附件的上下文段（模型输入专用；用户可见正文不含此段）。
pub fn attachment_context_block(resolved: &[ResolvedAttachment]) -> Option<String> {
    if resolved.is_empty() {
        return None;
    }
    let has_audio = resolved
        .iter()
        .any(|a| matches!(a, ResolvedAttachment::Audio { .. }));
    let mut parts: Vec<String> = resolved
        .iter()
        .enumerate()
        .map(|(index, a)| attachment_block_for(a, index))
        .collect();
    if has_audio {
        parts.push("（以上音频附件可用 transcribe_audio 能力转写后处理）".to_string());
    }
    if resolved.iter().any(|a| {
        matches!(a, ResolvedAttachment::TextFile { injection, .. } if injection.truncated)
    }) {
        parts.push(
            "（存在被截断的文本附件；用 read_attachment_text 按游标续读未注入部分）".to_string(),
        );
    }
    Some(parts.join("\n"))
}

/// 组装本轮模型输入：附件上下文（仅本轮有效）+ 用户可见正文。
pub fn compose_model_input(visible: &str, resolved: &[ResolvedAttachment]) -> String {
    match attachment_context_block(resolved) {
        None => visible.to_string(),
        Some(block) => {
            if visible.is_empty() {
                block
            } else {
                format!("{block}\n{visible}")
            }
        }
    }
}

/// 徽标投影（持久化 + 前端附件卡）。
pub fn to_badges(resolved: &[ResolvedAttachment]) -> Vec<AttachmentBadge> {
    resolved
        .iter()
        .map(|a| AttachmentBadge {
            kind: a.kind(),
            name: a.display().to_string(),
        })
        .collect()
}

// ── 测试 ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn input(kind: AttachmentKind, r: &str, display: &str) -> ChatAttachmentInput {
        ChatAttachmentInput {
            kind,
            r#ref: r.to_string(),
            display: display.to_string(),
        }
    }

    #[test]
    fn validate_keeps_valid_and_reports_each_invalid() {
        let ok = input(AttachmentKind::TextFile, "rref_aaaabbbbccccdddd", "a.txt");
        let (valid, invalid) = validate_inputs(vec![
            ok.clone(),
            input(AttachmentKind::TextFile, "bearer-token", "b.txt"),
            input(AttachmentKind::TextFile, "   ", "c.txt"),
            input(AttachmentKind::TextFile, "rref_x", "\u{7}"),
        ]);
        assert_eq!(valid, vec![ok]);
        assert_eq!(invalid.len(), 3, "逐项报错，不静默丢项");
        assert_eq!(invalid[0].reason, AttachmentInvalidReason::BadRef);
        assert_eq!(invalid[1].reason, AttachmentInvalidReason::EmptyRef);
        assert_eq!(invalid[2].reason, AttachmentInvalidReason::EmptyDisplay);
        assert_eq!(invalid[2].display, "", "display 已清洗");
    }

    #[test]
    fn validate_reports_overflow_not_truncates() {
        let inputs = (0..10)
            .map(|i| input(AttachmentKind::Audio, &format!("rref_{i:020}"), "a.wav"))
            .collect();
        let (valid, invalid) = validate_inputs(inputs);
        assert_eq!(valid.len(), MAX_CHAT_ATTACHMENTS);
        assert_eq!(invalid.len(), 2, "超限逐项 TooMany，不静默截断");
        assert!(invalid
            .iter()
            .all(|i| i.reason == AttachmentInvalidReason::TooMany));
    }

    #[test]
    fn validate_sanitizes_display_control_chars() {
        let (_, invalid) = validate_inputs(vec![input(
            AttachmentKind::Audio,
            "rref_aaaabbbbccccdddd",
            "a\nb.wav",
        )]);
        assert!(invalid.is_empty());
    }

    #[test]
    fn probe_classifies_statuses() {
        assert_eq!(classify_text_probe(b""), TextProbeStatus::Empty);
        assert_eq!(classify_text_probe(b"hello"), TextProbeStatus::Utf8);
        assert_eq!(
            classify_text_probe(&[0xEF, 0xBB, 0xBF, b'h']),
            TextProbeStatus::Utf8,
            "BOM 剥离后按 UTF-8"
        );
        assert_eq!(classify_text_probe(b"a\0b"), TextProbeStatus::Binary);
        assert_eq!(
            classify_text_probe(&[0xFF, 0xFE, 0x41]),
            TextProbeStatus::NonUtf8,
            "UTF-16 BOM 判非 UTF-8"
        );
    }

    #[test]
    fn probe_tolerates_truncated_multibyte_tail() {
        // 「你」的 UTF-8 前两字节被采样截断——不算非法
        assert_eq!(classify_text_probe(&[0xE4, 0xBD]), TextProbeStatus::Utf8);
    }

    #[test]
    fn text_injection_within_limit_is_full() {
        let text = "你好".repeat(1000); // 2000 tokens
        let inj = build_text_injection(&text);
        assert!(!inj.truncated);
        assert_eq!(inj.token_est, 2000);
        assert_eq!(inj.text, text);
    }

    #[test]
    fn text_injection_over_limit_truncates_utf8_safe() {
        let text = "你".repeat(10_000); // 10000 tokens > 4000
        let inj = build_text_injection(&text);
        assert!(inj.truncated);
        assert!(inj.token_est <= TEXT_TRUNCATED_TOKEN_LIMIT);
        assert!(inj.token_est >= TEXT_TRUNCATED_TOKEN_LIMIT - 1);
        // UTF-8 安全：完整字符边界（chars 可正常重collect 回原文前缀）
        assert!(text.starts_with(&inj.text), "前缀截断");
        assert_eq!(inj.original_token_est, 10_000);
    }

    #[test]
    fn text_total_overrun_sums_injections() {
        let a = build_text_injection(&"好".repeat(3000));
        let b = build_text_injection(&"好".repeat(3000));
        assert_eq!(text_total_overrun(&[a.clone()]), 0);
        // 6000 > 5000？不——合计 6000 ≤ 8000
        assert_eq!(text_total_overrun(&[a, b]), 0);
        let c = build_text_injection(&"好".repeat(2001)); // 2001
        let d = build_text_injection(&"好".repeat(2001));
        let e = build_text_injection(&"好".repeat(2001));
        let f = build_text_injection(&"好".repeat(2001));
        assert_eq!(text_total_overrun(&[c, d, e, f]), 4, "8040 - 8000");
    }

    fn resolved_audio() -> ResolvedAttachment {
        ResolvedAttachment::Audio {
            r#ref: "rref_aaaabbbbccccdddd".into(),
            display: "录音.wav".into(),
        }
    }

    fn resolved_text(truncated: bool) -> ResolvedAttachment {
        let injection = if truncated {
            TextInjection {
                text: "前缀".into(),
                token_est: 2,
                truncated: true,
                original_token_est: 9000,
            }
        } else {
            TextInjection {
                text: "全文".into(),
                token_est: 2,
                truncated: false,
                original_token_est: 2,
            }
        };
        ResolvedAttachment::TextFile {
            r#ref: "rref_bbbbccccddddeeee".into(),
            display: "笔记.txt".into(),
            injection,
        }
    }

    #[test]
    fn compose_model_input_identity_without_attachments() {
        assert_eq!(compose_model_input("你好", &[]), "你好");
    }

    #[test]
    fn compose_model_input_prepends_text_file_content() {
        let input = compose_model_input("总结一下", &[resolved_text(false)]);
        assert!(input.contains("----- 文件内容开始 -----"), "{input}");
        assert!(input.contains("全文"), "{input}");
        assert!(input.contains("resource_ref=\"rref_bbbbccccddddeeee\""), "{input}");
        assert!(input.ends_with("总结一下"), "可见正文在附件块之后");
        assert!(!input.contains("read_attachment_text"), "未截断不给续读提示");
    }

    #[test]
    fn compose_model_input_marks_truncation_with_cursor_hint() {
        let input = compose_model_input("继续", &[resolved_text(true)]);
        assert!(input.contains("超过单文件注入上限"), "{input}");
        assert!(input.contains("read_attachment_text"), "截断时给续读指引");
        assert!(input.contains("9000"), "原文规模说明");
    }

    #[test]
    fn compose_model_input_keeps_audio_hint_and_neutralizes_quotes() {
        let audio = ResolvedAttachment::Audio {
            r#ref: "rref_aaaabbbbccccdddd".into(),
            display: "a\"b.wav".into(),
        };
        let input = compose_model_input("x", &[audio]);
        assert!(input.contains("transcribe_audio"), "{input}");
        assert!(!input.contains("a\"b.wav"), "引号被中和");
    }

    #[test]
    fn compose_model_input_image_lists_ref_only() {
        let image = ResolvedAttachment::Image {
            r#ref: "rref_ccccddddeeeeffff".into(),
            display: "截图.png".into(),
        };
        let input = compose_model_input("", &[image]);
        assert!(input.contains("[图片附件 1]"), "{input}");
        assert!(input.contains("多模态"), "{input}");
        assert!(!input.ends_with("\n"));
    }

    #[test]
    fn to_badges_projects_kind_and_name() {
        let badges = to_badges(&[resolved_audio(), resolved_text(false)]);
        assert_eq!(badges[0].kind, AttachmentKind::Audio);
        assert_eq!(badges[0].name, "录音.wav");
        assert_eq!(badges[1].kind, AttachmentKind::TextFile);
    }

    #[test]
    fn attachment_input_wire_roundtrip() {
        let json = serde_json::json!([
            {"kind": "text_file", "ref": "rref_x", "display": "a.txt"}
        ]);
        let parsed: Vec<ChatAttachmentInput> = serde_json::from_value(json).unwrap();
        assert_eq!(parsed[0].kind, AttachmentKind::TextFile);
        assert_eq!(parsed[0].r#ref, "rref_x");
        // 序列化回到同形 wire
        let back = serde_json::to_value(&parsed).unwrap();
        assert_eq!(back[0]["ref"], "rref_x");
        assert_eq!(back[0]["kind"], "text_file");
    }
}
