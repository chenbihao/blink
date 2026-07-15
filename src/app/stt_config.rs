//! STT(语音转文字)配置分片——第 8 个 KV,key = `"stt:config"`。
//!
//! 与 AIConfig 同级(独立于 AppConfig 门面),独立 opt-in。
//! 老用户首次读拿到 `SttConfig::default()`,`enabled = false`,零副作用。
//!
//! ## 与 AIConfig 的关系
//!
//! STT 复用 AIConfig 的 secret 管理体系(Credential Manager)——云端 STT 的 API Key
//! 存在 CM 里,secret_ref 引用方式与 AIConfig::ProviderEntry 一致。
//! 但 STT 的供应商配置是独立结构(SttCloudProvider),不复用 ProviderEntry——
//! STT 不需要 tier/temperature 等 LLM 概念,只需要 kind + model_id + base_url。
//!
//! ## 本地 STT 配置
//!
//! 本地 STT 使用 FunASR Python 工具箱的 `funasr-server`。
//! 配置项：
//! - `server_port`: funasr-server 监听端口（默认 8000）
//! - `funasr_model`: FunASR 模型标识（如 "sensevoice"）
//! - `device`: 推理设备（"cpu" 或 "cuda"）

use serde::{Deserialize, Serialize};

/// STT 配置分片。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SttConfig {
    /// STT 总开关(默认关,opt-in)
    #[serde(default)]
    pub enabled: bool,

    /// STT 模式:云端 / 本地
    #[serde(default)]
    pub mode: SttMode,

    // ── 云端配置 ──────────────────────────────────────────────────

    /// 云端 STT 供应商配置(mode = Cloud 时生效)
    #[serde(default)]
    pub cloud_provider: Option<SttCloudProvider>,

    // ── 本地配置 ──────────────────────────────────────────────────

    /// 本地引擎配置(FunASR server)
    #[serde(default)]
    pub local_engine: LocalEngineConfig,

    /// 当前选用的本地模型 id(如 "sensevoice-small")
    /// 对应 `ModelDescriptor::id`(模型注册表在 `domain::stt::mod.rs`)
    #[serde(default)]
    pub local_model_id: Option<String>,

    /// 模型存储目录(保留字段，FunASR 自动管理模型路径)
    #[serde(default)]
    #[allow(dead_code)]
    pub model_dir: Option<String>,

    /// 音频输入设备 ID(None = 系统默认设备)
    /// 使用 cpal 设备名称作为 ID（WASAPI 枚举）
    #[serde(default)]
    pub audio_device_id: Option<String>,

    // ── 行为开关 ──────────────────────────────────────────────────

    /// 流式识别开关(默认开——边说边出字;关闭则松开后一次性识别)
    #[serde(default = "default_streaming")]
    pub streaming: bool,
}

/// STT 模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum SttMode {
    /// 云端 STT(走 OpenAI 兼容 API)
    #[default]
    Cloud,
    /// 本地 STT(FunASR server)
    Local,
}

/// 云端 STT 供应商配置。
///
/// **secret 管理**:API Key 不在此结构中——通过 `save_ai_secret` /
/// `has_ai_secret` 命令存取(复用 AIConfig 的 Credential Manager 体系),
/// secret_ref 用 `stt:{provider_kind}` 约定。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SttCloudProvider {
    /// 供应商种类:"openai" / "groq" / "azure" / "gemini" / "huggingface" /
    /// "mistral" / "openrouter"
    pub kind: String,
    /// 自定义 base_url(None = 供应商默认)
    #[serde(default)]
    pub base_url: Option<String>,
    /// 模型 id(如 "whisper-large-v3")
    pub model_id: String,
}

/// 本地引擎配置(FunASR server)。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocalEngineConfig {
    /// funasr-server 监听端口（默认 8000）
    #[serde(default = "default_server_port")]
    pub server_port: u16,
    /// FunASR 模型标识(传给 funasr-server --model 参数)
    /// 如 "sensevoice" / "paraformer"
    #[serde(default = "default_funasr_model")]
    pub funasr_model: String,
    /// 推理设备: "cpu" 或 "cuda"
    #[serde(default = "default_device")]
    pub device: String,
    /// CPU 推理线程数(None = 自动)
    #[serde(default)]
    pub num_threads: Option<u32>,
    /// Blink 启动后自动启动 funasr-server（懒加载，延迟 3s）
    #[serde(default)]
    pub auto_start_server: bool,
}

fn default_server_port() -> u16 {
    8000
}

fn default_funasr_model() -> String {
    "sensevoice".to_string()
}

fn default_device() -> String {
    "cpu".to_string()
}

fn default_streaming() -> bool {
    true
}

impl Default for LocalEngineConfig {
    fn default() -> Self {
        Self {
            server_port: default_server_port(),
            funasr_model: default_funasr_model(),
            device: default_device(),
            num_threads: None,
            auto_start_server: false,
        }
    }
}

