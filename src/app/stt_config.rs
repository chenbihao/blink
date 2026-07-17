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
//! 本地 STT 使用 blink_stt_server.py（自定义统一服务，兼容官方 funasr-server API）。
//! 配置项：
//! - `server_port`: 监听端口（默认 8000）
//! - `funasr_model`: 模型标识（如 "iic/SenseVoiceSmall" / "paraformer-zh"）
//! - `device`: 推理设备（"cpu" 或 "cuda"）
//! - `hotwords`: 热词列表（每行 "词 权重"）
//! - `use_itn`: ITN 逆文本归一化

use serde::{Deserialize, Serialize};

/// 流式模式选择。
///
/// 伪流式（默认）：VAD 切句定稿 + 累积预览，在非自回归模型上实现"边说边出字"体感。
/// 非流式：hold → release → 一次性识别。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum StreamingMode {
    /// 伪流式：VAD 切句定稿 + 累积预览 ⭐ 默认
    #[default]
    Pseudo,
    /// 非流式：hold → release → 一次性识别
    Off,
}

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
    /// 本地引擎配置(blink_stt_server)
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
    /// 流式模式（默认伪流式）
    ///
    /// **兼容性**：旧配置中的 `streaming: bool` 和 `streaming_mode: Option<StreamingMode>`
    /// 字段已移除。反序列化时通过自定义 `deserialize_streaming_mode` 兼容旧配置：
    /// - 旧 `streaming_mode = "true"` → `Pseudo`（真流式已废弃，统一降级为伪流式）
    /// - 旧 `streaming_mode = "pseudo"` → `Pseudo`
    /// - 旧 `streaming_mode = "off"` / `streaming = false` → `Off`
    /// - 旧 `streaming = true`（无 streaming_mode）→ `Pseudo`
    #[serde(default = "default_streaming_mode", deserialize_with = "deserialize_streaming_mode")]
    pub streaming_mode: StreamingMode,

    // ── 0.10.3 新增：文本注入方式 ──
    /// G2 文本注入方式（默认 SendInput Unicode，不碰剪贴板）
    #[serde(default = "default_inject_method")]
    pub inject_method: InjectMethod,

    // ── 已废弃字段（反序列化时忽略，不报错）──
    /// 旧 `streaming: bool` 字段，已由 `streaming_mode` 替代。
    /// 保留仅为反序列化兼容，不实际使用。
    #[serde(default)]
    #[allow(dead_code)]
    pub streaming: bool,
}

/// 文本注入方式（G2 语音输入法上屏）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum InjectMethod {
    /// Clipboard + Ctrl+V（0.10.1~0.10.2，兼容性最好但有剪贴板污染）
    Clipboard,
    /// SendInput Unicode 逐字符（0.10.3 默认，不碰剪贴板）
    #[default]
    SendInput,
}

impl<'de> Deserialize<'de> for InjectMethod {
    /// 兼容旧配置中的 "tsf" 值（TSF 已移除，降级为 SendInput）。
    ///
    /// 0.10.5 曾引入 imekit 做 TSF Composition 注入，实测发现 `ITfThreadMgr::GetFocus()`
    /// 是进程本地的——Blink 在自己进程创建的 TSF 管理器拿不到前台应用的编辑上下文，
    /// 跨进程时 TSF 路径静默失败，最终退化成 SendInput，无额外价值。已移除。
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        match s.as_str() {
            "clipboard" => Ok(InjectMethod::Clipboard),
            "sendinput" => Ok(InjectMethod::SendInput),
            // 兼容旧配置：TSF 已移除（跨进程不可用），降级为 SendInput
            "tsf" => {
                tracing::info!("旧配置 inject_method=tsf 已降级为 sendinput（TSF 跨进程不可用，已移除）");
                Ok(InjectMethod::SendInput)
            }
            other => {
                tracing::warn!(value = %other, "未知 inject_method 值，使用默认 SendInput");
                Ok(InjectMethod::SendInput)
            }
        }
    }
}

/// STT 模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum SttMode {
    /// 云端 STT(走 OpenAI 兼容 API)
    #[default]
    Cloud,
    /// 本地 STT(blink_stt_server)
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

