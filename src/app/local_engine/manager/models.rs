//! EngineManager 模型资产用例：
//! 安装 / 修复 / 删除 / 取消 / 查询模型与删除冲突检查（selected / launch snapshot）。

use super::*;

impl EngineManager {
    // ── 模型资产操作（从 ModelService 并入，单一业务真相）──────────────────
    //
    // 语义变化：
    // - 变更互斥由 EngineOperationCoordinator 承载（key = engine_id）——
    //   同一引擎的模型安装与环境安装/修复/启动/停止互斥；
    // - 删除冲突检查依据 **launch snapshot**（active）与配置真源（selected），
    //   不再用当前配置猜测运行中的模型；
    // - descriptor 默认模型只提供首次默认值，不构成删除保护。

    /// 读取引擎当前 selected 模型（配置真源）。
    fn read_selected_model(&self, engine_id: &EngineId) -> Option<String> {
        if engine_id.as_str() == super::super::funasr::FUNASR_ENGINE_ID {
            let m = crate::app::stt_config::get_stt_config()
                .local_engine
                .funasr_model;
            if m.is_empty() { None } else { Some(m) }
        } else {
            None
        }
    }

    /// 检查模型删除冲突（selected / active launch snapshot）。
    ///
    /// active 判定依据 launch snapshot 冻结的模型身份与 instance_id——
    /// 不根据当前配置猜测。descriptor 默认模型不构成冲突。
    async fn check_delete_conflict(
        &self,
        engine_id: &EngineId,
        model_id: &str,
    ) -> Option<ModelDeleteConflict> {
        let mut reasons = Vec::new();

        // selected（配置真源）
        if self.read_selected_model(engine_id).as_deref() == Some(model_id) {
            reasons.push(DeleteConflictReason::ReferencedByConfig {
                config_field: "funasr_model".to_string(),
                config_value: model_id.to_string(),
            });
        }

        // active（launch snapshot）
        if let Ok(entry) = self.get_entry(engine_id).await
            && let Some(launch) = entry.current_launch().await
            && let Some(ref m) = launch.model
            && m.model_id == model_id
        {
            reasons.push(DeleteConflictReason::ActiveInRunningInstance {
                instance_id: launch.identity.instance_id.clone(),
            });
        }

        if reasons.is_empty() {
            None
        } else {
            Some(ModelDeleteConflict {
                engine_id: engine_id.clone(),
                model_id: model_id.to_string(),
                reasons,
            })
        }
    }

    /// 列出引擎的所有模型候选及其当前状态（只读查询，无副作用）。
    ///
    /// 状态从磁盘 manifest 结构恢复（不做全量 hash）；
    /// is_selected 来自配置，is_active 来自 launch snapshot。
    pub async fn list_models(&self, engine_id: &EngineId) -> Vec<EngineModelStatus> {
        let descriptors = self.model_registry.list(engine_id);
        let selected = self.read_selected_model(engine_id);
        let launch_model = match self.get_entry(engine_id).await {
            Ok(entry) => entry
                .current_launch()
                .await
                .and_then(|l| l.model.map(|m| m.model_id)),
            Err(_) => None,
        };

        descriptors
            .iter()
            .map(|desc| {
                let asset_key = mstore::encode_asset_key(&desc.model_id);
                let mut status = match mstore::restore_model_state(engine_id, &asset_key) {
                    Ok(mstore::RestoredModelState::Installed { manifest, .. }) => {
                        let mut st = EngineModelStatus::not_installed(desc);
                        st.install_state = ModelInstallState::Installed;
                        st.verification_state = ModelVerificationState::Unverified;
                        st.cache_size_bytes = Some(manifest.payload_size_bytes);
                        st
                    }
                    Ok(mstore::RestoredModelState::Corrupted { .. }) => {
                        let mut st = EngineModelStatus::not_installed(desc);
                        st.install_state = ModelInstallState::NotInstalled;
                        st.verification_state = ModelVerificationState::Corrupted;
                        st.compatibility = ModelCompatibility::Unknown;
                        st
                    }
                    _ => EngineModelStatus::not_installed(desc),
                };
                status.is_selected = selected.as_deref() == Some(desc.model_id.as_str());
                status.is_active = launch_model.as_deref() == Some(desc.model_id.as_str());
                status
            })
            .collect()
    }

