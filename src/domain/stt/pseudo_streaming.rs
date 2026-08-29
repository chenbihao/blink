//! 伪流式 STT 引擎——VAD 切句定稿 + 累积预览。
//!
//! ## 设计
//!
//! 在非自回归的 SenseVoice 上实现"边说边出字"体感：
//! - 每 500ms 对累积音频做一次 HTTP 识别 → 预览文本（灰色半透明）
//! - VAD 检测到句尾时对本句音频做定稿识别 → 确认文本（不再变化）
//!
//! 用户体验：
//! ```text
//! 定稿: "你好世界。"          ← 白色，不变
//! 预览: "今天天气"            ← 灰色，可能变化
//! ```
//!
//! ## 与其他引擎的关系
//!
//! - [`LocalSttEngine`](super::local::LocalSttEngine)：非流式（transcribe_chunk 空转）
//! - **本引擎**：伪流式（VAD 切句 + 定时 HTTP 轮询）⭐ 默认
//!
//! ## transcribe_chunk 返回值
//!
//! 返回 JSON 字符串 `{"confirmed":"...","preview":"..."}`，
//! voice.rs 解析后分别 emit confirmed 和 preview。
//! 如果 confirmed 和 preview 都为空，返回空字符串（兼容现有逻辑）。
//!
//! ## 并发安全
//!
//! 使用 `Arc<std::sync::Mutex>` 保护内部状态。后台 HTTP task 通过 clone 的
//! `Arc` 在完成后短暂加锁写入结果。`transcribe_chunk` 是 async 但不跨 await
//! 持有 `std::sync::Mutex`（先 lock 取数据/写数据，再 drop guard，再 await）。

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::vad::{EnergyVad, VadEvent};
use super::{SttEngine, SttError};

/// 预览识别间隔（毫秒）。
const PREVIEW_INTERVAL_MS: u64 = 500;

/// 累积音频超过此时长时，预览间隔自动拉长（毫秒）。
const PREVIEW_SLOWDOWN_THRESHOLD_MS: u64 = 8000;

/// 预览间隔在慢速模式下的值（毫秒）。
const PREVIEW_SLOW_INTERVAL_MS: u64 = 1000;

/// finalize 等待 in_flight 请求的最大时间。
const FINALIZE_WAIT_TIMEOUT_MS: u64 = 3000;

/// 伪流式 STT 引擎。
///
/// 组合 VAD 切句 + 累积预览，在非自回归 SenseVoice 上实现"边说边出字"体感。
///
/// 0.22.6 批次 3: 存储完整 `SttEngineConnection` 快照，确保 health 检查和
/// 转录请求使用同一 endpoint/token——不再分别用 port 和 token 猜测。
pub struct PseudoStreamingSttEngine {
    /// 内部状态
    inner: Arc<Mutex<PseudoInner>>,
    /// 复用 HTTP client（避免每次建连）
    client: reqwest::Client,
    /// 连接快照（host + port + token + engine_id + instance_id）
    ///
    /// 0.22.6: health 和 transcribe 共用此快照，保证同一连接。
    /// 服务重启后旧连接的 token/instance_id 不匹配新实例，请求被拒绝。
    connection: Option<crate::domain::stt::SttEngineConnection>,
    /// FunASR 模型标识
    funasr_model: String,
    /// 采样率
    sample_rate: u32,
}

/// 伪流式引擎内部状态。
struct PseudoInner {
    /// VAD 切句器
    vad: EnergyVad,
    /// 句子缓冲管理
    sentences: SentenceBuffer,
    /// 累积音频样本
    samples: Vec<f32>,
    /// 上一次触发预览识别的时刻
    last_preview: Instant,
    /// 是否有预览识别请求在飞行中
    preview_in_flight: bool,
    /// 最新预览文本
    latest_preview: String,
    /// 是否有定稿识别请求在飞行中
    finalize_in_flight: bool,
    /// 最新定稿文本（句尾触发，尚未追加到 confirmed_sentences）
    pending_confirmed: Option<String>,
    /// 预览代际计数器（0.10.6 防重复影子）
    ///
    /// 每次 VAD 句尾时递增。`spawn_preview_recognition` 启动时捕获当前代际，
    /// 返回时校验：若代际不匹配（句尾已发生），说明此预览的音频跨越了句子边界，
    /// 包含已定稿句子的内容，直接丢弃避免覆盖 `latest_preview` 造成重复影子。
    preview_generation: u64,
}

