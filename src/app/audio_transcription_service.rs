//! 一次性文件转写编排服务（0.22.16 Handoff 04）。
//!
//! `AudioTranscriptionService` 是唯一的 app 层转写编排器：
//! 消费 `audio_ref` → 解析资源 → 冻结 STT 身份 → 阻塞池解码规范化 →
//! 调用现有 transport → 返回稳定结构化结果。
//!
//! **设计约束**：
//! - 禁止调用 `VoiceService`；禁止新建 worker/executor。
//! - CPU 密集解码/规范化不在 async worker 上裸跑——走 `spawn_blocking`。
//! - 日志/错误/断言不含音频字节、绝对路径、私有文件名或转写全文。
//! - deadline/cancel 后的迟到结果只记分类并丢弃。
//! - 切模或重启后旧结果不得成功投影（冻结的 model/instance 二次验证）。
//!
//! **分层**：app 层模块，消费 `domain::stt::transcribe`、
//! `domain::resource`（统一 ResourceStore）、`infra::platform::audio` 和 `app::local_engine`。

use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;

use crate::domain::config::stt_config::{LocalSttSelection, SttMode, get_stt_config};
use crate::domain::resource::{
    DefaultResourceStore, ResourceError, ResourceErrorKind, ResourceRef, ResourceUse,
};
use crate::domain::stt::pseudo_streaming::{
    CompositeDraftDetail, CompositeSpanDetail, PseudoStreamingSttEngine, SttBoundaryRecord,
    SttCompositeRecord, SttDecisionRecord,
};
use crate::domain::stt::transcribe::{
    AudioTranscriptionError, AudioTranscriptionPort, AudioTranscriptionRequest,
    AudioTranscriptionResult,
};
use crate::domain::stt::vad::EnergyVad;
use crate::domain::stt::vad_diagnostics::{VadTrace, trace_vad};
use crate::domain::stt::{AudioRange, DraftSpan};
use crate::domain::stt::{SttEngine, SttEngineConnection};
use crate::domain::stt::{SttTransport, SttTransportError};
use crate::infra::platform::audio::format::AudioDecodeError;
use crate::infra::platform::audio::normalize::{AudioNormalizer, NormalizationSummary};
use crate::infra::platform::audio::wav::decode_wav_with_budget;

// ── 配置 ─────────────────────────────────────────────────────────────────

/// 转写服务的配置参数。
///
/// 预算属于实现/config 参数，按真实负载调整，不在规范中固化。
#[derive(Debug, Clone)]
pub struct TranscriptionConfig {
    /// 解码后 f32 样本数上限（含声道维度）。
    /// 60 秒 × 96kHz × 8 声道 ≈ 46M；默认 50M 覆盖常见 WAV。
    pub sample_budget: usize,
    /// 音频时长上限（秒）——超出返回 `AudioBudgetExceeded`。
    pub max_duration_secs: f64,
}

impl Default for TranscriptionConfig {
    fn default() -> Self {
        Self {
            sample_budget: 50_000_000,
            max_duration_secs: 600.0, // 10 分钟
        }
    }
}

// ── Engine snapshot seam ─────────────────────────────────────────────────

/// 冻结的引擎身份快照——在转写开始时冻结，结束时二次验证。
///
/// 包含足以判断"引擎是否被切换或重启"的信息：
/// engine_id、model_id、instance_id。
#[derive(Debug, Clone)]
pub struct FrozenEngineIdentity {
    pub engine_id: String,
    pub model_id: String,
    pub instance_id: String,
}

// ── Port seams ───────────────────────────────────────────────────────────

/// 引擎连接获取 seam——抽象 `EngineManager::get_connection`。
///
/// 生产实现由 `EngineManager` 提供；测试用 fake 实现。
#[async_trait]
pub trait EngineConnectionPort: Send + Sync {
    /// 获取当前运行实例的 transport + 身份快照。
    ///
    /// 返回 `None` 表示引擎未运行。
    /// 返回 `Some(transport, identity)` 表示已就绪。
    async fn get_stt_connection(
        &self,
        frozen_selection: &LocalSttSelection,
    ) -> Result<Option<(Arc<dyn SttTransport>, FrozenEngineIdentity)>, String>;
}

/// 云端转写授权 seam——检查云端外发是否已获用户动态确认。
///
/// 生产实现由设置页/授权流程提供；测试用 fake 实现。
pub trait CloudEgressAuthorizer: Send + Sync {
    /// 检查给定供应商的云端转写是否已获动态外发确认。
    ///
    /// 返回 `true` 表示已授权，可以发起网络请求。
    /// 当前文件转写尚未实现云端执行，因此无论授权状态如何都优先返回
    /// `Unsupported`；此 seam 仅为未来实现保留，任何路径都不得偷偷切换本地模型。
    fn is_authorized(&self, provider_kind: &str) -> bool;
}

// ── AudioTranscriptionService ───────────────────────────────────────────

/// 一次性文件转写编排服务。
///
/// 编排顺序（Handoff 04 要求）：
/// 1. 检查 deadline
/// 2. resolve audio_ref 并持有已打开资源
/// 3. 冻结 STT config、engine/model/instance 身份
/// 4. 验证已配置、已安装且当前可用；不启动、安装、下载或切换
/// 5. 在 blocking pool 中有界读取、decode、normalize
/// 6. 再检查 deadline 和被冻结身份
/// 7. 编码成 canonical 16k mono PCM16 WAV，调用现有唯一 transport
/// 8. 返回前再次验证 model/instance
///
/// deadline/cancel 后的迟到结果只记分类并丢弃。
pub struct AudioTranscriptionService {
    store: Arc<DefaultResourceStore>,
    engine_conn: Arc<dyn EngineConnectionPort>,
    cloud_auth: Arc<dyn CloudEgressAuthorizer>,
    config: TranscriptionConfig,
}

/// 设置页 WAV 伪流式诊断结果。正文只返回给主动发起的本地设置页，不写日志。
#[derive(Debug, serde::Serialize)]
pub struct VadDebugResult {
    pub duration_ms: u64,
    pub min_sentence_ms: u32,
    pub wall_ms: u64,
    pub finalize_ms: u64,
    pub engine_id: String,
    pub model_id: String,
    pub final_text: String,
    pub trace: VadTrace,
    pub boundaries: Vec<SttBoundaryRecord>,
    pub decisions: Vec<SttDecisionRecord>,
    pub text_events: Vec<VadDebugTextEvent>,
    pub commits: Vec<VadDebugCommit>,
    /// 0.23.16.1 组合预览时间线（边沿记录，墙钟换算后）。
    pub composite: Vec<VadDebugComposite>,
}

#[derive(Debug, serde::Serialize)]
pub struct VadDebugTextEvent {
    pub kind: &'static str,
    pub fed_ms: u64,
    pub wall_ms: u64,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_id: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub span_id: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revision: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audio_range: Option<AudioRange>,
}

#[derive(Debug, serde::Serialize)]
pub struct VadDebugCommit {
    pub audio_ms: u64,
    pub observed_wall_ms: u64,
}

/// 0.23.16.1 组合预览时间线的一条边沿记录（墙钟已换算）。
///
/// 组合预览 = 短语账本 + 尾部的拼接字符串，即用户在 G2 浮窗实际看到的
/// 预览文本。`retreat_chars > 0` 即一次可见回退，`cause` 标明归因。
#[derive(Debug, serde::Serialize)]
pub struct VadDebugComposite {
    pub cause: &'static str,
    pub fed_ms: u64,
    pub wall_ms: u64,
    pub text: String,
    pub retreat_chars: u64,
    pub spans: usize,
    /// 0.23.16.6：变化后的组成段明细（phrase/tail + 音频范围 + 文本）。
    pub spans_detail: Vec<CompositeSpanDetail>,
    /// 0.23.16.6：SettleCommit 时刚提交的 Draft 段（其他归因为 None）。
    pub draft: Option<CompositeDraftDetail>,
}

impl AudioTranscriptionService {
    /// 构造转写服务。
    ///
    /// 所有依赖通过 trait object 注入，便于测试替换。
    /// `store` 必须与 picker 命令共享同一实例（ref 跨 command/service 流通）。
    pub fn new(
        store: Arc<DefaultResourceStore>,
        engine_conn: Arc<dyn EngineConnectionPort>,
        cloud_auth: Arc<dyn CloudEgressAuthorizer>,
        config: TranscriptionConfig,
    ) -> Self {
        Self {
            store,
            engine_conn,
            cloud_auth,
            config,
        }
    }