    /// 列出引擎**可选**（已安装、校验可用、当前兼容）的模型。
    ///
    /// "什么模型可选"是业务规则——由 EngineManager（单一业务真相）过滤，
    /// STT 选择入口（command 层）只做参数适配与投影，不复制过滤规则。
    ///
    /// 返回 `(descriptor, status)` 对；`is_selected` 已按配置真源填充。
    pub async fn list_selectable_models(
        &self,
        engine_id: &EngineId,
    ) -> Result<Vec<(EngineModelDescriptor, EngineModelStatus)>, LocalEngineError> {
        let models = self.list_models(engine_id).await;
        let mut result = Vec::new();
        for status in models {
            if !status.is_usable() {
                continue;
            }
            if !matches!(
                status.compatibility,
                ModelCompatibility::Compatible | ModelCompatibility::Unknown
            ) {
                continue;
            }
            let desc = self
                .model_registry
                .find(engine_id, &status.model_id)
                .ok_or_else(|| {
                    LocalEngineError::with_detail(
                        LocalEngineErrorCode::Internal,
                        ErrorPhase::Request,
                        "模型目录不一致",
                        format!(
                            "engine_id={}, model_id={} 有状态但无 descriptor",
                            engine_id, status.model_id
                        ),
                    )
                })?;
            result.push((desc.clone(), status));
        }
        Ok(result)
    }

    /// 获取单个模型状态。
    pub async fn get_model_status(
        &self,
        engine_id: &EngineId,
        model_id: &str,
    ) -> Result<EngineModelStatus, LocalEngineError> {
        let desc = self
            .model_registry
            .find(engine_id, model_id)
            .ok_or_else(|| {
                LocalEngineError::with_detail(
                    LocalEngineErrorCode::Unsupported,
                    ErrorPhase::Request,
                    "未知模型",
                    format!(
                        "engine_id={}, model_id={} 不在 allowlist",
                        engine_id, model_id
                    ),
                )
            })?;

        let asset_key = mstore::encode_asset_key(model_id);
        let mut status = match mstore::restore_model_state(engine_id, &asset_key) {
            Ok(mstore::RestoredModelState::Installed { manifest, .. }) => {
                let mut st = EngineModelStatus::not_installed(desc);
                st.install_state = ModelInstallState::Installed;
                st.verification_state = ModelVerificationState::Unverified;
                st.cache_size_bytes = Some(manifest.payload_size_bytes);
                st
            }
            Ok(mstore::RestoredModelState::Corrupted { .. }) => {
                let mut st = EngineModelStatus::not_installed(desc);
                st.verification_state = ModelVerificationState::Corrupted;
                st.compatibility = ModelCompatibility::Unknown;
                st
            }
            _ => EngineModelStatus::not_installed(desc),
        };
        status.is_selected = self.read_selected_model(engine_id).as_deref() == Some(model_id);
        if let Ok(entry) = self.get_entry(engine_id).await
            && let Some(launch) = entry.current_launch().await
            && let Some(ref m) = launch.model
        {
            status.is_active = m.model_id == model_id;
        }
        Ok(status)
    }

    /// 安装模型（真实事务：staging/下载/校验/提升）。
    ///
    /// 变更互斥：与同引擎其他变更操作（环境安装/修复/启停/其他模型操作）
    /// 通过 EngineOperationCoordinator 串行。
    pub async fn install_model(
        &self,
        engine_id: &EngineId,
        model_id: &str,
        operation_id: Option<String>,
    ) -> Result<ModelOperationResult, LocalEngineError> {
        self.execute_model_install_or_repair(engine_id, model_id, operation_id, false)
            .await
    }

