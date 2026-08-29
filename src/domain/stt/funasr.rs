//! Blink STT Server Python 环境工具 + HTTP 健康检查（0.22.6）。
//!
//! ## 设计
//!
//! 本模块提供 FunASR STT 所需的纯工具函数和 HTTP 健康检查：
//! 1. 嵌入 `blink_stt_server.py` 并在启动时释放到 `%APPDATA%\blink\python\`
//! 2. 热词文件生成（逗号分隔 → 换行分隔）
//! 3. HTTP 健康检查（`/health` 端点模型加载状态查询）
//! 4. TCP 端口预检 + server base URL 构造
//! 5. FunASR 日志噪声过滤
//! 6. 诊断状态聚合（`FunasrEnv`，兼容旧前端事件）
//!
//! ## 0.22.6 变更
//!
//! ServerStartParams / build_launch_request / start_server / mark_server_stopped
//! 已删除。启动逻辑完全由 `app/local_engine/funasr.rs` 的 `FunasrAdapter::prepare_launch`
//! 通过 `EngineManager` 统一管理，使用 generation-based 隔离环境。
//!
//! ## 兼容性
//!
//! HTTP 端点路径和响应格式与官方 `funasr-server` 完全一致，
//! 现有 Rust 侧的 `LocalSttEngine` / `PseudoStreamingSttEngine` 和 `check_model_loaded()` 无需修改。
//!
//! ## 旧全局 venv
//!
//! `get_env_status_async` 中的 `check_status_async` / `check_torch` / `check_funasr`
//! 仍读取旧全局 venv（`%APPDATA%\blink\python\venv`），仅供兼容诊断展示。
//! 新安装路径为 `runtimes/engines/funasr/generations/{install_id}/venv`。

use std::path::PathBuf;

/// 嵌入的 blink_stt_server.py 脚本（随 Rust 二进制发布）。
const BLINK_STT_SERVER_PY: &str = include_str!("../../../resources/stt/funasr/blink_stt_server.py");

/// server 启动超时（秒）。
/// 首次启动需要从 ModelScope 下载模型（~234MB），加上 PyTorch 加载，
/// 可能需要 3-5 分钟。后续启动仅模型加载，通常 30-60 秒。
#[cfg(test)]
pub const SERVER_STARTUP_TIMEOUT_SECS: u64 = 300;

// ── Python 脚本释放 ────────────────────────────────────────────────────────

/// 获取 `%APPDATA%\blink\python\` 目录路径。
fn python_dir() -> PathBuf {
    #[cfg(test)]
    {
        return crate::infra::local_engine::runtime::python_shared_root();
    }
    #[cfg(not(test))]
    crate::infra::utils::paths::python_dir()
}

/// 获取 blink_stt_server.py 的目标路径。
#[allow(dead_code)] // STT 脚本路径工具，待 release 流程消费
pub fn server_script_path() -> PathBuf {
    python_dir().join("blink_stt_server.py")
}

/// 确保 blink_stt_server.py 已释放到 `%APPDATA%\blink\python\`。
///
/// 每次调用都覆写（保证脚本随 Blink 版本更新），失败不阻断——
/// 如果文件已存在且内容相同则跳过写入。
///
/// 返回脚本路径，失败时返回 None（调用方应提示用户）。
pub fn ensure_server_script() -> Result<PathBuf, String> {
    ensure_server_script_in(&python_dir())
}

/// `ensure_server_script` 的内部实现，接受显式目标目录（测试用）。
///
/// 生产入口 [`ensure_server_script`] 使用正式 `python_dir()`，
/// 测试传入 `tempfile::TempDir` 路径以隔离真实 `%APPDATA%`。
pub(crate) fn ensure_server_script_in(dir: &std::path::Path) -> Result<PathBuf, String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("创建 python 目录失败: {e}"))?;

    let script_path = dir.join("blink_stt_server.py");

    // 检查是否已存在且内容一致（避免无谓写入）
    let need_write = match std::fs::read_to_string(&script_path) {
        Ok(existing) => existing != BLINK_STT_SERVER_PY,
        Err(_) => true, // 不存在或读取失败
    };

    if need_write {
        tracing::info!(
            path = %script_path.display(),
            "释放 blink_stt_server.py（{}字节）",
            BLINK_STT_SERVER_PY.len()
        );
        std::fs::write(&script_path, BLINK_STT_SERVER_PY)
            .map_err(|e| format!("写入 blink_stt_server.py 失败: {e}"))?;
    }

    Ok(script_path)
}

