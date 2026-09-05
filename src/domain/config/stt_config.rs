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
//! - ~~`hotwords`~~: 已删除（GGUF worker 不支持热词）
//! - ~~`use_itn`~~: 已删除（GGUF worker 不消费 ITN 开关；SenseVoice 内置 ITN 不可控）

use serde::{Deserialize, Serialize};

use super::store::ConfigKey;

// ── LocalSttSelection 联合引用（0.22.6 H4）───────────────────────────────────

/// 本地 STT 选择——`engine_id + model_id` 联合引用。
///
/// 这是 0.22.6 的稳定选择真源。旧的 `local_model_id` 和
/// `local_engine.funasr_model` 只作为兼容迁移输入，不再作为第一真源。
///
/// **语义约束**：
/// - `engine_id` 必须在编译期 allowlist 中（当前仅 "funasr"）
/// - `model_id` 必须是该引擎 `ModelRegistry` 中已注册的模型 id
/// - 模型必须已安装（`Installed` + `Verified`/`Unverified`）才可被选择
///
/// 迁移策略（`migrate_local_stt_selection`）：
/// - 旧 `local_model_id` 存在时，映射为 `LocalSttSelection { engine_id: "funasr", model_id }`
/// - 旧 `local_model_id` 为 None 但 `local_engine.funasr_model` 存在时，
///   使用 `funasr_model` 值作为 `model_id`
/// - 两者都为空时不产生选择
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalSttSelection {
    /// 引擎 id（当前仅 "funasr"）
    pub engine_id: String,
    /// 模型 id（如 "iic/SenseVoiceSmall" / "paraformer-zh"）
    pub model_id: String,
}

impl LocalSttSelection {
    /// 创建新的本地 STT 选择。
    pub fn new(engine_id: impl Into<String>, model_id: impl Into<String>) -> Self {
        Self {
            engine_id: engine_id.into(),
            model_id: model_id.into(),
        }
    }

    /// 默认 FunASR 引擎 id。
    pub const FUNASR_ENGINE_ID: &'static str = "funasr";
}

/// 旧 `local_model_id`（短名）到 `model_id`（FunASR ModelScope id）的映射。
///
/// 旧的 `local_model_id` 使用短名（如 "sensevoice-small"），
/// 新的 `model_id` 使用完整 FunASR ModelScope id（如 "iic/SenseVoiceSmall"）。
///
/// 此函数仅供迁移使用——迁移完成后不再需要。
fn migrate_local_model_id_to_funasr_model(local_model_id: &str) -> Option<&'static str> {
    match local_model_id {
        "sensevoice-small" => Some("iic/SenseVoiceSmall"),
        "paraformer-zh" => Some("paraformer-zh"),
        // 兼容旧配置中的完整 id
        "iic/SenseVoiceSmall" => Some("iic/SenseVoiceSmall"),
        other => {
            tracing::warn!(
                local_model_id = other,
                "旧 local_model_id 无法映射到已知 FunASR 模型"
            );
            None
        }
    }
}

/// GGUF 常驻 worker 的三个稳定模型 id（0.22.7.3+；与 app 层 GGUF 目录一致）。
pub const GGUF_SENSEVOICE_MODEL_ID: &str = "gguf/sensevoice-small-q8";
pub const GGUF_PARAFORMER_MODEL_ID: &str = "gguf/paraformer-zh-q8";
pub const GGUF_NANO_MODEL_ID: &str = "gguf/fun-asr-nano-q4km";

/// 旧模型 id（Python/PyTorch 时代的 ModelScope id 与短名）→ GGUF 模型 id 的
/// 确定映射（0.22.7 切换用）。
///
/// 语义：**保持用户选择的模型种类不变**（SenseVoice→SenseVoice GGUF、
/// Paraformer→Paraformer GGUF），只更换底层 runtime；不存在静默换模型。
/// 旧 id 无对应 GGUF 实现时返回 None——此时选择保持原样并记录 warn，
/// 让用户知道该模型已不可用，需手动切换。
pub fn legacy_model_to_gguf_id(legacy: &str) -> Option<&'static str> {
    match legacy {
        // SenseVoice 家族
        "iic/SenseVoiceSmall" | "SenseVoice" | "sensevoice" | "sensevoice-small" => {
            Some(GGUF_SENSEVOICE_MODEL_ID)
        }
        // Paraformer 家族（含旧真流式 online 变体——同种类迁移为 Paraformer GGUF）
        "paraformer-zh"
        | "paraformer-zh-streaming"
        | "iic/speech_seaco_paraformer_large_asr_nat-zh-cn-16k-common-vocab8404-pytorch"
        | "iic/speech_paraformer-large_asr_nat-zh-cn-16k-common-vocab8404-online"
        | "iic/speech_paraformer-large_asr_nat-zh-cn-16k-common-vocab8404" => {
            Some(GGUF_PARAFORMER_MODEL_ID)
        }
        _ => None,
    }
}

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

    /// 本地 STT 选择（0.22.6 H4：`engine_id + model_id` 联合引用）。
    ///
    /// 这是 0.22.6 的稳定选择真源。旧的 `local_model_id` 和
    /// `local_engine.funasr_model` 只作为兼容迁移输入。
    ///
    /// 迁移在 `migrate_local_stt_selection` 中确定性执行，迁移后此字段成为唯一真源。
    #[serde(default)]
    pub local_stt_selection: Option<LocalSttSelection>,

    /// 当前选用的本地模型 id(如 "sensevoice-small")
    /// 对应 `ModelDescriptor::id`(模型注册表在 `domain::stt::mod.rs`)
    ///
    /// **已废弃**：0.22.6 改用 `local_stt_selection` 联合引用。
    /// 保留仅为反序列化兼容——启动期迁移时读取此字段构造 `local_stt_selection`。
    #[serde(default)]
    #[allow(dead_code)]
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
    /// 模型标识（GGUF 常驻 worker，0.22.7.4 起唯一本地实现）
    /// 如 "gguf/sensevoice-small-q8" / "gguf/paraformer-zh-q8" /
    /// "gguf/fun-asr-nano-q4km"；旧 Python 时代 id 在反序列化时归一化。
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
    // ── 已废弃字段（反序列化时忽略，不报错）──
    /// 旧 `hotwords` 字段（GGUF worker 不支持热词，0.22.7 契约收口删除）。
    /// 保留仅为反序列化兼容，不实际使用，下次正常保存时自然消失。
    #[serde(default)]
    #[allow(dead_code)]
    pub hotwords: Option<String>,
    /// 旧 `use_itn` 字段（GGUF worker 不消费 ITN 开关，0.22.7 契约收口删除）。
    /// 保留仅为反序列化兼容，不实际使用，下次正常保存时自然消失。
    #[serde(default)]
    #[allow(dead_code)]
    pub use_itn: Option<bool>,
    /// VAD 切句参数（伪流式模式生效）
    #[serde(default)]
    pub vad: VadConfig,

    /// VAD 前端种类（内部解析用，不暴露给普通用户）。
    ///
    /// 0.22.9 Handoff 06：`auto` 在 production gate 前解析到 `energy`。
    /// 显式修改过 EnergyVad 参数的旧配置继续走 `energy`。
    /// 此字段缺失时默认为 `auto`——缺失字段安全迁移，不误判为用户定制。
    #[serde(default = "default_vad_kind")]
    pub vad_kind: String,

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
    GGUF_SENSEVOICE_MODEL_ID.to_string()
}

