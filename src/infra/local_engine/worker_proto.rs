//! Blink 常驻 GGUF worker 的 NDJSON stdin/stdout 协议客户端（0.22.7，协议 v1）。
//!
//! 协议真源：`xtask/funasr-worker/blink_worker_protocol.h`（worker 侧实现，
//! 构建时随补丁注入 FunASR 源码树）。Rust 侧本模块与其逐字段对齐：
//!
//! ```text
//! ready（模型加载完成后输出一次）:
//!   {"type":"ready","protocol_version":1,"engine_id":..,"instance_id":..,
//!    "token_fingerprint":"fp:..","model_id":..,"model_revision":..,
//!    "model_status":"ready","model_content_fingerprint":"<64hex>",
//!    "backend":"cpu","requested_backend":"cpu"}
//! 请求: {"type":"hello","protocol_version":1}
//!       {"type":"transcribe","request_id":"..","audio_path":"..","language":?}
//!       {"type":"shutdown"}
//! 响应: {"type":"hello_ok",...}
//!       {"type":"transcribe_result","request_id":"..","ok":bool,"text":?,"error":?,"elapsed_ms":?}
//!       {"type":"error","request_id":?,"error":{"code":..,"message":..}}
//! ```
//!
//! ## 铁则
//!
//! - stdout 只承载协议（每行一条 JSON，UTF-8）；worker 诊断写 stderr。
//! - `request_id` 原样关联；**迟到/错位结果不污染当前请求**——收到不匹配
//!   的 result 记 warn 并继续等待匹配项。
//! - **同一时刻只允许一个请求在途**：客户端用请求锁串行化（并发调用方
//!   排队等待，不做并发调度）。
//! - 管道断裂/EOF/非 JSON 行是协议违例：当前等待者收到结构化错误，
//!   客户端置 poisoned，后续调用立即失败（不伪装健康）。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStdin, ChildStdout};
use tokio::sync::Mutex;

/// 冻结的协议版本（与 worker 侧 `BLINK_WORKER_PROTOCOL_VERSION` 一致）。
pub const WORKER_PROTOCOL_VERSION: u32 = 1;

// ── 消息模型 ─────────────────────────────────────────────────────────────

/// worker 错误（结构化，code 为稳定字符串）。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{code}: {message}")]
pub struct WorkerError {
    pub code: String,
    pub message: String,
}

