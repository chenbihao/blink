//! 跨 runtime 模型切换事务（0.22.9 Handoff 08）。
//!
//! `GGUF ↔ ONNX` 切换是**跨实现事务**——旧实例（GGUF worker 或 ONNX worker）
//! 与目标实例的 runtime、部署空间、连接通道都不同，不能依赖单进程内的
//! 原子换模。本模块实现 handoff 定案的事务序列：
//!
//! ```text
//! 1. 冻结旧 snapshot（launch snapshot = active；selected = 配置真源）
//! 2. 验证目标模型和 deployment（fail-closed，不动任何状态）
//! 3. stop old（优雅：GGUF NDJSON shutdown / ONNX Quit；Job Object 兜底）
//! 4. commit selected target（SelectedModelStore：DB + 缓存 + 事件）
//! 5. start target（内部冻结 → Ready → 提交 active）
//! 6. Ready 后提交 active——start 成功即 Healthy + launch snapshot，无额外状态
//! 7. 目标失败 → 恢复旧 selected 并按冻结 snapshot 重启旧模型
//! 8. 恢复也失败 → selected=旧模型；active=None；同时返回
//!    target failure 和 rollback failure（SwitchModelFailure::RollbackFailed）
//! ```
//!
//! ## 互斥与取消
//!
//! - 整个事务持有 engine 的操作 claim（stop/commit/start/rollback 全程
//!   串行，其他安装/启停/模型操作在事务期间被拒绝）——复用 `stop_internal`
//!   与 `start_internal`（无二级 claim）。
//! - 事务期间收到 `cancel_operation` 信号不中断事务：stop/start 不消费
//!   该 token，事务要么完整成功要么走回滚——**取消不产生半状态**。
//!
//! ## 配置提交端口
//!
//! manager 不接触 DB/AppHandle——selected 的读/写经 [`SelectedModelStore`]
//! 端口（wiring 层注入生产实现：ConfigStore 持久化 + 内存缓存 + 事件广播；
//! 测试注入内存 fake）。

use super::*;

/// 本地 STT selected 模型存储端口（切换事务的配置提交/回写）。
///
/// 生产实现（wiring 层）：读 SttConfig 缓存；写 = ConfigStore 持久化 +
/// `update_cache` + `CONFIG_CHANGED` 广播（与 `set_local_stt_selection`
/// 的三个字段同步语义一致：`local_stt_selection` / `local_model_id` /
/// `local_engine.funasr_model`）。
#[async_trait::async_trait]
pub trait SelectedModelStore: Send + Sync {
    /// 读取当前 selected 模型 id（None = 未选择）。
    fn read_selected(&self) -> Option<String>;

    /// 提交 selected 模型 id（持久化 + 缓存 + 事件广播）。
    async fn commit_selected(&self, model_id: &str) -> Result<(), String>;
}

/// 切换事务结果。
#[derive(Debug, Clone)]
pub enum SwitchModelOutcome {
    /// 目标模型已运行（Ready，active 已提交）。
    Completed {
        /// 目标 implementation。
        implementation: ImplementationId,
    },
    /// 引擎未运行——只提交 selected，不自动启动（保持现有选择语义）。
    CommittedSelectedOnly {
        /// 目标 implementation。
        implementation: ImplementationId,
    },
    /// 目标失败，已恢复旧 selected 并成功重启旧模型。
    RolledBack {
        /// 目标启动失败原因（已随状态写入 last_error 后又被重启清除前的值，
        /// 此处保留给调用方呈现）。
        target_error: LocalEngineError,
        /// 恢复后运行的旧模型 id。
        restored_model: Option<String>,
    },
}

/// 切换事务失败。
#[derive(Debug, Clone)]
pub enum SwitchModelFailure {
    /// 目标验证/提交/启动失败——引擎回到事务前状态或已恢复旧模型失败前的
    /// 稳定态（具体见变体）。
    Target(LocalEngineError),
    /// 目标失败且回滚也失败：selected=旧模型；active=None；
    /// **同时携带 target failure 与 rollback failure**（Handoff §跨8）。
    RollbackFailed {
        target_error: LocalEngineError,
        rollback_error: LocalEngineError,
    },
}

