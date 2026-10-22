//! 编辑器 AI 整理的纯逻辑（0.23.4，phase 文档 §3.7 / §3.10）。
//!
//! **定位**：框架无关。本模块不做网络、不做事件、不做窗口；输入门、
//! max_tokens 推导、输出可应用判定都是纯函数，应用层
//! `crate::app::editor_transform::EditorTransformService` 照单执行。
//!
//! **冻结规则**（§3.7 / §3.10）：
//! - 整理只修正明显误识别、标点、分段和无意义口水词；不总结、扩写、翻译。
//! - 输入门：`estimate_text_tokens(input) ≤ 24,000`，超限不发送（不截断）。
//! - `max_tokens = clamp(input_est + 2048, 1024, context − input_est − margin)`；
//!   上下文不足以容纳下限时拒绝（模型窗口过小）。
//! - `temperature = 0.1`；无工具；system instruction 与用户正文分消息传递，
//!   不把正文拼进可闭合的 XML/Markdown 指令模板（正文中的指令不被执行）。
//! - `finish_reason` 截断（Length/ContentFilter）或文本为空 → 不生成可应用候选；
//!   `None/Other` 不宣称截断保护，记非敏感诊断。

use crate::ai::FinishReasonKind;
use crate::ai::memory::estimate_tokens;

/// AI 整理输入门（§3.10 冻结值）：估算输入 token 上限。
pub const MAX_INPUT_TOKENS: usize = 24_000;

/// max_tokens 推导余量（§3.10 公式的 `+2048`）。
const OUTPUT_TOKENS_MARGIN: u32 = 2048;

/// max_tokens 下限（§3.10 公式的 clamp 下界）。
const MIN_OUTPUT_TOKENS: u32 = 1024;

/// max_tokens 上界推导时的安全边距（context − input_est − margin）。
const CONTEXT_SAFETY_MARGIN: u32 = 1024;

/// 无记忆整理的固定采样温度（§3.7 冻结）。
pub const TRANSFORM_TEMPERATURE: f32 = 0.1;

/// 整理范围（§3.7 首版只有两个显式入口）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransformScope {
    /// 整理选中内容。
    Selection,
    /// 整理本次听写。
    Dictation,
}

impl TransformScope {
    /// 诊断/日志用的稳定标识。
    pub fn as_str(self) -> &'static str {
        match self {
            TransformScope::Selection => "selection",
            TransformScope::Dictation => "dictation",
        }
    }
}

/// 互译目标语言（0.27.6 §3.9）。
///
/// 自动方向：按输入主语言选择（中文为主 → 译英文；否则 → 译中文）；
/// 混合或不明确时由用户显式选择，显式值恒优先。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TranslateTarget {
    Chinese,
    English,
}

impl TranslateTarget {
    /// IPC wire 字符串（前端载荷 `"zh" | "en"`）。
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "zh" | "chinese" => Some(Self::Chinese),
            "en" | "english" => Some(Self::English),
            _ => None,
        }
    }

    /// 追加到互译 system instruction 的目标语言行（§3.9：按主语言/显式选择）。
    pub fn instruction_line(self) -> &'static str {
        match self {
            Self::Chinese => "目标语言：简体中文。",
            Self::English => "目标语言：英文（English）。",
        }
    }

    /// 候选/按钮标注用的目标标签。
    pub fn label(self) -> &'static str {
        match self {
            Self::Chinese => "zh",
            Self::English => "en",
        }
    }
}

/// 解析互译方向：显式选择优先；未指定时按输入主语言自动判定
/// （CJK 字符占比 ≥ 1/2 → 输入以中文为主 → 译英文，否则译中文）。
pub fn resolve_translate_target(
    explicit: Option<TranslateTarget>,
    input_text: &str,
) -> TranslateTarget {
    if let Some(target) = explicit {
        return target;
    }
    let mut cjk = 0usize;
    let mut latin = 0usize;
    for ch in input_text.chars() {
        if matches!(ch, '\u{4E00}'..='\u{9FFF}' | '\u{3400}'..='\u{4DBF}') {
            cjk += 1;
        } else if ch.is_ascii_alphabetic() {
            latin += 1;
        }
    }
    if latin == 0 && cjk == 0 {
        // 无文字信息（纯符号/数字）：默认译英文
        return TranslateTarget::English;
    }
    if cjk * 2 >= cjk + latin {
        TranslateTarget::English
    } else {
        TranslateTarget::Chinese
    }
}

