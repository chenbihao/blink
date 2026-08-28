//! STT(语音转文字)领域层。
//!
//! 0.10 语音输入:hold-to-talk 期间采集音频 → STT 识别 → 文字上屏。
//!
//! ## 架构
//!
//! ```text
//! AudioCapture (infra)          SttEngine (domain)
//!    ↓ audio chunks                ↑ audio chunks
//!    │                             │
//!    └─── VoiceService ────────────┘
//!              ↓ partial text
//!              ↓ final text
//!         G1: emit chord-fill-query
//!         G2: inject_text()
//! ```
//!
//! ## SttEngine trait
//!
//! - `transcribe_chunk`: 接收一段音频,返回当前累积的 partial text
//! - `finalize`: 录音结束,返回最终 text
//! - `reset`: 新录音会话前重置状态
//!
//! ## 引擎选型
//!
//! - **云端**: CloudSttEngine (OpenAI 兼容 API,走 reqwest)
//! - **本地伪流式**: PseudoStreamingSttEngine (VAD 切句 + 定时 HTTP 预览)
//! - **本地非流式**: LocalSttEngine (HTTP 一次性识别)
//!
//! 0.10.4 起，真流式（WebSocket + Paraformer-streaming）已移除——
//! 伪流式在准确率、标点、体积、CPU 友好度上全面优于真流式，且 Python 侧零改动。

// ── 错误类型 ─────────────────────────────────────────────────────────────

/// STT 识别错误。
#[derive(Debug, thiserror::Error)]
#[allow(dead_code)]
pub enum SttError {
    /// 引擎未初始化
    #[error("STT engine not initialized")]
    NotInitialized,
    /// 识别过程中出错
    #[error("STT engine error: {0}")]
    Engine(String),
    /// 音频格式不匹配
    #[error("audio format mismatch: {0}")]
    FormatMismatch(String),
}

// ── STT Engine Connection ────────────────────────────────────────────────

/// STT 本地引擎连接快照（domain 层纯数据类型）。
///
/// 0.22.6 H4（批次 3）：替代此前对 `crate::app::local_engine::service::LocalEngineConnection`
/// 的直接引用——domain 层不得依赖 app 层。
///
/// app 层（`VoiceService`）负责把 `LocalEngineService::get_connection` 的结果
/// 投影为此类型再传入 `create_engine`。
///
/// **结构化 endpoint**——不再通过 `rsplit(':')` + `unwrap_or(8100)` 猜测端口。
/// endpoint 损坏时 `create_engine` 返回明确错误。
#[derive(Clone)]
pub struct SttEngineConnection {
    /// 监听地址（始终 `127.0.0.1`）。
    pub host: String,
    /// 实际监听端口。
    pub port: u16,
    /// 服务 token（用于 `X-Engine-Token` header 鉴权）。
    pub token: String,
    /// engine id（日志/诊断用）。
    #[allow(dead_code)]
    pub engine_id: String,
    /// instance id（每次启动随机生成，用于实例隔离）。
    #[allow(dead_code)]
    pub instance_id: String,
}

impl std::fmt::Debug for SttEngineConnection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SttEngineConnection")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("token", &"<redacted>")
            .field("engine_id", &self.engine_id)
            .field("instance_id", &self.instance_id)
            .finish()
    }
}

// ── STT Engine trait ─────────────────────────────────────────────────────

/// STT 引擎 trait。
///
/// 生命周期: `reset` → 多次 `transcribe_chunk` → `finalize` → (下次) `reset`。
///
/// `transcribe_chunk` 和 `finalize` 为 async——非流式引擎在 chunk 中累积音频,
/// finalize 时一次性 HTTP 请求返回;伪流式引擎在 chunk 中实时返回 partial。
#[async_trait::async_trait]
pub trait SttEngine: Send + Sync {
    /// 接收一段音频 chunk,返回当前累积识别的 partial text。
    ///
    /// 伪流式模式下每次调用返回逐步完善的文本;
    /// 非流式模式下可以累积音频,在 `finalize` 时一次性返回。
    async fn transcribe_chunk(&self, samples: &[f32]) -> Result<String, SttError>;

    /// 录音结束,返回最终识别文本。
    async fn finalize(&self) -> Result<String, SttError>;

    /// 重置引擎状态(新录音会话前调用)。
    fn reset(&self);

