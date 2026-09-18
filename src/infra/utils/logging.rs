//! 日志系统：文件轮转 + 控制台 + 动态级别（reload）。
//!
//! - 文件：%APPDATA%\blink\logs\，每日轮转，保留 7 天（启动时清理旧文件）。
//! - 控制台：始终输出 stderr（release 无控制台时无害丢弃；debug 可见）。
//! - 级别：EnvFilter + reload，默认 error，update_level 运行时切换（设置页触发）。

use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::time::{Duration, SystemTime};

use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::filter::EnvFilter;
use tracing_subscriber::fmt::format::Writer;
use tracing_subscriber::fmt::time::FormatTime;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

/// 日志时间戳用本地时区格式化（tracing 默认 UTC，与用户观感不符）。
struct LocalTimer;

impl FormatTime for LocalTimer {
    fn format_time(&self, w: &mut Writer<'_>) -> std::fmt::Result {
        write!(
            w,
            "{}",
            chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.3f")
        )
    }
}

/// 日志保留天数
const RETAIN_DAYS: u64 = 7;

/// 非阻塞 writer 的 guard（必须保活到程序结束，否则可能丢末尾日志）
static GUARD: OnceLock<WorkerGuard> = OnceLock::new();
/// 动态级别切换（闭包包装 reload handle，避免暴露泛型类型）
type ReloadFn = Box<dyn Fn(&str) + Send + Sync>;
static RELOAD: OnceLock<ReloadFn> = OnceLock::new();
/// 当前日志级别（供 update_level 重载用）
static CURRENT_LEVEL: OnceLock<Mutex<String>> = OnceLock::new();

/// 初始化日志系统。level: error/info/debug。
pub fn init(level: &str) {
    let dir = log_dir();
    std::fs::create_dir_all(&dir).ok();

    // 每日轮转文件 appender（文件名 blink.YYYY-MM-DD.log，.log 后缀方便软件打开）
    let file_appender = tracing_appender::rolling::Builder::new()
        .rotation(tracing_appender::rolling::Rotation::DAILY)
        .filename_prefix("blink")
        .filename_suffix("log")
        .build(&dir)
        .expect("failed to build log appender");
    let (writer, guard) = tracing_appender::non_blocking(file_appender);
    let _ = GUARD.set(guard);

    // 动态级别 filter（reload 可运行时改）
    *current_level().lock().unwrap() = level.to_string();

    let filter = EnvFilter::new(parse_level(level));
    let (filter_layer, handle) = tracing_subscriber::reload::Layer::new(filter);
    let _ = RELOAD.set(Box::new(move |lvl: &str| {
        *current_level().lock().unwrap() = lvl.to_string();
        let _ = handle.reload(EnvFilter::new(parse_level(lvl)));
    }));

    tracing_subscriber::registry()
        .with(filter_layer)
        // 文件：本地时区时间戳；关闭 ANSI 颜色码（否则文件里是乱码方块）
        .with(
            tracing_subscriber::fmt::layer()
                .with_timer(LocalTimer)
                .with_writer(writer)
                .with_ansi(false),
        )
        // 控制台：本地时区时间戳；保留 ANSI 彩色（release 无控制台时 stderr 丢弃，无害）
        .with(
            tracing_subscriber::fmt::layer()
                .with_timer(LocalTimer)
                .with_writer(std::io::stderr),
        )
        .init();

    clean_old_logs(&dir);
}

/// 运行时切换日志级别（设置页触发，立即生效）。
pub fn update_level(level: &str) {
    *current_level().lock().unwrap() = level.to_string();
    if let Some(f) = RELOAD.get() {
        f(level);
    }
}

/// 日志目录：%APPDATA%\blink\logs
pub fn log_dir() -> PathBuf {
    crate::infra::utils::paths::logs_dir()
}

/// 当天日志文件路径（tracing-appender daily 格式：blink.log.YYYY-MM-DD）。
pub fn current_log_file() -> PathBuf {
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    log_dir().join(format!("blink.{today}.log"))
}

/// 级别字符串归一化为 EnvFilter 指令（非法值降级 error）。
fn current_level() -> &'static Mutex<String> {
    CURRENT_LEVEL.get_or_init(|| Mutex::new("error".to_string()))
}

