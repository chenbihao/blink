//! Blink 翻译插件（Rust 版）— 5 引擎多语言翻译。
//!
//! 1:1 迁移自历史 Python 版（0.11.6，源码已于 0.11.7 收尾时清理）。
//! 走 xtask 编译流水线，manifest runtime.type=process。
//!
//! **HTTP 代理协议**：不直接联网，通过 JSONL `http_request` → core 代发，
//! 与 IP/天气插件一致。统一代理 + 超时 + 重试。
//!
//! **双路径返回**（D6）：
//! - 查询路径（用户打字"翻译 hello"）：返回多引擎 items（译文+原文）
//! - AI tool 路径：返回相同 items（0.11.0 后走 items_to_entries 统一投影，
//!   result_type=text 语义由 manifest 声明，插件实际返回 items 供前端展示）

#![cfg_attr(windows, windows_subsystem = "windows")]

mod engine;
mod engines;
mod protocol;

use std::collections::HashMap;
use std::io::{self, BufRead, Write};

use serde_json::Value;

use engine::{EngineRequest, TranslateEngine};
use engines::{AliEngine, BaiduEngine, DeeplEngine, TencentEngine, YoudaoEngine};
use protocol::{
    CoreToPlugin, HttpRequest, PluginAction, PluginError, PluginItem, PluginResponse,
    PluginToCore, RawToolResult,
};

/// HTTP 请求 id 全局计数器。
///
/// 旧实现用 `chrono::Local::now().timestamp_millis()` 生成 id,批量并发时同毫秒到达的请求
/// 会生成相同 id,导致 `pending` HashMap key 被覆盖,response 变孤儿（"unknown request"）。
/// 改用进程内单调递增计数器,彻底消除碰撞（插件是单进程,无需跨进程协调）。
static HTTP_ID_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// 生成进程内唯一的 HTTP 请求 id。格式 `tr_{seq}`,seq 单调递增。
fn next_http_id() -> String {
    let seq = HTTP_ID_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!("tr_{seq}")
}

/// 引擎显示名称（1:1 对齐 Python）
const ENGINE_NAMES: &[(&str, &str)] = &[
    ("youdao", "有道智云"),
    ("baidu", "百度翻译"),
    ("deepl", "DeepL"),
    ("ali", "阿里翻译"),
    ("tencent", "腾讯翻译"),
];

fn engine_display_name(id: &str) -> &str {
    ENGINE_NAMES
        .iter()
        .find(|(k, _)| *k == id)
        .map(|(_, v)| *v)
        .unwrap_or(id)
}

/// 获取引擎实例（按 id）
fn get_engine(id: &str) -> Option<Box<dyn TranslateEngine>> {
    match id {
        "youdao" => Some(Box::new(YoudaoEngine)),
        "baidu" => Some(Box::new(BaiduEngine)),
        "deepl" => Some(Box::new(DeeplEngine)),
        "ali" => Some(Box::new(AliEngine)),
        "tencent" => Some(Box::new(TencentEngine)),
        _ => None,
    }
}

/// 有效引擎集合（降级链过滤用）
fn valid_engines() -> &'static [&'static str] {
    &["youdao", "baidu", "deepl", "ali", "tencent"]
}

// ── 文本辅助（1:1 对齐 Python）──────────────────────────────────────────────

/// 拆分程序员命名风格：snake_case / camelCase / SCREAMING_SNAKE / kebab-case → 空格分隔。
///
/// Rust 手写实现（避免 regex 依赖）。单单词和中文不受影响。
fn preprocess_code_identifiers(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 8);
    let chars: Vec<char> = text.chars().collect();
    for (i, &c) in chars.iter().enumerate() {
        if c == '_' || c == '-' {
            out.push(' ');
        } else if c.is_uppercase() {
            // camelCase 拆分规则：
            // 1. 大写+小写前断开（HTTPSConnection → HTTPS Connection）：
            //    当前大写 + 下一字符小写 + 前一字符大写 → 前插空格
            // 2. 小写+大写前断开（getUserName → get UserName）：
            //    前一字符小写 + 当前大写 → 前插空格
            let prev_lower = i > 0 && chars[i - 1].is_lowercase();
            let next_lower = i + 1 < chars.len() && chars[i + 1].is_lowercase();
            let prev_upper = i > 0 && chars[i - 1].is_uppercase();
            if prev_lower || (prev_upper && next_lower) {
                if !out.ends_with(' ') {
                    out.push(' ');
                }
            }
            out.push(c);
        } else {
            out.push(c);
        }
    }
    out.trim().to_string()
}

/// 简单检测文本语言：中文为主返回 'zh'，否则返回 'en'。
fn detect_lang(text: &str) -> &'static str {
    let total = text.chars().count();
    if total == 0 {
        return "en";
    }
    let chinese = text
        .chars()
        .filter(|&c| ('\u{4e00}'..='\u{9fff}').contains(&c))
        .count();
    if (chinese as f64) > total as f64 * 0.3 {
        "zh"
    } else {
        "en"
    }
}

/// 自动交换目标语言：中文输入→译英文，英文输入→译中文。
fn auto_swap_lang(text: &str, target_lang: &str) -> String {
    if target_lang != "auto" {
        return target_lang.to_string();
    }
    let detected = detect_lang(text);
    if detected == "zh" {
        "en".into()
    } else {
        "zh".into()
    }
}