/// 将热词配置写入 `%APPDATA%\blink\python\hotwords.txt`。
///
/// 前端用英文逗号分隔热词（省空间），FunASR 要求每行一个——
/// 此函数自动将逗号 / 换行混合分隔转为换行格式。
///
/// 返回文件路径（如果 hotwords 为空则返回 None，不写文件）。
pub fn write_hotwords_file(hotwords: &Option<String>) -> Option<PathBuf> {
    write_hotwords_file_in(&python_dir(), hotwords)
}

/// `write_hotwords_file` 的内部实现，接受显式目标目录（测试用）。
///
/// 生产入口 [`write_hotwords_file`] 使用正式 `python_dir()`，
/// 测试传入 `tempfile::TempDir` 路径以隔离真实 `%APPDATA%`。
pub(crate) fn write_hotwords_file_in(
    dir: &std::path::Path,
    hotwords: &Option<String>,
) -> Option<PathBuf> {
    let normalized = normalize_hotwords(hotwords.as_deref()?);
    if normalized.is_empty() {
        return None;
    }

    if std::fs::create_dir_all(dir).is_err() {
        return None;
    }

    let path = dir.join("hotwords.txt");
    match std::fs::write(&path, &normalized) {
        Ok(()) => {
            tracing::info!(path = %path.display(), "热词文件已写入");
            Some(path)
        }
        Err(e) => {
            tracing::warn!(%e, "热词文件写入失败");
            None
        }
    }
}

/// 纯函数：将热词配置文本归一化为换行分隔格式。
///
/// 前端用英文逗号分隔热词（省空间），FunASR 要求每行一个——
/// 此函数自动将逗号 / 换行混合分隔转为换行格式。
///
/// 空白输入返回空字符串（调用方据此跳过文件写入）。
pub(crate) fn normalize_hotwords(raw: &str) -> String {
    if raw.trim().is_empty() {
        return String::new();
    }

    raw.split([',', '\n', '\r'])
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

// ── 状态结构 ──────────────────────────────────────────────────────────────

/// 检查 server 是否在指定端口上监听（TCP 级别）。
///
/// **注意**：此函数只检查 TCP 端口是否可连接，**不验证 HTTP API 是否就绪**，
/// 也**不区分端口占用者是否为 Blink 管理的子进程**。
///
/// 以下情况都会返回 `true`：
/// - Blink 通过 EngineManager 启动的子进程正在监听
/// - Blink 崩溃后遗留的孤儿进程仍在监听（child handle 已丢失）
/// - 其他程序恰好占用了同一端口
///
/// server 启动后 uvicorn 先绑定 TCP 端口，但模型可能还在加载（30-60s），
/// 此时 TCP 连接成功但 HTTP 请求会失败。
///
/// 用于快速预检（如 `LocalSttEngine::from_connection` 中的快速失败判断）。
/// 在需要确保模型真正就绪的场景，使用 [`check_model_loaded`]。
/// 在需要清理孤儿进程的场景，使用 `infra::platform::process::kill_process_by_port`。
pub fn is_server_ready(port: u16) -> bool {
    use std::net::TcpStream;
    use std::time::Duration;

    // 0.22.3：直接使用 127.0.0.1，与 Endpoint 协议一致。
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    TcpStream::connect_timeout(
        &addr,
        // 500ms 超时：服务正常时 loopback TCP 连接毫秒级返回；
        // 服务未启动时 Windows 上未监听端口会等满超时（非 RST），2s 太长会阻塞调用方。
        Duration::from_millis(500),
    )
    .is_ok()
}

/// `is_server_ready` 的异步版本，用 tokio async TCP + 短超时。
///
/// **为什么不用 `spawn_blocking(is_server_ready)`**：Windows 上 127.0.0.1 未监听端口
/// 的 `connect_timeout` 返回 "connection timed out"（非 "refused"），等满整个超时时间。
/// 旧实现用 2s 超时 + spawn_blocking，虽不阻塞 worker 线程，但 effect 串行循环仍需
/// 等 2s 才能处理 HoldReleased -> 窗口出现/消失慢一拍。
///
/// 改用 `tokio::net::TcpStream::connect` + 500ms 超时：端口有服务时毫秒级返回，
/// 无服务时最多等 500ms（而非 2s）。不影响其他 tokio task。
pub async fn is_server_ready_async(port: u16) -> bool {
    let addr = format!("127.0.0.1:{port}");
    match tokio::net::lookup_host(addr).await {
        Ok(mut addrs) => {
            if let Some(sock_addr) = addrs.next() {
                matches!(
                    tokio::time::timeout(
                        std::time::Duration::from_millis(500),
                        tokio::net::TcpStream::connect(sock_addr),
                    )
                    .await,
                    Ok(Ok(_))
                )
            } else {
                false
            }
        }
        Err(_) => false,
    }
}

/// 模型加载状态（从 Python server `/health` 端点获取）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelLoadStatus {
    /// Python server 尚未响应（HTTP 不可达或响应异常）
    Unreachable,
    /// 模型尚未开始加载（idle）
    Idle,
    /// 模型正在下载/加载中（首次需从 ModelScope 下载 ~234MB）
    Loading,
    /// 模型已就绪，可接受转录请求
    Ready,
    /// 模型加载失败
    Error,
}

