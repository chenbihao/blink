//! Blink 离线词典插件（0.28.4）——主窗快查 / 独立详情页 / AI·CLI·MCP 纯数据
//! 复用同一查询实现（dict.rs），词库数据集独立版本化（builder.rs 构建）。
//!
//! 运行形态：
//! - 无参数：JSONL stdio 服务（宿主拉起；manifest protocol_version 2，spawn 后
//!   先收 initialize 帧获得 datasets 目录，应答 initialized 后进业务帧循环）。
//! - `--build-basic / --build-ety / --verify / --check-quality`：词库构建与
//!   校验 CLI（xtask 与数据发布流程调用；不依赖宿主）。
//!
//! 查询语义（phase §3.4）：
//! - 明确查词关键词走确定路由（"查词 apple" Takeover）；普通单词的混排/划词
//!   建议不能抢占应用搜索——本插件不注册 context 触发。
//! - 纯中文输入 → 基础汉英反查；英文输入 → 精确词头 → 词形还原 → 前缀 →
//!   拼写候选；未收录给显式转翻译入口（Route），不自动联网或 AI 补词源。
//! - 词源缺失如实显示（etymology 空）；发音后置。

mod builder;
mod dict;
mod protocol;

use dict::{BASIC_DATASET_ID, DictState, DictionaryService};
use protocol::{CoreToPlugin, PluginAction, PluginItem, PluginToCore, PluginResponse, RawToolResult};
use std::io::{BufRead, Write};
use std::path::PathBuf;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        run_service();
        return;
    }
    let code = run_cli(&args);
    std::process::exit(code);
}

// ── JSONL 服务（宿主子进程）─────────────────────────────────────────────────

fn run_service() {
    let service = DictionaryService::new();
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut out = std::io::BufWriter::new(stdout.lock());

    for line in stdin.lock().lines() {
        let Ok(line) = line else {
            break;
        };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let frame: CoreToPlugin = match serde_json::from_str(line) {
            Ok(f) => f,
            Err(e) => {
                eprintln!("[dictionary][warn] 帧解析失败: {e}");
                continue;
            }
        };
        match frame {
            CoreToPlugin::Initialize { protocol, dirs } => {
                if let Some(dirs) = dirs
                    && let Some(datasets) = dirs.datasets
                {
                    service.set_datasets_dir(PathBuf::from(&datasets));
                    eprintln!(
                        "[dictionary][info] v2 握手：datasets = {datasets}（协议 {protocol}）"
                    );
                }
                let _ = write_frame(
                    &mut out,
                    &PluginToCore::Initialized { protocol: 2 },
                );
            }
            CoreToPlugin::Query { id, query, settings } => {
                let items = handle_query(&service, &query, settings.as_ref());
                let _ = write_frame(
                    &mut out,
                    &PluginToCore::Response(PluginResponse {
                        id,
                        items,
                        error: None,
                    }),
                );
            }
            CoreToPlugin::ToolCall {
                id,
                tool_name,
                arguments,
                settings: _,
            } => {
                let result = handle_tool(&service, &id, &tool_name, &arguments);
                let _ = write_frame(&mut out, &PluginToCore::RawResult(result));
            }
            CoreToPlugin::Cancel { .. } => {
                // 本地查询即时完成，无可取消阶段
            }
        }
    }
}

fn write_frame<W: Write>(out: &mut W, frame: &PluginToCore) -> std::io::Result<()> {
    serde_json::to_writer(&mut *out, frame).map_err(std::io::Error::other)?;
    out.write_all(b"\n")?;
    // BufWriter 不 flush 则帧滞留 8KB 缓冲，宿主读到超时（0.28.4 起 stdio
    // 全链路因此恒超时）。JSONL 协议按行应答，每帧必须立刻冲刷。
    out.flush()
}

