//! EngineManager 进程生命周期用例：
//! start / stop / 条件停止、回滚（rollback_started_instance）、exit monitor、
//! lease 写入与退出回收（shutdown_all / shutdown_all_blocking）。

use super::*;

use super::logs::pump_logs_to_event_port;

#[allow(dead_code)]
impl EngineManager {
    // ── start ───────────────────────────────────────────────────────────────

    /// 启动引擎服务。
    ///
    /// **start 的成功定义**：只有在 token health 的 engine_id/instance_id/backend
    /// 校验通过后，且 model 变为 Ready，才返回 Ok。
    /// process spawned 不能直接等价为 Healthy。
    /// model Ready 由 adapter health 映射产生。
    ///
    /// **任何失败分支都执行 rollback_started_instance 并返回 Err**——
    /// timeout/mismatch/backend 错误/ModelFailed/health 不可达全部返回 Err。
    ///
    /// 幂等：如果 desired 已为 Running 且进程活跃，直接返回 Ok。
    /// 迟到的 health/task/exit 不能覆盖新实例。
    pub async fn start(
        &self,
        engine_id: &EngineId,
        config: AdapterConfig,
    ) -> Result<(), LocalEngineError> {
        self.validate_engine_id(engine_id)?;
        let entry = self.get_entry(engine_id).await?;

        // claim 进程级操作（与 install/repair/模型操作互斥，key = engine_id）
        let operation_id = generate_operation_id();
        let _guard = self.coordinator.try_claim(engine_id, &operation_id)?;

        // 幂等检查：desired=Running 且进程活跃 → 直接返回
        {
            let status = entry.status.read().await;
            if status.desired == DesiredState::Running && status.is_process_active() {
                tracing::debug!(engine = %engine_id, "start 幂等：已 Running/Starting");
                return Ok(());
            }
        }

        // 环境检查
        {
            let status = entry.status.read().await;
            if status.environment != crate::domain::local_engine::EnvironmentHealth::Ready {
                return Err(LocalEngineError::with_detail(
                    LocalEngineErrorCode::EnvironmentMissing,
                    ErrorPhase::Start,
                    "环境未就绪，请先安装",
                    format!("environment={:?}", status.environment),
                ));
            }
        }

        // 解析 compute profile 的唯一真源是 **active 部署 manifest**——
        // 安装事务（InstallTransaction::resolve_profile）已按 descriptor 候选顺序
        // 做过兼容性检查并解析为具体 profile；start 不从 descriptor 候选列表
        // 二次推导（那会绕过兼容性检查：GPU-first descriptor 在 CPU 主机上
        // 会得到与实际安装不一致的期望 backend，导致 health 校验误报）。
        let descriptor = entry.adapter.descriptor();

        // ── 冻结 launch snapshot ──
        // deployment identity、resolved profile 与模型身份在 start 时冻结；
        // 配置变化只改变 selected，不改变正在运行的 active。
        // read_active（磁盘 IO）+ resolve_expected_model_identity（manifest 读取）
        // 是阻塞操作——在 spawn_blocking 内执行。
        // **fail-closed**：无 active 部署 / 模型未安装 / 损坏 → 拒绝启动。
        let adapter_for_freeze = Arc::clone(&entry.adapter);
        let eid_for_freeze = engine_id.clone();
        let contract = descriptor.model_contract.clone();
        let uses_managed = adapter_for_freeze.uses_managed_model_storage();
        let selected_model_id = if engine_id.as_str() == super::super::funasr::FUNASR_ENGINE_ID {
            Some(
                crate::app::stt_config::get_stt_config()
                    .local_engine
                    .funasr_model,
            )
        } else {
            None
        };
        let (deployment_install_id, frozen_profile, frozen_model) = tokio::task::spawn_blocking(
            move || -> Result<(String, ResolvedProfile, Option<FrozenModelIdentity>), LocalEngineError> {
                let (pointer, manifest) = DeploymentStore::read_active(&eid_for_freeze)
                    .map_err(|e| from_runtime(ErrorPhase::Start, "读取 active 部署失败", &e))?
                    .ok_or_else(|| {
                        LocalEngineError::with_detail(
                            LocalEngineErrorCode::EnvironmentMissing,
                            ErrorPhase::Start,
                            "环境未安装，请先安装",
                            "无 active deployment.json 指针（fail-closed）".to_string(),
                        )
                    })?;
                let install_id = pointer.install_id;

                // fail-closed：managed 模型未安装/损坏时不允许 start
                let frozen = match resolve_expected_model_identity(
                    &eid_for_freeze,
                    selected_model_id.as_deref(),
                    &contract,
                    uses_managed,
                ) {
                    Ok((model_id, revision, fingerprint)) => Some(FrozenModelIdentity {
                        model_id,
                        revision,
                        fingerprint,
                    }),
                    Err(reason) => {
                        return Err(LocalEngineError::with_detail(
                            LocalEngineErrorCode::ModelNotReady,
                            ErrorPhase::Start,
                            "模型未就绪，请先安装模型",
                            reason,
                        ));
                    }
                };
                Ok((install_id, manifest.resolved_profile, frozen))
            },
        )
        .await
        .map_err(|e| {
            LocalEngineError::with_detail(
                LocalEngineErrorCode::Internal,
                ErrorPhase::Start,
                "冻结启动快照失败",
                format!("spawn_blocking join 错误: {e}"),
            )
        })??;

        // ── 启动尝试循环（bind race 有限重试）──
        //
        // probe 空闲与子进程 bind 之间存在竞争：探测后端口可能被其他进程抢走，
        // 子进程 bind 失败即退出。检测到**明确的** address-in-use（见
        // `is_explicit_address_in_use`）时重新分配端口重试，次数由
        // `ConflictRetryPolicy` 封顶；其他任何失败不重试；
        // **永不终止占用端口的未知进程**。
        let retry_policy = ConflictRetryPolicy::default();
        let preferred_port = config.preferred_port.unwrap_or(8100);
        let allocator = EndpointAllocator::with_defaults(preferred_port);
        let mut attempt: usize = 0;

        loop {
            attempt += 1;

            // 分配 endpoint（每次尝试重新探测——此前尝试可能留下新的占用者）
            let endpoint = allocator.allocate().map_err(|e| {
                LocalEngineError::with_detail(
                    LocalEngineErrorCode::PortConflict,
                    ErrorPhase::Start,
                    "端口分配失败",
                    format!("endpoint allocation failed: {e}"),
                )
            })?;

            // 生成 token + identity
            let token = generate_service_token();
            let instance_id = format!("inst-{}", &token[..8]);
            let identity_input = ServiceIdentityInput {
                engine_id: engine_id.to_string(),
                instance_id: instance_id.clone(),
                token: token.clone(),
                endpoint: endpoint.clone(),
            };

            // 构建 LaunchContext（包含 endpoint、身份参数和 resolved profile）
            let ctx = LaunchContext {
                endpoint: endpoint.clone(),
                engine_id: engine_id.to_string(),
                instance_id: instance_id.clone(),
                token: token.clone(),
                resolved_profile: frozen_profile.clone(),
            };

            // adapter prepare_launch（可能等待 venv python 子进程检查包——阻塞隔离）
            let adapter_for_launch = Arc::clone(&entry.adapter);
            let config_for_launch = config.clone();
            let resolved_launch = tokio::task::spawn_blocking(move || {
                adapter_for_launch.prepare_launch(&ctx, &config_for_launch)
            })
            .await
            .map_err(|e| {
                LocalEngineError::with_detail(
                    LocalEngineErrorCode::Internal,
                    ErrorPhase::Start,
                    "启动参数准备失败",
                    format!("spawn_blocking join 错误: {e}"),
                )
            })??;

            // 构建 LaunchRequest
            let launch = &resolved_launch.launch;
            let mut env = launch.env.clone();
            // 注入 token 和 endpoint 到环境变量（作为后备，adapter 应已通过 CLI 参数传递）
            env.insert("BLINK_ENGINE_TOKEN".to_string(), token.clone());
            env.insert("BLINK_ENGINE_ENDPOINT".to_string(), endpoint.base_url());
            env.insert("BLINK_ENGINE_ID".to_string(), engine_id.to_string());
            env.insert("BLINK_INSTANCE_ID".to_string(), instance_id.clone());

            let req = LaunchRequest {
                executable: launch.executable.clone(),
                args: launch.args.iter().map(|s| s.clone().into()).collect(),
                current_dir: launch.current_dir.clone(),
                env,
                instance_id: instance_id.clone(),
                label: launch.label.clone(),
                shutdown: ShutdownConfig::default(),
            };

            // 创建 ManagedProcess
            let managed = ManagedProcess::with_defaults();

            // 标记 desired=Running, process=Starting
            self.commit_status_internal(engine_id, Some(&operation_id), |status| {
                status.desired = DesiredState::Running;
                status.process = ProcessState::Starting;
                status.service = ServiceHealth::Unknown;
                // 新一轮显式启动已经接管状态，旧实例的错误不应继续挂在界面上。
                // 本轮若失败，rollback 会写入新的 last_error。
                status.last_error = None;
                status.operation = EngineOperation {
                    kind: OperationKind::Idle,
                    operation_id: String::new(),
                    stage: OperationStage::Pending,
                    cancellable: false,
                };
            })
            .await?;

            // 保存 launch snapshot（identity + profile + deployment + 模型身份）+ 进程句柄
            {
                let mut l = entry.launch.lock().await;
                *l = Some(LaunchSnapshot {
                    identity: identity_input.clone(),
                    profile: resolved_launch.profile.clone(),
                    deployment_install_id: deployment_install_id.clone(),
                    model: frozen_model.clone(),
                });
            }
            {
                let mut mp = entry.managed_process.lock().await;
                *mp = Some(managed.clone());
            }
            // 同步 process registry——登记到 service 级 registry
            let pkey = ProcessKey {
                engine_id: engine_id.clone(),
                instance_id: instance_id.clone(),
            };
            {
                let mut reg = self.process_registry.lock().unwrap();
                reg.insert(pkey.clone(), managed.clone());
            }
            // 启动日志 pump task——把 ManagedProcess 的实时日志转发到 EventPort
            // 日志实例隔离——每次 start 创建新 CancellationToken，
            // stop/rollback/restart 时 cancel 旧 pump。
            // pump 每条日志 emit 前实时读取 launch snapshot 校验实例归属。
            let pump_token = CancellationToken::new();
            {
                // 先取消旧 pump（restart/retry 场景）
                let mut old_cancel = entry.log_pump_cancel.lock().await;
                if let Some(old) = old_cancel.take() {
                    tracing::debug!(engine = %engine_id, "start: 取消旧日志 pump");
                    old.cancel();
                }
                *old_cancel = Some(pump_token.clone());
            }
            {
                let event_port = self.event_port.clone();
                let engine_id_clone = engine_id.clone();
                let instance_id_clone = instance_id.clone();
                let subscriber = managed.subscribe_logs();
                let entry_clone = Arc::clone(&entry);
                let pump_token_clone = pump_token.clone();
                tokio::spawn(async move {
                    pump_logs_to_event_port(
                        subscriber,
                        event_port,
                        engine_id_clone,
                        instance_id_clone,
                        entry_clone,
                        pump_token_clone,
                    )
                    .await;
                });
            }

            // spawn 进程
            match managed.start(&req).await {
                Ok(()) => {
                    // 进程 spawn 成功——但 process spawned 不等价于 Healthy
                    let pid = managed.pid().await.unwrap_or(0);
                    tracing::info!(engine = %engine_id, pid, attempt, "进程已 spawn，等待 health 验证");

                    // 更新 process=Running（但 service 仍为 Unknown）
                    self.commit_status_internal(engine_id, Some(&operation_id), |status| {
                        status.process = ProcessState::Running { pid };
                        // service 保持 Unknown——需要 health 验证
                    })
                    .await?;

                    // spawn 成功后立即写 lease
                    // 此时 PID、executable、creation_time_ms 均已从 OS 获取，
                    // token_fingerprint 可从 identity_input 计算。
                    // 如果 Blink 在 health 验证期间崩溃，lease 已存在，
                    // 下次启动的恢复扫描能发现此遗留进程。
                    // health 验证失败时，rollback_started_instance 会清理此 lease。
                    self.write_lease_for_engine(
                        engine_id,
                        &managed,
                        &identity_input,
                        &endpoint,
                        &req,
                        &deployment_install_id,
                    )
                    .await;

                    // health 验证——只有 Model Ready 才返回 Ok
                    // 任何失败（timeout/mismatch/backend/ModelFailed/早退）执行统一 rollback
                    match self
                        .verify_engine_health(engine_id, &entry, &identity_input, &managed)
                        .await
                    {
                        Ok(mapping) => {
                            // health 验证通过 + Model Ready——进入 Healthy
                            self.commit_status_internal(engine_id, Some(&operation_id), |status| {
                                status.service = mapping.service;
                                status.model = mapping.model;
                                if let Some(ref backend_obs) = mapping.backend {
                                    status.backend.backend_verification =
                                        runtime::verify_backend_consistency(
                                            resolved_launch.profile.backend,
                                            Some(backend_obs),
                                        );
                                }
                            })
                            .await?;
                            tracing::info!(
                                engine = %engine_id,
                                instance_id = %instance_id,
                                deployment = %deployment_install_id,
                                "引擎 health 验证通过，Model Ready"
                            );

                            // spawn exit monitor——监听进程意外退出
                            // server crash 后状态必须收敛到 Exited/Unreachable/Failed
                            self.spawn_exit_monitor(
                                engine_id,
                                &managed,
                                &entry,
                                &instance_id,
                                &pkey,
                            );

                            return Ok(());
                        }
                        Err(StartAttemptFailure::BindRace { detail })
                            if retry_policy.should_retry(attempt) =>
                        {
                            // probe-then-bind race——换端口重试（有限次数）
                            let err = LocalEngineError::with_detail(
                                LocalEngineErrorCode::PortConflict,
                                ErrorPhase::Start,
                                "端口被占用，尝试其他端口",
                                detail,
                            );
                            tracing::warn!(
                                engine = %engine_id,
                                attempt,
                                port = endpoint.port(),
                                %err,
                                "bind race：重新分配端口后重试"
                            );
                            self.rollback_started_instance(
                                engine_id,
                                &entry,
                                &pkey,
                                &instance_id,
                                &operation_id,
                                &err,
                            )
                            .await;
                            continue;
                        }
                        Err(StartAttemptFailure::BindRace { detail }) => {
                            // 重试次数耗尽——结构化 PortConflict 终态
                            let err = LocalEngineError::with_detail(
                                LocalEngineErrorCode::PortConflict,
                                ErrorPhase::Start,
                                "候选端口反复被占用，请检查是否有残留引擎进程",
                                detail,
                            );
                            tracing::error!(engine = %engine_id, attempt, %err, "bind race 重试耗尽");
                            self.rollback_started_instance(
                                engine_id,
                                &entry,
                                &pkey,
                                &instance_id,
                                &operation_id,
                                &err,
                            )
                            .await;
                            return Err(err);
                        }
                        Err(StartAttemptFailure::Fatal(err)) => {
                            // 任何非 bind-race 失败——统一 rollback，不重试
                            tracing::warn!(engine = %engine_id, %err, "health 验证失败，执行 rollback");
                            self.rollback_started_instance(
                                engine_id,
                                &entry,
                                &pkey,
                                &instance_id,
                                &operation_id,
                                &err,
                            )
                            .await;
                            return Err(err);
                        }
                    }
                }
                Err(e) => {
                    // spawn 失败——直接 rollback（清理已设置的中间状态），不重试
                    let err = from_process(ErrorPhase::Start, "进程启动失败", &e);
                    tracing::warn!(engine = %engine_id, %err, "进程 spawn 失败，执行 rollback");
                    self.rollback_started_instance(
                        engine_id,
                        &entry,
                        &pkey,
                        &instance_id,
                        &operation_id,
                        &err,
                    )
                    .await;
                    return Err(err);
                }
            }
        }
    }