/// 协议层错误（客户端侧）。
#[derive(Debug, thiserror::Error)]
pub enum WorkerProtoError {
    #[error("worker 管道已断开（进程可能已退出）")]
    Disconnected,
    #[error("worker 协议违例: {0}")]
    Protocol(String),
    #[error("worker 返回错误: {0}")]
    Worker(#[from] WorkerError),
    #[error("等待 worker 响应超时（{timeout_ms}ms）")]
    Timeout { timeout_ms: u64 },
    #[error("写入 worker stdin 失败: {0}")]
    Write(String),
}

/// 解析后的一行 worker 输出。
#[derive(Debug, Clone)]
pub enum WorkerLine {
    /// ready（原始 JSON——manager 复用 HTTP health 校验路径做身份核对）。
    Ready(serde_json::Value),
    /// hello_ok（原始 JSON）。
    HelloOk(serde_json::Value),
    /// 转录结果。
    TranscribeResult {
        request_id: Option<String>,
        ok: bool,
        text: Option<String>,
        error: Option<WorkerError>,
        elapsed_ms: Option<f64>,
    },
    /// worker 主动错误（不关联具体请求或关联失败）。
    Error {
        request_id: Option<String>,
        error: WorkerError,
    },
}

/// reader task → client 的事件。
#[derive(Debug)]
enum WorkerEvent {
    Line(WorkerLine),
    /// stdout 出现非 JSON 行——协议违例（stdout 被污染）。
    Garbage(String),
    /// stdout EOF（worker 退出或管道关闭）。
    Eof,
}

/// 解析一行 worker stdout 输出。
///
/// `Ok(None)` 表示可忽略的行（如未知但良性的 type）；
/// `Err` 表示该行不是合法 JSON——协议违例。
pub fn parse_worker_line(line: &str) -> Result<Option<WorkerLine>, String> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    let v: serde_json::Value =
        serde_json::from_str(trimmed).map_err(|e| format!("非 JSON 行（{e}）: {trimmed}"))?;
    let kind = v
        .get("type")
        .and_then(|t| t.as_str())
        .ok_or_else(|| format!("缺少 type 字段: {trimmed}"))?;
    let request_id = || {
        v.get("request_id")
            .and_then(|r| r.as_str())
            .map(String::from)
    };
    let take_error = || {
        v.get("error").and_then(|e| {
            Some(WorkerError {
                code: e.get("code")?.as_str()?.to_string(),
                message: e
                    .get("message")
                    .and_then(|m| m.as_str())
                    .unwrap_or_default()
                    .to_string(),
            })
        })
    };
    match kind {
        "ready" | "hello_ok" | "selftest" => Ok(Some(if kind == "ready" {
            WorkerLine::Ready(v)
        } else if kind == "hello_ok" {
            WorkerLine::HelloOk(v)
        } else {
            // selftest 行只出现在 --blink-selftest 单次模式，运行期出现视为良性行
            return Ok(None);
        })),
        "transcribe_result" => {
            let ok = v.get("ok").and_then(|b| b.as_bool()).unwrap_or(false);
            Ok(Some(WorkerLine::TranscribeResult {
                request_id: request_id(),
                ok,
                text: v.get("text").and_then(|t| t.as_str()).map(String::from),
                error: take_error(),
                elapsed_ms: v.get("elapsed_ms").and_then(|e| e.as_f64()),
            }))
        }
        "error" => {
            let error = take_error().unwrap_or(WorkerError {
                code: "unknown".to_string(),
                message: trimmed.to_string(),
            });
            Ok(Some(WorkerLine::Error {
                request_id: request_id(),
                error,
            }))
        }
        other => Err(format!("未知 type '{other}': {trimmed}")),
    }
}

// ── 客户端 ───────────────────────────────────────────────────────────────

/// 转录请求的受限识别选项（闭合字段，不透传任意参数）。
#[derive(Debug, Clone, Default)]
pub struct TranscribeOptions {
    /// 语言提示（如 "zh"）——仅 SenseVoice 语义有效，worker 按需消费。
    pub language: Option<String>,
}

/// NDJSON worker 客户端。
///
/// 持有 worker 的 stdin/stdout；`Arc<NdjsonWorkerClient>` 可克隆共享。
/// 请求串行化：`transcribe_*` / `hello` 共用一把请求锁，保证同一时刻
/// 只有一个请求在途（并发调用方排队）。
pub struct NdjsonWorkerClient {
    stdin: Mutex<ChildStdin>,
    /// reader task 的事件流（每客户端一个）。
    events: Mutex<tokio::sync::mpsc::Receiver<WorkerEvent>>,
    /// 请求序号（单调递增，request_id 生成用）。
    seq: AtomicU64,
    /// 协议违例/EOF 后置位——后续调用立即失败。
    poisoned: AtomicBool,
}