/// Token-aware 模型健康检查（0.22.6 批次 3 H4）。
///
/// 使用结构化 `SttEngineConnection`（host + port + token + engine_id + instance_id）
/// 做 `/health` 请求，携带 `X-Engine-Token` header。
///
/// **这是生产链路唯一合法的 health 检查方式**——
/// Python server 的 `/health` 端点强制要求 token，
/// 无 token 的 [`check_model_loaded`] 会得到 401 并报告 `Unreachable`。
///
/// 返回 [`ModelLoadStatus`]，与 `check_model_loaded` 语义一致。
/// 401/403 返回 `Unreachable`（鉴权失败 = 无法确认服务身份）。
pub async fn check_model_loaded_with_token(
    conn: &crate::domain::stt::SttEngineConnection,
) -> ModelLoadStatus {
    let url = format!("http://{}:{}/health", conn.host, conn.port);
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(2))
        .build()
    {
        Ok(c) => c,
        Err(_) => return ModelLoadStatus::Unreachable,
    };

    let resp = match client
        .get(&url)
        .header("X-Engine-Token", &conn.token)
        .send()
        .await
    {
        Ok(r) => r,
        Err(_) => return ModelLoadStatus::Unreachable,
    };

    let status = resp.status();
    if !status.is_success() {
        // 401/403 = token 错误或服务身份不匹配
        // 对调用方来说，鉴权失败 = 无法确认服务身份 = Unreachable
        tracing::debug!(
            host = %conn.host,
            port = conn.port,
            http_status = status.as_u16(),
            "token-aware health: HTTP 非 2xx，视为 Unreachable"
        );
        return ModelLoadStatus::Unreachable;
    }

    match resp.json::<serde_json::Value>().await {
        Ok(v) => {
            // 身份字段是鉴权结果的一部分：缺失、类型错误或不匹配均 fail-closed。
            match v.get("engine_id").and_then(|s| s.as_str()) {
                Some(resp_engine) if resp_engine == conn.engine_id => {}
                actual => {
                    tracing::warn!(
                        expected = %conn.engine_id,
                        actual = ?actual,
                        "health 缺少或回显错误的 engine_id，拒绝连接"
                    );
                    return ModelLoadStatus::Unreachable;
                }
            }
            match v.get("instance_id").and_then(|s| s.as_str()) {
                Some(resp_instance) if resp_instance == conn.instance_id => {}
                actual => {
                    tracing::warn!(
                        expected = %conn.instance_id,
                        actual = ?actual,
                        "health 缺少或回显错误的 instance_id，拒绝连接"
                    );
                    return ModelLoadStatus::Unreachable;
                }
            }

            match v.get("model_status").and_then(|s| s.as_str()) {
                Some("ready") => ModelLoadStatus::Ready,
                Some("loading") => ModelLoadStatus::Loading,
                Some("error") => ModelLoadStatus::Error,
                Some("idle") => ModelLoadStatus::Idle,
                _ => {
                    if v.get("model_loaded").and_then(|b| b.as_bool()) == Some(true) {
                        ModelLoadStatus::Ready
                    } else {
                        ModelLoadStatus::Loading
                    }
                }
            }
        }
        Err(_) => ModelLoadStatus::Unreachable,
    }
}