/// 清除文本中的 Unicode 私用区字符(U+E000..=U+F8FF)。
///
/// 私用区字符可能来自 OCR 识别噪声或上游拼接残留,送翻译前必须清除,
/// 否则与批量 tag 标记混淆导致解析失败。
fn strip_private_use_chars(s: &str) -> String {
    s.chars()
        .filter(|c| !('\u{E000}'..='\u{F8FF}').contains(c))
        .collect()
}

/// 批量翻译的 tag 边界字符——使用 Unicode 私用区(U+E000 / U+E001)。
///
/// 私用区字符不属于任何自然语言词表,翻译引擎无法"翻译"它们,几乎只能原样透传。
/// 对比旧实现 `[[BLINK_0]]`(纯 ASCII 标点 + 英文词),可能被引擎识别为:
///   - markdown 风格标记 → 改写
///   - 未知英文词 → 翻成"闪烁"
///   - 数字 → 全角化
/// 私用区字符从源头规避这些风险。
const TAG_OPEN: char = '\u{E000}';
const TAG_CLOSE: char = '\u{E001}';

/// 给批量文本加稳定 tag，让只支持单字符串的翻译引擎也能一次请求后按原顺序拆回。
///
/// 格式:`\u{E000}0\u{E001}\n原文行0\n\n\u{E000}1\u{E001}\n原文行1`
/// tag 由两个私用区字符包夹一个数字组成,引擎无法翻译私用区字符,
/// 数字即使被全角化,parse 时也做全角→半角归一化。
fn build_tagged_batch(texts: &[String]) -> String {
    texts
        .iter()
        .enumerate()
        .map(|(i, text)| format!("{TAG_OPEN}{i}{TAG_CLOSE}\n{text}"))
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// 从翻译结果中按 tag 恢复顺序。任一 tag 缺失或重复都视为失败，交给 core 单行降级。
fn parse_tagged_batch(result: &str, count: usize) -> Option<Vec<String>> {
    let mut positions = Vec::with_capacity(count);
    for i in 0..count {
        // 同时匹配半角和全角数字(引擎可能把 0 全角化成 ０)
        let tag_patterns = [
            format!("{TAG_OPEN}{i}{TAG_CLOSE}"),
            format!("{TAG_OPEN}{}{TAG_CLOSE}", fullwidth_digit(i)),
        ];
        let found = tag_patterns.iter().find_map(|tag| {
            let start = result.find(tag)?;
            if result[start + tag.len()..].contains(tag.as_str()) {
                // 同一 tag 出现两次 → 视为乱序,拒绝
                return None;
            }
            Some((start, tag.len()))
        });
        let Some(pos) = found else { return None };
        positions.push(pos);
    }
    positions.sort_by_key(|(start, _)| *start);

    let mut values = vec![String::new(); count];
    for (order, &(start, tag_len)) in positions.iter().enumerate() {
        let tag = &result[start..start + tag_len];
        // 提取两个私用区字符中间的数字(可能是全角)
        let inner: String = tag
            .chars()
            .filter(|c| *c != TAG_OPEN && *c != TAG_CLOSE)
            .collect();
        let index = parse_digit_mixed(&inner)?;
        let end = positions
            .get(order + 1)
            .map(|(next, _)| *next)
            .unwrap_or(result.len());
        let value = result[start + tag_len..end].trim();
        if value.is_empty() {
            return None;
        }
        values[index] = value.to_string();
    }
    Some(values)
}

/// 数字 → 全角字符串(应对引擎把 ASCII 数字全角化)。
/// 0→０, 1→１, ..., 9→９, 10→１０
fn fullwidth_digit(n: usize) -> String {
    n.to_string()
        .chars()
        .map(|c| {
            if c.is_ascii_digit() {
                char::from_u32(c as u32 - b'0' as u32 + 0xFF10).unwrap_or(c)
            } else {
                c
            }
        })
        .collect()
}

/// 解析 tag 内部的数字(容忍全角/半角混用)。
fn parse_digit_mixed(s: &str) -> Option<usize> {
    let normalized: String = s
        .chars()
        .map(|c| {
            // 全角数字 ０(0xFF10) ~ ９(0xFF19) → 半角
            if (0xFF10..=0xFF19).contains(&(c as u32)) {
                char::from_digit(c as u32 - 0xFF10, 10).unwrap_or(c)
            } else {
                c
            }
        })
        .collect();
    normalized.parse::<usize>().ok()
}

/// 待处理的 HTTP 请求上下文（发 http_request 后等 http_response 恢复）。
struct PendingTranslate {
    query_id: String,
    is_tool_call: bool,
    text: String,          // 预处理后的文本
    original_text: String, // 原始输入
    target_lang: String,
    engine_id: String,
    fallback_order: Vec<String>,
    settings: Value,
    /// 已尝试的引擎（含主引擎，降级时跳过）
    tried_engines: Vec<String>,
    /// 批量 tool 的原始行；None 表示普通 query/translate。
    batch_originals: Option<Vec<String>>,
    /// 批量请求是否走引擎原生批量 API（true=parse_batch_response / false=parse_tagged_batch）。
    /// 降级到下一个引擎时按该引擎能力重新判定。
    batch_native: bool,
    /// tag 拼接是否已被某引擎破坏过。true 后续不再尝试 tag,直接走单行并发兜底。
    /// (一家引擎破坏 tag,换一家大概率也破坏;tag 在多引擎间不可靠)
    tag_poisoned: bool,
}

fn main() {
    let stdin = io::stdin();
    let mut stdout = io::stdout();

    // http_request_id → PendingTranslate
    let mut pending: HashMap<String, PendingTranslate> = HashMap::new();

    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let msg: CoreToPlugin = match serde_json::from_str(&line) {
            Ok(m) => m,
            Err(e) => {
                eprintln!("invalid message: {e}");
                continue;
            }
        };

        match msg {
            CoreToPlugin::Query {
                id,
                query,
                settings,
            } => {
                handle_query(&mut stdout, &mut pending, id, query, settings);
            }
            CoreToPlugin::ToolCall {
                id,
                tool_name,
                arguments,
                settings,
            } => {
                handle_tool_call(
                    &mut stdout,
                    &mut pending,
                    id,
                    tool_name,
                    arguments,
                    settings,
                );
            }
            CoreToPlugin::HttpResponse {
                id,
                status,
                body,
                error,
            } => {
                handle_http_response(&mut stdout, &mut pending, id, status, body, error);
            }
            CoreToPlugin::Cancel { .. } => {
                // 不支持取消，忽略（与 weather 一致）
            }
        }
    }
}