    // ── stop ────────────────────────────────────────────────────────────────

    /// 停止引擎服务。
    ///
    /// 幂等：如果进程已 Stopped，直接返回 Ok。
    /// 迟到的 health/task/exit 不能覆盖新实例。
    pub async fn stop(&self, engine_id: &EngineId) -> Result<(), LocalEngineError> {
        self.validate_engine_id(engine_id)?;
        let entry = self.get_entry(engine_id).await?;

        // claim 进程级操作（与其他变更操作互斥）
        let operation_id = generate_operation_id();
        let _guard = self.coordinator.try_claim(engine_id, &operation_id)?;

        self.stop_internal_with_status(engine_id, &entry, &operation_id)
            .await
    }

    /// 无 claim 的停止执行体——供已持有操作 claim 的路径
    /// （install/repair 先停引擎）复用，不产生二级 claim。
    ///
    /// **必须传入 claim 持有者的 operation_id**：状态提交的 operation 门
    /// 以协调器 claim 为真源，二级 id 会被判定为迟到操作而拒绝，
    /// 导致运行中实例实际未被停止。
    pub(super) async fn stop_internal(
        &self,
        engine_id: &EngineId,
        entry: &Arc<EngineEntry>,
        operation_id: &str,
    ) {
        let _ = self
            .stop_internal_with_status(engine_id, entry, operation_id)
            .await;
    }

