use super::*;

impl EngineManager {
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
    pub(in super::super) async fn stop_internal(
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

                // 0.22.7：先走优雅停止（StdioWorker: shutdown + stdin EOF；
                // HTTP: POST /shutdown），短暂等待进程自行退出；
                // 超时再由 ManagedProcess 强制回收进程树。
                self.graceful_stop_worker(engine_id, entry, &mp).await;

                match mp.stop().await {
                    Ok(()) => {
                        self.commit_status_internal(engine_id, Some(operation_id), |status| {
                            status.process = ProcessState::Stopped;
                            status.service = ServiceHealth::Unknown;
                            status.model = ModelHealth::Unknown;
                            status.active_implementation = None;
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
                    status.active_implementation = None;
                    status.last_error = None;
                })
                .await?;
                Ok(())
            }
        }
    }

    /// 引擎优雅停止（0.22.7）。
    ///
    /// 根据引擎的 `service_transport` 分派路径：
    ///
    /// - **StdioWorker**（FunASR）：发送 NDJSON `shutdown` + drop 客户端（stdin EOF），
    ///   worker 自行退出。
    /// - **HTTP**（PaddleOCR legacy）：POST `/shutdown`（带 `X-Engine-Token` 鉴权），
    ///   Python server 收到后设置 `should_exit = True` 自行退出。
    /// - **InProcess**（0.22.9 implementation 层，OCR ONNX）：无子进程，
    ///   优雅停止不适用——直接返回（进程级兜底不存在）。
    ///
    /// 前两条路径都在 `GRACEFUL_WAIT_SECS` 窗口内轮询进程状态；
    /// 无论结果如何，后续 `ManagedProcess::stop` 兜底回收（Job Object）。
    pub(in super::super) async fn graceful_stop_worker(
        &self,
        engine_id: &EngineId,
        entry: &Arc<EngineEntry>,
        managed: &Arc<ManagedProcess>,
    ) {
        const GRACEFUL_WAIT_SECS: u64 = 5;

        let transport = entry.adapter.descriptor().service_transport;

        match transport {
            crate::domain::local_engine::ServiceTransport::StdioWorker => {
                // StdioWorker 路径：shutdown 请求 + stdin EOF
                let client = entry.worker_client.lock().await.take();
                let Some(client) = client else {
                    return; // 客户端已销毁
                };
                tracing::debug!(engine = %engine_id, "优雅停止 stdio worker：shutdown + EOF");
                client.request_shutdown().await;
                drop(client); // stdin EOF

                wait_graceful_exit(engine_id, managed, GRACEFUL_WAIT_SECS, "stdio worker").await;
            }
            crate::domain::local_engine::ServiceTransport::Http => {
                // HTTP 路径：POST /shutdown（带 X-Engine-Token 鉴权）
                let identity = entry.current_identity().await;
                let Some(identity) = identity else {
                    tracing::debug!(
                        engine = %engine_id,
                        "优雅停止 HTTP 引擎：无 launch snapshot（已停止？），跳过"
                    );
                    return;
                };

                let base_url = identity.endpoint.base_url();
                let token = identity.token.clone();
                let shutdown_url = format!("{base_url}/shutdown");

                tracing::debug!(
                    engine = %engine_id,
                    url = %shutdown_url,
                    "优雅停止 HTTP 引擎：POST /shutdown"
                );

                // 构建短超时 HTTP client——shutdown 请求不应长时间阻塞
                let client = match reqwest::Client::builder()
                    .timeout(std::time::Duration::from_secs(3))
                    .build()
                {
                    Ok(c) => c,
                    Err(e) => {
                        tracing::warn!(
                            engine = %engine_id,
                            error = %e,
                            "构建 HTTP client 失败，跳过优雅停止"
                        );
                        return;
                    }
                };

                match client
                    .post(&shutdown_url)
                    .header("X-Engine-Token", &token)
                    .send()
                    .await
                {
                    Ok(resp) if resp.status().is_success() => {
                        tracing::info!(
                            engine = %engine_id,
                            status = %resp.status(),
                            "HTTP /shutdown 请求成功，等待进程退出"
                        );
                    }
                    Ok(resp) => {
                        tracing::warn!(
                            engine = %engine_id,
                            status = %resp.status(),
                            "HTTP /shutdown 返回非成功状态，转入强制回收"
                        );
                    }
                    Err(e) => {
                        tracing::warn!(
                            engine = %engine_id,
                            error = %e,
                            "HTTP /shutdown 请求失败，转入强制回收"
                        );
                    }
                }

                wait_graceful_exit(engine_id, managed, GRACEFUL_WAIT_SECS, "HTTP 引擎").await;
            }
            crate::domain::local_engine::ServiceTransport::InProcess => {
                // in-process 引擎没有子进程，优雅停止不适用；
                // 正常路径不会到达此处（无 managed process），到达即防御性记录
                tracing::debug!(
                    engine = %engine_id,
                    "优雅停止 InProcess 引擎：无子进程，跳过"
                );
            }
        }
    }

