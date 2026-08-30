//! EngineManager 环境部署用例：
//! install / repair / ensure_installed（InstallTransaction 事务）
//! 与后台环境探测（probe，fail-closed 恢复）。

use super::*;

#[allow(dead_code)]
impl EngineManager {
    // ── install ─────────────────────────────────────────────────────────────

    /// 安装/更新引擎环境。
    ///
    /// **唯一真源**：通过 `InstallTransaction` 事务执行安装
    /// （slot + journal，见 `infra/local_engine/deployment`）。
    ///
    /// 事务流程（由 `InstallTransaction::execute` 编排）：
    /// 1. journal begin（fail-closed 前提）
    /// 2. resolve_profile → 解析 compute preference
    /// 3. provider.prepare_environment → uv venv + pip install + self-test
    /// 4. promote → staging → candidate slot
    /// 5. atomic switch → `deployment.json`
    /// 6. 切换后验证失败 → 自动回滚 previous
    /// 7. 成功 → 删除旧 slot（占用记 residue），清 journal
    ///
    /// 安装前先停止运行中的引擎实例（安装持有操作 claim，串行安全）。
    /// candidate 内的完整 provider self-test 与切换后的结构验证通过后，
    /// 标记 environment=Ready。
    ///
    /// **终态协议**：返回 `EnvOperationEndState`——`Completed` 或 `Cancelled`
    /// （取消是正常终态，不包装成错误）；失败走 `Err(LocalEngineError)`。
    /// 无论哪种结束方式，状态快照的 `operation` 都归位 Idle——
    /// 操作结果由本返回值 + status 事件表达，不留 busy 残留。
    pub async fn install(
        &self,
        engine_id: &EngineId,
        config: AdapterConfig,
    ) -> Result<(Option<String>, EnvOperationEndState), LocalEngineError> {
        self.validate_engine_id(engine_id)?;

        // 等待后台探测完成，避免竞态重复安装
        self.await_probe(engine_id).await?;

        let entry = self.get_entry(engine_id).await?;

        // 先检查 adapter self_test——如果已通过，环境已就绪，无需重新安装。
        // self_test 可能等待 venv python 子进程——阻塞隔离到 spawn_blocking。
        let adapter = Arc::clone(&entry.adapter);
        let pre_test = tokio::task::spawn_blocking(move || adapter.self_test())
            .await
            .map_err(|e| {
                LocalEngineError::with_detail(
                    LocalEngineErrorCode::Internal,
                    ErrorPhase::Install,
                    "安装前检查失败",
                    format!("spawn_blocking join 错误: {e}"),
                )
            })?;
        if pre_test.passed {
            self.commit_status_internal(engine_id, None, |status| {
                status.environment = EnvironmentHealth::Ready;
            })
            .await?;
            tracing::info!(engine = %engine_id, "install 跳过（self-test 已通过，环境就绪）");
            return Ok((None, EnvOperationEndState::Completed));
        }

        // claim 进程级操作（原子 busy 检查 + 登记）
        let operation_id = generate_operation_id();
        let guard = self
            .coordinator
            .try_claim(engine_id, &operation_id)
            .map_err(|e| {
                tracing::info!(engine = %engine_id, %e, "install: 引擎操作进行中，拒绝");
                e
            })?;

        self.commit_status_internal(engine_id, Some(&operation_id), |status| {
            status.operation = EngineOperation {
                kind: OperationKind::Installing,
                operation_id: operation_id.clone(),
                stage: OperationStage::Preparing,
                cancellable: true,
            };
        })
        .await?;

        // 更新会切换 slot——先停止运行中的实例（复用当前 claim 的 operation_id）
        self.stop_internal(engine_id, &entry, &operation_id).await;

        let result = self
            .install_transaction_locked(engine_id, &config, &pre_test, &operation_id, &guard)
            .await;

        match result {
            Ok(()) => {
                // guard 仍持有 claim——归位 Idle 并广播终态后随 guard drop 释放 claim
                self.commit_status_internal(engine_id, Some(&operation_id), |status| {
                    status.environment = EnvironmentHealth::Ready;
                    status.clear_operation();
                })
                .await?;
                tracing::info!(engine = %engine_id, "install 完成（InstallTransaction + self-test passed）");
                Ok((Some(operation_id), EnvOperationEndState::Completed))
            }
            Err(err) => {
                // 取消是正常终态——事务已回滚，环境保持原状，不记 last_error
                if guard.is_cancelled() || err.code == LocalEngineErrorCode::Cancelled {
                    self.commit_status_internal(engine_id, Some(&operation_id), |status| {
                        status.clear_operation();
                    })
                    .await?;
                    tracing::info!(engine = %engine_id, op = %operation_id, "install 已取消（正常终态）");
                    return Ok((Some(operation_id), EnvOperationEndState::Cancelled));
                }
                // 安装失败——事务内部已回滚（old 部署不受影响），标记 Broken
                self.commit_status_internal(engine_id, Some(&operation_id), |status| {
                    status.last_error = Some(err.clone());
                    status.environment = EnvironmentHealth::Broken;
                    status.clear_operation();
                })
                .await?;
                Err(err)
            }
        }
    }

