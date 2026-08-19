//! 统一 token 预算模块（0.21.17）。
//!
//! 建立发送前唯一 `TokenEstimator` / `TokenBudget`，收敛 `memory.rs` 与 `prompt.rs`
//! 的两套重复启发式算法。所有发送前 token 估算统一走 `estimate_text_tokens`；
//! 完整请求预算走 `estimate_request_budget`。
//!
//! **设计约束**：
//! - 所有计算使用 `saturating_add` / `saturating_sub`，避免极小 context window 下溢
//! - 协议开销集中为常量，禁止散落 magic number
//! - 工具定义按实际送入 Rig 的 `ToolDefinition`/JSON Schema 投影计入
//! - 未知多模态内容不能默认为"精确 0"，降低 `confidence` 并采用保守余量
//! - `effective_input_limit = context_window - reserved_output - safety_margin`
//! - 百分比基于安全输入容量 `estimated_input / effective_input_limit`，而非 `context_window`

use crate::domain::ai::prompt::ToolPromptInfo;

// ── 常量 ─────────────────────────────────────────────────────────────────────

/// 保守默认 context limit（`ModelEntry.context_window` 缺失时使用）。
pub const FALLBACK_CONTEXT_LIMIT: usize = 32768;

/// 每条消息的固定协议开销（role 标签 + 分隔符等，启发式）。
const PER_MESSAGE_OVERHEAD: usize = 4;

/// 请求级固定开销（chat 格式包裹 + 系统角色定位等，启发式）。
const REQUEST_FIXED_OVERHEAD: usize = 3;

/// 每个工具定义的固定协议开销（函数声明包裹等，启发式）。
const PER_TOOL_OVERHEAD: usize = 8;

/// 每个 ToolCall 的 ID/关联开销（启发式）。
const PER_TOOL_CALL_ID_OVERHEAD: usize = 6;

/// 无配置时的保守输出预留 fallback。
const DEFAULT_RESERVED_OUTPUT: usize = 2048;

/// 安全余量占 context window 的比例（5%）。
const SAFETY_MARGIN_RATIO: f64 = 0.05;

/// 安全余量下界（至少 256 token）。
const SAFETY_MARGIN_MIN: usize = 256;

/// 安全余量上界（最多 4096 token）。
const SAFETY_MARGIN_MAX: usize = 4096;

/// 无法精确估算的多模态内容（图片等）的保守固定成本。
const MULTIMODAL_PENALTY: usize = 256;

/// 校准系数保守下界——估算值不能低于实际的 50%。
pub const CALIBRATION_RATIO_MIN: f64 = 0.5;

/// 校准系数保守上界——估算值不能高于实际的 200%。
pub const CALIBRATION_RATIO_MAX: f64 = 2.0;

/// 校准器最大样本数。
#[allow(dead_code)]
pub const CALIBRATION_MAX_SAMPLES: usize = 20;

/// 校准器启用所需最小样本数。
#[allow(dead_code)]
pub const CALIBRATION_MIN_SAMPLES: usize = 3;

// ── CJK 判断 ──────────────────────────────────────────────────────────────────

/// 判断字符是否为 CJK（中日韩统一表意文字 + 韩文 + 全角符号 + 假名）。
///
/// 合并 `memory.rs` 和 `prompt.rs` 两套 `is_cjk` 的并集，取最宽覆盖。
pub fn is_cjk(ch: char) -> bool {
    let code = ch as u32;
    matches!(
        code,
        0x3000..=0x33FF    // CJK 符号和标点 + 假名（平假名/片假名）
        | 0x3400..=0x4DBF  // CJK 扩展 A
        | 0x4E00..=0x9FFF  // CJK 统一表意文字
        | 0xAC00..=0xD7AF  // 韩文音节
        | 0xF900..=0xFAFF  // CJK 兼容表意文字
        | 0xFF00..=0xFFEF  // 半角/全角形式
        | 0x20000..=0x2A6DF // CJK 扩展 B
    )
}

// ── 文本 token 估算（唯一真源）──────────────────────────────────────────────

/// 启发式文本 token 估算——**全仓唯一发送前文本估算入口**。
///
/// 估算规则（合并 `memory.rs` 与 `prompt.rs` 两套算法的共识）：
/// - CJK 字符：1 token/char
/// - 其他字符（ASCII 字母/数字/标点/空格）：4 char/token（向上取整）
/// - 空文本返回 0
///
/// 误差 ±20% 可接受——压缩阈值留 buffer（80% 触发，不要求精确）。
/// 不引入 BPE tokenizer 依赖（`tiktoken-rs` ~1.8MB 词表对"仅监控告警"场景过重）。
pub fn estimate_text_tokens(text: &str) -> usize {
    if text.is_empty() {
        return 0;
    }
    let mut cjk = 0usize;
    let mut other = 0usize;
    for ch in text.chars() {
        if is_cjk(ch) {
            cjk += 1;
        } else {
            other += 1;
        }
    }
    // CJK: ~1 token/char; 非 CJK: ~4 char/token（向上取整）
    cjk.saturating_add(other.div_ceil(4))
}

// ── 预算类型 ──────────────────────────────────────────────────────────────────

/**
 * 估算置信度——告诉 UI 是否应展示"精确"还是"估算"。
 */
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EstimateConfidence {
    /// 纯文本内容，启发式估算可信。
    #[default]
    High,
    /// 包含部分无法精确估算的内容（如工具 Schema 复杂），但整体可控。
    Medium,
    /// 包含多模态内容或其他无法精确估算的因素，仅作粗略参考。
    Low,
}

/**
 * context limit 的来源——UI 必须区分真实规格与 fallback。
 */
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextLimitSource {
    /// 用户在 `ModelEntry.context_window` 中显式配置。
    Configured,
    /// 从 Provider 元数据获取（未来扩展预留）。
    #[allow(dead_code)]
    ProviderMetadata,
    /// `ModelEntry.context_window` 缺失，使用 32K 保守 fallback。
    #[default]
    Fallback,
}

/// token 预算分项拆解。
#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct TokenBreakdown {
    /// 历史消息 token（含 System/User/Assistant/ToolCall/ToolResult 文本）。
    pub history_tokens: usize,
    /// 系统提示词（preamble）token。
    pub system_tokens: usize,
    /// 当前待发消息 token。
    pub pending_tokens: usize,
    /// 工具定义 token（name + description + parameters JSON Schema）。
    pub tools_tokens: usize,
    /// 消息协议开销 token（role 标签、分隔符、ToolCall ID 等）。
    pub protocol_overhead_tokens: usize,
    /// 多模态内容保守估算 token（图片等无法精确估算的内容）。
    pub multimodal_tokens: usize,
}