/// 句子缓冲管理。
struct SentenceBuffer {
    /// 已定稿的句子列表
    confirmed_sentences: Vec<String>,
    /// 当前句子的起始样本索引
    current_sentence_start: usize,
}

impl SentenceBuffer {
    fn new() -> Self {
        Self {
            confirmed_sentences: Vec::new(),
            current_sentence_start: 0,
        }
    }

    /// 句尾事件：取出本句音频范围，标记下一句起始。
    fn on_sentence_end(&mut self, total_samples: usize) -> std::ops::Range<usize> {
        let range = self.current_sentence_start..total_samples;
        self.current_sentence_start = total_samples;
        range
    }

    /// 追加一句定稿文本。
    fn append_confirmed(&mut self, text: &str) {
        if !text.is_empty() {
            self.confirmed_sentences.push(text.to_string());
        }
    }

    /// 获取已确认部分的文本。
    fn confirmed_text(&self) -> String {
        self.confirmed_sentences.join("")
    }
}

impl PseudoStreamingSttEngine {
    /// 从 `SttEngineConnection` 创建伪流式 STT 引擎（0.22.6 批次 3）。
    ///
    /// 推荐的生产构造方式——连接快照包含完整身份（host/port/token/IDs），
    /// health 和 transcribe 共用同一快照。
    pub fn from_connection(
        config: &crate::domain::config::stt_config::SttConfig,
        conn: crate::domain::stt::SttEngineConnection,
    ) -> Result<Self, String> {
        let model = config.local_engine.funasr_model.clone();

        let ready = super::funasr::is_server_ready(conn.port);
        if !ready {
            return Err(format!(
                "FunASR 服务未在端口 {} 上运行。\
                 请在设置页「语音输入」→「本地模式」中点击「启动服务」按钮。",
                conn.port
            ));
        }

        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .map_err(|e| format!("HTTP client 创建失败: {e}"))?;

        let vad_cfg = &config.local_engine.vad;
        tracing::info!(
            port = conn.port, model = %model,
            silence_threshold = vad_cfg.silence_threshold,
            min_silence_ms = vad_cfg.min_silence_ms,
            min_sentence_ms = vad_cfg.min_sentence_ms,
            "伪流式 STT 引擎: VAD + HTTP 轮询 (就绪)"
        );

        Ok(Self {
            inner: Arc::new(Mutex::new(PseudoInner {
                vad: EnergyVad::with_params(
                    16000,
                    vad_cfg.silence_threshold,
                    vad_cfg.min_silence_ms,
                    vad_cfg.min_sentence_ms,
                ),
                sentences: SentenceBuffer::new(),
                samples: Vec::new(),
                last_preview: Instant::now(),
                preview_in_flight: false,
                latest_preview: String::new(),
                finalize_in_flight: false,
                pending_confirmed: None,
                preview_generation: 0,
            })),
            client,
            connection: Some(conn),
            funasr_model: model,
            sample_rate: 16000,
        })
    }

    /// 返回当前应使用的预览间隔（累积过长时降频）。
    fn preview_interval(samples_len: usize, sample_rate: u32) -> Duration {
        let duration_ms = (samples_len as f64 / sample_rate as f64 * 1000.0) as u64;
        if duration_ms > PREVIEW_SLOWDOWN_THRESHOLD_MS {
            Duration::from_millis(PREVIEW_SLOW_INTERVAL_MS)
        } else {
            Duration::from_millis(PREVIEW_INTERVAL_MS)
        }
    }

    /// 组装返回 JSON 字符串。
    fn compose_result(confirmed: &str, preview: &str) -> String {
        if confirmed.is_empty() && preview.is_empty() {
            return String::new();
        }
        serde_json::json!({
            "confirmed": confirmed,
            "preview": preview,
        })
        .to_string()
    }

    /// HTTP 转录 URL（0.22.6: 使用连接快照中的 host:port）。
    fn transcription_url(&self) -> String {
        let conn = match &self.connection {
            Some(c) => c,
            None => return String::new(), // 诊断模式无连接
        };
        format!("http://{}:{}/v1/audio/transcriptions", conn.host, conn.port)
    }