    /// 修复模型（重下载 + 完整校验；保留旧 payload 直至新 payload 提升成功）。
    pub async fn repair_model(
        &self,
        engine_id: &EngineId,
        model_id: &str,
        operation_id: Option<String>,
    ) -> Result<ModelOperationResult, LocalEngineError> {
        self.execute_model_install_or_repair(engine_id, model_id, operation_id, true)
            .await
    }

    /// install/repair 共享事务体（差异只在 kind 与幂等短路）。
    async fn execute_model_install_or_repair(
        &self,
        engine_id: &EngineId,
        model_id: &str,
        operation_id: Option<String>,
        is_repair: bool,
    ) -> Result<ModelOperationResult, LocalEngineError> {
        let desc = self
            .model_registry
            .find(engine_id, model_id)
            .ok_or_else(|| {
                LocalEngineError::with_detail(
                    LocalEngineErrorCode::Unsupported,
                    ErrorPhase::Request,
                    "未知模型",
                    format!(
                        "engine_id={}, model_id={} 不在 allowlist",
                        engine_id, model_id
                    ),
                )
            })?;

        let kind = if is_repair {
            ModelOperationKind::Repair
        } else {
            ModelOperationKind::Install
        };

        // 已安装且非修复 → 幂等返回
        if !is_repair {
            let asset_key = mstore::encode_asset_key(model_id);
            if matches!(
                mstore::restore_model_state(engine_id, &asset_key),
                Ok(mstore::RestoredModelState::Installed { .. })
            ) {
                return Ok(ModelOperationResult {
                    engine_id: engine_id.to_string(),
                    model_id: model_id.to_string(),
                    operation_id: operation_id.unwrap_or_default(),
                    operation_kind: kind,
                    final_stage: ModelOperationStage::Done,
                    success: true,
                    error: None,
                });
            }
        }

        // claim 进程级操作（与同引擎所有变更互斥）
        let op_id = operation_id.unwrap_or_else(generate_operation_id);
        let guard = self.coordinator.try_claim(engine_id, &op_id)?;

        let slot_id = generate_install_id();
        let asset_key = mstore::encode_asset_key(model_id);

        // sink 必须在任何 staging 操作之前建立：路径校验、目录创建等下载前失败
        // 也要进入前端日志卡片，不能只留在后端 tracing。
        let sink =
            std::sync::Arc::new(super::super::model_installer::BroadcastingInstallSink::new(
                super::super::model_installer::BoundedInstallSink::new(500),
                Arc::clone(&self.event_port) as Arc<dyn EventPort>,
                engine_id.clone(),
                op_id.clone(),
            ));
        use super::super::model_installer::InstallSink as _ModelInstallSink;
        sink.emit_stage("preparing");

        // 模型状态真源 = 磁盘 manifest（list_models / get_model_status /
        // resolve_expected_model_identity 均从磁盘恢复）；下载/校验等瞬态阶段
        // 不再有内存缓存投影——它从未被任何消费者读取，且与磁盘真源存在漂移风险。

        // 清理孤儿 staging（claim 已保证无活跃操作，删除安全）
        let orphan_cleaned = tokio::task::spawn_blocking({
            let eid = engine_id.clone();
            let ak = asset_key.clone();
            move || mstore::cleanup_orphan_staging(&eid, &ak)
        })
        .await
        .unwrap_or(0);
        if orphan_cleaned > 0 {
            tracing::info!(
                engine_id = %engine_id,
                model_id = %model_id,
                count = orphan_cleaned,
                "已清理孤儿 staging 残留"
            );
        }

        // staging payload 目录
        let staging_payload_dir =
            match mstore::model_operation_staging_payload_dir(engine_id, &asset_key, &op_id) {
                Ok(p) => p,
                Err(e) => {
                    return Ok(self
                        .model_op_failed(
                            engine_id,
                            model_id,
                            &op_id,
                            kind,
                            &asset_key,
                            format!("staging 目录创建失败: {e}"),
                            &sink,
                        )
                        .await);
                }
            };
        if let Err(e) = tokio::fs::create_dir_all(&staging_payload_dir).await {
            return Ok(self
                .model_op_failed(
                    engine_id,
                    model_id,
                    &op_id,
                    kind,
                    &asset_key,
                    format!("staging 目录创建失败: {e}"),
                    &sink,
                )
                .await);
        }

        // 下载（worker 执行；sink 实时广播日志 + 内存缓冲）
        let download_result = self
            .model_worker
            .download_to_staging(
                engine_id,
                model_id,
                &desc.revision,
                &staging_payload_dir,
                guard.cancel_token().clone(),
                Some(Arc::clone(&sink) as Arc<dyn super::super::model_installer::InstallSink>),
            )
            .await;

        // 取消优先判定（claim 未释放——guard 仍持有）
        if guard.is_cancelled() {
            let _ = mstore::cleanup_staging(engine_id, &asset_key, &op_id);
            return Ok(ModelOperationResult {
                engine_id: engine_id.to_string(),
                model_id: model_id.to_string(),
                operation_id: op_id,
                operation_kind: kind,
                final_stage: ModelOperationStage::Cancelled,
                success: true,
                error: None,
            });
        }

        if let Err(e) = download_result {
            let _ = mstore::cleanup_staging(engine_id, &asset_key, &op_id);
            let tail = sink.tail_lines(15);
            let detail = if tail.is_empty() {
                e.to_string()
            } else {
                format!("{e}\n最近日志:\n{}", tail.join("\n"))
            };
            return Ok(ModelOperationResult {
                engine_id: engine_id.to_string(),
                model_id: model_id.to_string(),
                operation_id: op_id,
                operation_kind: kind,
                final_stage: ModelOperationStage::Failed,
                success: false,
                error: Some(LocalEngineError::with_detail(
                    e.to_code(),
                    if is_repair {
                        ErrorPhase::Repair
                    } else {
                        ErrorPhase::Install
                    },
                    "模型下载失败",
                    detail,
                )),
            });
        }

        // 完整 fingerprint 校验（GB 级 hash 在 blocking pool 执行）
        let fingerprint = match tokio::task::spawn_blocking({
            let dir = staging_payload_dir.clone();
            move || mstore::compute_content_fingerprint(&dir)
        })
        .await
        {
            Ok(Ok(fp)) => fp,
            Ok(Err(e)) => {
                let _ = mstore::cleanup_staging(engine_id, &asset_key, &op_id);
                return Ok(ModelOperationResult {
                    engine_id: engine_id.to_string(),
                    model_id: model_id.to_string(),
                    operation_id: op_id,
                    operation_kind: kind,
                    final_stage: ModelOperationStage::Failed,
                    success: false,
                    error: Some(LocalEngineError::with_detail(
                        LocalEngineErrorCode::ArtifactCorrupted,
                        ErrorPhase::Install,
                        "模型校验失败",
                        format!("{e}"),
                    )),
                });
            }
            Err(join_err) => {
                let _ = mstore::cleanup_staging(engine_id, &asset_key, &op_id);
                return Ok(ModelOperationResult {
                    engine_id: engine_id.to_string(),
                    model_id: model_id.to_string(),
                    operation_id: op_id,
                    operation_kind: kind,
                    final_stage: ModelOperationStage::Failed,
                    success: false,
                    error: Some(LocalEngineError::with_detail(
                        LocalEngineErrorCode::Internal,
                        ErrorPhase::Install,
                        "模型校验失败",
                        format!("fingerprint join 错误: {join_err}"),
                    )),
                });
            }
        };

        // manifest（保留来源、revision、checksum provenance、fingerprint、兼容 schema）
        let (source, checksum_source) = match download_result.as_ref() {
            Ok(outcome) => {
                let s = outcome.source.clone();
                match &outcome.checksum_source {
                    super::super::model_installer::ModelDownloadChecksumSource::Sha256(sha) => (
                        s,
                        crate::domain::local_engine::ChecksumSource::Sha256(sha.clone()),
                    ),
                }
            }
            Err(_) => unreachable!("download_result 已在上面处理"),
        };

        let downloaded_at_ms = runtime::now_ms();
        let manifest = mstore::ModelManifest {
            schema_version: mstore::MODEL_MANIFEST_SCHEMA_VERSION,
            engine_id: engine_id.clone(),
            model_id: model_id.to_string(),
            revision: desc.revision.clone(),
            source: match checksum_source {
                crate::domain::local_engine::ChecksumSource::Sha256(ref sha) => {
                    mstore::ModelSource::Sha256 {
                        sha256: sha.clone(),
                        source,
                        downloaded_at_ms,
                    }
                }
                _ => mstore::ModelSource::Unverified {
                    source,
                    downloaded_at_ms,
                },
            },
            slot_id: slot_id.clone(),
            installed_at_ms: downloaded_at_ms,
            content_fingerprint_algorithm: mstore::CONTENT_FINGERPRINT_ALGORITHM.to_string(),
            content_fingerprint: fingerprint.fingerprint.clone(),
            payload_size_bytes: fingerprint.total_size_bytes,
            file_count: fingerprint.file_count,
            compatibility_schema: desc.compatibility_schema,
            model_contract_identity: mstore::ModelContractIdentity {
                model_id: model_id.to_string(),
                revision: desc.revision.clone(),
                checksum_source_kind: match &desc.checksum_source {
                    crate::domain::local_engine::ChecksumSource::Sha256(_) => "sha256",
                    crate::domain::local_engine::ChecksumSource::DownloadSource { .. } => {
                        "download_source"
                    }
                    crate::domain::local_engine::ChecksumSource::Unverified => "unverified",
                }
                .to_string(),
            },
        };

        // 提交：已校验 staging → candidate slot → 原子切换 active.json。
        if let Err(e) = mstore::promote_staging_to_active_slot(
            engine_id, &asset_key, &slot_id, &op_id, &manifest,
        ) {
            let _ = mstore::cleanup_staging(engine_id, &asset_key, &op_id);
            return Ok(ModelOperationResult {
                engine_id: engine_id.to_string(),
                model_id: model_id.to_string(),
                operation_id: op_id,
                operation_kind: kind,
                final_stage: ModelOperationStage::Failed,
                success: false,
                error: Some(from_runtime(ErrorPhase::Install, "模型提升失败", &e)),
            });
        }

        // 稳定状态只保留一个 installed revision；失败删除记为有界 residue。
        let _ = mstore::cleanup_staging(engine_id, &asset_key, &op_id);
        let _ = mstore::cleanup_inactive_slots(engine_id, &asset_key, &slot_id);

        Ok(ModelOperationResult {
            engine_id: engine_id.to_string(),
            model_id: model_id.to_string(),
            operation_id: op_id,
            operation_kind: kind,
            final_stage: ModelOperationStage::Done,
            success: true,
            error: None,
        })
    }