    /// 停止执行体（携带用于状态提交的 operation_id）。
    async fn stop_internal_with_status(
        &self,
        engine_id: &EngineId,
        entry: &Arc<EngineEntry>,
        operation_id: &str,
    ) -> Result<(), LocalEngineError> {
        // 幂等检查
        let managed = {
            let mp = entry.managed_process.lock().await;
            mp.clone()
        };

        match managed {
            Some(mp) => {
                // 标记 desired=Stopped, process=Stopping
                self.commit_status_internal(engine_id, Some(operation_id), |status| {
                    status.desired = DesiredState::Stopped;
                    status.process = ProcessState::Stopping;
                })
                .await?;

                match mp.stop().await {
                    Ok(()) => {
                        self.commit_status_internal(engine_id, Some(operation_id), |status| {
                            status.process = ProcessState::Stopped;
                            status.service = ServiceHealth::Unknown;
                            status.model = ModelHealth::Unknown;
                            status.last_error = None;
                        })
                        .await?;

                        // 清理运行实例状态（launch snapshot + pump + registry + lease）
                        self.clear_running_instance(engine_id, entry, true).await;

                        tracing::info!(engine = %engine_id, "引擎已停止");
                        Ok(())
                    }
                    Err(e) => {
                        let err = from_process(ErrorPhase::Stop, "停止失败", &e);
                        self.commit_status_internal(engine_id, Some(operation_id), |status| {
                            status.process = ProcessState::Exited {
                                reason: format!("stop failed: {e}"),
                            };
                            status.last_error = Some(err.clone());
                        })
                        .await?;
                        Err(err)
                    }
                }
            }
            None => {
                // 已 Stopped，幂等返回
                self.commit_status_internal(engine_id, Some(operation_id), |status| {
                    status.desired = DesiredState::Stopped;
                    if status.process == ProcessState::Starting {
                        status.process = ProcessState::Stopped;
                    }
                    status.last_error = None;
                })
                .await?;
                Ok(())
            }
        }
    }