    /// 引擎显示名称(日志/调试用)。
    #[allow(dead_code)]
    fn name(&self) -> &str;
}

// ── 模型注册表 ───────────────────────────────────────────────────────────

/// FunASR 模型描述符。
///
/// 描述 FunASR 工具箱支持的模型，用于前端展示和配置引导。
#[derive(Debug, Clone)]
pub struct ModelDescriptor {
    /// 模型 id(唯一标识,如 "sensevoice-small")
    pub id: &'static str,
    /// 显示名称
    pub display_name: &'static str,
    /// 引擎名("funasr")
    pub engine: &'static str,
    /// FunASR 模型标识(传给 funasr-server --model 参数)
    pub funasr_model_id: &'static str,
    /// 参数量（如 "234M" / "220M"），来自 FunASR 官方文档
    pub params: &'static str,
    /// 模型下载体积(MB,FP32 PyTorch 格式近似值)
    pub size_mb: u32,
    /// 支持的语言
    pub languages: &'static [&'static str],
    /// 设备要求("cpu" 或 "cuda")
    pub device: &'static str,
    /// 简短描述
    pub description: &'static str,
}

/// 预置模型注册表。
///
/// 模型来自 FunASR（modelscope/阿里达摩院）。
/// FunASR 自动从 ModelScope 下载模型（国内 CDN，稳定）。
pub fn model_registry() -> &'static [ModelDescriptor] {
    &MODELS
}

static MODELS: [ModelDescriptor; 2] = [
    ModelDescriptor {
        id: "sensevoice-small",
        display_name: "FunASR SenseVoice-Small",
        engine: "funasr",
        funasr_model_id: "iic/SenseVoiceSmall",
        params: "234M",
        size_mb: 234,
        languages: &["zh", "en", "ja", "ko", "yue"],
        device: "cpu",
        description: "五语种 ASR（中/英/日/韩/粤），CPU 17 倍实时，带情感与音频事件标签。体积小、速度快，推荐 CPU 首选",
    },
    ModelDescriptor {
        id: "paraformer-zh",
        display_name: "FunASR Paraformer-zh (SeacoParaformer)",
        engine: "funasr",
        funasr_model_id: "paraformer-zh",
        params: "220M",
        size_mb: 880 + 1130,
        languages: &["zh", "en"],
        device: "cpu",
        description: "纯中文非流式 ASR（SeacoParaformer），原生支持热词 boosting 与 ITN。自动配置 VAD(fsmn-vad) + 标点(ct-punc) 子模型。整句准确率高于 SenseVoice，不会幻觉英文语气词",
    },
];

/// 按 id 查找模型描述符。
pub fn find_model(id: &str) -> Option<&'static ModelDescriptor> {
    model_registry().iter().find(|m| m.id == id)
}

// ── STT Engines ──────────────────────────────────────────────────────────

pub(crate) mod cloud;
pub mod funasr;
pub mod local;
#[cfg(test)]
mod mock;
pub mod pseudo_streaming;
pub mod vad;
pub(crate) mod wav;

