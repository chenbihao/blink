use super::*;

use super::super::logs::pump_logs_to_event_port;

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

        self.start_internal(engine_id, &entry, config, &operation_id)
            .await
    }

    /// start 执行体（供已持有操作 claim 的路径复用：切换事务 stop→start→回滚）。
    ///
    /// **调用方必须已持有 engine 的操作 claim 并传入其 operation_id**——
    /// 状态提交的 operation 门以协调器 claim 为真源。guard 生命周期由调用方
    /// 承担（事务期间不允许其他变更操作插入）。
    pub(in super::super) async fn start_internal(
        &self,
        engine_id: &EngineId,
        entry: &Arc<EngineEntry>,
        config: AdapterConfig,
        operation_id: &str,
    ) -> Result<(), LocalEngineError> {
        // 幂等检查：desired=Running 且进程活跃 → 直接返回
        {
            let status = entry.status.read().await;
            if status.desired == DesiredState::Running && status.is_process_active() {
                tracing::debug!(engine = %engine_id, "start 幂等：已 Running/Starting");
                return Ok(());
            }
        }

        // 解析 compute profile 的唯一真源是 **resolved implementation 空间的
        // active 部署 manifest**——安装事务（InstallTransaction::resolve_profile）
        // 已按 descriptor 候选顺序做过兼容性检查并解析为具体 profile；start 不从
        // descriptor 候选列表二次推导（那会绕过兼容性检查：GPU-first descriptor
        // 在 CPU 主机上会得到与实际安装不一致的期望 backend，导致 health 校验误报）。
        let descriptor = entry.adapter.descriptor();

        // ── 预解析 implementation（Handoff 08：从"配置 selected（无则退化
        // descriptor 模型契约）"的模型 id 解析，决定后续冻结策略与环境门；
        // 绑定表 fail-closed——未知模型拒绝启动）──
        let selected_model_id = super::selected_model_id_from_config(engine_id, &config);
        let impl_seed_model = super::seed_model_id_for_implementation(
            engine_id,
            &config,
            &descriptor.model_contract.model_id,
        );
        let resolved_implementation =
            self.resolve_implementation_for_model(engine_id, impl_seed_model.as_deref())?;
        let is_paraformer_onnx =
            resolved_implementation == Some(ImplementationId::ParaformerOnnxWorker);

        // 环境检查（engine 级环境 = GGUF 主 implementation 的部署真源）。
        // ParaformerOnline 走 per-implementation deployment——其部署就绪由
        // 下方冻结段 fail-closed 校验，不要求 GGUF 环境已安装。
        if !is_paraformer_onnx {
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

        // ── 冻结 launch snapshot（0.22.9：模型 → implementation → deployment）──
        //
        // 冻结顺序 fail-closed：
        // 1. 模型身份（GGUF：selected 的 model_storage manifest 回读；
        //    ParaformerOnline 等 per-implementation 实现：对应部署空间的
        //    active manifest 回读）
        // 2. implementation（编译期绑定表按冻结模型解析，未知模型不换模）
        // 3. deployment identity（**resolved implementation 的部署空间**内
        //    读 active 指针——GGUF 映射到 engine 级兼容真源，新实现读自己的
        //    implementation 空间；空间内无部署 → 拒绝启动）
        //
        // 配置变化只改变 selected，不改变正在运行的 active。
        // 磁盘 IO（manifest 读取、指针读取）在 spawn_blocking 内执行。
        let eid_for_freeze = engine_id.clone();
        let contract = descriptor.model_contract.clone();
        let uses_managed = entry.adapter.uses_managed_model_storage();
        let impl_for_freeze = resolved_implementation;
        let eid_for_space = engine_id.clone();
        // "模型未安装"错误文案带显示名——用户配置了（或默认选中）某个模型
        // 但未下载时，报错必须说清是哪个模型（0.22.9 实测反馈：只说
        // "未下载模型"无法对应到模型列表里的具体条目）。
        let identity_seed_model = selected_model_id
            .clone()
            .filter(|m| !m.is_empty())
            .unwrap_or_else(|| contract.model_id.clone());
        let model_display_name = self
            .model_registry
            .find(engine_id, &identity_seed_model)
            .map(|d| d.display_name.clone());
        let frozen_model = tokio::task::spawn_blocking(
            move || -> Result<FrozenModelIdentity, LocalEngineError> {
                if impl_for_freeze == Some(ImplementationId::ParaformerOnnxWorker) {
                    // per-implementation deployment（ParaformerOnline 等）：
                    // 模型身份从该 implementation 部署空间的 active manifest 冻结。
                    // 闭合映射决定空间归属（GGUF/OCR → engine 级；新实现 → impl 级）。
                    let space = deployment_space_for(&eid_for_space, impl_for_freeze);
                    let (pointer, manifest) = DeploymentStore::read_active(&space)
                        .map_err(|e| from_runtime(ErrorPhase::Start, "读取 active 部署失败", &e))?
                        .ok_or_else(|| {
                            let space_label = space
                                .implementation()
                                .map(|i| format!("implementation '{i}' 的部署空间"))
                                .unwrap_or_else(|| "engine 级部署空间".to_string());
                            LocalEngineError::with_detail(
                                LocalEngineErrorCode::EnvironmentMissing,
                                ErrorPhase::Start,
                                "环境未安装，请先安装",
                                format!("{space_label}内无 active deployment（fail-closed）"),
                            )
                        })?;
                    // 指针的模型契约即冻结身份；fingerprint 取 ONNX 扩展的
                    // DLL SHA-256（64-hex，安装事务已校验；非 ONNX 扩展 = 无）
                    let fingerprint = match &manifest.extension {
                        crate::infra::local_engine::runtime::ManifestExtension::OnnxRuntime(
                            ext,
                        ) => Some(ext.dll_sha256.clone()),
                        _ => None,
                    };
                    // active slot 目录（launch 构造直接使用，避免二次读指针漂移）
                    let slot_dir = crate::infra::local_engine::deployment::DeploymentSlot::parse(
                        &pointer.slot,
                    )
                    .map_err(|e| from_runtime(ErrorPhase::Start, "解析部署 slot 失败", &e))?
                    .dir_in(&space);
                    let _ = slot_dir; // slot 目录由 adapter 侧按部署空间解析
                    return Ok(FrozenModelIdentity {
                        model_id: manifest.model_contract.model_id,
                        revision: manifest.model_contract.revision,
                        fingerprint,
                    });
                }
                // fail-closed：managed 模型未安装/损坏时不允许 start
                match resolve_expected_model_identity(
                    &eid_for_freeze,
                    selected_model_id.as_deref(),
                    &contract,
                    uses_managed,
                ) {
                    Ok((model_id, revision, fingerprint)) => Ok(FrozenModelIdentity {
                        model_id,
                        revision,
                        fingerprint,
                    }),
                    Err(reason) => {
                        let action_hint = if reason.starts_with("模型未安装") {
                            match &model_display_name {
                                Some(display) => format!(
                                    "模型 '{display}' 尚未下载，请先在「引擎」页的模型列表中下载"
                                ),
                                None => "模型未下载，请先在「引擎」页的模型列表中下载".to_string(),
                            }
                        } else {
                            "模型未就绪，请先安装模型".to_string()
                        };
                        Err(LocalEngineError::with_detail(
                            LocalEngineErrorCode::ModelNotReady,
                            ErrorPhase::Start,
                            action_hint,
                            reason,
                        ))
                    }
                }
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

        // ── 冻结 implementation（0.22.9，fail-closed，纯内存）──
        // 按冻结模型身份复核（GGUF manifest 回读的 model_id 与 selected 一致；
        // 不一致以 manifest 为准并要求仍在绑定表中——未知模型不换模）。
        let frozen_implementation =
            self.resolve_implementation_for_model(engine_id, Some(frozen_model.model_id.as_str()))?;
        if frozen_implementation != resolved_implementation {
            return Err(LocalEngineError::with_detail(
                LocalEngineErrorCode::InvalidConfig,
                ErrorPhase::Start,
                "implementation 解析不一致",
                format!(
                    "selected 预解析={resolved_implementation:?}，manifest 复核={frozen_implementation:?}（模型/绑定漂移，fail-closed）"
                ),
            ));
        }

        // ── 从 resolved implementation 的部署空间读取 active 部署（fail-closed）──
        let deployment_space = deployment_space_for(engine_id, frozen_implementation);
        let (deployment_install_id, frozen_profile) = tokio::task::spawn_blocking(
            move || -> Result<(String, ResolvedProfile), LocalEngineError> {
                let (pointer, manifest) = DeploymentStore::read_active(&deployment_space)
                    .map_err(|e| from_runtime(ErrorPhase::Start, "读取 active 部署失败", &e))?
                    .ok_or_else(|| {
                        let space_label = deployment_space
                            .implementation()
                            .map(|i| format!("implementation '{i}' 的部署空间"))
                            .unwrap_or_else(|| "engine 级部署空间".to_string());
                        LocalEngineError::with_detail(
                            LocalEngineErrorCode::EnvironmentMissing,
                            ErrorPhase::Start,
                            "环境未安装，请先安装",
                            format!("{space_label}内无 active deployment.json 指针（fail-closed）"),
                        )
                    })?;
                Ok((pointer.install_id, manifest.resolved_profile))
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
        //
        // StdioWorker 引擎（0.22.7）不监听端口：无 bind race，单次尝试，
        // endpoint 使用占位值。
        let stdio_worker = descriptor.service_transport
            == crate::domain::local_engine::ServiceTransport::StdioWorker;
        let retry_policy = ConflictRetryPolicy::default();
        let preferred_port = config.preferred_port.unwrap_or(8100);
        let allocator = EndpointAllocator::with_defaults(preferred_port);
        let mut attempt: usize = 0;

        loop {
            attempt += 1;

            // 分配 endpoint（每次尝试重新探测——此前尝试可能留下新的占用者）
            let endpoint = if stdio_worker {
                crate::infra::local_engine::port::Endpoint::stdio_placeholder()
            } else {
                allocator.allocate().map_err(|e| {
                    LocalEngineError::with_detail(
                        LocalEngineErrorCode::PortConflict,
                        ErrorPhase::Start,
                        "端口分配失败",
                        format!("endpoint allocation failed: {e}"),
                    )
                })?
            };

            // 生成 token + identity
            let token = generate_service_token();
            let instance_id = format!("inst-{}", &token[..8]);
            let identity_input = ServiceIdentityInput {
                engine_id: engine_id.to_string(),
                instance_id: instance_id.clone(),
                token: token.clone(),
                endpoint,
            };

            // 构建 LaunchContext（endpoint、身份参数、resolved profile、
            // 冻结的 implementation——adapter 据此分派启动构造，Handoff 08）
            let ctx = LaunchContext {
                endpoint,
                engine_id: engine_id.to_string(),
                instance_id: instance_id.clone(),
                token: token.clone(),
                resolved_profile: frozen_profile.clone(),
                implementation: frozen_implementation,
            };

            // adapter prepare_launch（可能等待 venv python 子进程检查包——阻塞隔离）。
            // 冻结的 implementation 经 ctx 注入，FunASR adapter 据此分派实现内
            // 启动构造（GGUF → NDJSON worker；ParaformerOnline → ONNX worker）。
            let adapter_for_launch = Arc::clone(&entry.adapter);
            let config_for_launch = config.clone();
            let ctx_for_launch = ctx.clone();
            let resolved_launch = tokio::task::spawn_blocking(move || {
                adapter_for_launch.prepare_launch(&ctx_for_launch, &config_for_launch)
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
                stdio: if stdio_worker {
                    crate::infra::local_engine::process::StdioConfig::worker_protocol()
                } else {
                    crate::infra::local_engine::process::StdioConfig::default()
                },
            };

            // 创建 ManagedProcess
            let managed = ManagedProcess::with_defaults();

            // 标记 desired=Running, process=Starting
            self.commit_status_internal(engine_id, Some(operation_id), |status| {
                status.desired = DesiredState::Running;
                status.process = ProcessState::Starting;
                status.service = ServiceHealth::Unknown;
                // 冻结 active implementation（selected 变化不影响）
                status.active_implementation = frozen_implementation;
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

            // 保存 launch snapshot（identity + profile + deployment + 模型身份 +
            // implementation）+ 进程句柄
            {
                let mut l = entry.launch.lock().await;
                *l = Some(LaunchSnapshot {
                    identity: identity_input.clone(),
                    profile: resolved_launch.profile.clone(),
                    deployment_install_id: deployment_install_id.clone(),
                    model: Some(frozen_model.clone()),
                    implementation: frozen_implementation,
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
                let entry_clone = Arc::clone(entry);
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
                    self.commit_status_internal(engine_id, Some(operation_id), |status| {
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
                    // StdioWorker 引擎走 NDJSON ready 握手（0.22.7），HTTP 引擎走轮询；
                    // ParaformerOnline 走二进制协议 v2 握手（Handoff 08）
                    let verify_future = if is_paraformer_onnx {
                        self.verify_paraformer_worker_health(
                            engine_id,
                            entry,
                            &identity_input,
                            &managed,
                            frozen_model.clone(),
                        )
                        .await
                    } else if stdio_worker {
                        self.verify_stdio_worker_health(engine_id, entry, &identity_input, &managed)
                            .await
                    } else {
                        self.verify_engine_health(engine_id, entry, &identity_input, &managed)
                            .await
                    };
                    match verify_future {
                        Ok(mapping) => {
                            // health 验证通过 + Model Ready——进入 Healthy
                            self.commit_status_internal(engine_id, Some(operation_id), |status| {
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
                                entry,
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
                                entry,
                                &pkey,
                                &instance_id,
                                operation_id,
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
                                entry,
                                &pkey,
                                &instance_id,
                                operation_id,
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
                                entry,
                                &pkey,
                                &instance_id,
                                operation_id,
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
                        entry,
                        &pkey,
                        &instance_id,
                        operation_id,
                        &err,
                    )
                    .await;
                    return Err(err);
                }
            }
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
                    ProcessStatus::Exited { reason } => reason.clone(),
                    _ => unreachable!(),
                };

                // 0.22.7：区分 deliberate stop（主动停止）与 unexpected exit（真实崩溃）。
                // 用户 stop、切模重启、OCR idle TTL、应用退出均属于 deliberate stop，
                // 不能被上层投影成"进程意外退出"。Stopped { code: Some(1) } 可以保留
                // 为诊断数据，但不进入错误态。真正无 stop intent 的崩溃（NonZeroExit/
                // WaitError）仍必须报告意外退出。
                let is_deliberate = exit_reason.is_deliberate_stop();

                // 取消旧日志 pump——确保退出后旧实例日志不再投影
                {
                    let mut lc = entry.log_pump_cancel.lock().await;
                    if let Some(cancel) = lc.take() {
                        tracing::debug!(engine = %engine_id, "exit monitor: 取消日志 pump");
                        cancel.cancel();
                    }
                }

                // 0.22.7：销毁 stdio worker 客户端——崩溃后旧连接的写入
                // 因管道关闭立即失败（迟到结果不污染新实例），并清理音频临时目录。
                {
                    let client = entry.worker_client.lock().await.take();
                    if client.is_some() {
                        tracing::debug!(engine = %engine_id, "exit monitor: 销毁 worker 客户端");
                    }
                }
                // 0.22.9 Handoff 08：销毁 ParaformerOnline 适配器（崩溃后旧
                // streaming port 的操作立即失败，迟到结果不污染新实例）。
                {
                    let port = entry.streaming_port.lock().await.take();
                    if port.is_some() {
                        tracing::debug!(engine = %engine_id, "exit monitor: 销毁 paraformer 适配器");
                    }
                }
                super::super::super::funasr::worker::clean_audio_tmp_dir(&engine_id);

                // 取出 instance_id 用于 lease 删除
                let saved_instance_id = entry
                    .current_identity()
                    .await
                    .map(|i| i.instance_id.clone());

                // 删除 lease
                if let Some(ref inst_id) = saved_instance_id
                    && let Err(e) = remove_lease(&engine_id.to_string(), inst_id)
                {
                    tracing::warn!(
                        engine = %engine_id,
                        instance = %inst_id,
                        %e,
                        "exit monitor: 删除 lease 失败（继续清理）"
                    );
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

                // 置终态：process=Exited
                // 0.22.7：deliberate stop（主动停止）不进入错误态——
                // 不设置 last_error，不报告"进程意外退出"。
                // 真实崩溃（NonZeroExit/WaitError）仍报告意外退出。
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

                    status_guard.desired = DesiredState::Stopped;
                    status_guard.process = ProcessState::Exited {
                        reason: format!("{exit_reason:?}"),
                    };
                    status_guard.service = ServiceHealth::Unknown;
                    status_guard.model = ModelHealth::Unknown;
                    // 实例终结——active implementation 随 launch snapshot 清除
                    status_guard.active_implementation = None;

                    if is_deliberate {
                        // 主动停止：清除旧错误，不设置新错误
                        status_guard.last_error = None;
                        tracing::info!(
                            engine = %engine_id,
                            instance = %instance_id,
                            reason = ?exit_reason,
                            "exit monitor: deliberate stop，不进入错误态"
                        );
                    } else {
                        // 真实崩溃：报告意外退出
                        let exit_err = LocalEngineError::with_detail(
                            LocalEngineErrorCode::NotRunning,
                            ErrorPhase::Stop,
                            "进程意外退出",
                            format!("{exit_reason:?}"),
                        );
                        status_guard.service = ServiceHealth::Unreachable;
                        status_guard.last_error = Some(exit_err);
                        tracing::warn!(
                            engine = %engine_id,
                            instance = %instance_id,
                            reason = ?exit_reason,
                            "exit monitor: unexpected exit，进入错误态"
                        );
                    }

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

                tracing::info!(
                    engine = %engine_id,
                    instance = %instance_id,
                    deliberate = is_deliberate,
                    "exit monitor: 状态已收敛，current identity 已清理"
                );

                // exit 事件只处理一次
                break;
            }
        });
    }
}