/// Token-aware 模型就绪检查，不就绪则返回对应的错误消息（0.22.6 批次 3）。
///
/// 供 `LocalSttEngine` 和 `PseudoStreamingSttEngine` 共用。
/// 使用结构化连接快照，确保 health 和 transcribe 使用同一 endpoint/token。
pub async fn check_model_ready_or_error_with_token(
    conn: &crate::domain::stt::SttEngineConnection,
) -> Result<(), String> {
    match check_model_loaded_with_token(conn).await {
        ModelLoadStatus::Ready => Ok(()),
        ModelLoadStatus::Loading | ModelLoadStatus::Idle => Err(format!(
            "模型正在加载中（{}:{}），请稍后在设置页等待加载完成后重试。",
            conn.host, conn.port
        )),
        ModelLoadStatus::Error => Err(format!(
            "模型加载失败（{}:{}），请在设置页查看日志或检查网络连接后重启服务。",
            conn.host, conn.port
        )),
        ModelLoadStatus::Unreachable => Err(format!(
            "FunASR 服务不可达或鉴权失败（{}:{}）。请确认服务已在设置页启动，且未发生重启。",
            conn.host, conn.port
        )),
    }
}

// ── server 启动参数（0.22.6 已移除 legacy 启动路径）──────────────────────
//
// 0.22.6：ServerStartParams / build_launch_request / start_server / mark_server_stopped
// 已删除。启动逻辑完全由 app/local_engine/funasr.rs 的 FunasrAdapter::prepare_launch
// 通过 EngineManager 统一管理。此模块仅保留工具函数和 HTTP 健康检查。

/// 生成 server 的 base_url（供 HTTP 转录使用）。
///
/// 0.22.3：使用 `127.0.0.1` 而非 `localhost`，与 EngineManager 的
/// Endpoint 协议一致——Endpoint 只允许 loopback。
#[cfg(test)]
pub fn server_base_url(port: u16) -> String {
    format!("http://127.0.0.1:{port}/v1")
}

