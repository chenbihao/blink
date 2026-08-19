//! STT(语音转文字)配置分片——第 8 个 KV,key = `"stt:config"`。
//!
//! 与 AIConfig 同级(独立于 AppConfig 门面),独立 opt-in。
//! 老用户首次读拿到 `SttConfig::default()`,`enabled = false`,零副作用。
//!
//! ## 云端 STT 配置（独立模式）
//!
//! STT 云端配置完全独立于 AIConfig——用户在语音设置页直接配置 kind/base_url/model_id，
//! API Key 用 `stt:cloud` 前缀存在 Credential Manager 里，不与 AI 供应商共用。
//!
//! STT 和 Chat 是两种完全不同的用途（用户可能用 Groq 做 STT 但 DeepSeek 做 chat），
//! 耦合在一起反而增加配置复杂度和心智负担。
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

use super::store::ConfigKey;

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
    /// 云端 STT 供应商配置（独立模式：kind/base_url/model_id 直接存储）。
    ///
    /// API Key 不在此结构中——通过 `save_stt_secret` / `has_stt_secret` 命令存取，
    /// secret_ref 用 `stt:cloud` 约定（复用 Credential Manager 体系，独立于 AI 供应商）。
    #[serde(default)]
    pub cloud_provider: Option<SttCloudProvider>,

    /// 已废弃字段（0.12 曾用 `cloud` 引用 AIConfig 供应商+模型，现已回归独立配置）。
    /// 保留仅为反序列化兼容——启动期迁移时从 AIConfig 解析回 `cloud_provider`。
    #[serde(default)]
    #[allow(dead_code)]
    pub cloud: Option<serde_json::Value>,

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
    #[serde(
        default = "default_streaming_mode",
        deserialize_with = "deserialize_streaming_mode"
    )]
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