    /// 异步 HTTP 转录（同步等待结果）。
    ///
    /// 0.22.6: 使用连接快照做 token-aware health 检查 + 转录请求，
    /// 确保 health 和 transcribe 使用同一 endpoint/token。
    async fn transcribe_samples(&self, samples: &[f32]) -> Result<String, SttError> {
        if samples.is_empty() {
            return Ok(String::new());
        }

        let conn = self
            .connection
            .as_ref()
            .ok_or_else(|| SttError::Engine("伪流式引擎无连接快照".to_string()))?;

        // 0.22.6: token-aware health 检查，确保模型就绪
        super::funasr::check_model_ready_or_error_with_token(conn)
            .await
            .map_err(SttError::Engine)?;

        // 裁剪尾部静音，减少 SenseVoice 幻觉英文语气词
        let trimmed = trim_trailing_silence(samples, self.sample_rate);
        let wav_bytes = super::wav::pcm_to_wav(&trimmed, self.sample_rate, 1);
        let url = self.transcription_url();

        let text = super::wav::transcribe_with_token(
            &self.client,
            &url,
            Some(&conn.token),
            &self.funasr_model,
            &wav_bytes,
        )
        .await?;

        // 剥离 SenseVoice 幻觉的英文语气词
        let cleaned = strip_filler_words(&text);
        Ok(cleaned)
    }

    /// 后台 spawn 一个定稿识别 task。
    ///
    /// 0.22.6: 使用连接快照中的 token，确保与 health 检查使用同一连接。
    fn spawn_sentence_finalize(&self, sentence_samples: Vec<f32>) {
        if sentence_samples.is_empty() {
            return;
        }

        // 标记 in_flight
        {
            let mut inner = self.inner.lock().unwrap();
            inner.finalize_in_flight = true;
        }

        let inner = Arc::clone(&self.inner);
        let client = self.client.clone();
        let url = self.transcription_url();
        let model = self.funasr_model.clone();
        let token = self.connection.as_ref().map(|c| c.token.clone());
        let sample_rate = self.sample_rate;

        tokio::spawn(async move {
            // 裁剪尾部静音，减少 SenseVoice 幻觉英文语气词
            let trimmed = trim_trailing_silence(&sentence_samples, sample_rate);
            let wav_bytes = super::wav::pcm_to_wav(&trimmed, sample_rate, 1);

            match super::wav::transcribe_with_token(
                &client,
                &url,
                token.as_deref(),
                &model,
                &wav_bytes,
            )
            .await
            {
                Ok(text) => {
                    let cleaned = strip_filler_words(&text);
                    tracing::debug!(
                        %cleaned, samples = sentence_samples.len(),
                        "定稿识别"
                    );
                    // 写入 pending_confirmed，下次 transcribe_chunk 时收取
                    let mut inner = inner.lock().unwrap();
                    inner.pending_confirmed = Some(cleaned);
                }
                Err(e) => {
                    tracing::warn!(%e, "定稿识别失败");
                }
            }

            // 清除 in_flight 标志
            let mut inner = inner.lock().unwrap();
            inner.finalize_in_flight = false;
        });
    }

    /// 后台 spawn 一个预览识别 task。
    ///
    /// 0.22.6: 使用连接快照中的 token，确保与 health 检查使用同一连接。
    fn spawn_preview_recognition(&self, samples_snapshot: Vec<f32>) {
        if samples_snapshot.is_empty() {
            return;
        }

        // 标记 in_flight + 捕获当前代际
        let generation = {
            let mut inner = self.inner.lock().unwrap();
            inner.preview_in_flight = true;
            inner.preview_generation
        };

        let inner = Arc::clone(&self.inner);
        let client = self.client.clone();
        let url = self.transcription_url();
        let model = self.funasr_model.clone();
        let token = self.connection.as_ref().map(|c| c.token.clone());
        let sample_rate = self.sample_rate;

        tokio::spawn(async move {
            // 裁剪尾部静音，减少 SenseVoice 幻觉英文语气词
            let trimmed = trim_trailing_silence(&samples_snapshot, sample_rate);
            let wav_bytes = super::wav::pcm_to_wav(&trimmed, sample_rate, 1);

            match super::wav::transcribe_with_token(
                &client,
                &url,
                token.as_deref(),
                &model,
                &wav_bytes,
            )
            .await
            {
                Ok(text) => {
                    let cleaned = strip_filler_words(&text);
                    if !cleaned.is_empty() {
                        // 精简日志：raw 仅在与 cleaned 不同时打印
                        if cleaned != text {
                            tracing::trace!(%cleaned, raw = %text, "预览识别");
                        } else {
                            tracing::trace!(%cleaned, "预览识别");
                        }
                        // 写入 latest_preview（代际校验：句尾后丢弃过期预览）
                        let mut inner = inner.lock().unwrap();
                        if inner.preview_generation == generation {
                            inner.latest_preview = cleaned;
                        } else {
                            tracing::debug!(
                                gen = generation,
                                cur_gen = inner.preview_generation,
                                "丢弃过期预览（句尾已发生）"
                            );
                        }
                    }
                }
                Err(e) => {
                    tracing::trace!(%e, "预览识别失败（非致命）");
                }
            }

            // 清除 in_flight 标志
            let mut inner = inner.lock().unwrap();
            inner.preview_in_flight = false;
        });
    }
}