// ── 测试 ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_script_is_not_empty() {
        assert!(!BLINK_STT_SERVER_PY.is_empty());
        // 验证脚本是有效 Python（含关键标识）
        assert!(BLINK_STT_SERVER_PY.contains("blink_stt_server"));
        assert!(BLINK_STT_SERVER_PY.contains("/v1/audio/transcriptions"));
        assert!(BLINK_STT_SERVER_PY.contains("/health"));
    }

    #[test]
    fn server_script_path_is_in_python_dir() {
        let path = server_script_path();
        assert!(
            path.ends_with("python\\blink_stt_server.py")
                || path.ends_with("python/blink_stt_server.py"),
            "script path should be in python dir, got: {}",
            path.display()
        );
    }

    #[test]
    fn ensure_server_script_creates_file() {
        // 使用临时目录，不写真实 %APPDATA%
        let tmp = tempfile::tempdir().expect("创建临时目录失败");
        let path = ensure_server_script_in(tmp.path()).expect("ensure_server_script_in 失败");
        assert!(path.exists(), "script file should exist after ensure");

        // 验证文件内容与嵌入内容一致
        let content = std::fs::read_to_string(&path).expect("读取脚本失败");
        assert_eq!(content, BLINK_STT_SERVER_PY);
    }

    // ── normalize_hotwords 纯函数测试 ──

    #[test]
    fn normalize_hotwords_empty_returns_empty() {
        assert_eq!(normalize_hotwords(""), "");
        assert_eq!(normalize_hotwords("   "), "");
        assert_eq!(normalize_hotwords("  \n  \r  "), "");
    }

    #[test]
    fn normalize_hotwords_comma_separated() {
        assert_eq!(
            normalize_hotwords("美团 100, 快手 80, Blink 100"),
            "美团 100\n快手 80\nBlink 100"
        );
    }

    #[test]
    fn normalize_hotwords_newline_separated() {
        assert_eq!(
            normalize_hotwords("美团 100\n快手 80\nBlink 100"),
            "美团 100\n快手 80\nBlink 100"
        );
    }

    #[test]
    fn normalize_hotwords_mixed_separators() {
        assert_eq!(
            normalize_hotwords("美团 100, 快手 80\nBlink 100"),
            "美团 100\n快手 80\nBlink 100"
        );
    }

    #[test]
    fn normalize_hotwords_trims_whitespace() {
        assert_eq!(
            normalize_hotwords("  美团 100 ,  快手 80  "),
            "美团 100\n快手 80"
        );
    }

    #[test]
    fn normalize_hotwords_filters_empty_entries() {
        assert_eq!(
            normalize_hotwords("美团 100,, ,快手 80"),
            "美团 100\n快手 80"
        );
    }

    // ── write_hotwords_file 落盘测试（使用临时目录，只验证一次实际写入）──

    #[test]
    fn write_hotwords_none_for_empty() {
        let tmp = tempfile::tempdir().expect("创建临时目录失败");
        let result = write_hotwords_file_in(tmp.path(), &None);
        assert!(result.is_none());

        let result = write_hotwords_file_in(tmp.path(), &Some("   \n  ".to_string()));
        assert!(result.is_none());
    }

    #[test]
    fn write_hotwords_creates_file() {
        // 只验证一次实际落盘和路径
        let tmp = tempfile::tempdir().expect("创建临时目录失败");
        let hotwords = "美团 100, 快手 80, Blink 100".to_string();
        let path = write_hotwords_file_in(tmp.path(), &Some(hotwords));
        assert!(path.is_some(), "热词文件应被创建");

        let path = path.unwrap();
        assert!(path.exists(), "热词文件应存在");
        assert!(path.ends_with("hotwords.txt"));

        let content = std::fs::read_to_string(&path).expect("读取热词文件失败");
        assert_eq!(content, "美团 100\n快手 80\nBlink 100");
    }

    /// 验证嵌入的 Python 脚本包含模型名解析函数（修复 FunASR 1.3.14 短名 404 问题）。
    #[test]
    fn embedded_script_contains_model_alias_resolution() {
        assert!(
            BLINK_STT_SERVER_PY.contains("_MODEL_ALIASES"),
            "blink_stt_server.py 应包含 _MODEL_ALIASES 模型别名映射"
        );
        assert!(
            BLINK_STT_SERVER_PY.contains("_resolve_model_id"),
            "blink_stt_server.py 应包含 _resolve_model_id 函数"
        );
        assert!(
            BLINK_STT_SERVER_PY.contains("iic/SenseVoiceSmall"),
            "blink_stt_server.py 应包含完整 ModelScope ID 'iic/SenseVoiceSmall'"
        );
    }

    /// 验证嵌入的 Python 脚本包含 SenseVoice 输出标签后处理。
    ///
    /// SenseVoice 模型输出形如 `<|zh|><|NEUTRAL|><|Speech|><|withitn|>文本`，
    /// 需用 `rich_transcription_postprocess` 去除这些元数据标签。
    #[test]
    fn embedded_script_contains_postprocess_for_sensevoice_tags() {
        assert!(
            BLINK_STT_SERVER_PY.contains("_postprocess_text"),
            "blink_stt_server.py 应包含 _postprocess_text 后处理函数"
        );
        assert!(
            BLINK_STT_SERVER_PY.contains("rich_transcription_postprocess"),
            "blink_stt_server.py 应导入 rich_transcription_postprocess"
        );
        assert!(
            BLINK_STT_SERVER_PY.contains("_postprocess_text(raw_text)"),
            "transcribe 端点应调用 _postprocess_text"
        );
    }

    // ── Hermetic 集成测试（0.22.6 批次 3）──────────────────────────────────
    //
    // 使用 tokio::net::TcpListener 启动 fake HTTP server，模拟 FunASR 的
    // /health 和 /v1/audio/transcriptions 端点。完全自包含，不依赖网络
    // 或外部进程。

    /// 启动一个 fake FunASR HTTP server，返回 (port, token, engine_id, instance_id)。
    ///
    /// server 行为：
    /// - `GET /health`（携带正确 X-Engine-Token）→ 200 + model_status=ready + identity 回显
    /// - `GET /health`（无 token / 错 token）→ 401
    /// - `POST /v1/audio/transcriptions`（携带正确 token）→ 200 + {"text": "你好世界"}
    /// - 其他请求 → 404
    async fn start_fake_funasr_server() -> (u16, String, String, String) {
        start_fake_funasr_server_with_identity(true, true).await
    }

    #[test]
    fn embedded_script_forbids_implicit_submodel_downloads() {
        assert!(BLINK_STT_SERVER_PY.contains("拒绝使用短名回退以避免运行期隐式下载"));
        assert!(!BLINK_STT_SERVER_PY.contains("kwargs[\"vad_model\"] = \"fsmn-vad\""));
        assert!(!BLINK_STT_SERVER_PY.contains("kwargs[\"punc_model\"] = \"ct-punc\""));
    }

    async fn start_fake_funasr_server_with_identity(
        include_engine_id: bool,
        include_instance_id: bool,
    ) -> (u16, String, String, String) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let token = "test-token-abc123def456".to_string();
        let engine_id = "funasr".to_string();
        let instance_id = "inst-fake-001".to_string();

        let token_clone = token.clone();
        let engine_id_clone = engine_id.clone();
        let instance_id_clone = instance_id.clone();

        tokio::spawn(async move {
            loop {
                let (mut socket, _) = match listener.accept().await {
                    Ok(c) => c,
                    Err(_) => break,
                };

                let token = token_clone.clone();
                let engine_id = engine_id_clone.clone();
                let instance_id = instance_id_clone.clone();

                tokio::spawn(async move {
                    // 循环读取直到收到完整的 HTTP headers（\r\n\r\n）。
                    // POST 请求的 body 可能很大（WAV ~64KB），
                    // 但我们只需 headers 来路由——body 读完即可丢弃。
                    let mut buf = vec![0u8; 8192];
                    let mut total = 0;
                    let header_end = loop {
                        if total >= buf.len() {
                            buf.resize(buf.len() * 2, 0);
                        }
                        let n = socket.read(&mut buf[total..]).await.unwrap_or(0);
                        if n == 0 {
                            break None;
                        }
                        total += n;
                        let s = String::from_utf8_lossy(&buf[..total]);
                        if let Some(idx) = s.find("\r\n\r\n") {
                            break Some(idx);
                        }
                    };
                    // header_end 为 None 表示连接关闭前未收到完整 headers
                    let header_end = match header_end {
                        Some(e) => e,
                        None => return,
                    };
                    let request = String::from_utf8_lossy(&buf[..header_end + 4]);

                    // 解析请求行和 headers
                    let (first_line, _rest) =
                        request.split_once("\r\n").unwrap_or((request.as_ref(), ""));
                    let method_path = first_line.split_whitespace().collect::<Vec<_>>();
                    let (method, path) = (method_path.get(0), method_path.get(1));

                    // 检查 X-Engine-Token header
                    let has_valid_token = request
                        .lines()
                        .any(|line| line.eq_ignore_ascii_case(&format!("X-Engine-Token: {token}")));

                    // 对 POST 请求，读取并丢弃 body 以避免 reqwest 因连接过早关闭报 502。
                    // Content-Length 指定了 body 大小，header_end+4 之后已读的部分是 body 开头。
                    if method == Some(&"POST") {
                        // 解析 Content-Length
                        let content_length: usize = request
                            .lines()
                            .find_map(|line| {
                                let lower = line.to_ascii_lowercase();
                                if lower.starts_with("content-length:") {
                                    lower
                                        .trim_start_matches("content-length:")
                                        .trim()
                                        .parse()
                                        .ok()
                                } else {
                                    None
                                }
                            })
                            .unwrap_or(0);

                        // 已读 body 字节数
                        let body_already_read = total.saturating_sub(header_end + 4);
                        let remaining = content_length.saturating_sub(body_already_read);
                        // 读取剩余 body
                        let mut discard = vec![0u8; 4096];
                        let mut left = remaining;
                        while left > 0 {
                            let to_read = left.min(discard.len());
                            match socket.read(&mut discard[..to_read]).await {
                                Ok(0) => break,
                                Ok(n) => left -= n,
                                Err(_) => break,
                            }
                        }
                    }

                    let response = if method == Some(&"GET") && path == Some(&"/health") {
                        if has_valid_token {
                            let mut body = serde_json::json!({
                                "status": "ok",
                                "model_loaded": true,
                                "model_status": "ready",
                            });
                            if include_engine_id {
                                body["engine_id"] = serde_json::Value::String(engine_id);
                            }
                            if include_instance_id {
                                body["instance_id"] = serde_json::Value::String(instance_id);
                            }
                            let body_str = body.to_string();
                            format!(
                                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                                body_str.len(),
                                body_str
                            )
                        } else {
                            "HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\n\r\n".to_string()
                        }
                    } else if method == Some(&"POST") && path == Some(&"/v1/audio/transcriptions") {
                        if has_valid_token {
                            let body = r#"{"text":"你好世界"}"#;
                            format!(
                                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                                body.len(),
                                body
                            )
                        } else {
                            "HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\n\r\n".to_string()
                        }
                    } else {
                        "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n".to_string()
                    };

                    let _ = socket.write_all(response.as_bytes()).await;
                    let _ = socket.flush().await;
                });
            }
        });

        (port, token, engine_id, instance_id)
    }

    /// 构造一个 SttEngineConnection 用于测试。
    fn make_connection(
        port: u16,
        token: &str,
        engine_id: &str,
        instance_id: &str,
    ) -> crate::domain::stt::SttEngineConnection {
        crate::domain::stt::SttEngineConnection {
            host: "127.0.0.1".to_string(),
            port,
            token: token.to_string(),
            engine_id: engine_id.to_string(),
            instance_id: instance_id.to_string(),
        }
    }

    /// Hermetic 测试：token-aware health 检查成功。
    #[tokio::test]
    async fn hermetic_token_aware_health_success() {
        let (port, token, engine_id, instance_id) = start_fake_funasr_server().await;
        let conn = make_connection(port, &token, &engine_id, &instance_id);

        let status = check_model_loaded_with_token(&conn).await;
        assert_eq!(status, ModelLoadStatus::Ready);
    }

    /// Hermetic 测试：缺 token / 错 token 返回 Unreachable（鉴权失败）。
    #[tokio::test]
    async fn hermetic_wrong_token_returns_unreachable() {
        let (port, _token, engine_id, instance_id) = start_fake_funasr_server().await;
        let conn = make_connection(port, "wrong-token", &engine_id, &instance_id);

        let status = check_model_loaded_with_token(&conn).await;
        assert_eq!(
            status,
            ModelLoadStatus::Unreachable,
            "错 token 应返回 Unreachable（鉴权失败）"
        );
    }

    /// Hermetic 测试：错 instance_id 返回 Unreachable（服务已重启）。
    #[tokio::test]
    async fn hermetic_wrong_instance_id_returns_unreachable() {
        let (port, token, engine_id, _real_instance) = start_fake_funasr_server().await;
        let conn = make_connection(port, &token, &engine_id, "stale-instance-id");

        let status = check_model_loaded_with_token(&conn).await;
        assert_eq!(
            status,
            ModelLoadStatus::Unreachable,
            "错 instance_id 应返回 Unreachable（旧连接不能误连新实例）"
        );
    }

    #[tokio::test]
    async fn hermetic_missing_identity_fields_return_unreachable() {
        for (include_engine_id, include_instance_id) in [(false, true), (true, false)] {
            let (port, token, engine_id, instance_id) =
                start_fake_funasr_server_with_identity(include_engine_id, include_instance_id)
                    .await;
            let conn = make_connection(port, &token, &engine_id, &instance_id);
            assert_eq!(
                check_model_loaded_with_token(&conn).await,
                ModelLoadStatus::Unreachable,
                "health identity 缺失时必须 fail-closed"
            );
        }
    }

    /// Hermetic 测试：check_model_ready_or_error_with_token 成功时返回 Ok。
    #[tokio::test]
    async fn hermetic_ready_or_error_success() {
        let (port, token, engine_id, instance_id) = start_fake_funasr_server().await;
        let conn = make_connection(port, &token, &engine_id, &instance_id);

        let result = check_model_ready_or_error_with_token(&conn).await;
        assert!(result.is_ok(), "模型就绪时应返回 Ok");
    }

    /// Hermetic 测试：check_model_ready_or_error_with_token 鉴权失败时返回 Err。
    #[tokio::test]
    async fn hermetic_ready_or_error_auth_failure() {
        let (port, _token, engine_id, instance_id) = start_fake_funasr_server().await;
        let conn = make_connection(port, "bad-token", &engine_id, &instance_id);

        let result = check_model_ready_or_error_with_token(&conn).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.contains("不可达") || err.contains("鉴权"),
            "错误消息应提及不可达或鉴权: {err}"
        );
    }

    /// Hermetic 测试：完整安装→转录链路。
    ///
    /// 模拟完整流程：
    /// 1. 启动 fake server
    /// 2. 构造连接快照
    /// 3. token-aware health 检查 → Ready
    /// 4. 使用同一连接发送最小音频 fixture → 收到确定转录结果
    #[tokio::test]
    async fn hermetic_authenticated_health_and_transcribe_contract() {
        use crate::domain::stt::wav;

        let (port, token, engine_id, instance_id) = start_fake_funasr_server().await;
        let conn = make_connection(port, &token, &engine_id, &instance_id);

        // 1. token-aware health 检查
        let status = check_model_loaded_with_token(&conn).await;
        assert_eq!(status, ModelLoadStatus::Ready);

        // 2. 使用同一连接做转录
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(5))
            .build()
            .unwrap();

        // 最小音频 fixture：1 秒静音 WAV
        let samples = vec![0.0f32; 16000];
        let wav_bytes = wav::pcm_to_wav(&samples, 16000, 1);
        let url = format!("http://127.0.0.1:{port}/v1/audio/transcriptions");

        let result = wav::transcribe_with_token(
            &client,
            &url,
            Some(&conn.token),
            "iic/SenseVoiceSmall",
            &wav_bytes,
        )
        .await;

        assert!(result.is_ok(), "转录应成功: {:?}", result.err());
        let text = result.unwrap();
        assert_eq!(text, "你好世界", "转录结果应为确定的文本");
    }

    /// 跨层闭环：真实模型安装事务（model_storage staging/promote）→
    /// 服务从 manifest 恢复 → selection gate 可用 → 动态 endpoint/token
    /// health → LocalSttEngine 转录。
    #[tokio::test]
    async fn hermetic_install_restore_selection_gate_health_and_transcribe() {
        use std::sync::Arc;

        use crate::app::local_engine::model_installer::{FakeInstaller, ModelRegistry};
        use crate::app::local_engine::registry::EngineRegistry;
        use crate::app::local_engine::{EngineManager, NoopEventPort};
        use crate::domain::local_engine::EngineModelDescriptor;
        use crate::domain::stt::{SttEngine, local::LocalSttEngine};
        use crate::infra::local_engine::{model_storage, runtime};

        static MODEL_STORAGE_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
        let _storage_guard = MODEL_STORAGE_TEST_LOCK.lock().await;

        let unique = format!(
            "e2e-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let engine_id = runtime::EngineId::new("funasr").unwrap();
        let descriptor = EngineModelDescriptor {
            engine_id: engine_id.clone(),
            model_id: unique.clone(),
            display_name: "Hermetic E2E".to_string(),
            description: "test model".to_string(),
            revision: "v1".to_string(),
            checksum_source: runtime::ChecksumSource::Unverified,
            estimated_size_mb: Some(1),
            compatibility_schema: 1,
        };
        let registry = ModelRegistry::new_with_models(vec![descriptor]);
        let asset_key = model_storage::encode_asset_key(&unique);
        let asset_root = model_storage::asset_root(&engine_id, &asset_key).unwrap();

        struct Cleanup(std::path::PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let _cleanup = Cleanup(asset_root);

        let make_manager = || {
            EngineManager::new_with_providers(
                Arc::new(EngineRegistry::new_with_adapters(vec![
                    crate::app::local_engine::funasr::make_funasr_adapter(),
                ])),
                Arc::new(NoopEventPort),
                std::collections::HashMap::new(),
                crate::infra::local_engine::providers::python::PythonVenvProvider::new(),
                registry.clone(),
                Arc::new(FakeInstaller::success()),
            )
        };
        let service = make_manager();
        let installed = service
            .install_model(&engine_id, &unique, Some("e2e-install".to_string()))
            .await
            .unwrap();
        assert!(
            installed.success,
            "模型安装事务应成功: {:?}",
            installed.error
        );

        // "重启"= 新 EngineManager 实例——模型状态从磁盘 manifest 无状态读取
        let restored_service = make_manager();
        let restored = restored_service
            .get_model_status(&engine_id, &unique)
            .await
            .unwrap();
        assert!(restored.is_usable(), "恢复后的模型必须通过 selection gate");

        let (port, token, server_engine_id, instance_id) = start_fake_funasr_server().await;
        let conn = make_connection(port, &token, &server_engine_id, &instance_id);
        let mut config = crate::domain::config::stt_config::SttConfig::default();
        config.local_engine.funasr_model = unique;
        let stt = LocalSttEngine::from_connection(&config, conn).unwrap();
        stt.transcribe_chunk(&vec![0.0; 16000]).await.unwrap();
        assert_eq!(stt.finalize().await.unwrap(), "你好世界");
    }

    /// Hermetic 测试：错 token 的转录请求失败。
    #[tokio::test]
    async fn hermetic_wrong_token_transcribe_fails() {
        use crate::domain::stt::wav;

        let (port, _token, _engine_id, _instance_id) = start_fake_funasr_server().await;

        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(5))
            .build()
            .unwrap();

        let samples = vec![0.0f32; 1600];
        let wav_bytes = wav::pcm_to_wav(&samples, 16000, 1);
        let url = format!("http://127.0.0.1:{port}/v1/audio/transcriptions");

        let result = wav::transcribe_with_token(
            &client,
            &url,
            Some("wrong-token"),
            "iic/SenseVoiceSmall",
            &wav_bytes,
        )
        .await;

        assert!(result.is_err(), "错 token 的转录请求应失败");
    }
}