/// 单次整理的执行计划——`plan_transform` 纯函数产物。
#[derive(Debug, Clone, PartialEq)]
pub struct TransformPlan {
    /// 请求发送的用户正文（原样，不拼接指令模板）。
    pub input_text: String,
    /// 估算输入 token 数（日志用，非敏感）。
    pub input_tokens: usize,
    /// 推导的生成上限。
    pub max_tokens: u32,
    /// 固定采样温度（§3.7）。
    pub temperature: f32,
}

/// 输入门与 max_tokens 推导（§3.10 冻结公式）。
///
/// - 超过 [`MAX_INPUT_TOKENS`]：拒绝发送（不截断，交上层报 `Unsupported`）；
/// - `context_window` 为 None：上下文未知，不宣称上限保护，
///   `max_tokens = input_est + 2048`（仍满足下限 1024）；
/// - clamp 上界 `context − input_est − margin` 小于下限：模型窗口过小，拒绝。
pub fn plan_transform(
    instruction: &str,
    input_text: String,
    context_window: Option<u32>,
) -> Result<TransformPlan, EditorTransformError> {
    // 0.27.6 §3.9：系统指令 + 自定义 prompt 与输入一起计入预算（指令在
    // 用户消息之外单独占 system 消息 token）
    let input_tokens = estimate_tokens(instruction) + estimate_tokens(&input_text);
    if input_tokens > MAX_INPUT_TOKENS {
        return Err(EditorTransformError::InputTooLarge {
            input_tokens,
            max_tokens: MAX_INPUT_TOKENS,
        });
    }

    let input_est = input_tokens as u32;
    let desired = input_est.saturating_add(OUTPUT_TOKENS_MARGIN);
    let max_tokens = match context_window {
        None => desired.max(MIN_OUTPUT_TOKENS),
        Some(context) => {
            let upper = context
                .saturating_sub(input_est)
                .saturating_sub(CONTEXT_SAFETY_MARGIN);
            if upper < MIN_OUTPUT_TOKENS {
                return Err(EditorTransformError::ContextTooSmall {
                    context,
                    input_tokens,
                });
            }
            desired.clamp(MIN_OUTPUT_TOKENS, upper)
        }
    };

    Ok(TransformPlan {
        input_text,
        input_tokens,
        max_tokens,
        temperature: TRANSFORM_TEMPERATURE,
    })
}

/// 整理输出的可应用性判定（§3.5.5 / §3.10 finish_reason 投影语义）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransformOutput {
    /// 终稿完整：可生成候选。
    Applicable { text: String },
    /// 输出被截断（Length/ContentFilter）——不生成可应用候选。
    Truncated { reason: &'static str },
    /// 输出为空——不生成可应用候选。
    Empty,
}

/// 判定 provider 返回是否可生成候选。
///
/// `finish_reason = None | Stop | ToolCalls | Other` 时只看文本非空；
/// `None/Other` 不宣称截断保护（记非敏感诊断由调用方完成）。
pub fn evaluate_transform_output(
    finish_reason: Option<FinishReasonKind>,
    text: Option<String>,
) -> TransformOutput {
    if let Some(reason) = finish_reason
        && reason.truncated_output()
    {
        return TransformOutput::Truncated {
            reason: match reason {
                FinishReasonKind::Length => "length",
                _ => "content_filter",
            },
        };
    }
    match text {
        Some(t) if !t.trim().is_empty() => TransformOutput::Applicable { text: t },
        _ => TransformOutput::Empty,
    }
}