impl NdjsonWorkerClient {
    /// 创建客户端并启动 stdout reader task。
    ///
    /// reader task 独立持有 `ChildStdout`；客户端 drop 后 channel 关闭，
    /// reader task 的 send 失败自然退出。
    pub fn new(stdin: ChildStdin, stdout: ChildStdout) -> Arc<Self> {
        let (tx, rx) = tokio::sync::mpsc::channel::<WorkerEvent>(64);
        tokio::spawn(async move {
            let mut reader = BufReader::new(stdout);
            let mut line = String::new();
            let mut total_lines = 0u32;
            loop {
                line.clear();
                match reader.read_line(&mut line).await {
                    Ok(0) => {
                        tracing::info!(total_lines, "worker stdout reader: EOF");
                        let _ = tx.send(WorkerEvent::Eof).await;
                        break;
                    }
                    Ok(_) => {
                        total_lines += 1;
                        let event = match parse_worker_line(&line) {
                            Ok(Some(parsed)) => WorkerEvent::Line(parsed),
                            Ok(None) => continue,
                            Err(reason) => WorkerEvent::Garbage(reason),
                        };
                        tracing::info!(
                            total_lines,
                            preview = %line.trim().chars().take(80).collect::<String>(),
                            "worker stdout reader: line received"
                        );
                        if tx.send(event).await.is_err() {
                            break;
                        }
                    }
                    Err(e) => {
                        tracing::info!(%e, total_lines, "worker stdout read error");
                        let _ = tx.send(WorkerEvent::Eof).await;
                        break;
                    }
                }
            }
        });
        Arc::new(Self {
            stdin: Mutex::new(stdin),
            events: Mutex::new(rx),
            seq: AtomicU64::new(0),
            poisoned: AtomicBool::new(false),
        })
    }

    /// 客户端是否已因协议违例/EOF 不可用。
    pub fn is_poisoned(&self) -> bool {
        self.poisoned.load(Ordering::Acquire)
    }

    async fn write_line(&self, line: &str) -> Result<(), WorkerProtoError> {
        if self.is_poisoned() {
            return Err(WorkerProtoError::Disconnected);
        }
        let mut stdin = self.stdin.lock().await;
        stdin
            .write_all(line.as_bytes())
            .await
            .map_err(|e| WorkerProtoError::Write(e.to_string()))?;
        stdin
            .write_all(b"\n")
            .await
            .map_err(|e| WorkerProtoError::Write(e.to_string()))?;
        stdin
            .flush()
            .await
            .map_err(|e| WorkerProtoError::Write(e.to_string()))
    }

    fn poison(&self, reason: &str) -> WorkerProtoError {
        tracing::warn!(reason, "worker 客户端置为 poisoned");
        self.poisoned.store(true, Ordering::Release);
        WorkerProtoError::Protocol(reason.to_string())
    }

    /// 等待 ready 行（deadline 由调用方给定）。
    ///
    /// **ready 只能在模型加载后出现**——等待成功即代表模型已加载。
    /// ready 前出现的其他消息/违例按协议错误处理。
    pub async fn wait_ready(
        &self,
        timeout: Duration,
    ) -> Result<serde_json::Value, WorkerProtoError> {
        let deadline = tokio::time::Instant::now() + timeout;
        let mut events = self.events.lock().await;
        let event = tokio::time::timeout_at(deadline, events.recv())
            .await
            .map_err(|_| WorkerProtoError::Timeout {
                timeout_ms: timeout.as_millis() as u64,
            })?
            .ok_or(WorkerProtoError::Disconnected)?;
        match event {
            WorkerEvent::Line(WorkerLine::Ready(v)) => Ok(v),
            WorkerEvent::Line(WorkerLine::HelloOk(_)) => Err(self.poison("ready 前收到 hello_ok")),
            WorkerEvent::Line(WorkerLine::TranscribeResult { .. }) => {
                Err(self.poison("ready 前收到 transcribe_result"))
            }
            WorkerEvent::Line(WorkerLine::Error { error, .. }) => {
                Err(WorkerProtoError::Worker(error))
            }
            WorkerEvent::Garbage(reason) => {
                Err(self.poison(&format!("ready 前协议违例: {reason}")))
            }
            WorkerEvent::Eof => Err(self.poison("等待 ready 期间 stdout EOF")),
        }
    }