/// 主窗快查（Takeover）→ 条目列表。
fn handle_query(
    service: &DictionaryService,
    query: &str,
    settings: Option<&serde_json::Value>,
) -> Vec<PluginItem> {
    let show_en_def = settings
        .and_then(|s| s.get("show_english_definition"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let q = query.trim();

    match service.state() {
        DictState::NoDirs | DictState::NotInstalled => {
            return vec![info_item(
                "离线词库未安装",
                "设置 → 插件 → 离线词典 → 数据包 安装后可用",
            )];
        }
        DictState::Corrupt(reason) => {
            return vec![info_item(
                "离线词库数据损坏",
                &format!("{reason}；在设置页重装数据包可修复"),
            )];
        }
        DictState::Ready => {}
    }

    // 纯中文（含 CJK）→ 基础汉英反查
    if q.chars().any(|c| ('\u{4e00}'..='\u{9fff}').contains(&c)) {
        return match service.reverse_zh(q) {
            Ok(hits) if !hits.is_empty() => hits
                .into_iter()
                .map(|h| PluginItem {
                    title: h.word.clone(),
                    subtitle: Some(truncate(&h.translation, 60)),
                    score: 0.9,
                    action: PluginAction::OpenPage {
                        page_id: None,
                        payload: Some(serde_json::json!({"word": h.word})),
                    },
                    payload: Some(serde_json::json!({"word": h.word, "translation": h.translation})),
                })
                .collect(),
            Ok(_) => vec![route_to_translate(q, "无对应词条")],
            Err(e) => vec![error_item(&e)],
        };
    }

    match service.lookup(q) {
        Ok((Some(entry), _)) => {
            let summary = service.summarize(&entry);
            let mut subtitle = entry
                .lemma_of
                .clone()
                .unwrap_or_else(|| entry.phonetic.clone());
            if show_en_def
                && let Some(first_def) = entry
                    .definition
                    .split("\\n")
                    .map(str::trim)
                    .find(|l| !l.is_empty())
            {
                subtitle.push_str(" · ");
                subtitle.push_str(&truncate(first_def, 40));
            }
            let mut items = vec![
                PluginItem {
                    title: format!("{} {}", entry.display, summary_word_marker(&summary)),
                    subtitle: Some(format!(
                        "{subtitle} | 按 Enter 打开详情 | {summary}"
                    )),
                    score: 0.95,
                    action: PluginAction::OpenPage {
                        page_id: None,
                        payload: Some(serde_json::json!({"word": entry.word})),
                    },
                    payload: Some(serde_json::to_value(&entry).unwrap_or_default()),
                },
                PluginItem {
                    title: format!("复制「{}」释义", entry.display),
                    subtitle: Some(truncate(&summary, 80)),
                    score: 0.6,
                    action: PluginAction::Copy { text: summary.clone() },
                    payload: Some(serde_json::json!({"word": entry.word, "copy": summary})),
                },
            ];
            if entry.lemma_of.is_none() && !entry.forms.is_empty() {
                let forms: Vec<String> = entry
                    .forms
                    .iter()
                    .map(|f| format!("{}（{}）", f.form, form_kind_zh(&f.kind)))
                    .collect();
                items.push(PluginItem {
                    title: format!("{} 词形：{}", entry.display, forms.join(" / ")),
                    subtitle: None,
                    score: 0.5,
                    action: PluginAction::None,
                    payload: None,
                });
            }
            items
        }
        Ok((None, candidates)) if !candidates.is_empty() => candidates
            .into_iter()
            .map(|c| PluginItem {
                title: c.word.clone(),
                subtitle: Some(format!(
                    "{} | {}",
                    if c.kind == "spell" { "拼写建议" } else { "前缀匹配" },
                    truncate(&c.translation, 50)
                )),
                score: 0.5,
                action: PluginAction::OpenPage {
                    page_id: None,
                    payload: Some(serde_json::json!({"word": c.word})),
                },
                payload: Some(serde_json::to_value(&c).unwrap_or_default()),
            })
            .collect(),
        // 英文未收录（无候选）→ 显式转翻译；原输入保留在输入框
        Ok((None, _)) => vec![route_to_translate(q, "未收录")],
        Err(e) => vec![error_item(&e)],
    }
}

fn handle_tool(
    service: &DictionaryService,
    id: &str,
    tool: &str,
    args: &serde_json::Value,
) -> RawToolResult {
    let tool = match tool {
        "dictionary_lookup" => Tool::Lookup,
        "dictionary_search_zh" => Tool::SearchZh,
        other => {
            return RawToolResult {
                id: id.into(),
                data: serde_json::Value::Null,
                error: Some(protocol::PluginError {
                    code: "unknown_tool".into(),
                    message: format!("未知 tool: {other}"),
                }),
                desc: None,
            };
        }
    };

    let input = match tool {
        Tool::Lookup => args
            .get("word")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string()),
        Tool::SearchZh => args
            .get("term")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string()),
    };
    let Some(input) = input else {
        return RawToolResult {
            id: id.into(),
            data: serde_json::Value::Null,
            error: Some(protocol::PluginError {
                code: "invalid_arguments".into(),
                message: "缺少查询参数（word / term）".into(),
            }),
            desc: None,
        };
    };

    match tool {
        Tool::Lookup => {
            let (data, desc) = match service.state() {
                DictState::NoDirs | DictState::NotInstalled => (
                    serde_json::json!({
                        "status": "not_installed",
                        "dataset": BASIC_DATASET_ID,
                        "word": input,
                    }),
                    "离线词库未安装",
                ),
                DictState::Corrupt(reason) => (
                    serde_json::json!({
                        "status": "corrupt",
                        "dataset": BASIC_DATASET_ID,
                        "word": input,
                        "reason": reason,
                    }),
                    "离线词库损坏",
                ),
                DictState::Ready => match service.lookup(&input) {
                    Ok((Some(entry), _)) => (
                        serde_json::to_value(&entry).unwrap_or_default(),
                        "离线词典 · 已收录",
                    ),
                    Ok((None, candidates)) => (
                        serde_json::json!({
                            "status": "not_found",
                            "word": input,
                            "candidates": candidates,
                        }),
                        "离线词典 · 未收录",
                    ),
                    Err(e) => (
                        serde_json::json!({"status": "error", "word": input, "error": e}),
                        "离线词典 · 查询失败",
                    ),
                },
            };
            RawToolResult {
                id: id.into(),
                data,
                error: None,
                desc: Some(desc.into()),
            }
        }
        Tool::SearchZh => match service.reverse_zh(&input) {
            Ok(hits) => {
                let data = serde_json::to_value(&hits).unwrap_or_default();
                RawToolResult {
                    id: id.into(),
                    data,
                    error: None,
                    desc: Some(format!("汉英反查 · {} 条", hits.len())),
                }
            }
            Err(e) => RawToolResult {
                id: id.into(),
                data: serde_json::Value::Null,
                error: Some(protocol::PluginError {
                    code: "lookup_failed".into(),
                    message: e,
                }),
                desc: None,
            },
        },
    }
}