    /// install/repair 共享的事务执行体（调用方持有 operation claim）。
    async fn install_transaction_locked(
        &self,
        engine_id: &EngineId,
        config: &AdapterConfig,
        pre_test: &crate::domain::local_engine::AdapterSelfTest,
        operation_id: &str,
        guard: &OperationGuard,
    ) -> Result<(), LocalEngineError> {
        let preference = config.compute_preference.unwrap_or(ComputePreference::Auto);

        // 查找此引擎的 ProviderDescriptor
        let provider_descriptor = match self.provider_descriptors.get(engine_id) {
            Some(d) => d,
            None => {
                // 无 ProviderDescriptor（测试/未接线场景）——直接返回 SelfTestFailed。
                return Err(LocalEngineError::with_detail(
                    LocalEngineErrorCode::SelfTestFailed,
                    ErrorPhase::SelfTest,
                    "引擎 self-test 失败",
                    pre_test.failure_reason.clone().unwrap_or_default(),
                ));
            }
        };

        // 更新进度：正在安装
        self.commit_status_internal(engine_id, Some(operation_id), |status| {
            status.operation.stage = OperationStage::Downloading;
        })
        .await?;

        // 执行 InstallTransaction（slot + journal 部署事务）
        // 0.22.7：按 descriptor.runtime_kind 选择 provider
        // （PythonVenv → python_provider；ManagedBinary → binary_provider）。
        let sink_adapter = InstallSinkAdapter::new(
            self.event_port.clone(),
            engine_id.clone(),
            operation_id.to_string(),
        );
        let install_result = match provider_descriptor.runtime_kind {
            crate::infra::local_engine::runtime::RuntimePlan::ManagedBinary => {
                crate::infra::local_engine::providers::InstallTransaction::new(
                    provider_descriptor,
                    &self.binary_provider,
                )
                .execute(
                    operation_id,
                    preference,
                    Some(guard.cancel_token()),
                    Some(&sink_adapter),
                )
                .await
            }
            crate::infra::local_engine::runtime::RuntimePlan::PythonVenv => {
                crate::infra::local_engine::providers::InstallTransaction::new(
                    provider_descriptor,
                    &self.python_provider,
                )
                .execute(
                    operation_id,
                    preference,
                    Some(guard.cancel_token()),
                    Some(&sink_adapter),
                )
                .await
            }
        };

        match install_result {
            Ok(result) => {
                tracing::info!(
                    engine = %engine_id,
                    install_id = %result.install_id,
                    operation_id = %result.operation_id,
                    fell_back = result.fell_back,
                    "InstallTransaction 完成"
                );

                // candidate 已执行完整 provider self-test；切换后事务又核对了
                // manifest + artifact identity。这里不再启动第三个 Python 子进程。
                Ok(())
            }
            Err(e) => Err(from_runtime(
                ErrorPhase::Install,
                "环境安装失败（InstallTransaction）",
                &e,
            )),
        }
    }

    // ── Task B: 后台环境探测 ────────────────────────────────────────────────