    /// 清理运行实例状态：取消日志 pump、删除 lease、清 launch snapshot、
    /// 移除 process registry 条目。
    ///
    /// `remove_lease`: stop/exit 路径删除；`stop_if_current` 条件停止成功后
    /// 同样删除（与 stop 语义一致）。
    ///
    /// 0.22.7：同时销毁 stdio worker 客户端（关闭管道）并清空受管音频目录。
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

        // 0.22.7：销毁 worker 客户端（drop → stdin EOF）+ 清空音频临时目录
        {
            let client = entry.worker_client.lock().await.take();
            if client.is_some() {
                tracing::debug!(engine = %engine_id, "clear_running_instance: 销毁 worker 客户端");
            }
        }
        super::super::super::funasr::worker::clean_audio_tmp_dir(engine_id);

        // 取出 instance_id 用于 lease 删除与 registry 移除
        let saved_instance_id = entry
            .current_identity()
            .await
            .map(|i| i.instance_id.clone());

        if remove_lease_flag
            && let Some(ref inst_id) = saved_instance_id
            && let Err(e) = remove_lease(&engine_id.to_string(), inst_id)
        {
            tracing::warn!(
                engine = %engine_id,
                instance = %inst_id,
                %e,
                "清理实例: 删除 lease 失败（继续清理）"
            );
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
    #[allow(dead_code)] // D 包迁移后 ONNX in-process 模式不再调用进程级停止
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

                // 0.22.7：先走优雅停止（条件停止路径同样适用）
                self.graceful_stop_worker(engine_id, &entry, &mp).await;

                match mp.stop_if_current(instance_token).await {
                    Ok(()) => {
                        self.commit_status_internal(engine_id, Some(&operation_id), |status| {
                            status.process = ProcessState::Stopped;
                            status.service = ServiceHealth::Unknown;
                            status.model = ModelHealth::Unknown;
                            status.active_implementation = None;
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
                    status.active_implementation = None;
                    status.last_error = None;
                })
                .await?;
                Ok(())
            }
        }
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
    pub(in super::super) async fn rollback_started_instance(
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
            if let Some(managed) = mp.as_ref()
                && let Err(e) = managed.stop().await
            {
                tracing::warn!(
                    engine = %engine_id,
                    error = %e,
                    "rollback: ManagedProcess.stop 失败（继续清理）"
                );
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
                // start 失败回滚——冻结的 implementation 一并清除
                status.active_implementation = None;
                status.last_error = Some(error.clone());
            })
            .await;
    }
}

/// 在优雅停止窗口内轮询进程是否已自行退出。
///
/// 进程在窗口内退出则记 info；超时则记 warn，由调用方后续 `ManagedProcess::stop` 兜底回收。
async fn wait_graceful_exit(
    engine_id: &EngineId,
    managed: &Arc<ManagedProcess>,
    graceful_wait_secs: u64,
    label: &str,
) {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(graceful_wait_secs);
    loop {
        let snapshot = managed.snapshot().await;
        if snapshot.status.is_exited() || snapshot.status == ProcessStatus::Stopped {
            tracing::info!(engine = %engine_id, "{label} 已在优雅窗口内退出");
            return;
        }
        if tokio::time::Instant::now() >= deadline {
            tracing::warn!(engine = %engine_id, "{label} 优雅退出超时，转入强制回收");
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}