    /// 模型操作早期失败的统一收尾（清 staging + 失败结果）。
    #[allow(clippy::too_many_arguments)] // 模型操作收尾需要全部上下文
    async fn model_op_failed(
        &self,
        engine_id: &EngineId,
        model_id: &str,
        op_id: &str,
        kind: ModelOperationKind,
        asset_key: &str,
        message: String,
        sink: &super::super::model_installer::BroadcastingInstallSink,
    ) -> ModelOperationResult {
        tracing::warn!(
            engine_id = %engine_id,
            model_id = %model_id,
            %message,
            "模型操作失败"
        );
        use super::super::model_installer::InstallSink as _;
        sink.emit_log(&format!("[ERROR] {message}"));
        sink.emit_stage("failed");
        let _ = mstore::cleanup_staging(engine_id, asset_key, op_id);
        ModelOperationResult {
            engine_id: engine_id.to_string(),
            model_id: model_id.to_string(),
            operation_id: op_id.to_string(),
            operation_kind: kind,
            final_stage: ModelOperationStage::Failed,
            success: false,
            error: Some(LocalEngineError::with_detail(
                LocalEngineErrorCode::InstallFailed,
                ErrorPhase::Install,
                "模型操作失败",
                message,
            )),
        }
    }

    /// 删除模型资产。
    ///
    /// 冲突判定：
    /// - selected（配置真源）→ 结构化冲突；
    /// - active（launch snapshot 冻结的模型身份 + instance_id）→ 结构化冲突；
    /// - descriptor 默认模型**不构成删除保护**（只提供首次默认值）。
    pub async fn delete_model(
        &self,
        engine_id: &EngineId,
        model_id: &str,
        operation_id: Option<String>,
    ) -> Result<ModelOperationResult, LocalEngineError> {
        self.model_registry
            .find(engine_id, model_id)
            .ok_or_else(|| {
                LocalEngineError::with_detail(
                    LocalEngineErrorCode::Unsupported,
                    ErrorPhase::Request,
                    "未知模型",
                    format!(
                        "engine_id={}, model_id={} 不在 allowlist",
                        engine_id, model_id
                    ),
                )
            })?;

        let asset_key = mstore::encode_asset_key(model_id);

        // 已安装检查（Corrupted 视为可删除——允许清理损坏资产）
        match mstore::restore_model_state(engine_id, &asset_key) {
            Ok(mstore::RestoredModelState::Installed { .. })
            | Ok(mstore::RestoredModelState::Corrupted { .. }) => {}
            Ok(mstore::RestoredModelState::NotInstalled) => {
                return Err(LocalEngineError::with_detail(
                    LocalEngineErrorCode::NotRunning,
                    ErrorPhase::Request,
                    "模型未安装，无需删除",
                    format!("engine_id={}, model_id={}", engine_id, model_id),
                ));
            }
            Err(e) => {
                return Err(LocalEngineError::with_detail(
                    LocalEngineErrorCode::Internal,
                    ErrorPhase::Request,
                    "模型状态读取失败",
                    format!("{e}"),
                ));
            }
        }

        // 冲突检查（selected / active launch snapshot）——结果由结构化冲突表达，
        // 不做状态缓存转移（磁盘 manifest 是唯一模型状态真源）
        if let Some(conflict) = self.check_delete_conflict(engine_id, model_id).await {
            return Ok(ModelOperationResult {
                engine_id: engine_id.to_string(),
                model_id: model_id.to_string(),
                operation_id: operation_id.unwrap_or_default(),
                operation_kind: ModelOperationKind::Delete,
                final_stage: ModelOperationStage::Failed,
                success: false,
                error: Some(conflict.to_error()),
            });
        }

        // claim 进程级操作
        let op_id = operation_id.unwrap_or_else(generate_operation_id);
        let _guard = self.coordinator.try_claim(engine_id, &op_id)?;

        let delete_result = tokio::task::spawn_blocking({
            let eid = engine_id.clone();
            let ak = asset_key.clone();
            move || mstore::delete_active_model(&eid, &ak)
        })
        .await;

        match delete_result {
            Ok(Ok(())) => Ok(ModelOperationResult {
                engine_id: engine_id.to_string(),
                model_id: model_id.to_string(),
                operation_id: op_id,
                operation_kind: ModelOperationKind::Delete,
                final_stage: ModelOperationStage::Done,
                success: true,
                error: None,
            }),
            Ok(Err(e)) => {
                // 删除失败不谎报已删除——磁盘 manifest 仍是真源
                Ok(ModelOperationResult {
                    engine_id: engine_id.to_string(),
                    model_id: model_id.to_string(),
                    operation_id: op_id,
                    operation_kind: ModelOperationKind::Delete,
                    final_stage: ModelOperationStage::Failed,
                    success: false,
                    error: Some(from_runtime(ErrorPhase::Cleanup, "模型删除失败", &e)),
                })
            }
            Err(join_err) => Err(LocalEngineError::with_detail(
                LocalEngineErrorCode::Internal,
                ErrorPhase::Cleanup,
                "模型删除失败",
                format!("spawn_blocking join 错误: {join_err}"),
            )),
        }
    }

