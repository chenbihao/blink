//! FunASR GGUF 常驻 worker 的模型目录、二进制定位与启动构造（0.22.7）。
//!
//! 目标拓扑（phase 0.22 §5.8.2）：
//! `PseudoStreamingSttEngine → 既有 funasr adapter → NDJSON worker client →
//!  llama.cpp/GGUF native worker → 当前选中的一个模型`。
//!
//! ## 供应链锁定（2026-08-30 核实，运行期不跟随 main 漂移）
//!
//! - runtime：FunASR `runtime-llamacpp-v0.2.6`（commit `55b662c...`，
//!   llama.cpp pin `803b7fc...`，MIT）。worker 由 `cargo xtask funasr-worker`
//!   从锁定源码 + Blink 补丁构建，SHA-256 记录在随发布的
//!   `resources/bin/funasr-worker/manifest.json`（安装期校验）。
//! - 模型：HuggingFace `FunAudioLLM/*-GGUF` 官方发布文件，URL/SHA-256
//!   在此编译期锁定（见 `gguf_model_specs`）。
//! - 不使用 VAD 子模型：Blink 伪流式在客户端做能量 VAD 切句并发送裁剪后
//!   快照，worker 侧 SenseVoice/Paraformer/Nano 单窗口推理即可覆盖；
//!   因此 fsmn-vad.gguf 不进入模型资产（phase §5.8.3 决策）。

use std::collections::HashMap;

use crate::domain::local_engine::{
    AdapterConfig, ErrorPhase, LaunchContext, LaunchDescriptor, LocalEngineError,
    LocalEngineErrorCode,
};
use crate::infra::local_engine::model_storage as mstore;
use crate::infra::local_engine::runtime::EngineId;

use super::FUNASR_ENGINE_ID;

// ── 模型目录（编译期 allowlist）──────────────────────────────────────────

/// 单个 GGUF 发布文件的锁定下载描述。
#[derive(Debug, Clone)]
pub struct GgufFileSpec {
    /// payload 内的文件名。
    pub file_name: &'static str,
    /// 锁定下载 URL（HuggingFace resolve 直链）。
    pub url: String,
    /// 锁定 SHA-256（小写 hex）。
    pub sha256: String,
    /// 文件字节数（预计体积展示用）。
    pub size_bytes: u64,
}

/// 单个 GGUF 模型候选。
#[derive(Debug, Clone)]
pub struct GgufModelSpec {
    pub model_id: &'static str,
    pub display_name: &'static str,
    pub description: &'static str,
    /// 模型 revision（与 runtime release 绑定的稳定标识）。
    pub revision: &'static str,
    /// worker 可执行文件名（随发布 bundle）。
    pub worker_exe: &'static str,
    /// 该模型必需的 GGUF 文件。
    pub files: Vec<GgufFileSpec>,
    /// per-model STT 能力声明（Handoff 02：稳定产品契约）。
    pub stt_capabilities: crate::domain::local_engine::SttModelCapabilities,
}

/// SenseVoice Small Q8（五语种，CPU 首选，内置 ITN/标点）。
/// 模型 id 真源在 domain 配置层（stt_config GGUF_*_MODEL_ID），此处为
/// app 层稳定别名——配置迁移与模型目录共享同一映射，避免两份表漂移。
pub const GGUF_SENSEVOICE_ID: &str = crate::domain::config::stt_config::GGUF_SENSEVOICE_MODEL_ID;
/// Paraformer-zh Q8（中文，非自回归；GGUF 版无热词/标点）。
pub const GGUF_PARAFORMER_ID: &str = crate::domain::config::stt_config::GGUF_PARAFORMER_MODEL_ID;
/// Fun-ASR-Nano Q4_K_M（LLM 自回归 + encoder，KV 每请求清空）。
pub const GGUF_NANO_ID: &str = crate::domain::config::stt_config::GGUF_NANO_MODEL_ID;

/// 模型 revision：与 runtime release 绑定（模型文件由该 runtime 首次发布）。
pub const GGUF_MODEL_REVISION: &str = "gguf-v0.2.6";

/// 校验模型 URL 不使用浮动 ref（`/resolve/main/`）。
///
/// 浮动 ref 指向仓库默认分支的最新提交，上游更新文件后会导致：
/// - SHA-256 不匹配 → 安装失败（但错误信息可能不明确）
/// - 无法复现下载（同一 URL 在不同时间返回不同文件）
///
/// release-check 拒绝任何包含 `/resolve/main/` 的 URL。
/// 正确做法是使用 `/resolve/<commit-sha>/` 固定到不可变 revision。
#[allow(dead_code)] // 预留校验入口，release-check 同规则消费
pub(crate) fn validate_model_url_stable(url: &str) -> Result<(), String> {
    if url.contains("/resolve/main/") {
        return Err(format!(
            "模型 URL 使用浮动 ref（resolve/main）：{url}。\
             请替换为 /resolve/<commit-sha>/ 固定到不可变 revision。"
        ));
    }
    Ok(())
}