/// 从预览文本中剥离已确认的前缀部分。
///
/// 预览识别只取未确认音频，但模型仍可能因句子边界切分不完全
/// 而在 preview 开头重复部分 confirmed 文本。此函数做兜底清理：
///
/// 1. 精确前缀匹配 → 直接剥离
/// 2. 逐字符匹配 → 剥离匹配部分（应对标点差异等）
/// 3. 无匹配 → 原样返回
///
/// # 算法
///
/// 逐字符从开头比较 confirmed 和 preview，遇到第一个不匹配的字符停止。
/// 匹配长度 ≥ confirmed 长度的 50% 时才剥离（避免误剥离短公共前缀如"我"）。
fn strip_confirmed_prefix(confirmed: &str, preview: &str) -> String {
    if confirmed.is_empty() || preview.is_empty() {
        return preview.to_string();
    }

    // 1. 精确前缀匹配
    if let Some(stripped) = preview.strip_prefix(confirmed) {
        return stripped.to_string();
    }

    // 2. 逐字符匹配（应对标点差异）
    let confirmed_chars: Vec<char> = confirmed.chars().collect();
    let preview_chars: Vec<char> = preview.chars().collect();

    let mut match_len = 0;
    for (c, p) in confirmed_chars.iter().zip(preview_chars.iter()) {
        if c == p {
            match_len += 1;
        } else {
            break;
        }
    }

    // 匹配长度需达到 confirmed 的 50% 才剥离
    // 避免短公共前缀（如 "我"）导致误剥离
    if match_len > 0 && match_len * 2 >= confirmed_chars.len() {
        preview_chars[match_len..].iter().collect()
    } else {
        preview.to_string()
    }
}

/// 尾部静音裁剪阈值（与 VAD 一致）。
const TRIM_SILENCE_THRESHOLD: f64 = 0.005;

/// 裁剪后保留的尾部缓冲（毫秒），避免切掉软辅音尾音。
const TRIM_TAIL_BUFFER_MS: u32 = 150;

/// 裁剪音频尾部的静音/低能量段。
///
/// SenseVoice 等多语言模型在尾部静音上容易幻觉出英文语气词
///（如 "Yeah." "Okay."）。裁剪尾部静音可大幅减少此问题。
///
/// 算法：从末尾向前扫描，找到最后一个超过阈值的样本，
/// 保留该位置 + `TRIM_TAIL_BUFFER_MS` 缓冲后的部分。
fn trim_trailing_silence(samples: &[f32], sample_rate: u32) -> Vec<f32> {
    if samples.is_empty() {
        return Vec::new();
    }

    // 从末尾向前找最后一个有声样本
    let threshold = TRIM_SILENCE_THRESHOLD as f32;
    let mut last_audible = None;
    for (i, &s) in samples.iter().enumerate().rev() {
        if s.abs() > threshold {
            last_audible = Some(i);
            break;
        }
    }

    match last_audible {
        None => {
            // 全静音 → 原样返回（不破坏空音频逻辑）
            samples.to_vec()
        }
        Some(idx) => {
            let buffer_samples = (TRIM_TAIL_BUFFER_MS as u64 * sample_rate as u64 / 1000) as usize;
            let end = (idx + 1 + buffer_samples).min(samples.len());
            samples[..end].to_vec()
        }
    }
}

/// SenseVoice 常见英文语气词幻觉。
///
/// 这些词在中文语音识别中不应出现，是多语言模型在静音段上的已知幻觉。
const FILLER_WORDS: &[&str] = &[
    "Yeah", "yeah", "Okay", "okay", "OK", "ok", "Mm", "mm", "Hmm", "hmm", "Uh", "uh", "Oh", "oh",
    "Ah", "ah", "Um", "um", "No", "no", "Yes", "yes", "Well", "well", "So", "so", "Right", "right",
    "Like", "like", "But", "but", "And", "and",
];

/// 判断字符是否为中文。
fn is_chinese_char(c: char) -> bool {
    matches!(c, '\u{4e00}'..='\u{9fff}' | '\u{3400}'..='\u{4dbf}')
}

