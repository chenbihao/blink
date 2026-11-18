//! 日志系统：文件轮转 + 控制台 + 动态级别（reload）。
//!
//! - 文件：%APPDATA%\blink\logs\，每日轮转，保留 7 天（启动与应用定期维护清理）。
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

    cleanup_old_logs(&dir);
    install_panic_hook(dir);
}

/// release 使用 panic=abort，非阻塞日志队列来不及排空。
/// panic 报告独立同步落盘，再保留默认 stderr hook 的行为。
fn install_panic_hook(dir: PathBuf) {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let report = format!("{info}\n{}", std::backtrace::Backtrace::force_capture());
        if let Err(error) = write_panic_report(&dir, &report) {
            tracing::error!(%error, "panic report: 同步写入失败");
        }
        previous(info);
    }));
}

fn write_panic_report(dir: &std::path::Path, report: &str) -> std::io::Result<()> {
    let today = chrono::Local::now().format("%Y-%m-%d");
    // 与异步 appender 分文件，避免 panic 时重入它持有的锁。
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join(format!("blink.panic.{today}.log")))?;
    let flush = file.try_clone()?;
    let subscriber = tracing_subscriber::fmt()
        .with_timer(LocalTimer)
        .with_ansi(false)
        .with_writer(Mutex::new(file))
        .finish();
    tracing::subscriber::with_default(subscriber, || {
        tracing::error!(panic = %report, "unhandled panic");
    });
    flush.sync_all()
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
    crate::utils::paths::logs_dir()
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
    // **TRACE 档也压**（0.29.2 收敛，取代 0.23.15 的"trace 放开"例外）。逐 batch
    // 4 条（forward / 首图尺寸 / 张量形状 / 解码文本）× batch 数在 trace 档同样
    // 刷屏，张量形状对应用侧排查也无行动价值；识别正文改由 blink 自身受控记录
    // （onnx_ocr/pipeline.rs `OCR 识别正文`，TRACE 档、换行转义保单行），与 STT
    // 转写正文（realtime_ws）同模式。错字 / 漏字排查看这条即可；WARN/ERROR
    // （下载失败 / resize 非法 / 字符数不一致 / 解码失败）不受影响。
    // **tungstenite / tokio_tungstenite 压到 warn**（0.29.1 修复）：WS 协议库在
    // TRACE 下逐帧打日志——poll_next/with_context/WouldBlock 每帧 6-10 行，且
    // `tungstenite::protocol` 会**不受控地 dump 完整帧 payload**：同一内容 hex 与
    // JSON 各打一份，其中 base64 音频（单帧数 KB）毫无诊断价值。协议帧细节对
    // 本项目无诊断价值，与 hyper/h2 同类恒压。连接失败（ERROR/WARN）仍可见。
    //
    // 注意压制理由是"不受控的整帧 dump + 噪音"，**不是**"TRACE 不得记转写正文"
    // ——spec-backend §3.2 对同类用户内容（剪贴板）明确留了 trace 级谨慎使用的
    // 例外。转写内容的诊断需求改由 blink 自身结构化日志受控承载（与 AI 对话
    // 同模式）：`blink_domain_stt::realtime_ws` 仅在 TRACE 档记录 delta 预览 /
    // completed 定稿 / Final 最终正文（不含音频），供排查错字断句；
    // debug/info 档不输出正文（见 `transcript_content_is_trace_only` 测试）。
    let transport_noise = "hyper=warn,reqwest=warn,hyper_util=warn,h2=warn,rustls=warn,tower=warn,hpack=warn,tungstenite=warn,tokio_tungstenite=warn";
    let ai_noise = "rig=warn,rig_core=warn,rig_agent=warn";
    let keyring_noise = "keyring=warn,keyring_core=warn";
    let ort_noise = "ort=warn";
    let ocr_noise = "oar_ocr_core=warn,oar_ocr=warn";
    match level {
        // trace：oar_ocr 也恒压——识别正文由 pipeline.rs 受控 trace 记录（0.29.2）
        "trace" => format!(
            "trace,sqlx=warn,tauri=warn,tao=warn,rmcp=warn,{ort_noise},{ocr_noise},{transport_noise},{ai_noise},{keyring_noise}"
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

/// 启动与定期清理共用：只处理 Blink 的日期日志（含 panic 与旧命名）。
pub fn cleanup_old_logs(dir: &std::path::Path) -> usize {
    let cutoff = SystemTime::now() - Duration::from_secs(60 * 60 * 24 * RETAIN_DAYS);
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let mut removed = 0;
    let today = chrono::Local::now().date_naive();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let day = name
            .strip_prefix("blink.panic.")
            .and_then(|n| n.strip_suffix(".log"))
            .or_else(|| {
                name.strip_prefix("blink.")
                    .and_then(|n| n.strip_suffix(".log"))
            })
            .or_else(|| name.strip_prefix("blink.log."));
        let date = day.and_then(|day| chrono::NaiveDate::parse_from_str(day, "%Y-%m-%d").ok());
        if !date.is_some_and(|date| date < today)
            || !entry.file_type().is_ok_and(|kind| kind.is_file())
        {
            continue;
        }
        if let Ok(meta) = entry.metadata()
            && let Ok(modified) = meta.modified()
            && modified < cutoff
        {
            match std::fs::remove_file(entry.path()) {
                Ok(()) => removed += 1,
                Err(error) => tracing::warn!(%error, "过期日志清理失败"),
            }
        }
    }
    if removed > 0 {
        tracing::info!(removed, "过期日志清理完成");
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::{parse_level, write_panic_report};

    /// 测试用 writer：把 fmt 层输出捕获到内存（行为级日志验证测试共用）。
    #[derive(Clone, Default)]
    struct CaptureWriter(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for CaptureWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for CaptureWriter {
        type Writer = CaptureWriter;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    /// 用指定 EnvFilter 指令执行 emit 并捕获全部日志输出。行为级验证——
    /// 用真实过滤器跑真实事件，而不是只断言指令字符串。
    fn capture_with(directives: &str, emit: &impl Fn()) -> String {
        let capture = CaptureWriter::default();
        let subscriber = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::filter::EnvFilter::new(directives))
            .with_writer(capture.clone())
            .with_ansi(false)
            .finish();
        tracing::subscriber::with_default(subscriber, emit);
        String::from_utf8(capture.0.lock().unwrap().clone()).unwrap()
    }

    #[test]
    fn cleanup_keeps_current_recent_and_unrelated_logs_and_removes_legacy_names() {
        use std::{
            fs::File,
            time::{Duration, SystemTime},
        };
        let root = tempfile::tempdir().unwrap();
        let today = chrono::Local::now().date_naive();
        let old_day = today - chrono::Days::new(10);
        let old_time = SystemTime::now() - Duration::from_secs(10 * 86400);
        let mut paths = Vec::new();
        for name in [
            format!("blink.{old_day}.log"),
            format!("blink.panic.{old_day}.log"),
            format!("blink.log.{old_day}"),
            format!("blink.{today}.log"),
            "other.log".into(),
        ] {
            let path = root.path().join(name);
            File::create(&path).unwrap().set_modified(old_time).unwrap();
            paths.push(path);
        }
        let recent = root
            .path()
            .join(format!("blink.{}.log", today - chrono::Days::new(1)));
        File::create(&recent).unwrap();
        assert_eq!(super::cleanup_old_logs(root.path()), 3);
        assert!(paths[..3].iter().all(|path| !path.exists()));
        assert!(paths[3..].iter().all(|path| path.exists()));
        assert!(recent.exists());
    }

    #[test]
    fn panic_report_is_written_synchronously_and_appended() {
        let dir = tempfile::tempdir().unwrap();
        write_panic_report(dir.path(), "首次启动失败 at src/main.rs:42").unwrap();
        write_panic_report(dir.path(), "second panic").unwrap();
        let files: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(files.len(), 1);
        let content = std::fs::read_to_string(files[0].path()).unwrap();
        assert!(
            content.contains("首次启动失败 at src/main.rs:42"),
            "{content}"
        );
        assert!(content.contains("second panic"), "{content}");
        assert_eq!(content.matches("unhandled panic").count(), 2);
    }

    #[test]
    fn parse_level_caps_third_party_noise() {
        for lvl in ["debug", "info", "trace"] {
            let f = parse_level(lvl);
            assert!(f.starts_with(lvl), "{f}");
            // 0.22.9：ort 内部细节恒压 warn（Env 硬编码 VERBOSE 的兜底）
            assert!(f.contains("ort=warn"), "{f}");
            assert!(f.contains("sqlx=warn"), "{f}");
            // 0.23.15 起 debug/info 压掉；0.29.2 起 trace 也压——识别正文由
            // pipeline.rs 受控记录，不再依赖引擎逐 batch 日志
            assert!(f.contains("oar_ocr_core=warn"), "{f}");
            assert!(f.contains("oar_ocr=warn"), "{f}");
        }
        assert_eq!(parse_level("error"), "error");
        assert_eq!(parse_level("bogus"), "error");
    }

    /// 0.29.1：WS 协议库在 TRACE 下逐帧打印完整 payload（云端语音转写正文、
    /// base64 音频）。行为级验证——用真实 EnvFilter 过滤实际事件，而不是只断言
    /// 指令字符串：`parse_level` 的所有可调档位下这些 target 的 TRACE 不得输出，
    /// 其 WARN 与 blink 自身 TRACE 不受影响。target 取自实机日志。
    #[test]
    fn websocket_frame_logs_are_suppressed_at_every_level() {
        fn emit_and_capture(directives: &str) -> String {
            capture_with(directives, &|| {
                tracing::trace!(target: "tungstenite::protocol", "Received message TRANSCRIPT_MARK");
                tracing::trace!(target: "tungstenite::protocol::frame", "writing frame AUDIO_MARK");
                tracing::trace!(target: "tungstenite::protocol::frame::frame", "Parsed headers HEADER_MARK");
                tracing::trace!(target: "tokio_tungstenite", "Stream.poll_next POLL_MARK");
                tracing::trace!(target: "tokio_tungstenite::compat", "WouldBlock COMPAT_MARK");
                tracing::warn!(target: "tungstenite::protocol", "WS_WARN_MARK");
                tracing::trace!(target: "blink::app::voice", "APP_TRACE_MARK");
            })
        }

        const FRAME_MARKS: [&str; 5] = [
            "TRANSCRIPT_MARK",
            "AUDIO_MARK",
            "HEADER_MARK",
            "POLL_MARK",
            "COMPAT_MARK",
        ];

        // 对照组：不带压制指令的裸 trace 下这些行确实会输出——证明本测试有检出力。
        let unfiltered = emit_and_capture("trace");
        for mark in FRAME_MARKS {
            assert!(unfiltered.contains(mark), "对照组应输出 {mark}：{unfiltered}");
        }

        // trace 档：帧级日志全部拦住；WS 的 WARN 与 blink 自身 TRACE 仍可见。
        let trace = emit_and_capture(&parse_level("trace"));
        for mark in FRAME_MARKS {
            assert!(!trace.contains(mark), "trace 档不得输出 {mark}：{trace}");
        }
        assert!(trace.contains("WS_WARN_MARK"), "WS 告警必须保留：{trace}");
        assert!(trace.contains("APP_TRACE_MARK"), "blink 自身 TRACE 必须保留：{trace}");

        // 其余可调档位同样不得泄露帧内容。
        for lvl in ["debug", "info"] {
            let out = emit_and_capture(&parse_level(lvl));
            for mark in FRAME_MARKS {
                assert!(!out.contains(mark), "{lvl} 档不得输出 {mark}：{out}");
            }
            assert!(out.contains("WS_WARN_MARK"), "{lvl} 档 WS 告警必须保留：{out}");
        }
    }

    /// 0.29.1：转写正文（预览/定稿/最终文本）由 blink 自身在 TRACE 档受控记录
    /// ——trace 档可见（用户显式开深挖档排查错字断句），debug/info 不泄露。
    /// 这是 spec-backend §3.2 铁则 5 对用户内容"trace 级谨慎使用"例外的应用
    /// （与剪贴板内容同模式），与 tungstenite 不受控的整帧 dump 不同。
    #[test]
    fn transcript_content_is_trace_only() {
        fn emit_and_capture(directives: &str) -> String {
            capture_with(directives, &|| {
                tracing::trace!(
                    target: "blink_domain_stt::realtime_ws",
                    preview = "语音预览正文 MARK",
                    "云端流式 delta（预览）"
                );
                tracing::trace!(
                    target: "blink_domain_stt::realtime_ws",
                    final_text = "最终正文 MARK",
                    "云端流式 Final（最终正文）"
                );
            })
        }

        let trace = emit_and_capture(&parse_level("trace"));
        assert!(trace.contains("语音预览正文 MARK"), "trace 档必须可见：{trace}");
        assert!(trace.contains("最终正文 MARK"), "trace 档必须可见：{trace}");
        for lvl in ["debug", "info"] {
            let out = emit_and_capture(&parse_level(lvl));
            assert!(
                !out.contains("正文 MARK") && !out.contains("预览正文 MARK"),
                "{lvl} 档不得泄露转写正文：{out}"
            );
        }
    }

    /// 0.29.2：oar-ocr（PP-OCRv6）逐 batch 内部日志（DBPostProcess / CRNN
    /// forward·首图尺寸·张量形状·解码文本）在所有可调档位恒压——trace 档不再
    /// 放开（0.23.15 例外的收敛）。识别正文由 blink 自身受控记录（与转写正文
    /// 同模式）。target 取自实机日志，行为级验证真实过滤器拦得住。
    #[test]
    fn oar_ocr_batch_logs_are_suppressed_at_every_level() {
        fn emit() {
            tracing::debug!(
                target: "oar_ocr_core::processors::db_postprocess",
                "DBPostProcess: pred 704x960 DET_MARK"
            );
            tracing::debug!(
                target: "oar_ocr_core::models::recognition::crnn",
                "CRNN forward: 16 images FWD_MARK"
            );
            tracing::debug!(
                target: "oar_ocr_core::models::recognition::crnn",
                "CRNN postprocess REC_MARK"
            );
            tracing::warn!(target: "oar_ocr_core", "OCR_WARN_MARK");
            tracing::trace!(
                target: "blink_infra::local_engine::onnx_ocr",
                "PIPELINE_TRACE_MARK"
            );
        }

        // 对照组：不带压制指令的裸 trace 下引擎 DEBUG 确实会输出——证明本测试有检出力。
        let unfiltered = capture_with("trace", &emit);
        for mark in ["DET_MARK", "FWD_MARK", "REC_MARK"] {
            assert!(unfiltered.contains(mark), "对照组应输出 {mark}：{unfiltered}");
        }

        // trace 档：引擎 batch 日志全部拦住；引擎 WARN 与 blink 自身 TRACE 仍可见。
        let trace = capture_with(&parse_level("trace"), &emit);
        for mark in ["DET_MARK", "FWD_MARK", "REC_MARK"] {
            assert!(!trace.contains(mark), "trace 档不得输出 {mark}：{trace}");
        }
        assert!(trace.contains("OCR_WARN_MARK"), "引擎告警必须保留：{trace}");
        assert!(trace.contains("PIPELINE_TRACE_MARK"), "blink 自身 TRACE 必须保留：{trace}");

        // 其余可调档位同样不得泄露；引擎 WARN 仍可见。
        for lvl in ["debug", "info"] {
            let out = capture_with(&parse_level(lvl), &emit);
            for mark in ["DET_MARK", "FWD_MARK", "REC_MARK"] {
                assert!(!out.contains(mark), "{lvl} 档不得输出 {mark}：{out}");
            }
            assert!(out.contains("OCR_WARN_MARK"), "{lvl} 档引擎告警必须保留：{out}");
        }
    }
}