    /// 构造后启动后台探测任务，为每个引擎检查 active deployment。
    ///
    /// 不阻塞主链路——`ensure_installed`/`start` 会 await 探测完成信号。
    /// 已安装旧用户启动 Blink 后，后台探测将自动识别 Ready。
    pub(super) fn spawn_background_probe(self: &Arc<Self>) {
        // 构造时 entries 刚写入，try_read 不会失败
        let entries = match self.entries.try_read() {
            Ok(e) => e,
            Err(_) => {
                tracing::warn!("spawn_background_probe: entries RwLock 被占用，跳过后台探测");
                return;
            }
        };
        for (engine_id, _entry) in entries.iter() {
            let engine_id = engine_id.clone();
            let svc = Arc::clone(self);

            tauri::async_runtime::spawn(async move {
                svc.probe_environment(&engine_id).await;
            });
        }
    }

    /// 单引擎环境探测逻辑。
    ///
    /// 状态判定规则（必须基于 deployment.json + manifest）：
    /// - 无 deployment.json → Missing（默认值，不改）
    /// - deployment.json 有效 + manifest 可读 + adapter self_test 通过 → Ready
    /// - manifest 损坏 / self_test 失败 → Broken
    ///
    /// **0.22.3 Task D**: 探测结果通过 `commit_status_internal` 统一提交，
    /// revision+1 并广播完整 snapshot（不再直接操作 RwLock）。
    /// **0.22.3 Task F**: probe 完成后设置 OnceCell + 发送 watch 信号，
    /// 所有等待者获得同一确定结果，不永久等待。
    async fn probe_environment(self: Arc<Self>, engine_id: &EngineId) {
        let entry = match self.get_entry(engine_id).await {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!(engine = %engine_id, %e, "probe_environment: 获取 entry 失败");
                // 即使失败也设置 probe_result + 发送 watch，让等待者获得确定结果
                if let Ok(entries) = self.entries.try_read() {
                    if let Some(entry) = entries.get(engine_id) {
                        let err = LocalEngineError::with_detail(
                            LocalEngineErrorCode::Internal,
                            ErrorPhase::Request,
                            "探测失败",
                            format!("{e}"),
                        );
                        let _ = entry.probe_result.set(Err(err));
                        let _ = entry.probe_tx.send(true);
                    }
                }
                return;
            }
        };

        let result = self.do_probe(engine_id, &entry).await;

        // 无论成功失败，都设置 probe_result + 发送 watch 信号——确定性协调
        let probe_outcome = result.as_ref().map(|()| ()).map_err(|e| {
            LocalEngineError::with_detail(
                LocalEngineErrorCode::Internal,
                ErrorPhase::Request,
                "探测失败",
                e.clone(),
            )
        });
        let _ = entry.probe_result.set(probe_outcome);
        // 通知所有等待者 probe 已完成
        let _ = entry.probe_tx.send(true);