/// 剥离 SenseVoice 幻觉产生的尾部英文语气词。
///
/// 当识别文本以中文为主时，模型可能在尾部静音段幻觉出
/// 英文填充词（如 "Yeah." "Okay."）。此函数做后处理清理。
///
/// emoji 和 CJK 间空格已由 Python server `_postprocess_text` 处理，
/// 此处不再重复。
///
/// 仅当文本包含中文字符时才执行剥离，避免误伤纯英文识别。
fn strip_filler_words(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return trimmed.to_string();
    }

    // 检查是否包含中文字符
    let has_chinese = trimmed.chars().any(is_chinese_char);
    if !has_chinese {
        return trimmed.to_string();
    }

    let mut result = trimmed.to_string();

    // 循环剥离尾部语气词（可能多个连续出现）
    loop {
        let stripped = strip_one_filler_suffix(&result);
        if stripped.len() == result.len() {
            break;
        }
        result = stripped;
    }

    // 清理尾部残留的空格和标点
    result.trim_end().to_string()
}

/// 尝试从文本末尾剥离一个英文语气词后缀。
/// 返回剥离后的文本；如果没有匹配则原样返回。
fn strip_one_filler_suffix(text: &str) -> String {
    let lower = text.to_lowercase();

    for &filler in FILLER_WORDS {
        let filler_lower = filler.to_lowercase();

        // 模式 1: "...中文 Yeah." → 匹配 " Yeah." / " Yeah," 等
        // 前面是空格或中文标点
        for &suffix in &[".", ",", "!", "?", ""] {
            let pattern = format!(" {}{}", filler_lower, suffix);
            if lower.ends_with(&pattern) {
                let cut = text.len() - pattern.len();
                return text[..cut].to_string();
            }
        }

        // 模式 2: "...中文Yeah." → 无空格直接拼接（较少见但存在）
        // 仅当 filler 前面是中文字符或中文标点时才匹配
        for &suffix in &[".", ",", "!", "?"] {
            let pattern = format!("{}{}", filler_lower, suffix);
            if lower.ends_with(&pattern) {
                let cut = text.len() - pattern.len();
                if cut > 0 {
                    let prev_char = text[..cut].chars().next_back();
                    if let Some(pc) = prev_char {
                        // 非 ASCII 字符 = 中文（汉字或标点）
                        if !pc.is_ascii() {
                            return text[..cut].to_string();
                        }
                    }
                }
            }
        }
    }

    text.to_string()
}

#[async_trait::async_trait]
impl SttEngine for PseudoStreamingSttEngine {
    async fn transcribe_chunk(&self, samples: &[f32]) -> Result<String, SttError> {
        // ── 1. 累积音频 + 喂 VAD ──
        let (_vad_event, sentence_range, should_preview, samples_snapshot) = {
            let mut inner = self.inner.lock().unwrap();
            inner.samples.extend_from_slice(samples);
            let total = inner.samples.len();

            // 喂 VAD
            let event = inner.vad.process_chunk(samples);

            // 处理句尾
            let range = if event == VadEvent::SentenceEnd {
                Some(inner.sentences.on_sentence_end(total))
            } else {
                None
            };

            // 检查是否该触发预览
            let interval = Self::preview_interval(total, self.sample_rate);
            let should_preview =
                inner.last_preview.elapsed() >= interval && !inner.preview_in_flight;

            // 收取 pending 定稿结果（如果有）
            if let Some(text) = inner.pending_confirmed.take() {
                inner.sentences.append_confirmed(&text);
            }

            // 句尾时清空预览（本句已定稿，下一段预览从空开始）
            // 同时递增 generation，使 in-flight 的旧预览返回时被丢弃（防重复影子）
            if event == VadEvent::SentenceEnd {
                inner.latest_preview.clear();
                inner.preview_generation = inner.preview_generation.wrapping_add(1);
            }

            let snapshot = if should_preview {
                // 只取未确认部分的音频（current_sentence_start 之后），
                // 避免预览重复已定稿的句子内容
                let start = inner.sentences.current_sentence_start;
                if start < inner.samples.len() {
                    inner.samples[start..].to_vec()
                } else {
                    Vec::new()
                }
            } else {
                Vec::new()
            };

            if should_preview {
                inner.last_preview = Instant::now();
            }

            (event, range, should_preview, snapshot)
        };

        // ── 2. VAD 句尾 → spawn 定稿识别（后台 HTTP） ──
        if let Some(range) = sentence_range {
            let sentence_samples: Vec<f32> = {
                let inner = self.inner.lock().unwrap();
                inner
                    .samples
                    .get(range)
                    .map(|s| s.to_vec())
                    .unwrap_or_default()
            };

            if !sentence_samples.is_empty() {
                self.spawn_sentence_finalize(sentence_samples);
            }

            // VAD 句尾后重置句子计数
            self.inner.lock().unwrap().vad.reset_sentence();
        }

        // ── 3. 500ms 定时 → spawn 预览识别（后台 HTTP） ──
        if should_preview {
            self.spawn_preview_recognition(samples_snapshot);
        }

        // ── 4. 组装返回 ──
        // strip_confirmed_prefix 兜底：即使预览只取了未确认音频，
        // 模型仍可能因为句子边界切分不完全而产生部分重叠文本
        let (confirmed, preview) = {
            let inner = self.inner.lock().unwrap();
            let confirmed = inner.sentences.confirmed_text();
            let preview = strip_confirmed_prefix(&confirmed, &inner.latest_preview);
            (confirmed, preview)
        };

        Ok(Self::compose_result(&confirmed, &preview))
    }