    /// 清理运行实例状态：取消日志 pump、删除 lease、清 launch snapshot、
    /// 移除 process registry 条目。
    ///
    /// `remove_lease`: stop/exit 路径删除；`stop_if_current` 条件停止成功后
    /// 同样删除（与 stop 语义一致）。
    async fn clear_running_instance(
        &self,
        engine_id: &EngineId,
        entry: &Arc<EngineEntry>,
        remove_lease_flag: bool,
    ) {
        // 取消旧日志 pump——确保 stop 后旧实例日志不再投影
        {
            let mut lc = entry.log_pump_cancel.lock().await;
            if let Some(cancel) = lc.take() {
                tracing::debug!(engine = %engine_id, "clear_running_instance: 取消日志 pump");
                cancel.cancel();
            }
        }

        // 取出 instance_id 用于 lease 删除与 registry 移除
        let saved_instance_id = entry
            .current_identity()
            .await
            .map(|i| i.instance_id.clone());

        if remove_lease_flag {
            if let Some(ref inst_id) = saved_instance_id {
                if let Err(e) = remove_lease(&engine_id.to_string(), inst_id) {
                    tracing::warn!(
                        engine = %engine_id,
                        instance = %inst_id,
                        %e,
                        "清理实例: 删除 lease 失败（继续清理）"
                    );
                }
            }
        }

        // 清理 launch snapshot + 进程句柄
        {
            let mut l = entry.launch.lock().await;
            if let Some(snapshot) = l.take() {
                tracing::debug!(
                    engine = %engine_id,
                    deployment = %snapshot.deployment_install_id,
                    "清理实例: 释放 launch snapshot（start 冻结的部署绑定至此失效）"
                );
            }
        }
        {
            let mut mp_guard = entry.managed_process.lock().await;
            *mp_guard = None;
        }

        // 从同步 registry 移除
        if let Some(instance_id) = saved_instance_id {
            let pkey = ProcessKey {
                engine_id: engine_id.clone(),
                instance_id,
            };
            let mut reg = self.process_registry.lock().unwrap();
            reg.remove(&pkey);
        }
    }