/// 云端 STT 供应商配置（独立模式）。
///
/// 用户在语音设置页直接配置 kind/base_url/model_id，不依赖 AIConfig。
///
/// **secret 管理**:API Key 不在此结构中——通过 `save_stt_secret` /
/// `has_stt_secret` 命令存取(复用 Credential Manager 体系),
/// secret_ref 用 `stt:cloud` 约定。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SttCloudProvider {
    /// 供应商种类:"openai" / "groq" / "mimo" / "mimo_plan" / "custom"
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
    #[serde(
        default = "default_funasr_model",
        deserialize_with = "deserialize_funasr_model"
    )]
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
            cloud_provider: None,
            cloud: None,
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
    /// 云端 STT 是否已配置（`cloud_provider` 有值即视为已配置）。
    pub fn is_cloud_configured(&self) -> bool {
        self.cloud_provider.is_some()
    }

    /// 启动期一次性迁移（0.12 `cloud` 引用模式 → 独立 `cloud_provider`）。
    ///
    /// 若 `cloud` 字段有值（0.12 引用 AIConfig 供应商+模型的结构），
    /// 尝试在 AIConfig 中找匹配的 provider，解析出 kind/base_url/model_id
    /// 写回 `cloud_provider`，并清空 `cloud` 字段。
    ///
    /// 返回 `true` = 已迁移（调用方需持久化到 DB + 更新缓存）。
    pub fn apply_migration(&mut self, ai_config: &super::ai_config::AIConfig) -> bool {
        // cloud_provider 已有值 → 不需要迁移
        if self.cloud_provider.is_some() {
            return false;
        }
        // cloud 字段无值 → 不需要迁移
        let cloud_val = match &self.cloud {
            Some(v) => v,
            None => return false,
        };

        // 解析 0.12 cloud 结构 {provider_id, model_id}
        #[derive(serde::Deserialize)]
        struct LegacyCloud {
            provider_id: String,
            model_id: String,
        }
        let legacy: LegacyCloud = match serde_json::from_value(cloud_val.clone()) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(error = %e, "STT cloud 字段反序列化失败,跳过迁移");
                return false;
            }
        };

        // 在 AIConfig 中找匹配的 provider
        let provider = ai_config
            .providers
            .iter()
            .find(|p| p.id == legacy.provider_id);
        let Some(provider) = provider else {
            tracing::warn!(
                provider_id = %legacy.provider_id,
                "STT 迁移: AIConfig 中未找到匹配的供应商,cloud 字段丢弃"
            );
            self.cloud = None;
            return true; // 清掉无效的 cloud 字段
        };

        // 从 ProviderKind + base_url 反推 STT kind 字符串
        let kind = match provider.kind {
            super::ai_config::ProviderKind::OpenAICompatible
                // 检查 base_url 是否为 mimo
                if provider
                    .base_url
                    .as_deref()
                    .is_some_and(|u| u.contains("xiaomimimo.com"))
                => {
                    if provider
                        .base_url
                        .as_deref()
                        .is_some_and(|u| u.contains("token-plan"))
                    {
                        "mimo_plan"
                    } else {
                        "mimo"
                    }
                }
            _ => "openai", // 其他 kind 降级为 openai
        };

        self.cloud_provider = Some(SttCloudProvider {
            kind: kind.to_string(),
            base_url: provider.base_url.clone(),
            model_id: legacy.model_id.clone(),
        });
        self.cloud = None;
        tracing::info!(
            kind = %kind,
            model_id = %legacy.model_id,
            "STT 云端配置迁移完成（cloud → cloud_provider，独立模式）"
        );
        true
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
    if let Some(lock) = CONFIG_CACHE.get()
        && let Ok(mut guard) = lock.write()
    {
        *guard = config.clone();
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

impl ConfigKey for SttConfig {
    const KEY: &'static str = "stt:config";
}

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
        let json =
            r#"{"enabled":true,"local_engine":{"streaming_model":"paraformer-zh-streaming"}}"#;
        let cfg: SttConfig = serde_json::from_str(json).unwrap();
        assert!(cfg.enabled);
        // streaming_model 字段保留但已废弃，不再使用
    }

    // ── 独立模式 + 迁移测试 ──────────────────────────────────────────────

    #[test]
    fn cloud_provider_serializes() {
        let c = SttCloudProvider {
            kind: "openai".into(),
            base_url: Some("https://api.openai.com/v1".into()),
            model_id: "whisper-large-v3".into(),
        };
        let s = serde_json::to_string(&c).unwrap();
        let c2: SttCloudProvider = serde_json::from_str(&s).unwrap();
        assert_eq!(c.kind, c2.kind);
        assert_eq!(c.model_id, c2.model_id);
    }

    #[test]
    fn config_with_cloud_provider_deserializes() {
        let json = r#"{"enabled":true,"cloud_provider":{"kind":"openai","base_url":"https://api.openai.com/v1","model_id":"whisper-large-v3"}}"#;
        let cfg: SttConfig = serde_json::from_str(json).unwrap();
        assert!(cfg.cloud_provider.is_some());
        assert_eq!(cfg.cloud_provider.as_ref().unwrap().kind, "openai");
    }

    #[test]
    fn legacy_cloud_field_deserializes_as_json_value() {
        // 0.12 cloud 字段现在存为 serde_json::Value（向后兼容）
        let json = r#"{"enabled":true,"cloud":{"provider_id":"p1","model_id":"whisper-large-v3"}}"#;
        let cfg: SttConfig = serde_json::from_str(json).unwrap();
        assert!(cfg.cloud.is_some());
        assert!(cfg.cloud_provider.is_none());
    }

    #[test]
    fn is_cloud_configured_checks_cloud_provider() {
        let cfg1 = SttConfig {
            cloud_provider: Some(SttCloudProvider {
                kind: "openai".into(),
                base_url: None,
                model_id: "m".into(),
            }),
            ..Default::default()
        };
        assert!(cfg1.is_cloud_configured());

        let cfg2 = SttConfig::default();
        assert!(!cfg2.is_cloud_configured());
    }

    // ── apply_migration 测试（0.12 cloud → cloud_provider 迁移）────────────

    #[test]
    fn apply_migration_migrates_legacy_cloud_to_provider() {
        let mut cfg = SttConfig {
            cloud_provider: None,
            cloud: Some(serde_json::json!({
                "provider_id": "ai-p1",
                "model_id": "whisper-1"
            })),
            ..Default::default()
        };
        let ai_config = crate::domain::config::ai_config::AIConfig {
            providers: vec![crate::domain::config::ai_config::ProviderEntry {
                id: "ai-p1".into(),
                display_name: "OpenAI".into(),
                kind: crate::domain::config::ai_config::ProviderKind::OpenAICompatible,
                base_url: Some("https://api.openai.com/v1".into()),
                secret_ref: "blink/ai-p1/key".into(),
                models: vec![crate::domain::config::ai_config::ModelEntry {
                    id: "whisper-1".into(),
                    display_name: "Whisper".into(),
                    enabled: true,
                    context_window: None,
                    input_price_per_million: None,
                    output_price_per_million: None,
                    temperature: None,
                    max_tokens: None,
                    custom_parameters: Vec::new(),
                    reasoning_effort: None,
                    capabilities: vec![crate::domain::config::ai_config::ModelCapability::Stt],
                }],
                enabled: true,
                created_at: 0,
            }],
            ..Default::default()
        };
        assert!(cfg.apply_migration(&ai_config), "应成功迁移");
        assert!(cfg.cloud_provider.is_some(), "cloud_provider 应已写回");
        assert!(cfg.cloud.is_none(), "cloud 字段应已清空");
        let cp = cfg.cloud_provider.as_ref().unwrap();
        assert_eq!(cp.kind, "openai");
        assert_eq!(cp.model_id, "whisper-1");
        assert_eq!(cp.base_url.as_deref(), Some("https://api.openai.com/v1"));
    }

    #[test]
    fn apply_migration_migrates_mimo_kind() {
        let mut cfg = SttConfig {
            cloud_provider: None,
            cloud: Some(serde_json::json!({
                "provider_id": "mimo-p1",
                "model_id": "mimo-asr-1"
            })),
            ..Default::default()
        };
        let ai_config = crate::domain::config::ai_config::AIConfig {
            providers: vec![crate::domain::config::ai_config::ProviderEntry {
                id: "mimo-p1".into(),
                display_name: "MiMo".into(),
                kind: crate::domain::config::ai_config::ProviderKind::OpenAICompatible,
                base_url: Some("https://token-plan-cn.xiaomimimo.com/v1".into()),
                secret_ref: "blink/mimo-p1/key".into(),
                models: vec![crate::domain::config::ai_config::ModelEntry {
                    id: "mimo-asr-1".into(),
                    display_name: "MiMo ASR".into(),
                    enabled: true,
                    context_window: None,
                    input_price_per_million: None,
                    output_price_per_million: None,
                    temperature: None,
                    max_tokens: None,
                    custom_parameters: Vec::new(),
                    reasoning_effort: None,
                    capabilities: vec![crate::domain::config::ai_config::ModelCapability::Stt],
                }],
                enabled: true,
                created_at: 0,
            }],
            ..Default::default()
        };
        assert!(cfg.apply_migration(&ai_config));
        let cp = cfg.cloud_provider.as_ref().unwrap();
        assert_eq!(cp.kind, "mimo_plan", "token-plan 域名应识别为 mimo_plan");
    }

    #[test]
    fn apply_migration_noop_when_provider_already_set() {
        let mut cfg = SttConfig {
            cloud_provider: Some(SttCloudProvider {
                kind: "openai".into(),
                base_url: None,
                model_id: "whisper-1".into(),
            }),
            cloud: Some(serde_json::json!({"provider_id": "p", "model_id": "m"})),
            ..Default::default()
        };
        // cloud_provider 已有值 -> 不迁移
        assert!(!cfg.apply_migration(&crate::domain::config::ai_config::AIConfig::default()));
        assert!(cfg.cloud_provider.is_some(), "cloud_provider 不变");
    }

    #[test]
    fn apply_migration_clears_invalid_cloud() {
        // cloud 有值但 AIConfig 中找不到匹配的 provider -> 清掉 cloud
        let mut cfg = SttConfig {
            cloud_provider: None,
            cloud: Some(serde_json::json!({
                "provider_id": "nonexistent",
                "model_id": "whisper-1"
            })),
            ..Default::default()
        };
        let ai_config = crate::domain::config::ai_config::AIConfig::default();
        assert!(cfg.apply_migration(&ai_config), "应清掉无效 cloud");
        assert!(cfg.cloud.is_none(), "cloud 应已清空");
        assert!(cfg.cloud_provider.is_none(), "cloud_provider 仍为 None");
    }

    #[test]
    fn apply_migration_noop_when_both_none() {
        let mut cfg = SttConfig::default();
        assert!(!cfg.apply_migration(&crate::domain::config::ai_config::AIConfig::default()));
    }
}