    async fn finalize(&self) -> Result<String, SttError> {
        // 1. 定稿剩余音频（finalize 识别）
        let remaining_samples: Vec<f32> = {
            let inner = self.inner.lock().unwrap();
            let start = inner.sentences.current_sentence_start;
            if start < inner.samples.len() {
                inner.samples[start..].to_vec()
            } else {
                Vec::new()
            }
        };

        let finalize_text = if !remaining_samples.is_empty() {
            match self.transcribe_samples(&remaining_samples).await {
                Ok(text) => text,
                Err(e) => {
                    tracing::warn!(%e, "finalize 定稿识别失败，使用已有结果");
                    String::new()
                }
            }
        } else {
            String::new()
        };

        // 2. 等待 in_flight 预览/定稿请求完成（最多 3s）
        let deadline = Instant::now() + Duration::from_millis(FINALIZE_WAIT_TIMEOUT_MS);
        loop {
            let (preview_in_flight, finalize_in_flight) = {
                let inner = self.inner.lock().unwrap();
                (inner.preview_in_flight, inner.finalize_in_flight)
            };

            if !preview_in_flight && !finalize_in_flight {
                break;
            }
            if Instant::now() >= deadline {
                tracing::warn!("finalize: 等待 in_flight 请求超时，使用已有结果");
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        // 3. 收取 pending 定稿结果
        {
            let mut inner = self.inner.lock().unwrap();
            if let Some(text) = inner.pending_confirmed.take() {
                inner.sentences.append_confirmed(&text);
            }
        }

        // 4. 拼接 confirmed + finalize_text + 最后一段 preview
        let final_text = {
            let inner = self.inner.lock().unwrap();
            let mut result = inner.sentences.confirmed_text();
            if !finalize_text.is_empty() {
                result.push_str(&finalize_text);
            }
            // 如果 finalize 没有识别到文本，用最后一段 preview 兜底
            if result.is_empty() && !inner.latest_preview.is_empty() {
                result = inner.latest_preview.clone();
            }
            result
        };

        tracing::info!(
            text_len = final_text.chars().count(),
            %final_text,
            "伪流式识别完成",
        );

        Ok(final_text)
    }

    fn reset(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.vad.reset();
        inner.sentences = SentenceBuffer::new();
        inner.samples.clear();
        inner.last_preview = Instant::now();
        inner.preview_in_flight = false;
        inner.latest_preview.clear();
        inner.finalize_in_flight = false;
        inner.pending_confirmed = None;
        inner.preview_generation = inner.preview_generation.wrapping_add(1);
        tracing::debug!("伪流式引擎 reset");
    }

    fn name(&self) -> &str {
        "pseudo-streaming"
    }
}

// ── 测试 ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sentence_buffer_compose() {
        let mut buf = SentenceBuffer::new();
        buf.append_confirmed("你好世界。");
        buf.append_confirmed("今天天气不错。");
        assert_eq!(buf.confirmed_text(), "你好世界。今天天气不错。");
    }

    #[test]
    fn sentence_buffer_empty() {
        let buf = SentenceBuffer::new();
        assert_eq!(buf.confirmed_text(), "");
    }

    #[test]
    fn sentence_buffer_on_sentence_end() {
        let mut buf = SentenceBuffer::new();
        let range1 = buf.on_sentence_end(1000);
        assert_eq!(range1, 0..1000);
        let range2 = buf.on_sentence_end(2500);
        assert_eq!(range2, 1000..2500);
    }

    #[test]
    fn compose_result_empty_returns_empty_string() {
        assert_eq!(PseudoStreamingSttEngine::compose_result("", ""), "");
    }

