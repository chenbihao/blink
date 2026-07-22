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
    /// 云端 STT 配置（0.12 重构:引用 AIConfig 供应商+模型）
    ///
    /// 优先使用此字段。若为 None 但 `cloud_provider` 有值（老配置），
    /// 运行时尝试自动迁移。
    #[serde(default)]
    pub cloud: Option<SttCloudConfig>,

    /// 旧云端 STT 供应商配置（0.12 前结构,保留向后兼容）
    ///
    /// 老配置反序列化后此字段有值。运行时若 `cloud` 为 None 但此字段有值,
    /// 尝试在 AIConfig 中找匹配的 provider 自动迁移。
    /// 迁移完成后（用户下次保存配置）此字段被丢弃。
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

    // ── 已废弃字段（反序列化时忽略，不报错）──
    /// 旧 `streaming: bool` 字段，已由 `streaming_mode` 替代。
    /// 保留仅为反序列化兼容，不实际使用。
    #[serde(default)]
    #[allow(dead_code)]
    pub streaming: bool,
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

/// 云端 STT 供应商配置（旧结构,0.12 前独立配置 kind/base_url/model_id）。
///
/// **0.12 重构**:改为引用 AIConfig 供应商（见 `SttCloudConfig`）。
/// 此结构保留仅为反序列化兼容——老配置反序列化后,运行时尝试在 AIConfig 中
/// 找匹配的 provider 自动迁移到 `SttCloudConfig`。
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

/// 云端 STT 配置（0.12 §2.7 重构:引用 AIConfig 供应商+模型）。
///
/// 用户在 AI 供应商页配好 OpenAI(含 whisper 模型,capabilities 含 Stt)后,
/// STT 设置页只需从下拉框选「供应商 → 模型」,不再重复填 kind/base_url/key。
///
/// 密钥、base_url、kind 全部从 AIConfig::ProviderEntry 继承——
/// 一个 OpenAI key 同时用于 chat(GPT-4)和 STT(whisper),用户只配一次。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SttCloudConfig {
    /// 引用 AIConfig::providers 中的 provider_id
    pub provider_id: String,
    /// 引用该 provider 下的 model_id（该模型的 capabilities 应含 Stt）
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
    /// 热词列表（英文逗号分隔，每项格式「词 权重」），存为 hotwords.txt 传给 FunASR
    /// 提升专有名词识别率
    #[serde(default)]
    pub hotwords: Option<String>,
    /// ITN 逆文本归一化（"二零二四年" → "2024年"），默认 true
    #[serde(default = "default_use_itn")]
    pub use_itn: bool,
    /// VAD 切句参数（伪流式模式生效）
    #[serde(default)]
    pub vad: VadConfig,

    // ── 已废弃字段（反序列化时忽略，不报错）──
    /// 旧 `streaming_model` 字段，真流式已移除，保留仅为反序列化兼容。
    #[serde(default)]
    #[allow(dead_code)]
    pub streaming_model: Option<String>,
}

/// VAD 切句参数（伪流式模式生效）。
///
/// 控制伪流式引擎何时判定"用户停顿了"从而触发句尾定稿。
/// 离麦克风较远或环境噪声较高时，可适当调低 `silence_threshold`。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VadConfig {
    /// RMS 低于此值视为静默（默认 0.005，约 -46dB）。
    ///
    /// 离麦克风远时声音能量低，可能被误判为静默导致一直灰色不定稿——
    /// 此时调低此值（如 0.002）。嘈杂环境调高（如 0.01）避免噪声触发。
    #[serde(default = "default_vad_silence_threshold")]
    pub silence_threshold: f64,
    /// 静默持续多久判定句尾（默认 300ms）。
    ///
    /// 说话连贯停顿短时可调低（如 200ms）加快定稿；慢速思考调高（如 500ms）。
    #[serde(default = "default_vad_min_silence_ms")]
    pub min_silence_ms: u32,
    /// 最小句子长度：短于此值不切句（默认 800ms）。
    ///
    /// 避免咳嗽、短暂噪声等触发误切。调低可让短句也定稿，但误切风险增大。
    #[serde(default = "default_vad_min_sentence_ms")]
    pub min_sentence_ms: u32,
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

fn default_vad_silence_threshold() -> f64 {
    0.005
}

fn default_vad_min_silence_ms() -> u32 {
    300
}

fn default_vad_min_sentence_ms() -> u32 {
    800
}

fn default_streaming_mode() -> StreamingMode {
    StreamingMode::Pseudo
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
            vad: VadConfig::default(),
            streaming_model: None,
        }
    }
}

impl Default for VadConfig {
    fn default() -> Self {
        Self {
            silence_threshold: default_vad_silence_threshold(),
            min_silence_ms: default_vad_min_silence_ms(),
            min_sentence_ms: default_vad_min_sentence_ms(),
        }
    }
}