    /// 发送 hello 并等待 hello_ok（运行期健康检查）。
    ///
    /// **串行化不变量**：`events` 锁的持有周期覆盖"写入请求 → 读到匹配响应"，
    /// 并发调用方在此锁上排队——同一时刻只有一个请求在途。
    pub async fn hello(&self, timeout: Duration) -> Result<serde_json::Value, WorkerProtoError> {
        let request = serde_json::json!({
            "type": "hello",
            "protocol_version": WORKER_PROTOCOL_VERSION,
        });
        let mut events = self.events.lock().await;
        self.write_line(&request.to_string()).await?;
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let event = tokio::time::timeout_at(deadline, events.recv())
                .await
                .map_err(|_| WorkerProtoError::Timeout {
                    timeout_ms: timeout.as_millis() as u64,
                })?
                .ok_or(WorkerProtoError::Disconnected)?;
            match event {
                WorkerEvent::Line(WorkerLine::HelloOk(v)) => return Ok(v),
                WorkerEvent::Line(WorkerLine::Error { error, .. }) => {
                    return Err(WorkerProtoError::Worker(error));
                }
                WorkerEvent::Line(other) => {
                    tracing::warn!(?other, "hello 等待期间收到非预期消息，忽略");
                }
                WorkerEvent::Garbage(reason) => {
                    return Err(self.poison(&format!("hello 等待期间协议违例: {reason}")));
                }
                WorkerEvent::Eof => {
                    return Err(self.poison("hello 等待期间 stdout EOF"));
                }
            }
        }
    }

    /// 发送转录请求并等待结果。
    ///
    /// `audio_path` 必须是**已 canonicalize 且位于 Blink 管理音频目录内**的
    /// 路径（由调用方保证；worker 侧同样做前缀校验）。
    ///
    /// 迟到的错位结果（request_id 不匹配）记 warn 丢弃，不污染本次结果。
    pub async fn transcribe(
        &self,
        audio_path: &std::path::Path,
        options: &TranscribeOptions,
        timeout: Duration,
    ) -> Result<TranscribeOutput, WorkerProtoError> {
        let request_id = format!("req-{}", self.seq.fetch_add(1, Ordering::Relaxed) + 1);
        let mut req = serde_json::json!({
            "type": "transcribe",
            "request_id": request_id,
            "audio_path": audio_path.to_string_lossy(),
        });
        if let Some(lang) = &options.language {
            req["language"] = serde_json::Value::String(lang.clone());
        }

        // 串行化不变量：events 锁持有覆盖"写入→匹配响应"全程（见 hello 注释）
        let mut events = self.events.lock().await;
        self.write_line(&req.to_string()).await?;

        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let event = tokio::time::timeout_at(deadline, events.recv())
                .await
                .map_err(|_| WorkerProtoError::Timeout {
                    timeout_ms: timeout.as_millis() as u64,
                })?
                .ok_or(WorkerProtoError::Disconnected)?;
            match event {
                WorkerEvent::Line(WorkerLine::TranscribeResult {
                    request_id: rid,
                    ok,
                    text,
                    error,
                    elapsed_ms,
                }) => {
                    if rid.as_deref() != Some(request_id.as_str()) {
                        // 错位结果：上一个请求的迟到响应——丢弃，不污染本次。
                        tracing::warn!(
                            expect = %request_id,
                            got = ?rid,
                            "丢弃错位的 worker 响应（迟到结果隔离）"
                        );
                        continue;
                    }
                    if ok {
                        return Ok(TranscribeOutput {
                            text: text.unwrap_or_default(),
                            elapsed_ms,
                        });
                    }
                    return Err(WorkerProtoError::Worker(error.unwrap_or(WorkerError {
                        code: "inference_failed".to_string(),
                        message: "worker 报告失败但未携带错误详情".to_string(),
                    })));
                }
                WorkerEvent::Line(WorkerLine::Error {
                    request_id: rid,
                    error,
                }) => {
                    if rid.is_none() || rid.as_deref() == Some(request_id.as_str()) {
                        return Err(WorkerProtoError::Worker(error));
                    }
                    tracing::warn!(?rid, %error, "丢弃错位的 worker 错误");
                }
                WorkerEvent::Line(other) => {
                    tracing::warn!(?other, "transcribe 等待期间收到非预期消息，忽略");
                }
                WorkerEvent::Garbage(reason) => {
                    return Err(self.poison(&format!("transcribe 等待期间协议违例: {reason}")));
                }
                WorkerEvent::Eof => {
                    return Err(self.poison("transcribe 等待期间 stdout EOF"));
                }
            }
        }
    }

    /// 发送 shutdown 请求（尽力而为；调用方随后应 drop 客户端关闭 stdin）。
    ///
    /// 不参与 events 锁串行化——shutdown 只写一行，无需等待响应。
    pub async fn request_shutdown(&self) {
        let line = r#"{"type":"shutdown"}"#;
        let _ = self.write_line(line).await;
    }
}

