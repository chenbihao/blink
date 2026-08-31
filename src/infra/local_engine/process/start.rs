use super::*;

impl ManagedProcess {
    // ── start ──────────────────────────────────────────────────────────────

    /// 幂等 start：如果已在运行返回 AlreadyRunning，否则启动新进程。
    ///
    /// ## 原子性保证
    ///
    /// "检查允许启动 → 创建 token → 创建 StartOperation → 状态进入 Starting"
    /// 在单次 inner lock 内完成。重复 start 不创建、覆盖或完成当前 StartOperation。
    pub async fn start(self: &Arc<Self>, req: &LaunchRequest) -> Result<(), ManagedProcessError> {
        // Phase 1: 原子决策——在单次 inner lock 内完成状态检查 + token 创建 + StartOperation 创建
        let (token, _start_rx) = {
            let mut inner = self.inner.lock().await;
            match &inner.state.status {
                ProcessStatus::Running { .. }
                | ProcessStatus::Starting
                | ProcessStatus::Stopping => {
                    // 重复 start：不创建、不覆盖、不完成当前 operation
                    return Err(ManagedProcessError::AlreadyRunning {
                        generation: inner.state.generation(),
                    });
                }
                ProcessStatus::Exited { .. } | ProcessStatus::Stopped => {
                    // 清理上一代已完成的 operation
                    inner.start_op = None;
                    inner.stop_op = None;
                    // 上一代遗留的 worker stdio（未被取走时）随新 start 一并丢弃——
                    // drop ChildStdin 关闭管道，触发旧 worker（若仍存活）收到 EOF。
                    inner.worker_stdio = None;

                    // 创建新 token
                    let token = inner.state.begin_start();
                    inner.cancellation = inner.state.cancellation_flag();
                    inner.force_stop_timeout = req.shutdown.force_stop_timeout;

                    // 创建 StartOperation 绑定到此 token
                    let (start_op, start_rx) = StartOperation::new(token.clone());
                    inner.start_op = Some(Arc::new(start_op));

                    let _ = self.status_notify.send(ProcessStatus::Starting);
                    (token, start_rx)
                }
            }
        };

        tracing::info!(
            label = %req.label,
            instance_id = %token.instance_id,
            gen = token.generation,
            "ManagedProcess: starting"
        );

        // 测试 gate：仅当测试方安装了 spawn_gate 时才阻塞
        #[cfg(test)]
        {
            if let Some(ref gate_tx) = self.spawn_gate {
                let mut gate_rx = gate_tx.subscribe();
                if !*gate_rx.borrow() {
                    let _ = gate_rx.changed().await;
                }
            }
        }

        let spawn_result = spawn_child(req).await;

        match spawn_result {
            Ok(spawned) => {
                let SpawnedChild {
                    mut child,
                    stdin,
                    stdout,
                    stderr,
                    pid,
                } = spawned;

                #[cfg(windows)]
                let job_handle = match crate::infra::platform::process::assign_job_object(pid) {
                    Ok(handle) => handle,
                    Err(e) => {
                        tracing::error!(%e, "Job Object 分配失败，终止子进程");
                        let kill_err = child.start_kill().err();
                        let wait_err = child.wait().await.err();
                        tracing::warn!(?kill_err, ?wait_err, "Job Object 失败后 child 回收");

                        let fail_msg = format!("Job Object 分配失败: {e}");
                        {
                            let mut inner = self.inner.lock().await;
                            if inner.state.is_current(&token) {
                                inner.state.set_status_exited(ExitReason::StartFailed {
                                    message: fail_msg.clone(),
                                });
                                let _ = self.status_notify.send(ProcessStatus::Exited {
                                    reason: ExitReason::StartFailed {
                                        message: fail_msg.clone(),
                                    },
                                });
                            }
                            // 完成属于自己的 StartOperation
                            if let Some(ref op) = inner.start_op
                                && op.token == token
                            {
                                op.complete(StartOutcome::Failed {
                                    message: fail_msg.clone(),
                                });
                            }
                        }
                        return Err(ManagedProcessError::JobObjectFailed { message: e });
                    }
                };

                // 查询真实 OS creation time（fail-closed：失败返回 0，kill_process_tree_verified 会拒绝）
                let creation_time = get_os_creation_time_ms(pid);

                let identity = ProcessIdentity {
                    pid,
                    executable: req.executable.clone(),
                    start_time_ms: creation_time,
                    instance_id: token.instance_id.clone(),
                };

                // Job 必须先按 token 安装，再公开 Running。这样任何观察到 Running
                // 的 stop 都能在同一时刻取得 child，并能通过 Job 回收进程树。
                #[cfg(windows)]
                self.install_job(&token, job_handle);

                #[cfg(test)]
                {
                    self.pre_running_commit_pid
                        .store(pid, std::sync::atomic::Ordering::Release);
                    if let Some(ref gate_tx) = self.pre_running_commit_gate {
                        let mut gate_rx = gate_tx.subscribe();
                        if !*gate_rx.borrow() {
                            let _ = gate_rx.changed().await;
                        }
                    }
                }

                let mut child_slot = Some(child);
                let commit_result = {
                    let mut inner = self.inner.lock().await;
                    let result = inner.state.try_commit_running(&token, pid, identity);
                    if result == CommitResult::Committed {
                        // Running 状态与 child 所有权在同一临界区内发布。
                        inner.child = child_slot.take();
                        let _ = self.status_notify.send(ProcessStatus::Running { pid });
                        if let Some(ref op) = inner.start_op
                            && op.token == token
                        {
                            op.complete(StartOutcome::Running { pid });
                        }
                    }
                    result
                };

                if commit_result.needs_reclaim() {
                    tracing::warn!(gen = token.generation, ?commit_result, "Running 提交失败，回收 child");
                    let mut child = child_slot
                        .take()
                        .expect("未提交 Running 时 child 必须仍由 start 持有");
                    let kill_err = child.start_kill().err();
                    let wait_result =
                        tokio::time::timeout(Duration::from_secs(5), child.wait()).await;
                    tracing::warn!(?kill_err, ?wait_result, "提交失败后 child 回收完成");
                    drop(child);

                    #[cfg(windows)]
                    {
                        if let Some(h) = self.take_job_for_token(&token) {
                            drop(h);
                        }
                    }

                    let outcome = if commit_result == CommitResult::Cancelled {
                        StartOutcome::Cancelled
                    } else {
                        StartOutcome::Failed {
                            message: "Running 提交被拒绝".to_string(),
                        }
                    };

                    {
                        let mut inner = self.inner.lock().await;
                        if inner.state.is_current(&token) {
                            let reason = if commit_result == CommitResult::Cancelled {
                                ExitReason::StartCancelled
                            } else {
                                ExitReason::StartFailed {
                                    message: "Running 提交被拒绝".to_string(),
                                }
                            };
                            inner.state.set_status_exited(reason.clone());
                            let _ = self.status_notify.send(ProcessStatus::Exited { reason });
                        }
                        // 完成属于自己的 StartOperation
                        if let Some(ref op) = inner.start_op
                            && op.token == token
                        {
                            op.complete(outcome);
                        }
                    }
                    return Ok(());
                }

                // 双向协议 worker：stdin/stdout 交由调用方接管（一次性取走）。
                // stdout 不进 LogPipe（协议通道只承载 NDJSON）；stderr 照常泵入。
                let mut worker_stdio_slot = None;
                let mut stdout = stdout;
                if req.stdio.stdin_piped && req.stdio.stdout_handoff {
                    if let (Some(s_in), Some(s_out)) = (stdin, stdout.take()) {
                        worker_stdio_slot = Some(WorkerStdio {
                            stdin: s_in,
                            stdout: s_out,
                        });
                    } else {
                        tracing::warn!(pid, "worker stdio 管道缺失，跳过接管（协议不可用）");
                    }
                }

                // 启动 pump（使用 ManagedProcess 的 log_config，唯一真源）
                let max_bytes = self.log_config.max_line_bytes;
                if let Some(stdout) = stdout
                    && !req.stdio.stdout_handoff
                {
                    let lp = self.log_pipe.clone();
                    tokio::spawn(async move {
                        pump_lines(stdout, LogSource::Stdout, &lp, max_bytes).await;
                    });
                }
                if let Some(stderr) = stderr {
                    let lp = self.log_pipe.clone();
                    tokio::spawn(async move {
                        pump_lines(stderr, LogSource::Stderr, &lp, max_bytes).await;
                    });
                }

                // 公开 worker stdio（Running 提交成功后）。未取走时随 stop/exit drop。
                if let Some(ws) = worker_stdio_slot {
                    let mut inner = self.inner.lock().await;
                    if inner.state.is_current(&token) {
                        inner.worker_stdio = Some(ws);
                    }
                }

                // 启动 wait task
                let inner_ref = Arc::clone(self);
                let token_clone = token.clone();
                tokio::spawn(async move {
                    inner_ref.wait_and_update(token_clone, pid).await;
                });

                tracing::info!(label = %req.label, pid, gen = token.generation, "ManagedProcess: started");
                Ok(())
            }
            Err(e) => {
                {
                    let mut inner = self.inner.lock().await;
                    if inner.state.is_current(&token) {
                        inner
                            .state
                            .set_status_exited(ExitReason::StartFailed { message: e.clone() });
                        let _ = self.status_notify.send(ProcessStatus::Exited {
                            reason: ExitReason::StartFailed { message: e.clone() },
                        });
                    }
                    // 完成属于自己的 StartOperation
                    if let Some(ref op) = inner.start_op
                        && op.token == token
                    {
                        op.complete(StartOutcome::Failed { message: e.clone() });
                    }
                }
                Err(ManagedProcessError::SpawnFailed { message: e })
            }
        }
    }
}