    /// 取消模型操作（只触发匹配 operation_id 的 claim token）。
    ///
    /// 取消成功返回 `Cancelled` 终态结果（正常语义）；未命中活跃操作或
    /// id 错配返回结构化错误。
    pub async fn cancel_model_operation(
        &self,
        engine_id: &EngineId,
        model_id: &str,
        operation_id: &str,
    ) -> Result<ModelOperationResult, LocalEngineError> {
        match self.coordinator.cancel(engine_id, operation_id) {
            CancelOutcome::Cancelled => {
                tracing::info!(
                    engine = %engine_id,
                    model = %model_id,
                    op = %operation_id,
                    "模型操作取消信号已发送"
                );
                Ok(ModelOperationResult {
                    engine_id: engine_id.to_string(),
                    model_id: model_id.to_string(),
                    operation_id: operation_id.to_string(),
                    operation_kind: ModelOperationKind::Install,
                    final_stage: ModelOperationStage::Cancelled,
                    success: true,
                    error: None,
                })
            }
            other => {
                let detail = match &other {
                    CancelOutcome::NoActiveOperation => "当前没有进行中的模型操作".to_string(),
                    CancelOutcome::Mismatched {
                        current_operation_id,
                    } => format!(
                        "operation_id 不匹配: 当前={current_operation_id}, 请求={operation_id}"
                    ),
                    CancelOutcome::Cancelled => unreachable!("已在上一个分支处理"),
                };
                Err(LocalEngineError::with_detail(
                    LocalEngineErrorCode::NotRunning,
                    ErrorPhase::Request,
                    "取消请求未命中活跃操作",
                    detail,
                ))
            }
        }
    }