/// 完整 token 预算——发送前对一次请求的完整估算。
#[derive(Clone, Debug, serde::Serialize)]
pub struct TokenBudget {
    /// 分项拆解。
    pub breakdown: TokenBreakdown,
    /// 估算的输入 token 总数（history + system + pending + tools + overhead + multimodal，
    /// 已应用 calibration_ratio）。
    pub estimated_input_tokens: usize,
    /// 未应用 calibration_ratio 的输入基线（history+system+pending 的原始估算 + tools + overhead + multimodal）。
    /// 用于校准器采样，避免校准生效后 ratio 被拉向 1.0 导致反馈回路。
    pub raw_estimated_input_tokens: usize,
    /// 输出 token 预留。
    pub reserved_output_tokens: usize,
    /// 安全余量 token。
    pub safety_margin_tokens: usize,
    /// context window 上限。
    pub context_limit: usize,
    /// 有效输入上限 = context_limit - reserved_output - safety_margin（饱和减法）。
    pub effective_input_limit: usize,
    /// 安全剩余 token = effective_input_limit - estimated_input_tokens（饱和减法）。
    pub remaining_tokens: usize,
    /// 安全输入预算占用百分比（0-100，基于 effective_input_limit）。
    pub usage_percent: u8,
    /// 估算置信度。
    pub confidence: EstimateConfidence,
    /// context limit 来源。
    pub context_limit_source: ContextLimitSource,
}

// ── 工具定义 token 估算 ───────────────────────────────────────────────────────

/// 估算单个工具定义的 token 数。
///
/// 工具定义发送给 Provider 时包含 `name` + `description` + `parameters` JSON Schema。
/// 此函数估算这三部分的 token 总和，加上固定协议开销。
pub fn estimate_tool_tokens(tool: &ToolPromptInfo) -> usize {
    let name_tokens = estimate_text_tokens(&tool.name);
    let desc_tokens = estimate_text_tokens(&tool.description);
    let params_tokens = estimate_text_tokens(&tool.parameters.to_string());
    let hint_tokens = tool
        .hint
        .as_ref()
        .map(|h| estimate_text_tokens(h))
        .unwrap_or(0);

    name_tokens
        .saturating_add(desc_tokens)
        .saturating_add(params_tokens)
        .saturating_add(hint_tokens)
        .saturating_add(PER_TOOL_OVERHEAD)
}

/// 估算工具集合的 token 总数。
///
/// `tools` 应是本轮实际送入 Rig 的工具集合（经 AI allowlist 过滤后的）。
/// 空工具池返回 0（无请求级工具开销——协议级开销由 `protocol_overhead` 覆盖）。
pub fn estimate_tools_tokens(tools: &[ToolPromptInfo]) -> usize {
    tools.iter().map(estimate_tool_tokens).sum()
}

// ── 输出预留 ──────────────────────────────────────────────────────────────────

/// 解析输出预留 token 数。
///
/// 优先级：
/// 1. 当前请求显式 `max_tokens`
/// 2. 当前模型默认 `max_tokens`
/// 3. 无配置时使用 `DEFAULT_RESERVED_OUTPUT`
///
/// fallback 不得超过 context window（饱和）。
pub fn resolve_reserved_output(
    request_max_tokens: Option<u32>,
    model_max_tokens: Option<u32>,
    context_limit: usize,
) -> usize {
    let raw = request_max_tokens
        .or(model_max_tokens)
        .map(|v| v as usize)
        .unwrap_or(DEFAULT_RESERVED_OUTPUT);

    // fallback 不得超过 context window
    raw.min(context_limit)
}

// ── 安全余量 ──────────────────────────────────────────────────────────────────

/// 计算安全余量 token 数。
///
/// 使用 context window 的 5%，并限制在 [256, 4096] 范围内。
/// 极小 context window（如 100）时下界 256 可能超过 context window 本身，
/// 此时饱和到 context_limit / 2（至少留一半给输入）。
pub fn compute_safety_margin(context_limit: usize) -> usize {
    let ratio_based = (context_limit as f64 * SAFETY_MARGIN_RATIO) as usize;
    let clamped = ratio_based.clamp(SAFETY_MARGIN_MIN, SAFETY_MARGIN_MAX);

    // 极小 context window：安全余量不超过 context_limit 的一半
    clamped.min(context_limit / 2)
}

// ── 预算输入 ──────────────────────────────────────────────────────────────────

/// `estimate_request_budget` 的输入参数。
#[derive(Clone, Debug)]
pub struct TokenBudgetInput<'a> {
    /// 历史消息文本（已提取纯文本，不含 rig Message 序列化开销）。
    pub history_texts: &'a [String],
    /// 系统提示词（preamble）文本。
    pub system_prompt: Option<&'a str>,
    /// 当前待发用户消息文本。
    pub pending_message: Option<&'a str>,
    /// 本轮实际送入 Rig 的工具集合。
    pub tools: &'a [ToolPromptInfo],
    /// 历史消息中包含的 ToolCall 数量（用于 ID/关联开销估算）。
    pub tool_call_count: usize,
    /// 历史消息中是否包含多模态内容（图片等）。
    pub has_multimodal: bool,
    /// context window 大小。
    pub context_window: Option<u32>,
    /// 当前请求显式 max_tokens。
    pub request_max_tokens: Option<u32>,
    /// 当前模型默认 max_tokens。
    pub model_max_tokens: Option<u32>,
    /// 校准系数（如有，从 `UsageCalibrator` 获取）。
    pub calibration_ratio: Option<f64>,
}

// ── 预算计算 ──────────────────────────────────────────────────────────────────