impl Default for SttConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            mode: SttMode::Cloud,
            cloud_provider: None,
            local_engine: LocalEngineConfig::default(),
            local_model_id: None,
            model_dir: None,
            audio_device_id: None,
            streaming: default_streaming(),
        }
    }
}

// ── 内存缓存（供非 async 上下文读取）──────────────────────────────────────────

use std::sync::{OnceLock, RwLock};

static CONFIG_CACHE: OnceLock<RwLock<SttConfig>> = OnceLock::new();

/// 初始化配置缓存（main.rs 启动时调用）。
pub fn init_cache(config: SttConfig) {
    let _ = CONFIG_CACHE.set(RwLock::new(config));
}

/// 更新配置缓存（set_stt_config 命令调用后同步）。
pub fn update_cache(config: &SttConfig) {
    if let Some(lock) = CONFIG_CACHE.get() {
        if let Ok(mut guard) = lock.write() {
            *guard = config.clone();
        }
    }
}

/// 同步读取配置缓存（供 STT 引擎等非 async 上下文使用）。
/// 若缓存未初始化，返回 default（STT 关闭）。
pub fn get_stt_config() -> SttConfig {
    CONFIG_CACHE
        .get()
        .and_then(|lock| lock.read().ok().map(|g| g.clone()))
        .unwrap_or_default()
}

// ConfigKey impl 在 config.rs 中统一注册(避免重复定义)。

// ── 测试 ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_disabled() {
        let cfg = SttConfig::default();
        assert!(!cfg.enabled);
        assert_eq!(cfg.mode, SttMode::Cloud);
        assert!(cfg.cloud_provider.is_none());
        assert!(cfg.streaming);
        assert_eq!(cfg.local_engine.server_port, 8000);
        assert_eq!(cfg.local_engine.funasr_model, "sensevoice");
        assert_eq!(cfg.local_engine.device, "cpu");
    }

    #[test]
    fn round_trip_through_json_preserves_all_fields() {
        let original = SttConfig {
            enabled: true,
            mode: SttMode::Local,
            cloud_provider: Some(SttCloudProvider {
                kind: "openai".into(),
                base_url: Some("https://api.openai.com/v1".into()),
                model_id: "whisper-large-v3".into(),
            }),
            local_engine: LocalEngineConfig {
                server_port: 9000,
                funasr_model: "paraformer".into(),
                device: "cuda".into(),
                num_threads: Some(4),
            },
            local_model_id: Some("sensevoice-small".into()),
            model_dir: None,
            audio_device_id: Some("麦克风 (Realtek Audio)".into()),
            streaming: true,
        };
        let s = serde_json::to_string(&original).unwrap();
        let restored: SttConfig = serde_json::from_str(&s).unwrap();

        assert_eq!(restored.enabled, original.enabled);
        assert_eq!(restored.mode, SttMode::Local);
        assert_eq!(restored.cloud_provider.as_ref().unwrap().kind, "openai");
        assert_eq!(restored.cloud_provider.as_ref().unwrap().model_id, "whisper-large-v3");
        assert_eq!(restored.local_engine.server_port, 9000);
        assert_eq!(restored.local_engine.funasr_model, "paraformer");
        assert_eq!(restored.local_engine.device, "cuda");
        assert_eq!(restored.local_engine.num_threads, Some(4));
        assert_eq!(restored.local_model_id.as_deref(), Some("sensevoice-small"));
        assert!(restored.streaming);
    }

    #[test]
    fn deserialize_from_partial_json_fills_defaults() {
        // 只 enabled = true,其余缺失 → 用 default 补
        let json = r#"{"enabled":true}"#;
        let cfg: SttConfig = serde_json::from_str(json).unwrap();
        assert!(cfg.enabled);
        assert_eq!(cfg.mode, SttMode::Cloud);
        assert!(cfg.streaming);
        // local_engine 用 default
        assert_eq!(cfg.local_engine.server_port, 8000);
        assert_eq!(cfg.local_engine.funasr_model, "sensevoice");
    }

    #[test]
    fn stt_mode_serializes_as_lowercase() {
        let s = serde_json::to_string(&SttMode::Local).unwrap();
        assert_eq!(s, r#""local""#);
        let s = serde_json::to_string(&SttMode::Cloud).unwrap();
        assert_eq!(s, r#""cloud""#);
    }

    /// 验证旧的 onnxruntime_path 字段不再存在，反序列化不报错。
    #[test]
    fn deserialize_old_config_without_onnxruntime_field() {
        // 旧配置可能含 onnxruntime_path，新结构忽略它
        let json = r#"{"enabled":true,"mode":"local","local_engine":{"server_port":8000,"funasr_model":"sensevoice","device":"cpu"}}"#;
        let cfg: SttConfig = serde_json::from_str(json).unwrap();
        assert!(cfg.enabled);
        assert_eq!(cfg.mode, SttMode::Local);
        assert_eq!(cfg.local_engine.server_port, 8000);
    }
}