/// 发送消息到 stdout。
fn send_message<W: Write, S: serde::Serialize>(writer: &mut W, msg: &S) {
    let json = serde_json::to_string(msg).unwrap();
    let _ = writeln!(writer, "{json}");
    let _ = writer.flush();
}

/// 处理查询请求（用户打字路径）。
fn handle_query<W: Write>(
    writer: &mut W,
    pending: &mut HashMap<String, PendingTranslate>,
    id: String,
    query: String,
    settings: Option<Value>,
) {
    let settings = settings.unwrap_or(Value::Null);
    let engine = settings
        .get("default_engine")
        .and_then(Value::as_str)
        .unwrap_or("youdao")
        .to_string();
    let target_lang = settings
        .get("target_lang")
        .and_then(Value::as_str)
        .unwrap_or("zh")
        .to_string();

    let text = strip_private_use_chars(query.trim());
    if text.is_empty() {
        let resp = PluginToCore::Response(PluginResponse {
            id,
            items: vec![],
            error: None,
        });
        send_message(writer, &resp);
        return;
    }

    let original_text = text.clone();
    let processed = preprocess_code_identifiers(&text);
    let target_lang = auto_swap_lang(&processed, &target_lang);
    let fallback_order = parse_fallback_order(&settings);

    eprintln!("[translate] query: id={id}, engine={engine}, target={target_lang}");

    try_translate(
        writer,
        pending,
        id,
        false,
        processed,
        original_text,
        target_lang,
        engine,
        fallback_order,
        settings,
    );
}

/// 处理 AI tool-call 请求。
fn handle_tool_call<W: Write>(
    writer: &mut W,
    pending: &mut HashMap<String, PendingTranslate>,
    id: String,
    tool_name: String,
    arguments: Value,
    settings: Option<Value>,
) {
    let settings = settings.unwrap_or(Value::Null);
    let engine = settings
        .get("default_engine")
        .and_then(Value::as_str)
        .unwrap_or("youdao")
        .to_string();

    if tool_name != "translate" && tool_name != "translate_batch" {
        let resp = PluginToCore::RawResult(RawToolResult {
            id,
            data: serde_json::Value::Null,
            error: Some(PluginError {
                code: "UNKNOWN_TOOL".into(),
                message: format!("未知 tool: {tool_name}"),
            }),
        });
        send_message(writer, &resp);
        return;
    }

    let mut target_lang = arguments
        .get("target_lang")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    if target_lang.is_empty() || target_lang == "auto" {
        target_lang = settings
            .get("target_lang")
            .and_then(Value::as_str)
            .unwrap_or("zh")
            .to_string();
    }
    let fallback_order = parse_fallback_order(&settings);

    if tool_name == "translate_batch" {
        let Some(raw_texts) = arguments.get("texts").and_then(Value::as_array) else {
            let resp = PluginToCore::RawResult(RawToolResult {
                id,
                data: serde_json::Value::Null,
                error: Some(PluginError {
                    code: "MISSING_ARG".into(),
                    message: "缺少 texts 参数".into(),
                }),
            });
            send_message(writer, &resp);
            return;
        };
        let texts: Vec<String> = raw_texts
            .iter()
            .filter_map(Value::as_str)
            .map(str::trim)
            .map(strip_private_use_chars)
            .collect();
        if texts.is_empty() || texts.iter().any(String::is_empty) {
            let resp = PluginToCore::RawResult(RawToolResult {
                id,
                data: serde_json::Value::Null,
                error: Some(PluginError {
                    code: "MISSING_ARG".into(),
                    message: "texts 必须是非空字符串数组".into(),
                }),
            });
            send_message(writer, &resp);
            return;
        }
        let tagged = build_tagged_batch(&texts);
        let detection_text = texts.join("\n");
        let target_lang = auto_swap_lang(&detection_text, &target_lang);
        eprintln!(
            "[translate] batch tool_call: id={id}, count={}, target={target_lang}",
            texts.len()
        );
        try_translate_batch(
            writer,
            pending,
            id,
            tagged,
            texts,
            target_lang,
            engine,
            fallback_order,
            settings,
        );
        return;
    }

    let text = strip_private_use_chars(
        arguments
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim(),
    );
    if text.is_empty() {
        let resp = PluginToCore::RawResult(RawToolResult {
            id,
            data: serde_json::Value::Null,
            error: Some(PluginError {
                code: "MISSING_ARG".into(),
                message: "缺少 text 参数".into(),
            }),
        });
        send_message(writer, &resp);
        return;
    }

    let target_lang = auto_swap_lang(&text, &target_lang);

    eprintln!("[translate] tool_call: id={id}, text={text:?}, target={target_lang}");

    try_translate(
        writer,
        pending,
        id,
        true,
        text.clone(),
        text,
        target_lang,
        engine,
        fallback_order,
        settings,
    );
}

