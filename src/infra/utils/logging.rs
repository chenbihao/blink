//! 日志系统：文件轮转 + 控制台 + 动态级别（reload）。
//!
//! - 文件：%APPDATA%\blink\logs\，每日轮转，保留 7 天（启动时清理旧文件）。
//! - 控制台：始终输出 stderr（release 无控制台时无害丢弃；debug 可见）。
//! - 级别：EnvFilter + reload，默认 error，update_level 运行时切换（设置页触发）。

use std::path::PathBuf;
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
        write!(w, "{}", chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.3f"))
    }
}

/// 日志保留天数
const RETAIN_DAYS: u64 = 7;

/// 非阻塞 writer 的 guard（必须保活到程序结束，否则可能丢末尾日志）
static GUARD: OnceLock<WorkerGuard> = OnceLock::new();
/// 动态级别切换（闭包包装 reload handle，避免暴露泛型类型）
static RELOAD: OnceLock<Box<dyn Fn(&str) + Send + Sync>> = OnceLock::new();

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
    let filter = EnvFilter::new(parse_level(level));
    let (filter_layer, handle) = tracing_subscriber::reload::Layer::new(filter);
    let _ = RELOAD.set(Box::new(move |lvl: &str| {
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
    if let Some(f) = RELOAD.get() {
        f(level);
    }
}

/// 日志目录：%APPDATA%\blink\logs
pub fn log_dir() -> PathBuf {
    let appdata = std::env::var("APPDATA").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(appdata).join("blink").join("logs")
}

/// 当天日志文件路径（tracing-appender daily 格式：blink.log.YYYY-MM-DD）。
pub fn current_log_file() -> PathBuf {
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    log_dir().join(format!("blink.{today}.log"))
}

/// 级别字符串归一化为 EnvFilter 指令（非法值降级 error）。
fn parse_level(level: &str) -> String {
    // 第三方库（sqlx/tauri）压到 warn，避免 query/asset 等 debug 噪音淹没 blink 自身日志。
    // **AI SLO 埋点**(0.9.0 §5.3)：`blink::ai::slo` target 是 `blink` 的子级,自动继承根级
    // filter——`info/debug/trace` 都会捕获;`error` 级别下 SLO event 会被过滤（预期,
    // 用户显式压 error 是"我什么都不想看"信号,不该被 AI 遥测污染）。
    //
    // **AI 相关噪音压制**（0.9.2 第一步）:rig 依赖链 h2 / rustls / tower / hpack
    // 在 TRACE 下会喷协议帧(每个 HTTP/2 请求上百行),用户开 trace 是要看自家逻辑,
    // 不是看 TLS 握手。这些统一压到 warn。`rig_core` 保留在原级别——它的
    // `completions request/response` TRACE 对诊断"发了什么/收了什么"很有价值。
    let ai_noise = "h2=warn,rustls=warn,tower=warn,hpack=warn";
    match level {
        "trace" => format!("trace,sqlx=warn,tauri=warn,hyper=warn,reqwest=warn,{ai_noise}"),
        "debug" => format!("debug,sqlx=warn,tauri=warn,{ai_noise}"),
        "info" => format!("info,sqlx=warn,tauri=warn,{ai_noise}"),
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
            .map_or(false, |n| n.starts_with("blink.") && n.ends_with(".log"));
        if !is_log {
            continue;
        }
        if let Ok(meta) = entry.metadata() {
            if let Ok(modified) = meta.modified() {
                if modified < cutoff {
                    let _ = std::fs::remove_file(entry.path());
                }
            }
        }
    }
}