    #[test]
    fn compose_result_with_preview_only() {
        let result = PseudoStreamingSttEngine::compose_result("", "你好");
        let v: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(v["confirmed"], "");
        assert_eq!(v["preview"], "你好");
    }

    #[test]
    fn compose_result_with_both() {
        let result = PseudoStreamingSttEngine::compose_result("你好。", "世界");
        let v: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(v["confirmed"], "你好。");
        assert_eq!(v["preview"], "世界");
    }

    #[test]
    fn preview_interval_normal() {
        let interval = PseudoStreamingSttEngine::preview_interval(16000 * 3, 16000);
        assert_eq!(interval, Duration::from_millis(PREVIEW_INTERVAL_MS));
    }

    #[test]
    fn preview_interval_slowdown() {
        let interval = PseudoStreamingSttEngine::preview_interval(16000 * 10, 16000);
        assert_eq!(interval, Duration::from_millis(PREVIEW_SLOW_INTERVAL_MS));
    }

    // ── strip_confirmed_prefix 测试 ──

    #[test]
    fn strip_prefix_exact_match() {
        // preview 完全以 confirmed 开头 → 剥离
        assert_eq!(
            strip_confirmed_prefix("你好世界。", "你好世界。今天天气"),
            "今天天气"
        );
    }

    #[test]
    fn strip_prefix_no_confirmed() {
        // confirmed 为空 → 原样返回
        assert_eq!(strip_confirmed_prefix("", "你好"), "你好");
    }

    #[test]
    fn strip_prefix_no_preview() {
        // preview 为空 → 原样返回
        assert_eq!(strip_confirmed_prefix("你好", ""), "");
    }

    #[test]
    fn strip_prefix_no_overlap() {
        // 完全不匹配 → 原样返回
        assert_eq!(
            strip_confirmed_prefix("你好世界。", "今天天气不错"),
            "今天天气不错"
        );
    }

    #[test]
    fn strip_prefix_partial_match() {
        // 部分匹配（前 2 字符匹配，第 3 个不同）→ 剥离匹配部分
        // confirmed = "你好世"（3 chars），匹配 2 个 = 67% ≥ 50% → 剥离
        assert_eq!(
            strip_confirmed_prefix("你好世", "你好时间今天天气"),
            "时间今天天气"
        );
    }

    #[test]
    fn strip_prefix_short_common_prefix_not_stripped() {
        // 短公共前缀（2 字符 = 33% < 50%）→ 不剥离
        // confirmed = "你好世界今天"（6 chars），匹配 2 个 = 33%
        assert_eq!(
            strip_confirmed_prefix("你好世界今天", "你好朋友"),
            "你好朋友"
        );
    }

    #[test]
    fn strip_prefix_preview_equals_confirmed() {
        // preview == confirmed → 剥离后为空
        assert_eq!(strip_confirmed_prefix("你好世界。", "你好世界。"), "");
    }

    // ── trim_trailing_silence 测试 ──

    #[test]
    fn trim_silence_all_silence() {
        // 全静音 → 原样返回
        let samples = vec![0.0f32; 1600];
        let trimmed = trim_trailing_silence(&samples, 16000);
        assert_eq!(trimmed.len(), 1600);
    }

    #[test]
    fn trim_silence_empty() {
        let trimmed = trim_trailing_silence(&[], 16000);
        assert!(trimmed.is_empty());
    }

    #[test]
    fn trim_silence_trims_trailing_zeros() {
        // 有声 50ms + 静音 1s → 裁剪后保留有声 + 150ms 缓冲
        let mut samples = vec![0.1f32; 800]; // 有声 50ms
        samples.extend(vec![0.0f32; 16000]); // 静音 1s
        let trimmed = trim_trailing_silence(&samples, 16000);
        // 最后有声样本在 index 799，缓冲 = 150ms * 16000 / 1000 = 2400
        // end = min(800 + 2400, 16800) = 3200
        assert_eq!(trimmed.len(), 3200);
    }

    #[test]
    fn trim_silence_no_trailing_silence() {
        // 无尾部静音 → 原样返回（缓冲不超出长度）
        let samples = vec![0.1f32; 1600];
        let trimmed = trim_trailing_silence(&samples, 16000);
        // idx = 1599, buffer = 2400, end = min(1600, 1600) = 1600
        assert_eq!(trimmed.len(), 1600);
    }

    // ── strip_filler_words 测试 ──

    #[test]
    fn filler_strip_yeah_period() {
        assert_eq!(
            strip_filler_words("我现在在做一个语音输入的。Yeah."),
            "我现在在做一个语音输入的。"
        );
    }