enum Tool {
    Lookup,
    SearchZh,
}

// ── 条目构造 helper ─────────────────────────────────────────────────────────

/// 未收录转翻译：Route 回写输入框（"翻译 <原文>"），原文不丢。
fn route_to_translate(input: &str, reason: &str) -> PluginItem {
    PluginItem {
        title: format!("「{input}」{reason}，转翻译"),
        subtitle: Some("按 Enter 以翻译关键词重查".into()),
        score: 0.4,
        action: PluginAction::Route {
            query: format!("翻译 {input}"),
        },
        payload: None,
    }
}

fn info_item(title: &str, subtitle: &str) -> PluginItem {
    PluginItem {
        title: title.into(),
        subtitle: (!subtitle.is_empty()).then(|| subtitle.into()),
        score: 0.3,
        action: PluginAction::None,
        payload: None,
    }
}

fn error_item(message: &str) -> PluginItem {
    PluginItem {
        title: format!("词典查询失败: {message}"),
        subtitle: None,
        // 负分 = 错误信息项（前端不可执行，排最后）
        score: -1.0,
        action: PluginAction::None,
        payload: None,
    }
}

/// 标题的释义摘要（截断到 24 字符，避免长释义挤爆主行）。
fn summary_word_marker(summary: &str) -> String {
    truncate(summary, 24)
}

fn truncate(s: &str, max_chars: usize) -> String {
    let s = s.split("\\n").map(str::trim).find(|l| !l.is_empty()).unwrap_or(s.trim());
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let cut: String = s.chars().take(max_chars).collect();
    format!("{cut}…")
}