    /// 校验 health 回报的模型身份（commands 兼容入口）。
    #[allow(dead_code)] // 预留 API：commands 目前通过 health 链路间接调用，直接入口待接入
    pub fn verify_model_identity(
        &self,
        engine_id: &EngineId,
        model_id: &str,
        health_model_id: Option<&str>,
        health_revision: Option<&str>,
        health_fingerprint: Option<&str>,
    ) -> Result<crate::domain::local_engine::ModelIdentityVerification, LocalEngineError> {
        let desc = self
            .model_registry
            .find(engine_id, model_id)
            .ok_or_else(|| {
                LocalEngineError::with_detail(
                    LocalEngineErrorCode::Unsupported,
                    ErrorPhase::Request,
                    "未知模型",
                    format!(
                        "engine_id={}, model_id={} 不在 allowlist",
                        engine_id, model_id
                    ),
                )
            })?;
        desc.verify_health_identity(health_model_id, health_revision, health_fingerprint)
    }

    /// 读取已安装模型 manifest（commands 兼容入口）。
    #[allow(dead_code)] // 预留 API：commands 目前未直接调用，待后续诊断面板接入
    pub fn get_installed_manifest(
        &self,
        engine_id: &EngineId,
        model_id: &str,
    ) -> Result<mstore::ModelManifest, LocalEngineError> {
        let asset_key = mstore::encode_asset_key(model_id);
        let pointer = mstore::read_model_active_pointer(engine_id, &asset_key)
            .map_err(|e| from_runtime(ErrorPhase::Request, "读取模型指针失败", &e))?
            .ok_or_else(|| {
                LocalEngineError::with_detail(
                    LocalEngineErrorCode::ArtifactCorrupted,
                    ErrorPhase::Request,
                    "模型未安装",
                    "active.json 不存在",
                )
            })?;
        mstore::read_model_manifest(engine_id, &asset_key, &pointer.slot_id)
            .map_err(|e| from_runtime(ErrorPhase::Request, "读取模型 manifest 失败", &e))
    }
}