/// 完整请求预算估算——**对话请求的唯一预算入口**。
///
/// 计算流程：
/// 1. 确定 context_limit + 来源
/// 2. 计算 reserved_output + safety_margin
/// 3. 计算 effective_input_limit = context_limit - reserved_output - safety_margin
/// 4. 估算各分项 token（history + system + pending + tools + overhead + multimodal）
/// 5. 如有校准系数，应用到启发式内容估算
/// 6. 计算 remaining + usage_percent
/// 7. 确定 confidence
///
/// 所有减法使用 `saturating_sub`，避免下溢。
pub fn estimate_request_budget(input: TokenBudgetInput) -> TokenBudget {
    // 1. context_limit + 来源
    let (context_limit, source) = match input.context_window {
        Some(cw) if cw > 0 => (cw as usize, ContextLimitSource::Configured),
        _ => (FALLBACK_CONTEXT_LIMIT, ContextLimitSource::Fallback),
    };

    // 2. reserved_output + safety_margin
    let reserved_output = resolve_reserved_output(
        input.request_max_tokens,
        input.model_max_tokens,
        context_limit,
    );
    let safety_margin = compute_safety_margin(context_limit);

    // 3. effective_input_limit
    let effective_input_limit = context_limit
        .saturating_sub(reserved_output)
        .saturating_sub(safety_margin);

    // 4. 分项估算
    let mut history_tokens = 0usize;
    for text in input.history_texts {
        history_tokens = history_tokens.saturating_add(estimate_text_tokens(text));
    }

    let system_tokens = input.system_prompt.map(estimate_text_tokens).unwrap_or(0);

    let pending_tokens = input.pending_message.map(estimate_text_tokens).unwrap_or(0);

    let tools_tokens = estimate_tools_tokens(input.tools);

    // 协议开销：每条消息固定开销 + 请求级固定开销 + ToolCall ID 开销
    let message_count = input.history_texts.len()
        + if input.system_prompt.is_some() { 1 } else { 0 }
        + if input.pending_message.is_some() {
            1
        } else {
            0
        };
    let protocol_overhead_tokens = message_count
        .saturating_mul(PER_MESSAGE_OVERHEAD)
        .saturating_add(REQUEST_FIXED_OVERHEAD)
        .saturating_add(
            input
                .tool_call_count
                .saturating_mul(PER_TOOL_CALL_ID_OVERHEAD),
        );

    // 多模态：保守固定成本
    let multimodal_tokens = if input.has_multimodal {
        MULTIMODAL_PENALTY
    } else {
        0
    };

    // 5. 校准系数应用到内容估算（history + system + pending）
    let calibration = input.calibration_ratio.unwrap_or(1.0);
    let calibrated_history = apply_calibration(history_tokens, calibration);
    let calibrated_system = apply_calibration(system_tokens, calibration);
    let calibrated_pending = apply_calibration(pending_tokens, calibration);

    // 6. 估算输入总计
    let estimated_input_tokens = calibrated_history
        .saturating_add(calibrated_system)
        .saturating_add(calibrated_pending)
        .saturating_add(tools_tokens)
        .saturating_add(protocol_overhead_tokens)
        .saturating_add(multimodal_tokens);

    // raw_estimated_input_tokens：未应用 calibration_ratio 的基线，供校准器采样
    let raw_estimated_input_tokens = history_tokens
        .saturating_add(system_tokens)
        .saturating_add(pending_tokens)
        .saturating_add(tools_tokens)
        .saturating_add(protocol_overhead_tokens)
        .saturating_add(multimodal_tokens);

    // remaining
    let remaining_tokens = effective_input_limit.saturating_sub(estimated_input_tokens);

    // usage_percent：基于 effective_input_limit
    let usage_percent = (estimated_input_tokens.saturating_mul(100))
        .checked_div(effective_input_limit)
        .map(|v| v.min(100) as u8)
        .unwrap_or(100);

    // 7. confidence
    let confidence = if input.has_multimodal {
        EstimateConfidence::Low
    } else if !input.tools.is_empty() || input.tool_call_count > 0 {
        EstimateConfidence::Medium
    } else {
        EstimateConfidence::High
    };

    TokenBudget {
        breakdown: TokenBreakdown {
            history_tokens: calibrated_history,
            system_tokens: calibrated_system,
            pending_tokens: calibrated_pending,
            tools_tokens,
            protocol_overhead_tokens,
            multimodal_tokens,
        },
        estimated_input_tokens,
        raw_estimated_input_tokens,
        reserved_output_tokens: reserved_output,
        safety_margin_tokens: safety_margin,
        context_limit,
        effective_input_limit,
        remaining_tokens,
        usage_percent,
        confidence,
        context_limit_source: source,
    }
}

/// 应用校准系数到启发式估算值。
///
/// 校准只修正内容估算（history/system/pending），不能覆盖工具、输出预留和安全余量。
/// 系数被限制在 [CALIBRATION_RATIO_MIN, CALIBRATION_RATIO_MAX] 范围内。
fn apply_calibration(raw_tokens: usize, ratio: f64) -> usize {
    let clamped_ratio = ratio.clamp(CALIBRATION_RATIO_MIN, CALIBRATION_RATIO_MAX);
    let adjusted = (raw_tokens as f64 * clamped_ratio) as usize;
    adjusted.max(raw_tokens.min(1)) // 至少保留原始值的最小下界
}

// ── memory 裁剪预算 ───────────────────────────────────────────────────────────

/// 为 memory 裁剪计算安全的目标 token 数。
///
/// memory 裁剪必须预先扣除 system、tools、pending、输出预留和安全余量，
/// 避免"历史裁剪到 80%，加上工具后仍超限"。
///
/// 返回 history 可用的 token 上限。
pub fn compute_history_token_budget(
    context_limit: usize,
    system_tokens: usize,
    tools_tokens: usize,
    pending_tokens: usize,
    reserved_output: usize,
    safety_margin: usize,
) -> usize {
    let non_history = system_tokens
        .saturating_add(tools_tokens)
        .saturating_add(pending_tokens)
        .saturating_add(reserved_output)
        .saturating_add(safety_margin);

    context_limit.saturating_sub(non_history)
}

// ── 统一 Usage 类型（重导出 message::Usage）──────────────────────────────────

/// **全仓唯一生产 Usage 类型**——重导出 `message::Usage`。
///
/// 0.21.17: `Usage` 已删除，所有代码统一使用 `message::Usage`。
/// `from_rig_usage` / `unreported` / `add` / `has_real_usage` 方法在 `message::Usage` 上。
pub use crate::domain::ai::message::Usage;

// ── 真实 usage 校准器 ──────────────────────────────────────────────────────────

/// 进程内、有界的 provider/model 级 usage 校准器。
///
/// 为每个 `provider_id + model_id` 维护最近 `CALIBRATION_MAX_SAMPLES` 个有效样本，
/// 在至少 `CALIBRATION_MIN_SAMPLES` 个样本后启用校准。
///
/// 校准只应用于启发式内容估算，不能覆盖工具、输出预留和安全余量。
pub struct UsageCalibrator {
    samples: std::sync::RwLock<HashMap<(String, String), Vec<CalibrationSample>>>,
}

/// 单个校准样本。
#[derive(Clone, Debug)]
struct CalibrationSample {
    /// 发送前估算的输入 token 数。
    estimated_input_tokens: u32,
    /// 响应后实际的输入 token 数。
    actual_input_tokens: u32,
}

impl Default for UsageCalibrator {
    fn default() -> Self {
        Self::new()
    }
}

#[allow(dead_code)]
impl UsageCalibrator {
    pub fn new() -> Self {
        Self {
            samples: std::sync::RwLock::new(HashMap::new()),
        }
    }

    /// 记录一次请求的估算值和实际值。
    ///
    /// 只有 `actual.reported` 为 true 且 `actual.input_tokens > 0` 时才采样。
    /// 失败、取消、零 usage、明显无效数据不采样。
    pub fn record(
        &self,
        provider_id: &str,
        model_id: &str,
        estimated_input_tokens: u32,
        actual: &Usage,
    ) {
        if !actual.reported || actual.input_tokens == 0 {
            return;
        }

        // 估算值也不能为零（否则 ratio 无意义）
        if estimated_input_tokens == 0 {
            return;
        }

        let sample = CalibrationSample {
            estimated_input_tokens,
            actual_input_tokens: actual.input_tokens,
        };

        let key = (provider_id.to_string(), model_id.to_string());
        let mut map = self.samples.write().expect("calibrator lock poisoned");
        let samples = map.entry(key).or_default();
        samples.push(sample);

        // 保留最近 CALIBRATION_MAX_SAMPLES 个
        if samples.len() > CALIBRATION_MAX_SAMPLES {
            let drop_count = samples.len() - CALIBRATION_MAX_SAMPLES;
            samples.drain(0..drop_count);
        }
    }