/// GGUF 模型目录（编译期锁定）。
pub fn gguf_model_specs() -> &'static [GgufModelSpec] {
    use std::sync::OnceLock;
    static SPECS: OnceLock<Vec<GgufModelSpec>> = OnceLock::new();
    SPECS.get_or_init(|| {
        vec![
            GgufModelSpec {
                model_id: GGUF_SENSEVOICE_ID,
                display_name: "SenseVoice Small (GGUF Q8)",
                description: "五语种 ASR（中/英/日/韩/粤），llama.cpp 常驻，CPU 首选",
                revision: GGUF_MODEL_REVISION,
                worker_exe: "funasr-sensevoice-worker.exe",
                files: vec![GgufFileSpec {
                    file_name: "sensevoice-small-q8.gguf",
                    url: "https://huggingface.co/FunAudioLLM/SenseVoiceSmall-GGUF/resolve/90c1c61912018b70ada0fcc024ea24aca62f2e63/sensevoice-small-q8.gguf".to_string(),
                    sha256: "4ae45c94422de949b387e2e0fb10d7e14e4c42c69db30c3444ecc7d4b844b7c5".to_string(),
                    size_bytes: 254_208_320,
                }],
                // SenseVoice 能力矩阵（证据来源：0.22.7.1 spike 实测 + FunASR 上游文档）
                // - languages: 五语种内置（中/英/日/韩/粤），上游文档明确
                // - pseudo_streaming: PseudoStreamingSttEngine 对所有模型可用
                // - true_streaming: 非自回归模型，无增量 encoder
                // - timestamps: worker NDJSON 协议 TranscribeOptions 未开放
                // 0.22.7 契约收口：hotwords/itn 已删除（GGUF worker 不消费）
                stt_capabilities: crate::domain::local_engine::SttModelCapabilities {
                    languages: vec!["zh".into(), "en".into(), "ja".into(), "ko".into(), "yue".into()],
                    pseudo_streaming: crate::domain::local_engine::CapabilityFlag::yes(),
                    true_streaming: crate::domain::local_engine::CapabilityFlag::no("stt.capability.streaming.no_incremental_encoder"),
                    timestamps: crate::domain::local_engine::CapabilityFlag::no("stt.capability.timestamps.not_exposed"),
                },
            },
            GgufModelSpec {
                model_id: GGUF_PARAFORMER_ID,
                display_name: "Paraformer-zh (GGUF Q8)",
                description: "中文 ASR（非自回归），llama.cpp 常驻；GGUF 版无热词",
                revision: GGUF_MODEL_REVISION,
                worker_exe: "funasr-paraformer-worker.exe",
                files: vec![GgufFileSpec {
                    file_name: "paraformer-q8.gguf",
                    url: "https://huggingface.co/FunAudioLLM/Paraformer-GGUF/resolve/1a5063b305a2b4e418ccffaf7be2c02a3cac6c89/paraformer-q8.gguf".to_string(),
                    sha256: "42bf76ea1575a336aaca4c1b7c01a82b79113e6d04d0d6b799561bfcf07ee011".to_string(),
                    size_bytes: 236_929_024,
                }],
                // Paraformer 能力矩阵（证据来源：0.22.7.3 spike 实测）
                // - languages: 中文专用模型（实测英文单词无空格拼接）
                // - pseudo_streaming: 可用（粗粒度，非自回归延迟低）
                // - true_streaming: 非自回归，无增量 encoder
                // 0.22.7 契约收口：hotwords/itn 已删除（GGUF worker 不消费）
                stt_capabilities: crate::domain::local_engine::SttModelCapabilities {
                    languages: vec!["zh".into()],
                    pseudo_streaming: crate::domain::local_engine::CapabilityFlag::yes(),
                    true_streaming: crate::domain::local_engine::CapabilityFlag::no("stt.capability.streaming.no_incremental_encoder"),
                    timestamps: crate::domain::local_engine::CapabilityFlag::no("stt.capability.timestamps.not_exposed"),
                },
            },
            GgufModelSpec {
                model_id: GGUF_NANO_ID,
                display_name: "Fun-ASR-Nano (Q4_K_M)",
                description: "LLM 自回归 ASR（encoder + Qwen3-0.6B），延迟高于 SenseVoice",
                revision: GGUF_MODEL_REVISION,
                worker_exe: "funasr-nano-worker.exe",
                files: vec![
                    GgufFileSpec {
                        file_name: "funasr-encoder-f16.gguf",
                        url: "https://huggingface.co/FunAudioLLM/Fun-ASR-Nano-GGUF/resolve/46e849502a867080d66d351b8dfb1018b607e509/funasr-encoder-f16.gguf".to_string(),
                        sha256: "f92f91d01a24fbed6c863495b2ee8c6a6788144a02858b75743f0946668de8a2".to_string(),
                        size_bytes: 469_331_008,
                    },
                    GgufFileSpec {
                        file_name: "qwen3-0.6b-q4km.gguf",
                        url: "https://huggingface.co/FunAudioLLM/Fun-ASR-Nano-GGUF/resolve/46e849502a867080d66d351b8dfb1018b607e509/qwen3-0.6b-q4km.gguf".to_string(),
                        sha256: "cc5057552aa9dddedcda73ea8889854e8a257eb07d0a561b7234465c1e856f22".to_string(),
                        size_bytes: 484_219_776,
                    },
                ],
                // Nano 能力矩阵（证据来源：0.22.7.3 spike 实测）
                // - languages: 中文为主（自回归 LLM，多语言能力未验证）
                // - pseudo_streaming: 可用但粗粒度（自回归延迟显著高于 SenseVoice）
                // - true_streaming: 自回归但 KV 每请求清空，非增量
                // 0.22.7 契约收口：hotwords/itn 已删除（GGUF worker 不消费）
                stt_capabilities: crate::domain::local_engine::SttModelCapabilities {
                    languages: vec!["zh".into()],
                    pseudo_streaming: crate::domain::local_engine::CapabilityFlag::yes(),
                    true_streaming: crate::domain::local_engine::CapabilityFlag::no("stt.capability.streaming.kv_cleared_per_request"),
                    timestamps: crate::domain::local_engine::CapabilityFlag::no("stt.capability.timestamps.not_exposed"),
                },
            },
        ]
    })
}