fn form_kind_zh(kind: &str) -> &'static str {
    match kind {
        "p" => "过去式",
        "d" => "过去分词",
        "i" => "现在分词",
        "3" => "第三人称单数",
        "r" => "比较级",
        "t" => "最高级",
        "s" => "复数",
        _ => "词形",
    }
}

// ── 构建/校验 CLI（xtask 与数据发布流程调用）─────────────────────────────────

fn run_cli(args: &[String]) -> i32 {
    let flag_value = |flag: &str| -> Option<String> {
        args.iter()
            .position(|a| a == flag)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };

    if args.iter().any(|a| a == "--build-basic") {
        let Some(csv) = flag_value("--ecdict") else {
            eprintln!("缺少 --ecdict <ecdict.csv>");
            return 2;
        };
        let profile = flag_value("--profile")
            .and_then(|p| builder::Profile::parse(&p))
            .unwrap_or(builder::Profile::Basic);
        let version = flag_value("--version").unwrap_or_else(|| "1.0.0".into());
        let out = flag_value("--out").unwrap_or_else(|| "target/dictionary".into());
        let opts = builder::BuildOptions {
            profile,
            version: version.clone(),
            source_revision: flag_value("--source-revision"),
            name: flag_value("--name").unwrap_or_else(|| "Blink 离线词库（ECDICT）".into()),
        };
        return match builder::build_basic_dataset(&PathBuf::from(csv), &opts, &PathBuf::from(out))
        {
            Ok(output) => {
                println!(
                    "OK {} entries={} phrases={} forms={} zh_terms={} content_sha256={}",
                    output.package_dir.display(),
                    output.stats.entries,
                    output.stats.phrases,
                    output.stats.forms,
                    output.stats.zh_terms,
                    output.content_sha256
                );
                0
            }
            Err(e) => {
                eprintln!("FAIL {e}");
                1
            }
        };
    }

    if args.iter().any(|a| a == "--build-ety") {
        let Some(wordroot) = flag_value("--wordroot") else {
            eprintln!("缺少 --wordroot <wordroot.txt>");
            return 2;
        };
        let version = flag_value("--version").unwrap_or_else(|| "1.0.0".into());
        let out = flag_value("--out").unwrap_or_else(|| "target/dictionary".into());
        return match builder::build_etymology_dataset(
            &PathBuf::from(wordroot),
            &version,
            &PathBuf::from(out),
        ) {
            Ok(output) => {
                println!(
                    "OK {} roots={} words={} content_sha256={}",
                    output.package_dir.display(),
                    output.stats.entries,
                    output.stats.forms,
                    output.content_sha256
                );
                0
            }
            Err(e) => {
                eprintln!("FAIL {e}");
                1
            }
        };
    }

    if args.iter().any(|a| a == "--verify") {
        let Some(dir) = flag_value("--verify") else {
            eprintln!("缺少 --verify <package_dir>");
            return 2;
        };
        let dir = PathBuf::from(dir);
        return match builder::verify_dataset(&dir) {
            Ok(true) => {
                println!("OK 数据包校验通过: {}", dir.display());
                0
            }
            Ok(false) => {
                eprintln!("FAIL 数据包校验未通过: {}", dir.display());
                1
            }
            Err(e) => {
                eprintln!("FAIL {e}");
                1
            }
        };
    }

    if args.iter().any(|a| a == "--check-quality") {
        let Some(dir) = flag_value("--check-quality") else {
            eprintln!("缺少 --check-quality <package_dir>");
            return 2;
        };
        let samples = flag_value("--samples").unwrap_or_else(|| {
            "plugins/examples/dictionary/quality-samples.json".into()
        });
        return match builder::check_quality_samples(&PathBuf::from(dir), &PathBuf::from(samples)) {
            Ok(()) => {
                println!("OK 质量样本全部达标");
                0
            }
            Err(e) => {
                eprintln!("FAIL {e}");
                1
            }
        };
    }

    eprintln!(
        "用法：\n  （无参数）JSONL 服务模式\n  --build-basic --ecdict <csv> [--profile basic|full] [--version V] [--out DIR] [--source-revision R]\n  --build-ety --wordroot <json> [--version V] [--out DIR]\n  --verify <package_dir>\n  --check-quality <package_dir> [--samples <json>]"
    );
    2
}