/// 解析降级顺序设置（兼容字符串和数组格式）。
fn parse_fallback_order(settings: &Value) -> Vec<String> {
    let default = || {
        vec![
            "tencent".into(),
            "ali".into(),
            "baidu".into(),
            "youdao".into(),
            "deepl".into(),
        ]
    };
    let default_val = serde_json::json!(default());
    let val = settings.get("fallback_order").unwrap_or(&default_val);
    let raw: Vec<String> = match val {
        Value::String(s) => s.split(',').map(|e| e.trim().to_string()).collect(),
        Value::Array(arr) => arr
            .iter()
            .filter_map(|v| v.as_str().map(|s| s.to_string()))
            .collect(),
        _ => return default(),
    };
    // 空列表或全无效 → 回退默认
    if raw.is_empty() {
        return default();
    }
    let valid: std::collections::HashSet<&str> = valid_engines().iter().copied().collect();
    let filtered: Vec<String> = raw
        .into_iter()
        .filter(|e| valid.contains(e.as_str()))
        .collect();
    if filtered.is_empty() {
        default()
    } else {
        filtered
    }
}

/// 尝试翻译：主引擎失败则按 fallback_order 降级。
fn try_translate<W: Write>(
    writer: &mut W,
    pending: &mut HashMap<String, PendingTranslate>,
    query_id: String,
    is_tool_call: bool,
    text: String,
    original_text: String,
    target_lang: String,
    engine_id: String,
    fallback_order: Vec<String>,
    settings: Value,
) {
    // 发起首次翻译请求
    issue_translate_request(
        writer,
        pending,
        PendingTranslate {
            query_id,
            is_tool_call,
            text,
            original_text,
            target_lang,
            engine_id,
            fallback_order,
            settings,
            tried_engines: vec![],
            batch_originals: None,
            batch_native: false,
            tag_poisoned: false,
        },
    );
}

fn try_translate_batch<W: Write>(
    writer: &mut W,
    pending: &mut HashMap<String, PendingTranslate>,
    query_id: String,
    _tagged_text: String, // 保留参数兼容调用点;实际由引擎能力决定用原生还是 tag
    originals: Vec<String>,
    target_lang: String,
    engine_id: String,
    fallback_order: Vec<String>,
    settings: Value,
) {
    let ctx = PendingTranslate {
        query_id,
        is_tool_call: true,
        text: String::new(),
        original_text: String::new(),
        target_lang,
        engine_id,
        fallback_order,
        settings,
        tried_engines: vec![],
        batch_originals: Some(originals),
        batch_native: false,
        tag_poisoned: false,
    };
    dispatch_batch_by_engine(writer, pending, ctx);
}

/// 批量翻译的引擎分发器(三档降级)。
///
/// 1. 引擎支持原生批量 → `build_batch_request`(API 原生保序,无 tag 风险)
/// 2. 引擎不支持原生 + tag 未被破坏 → tag 拼接单次请求
/// 3. tag 已被破坏(`tag_poisoned=true`)→ 单行并发兜底(N 次 translate)
///
/// 设计动机:tag 拼接在不同引擎间不可靠(一家破坏,换一家大概率也破坏),
/// 一旦失败立即切单行并发,避免在 tag 上反复浪费请求。
fn dispatch_batch_by_engine<W: Write>(
    writer: &mut W,
    pending: &mut HashMap<String, PendingTranslate>,
    mut ctx: PendingTranslate,
) {
    let engine = match get_engine(&ctx.engine_id) {
        Some(e) => e,
        None => {
            try_next_fallback(writer, pending, ctx);
            return;
        }
    };

    // 档 1:原生批量
    if engine.supports_batch() {
        if let Some(req) = engine.build_batch_request(
            ctx.batch_originals.as_ref().unwrap_or(&vec![]),
            &ctx.target_lang,
            &ctx.settings,
        ) {
            ctx.batch_native = true;
            issue_http_request(writer, pending, req, ctx);
            return;
        }
        // 原生批量请求构造失败(配置缺失/超限)→ 落到档 2/3
    }

    // 档 2:tag 拼接（引擎支持 tag 且 tag 未被破坏）
    if !ctx.tag_poisoned && engine.supports_tag_batch() {
        if let Some(originals) = ctx.batch_originals.as_ref() {
            let tagged = build_tagged_batch(originals);
            ctx.text = tagged.clone();
            ctx.original_text = tagged;
            ctx.batch_native = false;
            issue_translate_request(writer, pending, ctx);
            return;
        }
    }

    // 档 3:引擎不支持批量能力 → 尝试下一个引擎的批量（而非直接单行并发）
    // 只有当所有引擎都不支持批量时，才走单行并发兜底
    if !engine.supports_batch() && !engine.supports_tag_batch() {
        // 当前引擎无批量能力，尝试下一个引擎
        ctx.tried_engines.push(ctx.engine_id.clone());
        try_next_fallback(writer, pending, ctx);
        return;
    }

    // tag 被破坏 → 单行并发兜底
    dispatch_single_line_fallback(writer, pending, ctx);
}