// 供应链锁定实测值（2026-08-30 从锁定 URL 下载并计算 SHA-256）：
// - sensevoice-small-q8.gguf: 254,208,320 B（spike 下载）
// - paraformer-q8.gguf: 236,929,024 B
// - funasr-encoder-f16.gguf: 469,331,008 B
// - qwen3-0.6b-q4km.gguf: 484,219,776 B

/// 查找 GGUF 模型 spec。
pub fn find_gguf_spec(model_id: &str) -> Option<&'static GgufModelSpec> {
    gguf_model_specs().iter().find(|m| m.model_id == model_id)
}

/// 把 GGUF 模型 spec 投影为领域模型 descriptor。
pub fn gguf_model_descriptor(
    spec: &GgufModelSpec,
) -> crate::domain::local_engine::EngineModelDescriptor {
    let total_bytes: u64 = spec.files.iter().map(|f| f.size_bytes).sum();
    crate::domain::local_engine::EngineModelDescriptor {
        engine_id: EngineId::new(FUNASR_ENGINE_ID).expect("funasr is valid"),
        model_id: spec.model_id.to_string(),
        display_name: spec.display_name.to_string(),
        description: spec.description.to_string(),
        revision: spec.revision.to_string(),
        checksum_source: crate::infra::local_engine::runtime::ChecksumSource::Sha256(
            spec.files
                .iter()
                .map(|f| f.sha256.clone())
                .collect::<Vec<_>>()
                .join("+"),
        ),
        estimated_size_mb: Some(total_bytes / (1024 * 1024)),
        compatibility_schema: crate::infra::local_engine::worker_proto::WORKER_PROTOCOL_VERSION,
        stt_capabilities: spec.stt_capabilities.clone(),
    }
}

/// 旧模型 id → 新 GGUF 模型 id 的确定迁移映射（0.22.7.3/4 配置迁移用）。
#[allow(dead_code)] // 0.22.7.3 配置迁移接线后消费
pub fn migrate_legacy_model_id(legacy: &str) -> Option<&'static str> {
    // 单一真源在 domain 配置层（stt_config::legacy_model_to_gguf_id）——
    // 配置迁移与模型目录共享同一映射，避免两份表漂移。
    crate::domain::config::stt_config::legacy_model_to_gguf_id(legacy)
}

// ── worker 二进制定位 ────────────────────────────────────────────────────
//
// 捆绑资源定位与 manifest hash 校验的唯一实现在
// `infra/local_engine/providers/binary.rs`（安装事务用）。
// adapter 侧 self-test 只做 active deployment 的结构检查（部署存在 + exe 在），
// 完整 hash 校验发生在安装事务（candidate 内一次性执行）。