    /// 同一可信 WAV 走真实伪流式引擎按 10ms 实时喂入；一次性转写入口保持独立。
    pub async fn debug_vad(
        &self,
        audio_ref: &str,
        progress: impl Fn(&'static str, u64, u64) + Send + Sync,
    ) -> Result<VadDebugResult, AudioTranscriptionError> {
        progress("preparing", 0, 0);
        let mut opened = self.resolve_audio_ref(audio_ref, ResourceUse::TranscribeAudio)?;
        let (transport, frozen) =
            self.freeze_identity()
                .await?
                .ok_or_else(|| AudioTranscriptionError::Unsupported {
                    detail: "VAD debug requires a running local STT engine".into(),
                })?;
        let stt_config = get_stt_config();
        let vad_config = stt_config.local_engine.vad.clone();
        let min_sentence_ms = vad_config.min_sentence_ms;
        let decode_config = self.config.clone();
        let (samples, trace) = tokio::task::spawn_blocking(move || {
            let bytes = Self::read_opened_bounded(&mut opened)?;
            let (samples, _, _) = Self::decode_and_normalize_with_config(&bytes, &decode_config)?;
            if samples.len() > 120 * 16_000 {
                return Err(AudioTranscriptionError::AudioBudgetExceeded {
                    detail: "VAD debug accepts WAV files up to 120 seconds".into(),
                });
            }
            let mut vad = EnergyVad::with_params_and_windows(
                16_000,
                vad_config.silence_threshold,
                vad_config.min_silence_ms,
                vad_config.min_sentence_ms,
                vad_config.soft_window_ms(),
                vad_config.hard_window_ms(),
            );
            let trace = trace_vad(&samples, 16_000, &mut vad);
            Ok::<_, AudioTranscriptionError>((samples, trace))
        })
        .await
        .map_err(|error| AudioTranscriptionError::Internal {
            detail: format!("VAD debug decode task failed: {error}"),
        })??;

        self.verify_identity(&frozen).await?;
        let duration_ms = trace.duration_ms;
        progress("replaying", 0, duration_ms);
        let boundaries = Arc::new(std::sync::Mutex::new(Vec::new()));
        let decisions = Arc::new(std::sync::Mutex::new(Vec::new()));
        let composites: Arc<std::sync::Mutex<Vec<SttCompositeRecord>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let connection = SttEngineConnection {
            host: "127.0.0.1".into(),
            port: 0,
            engine_id: frozen.engine_id.clone(),
            instance_id: frozen.instance_id.clone(),
            transport: Some(transport),
        };
        let engine = PseudoStreamingSttEngine::from_connection(&stt_config, connection)
            .map_err(|detail| AudioTranscriptionError::Internal { detail })?
            .with_boundary_observer(Arc::clone(&boundaries))
            .with_decision_observer(Arc::clone(&decisions))
            .with_composite_observer(Arc::clone(&composites));
        // 0.23.10.2：回放同样预热 worker——VAD 调试页测得的"首预览 4s+"
        // 有相当部分是闲置 worker 的首次推理懒加载，预热后回放与真机
        // 的首预览延迟口径才一致。
        engine.warm_up_worker();
        let started = Instant::now();
        let mut text_events = Vec::new();
        let mut commits = Vec::new();
        let mut last_preview = String::new();
        let mut last_confirmed = String::new();
        let mut last_confirmed_revision = 0u64;
        let mut fed = 0usize;

        for chunk in samples.chunks(160) {
            let response = engine.transcribe_chunk(chunk).await.map_err(|error| {
                AudioTranscriptionError::Internal {
                    detail: error.to_string(),
                }
            })?;
            fed += chunk.len();
            let fed_ms = fed as u64 * 1000 / 16_000;
            collect_vad_debug_text(
                &response,
                fed_ms,
                started.elapsed().as_millis() as u64,
                &mut last_preview,
                &mut last_confirmed,
                &mut text_events,
            );
            tokio::time::sleep_until(
                tokio::time::Instant::from_std(started) + std::time::Duration::from_millis(fed_ms),
            )
            .await;
            if fed_ms % 250 == 0 || fed == samples.len() {
                progress("replaying", fed_ms, duration_ms);
            }
            let stats = engine.stream_stats();
            if stats.confirmed_revision > last_confirmed_revision {
                commits.push(VadDebugCommit {
                    audio_ms: stats.pcm_committed_end as u64 * 1000 / 16_000,
                    observed_wall_ms: started.elapsed().as_millis() as u64,
                });
                last_confirmed_revision = stats.confirmed_revision;
            }
        }
        progress("finalizing", duration_ms, duration_ms);
        let finalize_started = Instant::now();
        let final_text =
            engine
                .finalize()
                .await
                .map_err(|error| AudioTranscriptionError::Internal {
                    detail: error.to_string(),
                })?;
        let finalize_ms = finalize_started.elapsed().as_millis() as u64;
        self.verify_identity(&frozen).await?;
        // 尾段 Draft 由 terminal finalize 直接写入 ledger，不经过 chunk 响应；
        // 最后一个 chunk 之后才完成的句段同样如此。这里显式补收，
        // 时间轴才能像 final_text 一样看到收尾草稿段。
        let finalize_wall_ms = started.elapsed().as_millis() as u64;
        for span in engine.drain_unreported_draft_spans() {
            if span.text.is_empty() {
                continue;
            }
            text_events.push(VadDebugTextEvent {
                kind: "draft",
                fed_ms: span.audio_range.end_sample * 1000 / 16_000,
                wall_ms: finalize_wall_ms,
                text: span.text,
                request_id: None,
                span_id: Some(span.span_id),
                revision: Some(span.revision),
                audio_range: Some(span.audio_range),
            });
        }
        // finalize 期间的尾段提交也要记入 commits，末尾锚点才有"定稿"决策。
        if engine.stream_stats().confirmed_revision > last_confirmed_revision {
            commits.push(VadDebugCommit {
                audio_ms: duration_ms,
                observed_wall_ms: finalize_wall_ms,
            });
        }
        progress("done", duration_ms, duration_ms);
        let composite = composites
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .iter()
            .map(|record| VadDebugComposite {
                cause: record.cause,
                fed_ms: record.fed_ms,
                wall_ms: record
                    .observed_at
                    .duration_since(started)
                    .as_millis() as u64,
                text: record.text.clone(),
                retreat_chars: record.retreat_chars,
                spans: record.spans,
                spans_detail: record.spans_detail.clone(),
                draft: record.draft.clone(),
            })
            .collect();
        let result = VadDebugResult {
            duration_ms: trace.duration_ms,
            min_sentence_ms,
            wall_ms: started.elapsed().as_millis() as u64,
            finalize_ms,
            engine_id: frozen.engine_id,
            model_id: frozen.model_id,
            final_text,
            trace,
            boundaries: boundaries
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .clone(),
            decisions: decisions
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .clone(),
            text_events,
            commits,
            composite,
        };
        tracing::info!(
            duration_ms = result.duration_ms,
            boundaries = result.boundaries.len(),
            decisions = result.decisions.len(),
            commits = result.commits.len(),
            composite_changes = result.composite.len(),
            composite_retreats = result
                .composite
                .iter()
                .filter(|change| change.retreat_chars > 0)
                .count(),
            final_chars = result.final_text.chars().count(),
            "VAD WAV 伪流式诊断完成"
        );
        Ok(result)
    }

    /// 检查 deadline 是否已过期。
    fn check_deadline(&self, deadline: Option<Instant>) -> Result<(), AudioTranscriptionError> {
        if let Some(dl) = deadline
            && Instant::now() >= dl
        {
            tracing::debug!("transcribe: deadline already expired before start");
            return Err(AudioTranscriptionError::Timeout {
                detail: "deadline expired before start".into(),
            });
        }
        Ok(())
    }

    /// 解析 audio_ref，返回已打开的资源 lease（0.23.12：走统一 ResourceStore）。
    fn resolve_audio_ref(
        &self,
        audio_ref: &str,
        use_: ResourceUse,
    ) -> Result<crate::domain::resource::OpenedResource, AudioTranscriptionError> {
        self.store
            .open(&ResourceRef::from_token(audio_ref), use_)
            .map_err(map_resource_error)
    }

    /// 有界整读上限（沿用 0.22.16 read_file_bytes 的 256MB cap）。
    fn read_opened_bounded(
        opened: &mut crate::domain::resource::OpenedResource,
    ) -> Result<bytes::Bytes, AudioTranscriptionError> {
        const READ_BOUND_BYTES: u64 = 256 * 1024 * 1024;
        opened
            .read_all_bounded(READ_BOUND_BYTES)
            .map_err(|error| match error.kind {
                ResourceErrorKind::ResourceBudgetExceeded => {
                    AudioTranscriptionError::AudioBudgetExceeded {
                        detail: "resource exceeds read bound".into(),
                    }
                }
                _ => AudioTranscriptionError::Internal {
                    detail: "io error during bounded read".into(),
                },
            })
    }

    /// 冻结当前 STT 配置和引擎身份。
    async fn freeze_identity(
        &self,
    ) -> Result<Option<(Arc<dyn SttTransport>, FrozenEngineIdentity)>, AudioTranscriptionError>
    {
        let config = get_stt_config();

        // 验证已配置
        if !config.enabled {
            return Err(AudioTranscriptionError::SttNotConfigured);
        }

        match config.mode {
            SttMode::Local => {
                // 本地模式：需要 local_stt_selection
                let selection = config
                    .local_stt_selection
                    .as_ref()
                    .ok_or(AudioTranscriptionError::SttNotConfigured)?;

                // 获取连接——不启动、不安装
                let conn = self
                    .engine_conn
                    .get_stt_connection(selection)
                    .await
                    .map_err(|e| AudioTranscriptionError::Internal { detail: e })?;

                match conn {
                    Some((transport, identity)) => {
                        // 验证 engine_id 匹配
                        if identity.engine_id != selection.engine_id {
                            tracing::warn!(
                                frozen_engine = %identity.engine_id,
                                selected_engine = %selection.engine_id,
                                "transcribe: engine_id mismatch (可能切模中)"
                            );
                            return Err(AudioTranscriptionError::SttIdentityChanged {
                                detail: "engine_id mismatch between selection and running instance"
                                    .into(),
                            });
                        }
                        if identity.model_id != selection.model_id {
                            return Err(AudioTranscriptionError::SttIdentityChanged {
                                detail: "model_id mismatch between selection and running instance"
                                    .into(),
                            });
                        }
                        Ok(Some((transport, identity)))
                    }
                    None => Err(AudioTranscriptionError::SttBackendUnavailable {
                        detail: "local STT engine not running".into(),
                    }),
                }
            }
            SttMode::Cloud => {
                // 云端模式：检查授权
                let provider = config
                    .cloud_provider
                    .as_ref()
                    .ok_or(AudioTranscriptionError::SttNotConfigured)?;

                let _authorized = self.cloud_auth.is_authorized(&provider.kind);
                Err(AudioTranscriptionError::Unsupported {
                    detail: "cloud file transcription is not implemented".into(),
                })
            }
        }
    }

    /// 二次验证引擎身份是否未变。
    ///
    /// 切模或重启后旧结果不得成功投影。
    async fn verify_identity(
        &self,
        frozen: &FrozenEngineIdentity,
    ) -> Result<(), AudioTranscriptionError> {
        let conn = self
            .engine_conn
            .get_stt_connection(&LocalSttSelection {
                engine_id: frozen.engine_id.clone(),
                model_id: frozen.model_id.clone(),
            })
            .await
            .map_err(|e| AudioTranscriptionError::Internal { detail: e })?;

        match conn {
            Some((_transport, current)) => {
                if current.engine_id != frozen.engine_id
                    || current.instance_id != frozen.instance_id
                    || current.model_id != frozen.model_id
                {
                    tracing::warn!(
                        "transcribe: identity changed during execution (engine/model/instance mismatch)"
                    );
                    return Err(AudioTranscriptionError::SttIdentityChanged {
                        detail: "engine identity changed during transcription".into(),
                    });
                }
                Ok(())
            }
            None => {
                // 引擎在执行期间停止了
                Err(AudioTranscriptionError::SttBackendUnavailable {
                    detail: "engine stopped during transcription".into(),
                })
            }
        }
    }
}

/// 把引擎的 v2 状态投影为可观察的预览与确认片段；不在日志中记录正文。
///
/// 支持两种响应格式：
/// - **Legacy v2**：`{"confirmed_changed": true, "confirmed": "...", "preview": "..."}`
/// - **PreviewDraft typed**：`{"v":2, "kind":"draft", "span":{...}}` 或
///   `{"v":2, "kind":"preview", "text":"...", "revision": N, ...}`
///
/// PreviewDraft 格式保留 span/request identity 与绝对音频范围，调试界面据此
/// 把重叠 Preview 和非重叠 Draft 投影回真实音频时间轴。
fn collect_vad_debug_text(
    response: &str,
    fed_ms: u64,
    wall_ms: u64,
    last_preview: &mut String,
    last_confirmed: &mut String,
    events: &mut Vec<VadDebugTextEvent>,
) {
    if response.is_empty() {
        return;
    }
    let Ok(value) = serde_json::from_str::<serde_json::Value>(response) else {
        return;
    };

    // ── PreviewDraft typed envelope（v=2, kind=draft|preview）──
    if value.get("v").and_then(|v| v.as_u64()) == Some(2)
        && value.get("kind").and_then(|k| k.as_str()).is_some()
    {
        let kind = value["kind"].as_str().unwrap_or("");
        if kind == "draft" {
            if let Ok(span) = serde_json::from_value::<DraftSpan>(value["span"].clone())
                && !span.text.is_empty()
            {
                events.push(VadDebugTextEvent {
                    kind: "draft",
                    fed_ms,
                    wall_ms,
                    text: span.text,
                    request_id: None,
                    span_id: Some(span.span_id),
                    revision: Some(span.revision),
                    audio_range: Some(span.audio_range),
                });
            }
        } else if kind == "preview" {
            if let Some(preview) = value["text"].as_str() {
                if !preview.is_empty() && preview != *last_preview {
                    if events
                        .iter()
                        .filter(|event| event.kind == "preview")
                        .count()
                        < 256
                    {
                        events.push(VadDebugTextEvent {
                            kind: "preview",
                            fed_ms,
                            wall_ms,
                            text: preview.to_string(),
                            request_id: value
                                .get("requestId")
                                .or_else(|| value.get("request_id"))
                                .and_then(serde_json::Value::as_u64),
                            span_id: None,
                            revision: value.get("revision").and_then(serde_json::Value::as_u64),
                            audio_range: value
                                .get("audioRange")
                                .or_else(|| value.get("audio_range"))
                                .and_then(|range| {
                                    serde_json::from_value::<AudioRange>(range.clone()).ok()
                                }),
                        });
                    }
                    *last_preview = preview.to_string();
                }
            }
        }
        return;
    }

    // ── Legacy v2 格式 ──
    if value["confirmed_changed"].as_bool() == Some(true)
        && let Some(confirmed) = value["confirmed"].as_str()
    {
        let added = confirmed
            .strip_prefix(last_confirmed.as_str())
            .unwrap_or(confirmed);
        if !added.is_empty() {
            events.push(VadDebugTextEvent {
                kind: "confirmed",
                fed_ms,
                wall_ms,
                text: added.to_string(),
                request_id: None,
                span_id: None,
                revision: None,
                audio_range: None,
            });
        }
        *last_confirmed = confirmed.to_string();
    }
    if let Some(preview) = value["preview"].as_str()
        && !preview.is_empty()
        && preview != last_preview
    {
        if events
            .iter()
            .filter(|event| event.kind == "preview")
            .count()
            < 256
        {
            events.push(VadDebugTextEvent {
                kind: "preview",
                fed_ms,
                wall_ms,
                text: preview.to_string(),
                request_id: None,
                span_id: None,
                revision: None,
                audio_range: None,
            });
        }
        *last_preview = preview.to_string();
    }
}

// ── AudioTranscriptionPort 实现 ──────────────────────────────────────────

#[async_trait]
impl AudioTranscriptionPort for AudioTranscriptionService {
    async fn transcribe(
        &self,
        request: AudioTranscriptionRequest,
        deadline: Option<Instant>,
    ) -> Result<AudioTranscriptionResult, AudioTranscriptionError> {
        // 1. 检查 deadline
        self.check_deadline(deadline)?;

        // 2. 解析 audio_ref
        let mut opened =
            self.resolve_audio_ref(&request.audio_ref, ResourceUse::TranscribeAudio)?;

        // 3. 冻结 STT config、engine/model/instance 身份
        let conn_info = self.freeze_identity().await?;
        let frozen_identity = conn_info.as_ref().map(|(_, id)| id.clone());

        // 4. 本地模式需要 transport 可用
        let transport = match &conn_info {
            Some((t, _)) => t.clone(),
            None => {
                // 云端模式——不通过此路径调用本地 transport
                // 云端转写在当前首版暂不支持通过此 port 调用
                // （需要额外的 HTTP 转写逻辑和 secret 管理）
                return Err(AudioTranscriptionError::Unsupported {
                    detail: "cloud file transcription is not implemented".into(),
                });
            }
        };

        let frozen = match &frozen_identity {
            Some(id) => id.clone(),
            None => {
                return Err(AudioTranscriptionError::SttBackendUnavailable {
                    detail: "no running engine instance".into(),
                });
            }
        };

        // 5. 在 blocking pool 中有界读取、decode、normalize
        let config_clone = self.config.clone();
        let decode_result = tokio::task::spawn_blocking(move || -> Result<
            (
                Vec<f32>,
                crate::infra::platform::audio::format::SourceFormat,
                NormalizationSummary,
            ),
            AudioTranscriptionError,
        > {
            let bytes = Self::read_opened_bounded(&mut opened)?;
            Self::decode_and_normalize_with_config(&bytes, &config_clone)
        })
        .await
        .map_err(|e| AudioTranscriptionError::Internal {
            detail: format!("blocking task join error: {:?}", e),
        })??;

        let (normalized_samples, source_format, norm_summary) = decode_result;

        // 6. 再检查 deadline
        if let Some(dl) = deadline
            && Instant::now() >= dl
        {
            tracing::debug!("transcribe: deadline expired after decode/normalize");
            return Err(AudioTranscriptionError::Timeout {
                detail: "deadline expired during decode/normalize".into(),
            });
        }

        // 7. 编码成 canonical 16k mono PCM16 WAV
        let wav_bytes = crate::domain::stt::wav::pcm_to_wav(&normalized_samples, 16000, 1);
        if wav_bytes.is_empty() {
            return Err(AudioTranscriptionError::Internal {
                detail: "WAV encoding failed (size overflow?)".into(),
            });
        }

        // 计算 duration_ms
        let duration_ms = (normalized_samples.len() as f64 / 16000.0 * 1000.0) as u64;

        // 8. 调用 transport
        let transcribe_result = transport.transcribe(&wav_bytes).await;

        // 9. 返回前再次验证 model/instance
        if let Err(e) = self.verify_identity(&frozen).await {
            // 迟到结果丢弃——只记分类
            tracing::warn!(
                error_category = e.category(),
                "transcribe: identity verification failed post-transport, discarding result"
            );
            return Err(e);
        }

        // 处理 transport 结果
        let text = match transcribe_result {
            Ok(text) => text,
            Err(error) => return Err(map_transport_error(error)),
        };

        // 检查 deadline 是否在 transport 期间过期
        if let Some(dl) = deadline
            && Instant::now() >= dl
        {
            tracing::warn!("transcribe: deadline expired during transport, discarding late result");
            return Err(AudioTranscriptionError::Timeout {
                detail: "deadline expired during transport".into(),
            });
        }

        // 构建结果
        let no_speech = text.trim().is_empty();
        let result = AudioTranscriptionResult {
            text,
            duration_ms,
            engine_id: frozen.engine_id.clone(),
            model_id: frozen.model_id.clone(),
            engine_instance_id: frozen.instance_id.clone(),
            source_format: format!("{}", source_format),
            normalized_format: format!(
                "{}ch {}Hz mono",
                norm_summary.source_channels, norm_summary.target_sample_rate
            ),
            normalization: format!("{}", norm_summary),
            no_speech,
        };

        tracing::info!(
            engine_id = %result.engine_id,
            model_id = %result.model_id,
            duration_ms = result.duration_ms,
            no_speech = result.no_speech,
            "transcribe: success"
        );

        Ok(result)
    }
}

// ── 辅助：解码 + 规范化（静态方法，供 blocking pool 调用）──────────────

impl AudioTranscriptionService {
    /// 解码 + 规范化（静态方法，不依赖 &self，适合 blocking pool）。
    fn decode_and_normalize_with_config(
        file_bytes: &[u8],
        config: &TranscriptionConfig,
    ) -> Result<
        (
            Vec<f32>,
            crate::infra::platform::audio::format::SourceFormat,
            NormalizationSummary,
        ),
        AudioTranscriptionError,
    > {
        let decoded =
            decode_wav_with_budget(file_bytes, config.sample_budget).map_err(map_decode_error)?;

        if decoded.duration_secs > config.max_duration_secs {
            return Err(AudioTranscriptionError::AudioBudgetExceeded {
                detail: format!(
                    "duration {:.1}s exceeds max {:.0}s",
                    decoded.duration_secs, config.max_duration_secs
                ),
            });
        }

        let source_format = decoded.format;

        let mut normalizer = AudioNormalizer::from_source_format(source_format);
        let mut normalized_samples = normalizer.process(&decoded.samples);
        normalized_samples.extend(normalizer.finish());

        let summary = normalizer.summary();

        Ok((normalized_samples, source_format, summary))
    }
}

// ── 错误映射 ─────────────────────────────────────────────────────────────

/// 把 `ResourceError` 映射为 `AudioTranscriptionError`（0.23.12：ResourceStore 取代 AudioResourceRegistry）。
fn map_resource_error(e: ResourceError) -> AudioTranscriptionError {
    match e.kind {
        ResourceErrorKind::StaleResourceRef => AudioTranscriptionError::StaleAudioRef,
        ResourceErrorKind::InvalidResourceRef => AudioTranscriptionError::InvalidAudioRef {
            detail: "audio reference not found or invalid".into(),
        },
        ResourceErrorKind::UseDenied => AudioTranscriptionError::InvalidAudioRef {
            detail: "use not granted for this audio reference".into(),
        },
        ResourceErrorKind::PermissionEscalation => AudioTranscriptionError::InvalidAudioRef {
            detail: "derive would escalate the source grant".into(),
        },
        ResourceErrorKind::FileIdentityChanged => AudioTranscriptionError::InvalidAudioRef {
            detail: "file identity changed after issue".into(),
        },
        ResourceErrorKind::NotRegularFile => AudioTranscriptionError::UnsupportedAudioFormat {
            detail: "not a regular file".into(),
        },
        ResourceErrorKind::ResourceBudgetExceeded => AudioTranscriptionError::AudioBudgetExceeded {
            detail: "resource budget exceeded".into(),
        },
        ResourceErrorKind::FileNotFound => AudioTranscriptionError::InvalidAudioRef {
            detail: "file not found".into(),
        },
        ResourceErrorKind::IoError => AudioTranscriptionError::Internal {
            detail: "io error".into(),
        },
        ResourceErrorKind::UnsupportedBacking => AudioTranscriptionError::Unsupported {
            detail: "resource backing not supported".into(),
        },
    }
}

fn map_transport_error(error: SttTransportError) -> AudioTranscriptionError {
    match error {
        SttTransportError::Busy { .. } => AudioTranscriptionError::SttBusy,
        SttTransportError::Timeout { detail } => AudioTranscriptionError::Timeout { detail },
        SttTransportError::Cancelled => AudioTranscriptionError::Cancelled,
        SttTransportError::Unavailable { detail } => {
            AudioTranscriptionError::SttBackendUnavailable { detail }
        }
        SttTransportError::Internal { detail } => AudioTranscriptionError::Internal { detail },
    }
}

/// 把 `AudioDecodeError` 映射为 `AudioTranscriptionError`。
fn map_decode_error(e: AudioDecodeError) -> AudioTranscriptionError {
    match e {
        AudioDecodeError::BadRiffMagic(_)
        | AudioDecodeError::RiffSizeMismatch { .. }
        | AudioDecodeError::UnsupportedRiffVariant(_) => {
            AudioTranscriptionError::UnsupportedAudioFormat {
                detail: "not a valid RIFF/WAVE file".into(),
            }
        }
        AudioDecodeError::UnsupportedCodec { .. }
        | AudioDecodeError::UnsupportedBits(_)
        | AudioDecodeError::Unsupported8BitPcm
        | AudioDecodeError::UnknownExtensibleSubFormat => {
            AudioTranscriptionError::UnsupportedAudioFormat {
                detail: "unsupported audio codec or bit depth".into(),
            }
        }
        AudioDecodeError::Truncated(_)
        | AudioDecodeError::FmtTooSmall(_)
        | AudioDecodeError::DataMissing
        | AudioDecodeError::DataTruncated { .. }
        | AudioDecodeError::FrameMisalignment { .. }
        | AudioDecodeError::BlockAlignMismatch { .. }
        | AudioDecodeError::ByteRateMismatch { .. }
        | AudioDecodeError::NonFiniteFloat { .. }
        | AudioDecodeError::DuplicateChunk(_)
        | AudioDecodeError::Malformed(_) => AudioTranscriptionError::MalformedAudio {
            detail: "malformed WAV structure".into(),
        },
        AudioDecodeError::FmtMissing(_) => AudioTranscriptionError::MalformedAudio {
            detail: "fmt chunk missing".into(),
        },
        AudioDecodeError::InvalidChannels(_) | AudioDecodeError::InvalidSampleRate(_) => {
            AudioTranscriptionError::MalformedAudio {
                detail: "invalid channels or sample rate".into(),
            }
        }
        AudioDecodeError::BudgetExceeded(_, _) => AudioTranscriptionError::AudioBudgetExceeded {
            detail: "decoded sample budget exceeded".into(),
        },
    }
}

// ── 生产适配器 ─────────────────────────────────────────────────────────

/// `EngineConnectionPort` 的生产实现——桥接 `EngineManager::get_connection`。
///
/// 从 `EngineManager` 获取 `LocalEngineConnection`，提取 transport + 身份快照。
/// 不启动、不安装、不切换引擎——只查询当前运行实例。
pub struct EngineConnectionAdapter {
    engine_manager: Arc<crate::app::local_engine::EngineManager>,
}

impl EngineConnectionAdapter {
    pub fn new(engine_manager: Arc<crate::app::local_engine::EngineManager>) -> Self {
        Self { engine_manager }
    }
}

#[async_trait]
impl EngineConnectionPort for EngineConnectionAdapter {
    async fn get_stt_connection(
        &self,
        frozen_selection: &LocalSttSelection,
    ) -> Result<Option<(Arc<dyn SttTransport>, FrozenEngineIdentity)>, String> {
        // 只消费请求开始时冻结的 selection；禁止在适配层二次读取全局配置。
        let engine_id =
            crate::infra::local_engine::runtime::EngineId::new(&frozen_selection.engine_id)
                .map_err(|e| format!("invalid engine_id: {e}"))?;

        let conn = self
            .engine_manager
            .get_connection(&engine_id)
            .await
            .map_err(|e| format!("get_connection failed: {e}"))?;

        match conn {
            Some(c) => {
                let transport = c.worker.ok_or_else(|| {
                    "engine running but no worker transport available".to_string()
                })?;

                let running_model_id = c.model_id.ok_or_else(|| {
                    "engine running but launch snapshot has no frozen model identity".to_string()
                })?;
                if running_model_id != frozen_selection.model_id {
                    return Err(format!(
                        "running model does not match selected model: running={running_model_id}, selected={}",
                        frozen_selection.model_id
                    ));
                }

                let identity = FrozenEngineIdentity {
                    engine_id: c.engine_id.clone(),
                    model_id: running_model_id,
                    instance_id: c.instance_id.clone(),
                };

                Ok(Some((transport, identity)))
            }
            None => Ok(None),
        }
    }
}

/// `CloudEgressAuthorizer` 的生产实现——检查 STT 云端转写是否已获授权。
///
/// 动态外发确认尚未接入前采用 fail-closed；配置为 Cloud 只代表路由选择，
/// 不等于用户对本次音频外发作出授权。
pub struct SttCloudEgressAuthorizer {
    /// 后续显式动态授权流程接入后，由一次性授权票据替代此占位实现。
    _phantom: std::marker::PhantomData<()>,
}

impl SttCloudEgressAuthorizer {
    pub fn new() -> Self {
        Self {
            _phantom: std::marker::PhantomData,
        }
    }
}

impl Default for SttCloudEgressAuthorizer {
    fn default() -> Self {
        Self::new()
    }
}

impl CloudEgressAuthorizer for SttCloudEgressAuthorizer {
    fn is_authorized(&self, _provider_kind: &str) -> bool {
        false
    }
}

// ── 测试 ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn production_cloud_authorizer_is_fail_closed() {
        assert!(!SttCloudEgressAuthorizer::new().is_authorized("openai"));
    }

    #[test]
    fn vad_debug_preserves_typed_draft_identity_and_audio_range() {
        let mut events = Vec::new();
        collect_vad_debug_text(
            r#"{"v":2,"kind":"draft","span":{"spanId":7,"audioRange":{"startSample":16000,"endSample":96000},"text":"稳定草稿","revision":3}}"#,
            6_100,
            6_250,
            &mut String::new(),
            &mut String::new(),
            &mut events,
        );

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, "draft");
        assert_eq!(events[0].span_id, Some(7));
        assert_eq!(events[0].revision, Some(3));
        assert_eq!(events[0].audio_range, Some(AudioRange::new(16_000, 96_000)));
        assert_eq!(events[0].text, "稳定草稿");
    }

    #[test]
    fn vad_debug_preserves_typed_preview_request_and_range() {
        let mut events = Vec::new();
        collect_vad_debug_text(
            r#"{"v":2,"kind":"preview","requestId":11,"audioRange":{"startSample":48000,"endSample":96000},"revision":5,"text":"滚动预览"}"#,
            6_000,
            6_080,
            &mut String::new(),
            &mut String::new(),
            &mut events,
        );

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, "preview");
        assert_eq!(events[0].request_id, Some(11));
        assert_eq!(events[0].revision, Some(5));
        assert_eq!(events[0].audio_range, Some(AudioRange::new(48_000, 96_000)));
    }

    #[test]
    fn structured_transport_error_mapping_contract() {
        assert!(matches!(
            map_transport_error(SttTransportError::Busy {
                detail: "full".into()
            }),
            AudioTranscriptionError::SttBusy
        ));
        assert!(matches!(
            map_transport_error(SttTransportError::Timeout {
                detail: "deadline".into()
            }),
            AudioTranscriptionError::Timeout { .. }
        ));
        assert!(matches!(
            map_transport_error(SttTransportError::Cancelled),
            AudioTranscriptionError::Cancelled
        ));
        assert!(matches!(
            map_transport_error(SttTransportError::Unavailable {
                detail: "closed".into()
            }),
            AudioTranscriptionError::SttBackendUnavailable { .. }
        ));
    }
    use crate::domain::resource::{ResourceGrantSpec, ReusePolicy};
    use crate::infra::platform::audio::test_fixtures::*;
    use std::sync::Mutex;
    use tempfile::tempdir;

    // ── Fake transports and seams ──────────────────────────────────────

    /// Fake transport——受控 barrier + oneshot，模拟成功/失败/延迟。
    struct FakeTransport {
        response_text: String,
        failure: Option<SttTransportError>,
    }

    impl FakeTransport {
        fn success(text: &str) -> Self {
            Self {
                response_text: text.to_string(),
                failure: None,
            }
        }

        fn fail(error: SttTransportError) -> Self {
            Self {
                response_text: String::new(),
                failure: Some(error),
            }
        }
    }

    #[async_trait]
    impl SttTransport for FakeTransport {
        async fn check_ready(&self) -> Result<(), SttTransportError> {
            Ok(())
        }

        async fn transcribe(&self, _wav_bytes: &[u8]) -> Result<String, SttTransportError> {
            if let Some(error) = &self.failure {
                Err(error.clone())
            } else {
                Ok(self.response_text.clone())
            }
        }
    }

    struct ConfigChangingTransport;

    #[async_trait]
    impl SttTransport for ConfigChangingTransport {
        async fn check_ready(&self) -> Result<(), SttTransportError> {
            Ok(())
        }

        async fn transcribe(&self, _wav_bytes: &[u8]) -> Result<String, SttTransportError> {
            let mut changed = get_stt_config();
            changed.local_stt_selection = Some(LocalSttSelection::new("funasr", "paraformer-zh"));
            crate::domain::config::stt_config::update_cache(&changed);
            Ok("冻结配置仍然有效".into())
        }
    }

    /// Fake engine connection port——返回冻结的 transport + identity。
    struct FakeEngineConnection {
        transport: Option<Arc<dyn SttTransport>>,
        identity: Option<FrozenEngineIdentity>,
        /// 第二次调用（verify_identity）时返回的 identity——用于切模测试。
        second_identity: Mutex<Option<Option<FrozenEngineIdentity>>>,
        call_count: Mutex<u32>,
        requested_selections: Mutex<Vec<LocalSttSelection>>,
    }

    impl FakeEngineConnection {
        fn new(transport: Arc<dyn SttTransport>, identity: FrozenEngineIdentity) -> Self {
            Self {
                transport: Some(transport),
                identity: Some(identity),
                second_identity: Mutex::new(None),
                call_count: Mutex::new(0),
                requested_selections: Mutex::new(Vec::new()),
            }
        }

        /// 设置第二次调用时返回的 identity（None = 引擎停止）。
        fn set_second_identity(&self, identity: Option<FrozenEngineIdentity>) {
            *self.second_identity.lock().unwrap() = Some(identity);
        }

        fn requested_selections(&self) -> Vec<LocalSttSelection> {
            self.requested_selections.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl EngineConnectionPort for FakeEngineConnection {
        async fn get_stt_connection(
            &self,
            frozen_selection: &LocalSttSelection,
        ) -> Result<Option<(Arc<dyn SttTransport>, FrozenEngineIdentity)>, String> {
            self.requested_selections
                .lock()
                .unwrap()
                .push(frozen_selection.clone());
            let mut count = self.call_count.lock().unwrap();
            *count += 1;

            // 第一次调用
            if *count == 1 {
                if let (Some(t), Some(id)) = (&self.transport, &self.identity) {
                    return Ok(Some((t.clone(), id.clone())));
                }
                return Ok(None);
            }

            // 后续调用
            let second = self.second_identity.lock().unwrap();
            if let Some(ref second_id) = *second {
                match second_id {
                    Some(id) => {
                        if let Some(t) = &self.transport {
                            return Ok(Some((t.clone(), id.clone())));
                        }
                    }
                    None => return Ok(None),
                }
            }

            // 默认返回与第一次相同
            if let (Some(t), Some(id)) = (&self.transport, &self.identity) {
                return Ok(Some((t.clone(), id.clone())));
            }
            Ok(None)
        }
    }

    /// Fake cloud egress authorizer。
    struct FakeCloudAuth {
        authorized: bool,
    }

    impl CloudEgressAuthorizer for FakeCloudAuth {
        fn is_authorized(&self, _provider_kind: &str) -> bool {
            self.authorized
        }
    }

    // ── 测试辅助 ────────────────────────────────────────────────────────

    /// 生成临时 WAV 文件并返回路径 + audio_ref。
    fn make_test_wav(
        store: &DefaultResourceStore,
        dir: &std::path::Path,
        name: &str,
        cfg: &FixtureConfig,
    ) -> (String, std::path::PathBuf) {
        let wav_bytes = build_wav(cfg);
        let path = dir.join(name);
        std::fs::write(&path, &wav_bytes).unwrap();
        let audio_ref = store
            .issue_local_file(
                &path,
                ResourceGrantSpec::new(
                    ResourceUse::TranscribeAudio,
                    ReusePolicy::OneShot,
                    "stt_test",
                ),
            )
            .unwrap();
        (audio_ref.as_str().to_string(), path)
    }

    /// 默认测试配置。
    fn default_test_config() -> TranscriptionConfig {
        TranscriptionConfig::default()
    }

    /// 创建本地模式 SttConfig 缓存。
    /// 首次调用用 `init_cache`，后续用 `update_cache` 覆盖（OnceLock 只设一次）。
    ///
    /// **线程安全**：内部获取 `STT_CONFIG_TEST_LOCK`，确保配置变更不会被其他测试的
    /// `update_cache` 覆盖（Rust 默认并行跑 test，OnceLock 缓存是全局共享态）。
    /// 创建本地模式 SttConfig 缓存并返回锁守卫。
    ///
    /// 调用方应在 transcribe 调用期间持有此锁守卫，
    /// 确保其他测试不会在执行期间修改全局 OnceLock 缓存。
    ///
    /// 首次调用用 `init_cache`，后续用 `update_cache` 覆盖（OnceLock 只设一次）。
    async fn init_local_stt_config(enabled: bool) -> tokio::sync::MutexGuard<'static, ()> {
        use crate::domain::config::stt_config::{
            LocalEngineConfig, LocalSttSelection, SttConfig, SttMode,
        };
        let lock = STT_CONFIG_TEST_LOCK.lock().await;
        let config = SttConfig {
            enabled,
            mode: SttMode::Local,
            cloud_provider: None,
            cloud: None,
            local_engine: LocalEngineConfig::default(),
            local_stt_selection: Some(LocalSttSelection {
                engine_id: "funasr".into(),
                model_id: "sensevoice-small".into(),
            }),
            local_model_id: None,
            model_dir: None,
            audio_device_id: None,
            streaming_mode: Default::default(),
            streaming: false,
        };
        // 先尝试 init_cache（首次），如果已初始化则用 update_cache
        crate::domain::config::stt_config::init_cache(config.clone());
        crate::domain::config::stt_config::update_cache(&config);
        // 安全转 'static：OnceLock 是进程级静态变量，生命周期与进程相同
        unsafe {
            std::mem::transmute::<
                tokio::sync::MutexGuard<'_, ()>,
                tokio::sync::MutexGuard<'static, ()>,
            >(lock)
        }
    }

    // ── 成功路径 ─────────────────────────────────────────────────────────

    #[tokio::test]
    async fn transcribe_success() {
        let _lock = init_local_stt_config(true).await;
        let dir = tempdir().unwrap();

        let store = Arc::new(DefaultResourceStore::default());
        let cfg = FixtureConfig::default();
        let (audio_ref, _path) = make_test_wav(&store, dir.path(), "test.wav", &cfg);

        let transport = Arc::new(FakeTransport::success("你好世界"));
        let identity = FrozenEngineIdentity {
            engine_id: "funasr".into(),
            model_id: "sensevoice-small".into(),
            instance_id: "inst-1".into(),
        };
        let engine_conn = Arc::new(FakeEngineConnection::new(transport, identity));
        let cloud_auth = Arc::new(FakeCloudAuth { authorized: false });

        let service =
            AudioTranscriptionService::new(store, engine_conn, cloud_auth, default_test_config());

        let request = AudioTranscriptionRequest { audio_ref };
        let result = service.transcribe(request, None).await;

        assert!(
            result.is_ok(),
            "transcribe should succeed: {:?}",
            result.err()
        );
        let result = result.unwrap();
        assert_eq!(result.text, "你好世界");
        assert!(!result.no_speech);
        assert_eq!(result.engine_id, "funasr");
        assert_eq!(result.model_id, "sensevoice-small");
        assert_eq!(result.engine_instance_id, "inst-1");
        assert!(result.source_format.contains("1ch"));
        assert!(result.source_format.contains("16000Hz"));
        assert!(result.normalized_format.contains("mono"));
    }

    #[tokio::test]
    async fn config_change_during_transcription_keeps_frozen_selection() {
        let _lock = init_local_stt_config(true).await;
        let dir = tempdir().unwrap();
        let store = Arc::new(DefaultResourceStore::default());
        let (audio_ref, _) = make_test_wav(
            &store,
            dir.path(),
            "config-change.wav",
            &FixtureConfig::default(),
        );
        let identity = FrozenEngineIdentity {
            engine_id: "funasr".into(),
            model_id: "sensevoice-small".into(),
            instance_id: "inst-frozen".into(),
        };
        let engine_conn = Arc::new(FakeEngineConnection::new(
            Arc::new(ConfigChangingTransport),
            identity,
        ));
        let service = AudioTranscriptionService::new(
            store,
            engine_conn.clone(),
            Arc::new(FakeCloudAuth { authorized: false }),
            default_test_config(),
        );

        let result = service
            .transcribe(AudioTranscriptionRequest { audio_ref }, None)
            .await
            .unwrap();
        assert_eq!(result.model_id, "sensevoice-small");
        assert_eq!(
            engine_conn.requested_selections(),
            vec![
                LocalSttSelection::new("funasr", "sensevoice-small"),
                LocalSttSelection::new("funasr", "sensevoice-small"),
            ]
        );

        // 恢复全局测试配置，避免影响后续串行用例。
        let mut restored = get_stt_config();
        restored.local_stt_selection = Some(LocalSttSelection::new("funasr", "sensevoice-small"));
        crate::domain::config::stt_config::update_cache(&restored);
    }

    // ── 空文本 ───────────────────────────────────────────────────────────

    #[tokio::test]
    async fn transcribe_empty_text_no_speech() {
        let _lock = init_local_stt_config(true).await;
        let dir = tempdir().unwrap();

        let store = Arc::new(DefaultResourceStore::default());
        let cfg = FixtureConfig::default();
        let (audio_ref, _path) = make_test_wav(&store, dir.path(), "test.wav", &cfg);

        let transport = Arc::new(FakeTransport::success(""));
        let identity = FrozenEngineIdentity {
            engine_id: "funasr".into(),
            model_id: "sensevoice-small".into(),
            instance_id: "inst-1".into(),
        };
        let engine_conn = Arc::new(FakeEngineConnection::new(transport, identity));
        let cloud_auth = Arc::new(FakeCloudAuth { authorized: false });

        let service =
            AudioTranscriptionService::new(store, engine_conn, cloud_auth, default_test_config());

        let request = AudioTranscriptionRequest { audio_ref };
        let result = service.transcribe(request, None).await.unwrap();

        assert!(result.text.is_empty());
        assert!(result.no_speech);
    }

    // ── 超时 ────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn transcribe_timeout_before_start() {
        let _lock = init_local_stt_config(true).await;
        let dir = tempdir().unwrap();

        let store = Arc::new(DefaultResourceStore::default());
        let cfg = FixtureConfig::default();
        let (audio_ref, _path) = make_test_wav(&store, dir.path(), "test.wav", &cfg);

        let transport = Arc::new(FakeTransport::success("你好"));
        let identity = FrozenEngineIdentity {
            engine_id: "funasr".into(),
            model_id: "sensevoice-small".into(),
            instance_id: "inst-1".into(),
        };
        let engine_conn = Arc::new(FakeEngineConnection::new(transport, identity));
        let cloud_auth = Arc::new(FakeCloudAuth { authorized: false });

        let service =
            AudioTranscriptionService::new(store, engine_conn, cloud_auth, default_test_config());

        // deadline 已过期
        let deadline = Some(Instant::now() - std::time::Duration::from_millis(1));
        let request = AudioTranscriptionRequest { audio_ref };
        let result = service.transcribe(request, deadline).await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, AudioTranscriptionError::Timeout { .. }));
    }

    // ── 未配置 ──────────────────────────────────────────────────────────

    /// 串行化 STT 配置变更——所有修改全局 OnceLock 缓存的测试必须先获取此锁。
    /// `init_local_stt_config` 内部也会获取此锁，确保配置设置不会互相覆盖。
    /// 使用 `tokio::sync::Mutex` 以允许锁跨 `.await` 持有（`transcribe_stt_not_configured`
    /// 需要在锁内执行 transcribe 调用，确保其他测试不会在执行期间把 config 改回 true）。
    static STT_CONFIG_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    #[tokio::test]
    async fn transcribe_stt_not_configured() {
        // 加锁串行化——防止其他测试在 update_cache 和 get_stt_config 之间覆盖
        // tokio::sync::MutexGuard 可以跨 await 持有，确保 transcribe 执行期间
        // 其他测试不会把 config 改回 enabled=true
        let _lock = STT_CONFIG_TEST_LOCK.lock().await;

        // 先初始化为 true（其他测试可能已设），再更新为 false
        // 不调用 init_local_stt_config（它会递归获取锁）
        {
            use crate::domain::config::stt_config::{
                LocalEngineConfig, LocalSttSelection, SttConfig, SttMode,
            };
            let config = SttConfig {
                enabled: true,
                mode: SttMode::Local,
                cloud_provider: None,
                cloud: None,
                local_engine: LocalEngineConfig::default(),
                local_stt_selection: Some(LocalSttSelection {
                    engine_id: "funasr".into(),
                    model_id: "sensevoice-small".into(),
                }),
                local_model_id: None,
                model_dir: None,
                audio_device_id: None,
                streaming_mode: Default::default(),
                streaming: false,
            };
            crate::domain::config::stt_config::init_cache(config.clone());
            crate::domain::config::stt_config::update_cache(&config);
        }
        // 强制更新为 enabled=false
        use crate::domain::config::stt_config::{
            LocalEngineConfig, LocalSttSelection, SttConfig, SttMode,
        };
        let config = SttConfig {
            enabled: false,
            mode: SttMode::Local,
            cloud_provider: None,
            cloud: None,
            local_engine: LocalEngineConfig::default(),
            local_stt_selection: Some(LocalSttSelection {
                engine_id: "funasr".into(),
                model_id: "sensevoice-small".into(),
            }),
            local_model_id: None,
            model_dir: None,
            audio_device_id: None,
            streaming_mode: Default::default(),
            streaming: false,
        };
        crate::domain::config::stt_config::update_cache(&config);

        // 验证缓存确实为 enabled=false
        let cached = crate::domain::config::stt_config::get_stt_config();
        assert!(!cached.enabled, "config should be disabled");

        let dir = tempdir().unwrap();

        let store = Arc::new(DefaultResourceStore::default());
        let cfg = FixtureConfig::default();
        let (audio_ref, _path) = make_test_wav(&store, dir.path(), "test.wav", &cfg);

        let transport = Arc::new(FakeTransport::success("你好"));
        let identity = FrozenEngineIdentity {
            engine_id: "funasr".into(),
            model_id: "sensevoice-small".into(),
            instance_id: "inst-1".into(),
        };
        let engine_conn = Arc::new(FakeEngineConnection::new(transport, identity));
        let cloud_auth = Arc::new(FakeCloudAuth { authorized: false });

        let service =
            AudioTranscriptionService::new(store, engine_conn, cloud_auth, default_test_config());

        let request = AudioTranscriptionRequest { audio_ref };
        let result = service.transcribe(request, None).await;

        assert!(
            result.is_err(),
            "transcribe should fail when STT not configured"
        );
        let err = result.unwrap_err();
        assert!(
            matches!(err, AudioTranscriptionError::SttNotConfigured),
            "expected SttNotConfigured, got {:?}",
            err
        );

        // 恢复为 enabled=true 供后续测试使用
        drop(_lock); // 先释放锁，再调用 init_local_stt_config
        let _ = init_local_stt_config(true).await;
    }

    #[tokio::test]
    #[allow(clippy::field_reassign_with_default)]
    async fn cloud_file_transcription_reports_unsupported_not_authorization() {
        let _lock = STT_CONFIG_TEST_LOCK.lock().await;
        let mut cloud = crate::domain::config::stt_config::SttConfig::default();
        cloud.enabled = true;
        cloud.mode = SttMode::Cloud;
        cloud.cloud_provider = Some(crate::domain::config::stt_config::SttCloudProvider {
            kind: "openai".into(),
            base_url: None,
            model_id: "whisper".into(),
        });
        crate::domain::config::stt_config::init_cache(cloud.clone());
        crate::domain::config::stt_config::update_cache(&cloud);

        let dir = tempdir().unwrap();
        let store = Arc::new(DefaultResourceStore::default());
        let (audio_ref, _) =
            make_test_wav(&store, dir.path(), "cloud.wav", &FixtureConfig::default());
        let engine_conn = Arc::new(FakeEngineConnection {
            transport: None,
            identity: None,
            second_identity: Mutex::new(None),
            call_count: Mutex::new(0),
            requested_selections: Mutex::new(Vec::new()),
        });
        let service = AudioTranscriptionService::new(
            store,
            engine_conn,
            Arc::new(FakeCloudAuth { authorized: false }),
            default_test_config(),
        );

        let error = service
            .transcribe(AudioTranscriptionRequest { audio_ref }, None)
            .await
            .unwrap_err();
        assert!(matches!(error, AudioTranscriptionError::Unsupported { .. }));

        let mut restored = cloud;
        restored.mode = SttMode::Local;
        restored.cloud_provider = None;
        restored.local_stt_selection = Some(LocalSttSelection::new("funasr", "sensevoice-small"));
        crate::domain::config::stt_config::update_cache(&restored);
    }

    // ── 引擎不可用 ───────────────────────────────────────────────────────

    #[tokio::test]
    async fn transcribe_backend_unavailable() {
        let _lock = init_local_stt_config(true).await;
        let dir = tempdir().unwrap();

        let store = Arc::new(DefaultResourceStore::default());
        let cfg = FixtureConfig::default();
        let (audio_ref, _path) = make_test_wav(&store, dir.path(), "test.wav", &cfg);

        // FakeEngineConnection with no transport
        let engine_conn = Arc::new(FakeEngineConnection {
            transport: None,
            identity: None,
            second_identity: Mutex::new(None),
            call_count: Mutex::new(0),
            requested_selections: Mutex::new(Vec::new()),
        });
        let cloud_auth = Arc::new(FakeCloudAuth { authorized: false });

        let service =
            AudioTranscriptionService::new(store, engine_conn, cloud_auth, default_test_config());

        let request = AudioTranscriptionRequest { audio_ref };
        let result = service.transcribe(request, None).await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            matches!(err, AudioTranscriptionError::SttBackendUnavailable { .. }),
            "expected SttBackendUnavailable, got {:?}",
            err
        );
    }

    // ── 切模竞态（身份变化）────────────────────────────────────────────

    #[tokio::test]
    async fn transcribe_identity_changed_during_execution() {
        let _lock = init_local_stt_config(true).await;
        let dir = tempdir().unwrap();

        let store = Arc::new(DefaultResourceStore::default());
        let cfg = FixtureConfig::default();
        let (audio_ref, _path) = make_test_wav(&store, dir.path(), "test.wav", &cfg);

        let transport = Arc::new(FakeTransport::success("你好世界"));
        let identity = FrozenEngineIdentity {
            engine_id: "funasr".into(),
            model_id: "sensevoice-small".into(),
            instance_id: "inst-1".into(),
        };
        let engine_conn = Arc::new(FakeEngineConnection::new(transport, identity));

        // 设置第二次调用返回不同的 identity（切模）
        engine_conn.set_second_identity(Some(FrozenEngineIdentity {
            engine_id: "funasr".into(),
            model_id: "paraformer-zh".into(), // 不同 model
            instance_id: "inst-1".into(),
        }));
        let cloud_auth = Arc::new(FakeCloudAuth { authorized: false });

        let service =
            AudioTranscriptionService::new(store, engine_conn, cloud_auth, default_test_config());

        let request = AudioTranscriptionRequest { audio_ref };
        let result = service.transcribe(request, None).await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            matches!(err, AudioTranscriptionError::SttIdentityChanged { .. }),
            "expected SttIdentityChanged, got {:?}",
            err
        );
    }

    #[tokio::test]
    async fn transcribe_instance_restart_discards_old_result() {
        let _lock = init_local_stt_config(true).await;
        let dir = tempdir().unwrap();
        let store = Arc::new(DefaultResourceStore::default());
        let (audio_ref, _) =
            make_test_wav(&store, dir.path(), "restart.wav", &FixtureConfig::default());
        let transport = Arc::new(FakeTransport::success("迟到结果"));
        let engine_conn = Arc::new(FakeEngineConnection::new(
            transport,
            FrozenEngineIdentity {
                engine_id: "funasr".into(),
                model_id: "sensevoice-small".into(),
                instance_id: "inst-old".into(),
            },
        ));
        engine_conn.set_second_identity(Some(FrozenEngineIdentity {
            engine_id: "funasr".into(),
            model_id: "sensevoice-small".into(),
            instance_id: "inst-new".into(),
        }));
        let service = AudioTranscriptionService::new(
            store,
            engine_conn,
            Arc::new(FakeCloudAuth { authorized: false }),
            default_test_config(),
        );

        let error = service
            .transcribe(AudioTranscriptionRequest { audio_ref }, None)
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            AudioTranscriptionError::SttIdentityChanged { .. }
        ));
    }

    // ── 引擎在执行期间停止 ─────────────────────────────────────────────

    #[tokio::test]
    async fn transcribe_engine_stopped_during_execution() {
        let _lock = init_local_stt_config(true).await;
        let dir = tempdir().unwrap();

        let store = Arc::new(DefaultResourceStore::default());
        let cfg = FixtureConfig::default();
        let (audio_ref, _path) = make_test_wav(&store, dir.path(), "test.wav", &cfg);

        let transport = Arc::new(FakeTransport::success("你好世界"));
        let identity = FrozenEngineIdentity {
            engine_id: "funasr".into(),
            model_id: "sensevoice-small".into(),
            instance_id: "inst-1".into(),
        };
        let engine_conn = Arc::new(FakeEngineConnection::new(transport, identity));

        // 设置第二次调用返回 None（引擎停止）
        engine_conn.set_second_identity(None);
        let cloud_auth = Arc::new(FakeCloudAuth { authorized: false });

        let service =
            AudioTranscriptionService::new(store, engine_conn, cloud_auth, default_test_config());

        let request = AudioTranscriptionRequest { audio_ref };
        let result = service.transcribe(request, None).await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            matches!(err, AudioTranscriptionError::SttBackendUnavailable { .. }),
            "expected SttBackendUnavailable, got {:?}",
            err
        );
    }

    // ── stale audio_ref ────────────────────────────────────────────────

    #[tokio::test]
    async fn transcribe_stale_audio_ref() {
        let _lock = init_local_stt_config(true).await;
        let dir = tempdir().unwrap();

        let mut store_config = crate::domain::resource::ResourceStoreConfig::production();
        store_config.local.ttl = std::time::Duration::from_millis(1);
        let store = Arc::new(DefaultResourceStore::new(store_config));
        let cfg = FixtureConfig::default();
        let (audio_ref, _path) = make_test_wav(&store, dir.path(), "test.wav", &cfg);

        // 等待过期
        std::thread::sleep(std::time::Duration::from_millis(10));

        let transport = Arc::new(FakeTransport::success("你好"));
        let identity = FrozenEngineIdentity {
            engine_id: "funasr".into(),
            model_id: "sensevoice-small".into(),
            instance_id: "inst-1".into(),
        };
        let engine_conn = Arc::new(FakeEngineConnection::new(transport, identity));
        let cloud_auth = Arc::new(FakeCloudAuth { authorized: false });

        let service =
            AudioTranscriptionService::new(store, engine_conn, cloud_auth, default_test_config());

        let request = AudioTranscriptionRequest { audio_ref };
        let result = service.transcribe(request, None).await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, AudioTranscriptionError::StaleAudioRef));
    }

    // ── 损坏音频 ──────────────────────────────────────────────────────

    #[tokio::test]
    async fn transcribe_malformed_audio() {
        let _lock = init_local_stt_config(true).await;
        let dir = tempdir().unwrap();

        let store = Arc::new(DefaultResourceStore::default());

        // 写入非 WAV 文件
        let path = dir.path().join("bad.wav");
        std::fs::write(&path, b"not a wav file at all").unwrap();
        let audio_ref = store
            .issue_local_file(
                &path,
                ResourceGrantSpec::new(
                    ResourceUse::TranscribeAudio,
                    ReusePolicy::OneShot,
                    "stt_test",
                ),
            )
            .unwrap()
            .as_str()
            .to_string();

        let transport = Arc::new(FakeTransport::success("你好"));
        let identity = FrozenEngineIdentity {
            engine_id: "funasr".into(),
            model_id: "sensevoice-small".into(),
            instance_id: "inst-1".into(),
        };
        let engine_conn = Arc::new(FakeEngineConnection::new(transport, identity));
        let cloud_auth = Arc::new(FakeCloudAuth { authorized: false });

        let service =
            AudioTranscriptionService::new(store, engine_conn, cloud_auth, default_test_config());

        let request = AudioTranscriptionRequest { audio_ref };
        let result = service.transcribe(request, None).await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            matches!(err, AudioTranscriptionError::UnsupportedAudioFormat { .. })
                || matches!(err, AudioTranscriptionError::MalformedAudio { .. }),
            "expected UnsupportedAudioFormat or MalformedAudio, got {:?}",
            err
        );
    }

    // ── transport 返回 busy ───────────────────────────────────────────

    #[tokio::test]
    async fn transcribe_transport_busy() {
        let _lock = init_local_stt_config(true).await;
        let dir = tempdir().unwrap();

        let store = Arc::new(DefaultResourceStore::default());
        let cfg = FixtureConfig::default();
        let (audio_ref, _path) = make_test_wav(&store, dir.path(), "test.wav", &cfg);

        let transport = Arc::new(FakeTransport::fail(SttTransportError::Busy {
            detail: "queue full".into(),
        }));
        let identity = FrozenEngineIdentity {
            engine_id: "funasr".into(),
            model_id: "sensevoice-small".into(),
            instance_id: "inst-1".into(),
        };
        let engine_conn = Arc::new(FakeEngineConnection::new(transport, identity));
        let cloud_auth = Arc::new(FakeCloudAuth { authorized: false });

        let service =
            AudioTranscriptionService::new(store, engine_conn, cloud_auth, default_test_config());

        let request = AudioTranscriptionRequest { audio_ref };
        let result = service.transcribe(request, None).await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            matches!(err, AudioTranscriptionError::SttBusy),
            "expected SttBusy, got {:?}",
            err
        );
    }

    // ── transport 返回超时 ────────────────────────────────────────────

    #[tokio::test]
    async fn transcribe_transport_timeout() {
        let _lock = init_local_stt_config(true).await;
        let dir = tempdir().unwrap();

        let store = Arc::new(DefaultResourceStore::default());
        let cfg = FixtureConfig::default();
        let (audio_ref, _path) = make_test_wav(&store, dir.path(), "test.wav", &cfg);

        let transport = Arc::new(FakeTransport::fail(SttTransportError::Timeout {
            detail: "request timed out".into(),
        }));
        let identity = FrozenEngineIdentity {
            engine_id: "funasr".into(),
            model_id: "sensevoice-small".into(),
            instance_id: "inst-1".into(),
        };
        let engine_conn = Arc::new(FakeEngineConnection::new(transport, identity));
        let cloud_auth = Arc::new(FakeCloudAuth { authorized: false });

        let service =
            AudioTranscriptionService::new(store, engine_conn, cloud_auth, default_test_config());

        let request = AudioTranscriptionRequest { audio_ref };
        let result = service.transcribe(request, None).await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            matches!(err, AudioTranscriptionError::Timeout { .. }),
            "expected Timeout, got {:?}",
            err
        );
    }

    // ── 多声道 48kHz WAV 规范化 ───────────────────────────────────────

    #[tokio::test]
    async fn transcribe_stereo_48k_normalized_to_16k_mono() {
        let _lock = init_local_stt_config(true).await;
        let dir = tempdir().unwrap();

        let store = Arc::new(DefaultResourceStore::default());
        let cfg = FixtureConfig {
            channels: 2,
            sample_rate: 48000,
            bits_per_sample: 16,
            kind: FixtureSampleKind::PcmSigned,
            extensible: false,
            num_frames: 480, // 10ms @ 48k stereo
            values: FixtureValues::Zero,
            ..Default::default()
        };
        let (audio_ref, _path) = make_test_wav(&store, dir.path(), "stereo.wav", &cfg);

        let transport = Arc::new(FakeTransport::success("测试"));
        let identity = FrozenEngineIdentity {
            engine_id: "funasr".into(),
            model_id: "sensevoice-small".into(),
            instance_id: "inst-1".into(),
        };
        let engine_conn = Arc::new(FakeEngineConnection::new(transport, identity));
        let cloud_auth = Arc::new(FakeCloudAuth { authorized: false });

        let service =
            AudioTranscriptionService::new(store, engine_conn, cloud_auth, default_test_config());

        let request = AudioTranscriptionRequest { audio_ref };
        let result = service.transcribe(request, None).await;

        assert!(
            result.is_ok(),
            "stereo 48k should normalize: {:?}",
            result.err()
        );
        let result = result.unwrap();
        // 480 frames @ 48k stereo → 160 samples @ 16k mono = 10ms
        // duration_ms = 160 / 16000 * 1000 = 10
        assert_eq!(result.duration_ms, 10);
        assert!(result.source_format.contains("2ch"));
        assert!(result.source_format.contains("48000Hz"));
        assert!(result.normalized_format.contains("16000Hz"));
        assert!(result.normalized_format.contains("mono"));
    }

    // ── 结果不含敏感信息 ──────────────────────────────────────────────

    #[tokio::test]
    async fn transcribe_result_has_no_secrets() {
        let _lock = init_local_stt_config(true).await;
        let dir = tempdir().unwrap();

        let store = Arc::new(DefaultResourceStore::default());
        let cfg = FixtureConfig::default();
        let (audio_ref, _path) = make_test_wav(&store, dir.path(), "test.wav", &cfg);

        let transport = Arc::new(FakeTransport::success("你好世界"));
        let identity = FrozenEngineIdentity {
            engine_id: "funasr".into(),
            model_id: "sensevoice-small".into(),
            instance_id: "inst-abc".into(),
        };
        let engine_conn = Arc::new(FakeEngineConnection::new(transport, identity));
        let cloud_auth = Arc::new(FakeCloudAuth { authorized: false });

        let service =
            AudioTranscriptionService::new(store, engine_conn, cloud_auth, default_test_config());

        let request = AudioTranscriptionRequest { audio_ref };
        let result = service.transcribe(request, None).await.unwrap();

        // 结果不含路径
        let json = serde_json::to_string(&result).unwrap();
        assert!(!json.contains("C:\\"));
        assert!(!json.contains("/tmp"));
        assert!(!json.contains("test.wav"));
        // 不含 audio_ref token
        assert!(!json.contains("aref_"));
        // 不含 endpoint
        assert!(!json.contains("127.0.0.1"));
        assert!(!json.contains("http://"));
    }
}