/// tag 彻底失败后的单行并发兜底。
/// 把 N 行 originals 拆成 N 个独立的 translate tool 调用,通过 RawResult 返回保序数组。
/// 这条路径**不依赖任何 tag**,对所有引擎都可靠(代价是 N 次 API 往返)。
fn dispatch_single_line_fallback<W: Write>(
    writer: &mut W,
    _pending: &mut HashMap<String, PendingTranslate>,
    ctx: PendingTranslate,
) {
    let originals = match ctx.batch_originals.as_ref() {
        Some(v) if !v.is_empty() => v.clone(),
        _ => return,
    };
    eprintln!(
        "[translate] 批量 tag 失效,降级单行并发: engine={}, lines={}",
        ctx.engine_id,
        originals.len()
    );
    // 单行并发在 core 端的 translate_lines command 已实装(每行 spawn 一个 task)。
    // 插件层这里直接返回 BATCH_TAG_POISONED 错误,让 core 触发它的单行降级路径。
    // 0.14.3: tool-call 走轨道 A，返回 RawResult（纯 data + error）
    let resp = PluginToCore::RawResult(RawToolResult {
        id: ctx.query_id,
        data: serde_json::Value::Null,
        error: Some(PluginError {
            code: "BATCH_TAG_POISONED".into(),
            message: format!(
                "批量翻译 tag 在多引擎间失效,已尝试 {} 个引擎,建议走单行并发",
                ctx.tried_engines.len()
            ),
        }),
    });
    send_message(writer, &resp);
}

/// 构造 HTTP 请求并发送给 core（pending 存上下文）。
/// 单次翻译路径:用 engine.build_request 构造请求(text 单字符串)。
fn issue_translate_request<W: Write>(
    writer: &mut W,
    pending: &mut HashMap<String, PendingTranslate>,
    mut ctx: PendingTranslate,
) {
    // 尝试当前引擎，失败则降级
    let engine = match get_engine(&ctx.engine_id) {
        Some(e) => e,
        None => {
            // 引擎不存在，尝试降级
            try_next_fallback(writer, pending, ctx);
            return;
        }
    };

    // 引擎配置缺失（如 API key 为空）→ engine.build_request 返回 None
    let req = match engine.build_request(&ctx.text, &ctx.target_lang, &ctx.settings) {
        Some(r) => r,
        None => {
            ctx.tried_engines.push(ctx.engine_id.clone());
            try_next_fallback(writer, pending, ctx);
            return;
        }
    };

    issue_http_request(writer, pending, req, ctx);
}

/// 把已构造好的 EngineRequest 发给 core(pending 存上下文)。
/// 原生批量路径直接调此函数,跳过 engine.build_request。
fn issue_http_request<W: Write>(
    writer: &mut W,
    pending: &mut HashMap<String, PendingTranslate>,
    req: EngineRequest,
    mut ctx: PendingTranslate,
) {
    let http_id = next_http_id();
    ctx.tried_engines.push(ctx.engine_id.clone());
    pending.insert(http_id.clone(), ctx);

    let http_req = PluginToCore::HttpRequest(HttpRequest {
        id: http_id,
        method: req.method,
        url: req.url,
        body: req.body,
        timeout_ms: req.timeout_ms,
        headers: req.headers,
    });
    send_message(writer, &http_req);
}

/// 主引擎失败后，按 fallback_order 尝试下一个未试过的引擎。
fn try_next_fallback<W: Write>(
    writer: &mut W,
    pending: &mut HashMap<String, PendingTranslate>,
    mut ctx: PendingTranslate,
) {
    let next = ctx
        .fallback_order
        .iter()
        .find(|e| !ctx.tried_engines.contains(e))
        .cloned();

    match next {
        Some(engine_id) => {
            eprintln!(
                "[translate] 降级到: {} ({})",
                engine_display_name(&engine_id),
                engine_id
            );
            ctx.engine_id = engine_id.clone();
            // 批量降级:按目标引擎能力重新判定走原生批量还是 tag 拼接。
            if ctx.batch_originals.is_some() {
                dispatch_batch_by_engine(writer, pending, ctx);
            } else {
                issue_translate_request(writer, pending, ctx);
            }
        }
        None => {
            // 所有引擎都试过了
            // 如果是批量请求且还没走过单行并发 → 最后兜底走单行并发
            if ctx.batch_originals.is_some() && !ctx.tag_poisoned {
                eprintln!("[translate] 所有引擎批量均失败,兜底单行并发");
                dispatch_single_line_fallback(writer, pending, ctx);
                return;
            }
            // 真正失败 → 返回错误
            // 0.14.3: tool-call 走轨道 A（RawResult），query 走旧协议（Response）
            let error = PluginError {
                code: "TRANSLATE_FAILED".into(),
                message: "翻译失败，请检查 API 配置或网络连接".into(),
            };
            let resp = if ctx.is_tool_call {
                PluginToCore::RawResult(RawToolResult {
                    id: ctx.query_id,
                    data: serde_json::Value::Null,
                    error: Some(error),
                })
            } else {
                PluginToCore::Response(PluginResponse {
                    id: ctx.query_id,
                    items: vec![],
                    error: Some(error),
                })
            };
            send_message(writer, &resp);
        }
    }
}