        if let Err(e) = &result {
            tracing::warn!(
                engine = %engine_id,
                error = %e,
                "后台环境探测失败——保持默认 Missing 状态"
            );
        }
    }

    /// 实际探测逻辑（可返回错误用于日志）。
    ///
    /// 环境 Ready 判定必须同时满足：
    /// 1. 事务 journal 已按 fail-closed 规则恢复（`DeploymentStore::recover`）
    /// 2. active 指针存在且指向可读 manifest
    /// 3. adapter `self_test` 通过
    /// 缺少任何一项都不标记 Ready。
    ///
    /// **阻塞隔离**：recover（journal 扫描）、read_active（磁盘 IO）、
    /// self_test（venv python 子进程等待）全部在 `spawn_blocking` 内执行，
    /// async 上下文只做状态提交。
    async fn do_probe(&self, engine_id: &EngineId, entry: &EngineEntry) -> Result<(), String> {
        let adapter = Arc::clone(&entry.adapter);
        let eid = engine_id.clone();

        let outcome = tokio::task::spawn_blocking(move || probe_blocking(&eid, &adapter))
            .await
            .map_err(|e| format!("probe spawn_blocking join 错误: {e}"))??;

        match outcome {
            ProbeBlockingOutcome::NoDeployment => {
                tracing::debug!(engine = %engine_id, "探测: 无 deployment.json → Missing");
            }
            ProbeBlockingOutcome::Ready { install_id, slot } => {
                tracing::info!(
                    engine = %engine_id,
                    install_id = %install_id,
                    slot = %slot,
                    "探测: active 部署有效 + self_test 通过 → Ready"
                );
                self.commit_status_internal(engine_id, None, |status| {
                    status.environment = EnvironmentHealth::Ready;
                })
                .await
                .map_err(|e| format!("提交 Ready 状态失败: {e}"))?;
            }
            ProbeBlockingOutcome::Broken { reason } => {
                tracing::warn!(
                    engine = %engine_id,
                    reason = %reason,
                    "探测: self_test 失败 → Broken"
                );
                let _ = self
                    .commit_status_internal(engine_id, None, |status| {
                        status.environment = EnvironmentHealth::Broken;
                    })
                    .await;
            }
        }
        Ok(())
    }

    /// 等待后台探测完成——确定性协调，不轮询。
    ///
    /// `ensure_installed`/`start` 在执行前调用此方法，
    /// 确保不会在探测未完成时竞态重复安装。
    /// probe 完成（成功/失败）后所有等待者获得同一确定结果或 Err。
    async fn await_probe(&self, engine_id: &EngineId) -> Result<(), LocalEngineError> {
        let entry = self.get_entry(engine_id).await?;
        // OnceCell::get 在 set 后立即返回 Some(Result)，不阻塞
        if let Some(result) = entry.probe_result.get() {
            return result.clone();
        }
        // probe 未完成——await watch 直到完成
        // watch 不会永久阻塞：probe 任务完成（成功/失败）后发送 true
        let mut rx = entry.probe_watch.clone();
        // 先检查是否已完成（避免 race condition）
        if *rx.borrow() {
            return entry.probe_result.get().cloned().unwrap_or_else(|| {
                Err(LocalEngineError::with_detail(
                    LocalEngineErrorCode::Internal,
                    ErrorPhase::Request,
                    "探测状态不一致",
                    "probe_watch=true but probe_result=None",
                ))
            });
        }
        // 等待 probe 完成（watch 发送 true）
        let _ = rx.changed().await;
        // 完成后从 OnceCell 获取确定结果
        entry.probe_result.get().cloned().unwrap_or_else(|| {
            Err(LocalEngineError::with_detail(
                LocalEngineErrorCode::Internal,
                ErrorPhase::Request,
                "探测状态不一致",
                "probe_watch changed but probe_result=None",
            ))
        })
    }

    /// 确保 Python 环境已安装（如果未安装则安装，已安装则标记 Ready）。
    ///
    /// 用于 auto-start 和 start command 的前置检查。
    /// 如果环境已 Ready，直接返回 Ok。
    ///
    /// 环境 Ready 判定必须基于 deployment.json + manifest + self_test，
    /// 不能仅凭 self_test 通过就标记 Ready。如果 self_test 通过但没有受管部署，
    /// 说明环境是手动安装的（非 InstallTransaction 产生），仍需调用 install 建立受管部署。
    ///
    /// **阻塞隔离**：read_active（磁盘 IO）与 self_test（子进程等待）在
    /// `spawn_blocking` 内执行。
    pub async fn ensure_installed(
        &self,
        engine_id: &EngineId,
        config: AdapterConfig,
    ) -> Result<(), LocalEngineError> {
        self.validate_engine_id(engine_id)?;

        // 0.22.3 Task B: 等待后台探测完成，避免竞态重复安装
        self.await_probe(engine_id).await?;

        let entry = self.get_entry(engine_id).await?;

        // 检查当前环境状态
        {
            let status = entry.status.read().await;
            if status.environment == EnvironmentHealth::Ready {
                return Ok(());
            }
        }

        // 环境未就绪——验证受管部署（deployment.json + manifest）+ self_test。
        // 不能仅凭 self_test 通过就标记 Ready。磁盘 IO 与子进程等待在 blocking 线程。
        let adapter = Arc::clone(&entry.adapter);
        let eid = engine_id.clone();
        let verification = tokio::task::spawn_blocking(move || {
            let has_managed_deployment = match DeploymentStore::read_active(&eid) {
                Ok(Some(_)) => true,
                Ok(None) => false,
                Err(e) => {
                    tracing::warn!(
                        engine = %eid,
                        error = %e,
                        "ensure_installed: 读取 deployment.json 失败"
                    );
                    false
                }
            };
            let self_test = adapter.self_test();
            (has_managed_deployment, self_test)
        })
        .await
        .map_err(|e| {
            LocalEngineError::with_detail(
                LocalEngineErrorCode::Internal,
                ErrorPhase::Request,
                "环境检查失败",
                format!("spawn_blocking join 错误: {e}"),
            )
        })?;

        let (has_managed_deployment, self_test) = verification;
        if has_managed_deployment && self_test.passed {
            // self_test 通过 + 受管 generation 存在 → 标记 Ready
            self.commit_status_internal(engine_id, None, |status| {
                status.environment = EnvironmentHealth::Ready;
            })
            .await?;
            return Ok(());
        }

        // 没有受管 generation 或 self_test 未通过——需要安装
        self.install(engine_id, config).await.map(|_| ())
    }

    // ── repair / cleanup / storage / cancel（0.22.5 H2）─────────────────────

    /// 修复/更新引擎环境。
    ///
    /// repair 是一个完整的部署事务（复用 install 事务体）：
    /// 1. claim 操作（与所有变更互斥）
    /// 2. 停止运行中的实例
    /// 3. 在 candidate slot 中按当前配置重建环境
    /// 4. self-test + 切换后验证；失败自动回滚 previous
    /// 5. 成功删除旧 slot（占用记 residue）
    ///
    /// 不通过原地覆盖 active 部署"修复"。
    ///
    /// **终态协议**：同 `install`——返回 `EnvOperationEndState`，
    /// 取消是正常终态，结束后 `operation` 归位 Idle。
    pub async fn repair(
        &self,
        engine_id: &EngineId,
    ) -> Result<(Option<String>, EnvOperationEndState), LocalEngineError> {
        self.validate_engine_id(engine_id)?;
        let entry = self.get_entry(engine_id).await?;

        // claim 进程级操作（原子 busy 检查 + 登记）
        let operation_id = generate_operation_id();
        let guard = self.coordinator.try_claim(engine_id, &operation_id)?;

        self.commit_status_internal(engine_id, Some(&operation_id), |status| {
            status.operation = EngineOperation {
                kind: OperationKind::Repairing,
                operation_id: operation_id.clone(),
                stage: OperationStage::Preparing,
                cancellable: true,
            };
        })
        .await?;

        // 读取当前配置
        let config = self.read_adapter_config_for_engine(engine_id);

        // 无 ProviderDescriptor 时退化为 self_test 验证
        // （self_test 可能等待 venv python 子进程——阻塞隔离）
        if self.provider_descriptors.get(engine_id).is_none() {
            let adapter = Arc::clone(&entry.adapter);
            let self_test = tokio::task::spawn_blocking(move || adapter.self_test())
                .await
                .map_err(|e| {
                    LocalEngineError::with_detail(
                        LocalEngineErrorCode::Internal,
                        ErrorPhase::Repair,
                        "修复检查失败",
                        format!("spawn_blocking join 错误: {e}"),
                    )
                })?;
            if !self_test.passed {
                let err = LocalEngineError::with_detail(
                    LocalEngineErrorCode::SelfTestFailed,
                    ErrorPhase::Repair,
                    "修复后 self-test 仍失败",
                    self_test.failure_reason.unwrap_or_default(),
                );
                self.commit_status_internal(engine_id, Some(&operation_id), |status| {
                    status.last_error = Some(err.clone());
                    status.clear_operation();
                })
                .await?;
                return Err(err);
            }

            self.commit_status_internal(engine_id, Some(&operation_id), |status| {
                status.environment = EnvironmentHealth::Ready;
                status.clear_operation();
            })
            .await?;
            tracing::info!(engine = %engine_id, "repair 完成（self-test 降级路径）");
            return Ok((None, EnvOperationEndState::Completed));
        }

        let pre_test = {
            let adapter = Arc::clone(&entry.adapter);
            tokio::task::spawn_blocking(move || adapter.self_test())
                .await
                .map_err(|e| {
                    LocalEngineError::with_detail(
                        LocalEngineErrorCode::Internal,
                        ErrorPhase::Repair,
                        "修复检查失败",
                        format!("spawn_blocking join 错误: {e}"),
                    )
                })?
        };

        // 更新会切换 slot——先停止运行中的实例（复用当前 claim 的 operation_id）
        self.stop_internal(engine_id, &entry, &operation_id).await;

        let result = self
            .install_transaction_locked(engine_id, &config, &pre_test, &operation_id, &guard)
            .await;

        match result {
            Ok(()) => {
                self.commit_status_internal(engine_id, Some(&operation_id), |status| {
                    status.environment = EnvironmentHealth::Ready;
                    status.clear_operation();
                })
                .await?;
                tracing::info!(engine = %engine_id, "repair 完成（新部署已切换，旧 slot 已清理）");
                Ok((Some(operation_id), EnvOperationEndState::Completed))
            }
            Err(err) => {
                // 取消是正常终态——事务已回滚，不记 last_error
                if guard.is_cancelled() || err.code == LocalEngineErrorCode::Cancelled {
                    self.commit_status_internal(engine_id, Some(&operation_id), |status| {
                        status.clear_operation();
                    })
                    .await?;
                    tracing::info!(engine = %engine_id, op = %operation_id, "repair 已取消（正常终态）");
                    return Ok((Some(operation_id), EnvOperationEndState::Cancelled));
                }
                // 事务内部已回滚——active 部署不受影响
                self.commit_status_internal(engine_id, Some(&operation_id), |status| {
                    status.last_error = Some(err.clone());
                    status.clear_operation();
                })
                .await?;
                Err(err)
            }
        }
    }

    /// 从配置真源读取 AdapterConfig。
    ///
    /// 真源在 [`super::super::config_source`]——commands/maintenance/wiring 与本服务
    /// 共用同一构造入口，避免归一化规则（如 funasr device=cuda→Cpu）漂移。
    fn read_adapter_config_for_engine(&self, engine_id: &EngineId) -> AdapterConfig {
        super::super::config_source::adapter_config_for_engine(engine_id)
            .unwrap_or_else(AdapterConfig::new)
    }
}