/// GGUF 环境 self-test：active deployment 存在且 worker exe 就位。
///
/// 返回 Err(reason) 时附带给用户的可行动指引。
pub fn gguf_environment_self_test() -> Result<(), String> {
    let engine_id = EngineId::new(FUNASR_ENGINE_ID).expect("funasr is valid");
    let Some((_pointer, dir)) =
        crate::infra::local_engine::deployment::DeploymentStore::active_dir(&engine_id)
            .ok()
            .flatten()
    else {
        return Err(
            "FunASR GGUF runtime 未安装。请在设置页「引擎」→「本地模型运行时」中点击「安装环境」。"
                .to_string(),
        );
    };
    let probe = dir.join("funasr-sensevoice-worker.exe");
    if !probe.is_file() {
        return Err(format!(
            "GGUF worker 部署不完整（缺少 {}）。请点击「修复」重建环境。",
            probe.display()
        ));
    }
    Ok(())
}

// ── 启动构造 ─────────────────────────────────────────────────────────────

/// 构建 GGUF worker 的 `LaunchDescriptor`（0.22.7）。
///
/// 从 active deployment slot 取 worker exe，从 model_storage payload 取
/// GGUF 路径；身份经环境变量注入（与旧 Python server 同一约定）。
pub fn build_funasr_gguf_launch_descriptor(
    config: &super::launch::FunasrEngineConfig,
    _adapter_config: &AdapterConfig,
    ctx: &LaunchContext,
) -> Result<LaunchDescriptor, LocalEngineError> {
    let engine_id = EngineId::new(FUNASR_ENGINE_ID).map_err(|e| {
        LocalEngineError::with_detail(
            LocalEngineErrorCode::Internal,
            ErrorPhase::Start,
            "engine_id 无效",
            format!("解析 engine_id 失败: {e}"),
        )
    })?;

    // 1. active deployment 中的 worker exe
    let (_pointer, deployment_dir) =
        crate::infra::local_engine::deployment::DeploymentStore::active_dir(&engine_id)
            .ok()
            .flatten()
            .ok_or_else(|| {
                LocalEngineError::with_detail(
                    LocalEngineErrorCode::EnvironmentMissing,
                    ErrorPhase::Start,
                    "GGUF worker 未安装",
                    "FunASR GGUF runtime 未安装。请在设置页「引擎」→「本地模型运行时」中点击「安装环境」。",
                )
            })?;

    // 2. 选中模型 → spec → payload 目录
    let model_id = &config.funasr_model;
    let spec = find_gguf_spec(model_id).ok_or_else(|| {
        LocalEngineError::with_detail(
            LocalEngineErrorCode::ModelNotReady,
            ErrorPhase::Start,
            "未知 GGUF 模型",
            format!("model_id '{model_id}' 不在 GGUF 模型目录中"),
        )
    })?;
    let asset_key = mstore::encode_asset_key(model_id);
    let (manifest, payload_dir) = match mstore::restore_model_state(&engine_id, &asset_key) {
        Ok(mstore::RestoredModelState::Installed { manifest, slot_id }) => {
            let payload_dir =
                mstore::model_payload_dir(&engine_id, &asset_key, &slot_id).map_err(|e| {
                    LocalEngineError::with_detail(
                        LocalEngineErrorCode::Internal,
                        ErrorPhase::Start,
                        "payload 目录解析失败",
                        format!("{e}"),
                    )
                })?;
            (manifest, payload_dir)
        }
        _ => {
            return Err(LocalEngineError::with_detail(
                LocalEngineErrorCode::ModelNotReady,
                ErrorPhase::Start,
                "模型未安装",
                format!("GGUF 模型 '{model_id}' 未安装或状态损坏。请先在引擎页安装模型。"),
            ));
        }
    };

    // 3. 校验 payload 文件齐备
    let mut missing = Vec::new();
    for f in &spec.files {
        if !payload_dir.join(f.file_name).is_file() {
            missing.push(f.file_name);
        }
    }
    if !missing.is_empty() {
        return Err(LocalEngineError::with_detail(
            LocalEngineErrorCode::ModelNotReady,
            ErrorPhase::Start,
            "模型文件缺失",
            format!("payload 缺少文件: {}", missing.join(", ")),
        ));
    }

    // 4. 组装 argv（模型特定：sensevoice/paraformer 单 -m；nano 双文件）
    let exe = deployment_dir.join(spec.worker_exe);
    if !exe.is_file() {
        return Err(LocalEngineError::with_detail(
            LocalEngineErrorCode::EnvironmentMissing,
            ErrorPhase::Start,
            "worker 可执行文件缺失",
            format!("部署中缺少 {}（安装损坏？请修复环境）", spec.worker_exe),
        ));
    }
    let mut args: Vec<String> = Vec::new();
    match spec.model_id {
        GGUF_NANO_ID => {
            args.push("--enc".to_string());
            args.push(
                payload_dir
                    .join("funasr-encoder-f16.gguf")
                    .display()
                    .to_string(),
            );
            args.push("-m".to_string());
            args.push(
                payload_dir
                    .join("qwen3-0.6b-q4km.gguf")
                    .display()
                    .to_string(),
            );
        }
        _ => {
            args.push("-m".to_string());
            args.push(
                payload_dir
                    .join(spec.files[0].file_name)
                    .display()
                    .to_string(),
            );
        }
    }
    args.push("--stdin-server".to_string());

    // 5. 受限环境变量（身份注入约定与旧 Python server 一致；worker 回显校验）
    let mut env = HashMap::new();
    env.insert("BLINK_ENGINE_ID".to_string(), ctx.engine_id.clone());
    env.insert("BLINK_INSTANCE_ID".to_string(), ctx.instance_id.clone());
    env.insert("BLINK_ENGINE_TOKEN".to_string(), ctx.token.clone());
    env.insert("BLINK_MODEL_ID".to_string(), manifest.model_id.clone());
    env.insert(
        "BLINK_MODEL_REVISION".to_string(),
        manifest.revision.clone(),
    );
    env.insert(
        "BLINK_MODEL_PAYLOAD_DIR".to_string(),
        payload_dir.display().to_string(),
    );
    env.insert(
        "BLINK_AUDIO_DIR".to_string(),
        super::worker::engine_audio_tmp_dir(&engine_id)
            .display()
            .to_string(),
    );
    if let Some(threads) = config.num_threads {
        env.insert("BLINK_WORKER_THREADS".to_string(), threads.to_string());
    }

    // 音频目录就绪（worker 侧路径校验以前缀匹配，目录必须存在）
    if let Err(e) = std::fs::create_dir_all(super::worker::engine_audio_tmp_dir(&engine_id)) {
        return Err(LocalEngineError::with_detail(
            LocalEngineErrorCode::Internal,
            ErrorPhase::Start,
            "音频目录创建失败",
            format!("{e}"),
        ));
    }

    tracing::info!(
        worker = %spec.worker_exe,
        model = %manifest.model_id,
        revision = %manifest.revision,
        transport = "stdio",
        "构建 FunASR GGUF LaunchDescriptor"
    );

    Ok(LaunchDescriptor {
        executable: exe,
        args,
        current_dir: None,
        env,
        label: FUNASR_ENGINE_ID.to_string(),
    })
}