    #[test]
    fn filler_strip_okay_period() {
        assert_eq!(
            strip_filler_words("然后有一个假的流逝输入。Okay."),
            "然后有一个假的流逝输入。"
        );
    }

    #[test]
    fn filler_strip_multiple_fillers() {
        // 连续多个语气词
        assert_eq!(strip_filler_words("你好世界。Yeah. Okay."), "你好世界。");
    }

    #[test]
    fn filler_strip_no_chinese_not_stripped() {
        // 纯英文不剥离
        assert_eq!(strip_filler_words("Hello world Yeah."), "Hello world Yeah.");
    }

    #[test]
    fn filler_strip_no_filler() {
        // 无语气词 → 原样
        assert_eq!(
            strip_filler_words("你好世界。今天天气不错。"),
            "你好世界。今天天气不错。"
        );
    }

    #[test]
    fn filler_strip_empty() {
        assert_eq!(strip_filler_words(""), "");
    }

    #[test]
    fn filler_strip_only_filler_with_chinese() {
        // 中文 + 纯语气词（无标点）
        assert_eq!(strip_filler_words("你好世界 Yeah"), "你好世界");
    }

    #[test]
    fn filler_strip_no_space_variant() {
        // 无空格直接拼接（中文后直接跟英文）
        assert_eq!(strip_filler_words("你好世界。Yeah."), "你好世界。");
    }

    #[test]
    fn filler_strip_chinese_period_then_yeah() {
        // 用户实际遇到的 case：中文句号后无空格直接跟英文语气词
        assert_eq!(
            strip_filler_words("我现在呢在做一个语音输入的。然后有一个假的流逝输入。Yeah."),
            "我现在呢在做一个语音输入的。然后有一个假的流逝输入。"
        );
    }

    #[test]
    fn filler_strip_preserves_chinese_text() {
        // 确保不会误剥离正常中文文本
        assert_eq!(strip_filler_words("好的，我知道了。"), "好的，我知道了。");
    }

    #[test]
    fn engine_reset_clears_state() {
        let engine = PseudoStreamingSttEngine {
            inner: Arc::new(Mutex::new(PseudoInner {
                vad: {
                    let mut v = EnergyVad::new(16000);
                    // 模拟有状态
                    v.process_chunk(&[0.1; 1600]);
                    v
                },
                sentences: {
                    let mut s = SentenceBuffer::new();
                    s.append_confirmed("测试");
                    s
                },
                samples: vec![0.1; 1000],
                last_preview: Instant::now() - Duration::from_secs(10),
                preview_in_flight: true,
                latest_preview: "测试预览".to_string(),
                finalize_in_flight: true,
                pending_confirmed: Some("pending".to_string()),
                preview_generation: 0,
            })),
            client: reqwest::Client::new(),
            connection: None,
            funasr_model: "test".to_string(),
            sample_rate: 16000,
        };

        engine.reset();

        let inner = engine.inner.lock().unwrap();
        assert!(!inner.vad.is_speaking());
        assert!(inner.samples.is_empty());
        assert!(inner.latest_preview.is_empty());
        assert!(!inner.preview_in_flight);
        assert!(!inner.finalize_in_flight);
        assert!(inner.pending_confirmed.is_none());
        assert_eq!(
            inner.preview_generation, 1,
            "reset 应递增 preview_generation"
        );
        assert_eq!(inner.sentences.confirmed_text(), "");
    }

    // 验证带 token 字段的引擎能正常构造和 reset
    #[test]
    fn engine_with_token_constructs_and_resets() {
        let engine = PseudoStreamingSttEngine {
            inner: Arc::new(Mutex::new(PseudoInner {
                vad: EnergyVad::new(16000),
                sentences: SentenceBuffer::new(),
                samples: vec![0.1; 100],
                last_preview: Instant::now(),
                preview_in_flight: false,
                latest_preview: String::new(),
                finalize_in_flight: false,
                pending_confirmed: None,
                preview_generation: 0,
            })),
            client: reqwest::Client::new(),
            connection: Some(crate::domain::stt::SttEngineConnection {
                host: "127.0.0.1".to_string(),
                port: 8000,
                token: "test-token-abcdef0123456789".to_string(),
                engine_id: "funasr".to_string(),
                instance_id: "inst-test".to_string(),
            }),
            funasr_model: "test".to_string(),
            sample_rate: 16000,
        };

        engine.reset();

        let inner = engine.inner.lock().unwrap();
        assert!(inner.samples.is_empty());
        assert_eq!(inner.preview_generation, 1);
    }
}