/// 本地引擎配置(blink_stt_server)。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocalEngineConfig {
    /// 监听端口（默认 8000）
    #[serde(default = "default_server_port")]
    pub server_port: u16,
    /// 模型标识(传给 blink_stt_server --model 参数)
    /// 如 "iic/SenseVoiceSmall"（五语种 ASR，CPU 首选）
    /// 或 "paraformer-zh"（SeacoParaformer，原生支持热词）
    /// 注意：使用完整 ModelScope ID（含 `iic/` 前缀），短名在 FunASR 1.3.14 中解析会失败
    #[serde(default = "default_funasr_model", deserialize_with = "deserialize_funasr_model")]
    pub funasr_model: String,
    /// 推理设备: "cpu" 或 "cuda"
    #[serde(default = "default_device")]
    pub device: String,
    /// CPU 推理线程数(None = 自动)
    #[serde(default)]
    pub num_threads: Option<u32>,
    /// Blink 启动后自动启动服务（懒加载，延迟 3s）
    #[serde(default)]
    pub auto_start_server: bool,
    /// 热词列表（每行 "词 权重"），存为 hotwords.txt 传给 FunASR
    /// 提升专有名词识别率
    #[serde(default)]
    pub hotwords: Option<String>,
    /// ITN 逆文本归一化（"二零二四年" → "2024年"），默认 true
    #[serde(default = "default_use_itn")]
    pub use_itn: bool,

    // ── 已废弃字段（反序列化时忽略，不报错）──
    /// 旧 `streaming_model` 字段，真流式已移除，保留仅为反序列化兼容。
    #[serde(default)]
    #[allow(dead_code)]
    pub streaming_model: Option<String>,
}

fn default_server_port() -> u16 {
    8000
}

fn default_funasr_model() -> String {
    "iic/SenseVoiceSmall".to_string()
}

/// 反序列化时归一化旧配置中的模型名。
///
/// FunASR 1.3.14 的 AutoModel 短名解析在某些场景下会失效（ModelScope API
/// 返回 404），因此统一使用完整 ModelScope ID（含 `iic/` 前缀）。
/// 此函数将已知的旧名映射到正确的完整 ID。
fn deserialize_funasr_model<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<String, D::Error> {
    use serde::Deserialize;
    let raw = String::deserialize(deserializer)?;
    Ok(match raw.as_str() {
        // SenseVoice 短名需要显式映射，因为 FunASR name_maps_ms 中没有这些别名
        "sensevoice" | "SenseVoice" | "SenseVoiceSmall" => "iic/SenseVoiceSmall".to_string(),
        // paraformer-zh 短名不映射——FunASR 内部 name_maps_ms 会解析为
        // iic/speech_seaco_paraformer_large_asr_nat-zh-cn-16k-common-vocab8404-pytorch (SeacoParaformer)
        // 如果在这里映射为完整 ID，反而会绕过 FunASR 的正确解析
        // 兼容旧配置：曾经用过的错误完整 ID，归一化为短名让 FunASR 正确解析
        "iic/speech_paraformer-large_asr_nat-zh-cn-16k-common-vocab8404" => {
            "paraformer-zh".to_string()
        }
        // 旧真流式模型，已废弃——归一化为默认非流式模型
        "paraformer-zh-streaming"
        | "iic/speech_paraformer-large_asr_nat-zh-cn-16k-common-vocab8404-online" => {
            "iic/SenseVoiceSmall".to_string()
        }
        other => other.to_string(),
    })
}

fn default_device() -> String {
    "cpu".to_string()
}

fn default_use_itn() -> bool {
    true
}

fn default_streaming_mode() -> StreamingMode {
    StreamingMode::Pseudo
}

fn default_inject_method() -> InjectMethod {
    InjectMethod::SendInput
}

/// 反序列化 `streaming_mode` 字段，兼容旧配置。
///
/// 旧配置可能以以下形式出现：
/// - `"pseudo"` → `Pseudo`
/// - `"off"` → `Off`
/// - `"true"` → `Pseudo`（真流式已废弃，降级为伪流式）
/// - 缺失 → `Pseudo`（默认）
///
/// 注意：旧 `streaming: bool` 字段的迁移在 `deserialize_stt_config` 中处理。
fn deserialize_streaming_mode<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<StreamingMode, D::Error> {
    use serde::Deserialize;

    // 尝试反序列化为字符串或单元（缺失时的占位）
    let opt: Option<String> = Option::deserialize(deserializer)?;
    match opt.as_deref() {
        Some("off") => Ok(StreamingMode::Off),
        Some("true") | Some("pseudo") | None => Ok(StreamingMode::Pseudo),
        Some(other) => {
            tracing::warn!(value = %other, "未知 streaming_mode 值，使用默认 Pseudo");
            Ok(StreamingMode::Pseudo)
        }
    }
}