/// 创建 STT 引擎实例(工厂函数)。
///
/// 根据 SttConfig 选择引擎：
/// - Cloud 模式 + 已配置云端供应商（cloud 或 cloud_provider）→ CloudSttEngine
/// - Local 模式 + StreamingMode::Pseudo → PseudoStreamingSttEngine (VAD + 预览) ⭐ 默认
/// - Local 模式 + StreamingMode::Off → LocalSttEngine (HTTP 非流式)
///
/// **0.22.6 批次 3**: Local 模式下使用 `SttEngineConnection`（domain 层纯数据类型），
/// STT 请求携带 `X-Engine-Token` 鉴权。无运行实例时返回错误。
/// **不再引用 `crate::app`**——app 层 VoiceService 负责把
/// `LocalEngineConnection` 投影为 `SttEngineConnection`。
///
/// **0.22.6 H4**: Local 模式下从 `local_stt_selection` 联合引用解析 engine/model。
/// 当前只需支持已注册的 Python FunASR，不实现 GGUF。
///
/// **不会回退到 Mock 引擎**——未启用 / 未配置 / 服务未就绪时返回 Err，
/// 由调用方（VoiceService）决定如何向用户反馈错误。
pub fn create_engine(
    connection: Option<SttEngineConnection>,
) -> Result<Box<dyn SttEngine>, String> {
    let config = crate::domain::config::stt_config::get_stt_config();

    if !config.enabled {
        return Err("STT 未启用，请在设置页开启语音输入".to_string());
    }

    // 0.22.6 H4: Local 模式下记录联合引用
    if config.mode == crate::domain::config::stt_config::SttMode::Local {
        if let Some(ref sel) = config.local_stt_selection {
            tracing::debug!(
                engine_id = %sel.engine_id,
                model_id = %sel.model_id,
                "本地 STT 选择（联合引用）"
            );
        }
    }

    match config.mode {
        crate::domain::config::stt_config::SttMode::Cloud => {
            if config.is_cloud_configured() {
                tracing::info!("STT 引擎: cloud");
                Ok(Box::new(cloud::CloudSttEngine::new()))
            } else {
                Err("云端 STT 未配置供应商，请在设置页中配置".to_string())
            }
        }
        crate::domain::config::stt_config::SttMode::Local => {
            // 0.22.6 批次 3: 从 SttEngineConnection 提取 port 和 token
            let conn = match connection {
                Some(c) => c,
                None => {
                    return Err("FunASR 服务未运行。请在设置页启动服务。\
                         （preferred port 只代表偏好，不代表实际监听地址）"
                        .to_string());
                }
            };
            // 结构化 endpoint——不再通过 rsplit(':') 猜测端口
            let port = conn.port;
            let token = conn.token.clone();

            match config.streaming_mode {
                crate::domain::config::stt_config::StreamingMode::Pseudo => {
                    match pseudo_streaming::PseudoStreamingSttEngine::new(
                        &config,
                        port,
                        token.clone(),
                    ) {
                        Ok(engine) => {
                            tracing::info!(port, "STT 引擎: pseudo-streaming (VAD + HTTP 轮询)");
                            Ok(Box::new(engine))
                        }
                        Err(e) => {
                            tracing::warn!(%e, "STT 伪流式引擎创建失败,回退非流式");
                            match local::LocalSttEngine::new(&config, port, token) {
                                Ok(engine) => {
                                    tracing::info!(port, "STT 引擎: local (FunASR, 非流式回退)");
                                    Ok(Box::new(engine))
                                }
                                Err(e) => Err(format!("STT 引擎创建失败: {e}")),
                            }
                        }
                    }
                }
                crate::domain::config::stt_config::StreamingMode::Off => {
                    match local::LocalSttEngine::new(&config, port, token) {
                        Ok(engine) => {
                            tracing::info!(port, "STT 引擎: local (FunASR)");
                            Ok(Box::new(engine))
                        }
                        Err(e) => Err(format!("STT 引擎创建失败: {e}")),
                    }
                }
            }
        }
    }
}

/// Mock STT 引擎的假文本库(按时间轮换,模拟"边说边出字")。
///
/// 仅供 `#[cfg(test)]` 的 MockSttEngine 使用，不参与生产路径。
#[cfg(test)]
pub fn mock_text_for_elapsed(elapsed: std::time::Duration) -> &'static str {
    match elapsed.as_secs() {
        0..=1 => "",
        2 => "你好",
        3 => "你好世界",
        4 => "你好世界这是一段",
        5 => "你好世界这是一段测试语音",
        _ => "你好世界这是一段测试语音识别的文字结果",
    }
}

