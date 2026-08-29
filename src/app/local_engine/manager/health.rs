//! EngineManager 健康验证用例：
//! 两阶段 health 轮询（verify_engine_health）与身份 / backend / 模型身份校验。

use super::*;

#[allow(dead_code)]
impl EngineManager {
    // ── health 验证 ─────────────────────────────────────────────────────────

    /// 验证引擎 health——轮询直到 Model Ready 或 Err。
    ///
    /// **0.22.3 Task G**: 只有两个终态：
    /// - `Ok(HealthMapping)`：service=Healthy + model=Ready + 身份/backend 全匹配
    /// - `Err`：timeout / mismatch / backend 错误 / ModelFailed / 不可达
    ///
    /// 不返回模糊的 `Verified(last_mapping)`——last_mapping 可能为 NotLoaded。
    /// start 只有在真实 Model Ready 后才返回 Ok。
    ///
    /// **进程早退快速失败（0.22.6 phase B）**：每次轮询前检查进程状态——
    /// 已退出时不等满 start_timeout，按输出尾部分类：
    /// - 明确的 address-in-use → `StartAttemptFailure::BindRace`（可换端口重试）；
    /// - 其他任何退出 → `StartAttemptFailure::Fatal`（附输出尾部，便于诊断）。
    pub(super) async fn verify_engine_health(
        &self,
        engine_id: &EngineId,
        entry: &Arc<EngineEntry>,
        identity_input: &ServiceIdentityInput,
        managed: &Arc<ManagedProcess>,
    ) -> Result<HealthMapping, StartAttemptFailure> {
        let health_url = format!("{}/health", identity_input.endpoint.base_url());
        let token = identity_input.token.clone();
        let token_fp = identity_input.token_fingerprint();

        // 从 descriptor 读取配置化超时——不使用硬编码魔术数字。
        // - start_timeout: Phase 1——等待 HTTP 服务器连通 + 鉴权通过
        // - model_load_timeout: Phase 2——等待 Model Ready（模型加载可能较慢）
        let timeouts = &entry.adapter.descriptor().timeouts;
        let start_timeout = timeouts.start_timeout;
        let model_load_timeout = timeouts.model_load_timeout;

        tracing::info!(
            engine = %engine_id,
            url = %health_url,
            token_fp = %token_fp,
            start_timeout_secs = start_timeout.as_secs(),
            model_load_timeout_secs = model_load_timeout.as_secs(),
            "开始两阶段 health 轮询"
        );

        // 单次 HTTP 请求超时——使用 start_timeout（Phase 1 连通+鉴权），
        // 不硬编码 5s。reqwest 对每次 .send() 应用此超时。
        let client = reqwest::Client::builder()
            .timeout(start_timeout)
            .build()
            .map_err(|e| {
                StartAttemptFailure::Fatal(LocalEngineError::with_detail(
                    LocalEngineErrorCode::Internal,
                    ErrorPhase::Health,
                    "HTTP client 构造失败",
                    format!("{e}"),
                ))
            })?;

        // 两阶段轮询：
        // Phase 1: start_timeout 内——等待 HTTP 2xx + 鉴权通过（身份匹配）
        // Phase 2: model_load_timeout 内——等待 Model Ready
        // 总轮询窗口 = start_timeout + model_load_timeout
        let interval = std::time::Duration::from_millis(500);
        let phase1_deadline = tokio::time::Instant::now() + start_timeout;
        let phase2_deadline = phase1_deadline + model_load_timeout;
        let mut attempt: u32 = 0;
        let mut phase1_passed = false;

        loop {
            attempt += 1;

            // ── 进程早退快速失败 + bind race 识别 ──
            // 子进程 bind 失败（probe-then-bind race）或其他启动期崩溃会立即退出；
            // 等满 start_timeout 才报错会无谓拖慢失败路径。
            {
                let snapshot = managed.snapshot().await;
                if let ProcessStatus::Exited { reason } = snapshot.status {
                    let tail: Vec<String> = managed
                        .log_history()
                        .await
                        .into_iter()
                        .rev()
                        .take(30)
                        .map(|l| l.text)
                        .collect();
                    let reason_text = format!("{reason:?}");
                    let tail_text = tail.join("\n");
                    if is_explicit_address_in_use(&reason_text)
                        || is_explicit_address_in_use(&tail_text)
                    {
                        return Err(StartAttemptFailure::BindRace {
                            detail: format!(
                                "子进程退出（{reason_text:?}），输出包含明确的地址占用错误；输出尾部:\n{tail_text}"
                            ),
                        });
                    }
                    return Err(StartAttemptFailure::Fatal(LocalEngineError::with_detail(
                        LocalEngineErrorCode::SpawnFailed,
                        ErrorPhase::Start,
                        "引擎进程启动后立即退出",
                        format!("退出原因: {reason_text}; 输出尾部:\n{tail_text}"),
                    )));
                }
            }

            let now = tokio::time::Instant::now();
            // 检查是否超时
            // Phase 1 未通过时检查 phase1_deadline；通过后检查 phase2_deadline
            let deadline = if phase1_passed {
                phase2_deadline
            } else {
                phase1_deadline
            };
            if now >= deadline {
                let phase = if phase1_passed { "model_load" } else { "start" };
                tracing::warn!(
                    engine = %engine_id,
                    attempt,
                    phase,
                    "health 轮询超时"
                );
                return Err(StartAttemptFailure::Fatal(LocalEngineError::with_detail(
                    LocalEngineErrorCode::Timeout,
                    ErrorPhase::Health,
                    "health 验证超时",
                    format!("{phase} 阶段在 {attempt} 次尝试后未通过"),
                )));
            }

            match client
                .get(&health_url)
                .header("X-Engine-Token", &token)
                .send()
                .await
            {
                Ok(resp) if resp.status().is_success() => {
                    let raw_health: serde_json::Value = match resp.json().await {
                        Ok(v) => v,
                        Err(e) => {
                            tracing::debug!(attempt, %e, "health 响应解析失败，重试");
                            tokio::time::sleep(interval).await;
                            continue;
                        }
                    };

                    tracing::debug!(attempt, raw = %raw_health, "health 响应");

                    // 两阶段验证：先身份（Phase 1），后 backend/model（Phase 2）
                    match self
                        .parse_and_verify_health(&raw_health, entry, identity_input)
                        .await
                    {
                        Ok(mapping) => {
                            // 身份验证通过——标记 Phase 1 完成
                            if !phase1_passed {
                                phase1_passed = true;
                                tracing::info!(
                                    engine = %engine_id,
                                    attempt,
                                    "Phase 1 通过：HTTP 连通 + 鉴权成功"
                                );
                            }

                            // 只在 model=Ready 时返回 Ok
                            if mapping.model == ModelHealth::Ready {
                                tracing::info!(
                                    attempt,
                                    service = ?mapping.service,
                                    model = ?mapping.model,
                                    "health 验证通过，Model Ready"
                                );
                                return Ok(mapping);
                            }
                            // Model 未 Ready——继续轮询（Phase 2）
                            tracing::debug!(
                                attempt,
                                model = ?mapping.model,
                                "model 尚未 Ready，继续等待"
                            );
                        }
                        Err(err) => {
                            // 身份不匹配/backend 错误——直接返回 Err
                            tracing::warn!(attempt, %err, "health 验证失败");
                            return Err(StartAttemptFailure::Fatal(err));
                        }
                    }
                }
                Ok(resp) => {
                    tracing::debug!(attempt, status = %resp.status(), "health 非 2xx，重试");
                }
                Err(e) => {
                    tracing::debug!(attempt, %e, "health 请求失败，重试");
                }
            }

            tokio::time::sleep(interval).await;
        }
    }