/// 处理 HTTP 响应：查 pending 恢复上下文 → 解析 → 成功则返回结果，失败则降级。
fn handle_http_response<W: Write>(
    writer: &mut W,
    pending: &mut HashMap<String, PendingTranslate>,
    id: String,
    status: u16,
    body: Option<String>,
    error: Option<String>,
) {
    let Some(mut ctx) = pending.remove(&id) else {
        eprintln!("[translate] http response for unknown request: {id}");
        return;
    };

    let engine = get_engine(&ctx.engine_id);

    // HTTP 层错误 → 直接降级
    if error.is_some() || status != 200 {
        eprintln!(
            "[translate] {} HTTP error: status={status}, error={:?}",
            ctx.engine_id, error
        );
        try_next_fallback(writer, pending, ctx);
        return;
    }

    let body = body.unwrap_or_default();

    // 批量原生路径:用 engine.parse_batch_response 直接拿 Vec<String>
    if ctx.batch_native {
        if let Some(originals) = ctx.batch_originals.as_ref() {
            let expected = originals.len();
            if let Some(results) = engine
                .as_ref()
                .and_then(|e| e.parse_batch_response(&body, expected))
            {
                // 0.14.3: tool-call 走轨道 A，返回纯 data（译文数组）
                let data = serde_json::Value::Array(
                    results.into_iter().map(serde_json::Value::String).collect(),
                );
                let resp = PluginToCore::RawResult(RawToolResult {
                    id: ctx.query_id,
                    data,
                    error: None,
                });
                send_message(writer, &resp);
                return;
            }
            // 原生批量解析失败 → 降级到 tag 拼接,用同一引擎重试一次
            eprintln!(
                "[translate] {} batch parse failed, fallback to tagged. body: {}",
                ctx.engine_id,
                body.chars().take(500).collect::<String>()
            );
            let tagged = build_tagged_batch(originals);
            ctx.batch_native = false;
            ctx.text = tagged.clone();
            ctx.original_text = tagged;
            // tried_engines 已含当前引擎,issue_translate_request 会跳过它;
            // 但我们想用同一引擎的 tag 路径重试,所以清掉 tried_engines 让它能再选一次。
            // 实际上 issue_translate_request 用 ctx.engine_id 直接调,不看 tried_engines,
            // 所以这里不用清。
            issue_translate_request(writer, pending, ctx);
            return;
        }
    }

    // 单次 / tag 拼接路径:用 engine.parse_response 拿单字符串
    let translated = engine.as_ref().and_then(|e| e.parse_response(&body));
    let Some(result) = translated else {
        let preview: String = body.chars().take(500).collect();
        eprintln!(
            "[translate] {} parse failed, body: {}",
            ctx.engine_id, preview
        );
        try_next_fallback(writer, pending, ctx);
        return;
    };

    // 成功 → 返回翻译结果。
    // 0.14.3: tool-call 走轨道 A（返回纯 data），query 走旧协议（返回 items）。
    if ctx.is_tool_call {
        // 轨道 A: tool-call 返回纯 data
        let data = if let Some(originals) = ctx.batch_originals.as_ref() {
            let Some(results) = parse_tagged_batch(&result, originals.len()) else {
                // tag 被引擎破坏 → 标记 poisoned,后续不再尝试 tag,直接单行并发。
                eprintln!(
                    "[translate] {} tag 解析失败,标记 tag_poisoned,降级单行并发",
                    ctx.engine_id
                );
                ctx.tag_poisoned = true;
                try_next_fallback(writer, pending, ctx);
                return;
            };
            serde_json::Value::Array(
                results.into_iter().map(serde_json::Value::String).collect(),
            )
        } else {
            // 单次翻译：data = 译文字符串
            serde_json::Value::String(result.to_string())
        };
        let resp = PluginToCore::RawResult(RawToolResult {
            id: ctx.query_id,
            data,
            error: None,
        });
        send_message(writer, &resp);
    } else {
        // query 路径：旧协议，返回 PluginItem 列表给主窗口展示
        let items = if let Some(originals) = ctx.batch_originals.as_ref() {
            let Some(results) = parse_tagged_batch(&result, originals.len()) else {
                eprintln!(
                    "[translate] {} tag 解析失败,标记 tag_poisoned,降级单行并发",
                    ctx.engine_id
                );
                ctx.tag_poisoned = true;
                try_next_fallback(writer, pending, ctx);
                return;
            };
            vec![PluginItem {
                title: format!("已翻译 {} 行", results.len()),
                subtitle: Some("批量翻译完成".into()),
                score: 1.0,
                action: PluginAction::None,
                payload: Some(serde_json::json!({ "results": results })),
                ..Default::default()
            }]
        } else {
            build_result_items(&result, &ctx.text, &ctx.original_text, &ctx.target_lang)
        };
        let resp = PluginToCore::Response(PluginResponse {
            id: ctx.query_id,
            items,
            error: None,
        });
        send_message(writer, &resp);
    }
}