impl Default for SttConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            mode: SttMode::Cloud,
            cloud: None,
            cloud_provider: None,
            local_engine: LocalEngineConfig::default(),
            local_model_id: None,
            model_dir: None,
            audio_device_id: None,
            streaming_mode: default_streaming_mode(),
            streaming: false,
        }
    }
}

impl SttConfig {
    /// 获取有效的云端 STT 配置（0.12 §2.7）。
    ///
    /// 优先返回 `cloud`（新结构）。若 `cloud` 为 None 但 `cloud_provider` 有值（老配置）,
    /// 尝试在 AIConfig 中找匹配的 provider 自动迁移。
    ///
    /// **迁移逻辑**：按 `cloud_provider.kind` 对应的 `ProviderKind` + `base_url` 匹配
    /// AIConfig 中的 provider。找到则返回临时构造的 `SttCloudConfig`。
    ///
    /// 返回 `(SttCloudConfig, migration_needed)`——`migration_needed=true` 表示
    /// 老配置仍在用,设置页保存时应写回 `cloud` 字段并清空 `cloud_provider`。
    pub fn effective_cloud(&self, ai_config: &crate::app::ai_config::AIConfig) -> Option<(SttCloudConfig, bool)> {
        // 1. 新字段有值 → 直接用
        if let Some(cloud) = &self.cloud {
            return Some((cloud.clone(), false));
        }

        // 2. 老字段有值 → 尝试迁移
        let old = self.cloud_provider.as_ref()?;
        let old_kind_str = old.kind.as_str();

        // 旧 kind 字符串 → ProviderKind 映射
        let target_kind = match old_kind_str {
            "openai" | "groq" | "azure" | "huggingface" | "mistral" | "openrouter" => {
                crate::app::ai_config::ProviderKind::OpenAICompatible
            }
            "anthropic" => crate::app::ai_config::ProviderKind::AnthropicMessages,
            "gemini" => crate::app::ai_config::ProviderKind::GeminiGenerateContent,
            _ => {
                tracing::warn!(kind = %old_kind_str, "STT 云端配置迁移:未知 kind,无法自动匹配");
                return None;
            }
        };

        // 在 AIConfig 中找匹配的 provider（kind + base_url）
        for provider in &ai_config.providers {
            if provider.kind != target_kind {
                continue;
            }
            // base_url 匹配：old.base_url 与 provider.base_url 都为 None 或都为 Some 且相等
            let base_matches = match (&old.base_url, &provider.base_url) {
                (None, None) => true,
                (Some(a), Some(b)) => a.trim_end_matches('/') == b.trim_end_matches('/'),
                _ => false,
            };
            if !base_matches {
                continue;
            }
            // 检查 provider 是否有匹配 model_id 的模型
            if provider.models.iter().any(|m| m.id == old.model_id) {
                tracing::info!(
                    provider_id = %provider.id,
                    model_id = %old.model_id,
                    "STT 云端配置自动迁移:找到匹配的 AIConfig 供应商"
                );
                return Some((
                    SttCloudConfig {
                        provider_id: provider.id.clone(),
                        model_id: old.model_id.clone(),
                    },
                    true, // 需要迁移
                ));
            }
        }

        tracing::warn!(
            kind = %old_kind_str,
            model_id = %old.model_id,
            "STT 云端配置迁移:未在 AIConfig 中找到匹配的供应商,需手动迁移"
        );
        None
    }

    /// 云端 STT 是否已配置（`cloud` 或 `cloud_provider` 任一有值）。
    pub fn is_cloud_configured(&self) -> bool {
        self.cloud.is_some() || self.cloud_provider.is_some()
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
        assert_eq!(cfg.local_engine.vad.silence_threshold, 0.005);
        assert_eq!(cfg.local_engine.vad.min_silence_ms, 300);
        assert_eq!(cfg.local_engine.vad.min_sentence_ms, 800);
    }

    #[test]
    fn round_trip_through_json_preserves_all_fields() {
        let original = SttConfig {
            enabled: true,
            mode: SttMode::Local,
            cloud: None,
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
                hotwords: Some("美团 100, 快手 80".into()),
                use_itn: false,
                vad: VadConfig {
                    silence_threshold: 0.003,
                    min_silence_ms: 200,
                    min_sentence_ms: 600,
                },
                streaming_model: None,
            },
            local_model_id: Some("sensevoice-small".into()),
            model_dir: None,
            audio_device_id: Some("麦克风 (Realtek Audio)".into()),
            streaming_mode: StreamingMode::Off,
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
            Some("美团 100, 快手 80")
        );
        assert!(!restored.local_engine.use_itn);
        assert_eq!(restored.local_engine.vad.silence_threshold, 0.003);
        assert_eq!(restored.local_engine.vad.min_silence_ms, 200);
        assert_eq!(restored.local_engine.vad.min_sentence_ms, 600);
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

    // ── 0.12 §2.7: SttCloudConfig 迁移测试 ────────────────────────────────

