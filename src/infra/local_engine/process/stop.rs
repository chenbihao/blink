use super::*;

impl ManagedProcess {
    // ── stop ───────────────────────────────────────────────────────────────

    /// 幂等 stop：如果未运行返回 Ok，否则发起停止。
    ///
    /// 多个并发 stop 共享同一个停止结果。
    pub async fn stop(&self) -> Result<(), ManagedProcessError> {
        self.stop_impl(None).await
    }

    /// 条件停止：只停止指定 token 的实例。
    pub async fn stop_if_current(&self, token: &InstanceToken) -> Result<(), ManagedProcessError> {
        self.stop_impl(Some(token)).await
    }

    async fn stop_impl(
        &self,
        expected_token: Option<&InstanceToken>,
    ) -> Result<(), ManagedProcessError> {
        // Phase 1: 原子决策——在单次 inner lock 内完成
        // 状态检查 + token 验证 + StopOperation 创建 + Stopping 状态转换
        let plan = {
            let mut inner = self.inner.lock().await;

            if let Some(tok) = expected_token
                && !inner.state.is_current(tok)
            {
                tracing::debug!(
                    expected_gen = tok.generation,
                    current_gen = inner.state.generation(),
                    "stop_if_current: token 不匹配，跳过"
                );
                return Ok(());
            }

            match &inner.state.status {
                ProcessStatus::Stopped | ProcessStatus::Exited { .. } => StopPlan::AlreadyStopped,

                ProcessStatus::Stopping => {
                    // 已有 stop 在执行——只能订阅已存在的 StopOperation
                    // 绝不创建新 operation，绝不成为 executor
                    if let Some(ref stop_op) = inner.stop_op {
                        let stop_rx = stop_op.subscribe();
                        StopPlan::WaitConcurrent { stop_rx }
                    } else {
                        // 状态为 Stopping 但 stop_op 缺失——内部不变量被破坏
                        tracing::error!(
                            gen = inner.state.generation(),
                            "stop: Stopping 状态但 stop_op 缺失（内部不变量错误）"
                        );
                        return Err(ManagedProcessError::InternalInconsistency {
                            message: "Stopping 状态但 stop_op 缺失".to_string(),
                        });
                    }
                }

                ProcessStatus::Starting => {
                    // stop 在 Starting 阶段到达——取消启动
                    inner.state.mark_cancelled();

                    let token = inner.state.token.clone();
                    let force_timeout = inner.force_stop_timeout;

                    // 原子创建 StopOperation 并成为 executor
                    let (stop_op, _stop_rx) = StopOperation::new();
                    let stop_op = Arc::new(stop_op);
                    inner.stop_op = Some(Arc::clone(&stop_op));

                    // 状态改为 Stopping
                    inner.state.set_status_stopping();
                    let _ = self.status_notify.send(ProcessStatus::Stopping);

                    // 获取 StartOperation 的订阅者（用于等待 start 完成）
                    let start_rx = if let Some(ref start_op) = inner.start_op {
                        if start_op.token == token {
                            start_op.subscribe()
                        } else {
                            // start_op token 不匹配——不应该发生
                            tracing::error!(
                                gen = token.generation,
                                "stop: start_op token 不匹配（内部不变量错误）"
                            );
                            return Err(ManagedProcessError::InternalInconsistency {
                                message: "start_op token 不匹配".to_string(),
                            });
                        }
                    } else {
                        // start_op 缺失——不应该发生（Starting 状态必须有 start_op）
                        tracing::error!(
                            gen = token.generation,
                            "stop: Starting 状态但 start_op 缺失（内部不变量错误）"
                        );
                        return Err(ManagedProcessError::InternalInconsistency {
                            message: "Starting 状态但 start_op 缺失".to_string(),
                        });
                    };

                    // 取出 child（如果有迟到 child）
                    let child = inner.child.take();

                    StopPlan::CancelStart {
                        child,
                        token,
                        force_timeout,
                        start_rx,
                        stop_op,
                    }
                }

                ProcessStatus::Running { .. } => {
                    let token = inner.state.token.clone();
                    let force_timeout = inner.force_stop_timeout;

                    // 原子创建 StopOperation 并成为 executor
                    let (stop_op, _stop_rx) = StopOperation::new();
                    let stop_op = Arc::new(stop_op);
                    inner.stop_op = Some(Arc::clone(&stop_op));

                    // 状态改为 Stopping
                    inner.state.set_status_stopping();
                    let _ = self.status_notify.send(ProcessStatus::Stopping);

                    // 取出 child
                    let child = inner.child.take();

                    if let Some(child) = child {
                        StopPlan::KillChild {
                            child,
                            token,
                            force_timeout,
                            stop_op,
                        }
                    } else {
                        // Running 状态但 child 缺失——内部不变量错误
                        tracing::error!(
                            gen = token.generation,
                            "stop: Running 状态但 child 缺失（内部不变量错误）"
                        );
                        StopPlan::RunningButNoChild { token, stop_op }
                    }
                }
            }
        };

        match plan {
            StopPlan::AlreadyStopped => Ok(()),

            StopPlan::WaitConcurrent { mut stop_rx } => {
                // 等待已有 stop operation 完成
                if stop_rx.borrow().is_none() {
                    let _ = stop_rx.changed().await;
                }
                // 读取最终结果
                match stop_rx.borrow().clone() {
                    Some(StopOutcome::Done) => Ok(()),
                    Some(StopOutcome::Failed { message }) => {
                        Err(ManagedProcessError::StopFailed { message })
                    }
                    None => {
                        // 不应该发生——completion 已完成但结果为 None
                        tracing::error!("stop waiter: completion 返回 None（内部不变量错误）");
                        Err(ManagedProcessError::InternalInconsistency {
                            message: "stop completion 返回 None".to_string(),
                        })
                    }
                }
            }

            StopPlan::CancelStart {
                child,
                token,
                force_timeout,
                mut start_rx,
                stop_op,
            } => {
                // 当前调用是唯一 executor

                // 等待 StartOperation 完成——不超时。
                // start 的 spawn 要么成功要么失败，最终必定完成 StartOperation。
                // 超时后假装成功会破坏 stop postcondition（进程树可能仍存活）。
                if start_rx.borrow().is_none() {
                    let _ = start_rx.changed().await;
                }

                // 根据 start outcome 决定回收策略
                let start_outcome = start_rx.borrow().clone();
                match start_outcome {
                    Some(StartOutcome::Running { pid }) => {
                        // start 成功了但被取消——需要 kill child
                        // start 已经存储了 child，我们需要从 inner 取出
                        tracing::info!(pid, gen = token.generation, "stop: start 完成但已取消，kill child");
                        let child_to_kill = {
                            let mut inner = self.inner.lock().await;
                            inner.child.take()
                        };
                        if let Some(mut child) = child_to_kill {
                            let _ = child.start_kill();
                            let wait_result =
                                tokio::time::timeout(force_timeout, child.wait()).await;
                            if wait_result.is_err() {
                                tracing::warn!(pid, gen = token.generation, "stop: cancel-start child kill 超时");
                            }
                        }
                    }
                    Some(StartOutcome::Cancelled) => {
                        // start 已自行回收 child——无需再次回收
                        tracing::info!(gen = token.generation, "stop: start 已取消并回收 child");
                    }
                    Some(StartOutcome::Failed { .. }) => {
                        // start 失败——child 不存在或已被回收
                        tracing::info!(gen = token.generation, "stop: start 已失败，无需回收 child");
                    }
                    None => {
                        // start operation 未完成——不应该发生（我们已等待 changed）
                        // 但作为防御，如果有迟到 child 则回收
                        tracing::warn!(gen = token.generation, "stop: start operation 未完成结果");
                    }
                }

                // 回收可能在锁外产生的迟到 child（如果 start 在 CancelStart 后才 spawn 成功）
                if let Some(mut late_child) = child {
                    let pid = late_child.id().unwrap_or(0);
                    tracing::info!(pid, gen = token.generation, "stop: 回收 Starting 阶段迟到 child");
                    let _ = late_child.start_kill();
                    let wait_result = tokio::time::timeout(force_timeout, late_child.wait()).await;
                    if wait_result.is_err() {
                        tracing::warn!(pid, gen = token.generation, "stop: 迟到 child kill 超时");
                    }
                }

                #[cfg(windows)]
                {
                    if let Some(h) = self.take_job_for_token(&token) {
                        tracing::info!(gen = token.generation, "stop: drop Job handle");
                        drop(h);
                    }
                }

                let exit_reason = ExitReason::StartCancelled;
                {
                    let mut inner = self.inner.lock().await;
                    if inner.state.is_current(&token) {
                        inner.state.set_status_exited(exit_reason.clone());
                        let _ = self.status_notify.send(ProcessStatus::Exited {
                            reason: exit_reason.clone(),
                        });
                    }
                }

                // 完成 StopOperation
                stop_op.complete(StopOutcome::Done);
                Ok(())
            }

            StopPlan::KillChild {
                mut child,
                token,
                force_timeout,
                stop_op,
            } => {
                // 当前调用是唯一 executor
                let pid = child.id().unwrap_or(0);
                tracing::info!(pid, gen = token.generation, "ManagedProcess: stop");

                let kill_err = child.start_kill().err();
                if let Some(e) = &kill_err {
                    tracing::warn!(%e, pid, "child.start_kill 返回错误");
                }

                let timeout_result = tokio::time::timeout(force_timeout, child.wait()).await;

                let (exit_reason, stop_outcome) = match timeout_result {
                    Ok(Ok(status)) => {
                        let code = status.code();
                        let reason = ExitReason::Stopped { code };
                        tracing::info!(pid, gen = token.generation, "ManagedProcess: stopped (force kill)");
                        (reason, StopOutcome::Done)
                    }
                    Ok(Err(e)) => {
                        let reason = ExitReason::WaitError {
                            message: format!("child wait 错误: {e}"),
                        };
                        tracing::error!(%e, pid, gen = token.generation, "child.wait 返回错误");
                        (
                            reason.clone(),
                            StopOutcome::Failed {
                                message: format!("child wait 错误: {e}"),
                            },
                        )
                    }
                    Err(_) => {
                        tracing::warn!(pid, gen = token.generation, "ManagedProcess: force_stop 超时，强制回收");

                        #[cfg(windows)]
                        {
                            if let Some(h) = self.take_job_for_token(&token) {
                                tracing::info!(pid, gen = token.generation, "Job handle drop (KILL_ON_JOB_CLOSE)");
                                drop(h);
                            }
                        }

                        // 非 Windows：超时后无法依赖 Job Object，执行有限 deadline 的最终 wait
                        #[cfg(not(windows))]
                        {
                            let final_wait =
                                tokio::time::timeout(Duration::from_secs(3), child.wait()).await;
                            if let Ok(Ok(status)) = final_wait {
                                let code = status.code();
                                let reason = ExitReason::ForceKilled {
                                    deadline_exceeded: true,
                                };
                                let _ = self.status_notify.send(ProcessStatus::Exited {
                                    reason: reason.clone(),
                                });
                                let mut inner = self.inner.lock().await;
                                if inner.state.is_current(&token) {
                                    inner.state.set_status_exited(reason.clone());
                                }
                                stop_op.complete(StopOutcome::Done);
                                return Ok(());
                            }
                        }

                        // Windows：Job Object drop 后 child 应已退出，但仍然 wait 确认
                        let final_wait = child.wait().await;
                        let reason = ExitReason::ForceKilled {
                            deadline_exceeded: true,
                        };
                        tracing::info!(pid, gen = token.generation, ?final_wait, "Job Object 回收后 child 退出");
                        (reason, StopOutcome::Done)
                    }
                };

                {
                    let mut inner = self.inner.lock().await;
                    if inner.state.is_current(&token) {
                        inner.state.set_status_exited(exit_reason.clone());
                        let _ = self.status_notify.send(ProcessStatus::Exited {
                            reason: exit_reason,
                        });
                    }
                }

                #[cfg(windows)]
                {
                    if let Some(h) = self.take_job_for_token(&token) {
                        drop(h);
                    }
                }

                // 完成 StopOperation
                stop_op.complete(stop_outcome);
                Ok(())
            }

            StopPlan::RunningButNoChild { token, stop_op } => {
                // Running 状态但 child 缺失——内部不变量错误
                // 尝试通过 Job Object 回收，返回结构化错误
                tracing::error!(gen = token.generation, "stop: RunningButNoChild——尝试 Job Object 回收");

                #[cfg(windows)]
                {
                    if let Some(h) = self.take_job_for_token(&token) {
                        tracing::info!(gen = token.generation, "RunningButNoChild: drop Job handle");
                        drop(h);
                    }
                }

                let fail_msg = "Running 状态但 child 缺失（内部不变量错误）".to_string();
                {
                    let mut inner = self.inner.lock().await;
                    if inner.state.is_current(&token) {
                        inner.state.set_status_exited(ExitReason::WaitError {
                            message: fail_msg.clone(),
                        });
                        let _ = self.status_notify.send(ProcessStatus::Exited {
                            reason: ExitReason::WaitError {
                                message: fail_msg.clone(),
                            },
                        });
                    }
                }

                stop_op.complete(StopOutcome::Failed {
                    message: fail_msg.clone(),
                });
                Err(ManagedProcessError::InternalInconsistency { message: fail_msg })
            }
        }
    }