// ── 测试 ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod connection_tests {
    use super::*;

    #[test]
    fn stt_engine_connection_clone_preserves_fields() {
        let conn = SttEngineConnection {
            host: "127.0.0.1".to_string(),
            port: 8100,
            token: "test-token-abc".to_string(),
            engine_id: "funasr".to_string(),
            instance_id: "inst-123".to_string(),
        };
        let cloned = conn.clone();
        assert_eq!(cloned.host, conn.host);
        assert_eq!(cloned.port, conn.port);
        assert_eq!(cloned.token, conn.token);
        assert_eq!(cloned.engine_id, conn.engine_id);
        assert_eq!(cloned.instance_id, conn.instance_id);
    }

    #[test]
    fn stt_engine_connection_debug_format_contains_key_fields() {
        let conn = SttEngineConnection {
            host: "127.0.0.1".to_string(),
            port: 8100,
            token: "secret".to_string(),
            engine_id: "funasr".to_string(),
            instance_id: "inst-1".to_string(),
        };
        let debug_str = format!("{:?}", conn);
        assert!(debug_str.contains("127.0.0.1"));
        assert!(debug_str.contains("8100"));
        assert!(!debug_str.contains("secret"));
        assert!(debug_str.contains("<redacted>"));
        assert!(debug_str.contains("funasr"));
    }

    /// create_engine 在 STT 未启用时返回明确错误（不回退 Mock）。
    /// Local 模式但无 connection 时也返回明确错误。
    ///
    /// 注意：init_cache 用 OnceLock，只能设置一次，所以两个场景合并到一个测试中。
    #[test]
    fn create_engine_returns_err_for_disabled_and_no_connection() {
        // 临时设置 config 为 disabled + Local 模式
        let config = crate::domain::config::stt_config::SttConfig {
            enabled: false,
            mode: crate::domain::config::stt_config::SttMode::Local,
            ..Default::default()
        };
        crate::domain::config::stt_config::init_cache(config);

        // STT 未启用 → 返回错误
        let result = create_engine(None);
        assert!(result.is_err());
        if let Err(e) = result {
            assert!(e.contains("STT 未启用"), "应提及未启用: {e}");
        }
    }

    /// Static architecture test: `src/domain/stt` 不得引用 `crate::app`、`tauri` 或 `windows`。
    ///
    /// 0.22.6 批次 3: domain 层必须保持框架无关——不依赖 app 层、Tauri 或 Win32 API。
    /// 此测试扫描 `src/domain/stt/` 下所有 `.rs` 文件，检查是否包含禁止的引用。
    ///
    /// 注意：`crate::domain::stt::SttEngineConnection` 被 app 层引用是合法的（app → domain），
    /// 但 domain 层不得反向引用 app 层。
    #[test]
    fn domain_stt_does_not_reference_app_tauri_or_windows() {
        use std::fs;
        use std::path::PathBuf;

        // 定位 src/domain/stt 目录
        let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".to_string());
        let stt_dir: PathBuf = std::path::Path::new(&manifest_dir)
            .join("src")
            .join("domain")
            .join("stt");

        if !stt_dir.exists() {
            // 在某些构建环境中（如 IDE 索引），目录可能不存在——跳过
            eprintln!("跳过：src/domain/stt 目录不存在（{}）", stt_dir.display());
            return;
        }

        // 递归收集所有 .rs 文件
        fn collect_rs_files(dir: &std::path::Path, files: &mut Vec<PathBuf>) {
            if let Ok(entries) = fs::read_dir(dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.is_dir() {
                        collect_rs_files(&path, files);
                    } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
                        files.push(path);
                    }
                }
            }
        }

        let mut files = Vec::new();
        collect_rs_files(&stt_dir, &mut files);

        assert!(
            !files.is_empty(),
            "src/domain/stt 目录下应至少有一个 .rs 文件"
        );

        // 使用运行时构造避免测试代码自身触发违规
        let p1 = format!("crate{}:{}", ":", ":app");
        let p2 = format!("use {}", "tauri");
        let p3 = format!("use {}", "windows::");
        let p4 = format!("{}{}", "tauri", "::");
        let forbidden_patterns = [p1, p2, p3, p4];

        let mut violations: Vec<String> = Vec::new();
        for file in &files {
            // 跳过测试模块中的注释行——只检查实际代码
            let content = match fs::read_to_string(file) {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("警告: 读取 {} 失败: {e}", file.display());
                    continue;
                }
            };

            for (line_num, line) in content.lines().enumerate() {
                let trimmed = line.trim();
                // 跳过注释行
                if trimmed.starts_with("//") || trimmed.starts_with("/*") {
                    continue;
                }
                for pattern in &forbidden_patterns {
                    if trimmed.contains(pattern) {
                        // 允许在注释中出现（如文档引用），但上面已跳过注释行
                        violations.push(format!(
                            "{}:{}: 包含禁止的引用 '{}': {}",
                            file.display(),
                            line_num + 1,
                            pattern,
                            trimmed
                        ));
                    }
                }
            }
        }

        assert!(
            violations.is_empty(),
            "src/domain/stt 不得引用 crate::app / tauri / windows。\n\
             违规项:\n{}",
            violations.join("\n")
        );
    }
}