/// 反序列化时把旧 Python/PyTorch 时代的模型 id 归一化为 GGUF 模型 id。
///
/// 0.22.7.4 起 `funasr` 引擎只有 GGUF 常驻 worker 一个实现，镜像字段
/// `local_engine.funasr_model` 在加载时即归一到 GGUF id，保证无
/// `local_stt_selection` 的旧配置也能直接启动。模型种类保持不变：
/// SenseVoice→SenseVoice、Paraformer→Paraformer，不静默换模型。
fn deserialize_funasr_model<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<String, D::Error> {
    use serde::Deserialize;
    let raw = String::deserialize(deserializer)?;
    Ok(match raw.as_str() {
        // SenseVoice 旧短名与完整 ModelScope id
        "sensevoice" | "SenseVoice" | "SenseVoiceSmall" | "iic/SenseVoiceSmall" => {
            GGUF_SENSEVOICE_MODEL_ID.to_string()
        }
        // Paraformer 旧短名、完整 ModelScope id 与曾经的错误完整 id
        "paraformer-zh"
        | "iic/speech_seaco_paraformer_large_asr_nat-zh-cn-16k-common-vocab8404-pytorch"
        | "iic/speech_paraformer-large_asr_nat-zh-cn-16k-common-vocab8404" => {
            GGUF_PARAFORMER_MODEL_ID.to_string()
        }
        // 旧真流式 Paraformer 变体——同种类迁移为 Paraformer GGUF（不静默换模型）
        "paraformer-zh-streaming"
        | "iic/speech_paraformer-large_asr_nat-zh-cn-16k-common-vocab8404-online" => {
            GGUF_PARAFORMER_MODEL_ID.to_string()
        }
        other => other.to_string(),
    })
}

fn default_device() -> String {
    "cpu".to_string()
}