    // ── shutdown_blocking ──────────────────────────────────────────────────

    /// 应用退出时的同步 kill-on-close 路径。
    ///
    /// 不依赖 async mutex（退出路径可能在非 async 上下文调用）。
    /// 通过 shutdown_flag + Job Object CloseHandle 确保可靠回收。
    pub fn shutdown_blocking(&self) {
        self.shutdown_flag
            .store(true, std::sync::atomic::Ordering::Release);

        if let Ok(mut guard) = self.inner.try_lock() {
            if let Some(child) = guard.child.as_mut() {
                let _ = child.start_kill();
                tracing::info!(
                    gen = guard.state.generation(),
                    "ManagedProcess: shutdown_blocking kill sent"
                );
            }
            guard
                .state
                .set_status_exited(ExitReason::Stopped { code: None });
            let _ = self.status_notify.send(ProcessStatus::Exited {
                reason: ExitReason::Stopped { code: None },
            });
        } else {
            tracing::warn!(
                "ManagedProcess: shutdown_blocking 无法获取锁（可能正在 stop），依赖 Job Object 回收"
            );
        }

        #[cfg(windows)]
        {
            let mut holder = self.job_holder.lock().unwrap();
            if holder.take().is_some() {
                tracing::info!("ManagedProcess: shutdown_blocking Job handle dropped");
            }
        }
    }
}