    /// 获取指定 provider/model 的校准系数。
    ///
    /// 返回 `None` 表示样本不足，不应启用校准。
    /// 返回 `Some(ratio)` 表示估算值应乘以此系数。
    ///
    /// 使用截尾均值（去掉最大最小后取均值），避免异常值污染。
    /// ratio 被限制在 [CALIBRATION_RATIO_MIN, CALIBRATION_RATIO_MAX] 范围内。
    pub fn get_ratio(&self, provider_id: &str, model_id: &str) -> Option<f64> {
        let map = self.samples.read().expect("calibrator lock poisoned");
        let samples = map.get(&(provider_id.to_string(), model_id.to_string()))?;

        if samples.len() < CALIBRATION_MIN_SAMPLES {
            return None;
        }

        // 计算 ratio = actual / estimated
        let ratios: Vec<f64> = samples
            .iter()
            .map(|s| s.actual_input_tokens as f64 / s.estimated_input_tokens as f64)
            .collect();

        // 截尾均值：去掉最大最小后取均值
        let trimmed_mean = if ratios.len() <= 2 {
            ratios.iter().sum::<f64>() / ratios.len() as f64
        } else {
            let mut sorted = ratios.clone();
            sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            let trimmed = &sorted[1..sorted.len() - 1];
            trimmed.iter().sum::<f64>() / trimmed.len() as f64
        };

        // 限制到保守范围
        let clamped = trimmed_mean.clamp(CALIBRATION_RATIO_MIN, CALIBRATION_RATIO_MAX);
        Some(clamped)
    }

    /// 清除指定 provider/model 的样本（模型/配置切换后调用）。
    pub fn clear(&self, provider_id: &str, model_id: &str) {
        let mut map = self.samples.write().expect("calibrator lock poisoned");
        map.remove(&(provider_id.to_string(), model_id.to_string()));
    }

    /// 清除所有样本。
    #[allow(dead_code)]
    pub fn clear_all(&self) {
        let mut map = self.samples.write().expect("calibrator lock poisoned");
        map.clear();
    }
}

use std::collections::HashMap;