impl SwitchModelFailure {
    fn target(err: LocalEngineError) -> Self {
        Self::Target(err)
    }
}

impl EngineManager {
    /// 跨 runtime 模型切换事务（GGUF ↔ ONNX，Handoff 08）。
    ///
    /// 语义：
    /// - 引擎运行中：stop old → commit selected → start target → Ready；
    ///   失败回滚（恢复旧 selected + 按冻结 snapshot 重启）。
    /// - 引擎未运行：只验证目标 + commit selected（不自动启动）。
    /// - 目标已是 active：幂等（只同步 selected）。
    pub async fn switch_model(
        &self,
        engine_id: &EngineId,
        target_model_id: &str,
    ) -> Result<SwitchModelOutcome, SwitchModelFailure> {
        let fail = SwitchModelFailure::target;

        self.validate_engine_id(engine_id).map_err(fail)?;
        let entry = self.get_entry(engine_id).await.map_err(fail)?;
        let store = self.selected_store().map_err(fail)?;

        // claim 进程级操作——事务全程串行（stop_internal/start_internal
        // 复用本 operation_id 提交状态，不产生二级 claim）。
        let operation_id = generate_operation_id();
        let _guard = self
            .coordinator
            .try_claim(engine_id, &operation_id)
            .map_err(fail)?;

        // ── 1. 冻结旧 snapshot（active）与旧 selected ────────────────────
        let old_launch = entry.current_launch().await;
        let old_model = old_launch
            .as_ref()
            .and_then(|l| l.model.as_ref().map(|m| m.model_id.clone()));
        let old_selected = store.read_selected();
        let was_running = old_launch.is_some();

        tracing::info!(
            engine = %engine_id,
            target = %target_model_id,
            old_active = ?old_model,
            old_selected = ?old_selected,
            was_running,
            "模型切换事务：开始（冻结旧 snapshot）"
        );

        // ── 2. 验证目标模型和 deployment（fail-closed，不动任何状态）──────
        let target_implementation = self
            .verify_switch_target(engine_id, target_model_id)
            .await
            .map_err(fail)?;

        // ── 引擎未运行：只提交 selected（不自动启动）────────────────────
        if !was_running {
            store.commit_selected(target_model_id).await.map_err(|e| {
                fail(LocalEngineError::with_detail(
                    LocalEngineErrorCode::InvalidConfig,
                    ErrorPhase::Config,
                    "selected 提交失败",
                    e,
                ))
            })?;
            tracing::info!(
                engine = %engine_id,
                target = %target_model_id,
                "模型切换事务：引擎未运行，已提交 selected（下次启动生效）"
            );
            return Ok(SwitchModelOutcome::CommittedSelectedOnly {
                implementation: target_implementation,
            });
        }

        // ── 幂等：目标已是 active ────────────────────────────────────────
        if old_model.as_deref() == Some(target_model_id) {
            store.commit_selected(target_model_id).await.map_err(|e| {
                fail(LocalEngineError::with_detail(
                    LocalEngineErrorCode::InvalidConfig,
                    ErrorPhase::Config,
                    "selected 提交失败",
                    e,
                ))
            })?;
            tracing::info!(
                engine = %engine_id,
                target = %target_model_id,
                "模型切换事务：目标已是 active，幂等返回"
            );
            return Ok(SwitchModelOutcome::Completed {
                implementation: target_implementation,
            });
        }

        // ── 3. stop old ──────────────────────────────────────────────────
        self.stop_internal(engine_id, &entry, &operation_id).await;

        // ── 4. commit selected target ────────────────────────────────────
        if let Err(commit_err) = store.commit_selected(target_model_id).await {
            let err = LocalEngineError::with_detail(
                LocalEngineErrorCode::InvalidConfig,
                ErrorPhase::Config,
                "selected 提交失败",
                commit_err,
            );
            return self
                .rollback_switch(
                    engine_id,
                    &entry,
                    &store,
                    &old_launch,
                    old_model,
                    old_selected,
                    was_running,
                    err,
                    &operation_id,
                )
                .await;
        }

        // ── 5. start target（内部：冻结 → Ready → 提交 active）────────────
        // 无配置投影的引擎（测试 fake 等）退化为默认配置——与
        // read_adapter_config_for_engine 同语义
        let config =
            super::super::config_source::adapter_config_for_engine(engine_id).unwrap_or_default();
        match self
            .start_internal(engine_id, &entry, config, &operation_id)
            .await
        {
            Ok(()) => {
                tracing::info!(
                    engine = %engine_id,
                    target = %target_model_id,
                    implementation = %target_implementation,
                    "模型切换事务：目标模型 Ready，active 已提交"
                );
                Ok(SwitchModelOutcome::Completed {
                    implementation: target_implementation,
                })
            }
            Err(target_err) => {
                tracing::warn!(
                    engine = %engine_id,
                    target = %target_model_id,
                    error = %target_err,
                    "模型切换事务：目标启动失败，开始回滚"
                );
                self.rollback_switch(
                    engine_id,
                    &entry,
                    &store,
                    &old_launch,
                    old_model,
                    old_selected,
                    was_running,
                    target_err,
                    &operation_id,
                )
                .await
            }
        }
    }