    /// 条件停止：只停止指定 instance token 的实例。
    ///
    /// 如果当前实例的 token 与传入的 token 不匹配（已有新实例接管），
    /// 直接返回 Ok(())，不停止新实例。
    ///
    /// 用于 OcrCoordinator 的 lease 管理：旧 timer 或旧 startup task
    /// 不得停止/覆盖新实例。
    pub async fn stop_if_current(
        &self,
        engine_id: &EngineId,
        instance_token: &crate::infra::local_engine::state::InstanceToken,
    ) -> Result<(), LocalEngineError> {
        self.validate_engine_id(engine_id)?;
        let entry = self.get_entry(engine_id).await?;

        // claim 进程级操作（与其他变更操作互斥）
        let operation_id = generate_operation_id();
        let _guard = self.coordinator.try_claim(engine_id, &operation_id)?;

        let managed = {
            let mp = entry.managed_process.lock().await;
            mp.clone()
        };

        match managed {
            Some(mp) => {
                // 条件检查：token 不匹配则跳过
                if !mp.is_current_token(instance_token).await {
                    tracing::info!(
                        engine = %engine_id,
                        "stop_if_current: token 不匹配，跳过停止（新实例已接管）"
                    );
                    return Ok(());
                }

                // 标记 desired=Stopped, process=Stopping
                self.commit_status_internal(engine_id, Some(&operation_id), |status| {
                    status.desired = DesiredState::Stopped;
                    status.process = ProcessState::Stopping;
                })
                .await?;

                match mp.stop_if_current(instance_token).await {
                    Ok(()) => {
                        self.commit_status_internal(engine_id, Some(&operation_id), |status| {
                            status.process = ProcessState::Stopped;
                            status.service = ServiceHealth::Unknown;
                            status.model = ModelHealth::Unknown;
                            status.last_error = None;
                        })
                        .await?;

                        // 清理运行实例状态（含 lease——条件停止成功即实例终结）
                        self.clear_running_instance(engine_id, &entry, true).await;

                        tracing::info!(engine = %engine_id, "引擎已条件停止（token 匹配）");
                        Ok(())
                    }
                    Err(e) => {
                        let err = from_process(ErrorPhase::Stop, "条件停止失败", &e);
                        self.commit_status_internal(engine_id, Some(&operation_id), |status| {
                            status.process = ProcessState::Exited {
                                reason: format!("stop_if_current failed: {e}"),
                            };
                            status.last_error = Some(err.clone());
                        })
                        .await?;
                        Err(err)
                    }
                }
            }
            None => {
                // 已 Stopped，幂等返回
                self.commit_status_internal(engine_id, Some(&operation_id), |status| {
                    status.desired = DesiredState::Stopped;
                    if status.process == ProcessState::Starting {
                        status.process = ProcessState::Stopped;
                    }
                    status.last_error = None;
                })
                .await?;
                Ok(())
            }
        }
    }

    // ── shutdown_all ────────────────────────────────────────────────────────

    /// 异步遍历所有受管实例并回收。
    ///
    /// 单个失败不能阻止其他实例回收；最终返回汇总错误并记录结构化日志。
    pub async fn shutdown_all(&self) -> Result<(), Vec<LocalEngineError>> {
        let entries = self.entries.read().await;
        let mut errors = Vec::new();

        for (engine_id, entry) in entries.iter() {
            let mp = entry.managed_process.lock().await;
            if let Some(managed) = mp.as_ref() {
                tracing::info!(engine = %engine_id, "shutdown_all: 回收引擎实例");
                if let Err(e) = managed.stop().await {
                    let err = from_process(ErrorPhase::Stop, "shutdown_all 回收失败", &e);
                    tracing::error!(engine = %engine_id, %err, "shutdown_all: 回收失败");
                    errors.push(err);
                } else {
                    // 更新状态
                    drop(mp);
                    let _ = self
                        .commit_status_internal(engine_id, None, |status| {
                            status.desired = DesiredState::Stopped;
                            status.process = ProcessState::Stopped;
                            status.service = ServiceHealth::Unknown;
                            status.model = ModelHealth::Unknown;
                        })
                        .await;
                }
            }
        }

        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }

    /// 同步阻塞版本的 shutdown_all（应用退出用）。
    ///
    /// 遍历 `process_registry`（同步 Mutex，不依赖 async lock），
    /// 对每个 ManagedProcess 调用 `shutdown_blocking()`。
    /// 单个失败不阻止其他回收。
    ///
    /// **0.22.3 Task E**: 不依赖 `entries` 的 async lock——
    /// `process_registry` 是独立的同步 Mutex，shutdown 路径可靠。
    #[allow(dead_code)]
    pub fn shutdown_all_blocking(&self) {
        // 同步遍历 process_registry——不依赖 async entries lock
        let registry = self.process_registry.lock().unwrap();
        for (key, managed) in registry.iter() {
            tracing::info!(
                engine = %key.engine_id,
                instance = %key.instance_id,
                "shutdown_all_blocking: 回收"
            );
            managed.shutdown_blocking();
        }
    }

    // ── lease 写入辅助（0.22.6.1） ─────────────────────────────────────────

    /// 为引擎实例写入持久化 lease。
    ///
    /// 在 spawn 成功后立即调用——从 `ManagedProcess` 获取进程身份
    /// （PID、可执行路径、创建时间），从 `ServiceIdentityInput` 获取
    /// token fingerprint；`deployment_id` 使用 start 时冻结的
    /// launch snapshot 中的部署 install_id。
    ///
    /// 写入失败只打 warn 日志，不影响 start 成功返回（lease 是辅助证据，
    /// 不是运行时强依赖）。
    async fn write_lease_for_engine(
        &self,
        engine_id: &EngineId,
        managed: &Arc<ManagedProcess>,
        identity_input: &ServiceIdentityInput,
        endpoint: &crate::infra::local_engine::port::Endpoint,
        #[allow(unused_variables)] req: &LaunchRequest,
        deployment_id: &str,
    ) {
        // 从 ManagedProcess 获取进程身份
        let snapshot = managed.snapshot().await;
        let identity = match &snapshot.identity {
            Some(id) => id,
            None => {
                tracing::warn!(
                    engine = %engine_id,
                    "write_lease: ManagedProcess 无 identity，跳过 lease 写入"
                );
                return;
            }
        };

        let lease = build_process_lease(
            engine_id,
            identity,
            identity_input,
            endpoint,
            deployment_id.to_string(),
        );

        // lease 文件写入是同步 IO——挪到 blocking 线程，不占 async worker。
        if let Err(e) = tokio::task::spawn_blocking(move || write_lease(&lease)).await {
            tracing::warn!(
                engine = %engine_id,
                instance = %identity.instance_id,
                error = %e,
                "write_lease: 写入 lease 失败（不影响运行时）"
            );
        }
    }

    // ── exit monitor（0.22.6.3）─────────────────────────────────────────────