/// 整理计划推导的结构化错误（§3.9 冻结集合内的投影源）。
///
/// AI 未配置/全局单活跃冲突/取消不在计划层判定——它们属于应用层编排
/// （`EditorTransformService`），直接产 `EditorError`。
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum EditorTransformError {
    /// 输入超过 AI 整理门（§3.10：不截断发送）。
    #[error("文本超过整理上限（约 {max_tokens} token）")]
    InputTooLarge {
        input_tokens: usize,
        max_tokens: usize,
    },
    /// 模型上下文窗口容不下"输入 + 最小输出"。
    #[error("模型上下文不足以整理该文本")]
    ContextTooSmall { context: u32, input_tokens: usize },
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 空指令基准（0.27.6 前的旧公式等价形态：指令 token=0）。
    const NO_INSTRUCTION: &str = "";

    #[test]
    fn plan_rejects_over_input_gate_without_truncation() {
        // 24K 门：中文按 ~1 token/字估算
        let text = "字".repeat(MAX_INPUT_TOKENS);
        assert!(
            plan_transform(NO_INSTRUCTION, text, Some(128_000)).is_ok(),
            "恰好达限应放行"
        );

        let over = "字".repeat(MAX_INPUT_TOKENS + 1);
        let err = plan_transform(NO_INSTRUCTION, over, Some(128_000)).unwrap_err();
        assert_eq!(
            err,
            EditorTransformError::InputTooLarge {
                input_tokens: MAX_INPUT_TOKENS + 1,
                max_tokens: MAX_INPUT_TOKENS,
            }
        );
    }

    #[test]
    fn plan_clamps_max_tokens_to_formula() {
        // input = 1000（中文 1000 字）→ desired = 3048，落在 [1024, upper] 内
        let plan = plan_transform(NO_INSTRUCTION, "字".repeat(1000), Some(32_000)).unwrap();
        assert_eq!(plan.input_tokens, 1000);
        assert_eq!(plan.max_tokens, 3048);
        assert_eq!(plan.temperature, TRANSFORM_TEMPERATURE);

        // desired 超 upper：clamp 到 context − input − margin
        //（28,000 窗口：upper = 28000−24000−1024 = 2976 ≥ 下限，合法钳制）
        let plan = plan_transform(NO_INSTRUCTION, "字".repeat(24_000), Some(28_000)).unwrap();
        assert_eq!(
            plan.max_tokens,
            28_000 - 24_000 - CONTEXT_SAFETY_MARGIN,
            "上限 = context − input_est − margin"
        );

        // 未知上下文：不宣称上限保护，仅 desired 与下限
        let plan = plan_transform(NO_INSTRUCTION, "字".repeat(100), None).unwrap();
        assert_eq!(plan.max_tokens, 100 + OUTPUT_TOKENS_MARGIN);

        // 小输入：desired = input + 2048 本就高于下限 1024（下限只在极小 desired
        // 时生效，保持 §3.10 公式原样）
        let plan = plan_transform(NO_INSTRUCTION, "hi".to_string(), None).unwrap();
        assert_eq!(plan.max_tokens, 1 + OUTPUT_TOKENS_MARGIN);
    }

    #[test]
    fn plan_rejects_context_too_small() {
        let err = plan_transform(NO_INSTRUCTION, "字".repeat(10_000), Some(10_500)).unwrap_err();
        assert!(matches!(err, EditorTransformError::ContextTooSmall { .. }));
    }

    #[test]
    fn output_truncated_by_finish_reason() {
        let text = Some("整理结果".to_string());
        assert_eq!(
            evaluate_transform_output(Some(FinishReasonKind::Stop), text.clone()),
            TransformOutput::Applicable {
                text: text.clone().unwrap()
            }
        );
        assert_eq!(
            evaluate_transform_output(Some(FinishReasonKind::Length), text.clone()),
            TransformOutput::Truncated { reason: "length" }
        );
        assert_eq!(
            evaluate_transform_output(Some(FinishReasonKind::ContentFilter), text),
            TransformOutput::Truncated {
                reason: "content_filter"
            }
        );
    }

    #[test]
    fn output_unknown_finish_reason_does_not_claim_truncation() {
        // None/Other 不宣称截断保护——只按文本判定
        let text = Some("整理结果".to_string());
        assert_eq!(
            evaluate_transform_output(None, text.clone()),
            TransformOutput::Applicable {
                text: text.unwrap()
            }
        );
        assert!(matches!(
            evaluate_transform_output(Some(FinishReasonKind::Other), Some("t".into())),
            TransformOutput::Applicable { .. }
        ));
    }

    #[test]
    fn output_empty_text_never_applicable() {
        assert_eq!(
            evaluate_transform_output(None, None),
            TransformOutput::Empty
        );
        assert_eq!(
            evaluate_transform_output(None, Some("   \n ".into())),
            TransformOutput::Empty
        );
        assert_eq!(
            evaluate_transform_output(Some(FinishReasonKind::Stop), None),
            TransformOutput::Empty
        );
    }

    #[test]
    fn instruction_tokens_count_into_input_budget() {
        // 0.27.6 §3.9：系统指令计入预算——同输入下指令越长 max_tokens 上界越紧
        let short = plan_transform("指令", "字".repeat(1000), Some(32_000)).unwrap();
        let long = plan_transform(&"指".repeat(500), "字".repeat(1000), Some(32_000)).unwrap();
        assert_eq!(short.input_tokens, 1002);
        assert_eq!(long.input_tokens, 1500);
        // 预算计入的两种表现：小额输入时 desired（input+2048）随之增大；
        // 大额输入时 clamp 上界（context-input-margin）被指令挤压得更紧
        let near_gate_a = plan_transform(NO_INSTRUCTION, "字".repeat(20_000), Some(32_000)).unwrap();
        let near_gate_b =
            plan_transform(&"指".repeat(4_000), "字".repeat(20_000), Some(32_000)).unwrap();
        assert_eq!(near_gate_a.max_tokens, 32_000 - 20_000 - 1_024);
        assert!(near_gate_b.max_tokens < near_gate_a.max_tokens, "指令占预算后输出上限收紧");

        // 指令 + 输入合计过门 → InputTooLarge（不截断发送）
        let err = plan_transform(&"指".repeat(10), "字".repeat(MAX_INPUT_TOKENS), Some(128_000))
            .unwrap_err();
        assert!(matches!(err, EditorTransformError::InputTooLarge { .. }));
    }

    #[test]
    fn translate_target_explicit_wins_and_auto_detects_by_cjk_ratio() {
        use super::TranslateTarget;
        // 显式优先
        assert_eq!(
            resolve_translate_target(Some(TranslateTarget::Chinese), "pure english text"),
            TranslateTarget::Chinese
        );
        // 中文为主 → 译英文
        assert_eq!(
            resolve_translate_target(None, "这是一段中文文本"),
            TranslateTarget::English
        );
        // 英文为主 → 译中文
        assert_eq!(
            resolve_translate_target(None, "mostly english words here"),
            TranslateTarget::Chinese
        );
        // 各半 → 判中文为主（≥ 1/2）
        assert_eq!(
            resolve_translate_target(None, "中文ab中文ab中文ab"),
            TranslateTarget::English
        );
        // 纯符号 → 默认译英文
        assert_eq!(resolve_translate_target(None, "123 !!! ???"), TranslateTarget::English);
        // wire 解析
        assert_eq!(TranslateTarget::parse("zh"), Some(TranslateTarget::Chinese));
        assert_eq!(TranslateTarget::parse("en"), Some(TranslateTarget::English));
        assert_eq!(TranslateTarget::parse("fr"), None);
    }

    #[test]
    fn scope_serializes_snake_case() {
        assert_eq!(
            serde_json::to_string(&TransformScope::Selection).unwrap(),
            "\"selection\""
        );
        assert_eq!(
            serde_json::to_string(&TransformScope::Dictation).unwrap(),
            "\"dictation\""
        );
        assert_eq!(TransformScope::Dictation.as_str(), "dictation");
    }
}