/// 构造翻译结果 items（1:1 对齐 Python：译文 + 原文，预处理变化时加拆分版）。
///
/// **0.11 review L7**：首项（译文）显式填 `payload`，让 AI tool-call 路径直接拿到
/// 结构化数据（与 protocol.rs 文档对齐），不再靠 core 的 action 兜底投影。
fn build_result_items(
    result: &str,
    text: &str,
    original_text: &str,
    _target_lang: &str,
) -> Vec<PluginItem> {
    let mut items = vec![PluginItem {
        title: format!("📝 {result}"),
        subtitle: Some(format!(
            "按 Enter 复制译文 | 原文: {}{}",
            &original_text[..original_text.chars().take(50).map(char::len_utf8).sum()],
            if original_text.chars().count() > 50 {
                "..."
            } else {
                ""
            }
        )),
        score: 1.0,
        action: PluginAction::Copy {
            text: result.into(),
        },
        // L7: 显式填 payload，AI tool-call 路径直接读 {translated, source}，
        // 不依赖 core 从 action 兜底（与 protocol.rs 文档"译文项填 payload"一致）
        payload: Some(serde_json::json!({
            "translated": result,
            "source": original_text,
        })),
        ..Default::default()
    }];

    // 预处理改变了文本 → 额外提供拆分后的版本
    if text != original_text {
        items.push(PluginItem {
            title: format!("🔤 {text}"),
            subtitle: Some("按 Enter 复制拆分后的命名 | 来自命名风格预处理".into()),
            score: 0.9,
            action: PluginAction::Copy { text: text.into() },
            ..Default::default()
        });
    }

    let orig_preview = &original_text[..original_text.chars().take(60).map(char::len_utf8).sum()];
    items.push(PluginItem {
        title: format!(
            "📄 {orig_preview}{}",
            if original_text.chars().count() > 60 {
                "..."
            } else {
                ""
            }
        ),
        subtitle: Some("按 Enter 复制原文".into()),
        score: 0.8,
        action: PluginAction::Copy {
            text: original_text.into(),
        },
        ..Default::default()
    });

    items
}