/// 级别字符串归一化为 EnvFilter 指令（非法值降级 error）。
fn parse_level(level: &str) -> String {
    // 第三方库（sqlx/tauri/tao）压到 warn，避免 query/asset/IME 字符消息等 debug/trace 噪音
    // 淹没 blink 自身日志。tao 的「⌨️ Received a CHAR message…」在 TRACE 下每键一条，
    // 是 IME 合成字符的内部诊断，对用户无价值。
    // **AI SLO 埋点**(0.9.0 §5.3)：`blink::ai::slo` target 是 `blink` 的子级,自动继承根级
    // filter——`info/debug/trace` 都会捕获;`error` 级别下 SLO event 会被过滤（预期,
    // 用户显式压 error 是"我什么都不想看"信号,不该被 AI 遥测污染）。
    //
    // **AI 相关噪音压制**（0.9.2 第一步）:rig 依赖链 h2 / rustls / tower / hpack
    // 在 TRACE 下会喷协议帧(每个 HTTP/2 请求上百行),用户开 trace 是要看自家逻辑,
    // 不是看 TLS 握手。这些统一压到 warn。
    //
    // **传输层噪音恒压**（0.21.16）:reqwest/hyper/hyper_util/h2/rustls/tower/hpack 属于
    // 传输/协议层,`connecting to …` / `starting new connection` / TLS 握手 / HTTP/2 帧
    // 在 DEBUG/TRACE 下会喷屏且与 blink 业务无关。一律压到 warn（ERROR/WARN 仍可见），
    // 不随级别变化——这些噪音不该混入对话诊断日志。
    //
    // **rig_core / rig 压到 warn**（0.12.6 起，0.21.16 固定）:rig 的 `invoke_agent` /
    // `chat_streaming` span 携带 `gen_ai.system_instructions`(完整系统提示词) +
    // `gen_ai.prompt`(用户输入) 等字段,嵌套 span 导致同名字段重复输出
    // (`gen_ai.prompt="…" gen_ai.prompt="…"`),且 `rig::completions` 在 TRACE 下每次请求
    // 都打印完整 tool 列表 JSON(24 个 tool schema 数千行)。这些噪音淹没 blink 自身日志。
    // 压到 warn 后:ERROR(SSE 解析失败等)和 WARN(空响应)仍可见,但不再有 span 字段污染。
    // 0.21.16 移除「AI 对话完全打印日志」开关后不再解除压制——对话诊断由 blink 自身的
    // 结构化日志覆盖(chat_prompt 入参 / 流式增量 / 完整输入输出,见 commands/ai.rs 与
    // agent_provider.rs)。
    //
    // **rmcp 压到 warn**（0.13.9 修复）:rmcp 的 `serve_inner` span 在 TRACE 下打印
    // 每条 JSON-RPC 消息的完整内容（含 tool 列表 schema，单条数千行），与 rig 同类噪音。
    // MCP 连接/握手/工具调用结果由 blink 自身的 mcp::client 日志覆盖，rmcp 内部协议
    // 细节无诊断价值。压到 warn 后 ERROR（连接失败）和 WARN 仍可见。
    //
    // **keyring / keyring_core 压到 warn**（0.18.7 修复）:keyring v4 每次读/写密钥
    // 内部刷 4 条 DEBUG（creating entry / create entry wrapping / created entry /
    // get password），启动时 AI factory 构造 N 个 provider = 4N 行，淹没 blink 自身
    // 日志。blink 自己的 `密钥已从 keyring 读回`（store.rs，结构化、无明文）已覆盖
    // 诊断需求，keyring 内部 CM 调用细节无价值。
    //
    // **ort 压到 warn**（0.22.9 修复）:ort 的 tracing 桥接在创建 ORT Env 时硬编码
    // VERBOSE 级别（rc.13 environment.rs），OCR in-process Session 构建期会喷
    // GraphTransformer/BFCArena 等内部细节 INFO（数百行）。用户调高级别是想看
    // blink 自身逻辑；ORT 内部细节由引擎日志面板（worker 管道）单独承载。
    //
    // **oar_ocr_core / oar_ocr 在 DEBUG·INFO 下压到 warn**（0.23.15 收尾）:
    // oar-ocr（PP-OCRv6 det→crop→rec，in-process，不 spawn 子进程）的内部诊断全部是
    // **逐 batch** 输出——`DBPostProcess: pred 416x704, src 414x695`、
    // `CRNN forward: 16 images` / `First image size` / `preprocess output shape` /
    // `postprocess: 16 texts, first 3: [...]`，另有一大批 slanet / pp_formulanet /
    // table_structure_decode / layout_utils 的 tensor 细节。batch 数随文本行数线性增长，
    // 一次大选区 OCR 就能连打几十行，把 blink 自身日志挤出视野。
    // blink 侧已有等价摘要（`onnx_ocr/pipeline.rs` 的
    // `map_oarocr_to_ocr_result 完成`:regions/lines/words/char_boxes/text_chars），
    // 日常排查 OCR 是否"识别到东西"够用；WARN/ERROR（下载失败、resize 尺寸非法、
    // 字符数不一致、解码失败）不受影响。
    //
    // **例外：TRACE 档不压**（见下方 match 的 trace 分支）。这里的其它噪音组在 trace 下
    // 也一律压掉，因为协议帧 / IME 内部消息 / 密钥库调用对本项目永远没有诊断价值；
    // 而 oar-ocr 的这几行是**唯一**能看到"模型真实输入尺寸 + 每 batch 原始识别文本"的
    // 窗口，错字 / 漏字 / 字符框错位排查需要它。深挖 OCR 质量时把设置页级别切到 trace
    // 即可拿回全部细节。
    let transport_noise =
        "hyper=warn,reqwest=warn,hyper_util=warn,h2=warn,rustls=warn,tower=warn,hpack=warn";
    let ai_noise = "rig=warn,rig_core=warn,rig_agent=warn";
    let keyring_noise = "keyring=warn,keyring_core=warn";
    let ort_noise = "ort=warn";
    let ocr_noise = "oar_ocr_core=warn,oar_ocr=warn";
    match level {
        // trace：唯一放开 oar_ocr_core 的档位（OCR 内部细节只在深挖时可见）
        "trace" => format!(
            "trace,sqlx=warn,tauri=warn,tao=warn,rmcp=warn,{ort_noise},{transport_noise},{ai_noise},{keyring_noise}"
        ),
        "debug" => format!(
            "debug,sqlx=warn,tauri=warn,rmcp=warn,{ort_noise},{ocr_noise},{transport_noise},{ai_noise},{keyring_noise}"
        ),
        "info" => format!(
            "info,sqlx=warn,tauri=warn,rmcp=warn,{ort_noise},{ocr_noise},{transport_noise},{ai_noise},{keyring_noise}"
        ),
        _ => "error".to_string(),
    }
}