impl Default for LocalEngineConfig {
    fn default() -> Self {
        Self {
            server_port: default_server_port(),
            funasr_model: default_funasr_model(),
            device: default_device(),
            num_threads: None,
            auto_start_server: false,
            hotwords: None,
            use_itn: default_use_itn(),
            streaming_model: None,
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
            streaming_mode: default_streaming_mode(),
            inject_method: default_inject_method(),
            streaming: false,
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
        assert_eq!(cfg.streaming_mode, StreamingMode::Pseudo);
        assert_eq!(cfg.local_engine.server_port, 8000);
        assert_eq!(cfg.local_engine.funasr_model, "iic/SenseVoiceSmall");
        assert_eq!(cfg.local_engine.device, "cpu");
        assert!(cfg.local_engine.hotwords.is_none());
        assert!(cfg.local_engine.use_itn);
        assert_eq!(cfg.inject_method, InjectMethod::SendInput);
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
                funasr_model: "paraformer-zh".into(),
                device: "cuda".into(),
                num_threads: Some(4),
                auto_start_server: true,
                hotwords: Some("美团 100\n快手 80".into()),
                use_itn: false,
                streaming_model: None,
            },
            local_model_id: Some("sensevoice-small".into()),
            model_dir: None,
            audio_device_id: Some("麦克风 (Realtek Audio)".into()),
            streaming_mode: StreamingMode::Off,
            inject_method: InjectMethod::Clipboard,
            streaming: false,
        };
        let s = serde_json::to_string(&original).unwrap();
        let restored: SttConfig = serde_json::from_str(&s).unwrap();