    #[test]
    fn stt_cloud_config_serializes() {
        let c = SttCloudConfig {
            provider_id: "p1".into(),
            model_id: "whisper-large-v3".into(),
        };
        let s = serde_json::to_string(&c).unwrap();
        let c2: SttCloudConfig = serde_json::from_str(&s).unwrap();
        assert_eq!(c, c2);
    }

    #[test]
    fn old_config_with_cloud_provider_deserializes() {
        // 老配置有 cloud_provider 但没有 cloud → 反序列化成功,cloud 为 None
        let json = r#"{"enabled":true,"cloud_provider":{"kind":"openai","base_url":"https://api.openai.com/v1","model_id":"whisper-large-v3"}}"#;
        let cfg: SttConfig = serde_json::from_str(json).unwrap();
        assert!(cfg.cloud.is_none());
        assert!(cfg.cloud_provider.is_some());
        assert_eq!(cfg.cloud_provider.as_ref().unwrap().kind, "openai");
    }

    #[test]
    fn new_config_with_cloud_field_deserializes() {
        let json = r#"{"enabled":true,"cloud":{"provider_id":"p1","model_id":"whisper-large-v3"}}"#;
        let cfg: SttConfig = serde_json::from_str(json).unwrap();
        assert!(cfg.cloud.is_some());
        assert!(cfg.cloud_provider.is_none());
        assert_eq!(cfg.cloud.as_ref().unwrap().provider_id, "p1");
    }

    #[test]
    fn effective_cloud_prefers_new_field() {
        // cloud 有值时,不检查 cloud_provider
        let cfg = SttConfig {
            cloud: Some(SttCloudConfig {
                provider_id: "new-p".into(),
                model_id: "new-m".into(),
            }),
            cloud_provider: Some(SttCloudProvider {
                kind: "openai".into(),
                base_url: None,
                model_id: "old-m".into(),
            }),
            ..Default::default()
        };
        let ai_config = crate::app::ai_config::AIConfig::default();
        let (cloud, migration_needed) = cfg.effective_cloud(&ai_config).unwrap();
        assert_eq!(cloud.provider_id, "new-p");
        assert!(!migration_needed);
    }

    #[test]
    fn effective_cloud_migrates_old_config() {
        // cloud 为 None,cloud_provider 有值 → 在 AIConfig 中找匹配
        let cfg = SttConfig {
            cloud: None,
            cloud_provider: Some(SttCloudProvider {
                kind: "openai".into(),
                base_url: Some("https://api.openai.com/v1".into()),
                model_id: "whisper-1".into(),
            }),
            ..Default::default()
        };
        // AIConfig 中有匹配的 provider
        let ai_config = crate::app::ai_config::AIConfig {
            providers: vec![crate::app::ai_config::ProviderEntry {
                id: "ai-p1".into(),
                display_name: "OpenAI".into(),
                kind: crate::app::ai_config::ProviderKind::OpenAICompatible,
                base_url: Some("https://api.openai.com/v1".into()),
                secret_ref: "blink/ai-p1/key".into(),
                models: vec![crate::app::ai_config::ModelEntry {
                    id: "whisper-1".into(),
                    display_name: "Whisper".into(),
                    enabled: true,
                    context_window: None,
                    input_price_per_million: None,
                    output_price_per_million: None,
                    temperature: None,
                    max_tokens: None,
                    custom_parameters: Vec::new(),
                    capabilities: vec![crate::app::ai_config::ModelCapability::Stt],
                }],
                created_at: 0,
            }],
            ..Default::default()
        };
        let (cloud, migration_needed) = cfg.effective_cloud(&ai_config).unwrap();
        assert_eq!(cloud.provider_id, "ai-p1");
        assert_eq!(cloud.model_id, "whisper-1");
        assert!(migration_needed, "老配置应标记为需要迁移");
    }

    #[test]
    fn effective_cloud_returns_none_when_no_match() {
        // cloud 为 None,cloud_provider 有值,但 AIConfig 中无匹配
        let cfg = SttConfig {
            cloud: None,
            cloud_provider: Some(SttCloudProvider {
                kind: "openai".into(),
                base_url: None,
                model_id: "whisper-1".into(),
            }),
            ..Default::default()
        };
        let ai_config = crate::app::ai_config::AIConfig::default();
        assert!(cfg.effective_cloud(&ai_config).is_none());
    }

    #[test]
    fn is_cloud_configured_checks_both_fields() {
        let cfg1 = SttConfig {
            cloud: Some(SttCloudConfig {
                provider_id: "p".into(),
                model_id: "m".into(),
            }),
            ..Default::default()
        };
        assert!(cfg1.is_cloud_configured());

        let cfg2 = SttConfig {
            cloud_provider: Some(SttCloudProvider {
                kind: "openai".into(),
                base_url: None,
                model_id: "m".into(),
            }),
            ..Default::default()
        };
        assert!(cfg2.is_cloud_configured());

        let cfg3 = SttConfig::default();
        assert!(!cfg3.is_cloud_configured());
    }
}