/// 清理超过保留天数的旧日志（按文件 mtime，启动时执行）。
fn clean_old_logs(dir: &PathBuf) {
    let cutoff = SystemTime::now() - Duration::from_secs(60 * 60 * 24 * RETAIN_DAYS);
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let is_log = entry
            .file_name()
            .to_str()
            .is_some_and(|n| n.starts_with("blink.") && n.ends_with(".log"));
        if !is_log {
            continue;
        }
        if let Ok(meta) = entry.metadata()
            && let Ok(modified) = meta.modified()
            && modified < cutoff
        {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::parse_level;

    #[test]
    fn parse_level_caps_third_party_noise() {
        for lvl in ["debug", "info"] {
            let f = parse_level(lvl);
            assert!(f.starts_with(lvl), "{f}");
            // 0.22.9：ort 内部细节恒压 warn（Env 硬编码 VERBOSE 的兜底）
            assert!(f.contains("ort=warn"), "{f}");
            assert!(f.contains("sqlx=warn"), "{f}");
            // 0.23.15：oar-ocr（PP-OCRv6）逐 batch 内部细节在 debug/info 下压掉
            assert!(f.contains("oar_ocr_core=warn"), "{f}");
            assert!(f.contains("oar_ocr=warn"), "{f}");
        }
        // trace 是唯一放开 oar_ocr_core 的档位：OCR 错字/漏框排查需要原始 batch 细节，
        // 其余噪音组（协议帧 / IME / 密钥库）在 trace 下仍然压掉。
        let t = parse_level("trace");
        assert!(t.starts_with("trace"), "{t}");
        assert!(t.contains("ort=warn"), "{t}");
        assert!(!t.contains("oar_ocr"), "{t}");
        assert_eq!(parse_level("error"), "error");
        assert_eq!(parse_level("bogus"), "error");
    }
}
