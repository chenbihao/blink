//! Blink 翻译插件（Rust 版）— 5 引擎多语言翻译。
//!
//! 1:1 迁移自 Python 版（plugins/builtin/translate/main.py）。
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

use engine::TranslateEngine;
use engines::{TencentEngine, AliEngine, BaiduEngine, YoudaoEngine, DeeplEngine};
use protocol::{CoreToPlugin, PluginToCore, HttpRequest, PluginResponse, ToolResultPayload, PluginError, PluginItem, PluginAction};

/// 引擎显示名称（1:1 对齐 Python）
const ENGINE_NAMES: &[(&str, &str)] = &[
    ("youdao", "有道智云"),
    ("baidu", "百度翻译"),
    ("deepl", "DeepL"),
    ("ali", "阿里翻译"),
    ("tencent", "腾讯翻译"),
];

fn engine_display_name(id: &str) -> &str {
    ENGINE_NAMES.iter().find(|(k, _)| *k == id).map(|(_, v)| *v).unwrap_or(id)
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
    let chinese = text.chars().filter(|&c| ('\u{4e00}'..='\u{9fff}').contains(&c)).count();
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
    if detected == "zh" { "en".into() } else { "zh".into() }
}

// ── 主循环（HTTP 代理状态机）─────────────────────────────────────────────────

/// 待处理的 HTTP 请求上下文（发 http_request 后等 http_response 恢复）。
struct PendingTranslate {
    query_id: String,
    is_tool_call: bool,
    text: String,           // 预处理后的文本
    original_text: String,  // 原始输入
    target_lang: String,
    engine_id: String,
    fallback_order: Vec<String>,
    settings: Value,
    /// 已尝试的引擎（含主引擎，降级时跳过）
    tried_engines: Vec<String>,
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
            CoreToPlugin::Query { id, query, settings } => {
                handle_query(&mut stdout, &mut pending, id, query, settings);
            }
            CoreToPlugin::ToolCall { id, tool_name, arguments, settings } => {
                handle_tool_call(&mut stdout, &mut pending, id, tool_name, arguments, settings);
            }
            CoreToPlugin::HttpResponse { id, status, body, error } => {
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
    let engine = settings.get("default_engine").and_then(Value::as_str).unwrap_or("youdao").to_string();
    let target_lang = settings.get("target_lang").and_then(Value::as_str).unwrap_or("zh").to_string();

    let text = query.trim();
    if text.is_empty() {
        let resp = PluginToCore::Response(PluginResponse { id, items: vec![], error: None });
        send_message(writer, &resp);
        return;
    }

    let original_text = text.to_string();
    let processed = preprocess_code_identifiers(text);
    let target_lang = auto_swap_lang(&processed, &target_lang);
    let fallback_order = parse_fallback_order(&settings);

    eprintln!("[translate] query: id={id}, engine={engine}, target={target_lang}");

    try_translate(writer, pending, id, false, processed, original_text, target_lang, engine, fallback_order, settings);
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
    let engine = settings.get("default_engine").and_then(Value::as_str).unwrap_or("youdao").to_string();

    if tool_name != "translate" {
        let resp = PluginToCore::ToolResult(ToolResultPayload {
            id, items: vec![],
            error: Some(PluginError { code: "UNKNOWN_TOOL".into(), message: format!("未知 tool: {tool_name}") }),
        });
        send_message(writer, &resp);
        return;
    }

    let text = arguments.get("text").and_then(Value::as_str).unwrap_or("").trim().to_string();
    if text.is_empty() {
        let resp = PluginToCore::ToolResult(ToolResultPayload {
            id, items: vec![],
            error: Some(PluginError { code: "MISSING_ARG".into(), message: "缺少 text 参数".into() }),
        });
        send_message(writer, &resp);
        return;
    }

    let mut target_lang = arguments.get("target_lang").and_then(Value::as_str).unwrap_or("").to_string();
    if target_lang.is_empty() || target_lang == "auto" {
        target_lang = settings.get("target_lang").and_then(Value::as_str).unwrap_or("zh").to_string();
    }
    let target_lang = auto_swap_lang(&text, &target_lang);
    let fallback_order = parse_fallback_order(&settings);

    eprintln!("[translate] tool_call: id={id}, text={text:?}, target={target_lang}");

    try_translate(writer, pending, id, true, text.clone(), text, target_lang, engine, fallback_order, settings);
}

/// 解析降级顺序设置（兼容字符串和数组格式）。
fn parse_fallback_order(settings: &Value) -> Vec<String> {
    let default = vec!["tencent".into(), "ali".into(), "baidu".into(), "youdao".into(), "deepl".into()];
    let default_val = serde_json::json!(default);
    let val = settings.get("fallback_order").unwrap_or(&default_val);
    let raw: Vec<String> = match val {
        Value::String(s) => s.split(',').map(|e| e.trim().to_string()).collect(),
        Value::Array(arr) => arr.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect(),
        _ => default,
    };
    let valid: std::collections::HashSet<&str> = valid_engines().iter().copied().collect();
    raw.into_iter().filter(|e| valid.contains(e.as_str())).collect()
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
    issue_translate_request(writer, pending, PendingTranslate {
        query_id, is_tool_call, text, original_text, target_lang,
        engine_id, fallback_order, settings,
        tried_engines: vec![],
    });
}

/// 构造 HTTP 请求并发送给 core（pending 存上下文）。
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