    /// Spawn 进程退出监听 task。
    ///
    /// 在 health 验证通过后调用，监听 `ManagedProcess` 的状态变更。
    /// 当收到 `ProcessStatus::Exited` 时执行 `handle_process_exit`。
    ///
    /// **设计约束**：
    /// - task 内不持有 `&self`（生命周期不够），只持有 Arc 字段
    /// - task 内直接操作 `entry.status` 的 RwLock（等价于 commit_status_internal
    ///   但不检查 operation_id——exit 事件是异步到达的，不经过 op_gate）
    /// - 通过 `entry.managed_process` 的 instance_id 验证：如果已 restart，
    ///   旧 monitor 的 exit 事件不会覆盖新实例状态
    fn spawn_exit_monitor(
        &self,
        engine_id: &EngineId,
        managed: &Arc<ManagedProcess>,
        entry: &Arc<EngineEntry>,
        instance_id: &str,
        pkey: &ProcessKey,
    ) {
        let engine_id = engine_id.clone();
        let managed = Arc::clone(managed);
        let entry = Arc::clone(entry);
        let event_port = Arc::clone(&self.event_port);
        let epoch = self.epoch.clone();
        let instance_id = instance_id.to_string();
        let pkey = pkey.clone();
        // 0.22.6.4: 传入 process_registry 的 Arc 引用，使 exit monitor
        // 能在验证身份后移除对应 ProcessKey 条目，避免 registry 泄漏。
        // 不形成强引用环：registry 是 EngineManager 拥有的 Mutex<HashMap>，
        // 这里克隆的是 Arc 到同一 Mutex 的引用，不持有 EngineManager 自身。
        let process_registry = Arc::clone(&self.process_registry);

        tokio::spawn(async move {
            let mut rx = managed.subscribe_status();

            loop {
                if rx.changed().await.is_err() {
                    // sender dropped——进程已清理，退出 monitor
                    break;
                }

                let status = rx.borrow().clone();
                if !status.is_exited() {
                    continue;
                }

                tracing::warn!(
                    engine = %engine_id,
                    instance = %instance_id,
                    status = ?status,
                    "exit monitor: 收到进程退出事件"
                );

                // 0.22.6.3: 验证此 managed 仍是 entry 的当前实例
                // 如果已 restart（新 start 替换了 managed_process），旧 exit 事件不生效
                let is_current = {
                    let mp = entry.managed_process.lock().await;
                    if let Some(ref current) = *mp {
                        current
                            .is_current_token(&managed.current_token().await)
                            .await
                    } else {
                        false
                    }
                };

                if !is_current {
                    tracing::info!(
                        engine = %engine_id,
                        instance = %instance_id,
                        "exit monitor: managed 已不是当前实例（可能 restart），忽略旧 exit 事件"
                    );
                    break;
                }

                // 收到 exit 事件且验证为当前实例——执行状态收敛
                let exit_reason = match &status {
                    ProcessStatus::Exited { reason } => format!("{reason:?}"),
                    _ => unreachable!(),
                };

                // 取消旧日志 pump——确保退出后旧实例日志不再投影
                {
                    let mut lc = entry.log_pump_cancel.lock().await;
                    if let Some(cancel) = lc.take() {
                        tracing::debug!(engine = %engine_id, "exit monitor: 取消日志 pump");
                        cancel.cancel();
                    }
                }

                // 取出 instance_id 用于 lease 删除
                let saved_instance_id = entry
                    .current_identity()
                    .await
                    .map(|i| i.instance_id.clone());

                // 删除 lease
                if let Some(ref inst_id) = saved_instance_id {
                    if let Err(e) = remove_lease(&engine_id.to_string(), inst_id) {
                        tracing::warn!(
                            engine = %engine_id,
                            instance = %inst_id,
                            %e,
                            "exit monitor: 删除 lease 失败（继续清理）"
                        );
                    }
                }

                // 清理 launch snapshot + 进程句柄
                {
                    let mut l = entry.launch.lock().await;
                    *l = None;
                }
                {
                    let mut mp = entry.managed_process.lock().await;
                    *mp = None;
                }

                // 0.22.6.4: 从 process_registry 移除——exit monitor 持有
                // process_registry 的 Arc 引用，在验证身份后安全移除。
                // is_current 检查已确保不会误删新实例的条目。
                {
                    let mut reg = process_registry.lock().unwrap();
                    reg.remove(&pkey);
                    tracing::debug!(
                        engine = %engine_id,
                        instance = %instance_id,
                        "exit monitor: 已从 process_registry 移除"
                    );
                }

                // 置错误终态：process=Exited, service=Unreachable, model=Unknown
                {
                    let mut status_guard = entry.status.write().await;

                    // epoch 验证——新 epoch 重置状态
                    if status_guard.service_epoch != epoch {
                        *status_guard = EngineStatus {
                            service_epoch: epoch.clone(),
                            ..Default::default()
                        };
                    }

                    let new_revision = status_guard.revision + 1;
                    let exit_err = LocalEngineError::with_detail(
                        LocalEngineErrorCode::NotRunning,
                        ErrorPhase::Stop,
                        "进程意外退出",
                        exit_reason.clone(),
                    );

                    status_guard.desired = DesiredState::Stopped;
                    status_guard.process = ProcessState::Exited {
                        reason: exit_reason,
                    };
                    status_guard.service = ServiceHealth::Unreachable;
                    status_guard.model = ModelHealth::Unknown;
                    status_guard.last_error = Some(exit_err);
                    status_guard.revision = new_revision;

                    // 广播状态变更
                    let snapshot = EngineStatusSnapshot {
                        engine_id: engine_id.clone(),
                        service_epoch: epoch,
                        revision: new_revision,
                        status: status_guard.clone(),
                    };
                    event_port.emit_status(&snapshot);
                }

                tracing::warn!(
                    engine = %engine_id,
                    instance = %instance_id,
                    "exit monitor: 状态已收敛到 Exited/Unreachable，current identity 已清理"
                );

                // exit 事件只处理一次
                break;
            }
        });
    }

    // ── rollback ────────────────────────────────────────────────────────────

    /// 统一回滚已启动实例——start 失败时调用。
    ///
    /// 清理项：
    /// 1. 停止 ManagedProcess（如果存在）
    /// 2. 清理 launch snapshot / 日志 pump / lease / process registry
    /// 3. 置错误终态（process=Exited, service=Unreachable, last_error=err）
    ///
    /// **不回滚部署**：部署完整性由安装事务的切换后验证保证；
    /// 进程启动失败（端口冲突/超时等）是进程生命周期问题，
    /// 不构成部署回滚条件（旧 slot 已在事务成功时删除）。
    async fn rollback_started_instance(
        &self,
        engine_id: &EngineId,
        entry: &Arc<EngineEntry>,
        _pkey: &ProcessKey,
        instance_id: &str,
        operation_id: &str,
        error: &LocalEngineError,
    ) {
        tracing::warn!(
            engine = %engine_id,
            instance = instance_id,
            error = %error,
            "rollback_started_instance: 清理中间状态"
        );

        // 停止 ManagedProcess（如果仍在运行）
        {
            let mp = entry.managed_process.lock().await;
            if let Some(managed) = mp.as_ref() {
                if let Err(e) = managed.stop().await {
                    tracing::warn!(
                        engine = %engine_id,
                        error = %e,
                        "rollback: ManagedProcess.stop 失败（继续清理）"
                    );
                }
            }
        }

        // 清理运行实例状态（pump/lease/launch snapshot/registry）
        self.clear_running_instance(engine_id, entry, true).await;

        // 置错误终态。
        // 必须携带 start claim 的 operation_id 提交——start 的 claim 仍由
        // _guard 持有，不带 id（或带错 id）的提交会被 operation 门拒绝，
        // 导致 Exited/Unreachable 终态不落地、快照停留在 Running。
        let _ = self
            .commit_status_internal(engine_id, Some(operation_id), |status| {
                status.desired = DesiredState::Stopped;
                status.process = ProcessState::Exited {
                    reason: format!("rollback: {:?}", error.code),
                };
                status.service = ServiceHealth::Unreachable;
                status.model = ModelHealth::Unknown;
                status.last_error = Some(error.clone());
            })
            .await;
    }
}

