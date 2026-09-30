use super::*;

impl ManagedProcess {
    // ── wait / snapshot / 公共 API ──────────────────────────────────────────

    /// 等待进程退出（async）。如果进程已退出或未运行，立即返回。
    ///
    /// 使用 watch channel 而非固定轮询，避免超时伪装成功。
    #[allow(dead_code)] // 测试断言进程终态用
    pub async fn wait(&self) -> Result<ProcessStatus, ManagedProcessError> {
        {
            let inner = self.inner.lock().await;
            if inner.state.status.is_exited() || inner.state.status == ProcessStatus::Stopped {
                return Ok(inner.state.status.clone());
            }
        }

        let mut rx = self.status_notify.subscribe();
        loop {
            {
                let inner = self.inner.lock().await;
                if inner.state.status.is_exited() || inner.state.status == ProcessStatus::Stopped {
                    return Ok(inner.state.status.clone());
                }
            }
            if rx.changed().await.is_err() {
                let inner = self.inner.lock().await;
                return Ok(inner.state.status.clone());
            }
            let status = rx.borrow().clone();
            if status.is_exited() || status == ProcessStatus::Stopped {
                return Ok(status);
            }
        }
    }

    /// 获取当前状态快照（只读）。
    pub async fn snapshot(&self) -> ManagedProcessState {
        let inner = self.inner.lock().await;
        inner.state.clone()
    }

    /// 获取当前 token。
    pub async fn current_token(&self) -> InstanceToken {
        let inner = self.inner.lock().await;
        inner.state.token.clone()
    }

    /// 检查指定 token 是否为当前实例（条件停止辅助）。
    pub async fn is_current_token(&self, token: &InstanceToken) -> bool {
        let inner = self.inner.lock().await;
        inner.state.is_current(token)
    }

    /// 获取 PID（如果运行中）。
    pub async fn pid(&self) -> Option<u32> {
        let inner = self.inner.lock().await;
        match &inner.state.status {
            ProcessStatus::Running { pid } => Some(*pid),
            _ => None,
        }
    }

    /// 获取日志历史。
    pub async fn log_history(&self) -> Vec<LogEntry> {
        self.log_pipe.history().await
    }

    /// 订阅实时日志流。
    pub fn subscribe_logs(&self) -> LogSubscriber {
        self.log_pipe.subscribe()
    }

    /// 一次性取走双向协议 worker 的 stdio 句柄（0.22.7）。
    ///
    /// 仅当启动时声明 `StdioConfig::worker_protocol()` 且进程仍为当前实例时
    /// 返回 `Some`；重复调用返回 `None`。
    ///
    /// **调用方职责**：取走 stdin 后，正常停止路径由调用方先 drop 该句柄
    /// （管道 EOF）让 worker 自行退出，再走 `ManagedProcess::stop` 兜底回收；
    /// 未取走时任何 stop/exit/重启路径都会 drop 残留句柄关闭管道。
    pub async fn take_worker_stdio(&self) -> Option<WorkerStdio> {
        let mut inner = self.inner.lock().await;
        inner.worker_stdio.take()
    }

    /// 获取截断行计数。
    #[allow(dead_code)] // 测试断言日志洪泛截断用
    pub fn log_truncated_count(&self) -> u64 {
        self.log_pipe.truncated_line_count()
    }

    /// 订阅进程状态变更通知（0.22.6.3）。
    ///
    /// 返回 `watch::Receiver<ProcessStatus>`，调用方可以：
    /// - `rx.borrow().clone()` 获取当前状态快照
    /// - `rx.changed().await` 等待状态变更
    ///
    /// `EngineManager` 使用此方法监听进程意外退出，
    /// 在 server crash 后收敛 `EngineStatus` 到 Exited/Unreachable。
    pub fn subscribe_status(&self) -> watch::Receiver<ProcessStatus> {
        self.status_notify.subscribe()
    }

    // ── wait_and_update ────────────────────────────────────────────────────

    /// 内部：wait task 在子进程退出后更新状态。
    ///
    /// child wait 所有权唯一：此 task 通过 `try_wait` 轮询。
    /// 如果 stop 取走了 child，此 task 退出。
    pub(super) async fn wait_and_update(self: Arc<Self>, token: InstanceToken, pid: u32) {
        loop {
            if self
                .shutdown_flag
                .load(std::sync::atomic::Ordering::Acquire)
            {
                tracing::debug!(pid, gen = token.generation, "wait task: shutdown flag set, exiting");
                return;
            }

            let exit_status = {
                let mut inner = self.inner.lock().await;
                if !inner.state.is_current(&token) {
                    tracing::debug!(
                        gen = token.generation,
                        current = inner.state.generation(),
                        "wait task: token 过期，退出"
                    );
                    return;
                }
                if inner.child.is_none() {
                    return;
                }
                let child = inner.child.as_mut().unwrap();
                child.try_wait().ok().flatten()
            };

            match exit_status {
                Some(status) => {
                    let reason = if status.success() {
                        ExitReason::NormalExit {
                            code: status.code().unwrap_or(0),
                        }
                    } else {
                        ExitReason::NonZeroExit {
                            code: status.code().unwrap_or(-1),
                        }
                    };

                    let mut inner = self.inner.lock().await;
                    if !inner.state.is_current(&token) {
                        tracing::debug!(
                            gen = token.generation,
                            "wait task: token 过期，不更新状态"
                        );
                        return;
                    }

                    inner.state.try_commit_exit(&token, reason.clone());
                    inner.child = None;

                    #[cfg(windows)]
                    {
                        if let Some(h) = self.take_job_for_token(&token) {
                            drop(h);
                        }
                    }

                    let _ = self.status_notify.send(ProcessStatus::Exited {
                        reason: reason.clone(),
                    });

                    tracing::info!(
                        pid,
                        gen = token.generation,
                        reason = ?reason,
                        "ManagedProcess: process exited"
                    );
                    return;
                }
                None => {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
        }
    }
}