// ── 测试 ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_model_url_rejects_resolve_main() {
        let url = "https://huggingface.co/FunAudioLLM/SenseVoiceSmall-GGUF/resolve/main/sensevoice-small-q8.gguf";
        assert!(validate_model_url_stable(url).is_err());
    }

    #[test]
    fn validate_model_url_accepts_fixed_revision() {
        let url = "https://huggingface.co/FunAudioLLM/SenseVoiceSmall-GGUF/resolve/abc123def/sensevoice-small-q8.gguf";
        assert!(validate_model_url_stable(url).is_ok());
    }

    /// 所有模型 URL 已固定到上游 commit SHA——不使用 resolve/main 浮动 ref。
    #[test]
    fn current_model_urls_use_fixed_revision() {
        for spec in gguf_model_specs() {
            for file in &spec.files {
                assert!(
                    validate_model_url_stable(&file.url).is_ok(),
                    "URL {url} 仍使用 resolve/main 浮动 ref",
                    url = file.url
                );
            }
        }
    }

    #[test]
    fn model_specs_have_consistent_revision() {
        for spec in gguf_model_specs() {
            assert_eq!(
                spec.revision, GGUF_MODEL_REVISION,
                "model {} revision 不一致",
                spec.model_id
            );
        }
    }

    #[test]
    fn model_specs_have_sha256() {
        for spec in gguf_model_specs() {
            for file in &spec.files {
                assert!(
                    !file.sha256.is_empty(),
                    "model {} file {} 缺少 SHA-256",
                    spec.model_id,
                    file.file_name
                );
                assert_eq!(
                    file.sha256.len(),
                    64,
                    "model {} file {} SHA-256 长度异常",
                    spec.model_id,
                    file.file_name
                );
            }
        }
    }
}