fn default_vad_kind() -> String {
    "auto".to_string()
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
            use_itn: None,
            vad: VadConfig::default(),
            vad_kind: default_vad_kind(),
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
            local_stt_selection: None,
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

    /// 启动期一次性迁移：本地 STT 选择 + 云端配置。
    ///
    /// 依次执行：
    /// 1. `migrate_local_stt_selection`：旧 `local_model_id` +
    ///    `local_engine.funasr_model` → `local_stt_selection` 联合引用
    /// 2. `migrate_selection_to_gguf`：旧 Python 时代模型 id → GGUF 模型 id
    ///    （0.22.7.4 切换；模型种类不变）
    /// 3. 云端迁移：旧 0.12 `cloud` 字段 → `cloud_provider` 独立模式
    ///
    /// 任一步骤产生变更时返回 `true`（调用方需持久化到 DB + 更新缓存）。
    pub fn apply_migration(&mut self, ai_config: &super::ai_config::AIConfig) -> bool {
        let local_migrated = self.migrate_local_stt_selection();
        let gguf_migrated = self.migrate_selection_to_gguf();
        let cloud_migrated = self.migrate_cloud_config(ai_config);
        local_migrated || gguf_migrated || cloud_migrated
    }

    /// 迁移本地 STT 选择：旧 `local_model_id` + `local_engine.funasr_model` → `local_stt_selection`。
    ///
    /// 0.22.6 H4：将旧的本地 STT 选择（`local_model_id` 短名 +
    /// `local_engine.funasr_model` ModelScope id）收敛为
    /// `LocalSttSelection { engine_id, model_id }` 联合引用。
    ///
    /// 迁移规则（确定性，不丢失旧用户选择）：
    /// 1. `local_stt_selection` 已存在 → 跳过
    /// 2. `local_model_id` 存在 → 按短名映射为完整 model_id，
    ///    构造 `LocalSttSelection { engine_id: "funasr", model_id }`
    /// 3. `local_model_id` 为 None 但 `local_engine.funasr_model` 非默认值 →
    ///    使用 `funasr_model` 值作为 `model_id`
    /// 4. 两者都为空/默认 → 不产生选择
    ///
    /// 返回 `true` = 已迁移（调用方需持久化到 DB + 更新缓存）。
    pub fn migrate_local_stt_selection(&mut self) -> bool {
        // 已有联合引用 → 跳过
        if self.local_stt_selection.is_some() {
            return false;
        }

        // 优先从 local_model_id 迁移（短名 → 完整 model_id）
        if let Some(ref old_id) = self.local_model_id {
            if let Some(model_id) = migrate_local_model_id_to_funasr_model(old_id) {
                self.local_stt_selection = Some(LocalSttSelection::new(
                    LocalSttSelection::FUNASR_ENGINE_ID,
                    model_id,
                ));
                tracing::info!(
                    old_local_model_id = %old_id,
                    new_engine_id = LocalSttSelection::FUNASR_ENGINE_ID,
                    new_model_id = model_id,
                    "本地 STT 选择迁移完成（local_model_id → local_stt_selection）"
                );
                return true;
            }
            // local_model_id 存在但无法映射——继续尝试 funasr_model
            tracing::warn!(
                local_model_id = %old_id,
                "local_model_id 无法映射到已知模型，尝试 funasr_model"
            );
        }

        // 从 local_engine.funasr_model 迁移
        // 只有非默认值才表示用户曾显式选择过模型
        // 默认值比较用 GGUF 默认 id：旧默认 "iic/SenseVoiceSmall" 已在
        // `deserialize_funasr_model` 中归一化为 GGUF 默认 id。
        let funasr_model = &self.local_engine.funasr_model;
        if !funasr_model.is_empty() && funasr_model != GGUF_SENSEVOICE_MODEL_ID {
            self.local_stt_selection = Some(LocalSttSelection::new(
                LocalSttSelection::FUNASR_ENGINE_ID,
                funasr_model.as_str(),
            ));
            tracing::info!(
                funasr_model = %funasr_model,
                new_engine_id = LocalSttSelection::FUNASR_ENGINE_ID,
                "本地 STT 选择迁移完成（funasr_model → local_stt_selection）"
            );
            return true;
        }

        // 两者都为空/默认 → 不产生选择
        false
    }

    /// 把已存在的本地选择从旧 Python 时代模型 id 迁移到 GGUF 模型 id（0.22.7.4）。
    ///
    /// 在 GGUF 成为 `funasr` 引擎唯一实现时由启动迁移调用：
    /// - `local_stt_selection.model_id` 是旧 id → 按 [`legacy_model_to_gguf_id`]
    ///   确定映射（保持模型种类：SenseVoice→SenseVoice GGUF）；
    /// - 同步 `local_engine.funasr_model`（兼容镜像字段）；
    /// - 已是 GGUF id 或旧 id 无映射（返回 false，选择保持原样并记录 warn——
    ///   不静默切换到其他模型）。
    pub fn migrate_selection_to_gguf(&mut self) -> bool {
        let Some(sel) = self.local_stt_selection.clone() else {
            return false;
        };
        if sel.engine_id != LocalSttSelection::FUNASR_ENGINE_ID {
            return false;
        }
        let Some(new_id) = legacy_model_to_gguf_id(&sel.model_id) else {
            if !sel.model_id.starts_with("gguf/") {
                tracing::warn!(
                    model_id = %sel.model_id,
                    "本地 STT 选择的旧模型无 GGUF 映射——保持原样（不静默切换）"
                );
            }
            return false;
        };
        self.local_stt_selection = Some(LocalSttSelection::new(
            LocalSttSelection::FUNASR_ENGINE_ID,
            new_id,
        ));
        self.local_engine.funasr_model = new_id.to_string();
        tracing::info!(
            old_model_id = %sel.model_id,
            new_model_id = new_id,
            "本地 STT 选择已迁移到 GGUF 模型 id（模型种类不变）"
        );
        true
    }

    /// 迁移云端 STT 配置：旧 0.12 `cloud` 字段 → `cloud_provider` 独立模式。
    ///
    /// 迁移规则：
    /// 1. `cloud_provider` 已有值 → 跳过（不覆盖）
    /// 2. `cloud` 字段无值 → 跳过
    /// 3. `cloud` 字段有值但在 AIConfig 中找不到匹配的 provider → 清掉无效 cloud
    /// 4. 找到匹配 provider → 构造 `SttCloudProvider`，清掉 cloud
    ///
    /// 返回 `true` = 已迁移（调用方需持久化）。
    fn migrate_cloud_config(&mut self, ai_config: &super::ai_config::AIConfig) -> bool {
        // cloud_provider 已有值 → 不迁移
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
        assert_eq!(cfg.local_engine.funasr_model, GGUF_SENSEVOICE_MODEL_ID);
        assert_eq!(cfg.local_engine.device, "cpu");
        assert!(cfg.local_engine.hotwords.is_none());
        assert!(cfg.local_engine.use_itn.is_none());
        assert_eq!(cfg.local_engine.vad.silence_threshold, 0.005);
        assert_eq!(cfg.local_engine.vad.min_silence_ms, 300);
        assert_eq!(cfg.local_engine.vad.min_sentence_ms, 800);
        assert_eq!(cfg.local_engine.vad_kind, "auto");
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
                funasr_model: GGUF_PARAFORMER_MODEL_ID.into(),
                device: "cuda".into(),
                num_threads: Some(4),
                auto_start_server: true,
                hotwords: Some("美团 100, 快手 80".into()),
                use_itn: Some(false),
                vad: VadConfig {
                    silence_threshold: 0.003,
                    min_silence_ms: 200,
                    min_sentence_ms: 600,
                },
                vad_kind: "energy".into(),
                streaming_model: None,
            },
            local_stt_selection: Some(LocalSttSelection::new("funasr", GGUF_PARAFORMER_MODEL_ID)),
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
        assert_eq!(restored.local_engine.funasr_model, GGUF_PARAFORMER_MODEL_ID);
        assert_eq!(restored.local_engine.device, "cuda");
        assert_eq!(restored.local_engine.num_threads, Some(4));
        assert_eq!(
            restored.local_engine.hotwords.as_deref(),
            Some("美团 100, 快手 80")
        );
        assert_eq!(restored.local_engine.use_itn, Some(false));
        assert_eq!(restored.local_engine.vad.silence_threshold, 0.003);
        assert_eq!(restored.local_engine.vad.min_silence_ms, 200);
        assert_eq!(restored.local_engine.vad.min_sentence_ms, 600);
        assert_eq!(restored.local_engine.vad_kind, "energy");
        assert_eq!(restored.local_model_id.as_deref(), Some("sensevoice-small"));
        assert_eq!(restored.streaming_mode, StreamingMode::Off);
        // 0.22.6 H4: local_stt_selection round-trip
        assert_eq!(
            restored.local_stt_selection.as_ref().unwrap().engine_id,
            "funasr"
        );
        assert_eq!(
            restored.local_stt_selection.as_ref().unwrap().model_id,
            GGUF_PARAFORMER_MODEL_ID
        );
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
        assert_eq!(cfg.local_engine.funasr_model, GGUF_SENSEVOICE_MODEL_ID);
        assert!(cfg.local_engine.use_itn.is_none());
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

    /// 验证旧配置中的 "sensevoice" 模型名被归一化为 SenseVoice GGUF id。
    #[test]
    fn deserialize_normalizes_old_sensevoice_model_name() {
        let json = r#"{"enabled":true,"mode":"local","local_engine":{"server_port":8000,"funasr_model":"sensevoice","device":"cpu"}}"#;
        let cfg: SttConfig = serde_json::from_str(json).unwrap();
        assert_eq!(
            cfg.local_engine.funasr_model, GGUF_SENSEVOICE_MODEL_ID,
            "旧配置中的 'sensevoice' 应被归一化为 SenseVoice GGUF id"
        );
    }

    /// 验证 "paraformer-zh" 短名被归一化为 Paraformer GGUF id。
    #[test]
    fn deserialize_normalizes_paraformer_short_name() {
        let json = r#"{"enabled":true,"mode":"local","local_engine":{"server_port":8000,"funasr_model":"paraformer-zh","device":"cpu"}}"#;
        let cfg: SttConfig = serde_json::from_str(json).unwrap();
        assert_eq!(
            cfg.local_engine.funasr_model, GGUF_PARAFORMER_MODEL_ID,
            "短名 'paraformer-zh' 应被归一化为 Paraformer GGUF id"
        );
    }

    /// 验证旧的错误完整 ID 被归一化为 Paraformer GGUF id。
    #[test]
    fn deserialize_normalizes_old_wrong_full_id() {
        let json = r#"{"enabled":true,"mode":"local","local_engine":{"server_port":8000,"funasr_model":"iic/speech_paraformer-large_asr_nat-zh-cn-16k-common-vocab8404","device":"cpu"}}"#;
        let cfg: SttConfig = serde_json::from_str(json).unwrap();
        assert_eq!(
            cfg.local_engine.funasr_model, GGUF_PARAFORMER_MODEL_ID,
            "错误的完整 ID 应归一化为 Paraformer GGUF id"
        );
    }

    /// 验证旧真流式 Paraformer 模型名被归一化为 Paraformer GGUF id（同种类迁移）。
    #[test]
    fn deserialize_normalizes_old_streaming_model_name() {
        let json = r#"{"enabled":true,"mode":"local","local_engine":{"server_port":8000,"funasr_model":"paraformer-zh-streaming","device":"cpu"}}"#;
        let cfg: SttConfig = serde_json::from_str(json).unwrap();
        assert_eq!(
            cfg.local_engine.funasr_model, GGUF_PARAFORMER_MODEL_ID,
            "旧真流式模型 'paraformer-zh-streaming' 应归一化为 Paraformer GGUF id（同种类迁移，不静默换模型）"
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
                    thinking_style: None,
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
                    thinking_style: None,
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

    // ── 0.22.6 H4: 本地 STT 选择迁移测试 ──────────────────────────────────

    /// 旧 local_model_id="sensevoice-small" 迁移为 LocalSttSelection { funasr, iic/SenseVoiceSmall }
    #[test]
    fn migrate_local_stt_from_local_model_id_sensevoice() {
        let mut cfg = SttConfig {
            local_model_id: Some("sensevoice-small".into()),
            ..Default::default()
        };
        assert!(cfg.migrate_local_stt_selection());
        let sel = cfg.local_stt_selection.unwrap();
        assert_eq!(sel.engine_id, "funasr");
        assert_eq!(sel.model_id, "iic/SenseVoiceSmall");
    }

    /// 旧 local_model_id="paraformer-zh" 迁移为 LocalSttSelection { funasr, paraformer-zh }
    #[test]
    fn migrate_local_stt_from_local_model_id_paraformer() {
        let mut cfg = SttConfig {
            local_model_id: Some("paraformer-zh".into()),
            ..Default::default()
        };
        assert!(cfg.migrate_local_stt_selection());
        let sel = cfg.local_stt_selection.unwrap();
        assert_eq!(sel.engine_id, "funasr");
        assert_eq!(sel.model_id, "paraformer-zh");
    }

    /// 旧 local_model_id 已包含完整 ModelScope id（iic/SenseVoiceSmall）也能正确迁移
    #[test]
    fn migrate_local_stt_from_full_modelscope_id() {
        let mut cfg = SttConfig {
            local_model_id: Some("iic/SenseVoiceSmall".into()),
            ..Default::default()
        };
        assert!(cfg.migrate_local_stt_selection());
        let sel = cfg.local_stt_selection.unwrap();
        assert_eq!(sel.engine_id, "funasr");
        assert_eq!(sel.model_id, "iic/SenseVoiceSmall");
    }

    /// 无 local_model_id 但 funasr_model=paraformer-zh → 从 funasr_model 迁移
    #[test]
    fn migrate_local_stt_from_funasr_model_paraformer() {
        let mut cfg = SttConfig {
            local_model_id: None,
            local_engine: LocalEngineConfig {
                funasr_model: "paraformer-zh".into(),
                ..Default::default()
            },
            ..Default::default()
        };
        assert!(cfg.migrate_local_stt_selection());
        let sel = cfg.local_stt_selection.unwrap();
        assert_eq!(sel.engine_id, "funasr");
        assert_eq!(sel.model_id, "paraformer-zh");
    }

    /// 默认配置（funasr_model=iic/SenseVoiceSmall，local_model_id=None）→ 不迁移
    #[test]
    fn migrate_local_stt_noop_when_default() {
        let mut cfg = SttConfig::default();
        assert!(!cfg.migrate_local_stt_selection());
        assert!(cfg.local_stt_selection.is_none());
    }

    /// local_stt_selection 已存在 → 幂等跳过
    #[test]
    fn migrate_local_stt_idempotent_when_already_set() {
        let mut cfg = SttConfig {
            local_model_id: Some("sensevoice-small".into()),
            local_stt_selection: Some(LocalSttSelection::new("funasr", "paraformer-zh")),
            ..Default::default()
        };
        assert!(!cfg.migrate_local_stt_selection());
        // 不被 local_model_id 覆盖
        assert_eq!(cfg.local_stt_selection.unwrap().model_id, "paraformer-zh");
    }

    /// apply_migration 同时迁移本地 + 云端
    #[test]
    fn apply_migration_migrates_both_local_and_cloud() {
        let mut cfg = SttConfig {
            local_model_id: Some("sensevoice-small".into()),
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
                models: vec![],
                enabled: true,
                created_at: 0,
            }],
            ..Default::default()
        };
        assert!(cfg.apply_migration(&ai_config));
        assert!(cfg.local_stt_selection.is_some());
        assert!(cfg.cloud_provider.is_some());
        assert!(cfg.cloud.is_none());
    }

    // ── 0.22.6 H4: 自启条件测试 ──────────────────────────────────────────

    /// 自启条件：enabled + Local 模式 + auto_start_server + local_stt_selection 有值
    /// → 满足自启
    #[test]
    fn auto_start_condition_all_met() {
        let cfg = SttConfig {
            enabled: true,
            mode: SttMode::Local,
            local_engine: LocalEngineConfig {
                auto_start_server: true,
                ..Default::default()
            },
            local_stt_selection: Some(LocalSttSelection::new("funasr", "iic/SenseVoiceSmall")),
            ..Default::default()
        };
        assert!(cfg.enabled, "STT 应启用");
        assert_eq!(cfg.mode, SttMode::Local, "应为本地模式");
        assert!(cfg.local_engine.auto_start_server, "auto_start 应为 true");
        assert!(cfg.local_stt_selection.is_some(), "应有模型选择");
    }

    /// 自启条件：STT 未启用 → 不满足
    #[test]
    fn auto_start_condition_stt_disabled() {
        let cfg = SttConfig {
            enabled: false,
            mode: SttMode::Local,
            local_engine: LocalEngineConfig {
                auto_start_server: true,
                ..Default::default()
            },
            local_stt_selection: Some(LocalSttSelection::new("funasr", "iic/SenseVoiceSmall")),
            ..Default::default()
        };
        assert!(!cfg.enabled, "STT 未启用 → 不满足自启");
    }

    /// 自启条件：Cloud 模式 → 不满足本地自启
    #[test]
    fn auto_start_condition_cloud_mode() {
        let cfg = SttConfig {
            enabled: true,
            mode: SttMode::Cloud,
            local_engine: LocalEngineConfig {
                auto_start_server: true,
                ..Default::default()
            },
            local_stt_selection: Some(LocalSttSelection::new("funasr", "iic/SenseVoiceSmall")),
            ..Default::default()
        };
        assert_ne!(cfg.mode, SttMode::Local, "Cloud 模式 → 不满足本地自启");
    }

    /// 自启条件：auto_start_server = false → 不满足
    #[test]
    fn auto_start_condition_auto_start_false() {
        let cfg = SttConfig {
            enabled: true,
            mode: SttMode::Local,
            local_engine: LocalEngineConfig {
                auto_start_server: false,
                ..Default::default()
            },
            local_stt_selection: Some(LocalSttSelection::new("funasr", "iic/SenseVoiceSmall")),
            ..Default::default()
        };
        assert!(
            !cfg.local_engine.auto_start_server,
            "auto_start=false → 不满足自启"
        );
    }

    /// 自启条件：local_stt_selection = None → 不满足
    /// 这是 0.22.6 H4 新增的条件——没有选择模型时不应自启
    #[test]
    fn auto_start_condition_no_model_selection() {
        let cfg = SttConfig {
            enabled: true,
            mode: SttMode::Local,
            local_engine: LocalEngineConfig {
                auto_start_server: true,
                ..Default::default()
            },
            local_stt_selection: None,
            ..Default::default()
        };
        assert!(cfg.local_stt_selection.is_none(), "无模型选择 → 不满足自启");
    }

    /// 自启条件：读取/保存配置本身不隐式启动
    /// 验证 SttConfig::default() 中 auto_start_server = false
    #[test]
    fn auto_start_condition_default_is_false() {
        let cfg = SttConfig::default();
        assert!(
            !cfg.local_engine.auto_start_server,
            "默认 auto_start_server=false"
        );
    }

    // ── 0.22.6 H4: 不可用模型拒绝测试 ──────────────────────────────────

    /// EngineModelStatus.is_usable() 的行为验证——set_local_stt_selection
    /// 的验证逻辑依赖此方法。这里测试各组合。
    fn test_model_descriptor() -> crate::domain::local_engine::model::EngineModelDescriptor {
        use crate::domain::local_engine::model::EngineModelDescriptor;
        EngineModelDescriptor {
            engine_id: crate::infra::local_engine::runtime::EngineId::new("funasr")
                .expect("funasr is valid"),
            model_id: "gguf/sensevoice-small-q8".to_string(),
            display_name: "测试模型".to_string(),
            description: "测试".to_string(),
            revision: "gguf-v0.2.6".to_string(),
            checksum_source: crate::infra::local_engine::runtime::ChecksumSource::Sha256(
                "ab".repeat(32),
            ),
            estimated_size_mb: Some(243),
            compatibility_schema: 1,
            stt_capabilities: crate::domain::local_engine::SttModelCapabilities::default(),
            business: None,
        }
    }

    #[test]
    fn model_usability_check_rejects_non_installed() {
        use crate::domain::local_engine::model::{
            EngineModelStatus, ModelInstallState, ModelVerificationState,
        };
        let desc = test_model_descriptor();
        let mut status = EngineModelStatus::not_installed(&desc);
        // NotInstalled → 不可用
        status.install_state = ModelInstallState::NotInstalled;
        status.verification_state = ModelVerificationState::Verified;
        assert!(!status.is_usable(), "NotInstalled 应被拒绝");
    }

    #[test]
    fn model_usability_check_rejects_downloading() {
        use crate::domain::local_engine::model::{
            EngineModelStatus, ModelInstallState, ModelVerificationState,
        };
        let desc = test_model_descriptor();
        let mut status = EngineModelStatus::not_installed(&desc);
        status.install_state = ModelInstallState::Downloading;
        status.verification_state = ModelVerificationState::Unknown;
        assert!(!status.is_usable(), "Downloading 应被拒绝");
    }

    #[test]
    fn model_usability_check_rejects_download_failed() {
        use crate::domain::local_engine::model::{EngineModelStatus, ModelInstallState};
        let desc = test_model_descriptor();
        let mut status = EngineModelStatus::not_installed(&desc);
        status.install_state = ModelInstallState::DownloadFailed;
        assert!(!status.is_usable(), "DownloadFailed 应被拒绝");
    }

    #[test]
    fn model_usability_check_accepts_installed_verified() {
        use crate::domain::local_engine::model::{
            EngineModelStatus, ModelInstallState, ModelVerificationState,
        };
        let desc = test_model_descriptor();
        let mut status = EngineModelStatus::not_installed(&desc);
        status.install_state = ModelInstallState::Installed;
        status.verification_state = ModelVerificationState::Verified;
        assert!(status.is_usable(), "Installed + Verified 应可用");
    }

    #[test]
    fn model_usability_check_accepts_installed_unverified() {
        use crate::domain::local_engine::model::{
            EngineModelStatus, ModelInstallState, ModelVerificationState,
        };
        let desc = test_model_descriptor();
        let mut status = EngineModelStatus::not_installed(&desc);
        status.install_state = ModelInstallState::Installed;
        status.verification_state = ModelVerificationState::Unverified;
        assert!(status.is_usable(), "Installed + Unverified 应可用");
    }

    #[test]
    fn model_usability_check_rejects_installed_corrupted() {
        use crate::domain::local_engine::model::{
            EngineModelStatus, ModelInstallState, ModelVerificationState,
        };
        let desc = test_model_descriptor();
        let mut status = EngineModelStatus::not_installed(&desc);
        status.install_state = ModelInstallState::Installed;
        status.verification_state = ModelVerificationState::Corrupted;
        assert!(!status.is_usable(), "Installed + Corrupted 应被拒绝");
    }

    #[test]
    fn model_usability_check_rejects_installed_mismatched() {
        use crate::domain::local_engine::model::{
            EngineModelStatus, ModelInstallState, ModelVerificationState,
        };
        let desc = test_model_descriptor();
        let mut status = EngineModelStatus::not_installed(&desc);
        status.install_state = ModelInstallState::Installed;
        status.verification_state = ModelVerificationState::Mismatched;
        assert!(!status.is_usable(), "Installed + Mismatched 应被拒绝");
    }

    // ── 0.22.6 H4: LocalSttSelection 序列化测试 ──────────────────────────

    #[test]
    fn local_stt_selection_serializes_correctly() {
        let sel = LocalSttSelection::new("funasr", "iic/SenseVoiceSmall");
        let json = serde_json::to_string(&sel).unwrap();
        let restored: LocalSttSelection = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.engine_id, "funasr");
        assert_eq!(restored.model_id, "iic/SenseVoiceSmall");
    }

    #[test]
    fn local_stt_selection_equality() {
        let a = LocalSttSelection::new("funasr", "paraformer-zh");
        let b = LocalSttSelection::new("funasr", "paraformer-zh");
        let c = LocalSttSelection::new("funasr", "iic/SenseVoiceSmall");
        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn local_stt_selection_funasr_engine_id_constant() {
        assert_eq!(LocalSttSelection::FUNASR_ENGINE_ID, "funasr");
    }

    // ── 0.22.7 GGUF 模型迁移测试 ──────────────────────────────────────────

    #[test]
    fn legacy_to_gguf_mapping() {
        assert_eq!(
            legacy_model_to_gguf_id("iic/SenseVoiceSmall"),
            Some(GGUF_SENSEVOICE_MODEL_ID)
        );
        assert_eq!(
            legacy_model_to_gguf_id("paraformer-zh"),
            Some(GGUF_PARAFORMER_MODEL_ID)
        );
        assert_eq!(
            legacy_model_to_gguf_id(
                "iic/speech_seaco_paraformer_large_asr_nat-zh-cn-16k-common-vocab8404-pytorch"
            ),
            Some(GGUF_PARAFORMER_MODEL_ID)
        );
        // 旧真流式 Paraformer 变体也映射为 Paraformer GGUF（同种类迁移）
        assert_eq!(
            legacy_model_to_gguf_id("paraformer-zh-streaming"),
            Some(GGUF_PARAFORMER_MODEL_ID)
        );
        assert_eq!(
            legacy_model_to_gguf_id(
                "iic/speech_paraformer-large_asr_nat-zh-cn-16k-common-vocab8404-online"
            ),
            Some(GGUF_PARAFORMER_MODEL_ID)
        );
        assert_eq!(legacy_model_to_gguf_id("unknown"), None);
    }

    #[test]
    fn migrate_selection_to_gguf_maps_sensevoice_and_syncs_mirror() {
        let mut cfg = SttConfig {
            local_stt_selection: Some(LocalSttSelection::new("funasr", "iic/SenseVoiceSmall")),
            local_engine: LocalEngineConfig {
                funasr_model: "iic/SenseVoiceSmall".to_string(),
                ..Default::default()
            },
            ..Default::default()
        };
        assert!(cfg.migrate_selection_to_gguf());
        let sel = cfg.local_stt_selection.unwrap();
        assert_eq!(sel.engine_id, "funasr");
        assert_eq!(sel.model_id, GGUF_SENSEVOICE_MODEL_ID);
        // 兼容镜像字段同步
        assert_eq!(cfg.local_engine.funasr_model, GGUF_SENSEVOICE_MODEL_ID);
    }

    #[test]
    fn migrate_selection_to_gguf_maps_paraformer() {
        let mut cfg = SttConfig {
            local_stt_selection: Some(LocalSttSelection::new("funasr", "paraformer-zh")),
            ..Default::default()
        };
        assert!(cfg.migrate_selection_to_gguf());
        assert_eq!(
            cfg.local_stt_selection.unwrap().model_id,
            GGUF_PARAFORMER_MODEL_ID
        );
    }

    #[test]
    fn migrate_selection_to_gguf_noop_for_gguf_ids() {
        let mut cfg = SttConfig {
            local_stt_selection: Some(LocalSttSelection::new("funasr", GGUF_NANO_MODEL_ID)),
            ..Default::default()
        };
        assert!(!cfg.migrate_selection_to_gguf(), "GGUF id 应保持不变");
    }

    #[test]
    fn migrate_selection_to_gguf_noop_without_selection() {
        let mut cfg = SttConfig::default();
        assert!(!cfg.migrate_selection_to_gguf());
    }

    #[test]
    fn migrate_selection_to_gguf_keeps_unmappable_selection() {
        // 无映射的旧 id：不静默切换，保持原样
        let mut cfg = SttConfig {
            local_stt_selection: Some(LocalSttSelection::new("funasr", "some-exotic-model")),
            ..Default::default()
        };
        assert!(!cfg.migrate_selection_to_gguf());
        assert_eq!(
            cfg.local_stt_selection.unwrap().model_id,
            "some-exotic-model"
        );
    }

    /// 旧真流式 Paraformer 选择迁移为 Paraformer GGUF（同种类迁移，不静默换模型）。
    #[test]
    fn migrate_selection_to_gguf_maps_streaming_paraformer() {
        let mut cfg = SttConfig {
            local_stt_selection: Some(LocalSttSelection::new("funasr", "paraformer-zh-streaming")),
            local_engine: LocalEngineConfig {
                funasr_model: "paraformer-zh-streaming".to_string(),
                ..Default::default()
            },
            ..Default::default()
        };
        assert!(cfg.migrate_selection_to_gguf());
        let sel = cfg.local_stt_selection.unwrap();
        assert_eq!(sel.engine_id, "funasr");
        assert_eq!(sel.model_id, GGUF_PARAFORMER_MODEL_ID);
        // 兼容镜像字段同步
        assert_eq!(cfg.local_engine.funasr_model, GGUF_PARAFORMER_MODEL_ID);
    }

    // ── 0.22.7 契约收口：旧字段兼容测试 ──────────────────────────────────

    /// 旧 stt_config.json 中仍带 `hotwords` 和 `use_itn` 时必须能正常反序列化。
    /// 旧字段作为废弃字段被忽略，不导致启动失败或重置整个 STT 配置。
    #[test]
    fn old_config_with_hotwords_and_use_itn_deserializes_safely() {
        let json = r#"{
            "enabled": true,
            "mode": "local",
            "local_engine": {
                "server_port": 8000,
                "funasr_model": "gguf/sensevoice-small-q8",
                "device": "cpu",
                "hotwords": "美团 100, 快手 80",
                "use_itn": false,
                "vad": {
                    "silence_threshold": 0.005,
                    "min_silence_ms": 300,
                    "min_sentence_ms": 800
                }
            }
        }"#;
        let cfg: SttConfig = serde_json::from_str(json).unwrap();
        assert!(cfg.enabled);
        assert_eq!(cfg.mode, SttMode::Local);
        // 废弃字段仍可读取（但不再参与运行时逻辑）
        assert_eq!(
            cfg.local_engine.hotwords.as_deref(),
            Some("美团 100, 快手 80")
        );
        // use_itn 旧为 bool，现为 Option<bool>，serde 兼容读取
        assert_eq!(cfg.local_engine.use_itn, Some(false));
        // VAD 参数不受影响
        assert_eq!(cfg.local_engine.vad.silence_threshold, 0.005);
        assert_eq!(cfg.local_engine.vad.min_silence_ms, 300);
        assert_eq!(cfg.local_engine.vad.min_sentence_ms, 800);
    }

    /// 旧配置只有 `use_itn: true`（bool）时也能安全反序列化。
    #[test]
    fn old_config_with_use_itn_bool_deserializes_safely() {
        let json = r#"{"enabled":true,"local_engine":{"use_itn":true}}"#;
        let cfg: SttConfig = serde_json::from_str(json).unwrap();
        assert!(cfg.enabled);
        assert_eq!(cfg.local_engine.use_itn, Some(true));
    }

    /// 新配置不带 hotwords/use_itn 时，字段为 None（不影响新用户）。
    #[test]
    fn new_config_without_hotwords_and_use_itn_has_none() {
        let json = r#"{"enabled":true,"mode":"local","local_engine":{"server_port":8000,"funasr_model":"gguf/sensevoice-small-q8","device":"cpu"}}"#;
        let cfg: SttConfig = serde_json::from_str(json).unwrap();
        assert!(cfg.local_engine.hotwords.is_none());
        assert!(cfg.local_engine.use_itn.is_none());
    }

    /// 序列化后反序列化：废弃字段在 round-trip 中保留（兼容）。
    #[test]
    fn round_trip_preserves_deprecated_fields() {
        let cfg = SttConfig {
            enabled: true,
            mode: SttMode::Local,
            local_engine: LocalEngineConfig {
                hotwords: Some("test 100".into()),
                use_itn: Some(false),
                ..Default::default()
            },
            ..Default::default()
        };
        let json = serde_json::to_string(&cfg).unwrap();
        let restored: SttConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.local_engine.hotwords.as_deref(), Some("test 100"));
        assert_eq!(restored.local_engine.use_itn, Some(false));
    }

    // ── 0.22.9 Handoff 06: VAD kind + 旧配置迁移测试 ──────────────────────

    /// 新配置默认 `vad_kind = "auto"`。
    #[test]
    fn default_vad_kind_is_auto() {
        let cfg = LocalEngineConfig::default();
        assert_eq!(cfg.vad_kind, "auto");
    }

    /// 旧配置缺失 `vad_kind` 字段时安全迁移为 `"auto"`——不误判为用户定制。
    #[test]
    fn old_config_missing_vad_kind_defaults_to_auto() {
        let json = r#"{"enabled":true,"mode":"local","local_engine":{"server_port":8000,"funasr_model":"gguf/sensevoice-small-q8","device":"cpu","vad":{"silence_threshold":0.005,"min_silence_ms":300,"min_sentence_ms":800}}}"#;
        let cfg: SttConfig = serde_json::from_str(json).unwrap();
        assert_eq!(
            cfg.local_engine.vad_kind, "auto",
            "缺失 vad_kind 时应安全迁移为 auto"
        );
    }

    /// 显式设置 `vad_kind = "energy"` 的配置应保留该值。
    #[test]
    fn explicit_vad_kind_energy_preserved() {
        let json = r#"{"enabled":true,"mode":"local","local_engine":{"vad_kind":"energy"}}"#;
        let cfg: SttConfig = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.local_engine.vad_kind, "energy");
    }

    /// 显式设置 `vad_kind = "fsmn"` 的配置应保留该值。
    #[test]
    fn explicit_vad_kind_fsmn_preserved() {
        let json = r#"{"enabled":true,"mode":"local","local_engine":{"vad_kind":"fsmn"}}"#;
        let cfg: SttConfig = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.local_engine.vad_kind, "fsmn");
    }




    /// 旧配置缺失整个 `vad` 对象时安全迁移为默认值，不算定制。
    #[test]
    fn missing_vad_object_migrates_safely() {
        let json = r#"{"enabled":true,"mode":"local","local_engine":{"server_port":8000,"funasr_model":"gguf/sensevoice-small-q8","device":"cpu"}}"#;
        let cfg: SttConfig = serde_json::from_str(json).unwrap();
        // vad 缺失 → 默认值
        assert_eq!(cfg.local_engine.vad.silence_threshold, 0.005);
        assert_eq!(cfg.local_engine.vad.min_silence_ms, 300);
        assert_eq!(cfg.local_engine.vad.min_sentence_ms, 800);
        // vad_kind 也缺失 → auto
        assert_eq!(cfg.local_engine.vad_kind, "auto");
    }

    /// round-trip 保持 `vad_kind` 字段。
    #[test]
    fn round_trip_preserves_vad_kind() {
        let cfg = SttConfig {
            enabled: true,
            mode: SttMode::Local,
            local_engine: LocalEngineConfig {
                vad_kind: "fsmn".into(),
                ..Default::default()
            },
            ..Default::default()
        };
        let json = serde_json::to_string(&cfg).unwrap();
        let restored: SttConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.local_engine.vad_kind, "fsmn");
    }
}