    /// 验证切换目标：模型在目录中、绑定表可解析 implementation、
    /// 资产已安装（GGUF = model_storage；ONNX = per-implementation 部署）。
    ///
    /// 只读校验——不停止实例、不提交配置（事务第 2 步）。
    async fn verify_switch_target(
        &self,
        engine_id: &EngineId,
        model_id: &str,
    ) -> Result<ImplementationId, LocalEngineError> {
        // 模型在 allowlist 中
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

        // 绑定表解析 implementation（fail-closed，不静默换模）
        let implementation = self
            .resolve_implementation_for_model(engine_id, Some(model_id))?
            .ok_or_else(|| {
                LocalEngineError::with_detail(
                    LocalEngineErrorCode::InvalidConfig,
                    ErrorPhase::Config,
                    "模型未绑定本地 implementation",
                    format!("engine '{engine_id}' 的模型 '{model_id}' 无 implementation 绑定"),
                )
            })?;

        // 资产已安装（磁盘真源）：
        // - per-implementation 实现（ParaformerOnline 等）→ 该 implementation
        //   部署空间的 active manifest（闭合映射决定空间归属）；
        // - 其余（GGUF）→ model_storage。
        let eid = engine_id.clone();
        let mid = model_id.to_string();
        let impl_for_check = implementation;
        tokio::task::spawn_blocking(move || -> Result<(), String> {
            if impl_for_check == ImplementationId::ParaformerOnnxWorker {
                let space = super::lifecycle::deployment_space_for(&eid, Some(impl_for_check));
                let Some((_pointer, manifest)) = DeploymentStore::read_active(&space)
                    .map_err(|e| format!("读取部署失败: {e}"))?
                else {
                    return Err(format!(
                        "模型未安装: {mid}（部署空间内无 active deployment）"
                    ));
                };
                if manifest.model_contract.model_id != mid {
                    return Err(format!(
                        "部署 manifest 模型不一致: manifest='{}', 期望='{mid}'",
                        manifest.model_contract.model_id
                    ));
                }
                return Ok(());
            }
            let asset_key = mstore::encode_asset_key(&mid);
            match mstore::restore_model_state(&eid, &asset_key) {
                Ok(mstore::RestoredModelState::Installed { .. }) => Ok(()),
                Ok(mstore::RestoredModelState::Corrupted { reason, .. }) => {
                    Err(format!("模型已损坏: {reason}"))
                }
                _ => Err(format!("模型未安装: {mid}")),
            }
        })
        .await
        .map_err(|e| {
            LocalEngineError::with_detail(
                LocalEngineErrorCode::Internal,
                ErrorPhase::Request,
                "目标模型校验失败",
                format!("spawn_blocking join 错误: {e}"),
            )
        })?
        .map_err(|reason| {
            LocalEngineError::with_detail(
                LocalEngineErrorCode::ModelNotReady,
                ErrorPhase::Request,
                "目标模型未就绪",
                reason,
            )
        })?;

        Ok(implementation)
    }