// ── 单测 ───────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::ai::prompt::ToolPromptInfo;
    use serde_json::json;

    // ── estimate_text_tokens ──

    #[test]
    fn estimate_text_tokens_empty() {
        assert_eq!(estimate_text_tokens(""), 0);
    }

    #[test]
    fn estimate_text_tokens_pure_ascii() {
        // 22 ASCII chars → div_ceil(22, 4) = 6
        assert_eq!(estimate_text_tokens("Hello world, test 123!"), 6);
    }

    #[test]
    fn estimate_text_tokens_pure_cjk() {
        // 5 CJK chars → 5 tokens
        assert_eq!(estimate_text_tokens("你好世界啊"), 5);
    }

    #[test]
    fn estimate_text_tokens_mixed() {
        // 3 CJK (3) + 8 ASCII (2) = 5
        assert_eq!(estimate_text_tokens("你好啊 hello!"), 5);
    }

    #[test]
    fn estimate_text_tokens_emoji() {
        // Emoji 不是 CJK，按 4 char/token 计算
        // "hello 🌍" = 6 ASCII + 1 emoji = 7 non-CJK → div_ceil(7, 4) = 2
        assert_eq!(estimate_text_tokens("hello 🌍"), 2);
    }

    #[test]
    fn estimate_text_tokens_punctuation_and_whitespace() {
        // 纯标点和空格：15 chars → div_ceil(15, 4) = 4
        assert_eq!(estimate_text_tokens("  , . ; : ! ?  "), 4);
    }

    #[test]
    fn estimate_text_tokens_long_json() {
        let json = r#"{"name":"test","value":123,"nested":{"key":"value"}}"#;
        let tokens = estimate_text_tokens(json);
        assert!(tokens > 0);
        // 52 chars → div_ceil(52, 4) = 13
        assert_eq!(tokens, 13);
    }

    #[test]
    fn estimate_text_tokens_no_overflow() {
        // 超长文本不应溢出
        let huge = "a".repeat(1_000_000);
        let tokens = estimate_text_tokens(&huge);
        assert!(tokens > 0);
        assert!(tokens <= 1_000_000);
    }

    // ── TokenBudget ──

    fn make_tool(name: &str, desc: &str, params: serde_json::Value) -> ToolPromptInfo {
        ToolPromptInfo {
            name: name.to_string(),
            description: desc.to_string(),
            parameters: params,
            hint: None,
        }
    }

    #[test]
    fn budget_empty_history_empty_tools() {
        let budget = estimate_request_budget(TokenBudgetInput {
            history_texts: &[],
            system_prompt: None,
            pending_message: None,
            tools: &[],
            tool_call_count: 0,
            has_multimodal: false,
            context_window: Some(8192),
            request_max_tokens: None,
            model_max_tokens: None,
            calibration_ratio: None,
        });

        assert_eq!(budget.breakdown.history_tokens, 0);
        assert_eq!(budget.breakdown.tools_tokens, 0);
        assert_eq!(budget.estimated_input_tokens, REQUEST_FIXED_OVERHEAD);
        assert!(budget.remaining_tokens > 0);
        assert_eq!(budget.usage_percent, 0);
        assert_eq!(budget.confidence, EstimateConfidence::High);
        assert_eq!(budget.context_limit_source, ContextLimitSource::Configured);
    }

    #[test]
    fn budget_protocol_overhead_grows_with_messages() {
        let one = estimate_request_budget(TokenBudgetInput {
            history_texts: &["hello".to_string()],
            system_prompt: None,
            pending_message: None,
            tools: &[],
            tool_call_count: 0,
            has_multimodal: false,
            context_window: Some(8192),
            request_max_tokens: None,
            model_max_tokens: None,
            calibration_ratio: None,
        });

        let three = estimate_request_budget(TokenBudgetInput {
            history_texts: &["hello".to_string(), "world".to_string(), "test".to_string()],
            system_prompt: Some("system"),
            pending_message: Some("pending"),
            tools: &[],
            tool_call_count: 0,
            has_multimodal: false,
            context_window: Some(8192),
            request_max_tokens: None,
            model_max_tokens: None,
            calibration_ratio: None,
        });

        assert!(three.breakdown.protocol_overhead_tokens > one.breakdown.protocol_overhead_tokens);
    }

    #[test]
    fn budget_tools_increase_tokens() {
        let no_tools = estimate_request_budget(TokenBudgetInput {
            history_texts: &[],
            system_prompt: None,
            pending_message: None,
            tools: &[],
            tool_call_count: 0,
            has_multimodal: false,
            context_window: Some(8192),
            request_max_tokens: None,
            model_max_tokens: None,
            calibration_ratio: None,
        });

        let with_tools = estimate_request_budget(TokenBudgetInput {
            history_texts: &[],
            system_prompt: None,
            pending_message: None,
            tools: &[make_tool(
                "get_weather",
                "查询天气",
                json!({"type":"object","properties":{"city":{"type":"string"}}}),
            )],
            tool_call_count: 0,
            has_multimodal: false,
            context_window: Some(8192),
            request_max_tokens: None,
            model_max_tokens: None,
            calibration_ratio: None,
        });

        assert!(with_tools.breakdown.tools_tokens > 0);
        assert!(with_tools.estimated_input_tokens > no_tools.estimated_input_tokens);
    }

    #[test]
    fn budget_tool_schema_growth_monotonic() {
        let small = make_tool(
            "a",
            "x",
            json!({"type":"object","properties":{"p":{"type":"string"}}}),
        );
        let big = make_tool(
            "a",
            &"x".repeat(100),
            json!({"type":"object","properties":{"p1":{"type":"string","description":"long description here"},"p2":{"type":"number"}}}),
        );

        assert!(estimate_tool_tokens(&big) > estimate_tool_tokens(&small));
    }

    #[test]
    fn budget_output_reserve() {
        let budget = estimate_request_budget(TokenBudgetInput {
            history_texts: &[],
            system_prompt: None,
            pending_message: None,
            tools: &[],
            tool_call_count: 0,
            has_multimodal: false,
            context_window: Some(8192),
            request_max_tokens: Some(4096),
            model_max_tokens: None,
            calibration_ratio: None,
        });

        assert_eq!(budget.reserved_output_tokens, 4096);
        assert_eq!(
            budget.effective_input_limit,
            8192 - 4096 - compute_safety_margin(8192)
        );
    }

    #[test]
    fn budget_safety_margin() {
        let budget = estimate_request_budget(TokenBudgetInput {
            history_texts: &[],
            system_prompt: None,
            pending_message: None,
            tools: &[],
            tool_call_count: 0,
            has_multimodal: false,
            context_window: Some(8192),
            request_max_tokens: None,
            model_max_tokens: None,
            calibration_ratio: None,
        });

        // 8192 * 0.05 = 409.6 → 409, clamped to [256, 4096] → 409
        let expected = (8192.0 * 0.05) as usize;
        assert_eq!(budget.safety_margin_tokens, expected);
    }

    #[test]
    fn budget_tiny_context_window_no_underflow() {
        let budget = estimate_request_budget(TokenBudgetInput {
            history_texts: &["some text here".to_string()],
            system_prompt: Some("system"),
            pending_message: Some("user message"),
            tools: &[make_tool("tool", "description", json!({}))],
            tool_call_count: 2,
            has_multimodal: true,
            context_window: Some(1),
            request_max_tokens: Some(100),
            model_max_tokens: None,
            calibration_ratio: None,
        });

        // 不应 panic，不应下溢
        assert_eq!(budget.context_limit, 1);
        assert_eq!(budget.effective_input_limit, 0); // 1 - 1(min(100,1)) - 0 = 0
        assert_eq!(budget.remaining_tokens, 0); // 0 - estimated = 0 (saturating)
        assert_eq!(budget.usage_percent, 100); // effective=0 → 100%
    }

    #[test]
    fn budget_context_window_one() {
        let budget = estimate_request_budget(TokenBudgetInput {
            history_texts: &[],
            system_prompt: None,
            pending_message: None,
            tools: &[],
            tool_call_count: 0,
            has_multimodal: false,
            context_window: Some(1),
            request_max_tokens: None,
            model_max_tokens: None,
            calibration_ratio: None,
        });

        assert_eq!(budget.context_limit, 1);
        // reserved_output = min(2048, 1) = 1
        // safety_margin = min(clamp(0, 256, 4096), 0) = min(256, 0) = 0
        // effective = 1 - 1 - 0 = 0
        assert_eq!(budget.effective_input_limit, 0);
        assert_eq!(budget.usage_percent, 100);
    }

    #[test]
    fn budget_fallback_context_limit() {
        let budget = estimate_request_budget(TokenBudgetInput {
            history_texts: &[],
            system_prompt: None,
            pending_message: None,
            tools: &[],
            tool_call_count: 0,
            has_multimodal: false,
            context_window: None,
            request_max_tokens: None,
            model_max_tokens: None,
            calibration_ratio: None,
        });

        assert_eq!(budget.context_limit, FALLBACK_CONTEXT_LIMIT);
        assert_eq!(budget.context_limit_source, ContextLimitSource::Fallback);
    }

    #[test]
    fn budget_multimodal_lowers_confidence() {
        let budget = estimate_request_budget(TokenBudgetInput {
            history_texts: &["hello".to_string()],
            system_prompt: None,
            pending_message: None,
            tools: &[],
            tool_call_count: 0,
            has_multimodal: true,
            context_window: Some(8192),
            request_max_tokens: None,
            model_max_tokens: None,
            calibration_ratio: None,
        });

        assert_eq!(budget.confidence, EstimateConfidence::Low);
        assert!(budget.breakdown.multimodal_tokens > 0);
    }

    #[test]
    fn budget_usage_percent_saturates_to_100() {
        // estimated > effective_input_limit → 100%
        let budget = estimate_request_budget(TokenBudgetInput {
            history_texts: &["x".repeat(10000)],
            system_prompt: Some(&"y".repeat(10000)),
            pending_message: Some(&"z".repeat(10000)),
            tools: &[],
            tool_call_count: 0,
            has_multimodal: false,
            context_window: Some(100),
            request_max_tokens: None,
            model_max_tokens: None,
            calibration_ratio: None,
        });

        assert_eq!(budget.usage_percent, 100);
    }

    #[test]
    fn budget_remaining_uses_saturating_sub() {
        // estimated > effective → remaining = 0 (not underflow)
        let budget = estimate_request_budget(TokenBudgetInput {
            history_texts: &["x".repeat(10000)],
            system_prompt: None,
            pending_message: None,
            tools: &[],
            tool_call_count: 0,
            has_multimodal: false,
            context_window: Some(100),
            request_max_tokens: None,
            model_max_tokens: None,
            calibration_ratio: None,
        });

        assert_eq!(budget.remaining_tokens, 0);
    }

    #[test]
    fn budget_output_reserve_exceeds_context_window() {
        // max_tokens > context_window → reserved clamped to context_window
        let budget = estimate_request_budget(TokenBudgetInput {
            history_texts: &[],
            system_prompt: None,
            pending_message: None,
            tools: &[],
            tool_call_count: 0,
            has_multimodal: false,
            context_window: Some(1000),
            request_max_tokens: Some(100_000),
            model_max_tokens: None,
            calibration_ratio: None,
        });

        assert_eq!(budget.reserved_output_tokens, 1000);
        assert_eq!(budget.effective_input_limit, 0);
    }

    #[test]
    fn budget_disabled_tools_reduces_tokens() {
        let tools = vec![
            make_tool("tool_a", "description a", json!({"type":"object"})),
            make_tool("tool_b", "description b", json!({"type":"object"})),
        ];

        let with_tools = estimate_request_budget(TokenBudgetInput {
            history_texts: &[],
            system_prompt: None,
            pending_message: None,
            tools: &tools,
            tool_call_count: 0,
            has_multimodal: false,
            context_window: Some(8192),
            request_max_tokens: None,
            model_max_tokens: None,
            calibration_ratio: None,
        });

        let without_tools = estimate_request_budget(TokenBudgetInput {
            history_texts: &[],
            system_prompt: None,
            pending_message: None,
            tools: &tools[..1], // 只留一个
            tool_call_count: 0,
            has_multimodal: false,
            context_window: Some(8192),
            request_max_tokens: None,
            model_max_tokens: None,
            calibration_ratio: None,
        });

        assert!(without_tools.estimated_input_tokens < with_tools.estimated_input_tokens);
    }

    // ── compute_history_token_budget ──

    #[test]
    fn history_budget_deducts_non_history_costs() {
        let budget = compute_history_token_budget(
            8192, // context_limit
            200,  // system_tokens
            500,  // tools_tokens
            100,  // pending_tokens
            2048, // reserved_output
            409,  // safety_margin
        );

        // 8192 - 200 - 500 - 100 - 2048 - 409 = 4935
        assert_eq!(budget, 4935);
    }

    #[test]
    fn history_budget_saturates_when_costs_exceed_limit() {
        let budget = compute_history_token_budget(
            100,  // context_limit (tiny)
            200,  // system_tokens (exceeds limit)
            500,  // tools_tokens
            100,  // pending_tokens
            2048, // reserved_output
            409,  // safety_margin
        );

        // All non-history costs exceed context_limit → 0 (saturating)
        assert_eq!(budget, 0);
    }

    // ── Usage ──

    #[test]
    fn full_usage_from_rig_usage_maps_all_seven_fields() {
        let rig_usage = rig_core::completion::Usage {
            input_tokens: 100,
            output_tokens: 50,
            total_tokens: 150,
            cached_input_tokens: 30,
            cache_creation_input_tokens: 10,
            tool_use_prompt_tokens: 200,
            reasoning_tokens: 25,
        };

        let full = Usage::from_rig_usage(&rig_usage);

        assert_eq!(full.input_tokens, 100);
        assert_eq!(full.output_tokens, 50);
        assert_eq!(full.total_tokens, 150);
        assert_eq!(full.cached_input_tokens, 30);
        assert_eq!(full.cache_creation_input_tokens, 10);
        assert_eq!(full.tool_use_prompt_tokens, 200);
        assert_eq!(full.reasoning_tokens, 25);
        assert!(full.reported);
    }

    #[test]
    fn full_usage_u64_overflow_saturates() {
        let rig_usage = rig_core::completion::Usage {
            input_tokens: u64::MAX,
            output_tokens: u64::MAX,
            total_tokens: u64::MAX,
            cached_input_tokens: u64::MAX,
            cache_creation_input_tokens: u64::MAX,
            tool_use_prompt_tokens: u64::MAX,
            reasoning_tokens: u64::MAX,
        };

        let full = Usage::from_rig_usage(&rig_usage);

        assert_eq!(full.input_tokens, u32::MAX);
        assert_eq!(full.output_tokens, u32::MAX);
        assert_eq!(full.total_tokens, u32::MAX);
        assert_eq!(full.cached_input_tokens, u32::MAX);
        assert_eq!(full.cache_creation_input_tokens, u32::MAX);
        assert_eq!(full.tool_use_prompt_tokens, u32::MAX);
        assert_eq!(full.reasoning_tokens, u32::MAX);
    }

    #[test]
    fn full_usage_add_preserves_all_fields() {
        let mut a = Usage {
            input_tokens: 100,
            output_tokens: 50,
            total_tokens: 150,
            cached_input_tokens: 30,
            cache_creation_input_tokens: 10,
            tool_use_prompt_tokens: 200,
            reasoning_tokens: 25,
            reported: true,
        };

        let b = Usage {
            input_tokens: 200,
            output_tokens: 100,
            total_tokens: 300,
            cached_input_tokens: 60,
            cache_creation_input_tokens: 20,
            tool_use_prompt_tokens: 400,
            reasoning_tokens: 50,
            reported: true,
        };

        a.add(&b);

        assert_eq!(a.input_tokens, 300);
        assert_eq!(a.output_tokens, 150);
        assert_eq!(a.total_tokens, 450);
        assert_eq!(a.cached_input_tokens, 90);
        assert_eq!(a.cache_creation_input_tokens, 30);
        assert_eq!(a.tool_use_prompt_tokens, 600);
        assert_eq!(a.reasoning_tokens, 75);
        assert!(a.reported);
    }

    #[test]
    fn full_usage_unreported_not_real_zero() {
        let unreported = Usage::unreported();
        assert!(!unreported.reported);
        assert!(!unreported.has_real_usage());
    }

    #[test]
    fn full_usage_reported_zero_is_real() {
        // 报告了但全零——可能是纯缓存场景
        let reported_zero = Usage {
            reported: true,
            ..Default::default()
        };
        assert!(reported_zero.reported);
        // has_real_usage 需要 > 0，所以全零报了也不算 real usage
        assert!(!reported_zero.has_real_usage());
    }

    // ── UsageCalibrator ──

    #[test]
    fn calibrator_insufficient_samples_returns_none() {
        let cal = UsageCalibrator::new();
        cal.record(
            "p1",
            "m1",
            100,
            &Usage {
                input_tokens: 120,
                reported: true,
                ..Default::default()
            },
        );
        cal.record(
            "p1",
            "m1",
            100,
            &Usage {
                input_tokens: 110,
                reported: true,
                ..Default::default()
            },
        );
        // Only 2 samples, need at least 3
        assert!(cal.get_ratio("p1", "m1").is_none());
    }

    #[test]
    fn calibrator_sufficient_samples_returns_ratio() {
        let cal = UsageCalibrator::new();
        for _ in 0..5 {
            cal.record(
                "p1",
                "m1",
                100,
                &Usage {
                    input_tokens: 120,
                    reported: true,
                    ..Default::default()
                },
            );
        }
        let ratio = cal.get_ratio("p1", "m1").unwrap();
        // ratio = 120/100 = 1.2
        assert!((ratio - 1.2).abs() < 0.01);
    }

    #[test]
    fn calibrator_provider_model_isolation() {
        let cal = UsageCalibrator::new();
        for _ in 0..5 {
            cal.record(
                "p1",
                "m1",
                100,
                &Usage {
                    input_tokens: 120,
                    reported: true,
                    ..Default::default()
                },
            );
            cal.record(
                "p2",
                "m2",
                100,
                &Usage {
                    input_tokens: 200,
                    reported: true,
                    ..Default::default()
                },
            );
        }

        let r1 = cal.get_ratio("p1", "m1").unwrap();
        let r2 = cal.get_ratio("p2", "m2").unwrap();
        assert!((r1 - 1.2).abs() < 0.01);
        assert!((r2 - 2.0).abs() < 0.01);
    }

    #[test]
    fn calibrator_outliers_do_not_dominate() {
        let cal = UsageCalibrator::new();
        // 3 normal + 1 extreme outlier
        cal.record(
            "p1",
            "m1",
            100,
            &Usage {
                input_tokens: 110,
                reported: true,
                ..Default::default()
            },
        );
        cal.record(
            "p1",
            "m1",
            100,
            &Usage {
                input_tokens: 120,
                reported: true,
                ..Default::default()
            },
        );
        cal.record(
            "p1",
            "m1",
            100,
            &Usage {
                input_tokens: 115,
                reported: true,
                ..Default::default()
            },
        );
        cal.record(
            "p1",
            "m1",
            100,
            &Usage {
                input_tokens: 10000,
                reported: true,
                ..Default::default()
            },
        );

        let ratio = cal.get_ratio("p1", "m1").unwrap();
        // Trimmed mean removes max (10000) and min (110), average of 120 and 115 = 1.175
        assert!(ratio < 2.0, "Outlier should not dominate: ratio = {ratio}");
    }

    #[test]
    fn calibrator_samples_bounded() {
        let cal = UsageCalibrator::new();
        for _ in 0..50 {
            cal.record(
                "p1",
                "m1",
                100,
                &Usage {
                    input_tokens: 120,
                    reported: true,
                    ..Default::default()
                },
            );
        }

        // Should still return valid ratio (bounded samples)
        let ratio = cal.get_ratio("p1", "m1").unwrap();
        assert!((ratio - 1.2).abs() < 0.01);
    }

    #[test]
    fn calibrator_zero_usage_not_sampled() {
        let cal = UsageCalibrator::new();
        cal.record(
            "p1",
            "m1",
            100,
            &Usage {
                input_tokens: 0,
                reported: true,
                ..Default::default()
            },
        );
        cal.record(
            "p1",
            "m1",
            100,
            &Usage {
                input_tokens: 0,
                reported: true,
                ..Default::default()
            },
        );
        cal.record(
            "p1",
            "m1",
            100,
            &Usage {
                input_tokens: 0,
                reported: true,
                ..Default::default()
            },
        );

        // Zero usage not sampled → no samples → None
        assert!(cal.get_ratio("p1", "m1").is_none());
    }

    #[test]
    fn calibrator_unreported_not_sampled() {
        let cal = UsageCalibrator::new();
        cal.record("p1", "m1", 100, &Usage::unreported());
        cal.record("p1", "m1", 100, &Usage::unreported());
        cal.record("p1", "m1", 100, &Usage::unreported());

        assert!(cal.get_ratio("p1", "m1").is_none());
    }

    #[test]
    fn calibrator_ratio_clamped_to_bounds() {
        let cal = UsageCalibrator::new();
        // Extreme ratio: actual = 10000 * estimated
        for _ in 0..5 {
            cal.record(
                "p1",
                "m1",
                1,
                &Usage {
                    input_tokens: 10000,
                    reported: true,
                    ..Default::default()
                },
            );
        }

        let ratio = cal.get_ratio("p1", "m1").unwrap();
        assert_eq!(ratio, CALIBRATION_RATIO_MAX);
    }

    #[test]
    fn calibrator_ratio_clamped_to_min() {
        let cal = UsageCalibrator::new();
        // Extreme ratio: actual = 0.001 * estimated (but > 0)
        for _ in 0..5 {
            cal.record(
                "p1",
                "m1",
                10000,
                &Usage {
                    input_tokens: 1,
                    reported: true,
                    ..Default::default()
                },
            );
        }

        let ratio = cal.get_ratio("p1", "m1").unwrap();
        assert_eq!(ratio, CALIBRATION_RATIO_MIN);
    }

    #[test]
    fn calibrator_clear_removes_samples() {
        let cal = UsageCalibrator::new();
        for _ in 0..5 {
            cal.record(
                "p1",
                "m1",
                100,
                &Usage {
                    input_tokens: 120,
                    reported: true,
                    ..Default::default()
                },
            );
        }
        assert!(cal.get_ratio("p1", "m1").is_some());

        cal.clear("p1", "m1");
        assert!(cal.get_ratio("p1", "m1").is_none());
    }

    /// 校准反馈回路修复（P0-1）：使用 raw_estimated_input_tokens 采样后，
    /// ratio 生效后续样本不会把 ratio 拉向 1.0，真实偏差被保留。
    #[test]
    fn calibrator_raw_baseline_stable_across_rounds() {
        // 模拟：raw=100, actual=150（真实偏差 1.5）
        // 先用 raw=100 采样达到 MIN_SAMPLES 使 ratio 生效（1.5），
        // 然后继续 record 多轮，ratio 应稳定在 1.5 不漂移。
        let cal = UsageCalibrator::new();

        // 第一阶段：采够 MIN_SAMPLES（3）使 ratio 生效
        for _ in 0..CALIBRATION_MIN_SAMPLES {
            cal.record(
                "p",
                "m",
                100, // raw_estimated_input_tokens（固定未校准基线）
                &Usage {
                    input_tokens: 150, // actual
                    reported: true,
                    ..Default::default()
                },
            );
        }
        let ratio_after_min = cal.get_ratio("p", "m").unwrap();
        assert!(
            (ratio_after_min - 1.5).abs() < 0.01,
            "初始 ratio 应为 1.5，实际 {ratio_after_min}"
        );

        // 第二阶段：继续 record 多轮（模拟 ratio 生效后的后续请求）
        // 关键：采样值仍然是 raw=100，不随 ratio 变化
        for _ in 0..10 {
            cal.record(
                "p",
                "m",
                100, // raw_estimated_input_tokens 不变
                &Usage {
                    input_tokens: 150,
                    reported: true,
                    ..Default::default()
                },
            );
        }

        let ratio_final = cal.get_ratio("p", "m").unwrap();
        assert!(
            (ratio_final - 1.5).abs() < 0.01,
            "多轮后 ratio 应稳定在 1.5 不漂移，实际 {ratio_final}"
        );
    }

    // ── P0-3: history_budget 链路测试 ──────────────────────────────────────────

    /// 同输入下工具池从 N 增至 N+1 → estimate_request_budget 的 breakdown.tools_tokens
    /// 增大 → compute_history_token_budget(...) 严格变小。
    #[test]
    fn history_budget_shrinks_when_tools_increase() {
        let tool = make_tool(
            "get_weather",
            "查询天气",
            json!({"type":"object","properties":{"city":{"type":"string"}}}),
        );

        // N=1 工具池
        let budget_n = estimate_request_budget(TokenBudgetInput {
            history_texts: &["some history".to_string()],
            system_prompt: Some("system prompt"),
            pending_message: Some("user message"),
            tools: std::slice::from_ref(&tool),
            tool_call_count: 0,
            has_multimodal: false,
            context_window: Some(8192),
            request_max_tokens: None,
            model_max_tokens: None,
            calibration_ratio: None,
        });

        // N+1=2 工具池（加一个不同的工具）
        let extra_tool = make_tool(
            "search_web",
            "搜索网页",
            json!({"type":"object","properties":{"query":{"type":"string"}}}),
        );
        let budget_n1 = estimate_request_budget(TokenBudgetInput {
            history_texts: &["some history".to_string()],
            system_prompt: Some("system prompt"),
            pending_message: Some("user message"),
            tools: &[tool, extra_tool],
            tool_call_count: 0,
            has_multimodal: false,
            context_window: Some(8192),
            request_max_tokens: None,
            model_max_tokens: None,
            calibration_ratio: None,
        });

        // 断言链条：tools_tokens 增大
        assert!(
            budget_n1.breakdown.tools_tokens > budget_n.breakdown.tools_tokens,
            "工具池增大后 tools_tokens 应增大"
        );

        // 断言链条：compute_history_token_budget 严格变小
        let history_budget_n = compute_history_token_budget(
            budget_n.context_limit,
            budget_n.breakdown.system_tokens,
            budget_n.breakdown.tools_tokens,
            budget_n.breakdown.pending_tokens,
            budget_n.reserved_output_tokens,
            budget_n.safety_margin_tokens,
        );
        let history_budget_n1 = compute_history_token_budget(
            budget_n1.context_limit,
            budget_n1.breakdown.system_tokens,
            budget_n1.breakdown.tools_tokens,
            budget_n1.breakdown.pending_tokens,
            budget_n1.reserved_output_tokens,
            budget_n1.safety_margin_tokens,
        );
        assert!(
            history_budget_n1 < history_budget_n,
            "工具池增大后 history budget 应严格变小: {history_budget_n} -> {history_budget_n1}"
        );
    }

    /// request_max_tokens 从 None 变 Some(4096) → reserved_output_tokens 增大 →
    /// history budget 严格变小。
    #[test]
    fn history_budget_shrinks_when_request_max_tokens_set() {
        let tools = [make_tool("tool", "desc", json!({"type":"object"}))];

        // request_max_tokens = None → reserved_output = DEFAULT_RESERVED_OUTPUT (2048)
        let budget_none = estimate_request_budget(TokenBudgetInput {
            history_texts: &["some history".to_string()],
            system_prompt: Some("system prompt"),
            pending_message: Some("user message"),
            tools: &tools,
            tool_call_count: 0,
            has_multimodal: false,
            context_window: Some(8192),
            request_max_tokens: None,
            model_max_tokens: None,
            calibration_ratio: None,
        });

        // request_max_tokens = Some(4096) → reserved_output = 4096
        let budget_some = estimate_request_budget(TokenBudgetInput {
            history_texts: &["some history".to_string()],
            system_prompt: Some("system prompt"),
            pending_message: Some("user message"),
            tools: &tools,
            tool_call_count: 0,
            has_multimodal: false,
            context_window: Some(8192),
            request_max_tokens: Some(4096),
            model_max_tokens: None,
            calibration_ratio: None,
        });

        // 断言链条：reserved_output_tokens 增大
        assert!(
            budget_some.reserved_output_tokens > budget_none.reserved_output_tokens,
            "request_max_tokens 设定后 reserved_output_tokens 应增大"
        );

        // 断言链条：compute_history_token_budget 严格变小
        let history_budget_none = compute_history_token_budget(
            budget_none.context_limit,
            budget_none.breakdown.system_tokens,
            budget_none.breakdown.tools_tokens,
            budget_none.breakdown.pending_tokens,
            budget_none.reserved_output_tokens,
            budget_none.safety_margin_tokens,
        );
        let history_budget_some = compute_history_token_budget(
            budget_some.context_limit,
            budget_some.breakdown.system_tokens,
            budget_some.breakdown.tools_tokens,
            budget_some.breakdown.pending_tokens,
            budget_some.reserved_output_tokens,
            budget_some.safety_margin_tokens,
        );
        assert!(
            history_budget_some < history_budget_none,
            "request_max_tokens 设定后 history budget 应严格变小: {history_budget_none} -> {history_budget_some}"
        );
    }

    // ── P1-3: is_cjk 双源一致性测试 ─────────────────────────────────────────────

    /// `token_budget::is_cjk` 与 `infra::data::conversations::is_cjk_char` 是手工镜像，
    /// 分层约束不许 infra 反向依赖 domain。此测试在边界码点上断言两函数结果一致，
    /// 并加绝对断言防止两边同错时测试假绿。
    #[test]
    fn is_cjk_matches_infra_mirror() {
        use crate::infra::data::conversations::is_cjk_char;

        let boundary_codepoints: &[u32] = &[
            0x2FFF, 0x3000, 0x33FF, 0x3400, 0x4DBF, 0x4DC0, 0x4E00, 0x9FFF, 0xA000,
            0xABFF, 0xAC00, 0xD7AF, 0xD800, 0xF8FF, 0xF900, 0xFAFF, 0xFB00, 0xFEFF,
            0xFF00, 0xFFEF, 0x10000, 0x1FFFF, 0x20000, 0x2A6DF, 0x2A6E0,
        ];

        for &cp in boundary_codepoints {
            if let Some(ch) = char::from_u32(cp) {
                let domain_result = is_cjk(ch);
                let infra_result = is_cjk_char(ch);
                assert_eq!(
                    domain_result, infra_result,
                    "码点 U+{cp:04X}: domain is_cjk={domain_result}, infra is_cjk_char={infra_result} 不一致"
                );
            }
        }

        // 绝对断言：防两边同错时测试假绿
        assert!(is_cjk('\u{4E00}'), "U+4E00 应为 CJK（domain）");
        assert!(is_cjk_char('\u{4E00}'), "U+4E00 应为 CJK（infra）");
        assert!(!is_cjk('\u{0041}'), "U+0041 (A) 不应为 CJK（domain）");
        assert!(!is_cjk_char('\u{0041}'), "U+0041 (A) 不应为 CJK（infra）");
    }
}