// ── 单测（AGENTS.md §7: 纯逻辑/算法必须有单测）──────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tagged_batch_roundtrip_preserves_input_order() {
        let texts = vec!["hello".to_string(), "world".to_string()];
        let tagged = build_tagged_batch(&texts);
        // 私用区字符 U+E000/U+E001 包夹数字
        assert_eq!(
            tagged,
            "\u{E000}0\u{E001}\nhello\n\n\u{E000}1\u{E001}\nworld"
        );
        // 引擎返回乱序也能按 tag 拆回原顺序
        let translated = "\u{E000}1\u{E001}\n世界\n\n\u{E000}0\u{E001}\n你好";
        assert_eq!(
            parse_tagged_batch(translated, 2).unwrap(),
            vec!["你好".to_string(), "世界".to_string()]
        );
    }

    #[test]
    fn tagged_batch_tolerates_fullwidth_digits() {
        // 某些引擎(百度/有道)会把 ASCII 数字全角化:0→０, 1→１
        let translated = "\u{E000}１\u{E001}\n世界\n\n\u{E000}０\u{E001}\n你好";
        assert_eq!(
            parse_tagged_batch(translated, 2).unwrap(),
            vec!["你好".to_string(), "世界".to_string()]
        );
    }

    #[test]
    fn tagged_batch_rejects_missing_or_duplicate_tags() {
        // 缺一个 tag(只有 0,缺 1)→ None
        assert!(parse_tagged_batch("\u{E000}0\u{E001}\n你好", 2).is_none());
        // tag 重复 → None
        assert!(
            parse_tagged_batch(
                "\u{E000}0\u{E001}\n你好\n\u{E000}0\u{E001}\n重复\n\u{E000}1\u{E001}\n世界",
                2
            )
            .is_none()
        );
        // 旧格式(被引擎翻译破坏后私用区字符没了)→ None
        assert!(parse_tagged_batch("[[BLINK_0]]\n你好\n[[BLINK_1]]\n世界", 2).is_none());
    }

    // ── preprocess_code_identifiers ─────────────────────────────────────────

    #[test]
    fn preprocess_plain_text_unchanged() {
        assert_eq!(preprocess_code_identifiers("hello world"), "hello world");
        assert_eq!(preprocess_code_identifiers("你好世界"), "你好世界");
    }

    #[test]
    fn preprocess_snake_case() {
        assert_eq!(preprocess_code_identifiers("hello_world"), "hello world");
        assert_eq!(
            preprocess_code_identifiers("get_user_name"),
            "get user name"
        );
    }

    #[test]
    fn preprocess_kebab_case() {
        assert_eq!(preprocess_code_identifiers("hello-world"), "hello world");
        assert_eq!(preprocess_code_identifiers("content-type"), "content type");
    }

    #[test]
    fn preprocess_camel_case() {
        assert_eq!(preprocess_code_identifiers("getUserName"), "get User Name");
        assert_eq!(preprocess_code_identifiers("helloWorld"), "hello World");
    }

    #[test]
    fn preprocess_screaming_snake() {
        assert_eq!(
            preprocess_code_identifiers("MAX_RETRY_COUNT"),
            "MAX RETRY COUNT"
        );
    }

    #[test]
    fn preprocess_acronym_camel() {
        // HTTPSConnection → HTTPS Connection（大写序列 + 小写 → 大写前断开）
        assert_eq!(
            preprocess_code_identifiers("HTTPSConnection"),
            "HTTPS Connection"
        );
    }

    #[test]
    fn preprocess_single_word_upper() {
        // 单个全大写单词不应被拆分
        assert_eq!(preprocess_code_identifiers("URL"), "URL");
        assert_eq!(preprocess_code_identifiers("HTTP"), "HTTP");
    }

    #[test]
    fn preprocess_mixed() {
        assert_eq!(
            preprocess_code_identifiers("getUserID_str"),
            "get User ID str"
        );
    }

    #[test]
    fn preprocess_empty_string() {
        assert_eq!(preprocess_code_identifiers(""), "");
    }

    // ── detect_lang ──────────────────────────────────────────────────────────

    #[test]
    fn detect_lang_pure_chinese() {
        assert_eq!(detect_lang("你好世界"), "zh");
        assert_eq!(detect_lang("今天天气真好"), "zh");
    }

    #[test]
    fn detect_lang_pure_english() {
        assert_eq!(detect_lang("hello world"), "en");
        assert_eq!(detect_lang("The quick brown fox"), "en");
    }

    #[test]
    fn detect_lang_mixed_majority_chinese() {
        // 中文占比 > 30% → zh。"你好世界 hello" = 4 中文 / 10 字符 = 40%
        assert_eq!(detect_lang("你好世界 hello"), "zh");
    }

    #[test]
    fn detect_lang_mixed_majority_english() {
        // 中文占比 < 30% → en。"hello world 你好" = 2 中文 / 14 字符 ≈ 14%
        assert_eq!(detect_lang("hello world 你好"), "en");
    }

    #[test]
    fn detect_lang_empty() {
        assert_eq!(detect_lang(""), "en");
    }

    #[test]
    fn detect_lang_japanese_not_chinese() {
        // 日文假名不在 CJK 统一汉字范围 → en
        assert_eq!(detect_lang("こんにちは"), "en");
    }

    // ── auto_swap_lang ───────────────────────────────────────────────────────

    #[test]
    fn auto_swap_explicit_target_unchanged() {
        assert_eq!(auto_swap_lang("hello", "zh"), "zh");
        assert_eq!(auto_swap_lang("你好", "en"), "en");
        assert_eq!(auto_swap_lang("hello", "ja"), "ja");
    }

    #[test]
    fn auto_swap_chinese_to_english() {
        assert_eq!(auto_swap_lang("你好世界", "auto"), "en");
    }

    #[test]
    fn auto_swap_english_to_chinese() {
        assert_eq!(auto_swap_lang("hello world", "auto"), "zh");
    }

    // ── parse_fallback_order ─────────────────────────────────────────────────

    #[test]
    fn fallback_order_default() {
        let settings = serde_json::json!({});
        let order = parse_fallback_order(&settings);
        assert_eq!(order, vec!["tencent", "ali", "baidu", "youdao", "deepl"]);
    }

    #[test]
    fn fallback_order_from_array() {
        let settings = serde_json::json!({"fallback_order": ["baidu", "youdao"]});
        let order = parse_fallback_order(&settings);
        assert_eq!(order, vec!["baidu", "youdao"]);
    }

    #[test]
    fn fallback_order_from_string() {
        let settings = serde_json::json!({"fallback_order": "baidu,youdao,deepl"});
        let order = parse_fallback_order(&settings);
        assert_eq!(order, vec!["baidu", "youdao", "deepl"]);
    }

    #[test]
    fn fallback_order_filters_invalid_engines() {
        let settings = serde_json::json!({"fallback_order": ["baidu", "invalid_engine", "youdao"]});
        let order = parse_fallback_order(&settings);
        assert_eq!(order, vec!["baidu", "youdao"]);
    }

    #[test]
    fn fallback_order_empty_array_uses_default() {
        let settings = serde_json::json!({"fallback_order": []});
        let order = parse_fallback_order(&settings);
        assert_eq!(order, vec!["tencent", "ali", "baidu", "youdao", "deepl"]);
    }

    // ── engine_display_name ──────────────────────────────────────────────────

    #[test]
    fn engine_display_name_known() {
        assert_eq!(engine_display_name("youdao"), "有道智云");
        assert_eq!(engine_display_name("baidu"), "百度翻译");
        assert_eq!(engine_display_name("deepl"), "DeepL");
        assert_eq!(engine_display_name("ali"), "阿里翻译");
        assert_eq!(engine_display_name("tencent"), "腾讯翻译");
    }

    #[test]
    fn engine_display_name_unknown_fallback() {
        assert_eq!(engine_display_name("unknown"), "unknown");
        assert_eq!(engine_display_name("custom"), "custom");
    }

    // ── get_engine ───────────────────────────────────────────────────────────

    #[test]
    fn get_engine_known() {
        assert!(get_engine("youdao").is_some());
        assert!(get_engine("baidu").is_some());
        assert!(get_engine("deepl").is_some());
        assert!(get_engine("ali").is_some());
        assert!(get_engine("tencent").is_some());
    }

    #[test]
    fn get_engine_unknown() {
        assert!(get_engine("google").is_none());
        assert!(get_engine("").is_none());
    }

    // ── build_result_items ───────────────────────────────────────────────────

    #[test]
    fn build_result_items_no_preprocess() {
        let items = build_result_items("你好", "hello", "hello", "zh");
        // 译文项 + 原文项（无拆分版，因为 text == original_text）
        assert_eq!(items.len(), 2);
        assert!(items[0].title.starts_with("📝"));
        assert!(items[1].title.starts_with("📄"));
        assert_eq!(items[0].score, 1.0);
    }

    #[test]
    fn build_result_items_with_preprocess() {
        // text != original_text → 多一个拆分版
        let items = build_result_items("获取用户", "get user", "getUserName", "zh");
        assert_eq!(items.len(), 3);
        assert!(items[0].title.starts_with("📝"));
        assert!(items[1].title.starts_with("🔤"));
        assert!(items[2].title.starts_with("📄"));
    }
}