    /// 切换失败回滚（事务第 7/8 步）：
    /// 恢复旧 selected → 按冻结 snapshot 重启旧模型 → 失败则双错误返回。
    #[allow(clippy::too_many_arguments)]
    async fn rollback_switch(
        &self,
        engine_id: &EngineId,
        entry: &Arc<EngineEntry>,
        store: &std::sync::Arc<dyn SelectedModelStore>,
        _old_launch: &Option<LaunchSnapshot>,
        old_model: Option<String>,
        old_selected: Option<String>,
        was_running: bool,
        target_error: LocalEngineError,
        operation_id: &str,
    ) -> Result<SwitchModelOutcome, SwitchModelFailure> {
        // ── 7a. 恢复旧 selected ────────────────────────────────────────
        let mut rollback_error: Option<LocalEngineError> = None;
        if let Some(ref old) = old_selected
            && let Err(e) = store.commit_selected(old).await
        {
            rollback_error = Some(LocalEngineError::with_detail(
                LocalEngineErrorCode::InvalidConfig,
                ErrorPhase::Config,
                "回滚 selected 失败",
                e,
            ));
        }

        // ── 7b. 按冻结 snapshot 重启旧模型（仅事务前有运行实例时）──────
        // 重启目标 = 冻结 snapshot 的模型（正常路径与旧 selected 一致）
        if rollback_error.is_none() && was_running {
            let restart_model = old_model.clone().or(old_selected);
            if let Some(ref model) = restart_model {
                // selected 已恢复为旧模型——config_source 读到的即旧模型；
                // 与冻结 snapshot 不一致（运行期 selected 漂移）时以 snapshot
                // 为准先行纠正，保证"按冻结 snapshot 重启"。
                let selected_now = store.read_selected();
                if selected_now.as_deref() != Some(model.as_str())
                    && let Err(e) = store.commit_selected(model).await
                {
                    rollback_error = Some(LocalEngineError::with_detail(
                        LocalEngineErrorCode::InvalidConfig,
                        ErrorPhase::Config,
                        "回滚 selected 失败（snapshot 纠正）",
                        e,
                    ));
                }
            }
            if rollback_error.is_none() {
                // 无配置投影的引擎（测试 fake 等）退化为默认配置——与
                // read_adapter_config_for_engine 同语义
                let config = super::super::config_source::adapter_config_for_engine(engine_id)
                    .unwrap_or_default();
                match self
                    .start_internal(engine_id, entry, config, operation_id)
                    .await
                {
                    Ok(()) => {
                        tracing::info!(
                            engine = %engine_id,
                            restored = ?old_model,
                            "模型切换事务：已恢复旧模型运行"
                        );
                        return Ok(SwitchModelOutcome::RolledBack {
                            target_error,
                            restored_model: old_model.clone(),
                        });
                    }
                    Err(e) => {
                        tracing::error!(
                            engine = %engine_id,
                            error = %e,
                            "模型切换事务：旧模型重启失败"
                        );
                        rollback_error = Some(e);
                    }
                }
            }
        }

        // ── 8. 恢复也失败：selected=旧模型；active=None；双错误返回 ──────
        // active=None 已由 stop/start 失败的 rollback 路径收敛（引擎停止态、
        // active_implementation=None）；此处把回滚错误写入 last_error 留痕。
        let rollback_error = rollback_error.unwrap_or_else(|| {
            LocalEngineError::with_detail(
                LocalEngineErrorCode::NotRunning,
                ErrorPhase::Stop,
                "回滚未执行",
                "事务前无运行实例且回滚未重启",
            )
        });
        let _ = self
            .commit_status_internal(engine_id, Some(operation_id), |status| {
                status.desired = DesiredState::Stopped;
                status.active_implementation = None;
                status.last_error = Some(rollback_error.clone());
            })
            .await;
        tracing::error!(
            engine = %engine_id,
            target_error = %target_error,
            rollback_error = %rollback_error,
            "模型切换事务：回滚也失败——selected 已恢复旧值，active=None，双失败已记录"
        );
        Err(SwitchModelFailure::RollbackFailed {
            target_error,
            rollback_error,
        })
    }
}