// ── 启动恢复探测（阻塞隔离，0.22.6 phase B）───────────────────────────────

/// probe 阻塞段的判定结果——async 上下文只负责按此提交状态。
///
/// 铁则：探测是**只读恢复**——只做 fail-closed 事务收尾和结构校验，
/// 不同步 hash GB 模型、不启动 Python/OCR 服务进程、不进入主链路。
#[derive(Debug, Clone, PartialEq, Eq)]
enum ProbeBlockingOutcome {
    /// 无 active 部署——保持默认 Missing。
    NoDeployment,
    /// active 部署有效 + self_test 通过 → Ready。
    Ready { install_id: String, slot: String },
    /// active 部署存在但 self_test 失败 → Broken。
    Broken { reason: String },
}

/// probe 的全部阻塞工作：fail-closed 事务恢复 + active 部署读取 + self_test。
///
/// 必须在 `spawn_blocking` 中调用——journal 遍历、JSON 读取和
/// self_test 的 venv python 子进程等待都是阻塞操作。
fn probe_blocking(
    engine_id: &EngineId,
    adapter: &Arc<dyn LocalEngineAdapter>,
) -> Result<ProbeBlockingOutcome, String> {
    // 1. 崩溃恢复：journal 存在即事务未收尾，按恢复表回滚/收尾（fail-closed）。
    let recovery = DeploymentStore::recover(engine_id).map_err(|e| format!("部署恢复失败: {e}"))?;
    match recovery {
        crate::infra::local_engine::deployment::RecoveryOutcome::Stable => {}
        other => {
            tracing::warn!(engine = %engine_id, outcome = ?other, "探测: 已恢复未收尾事务");
        }
    }

    // 2. 读 active 部署（结构校验，不做全量 hash）。
    let active = DeploymentStore::read_active(engine_id)
        .map_err(|e| format!("读取 deployment.json 失败: {e}"))?;
    let Some((pointer, _manifest)) = active else {
        return Ok(ProbeBlockingOutcome::NoDeployment);
    };

    // 3. self_test（venv python 子进程等待——阻塞，必须在 blocking 线程）。
    let self_test = adapter.self_test();
    if self_test.passed {
        Ok(ProbeBlockingOutcome::Ready {
            install_id: pointer.install_id,
            slot: pointer.slot,
        })
    } else {
        Ok(ProbeBlockingOutcome::Broken {
            reason: self_test
                .failure_reason
                .unwrap_or_else(|| "unknown".to_string()),
        })
    }
}