        assert_eq!(restored.enabled, original.enabled);
        assert_eq!(restored.mode, SttMode::Local);
        assert_eq!(restored.cloud_provider.as_ref().unwrap().kind, "openai");
        assert_eq!(
            restored.cloud_provider.as_ref().unwrap().model_id,
            "whisper-large-v3"
        );
        assert_eq!(restored.local_engine.server_port, 9000);
        assert_eq!(restored.local_engine.funasr_model, "paraformer-zh");
        assert_eq!(restored.local_engine.device, "cuda");
        assert_eq!(restored.local_engine.num_threads, Some(4));
        assert_eq!(
            restored.local_engine.hotwords.as_deref(),
            Some("美团 100\n快手 80")
        );
        assert!(!restored.local_engine.use_itn);
        assert_eq!(restored.inject_method, InjectMethod::Clipboard);
        assert_eq!(restored.local_model_id.as_deref(), Some("sensevoice-small"));
        assert_eq!(restored.streaming_mode, StreamingMode::Off);
    }

    #[test]
    fn deserialize_from_partial_json_fills_defaults() {
        // 只 enabled = true,其余缺失 → 用 default 补
        let json = r#"{"enabled":true}"#;
        let cfg: SttConfig = serde_json::from_str(json).unwrap();
        assert!(cfg.enabled);
        assert_eq!(cfg.mode, SttMode::Cloud);
        assert_eq!(cfg.streaming_mode, StreamingMode::Pseudo);
        assert_eq!(cfg.inject_method, InjectMethod::SendInput);
        assert_eq!(cfg.local_engine.server_port, 8000);
        assert_eq!(cfg.local_engine.funasr_model, "iic/SenseVoiceSmall");
        assert!(cfg.local_engine.use_itn);
    }

    #[test]
    fn stt_mode_serializes_as_lowercase() {
        let s = serde_json::to_string(&SttMode::Local).unwrap();
        assert_eq!(s, r#""local""#);
        let s = serde_json::to_string(&SttMode::Cloud).unwrap();
        assert_eq!(s, r#""cloud""#);
    }

    #[test]
    fn inject_method_serializes_as_lowercase() {
        assert_eq!(
            serde_json::to_string(&InjectMethod::Clipboard).unwrap(),
            r#""clipboard""#
        );
        assert_eq!(
            serde_json::to_string(&InjectMethod::SendInput).unwrap(),
            r#""sendinput""#
        );
    }

    #[test]
    fn inject_method_deserializes_from_lowercase() {
        assert_eq!(
            serde_json::from_str::<InjectMethod>(r#""clipboard""#).unwrap(),
            InjectMethod::Clipboard
        );
        assert_eq!(
            serde_json::from_str::<InjectMethod>(r#""sendinput""#).unwrap(),
            InjectMethod::SendInput
        );
    }

    /// 验证旧配置中的 "tsf" 值反序列化为 SendInput（TSF 已移除）
    #[test]
    fn inject_method_deserializes_old_tsf_to_sendinput() {
        let method: InjectMethod = serde_json::from_str(r#""tsf""#).unwrap();
        assert_eq!(method, InjectMethod::SendInput);
    }

    /// 验证未知值反序列化为 SendInput
    #[test]
    fn inject_method_deserializes_unknown_to_sendinput() {
        let method: InjectMethod = serde_json::from_str(r#""unknown""#).unwrap();
        assert_eq!(method, InjectMethod::SendInput);
    }

    #[test]
    fn streaming_mode_serializes_as_lowercase() {
        assert_eq!(
            serde_json::to_string(&StreamingMode::Pseudo).unwrap(),
            r#""pseudo""#
        );
        assert_eq!(
            serde_json::to_string(&StreamingMode::Off).unwrap(),
            r#""off""#
        );
    }

    /// 验证旧配置（含 streaming_mode = "true"）反序列化为 Pseudo。
    #[test]
    fn deserialize_old_streaming_true_to_pseudo() {
        let json = r#"{"enabled":true,"streaming_mode":"true"}"#;
        let cfg: SttConfig = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.streaming_mode, StreamingMode::Pseudo);
    }

    /// 验证旧配置（含 streaming_mode = "pseudo"）反序列化为 Pseudo。
    #[test]
    fn deserialize_old_streaming_pseudo() {
        let json = r#"{"enabled":true,"streaming_mode":"pseudo"}"#;
        let cfg: SttConfig = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.streaming_mode, StreamingMode::Pseudo);
    }

    /// 验证旧配置（含 streaming_mode = "off"）反序列化为 Off。
    #[test]
    fn deserialize_old_streaming_off() {
        let json = r#"{"enabled":true,"streaming_mode":"off"}"#;
        let cfg: SttConfig = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.streaming_mode, StreamingMode::Off);
    }

    /// 验证旧配置中的 "sensevoice" 模型名被归一化为完整 ModelScope ID。
    #[test]
    fn deserialize_normalizes_old_sensevoice_model_name() {
        let json = r#"{"enabled":true,"mode":"local","local_engine":{"server_port":8000,"funasr_model":"sensevoice","device":"cpu"}}"#;
        let cfg: SttConfig = serde_json::from_str(json).unwrap();
        assert_eq!(
            cfg.local_engine.funasr_model, "iic/SenseVoiceSmall",
            "旧配置中的 'sensevoice' 应被归一化为 'iic/SenseVoiceSmall'"
        );
    }

    /// 验证 "paraformer-zh" 短名保持不变（FunASR 内部 name_maps_ms 解析为 SeacoParaformer）。
    #[test]
    fn deserialize_keeps_paraformer_zh_short_name() {
        let json = r#"{"enabled":true,"mode":"local","local_engine":{"server_port":8000,"funasr_model":"paraformer-zh","device":"cpu"}}"#;
        let cfg: SttConfig = serde_json::from_str(json).unwrap();
        assert_eq!(
            cfg.local_engine.funasr_model, "paraformer-zh",
            "短名 'paraformer-zh' 应保持不变，由 FunASR 内部 name_maps_ms 解析为 SeacoParaformer"
        );
    }

    /// 验证旧的错误完整 ID 被归一化为短名。
    #[test]
    fn deserialize_normalizes_old_wrong_full_id() {
        let json = r#"{"enabled":true,"mode":"local","local_engine":{"server_port":8000,"funasr_model":"iic/speech_paraformer-large_asr_nat-zh-cn-16k-common-vocab8404","device":"cpu"}}"#;
        let cfg: SttConfig = serde_json::from_str(json).unwrap();
        assert_eq!(
            cfg.local_engine.funasr_model, "paraformer-zh",
            "错误的完整 ID 应归一化为短名 'paraformer-zh'"
        );
    }

    /// 验证旧真流式模型名被归一化为默认非流式模型。
    #[test]
    fn deserialize_normalizes_old_streaming_model_name() {
        let json = r#"{"enabled":true,"mode":"local","local_engine":{"server_port":8000,"funasr_model":"paraformer-zh-streaming","device":"cpu"}}"#;
        let cfg: SttConfig = serde_json::from_str(json).unwrap();
        assert_eq!(
            cfg.local_engine.funasr_model, "iic/SenseVoiceSmall",
            "旧真流式模型 'paraformer-zh-streaming' 应归一化为 'iic/SenseVoiceSmall'"
        );
    }

    /// 验证旧配置中的 streaming_model 字段被忽略（不报错）。
    #[test]
    fn deserialize_old_config_with_streaming_model_field() {
        let json = r#"{"enabled":true,"local_engine":{"streaming_model":"paraformer-zh-streaming"}}"#;
        let cfg: SttConfig = serde_json::from_str(json).unwrap();
        assert!(cfg.enabled);
        // streaming_model 字段保留但已废弃，不再使用
    }
}