/// 转录输出。
#[derive(Debug, Clone)]
pub struct TranscribeOutput {
    pub text: String,
    pub elapsed_ms: Option<f64>,
}

// ── 测试 ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// 定位可用于 fake worker 的 Python 解释器（无则测试跳过）。
    fn find_test_python() -> Option<std::path::PathBuf> {
        for name in ["python", "python3"] {
            if let Ok(out) = std::process::Command::new(name)
                .arg("-c")
                .arg("print('ok')")
                .output()
                && out.status.success()
                && String::from_utf8_lossy(&out.stdout).trim() == "ok"
            {
                return Some(std::path::PathBuf::from(name));
            }
        }
        // Blink 托管解释器回退
        std::env::var("APPDATA")
            .ok()
            .map(|a| {
                std::path::PathBuf::from(a)
                    .join("blink/python/pythons/cpython-3.12.8-windows-x86_64-none/python.exe")
            })
            .filter(|p| p.exists())
    }

    #[test]
    fn parse_ready_line() {
        let line = r#"{"type":"ready","protocol_version":1,"engine_id":"funasr","instance_id":"inst-1","token_fingerprint":"fp:abcdef0123456789","model_id":"gguf/sensevoice-small-q8","model_revision":"v0.2.6","model_status":"ready","model_content_fingerprint":"a2b3c4d5","backend":"cpu","requested_backend":"cpu"}"#;
        let parsed = parse_worker_line(line).unwrap().expect("应解析出消息");
        match parsed {
            WorkerLine::Ready(v) => {
                assert_eq!(v["engine_id"], "funasr");
                assert_eq!(v["model_status"], "ready");
                assert_eq!(v["protocol_version"], 1);
            }
            other => panic!("应为 Ready，实际 {other:?}"),
        }
    }

    #[test]
    fn parse_transcribe_result_ok_and_error() {
        let ok = parse_worker_line(
            r#"{"type":"transcribe_result","request_id":"req-1","ok":true,"text":"你好","elapsed_ms":12.5}"#,
        )
        .unwrap()
        .expect("应解析出消息");
        match ok {
            WorkerLine::TranscribeResult {
                request_id,
                ok,
                text,
                error,
                elapsed_ms,
            } => {
                assert_eq!(request_id.as_deref(), Some("req-1"));
                assert!(ok);
                assert_eq!(text.as_deref(), Some("你好"));
                assert!(error.is_none());
                assert_eq!(elapsed_ms, Some(12.5));
            }
            other => panic!("应为 TranscribeResult，实际 {other:?}"),
        }

        let err = parse_worker_line(
            r#"{"type":"transcribe_result","request_id":"req-2","ok":false,"error":{"code":"inference_failed","message":"boom"}}"#,
        )
        .unwrap()
        .expect("应解析出消息");
        match err {
            WorkerLine::TranscribeResult { ok, error, .. } => {
                assert!(!ok);
                assert_eq!(error.unwrap().code, "inference_failed");
            }
            other => panic!("应为 TranscribeResult，实际 {other:?}"),
        }
    }

    #[test]
    fn parse_protocol_error_line() {
        let parsed = parse_worker_line(
            r#"{"type":"error","request_id":null,"error":{"code":"bad_json","message":"not json"}}"#,
        )
        .unwrap()
        .expect("应解析出消息");
        match parsed {
            WorkerLine::Error { request_id, error } => {
                assert!(request_id.is_none());
                assert_eq!(error.code, "bad_json");
            }
            other => panic!("应为 Error，实际 {other:?}"),
        }
    }

    #[test]
    fn malformed_json_is_protocol_violation() {
        assert!(parse_worker_line("not json at all").is_err());
        assert!(parse_worker_line(r#"{"no_type":1}"#).is_err());
        assert!(parse_worker_line(r#"{"type":"bogus"}"#).is_err());
    }

    #[test]
    fn empty_line_is_ignorable() {
        assert!(parse_worker_line("").unwrap().is_none());
        assert!(parse_worker_line("   ").unwrap().is_none());
    }

    #[test]
    fn selftest_line_is_ignorable() {
        let line = r#"{"type":"selftest","worker":"sensevoice","protocol_version":1}"#;
        assert!(parse_worker_line(line).unwrap().is_none());
    }

    /// 端到端：fake worker（echo 子进程）走 hello → transcribe → shutdown。
    /// 使用内联 Python 假 worker 保证测试不依赖真模型。
    #[tokio::test(flavor = "multi_thread")]
    async fn client_roundtrip_with_fake_worker() {
        let script = r#"
import sys, json
def emit(o): sys.stdout.write(json.dumps(o)+"\n"); sys.stdout.flush()
emit({"type":"ready","protocol_version":1,"engine_id":"funasr","instance_id":"inst-1",
      "token_fingerprint":"fp:x","model_id":"m","model_revision":"r","model_status":"ready",
      "model_content_fingerprint":"f","backend":"cpu","requested_backend":"cpu"})
for line in sys.stdin:
    line=line.strip()
    if not line: continue
    try: req=json.loads(line)
    except Exception:
        emit({"type":"error","request_id":None,"error":{"code":"bad_json","message":"x"}}); continue
    t=req.get("type")
    if t=="hello":
        emit({"type":"hello_ok","protocol_version":1,"engine_id":"funasr","instance_id":"inst-1","model_id":"m","backend":"cpu"})
    elif t=="transcribe":
        emit({"type":"transcribe_result","request_id":req.get("request_id"),"ok":True,
              "text":"识别文本:"+str(req.get("audio_path")),"elapsed_ms":1.0})
    elif t=="shutdown":
        break
    else:
        emit({"type":"error","request_id":req.get("request_id"),"error":{"code":"unknown_type","message":t}})
"#;
        let python = find_test_python();
        let Some(python) = python else {
            eprintln!("跳过：未找到可用 Python 解释器（fake worker 依赖）");
            return;
        };
        let mut cmd =
            crate::infra::platform::no_window_tokio(tokio::process::Command::new(&python));
        cmd.args(["-I", "-S", "-c", script])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let mut child = cmd.spawn().expect("fake worker spawn");
        let stdin = child.stdin.take().expect("stdin");
        let stdout = child.stdout.take().expect("stdout");

        let client = NdjsonWorkerClient::new(stdin, stdout);
        let ready = client
            .wait_ready(Duration::from_secs(10))
            .await
            .expect("ready");
        assert_eq!(ready["model_status"], "ready");

        let hello = client.hello(Duration::from_secs(10)).await.expect("hello");
        assert_eq!(hello["type"], "hello_ok");

        let out = client
            .transcribe(
                std::path::Path::new("C:/tmp/audio/a.wav"),
                &TranscribeOptions::default(),
                Duration::from_secs(10),
            )
            .await
            .unwrap_or_else(|e| {
                use tokio::io::AsyncReadExt;
                let mut err = String::new();
                if let Some(mut p) = child.stderr.take() {
                    // panic 路径：不 await stderr 读取，避免阻塞诊断
                    #[allow(clippy::let_underscore_future)]
                    let _ = p.read_to_string(&mut err);
                }
                panic!("transcribe: {e}; child stderr: {err}")
            });
        assert!(out.text.contains("识别文本:"));

        client.request_shutdown().await;
        let _ = tokio::time::timeout(Duration::from_secs(5), child.wait()).await;
    }

    /// 迟到结果隔离：fake worker 先回一个错位 request_id 的结果，再回正确结果。
    #[tokio::test(flavor = "multi_thread")]
    async fn late_result_is_isolated() {
        let script = r#"
import sys, json
def emit(o): sys.stdout.write(json.dumps(o)+"\n"); sys.stdout.flush()
emit({"type":"ready","protocol_version":1,"engine_id":"funasr","instance_id":"i",
      "token_fingerprint":"fp:x","model_id":"m","model_revision":"r","model_status":"ready",
      "model_content_fingerprint":"f","backend":"cpu","requested_backend":"cpu"})
for line in sys.stdin:
    req=json.loads(line.strip())
    if req.get("type")=="transcribe":
        # 先发迟到结果（错误 request_id），再发匹配结果
        emit({"type":"transcribe_result","request_id":"req-999","ok":True,"text":"迟到污染"})
        emit({"type":"transcribe_result","request_id":req["request_id"],"ok":True,"text":"正确结果"})
    elif req.get("type")=="shutdown":
        break
"#;
        let python = find_test_python();
        let Some(python) = python else {
            eprintln!("跳过：未找到可用 Python 解释器（fake worker 依赖）");
            return;
        };
        let mut cmd =
            crate::infra::platform::no_window_tokio(tokio::process::Command::new(&python));
        cmd.args(["-I", "-S", "-c", script])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let mut child = cmd.spawn().expect("fake worker spawn");
        let stdin = child.stdin.take().expect("stdin");
        let stdout = child.stdout.take().expect("stdout");

        let client = NdjsonWorkerClient::new(stdin, stdout);
        client
            .wait_ready(Duration::from_secs(10))
            .await
            .expect("ready");
        let out = client
            .transcribe(
                std::path::Path::new("C:/tmp/audio/a.wav"),
                &TranscribeOptions::default(),
                Duration::from_secs(10),
            )
            .await
            .expect("transcribe 应忽略迟到结果");
        assert_eq!(out.text, "正确结果");

        client.request_shutdown().await;
        let _ = tokio::time::timeout(Duration::from_secs(5), child.wait()).await;
    }

    /// stdout 污染（非 JSON 行）触发 poisoned，后续请求立即失败。
    #[tokio::test(flavor = "multi_thread")]
    async fn stdout_pollution_poisons_client() {
        let script = r#"
import sys, json
sys.stdout.write("loading model ...\n"); sys.stdout.flush()
sys.stdout.write(json.dumps({"type":"ready","protocol_version":1,"engine_id":"funasr","instance_id":"i",
  "token_fingerprint":"fp:x","model_id":"m","model_revision":"r","model_status":"ready",
  "model_content_fingerprint":"f","backend":"cpu","requested_backend":"cpu"})+"\n"); sys.stdout.flush()
for line in sys.stdin:
    req=json.loads(line.strip())
    if req.get("type")=="transcribe":
        sys.stdout.write("some log noise\n"); sys.stdout.flush()
        sys.stdout.write(json.dumps({"type":"transcribe_result","request_id":req["request_id"],"ok":True,"text":"t"})+"\n"); sys.stdout.flush()
    elif req.get("type")=="shutdown":
        break
"#;
        let python = find_test_python();
        let Some(python) = python else {
            eprintln!("跳过：未找到可用 Python 解释器（fake worker 依赖）");
            return;
        };
        let mut cmd =
            crate::infra::platform::no_window_tokio(tokio::process::Command::new(&python));
        cmd.args(["-I", "-S", "-c", script])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let mut child = cmd.spawn().expect("fake worker spawn");
        let stdin = child.stdin.take().expect("stdin");
        let stdout = child.stdout.take().expect("stdout");

        let client = NdjsonWorkerClient::new(stdin, stdout);
        // ready 前的 "loading model ..." 行 → 协议违例
        let err = client.wait_ready(Duration::from_secs(10)).await;
        assert!(err.is_err(), "ready 前的日志污染必须失败");
        assert!(client.is_poisoned());

        let _ = tokio::time::timeout(Duration::from_secs(5), child.wait()).await;
    }
}