    let http_id = format!("tr_{}", chrono::Local::now().timestamp_millis());
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
    let next = ctx.fallback_order.iter()
        .find(|e| !ctx.tried_engines.contains(e))
        .cloned();

    match next {
        Some(engine_id) => {
            eprintln!("[translate] 降级到: {} ({})", engine_display_name(&engine_id), engine_id);
            ctx.engine_id = engine_id;
            issue_translate_request(writer, pending, ctx);
        }
        None => {
            // 所有引擎都失败 → 返回错误
            let error = PluginError {
                code: "TRANSLATE_FAILED".into(),
                message: "翻译失败，请检查 API 配置或网络连接".into(),
            };
            let resp = if ctx.is_tool_call {
                PluginToCore::ToolResult(ToolResultPayload { id: ctx.query_id, items: vec![], error: Some(error) })
            } else {
                PluginToCore::Response(PluginResponse { id: ctx.query_id, items: vec![], error: Some(error) })
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
    let Some(ctx) = pending.remove(&id) else {
        eprintln!("[translate] http response for unknown request: {id}");
        return;
    };

    let engine = get_engine(&ctx.engine_id);

    // 解析响应
    let translated = if error.is_some() || status != 200 {
        eprintln!("[translate] {} HTTP error: status={status}, error={:?}", ctx.engine_id, error);
        None
    } else {
        let body = body.unwrap_or_default();
        let result = engine.as_ref().and_then(|e| e.parse_response(&body));
        if result.is_none() {
            eprintln!("[translate] {} parse failed, body: {}", ctx.engine_id, &body[..body.len().min(500)]);
        }
        result
    };

    match translated {
        Some(result) => {
            // 成功 → 返回翻译结果
            let items = build_result_items(&result, &ctx.text, &ctx.original_text, &ctx.target_lang);
            let resp = if ctx.is_tool_call {
                PluginToCore::ToolResult(ToolResultPayload { id: ctx.query_id, items, error: None })
            } else {
                PluginToCore::Response(PluginResponse { id: ctx.query_id, items, error: None })
            };
            send_message(writer, &resp);
        }
        None => {
            // 失败 → 降级
            try_next_fallback(writer, pending, ctx);
        }
    }
}

/// 构造翻译结果 items（1:1 对齐 Python：译文 + 原文，预处理变化时加拆分版）。
fn build_result_items(result: &str, text: &str, original_text: &str, target_lang: &str) -> Vec<PluginItem> {
    let _lang_display = [("zh","中文"),("en","英文"),("ja","日文"),("ko","韩文")]
        .iter().find(|(k,_)| *k == target_lang).map(|(_,v)|*v).unwrap_or(target_lang);

    let mut items = vec![PluginItem {
        title: format!("📝 {result}"),
        subtitle: Some(format!("按 Enter 复制译文 | 原文: {}{}", &original_text[..original_text.chars().take(50).map(char::len_utf8).sum()], if original_text.chars().count() > 50 {"..."} else {""})),
        score: 1.0,
        action: PluginAction::Copy { text: result.into() },
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
        title: format!("📄 {orig_preview}{}", if original_text.chars().count() > 60 {"..."} else {""}),
        subtitle: Some("按 Enter 复制原文".into()),
        score: 0.8,
        action: PluginAction::Copy { text: original_text.into() },
        ..Default::default()
    });

    items
}