/// 用服务身份与 OS 进程证据构造持久化 lease。
///
/// `ManagedProcess` 的 `ProcessIdentity::instance_id` 是 infra 状态机用于隔离
/// generation 的内部 token；health、回滚与恢复协议使用的是
/// `ServiceIdentityInput::instance_id`。lease 必须保存后者，否则 start 回滚时
/// 无法通过 instance 校验删除本次写入的 lease。
pub(super) fn build_process_lease(
    engine_id: &EngineId,
    process_identity: &ProcessIdentity,
    service_identity: &ServiceIdentityInput,
    endpoint: &crate::infra::local_engine::port::Endpoint,
    generation_id: String,
) -> ProcessLease {
    ProcessLease::new(
        engine_id.to_string(),
        service_identity.instance_id.clone(),
        process_identity.pid,
        process_identity.start_time_ms,
        process_identity.executable.to_string_lossy().to_string(),
        endpoint.base_url(),
        service_identity.token_fingerprint(),
        generation_id,
    )
}

// ── 动态模型身份解析（0.22.6 B2）─────────────────────────────────────────

/// 从 model_storage manifest 动态解析当前安装的模型身份。
///
/// 返回 `(model_id, revision, fingerprint)` 三元组（如果模型已安装且有效）。
///
/// **asset_key 真源**：managed 模式下用 `selected_model_id`（配置选中的模型，
/// 如 funasr 的 `funasr_model`）查找 manifest；`fallback_contract.model_id`
/// 只是 descriptor 默认占位——用户可能安装/选择了其他模型（如装了
/// paraformer-zh 而 descriptor 默认 SenseVoiceSmall），按硬编码查找会
/// 误报"模型未安装"。
///
/// **0.22.6 B2 fail-closed 铁则**：模型未安装、损坏或恢复失败时返回 `Err`，
/// 不再回退到 descriptor 静态值。调用方必须将此视为启动/健康检查失败。
///
/// 这确保 health Ready 校验只与实际安装的 manifest 比对，
/// 而非与 descriptor 中编译期常量比对——防止
/// "下载了模型 A 但 health 期望模型 B" 的静默通过。
pub(super) fn resolve_expected_model_identity(
    engine_id: &EngineId,
    selected_model_id: Option<&str>,
    fallback_contract: &ModelContract,
    uses_managed_model_storage: bool,
) -> Result<(String, String, Option<String>), String> {
    if !uses_managed_model_storage {
        return Ok((
            fallback_contract.model_id.clone(),
            fallback_contract.revision.clone(),
            None,
        ));
    }

    // 使用配置选中的 model_id 作为 asset_key 的来源
    let model_id_for_key = selected_model_id
        .filter(|m| !m.is_empty())
        .unwrap_or(&fallback_contract.model_id);
    let asset_key = mstore::encode_asset_key(model_id_for_key);
    match mstore::restore_model_state(engine_id, &asset_key) {
        Ok(mstore::RestoredModelState::Installed { manifest, .. }) => Ok((
            manifest.model_id,
            manifest.revision,
            Some(manifest.content_fingerprint),
        )),
        Ok(mstore::RestoredModelState::Corrupted { reason, .. }) => {
            tracing::warn!(
                engine_id = %engine_id,
                model_id = %model_id_for_key,
                reason = %reason,
                "模型状态 Corrupted——fail-closed，不回退到 descriptor 静态身份"
            );
            Err(format!("模型状态 Corrupted: {reason}"))
        }
        Ok(mstore::RestoredModelState::NotInstalled) => {
            tracing::debug!(
                engine_id = %engine_id,
                model_id = %model_id_for_key,
                "模型未安装——fail-closed，不回退到 descriptor 静态身份"
            );
            Err(format!("模型未安装: {model_id_for_key}"))
        }
        Err(e) => {
            tracing::warn!(
                engine_id = %engine_id,
                model_id = %model_id_for_key,
                error = %e,
                "模型状态恢复失败——fail-closed，不回退到 descriptor 静态身份"
            );
            Err(format!("模型状态恢复失败: {e}"))
        }
    }
}