    /// 两阶段验证 health 响应：先身份，后 backend/model。
    ///
    /// **Phase 1**: 从 health 响应中提取回显的身份字段，
    /// 调用 `ServiceIdentityInput::verify` 核对 engine_id/instance_id/token/endpoint。
    /// 任一不匹配返回 `IdentityVerification` 错误。
    ///
    /// **Phase 2**: adapter `map_health` 映射后，验证 backend 一致性。
    /// backend 交叉不匹配（如 GPU↔CPU）返回 `BackendMismatch` 错误。
    /// ModelFailed 返回 `ModelNotReady` 错误。
    ///
    /// **requested/actual 语义（0.22.6.1）**：身份字段仍从第一次 health 起严格
    /// 校验；backend 最终一致性只在 actual backend 可观察后校验——未观察期间
    /// （模型 Loading/Idle）backend 观测缺失是合法状态；但 **Model Ready 而
    /// actual backend 缺失**视为协议不完整，拒绝通过。
    async fn parse_and_verify_health(
        &self,
        raw_health: &serde_json::Value,
        entry: &Arc<EngineEntry>,
        identity_input: &ServiceIdentityInput,
    ) -> Result<HealthMapping, LocalEngineError> {
        // Phase 1: 身份验证
        let identity_result = ServiceIdentityResult {
            engine_id: raw_health
                .get("engine_id")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
            instance_id: raw_health
                .get("instance_id")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
            token_fingerprint: raw_health
                .get("token_fingerprint")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
            endpoint: raw_health
                .get("endpoint")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
        };

        match identity_input.verify(&identity_result) {
            IdentityVerification::Verified => {}
            IdentityVerification::Mismatch(mismatch) => {
                return Err(LocalEngineError::with_detail(
                    LocalEngineErrorCode::IdentityVerification,
                    ErrorPhase::Health,
                    "服务身份不匹配",
                    mismatch.detail,
                ));
            }
        }

        // Phase 2: adapter 映射 + backend 验证
        let mapping = entry.adapter.map_health(raw_health);

        // ModelFailed 直接返回错误
        if mapping.model == ModelHealth::Failed {
            return Err(LocalEngineError::with_detail(
                LocalEngineErrorCode::ModelNotReady,
                ErrorPhase::Health,
                "模型加载失败",
                "health 回报 model=Failed",
            ));
        }

        // backend 最终一致性前置（0.22.6.1）：Model Ready 必须已观测到 actual backend
        require_backend_when_ready(&mapping)?;

        // ── 模型身份校验（model_id + model_revision + fingerprint） ──
        // 期望身份来自 **start 时冻结的 launch snapshot**——配置变化（selected
        // 改变）不影响正在运行的 active；删除模型也不影响本次运行的校验合同。
        // adapter 自管模型的引擎（snapshot.model = None）使用编译期 descriptor
        // 身份，fingerprint 由 health 契约负责校验。
        let descriptor = entry.adapter.descriptor();
        let engine_id = &descriptor.engine_id;

        let launch = entry.current_launch().await;
        let (expected_model_id, expected_revision, expected_fingerprint) = match launch.as_ref() {
            Some(snap) => match &snap.model {
                Some(m) => (
                    m.model_id.clone(),
                    m.revision.clone(),
                    m.fingerprint.clone(),
                ),
                None => (
                    descriptor.model_contract.model_id.clone(),
                    descriptor.model_contract.revision.clone(),
                    None,
                ),
            },
            None => {
                // launch snapshot 缺失（不应发生——start 后由 claim 保护）——fail-closed
                let _ = engine_id;
                return Err(LocalEngineError::with_detail(
                    LocalEngineErrorCode::Internal,
                    ErrorPhase::Health,
                    "运行实例状态缺失",
                    "health 验证期间 launch snapshot 不存在",
                ));
            }
        };

        if mapping.model == ModelHealth::Ready {
            match mapping.model_id.as_deref() {
                Some(health_model_id) if health_model_id == expected_model_id => {}
                Some(health_model_id) => {
                    return Err(LocalEngineError::with_detail(
                        LocalEngineErrorCode::IdentityVerification,
                        ErrorPhase::Health,
                        "model_id 不匹配",
                        format!(
                            "health 报告 model_id='{health_model_id}'，期望='{expected_model_id}'"
                        ),
                    ));
                }
                None => {
                    return Err(LocalEngineError::with_detail(
                        LocalEngineErrorCode::IdentityVerification,
                        ErrorPhase::Health,
                        "模型 Ready 但缺少 model_id",
                        "health 报告 model=Ready 但 model_id 为 None",
                    ));
                }
            }

            match mapping.model_revision.as_deref() {
                Some(health_revision) if health_revision == expected_revision => {}
                Some(health_revision) => {
                    return Err(LocalEngineError::with_detail(
                        LocalEngineErrorCode::IdentityVerification,
                        ErrorPhase::Health,
                        "model_revision 不匹配",
                        format!(
                            "health 报告 model_revision='{health_revision}'，期望='{expected_revision}'"
                        ),
                    ));
                }
                None => {
                    return Err(LocalEngineError::with_detail(
                        LocalEngineErrorCode::IdentityVerification,
                        ErrorPhase::Health,
                        "模型 Ready 但缺少 model_revision",
                        "health 报告 model=Ready 但 model_revision 为 None",
                    ));
                }
            }
        } else {
            if let Some(ref health_model_id) = mapping.model_id {
                if health_model_id != &expected_model_id {
                    return Err(LocalEngineError::with_detail(
                        LocalEngineErrorCode::IdentityVerification,
                        ErrorPhase::Health,
                        "model_id 不匹配",
                        format!(
                            "health 报告 model_id='{health_model_id}'，期望='{expected_model_id}'"
                        ),
                    ));
                }
            }

            if let Some(ref health_revision) = mapping.model_revision {
                if health_revision != &expected_revision {
                    return Err(LocalEngineError::with_detail(
                        LocalEngineErrorCode::IdentityVerification,
                        ErrorPhase::Health,
                        "model_revision 不匹配",
                        format!(
                            "health 报告 model_revision='{health_revision}'，期望='{expected_revision}'"
                        ),
                    ));
                }
            }
        }

        // Ready 必须有合法 64-hex fingerprint；managed 模式还必须与 manifest 一致。
        if mapping.model == ModelHealth::Ready {
            match &mapping.model_content_fingerprint {
                None => {
                    return Err(LocalEngineError::with_detail(
                        LocalEngineErrorCode::ModelNotReady,
                        ErrorPhase::Health,
                        "模型 Ready 但缺少 fingerprint",
                        "health 报告 model=Ready 但 model_content_fingerprint 为 None",
                    ));
                }
                Some(fp) if !is_valid_model_fingerprint(fp) => {
                    return Err(LocalEngineError::with_detail(
                        LocalEngineErrorCode::ModelNotReady,
                        ErrorPhase::Health,
                        "模型 Ready 但 fingerprint 无效",
                        "health 报告 model=Ready 但 model_content_fingerprint 不是 64 位小写 hex",
                    ));
                }
                Some(fp)
                    if expected_fingerprint
                        .as_ref()
                        .is_some_and(|expected| fp != expected) =>
                {
                    return Err(LocalEngineError::with_detail(
                        LocalEngineErrorCode::IdentityVerification,
                        ErrorPhase::Health,
                        "fingerprint 不匹配",
                        format!(
                            "health 报告 fingerprint='{fp}'，manifest 期望='{}'",
                            expected_fingerprint.as_deref().unwrap_or_default()
                        ),
                    ));
                }
                _ => {}
            }
        }

        // backend 一致性验证——期望来自 launch snapshot 冻结的 profile
        if let Some(ref obs) = mapping.backend {
            let profile = entry.current_profile().await;
            if let Some(ref profile) = profile {
                let verification = runtime::verify_backend_consistency(profile.backend, Some(obs));
                if verification.state == BackendState::Error {
                    return Err(LocalEngineError::with_detail(
                        LocalEngineErrorCode::BackendMismatch,
                        ErrorPhase::Health,
                        "backend 不匹配",
                        verification.mismatch_reason.unwrap_or_default(),
                    ));
                }
            }
        }

        Ok(mapping)
    }
}

pub(super) fn is_valid_model_fingerprint(fingerprint: &str) -> bool {
    fingerprint.len() == 64
        && fingerprint
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        && !fingerprint.bytes().all(|byte| byte == b'0')
}

/// backend 最终一致性前置（0.22.6.1 requested/actual 语义）。
///
/// - 模型 Loading/Idle 时 Python 只回传 `requested_backend`、不回传 `backend`，
///   映射结果 `backend=None` 是合法状态（actual backend 尚不可观察，最终一致）。
/// - **Model Ready 而 actual backend 缺失**视为协议不完整——不允许把字符串
///   CPU/CUDA 冒充真实设备观察，也禁止 Ready 后仍无实际后端观测。
pub(super) fn require_backend_when_ready(mapping: &HealthMapping) -> Result<(), LocalEngineError> {
    if mapping.model == ModelHealth::Ready && mapping.backend.is_none() {
        return Err(LocalEngineError::with_detail(
            LocalEngineErrorCode::BackendMismatch,
            ErrorPhase::Health,
            "模型 Ready 但缺少 actual backend 观测",
            "health 报告 model=Ready 但未回传 actual backend——拒绝通过，请求设备不得冒充实际执行后端",
        ));
    }
    Ok(())
}
