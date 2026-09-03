//! EngineManager 状态查询与提交用例：
//! catalog / 状态快照读取、get_connection、状态提交统一入口（commit_status_internal）、
//! 操作取消（cancel_operation）与 provider descriptor 访问。

use super::*;

impl EngineManager {
    // ── 查询 API（可并发） ──────────────────────────────────────────────────

    /// 返回所有引擎的 catalog（描述符列表）。
    pub async fn catalog(&self) -> Vec<EngineDefinition> {
        let entries = self.entries.read().await;
        let mut catalog: Vec<_> = entries
            .values()
            .map(|e| e.adapter.descriptor().clone())
            .collect();
        catalog.sort_by(|a, b| a.engine_id.as_str().cmp(b.engine_id.as_str()));
        catalog
    }

    /// 返回指定引擎的状态快照。
    ///
    /// 查询无副作用——不因读取而启动进程或改变 generation。
    pub async fn get_status(
        &self,
        engine_id: &EngineId,
    ) -> Result<EngineStatusSnapshot, LocalEngineError> {
        self.validate_engine_id(engine_id)?;
        let entries = self.entries.read().await;
        let entry = entries.get(engine_id).ok_or_else(|| {
            LocalEngineError::with_detail(
                LocalEngineErrorCode::Unsupported,
                ErrorPhase::Request,
                "未知引擎",
                format!("engine_id '{}' 不在 registry 中", engine_id),
            )
        })?;
        let status = entry.status.read().await;
        Ok(EngineStatusSnapshot {
            engine_id: engine_id.clone(),
            service_epoch: self.epoch.clone(),
            revision: status.revision,
            status: status.clone(),
        })
    }

    /// 返回所有引擎的状态快照列表。
    pub async fn get_all_status(&self) -> Vec<EngineStatusSnapshot> {
        let entries = self.entries.read().await;
        let mut result = Vec::new();
        for (engine_id, entry) in entries.iter() {
            let status = entry.status.read().await;
            result.push(EngineStatusSnapshot {
                engine_id: engine_id.clone(),
                service_epoch: self.epoch.clone(),
                revision: status.revision,
                status: status.clone(),
            });
        }
        result.sort_by(|a, b| a.engine_id.as_str().cmp(b.engine_id.as_str()));
        result
    }

    /// 返回引擎当前的身份信息（endpoint + token）。
    ///
    /// 0.22.4：OCR Coordinator 需要获取 PaddleOCR server 的 endpoint 和 token
    /// 来发送 HTTP /recognize 请求。
    ///
    /// 如果引擎未运行或身份未设置，返回 `None`。
    #[allow(dead_code)] // D 包迁移后 ONNX in-process 模式不再查询 HTTP endpoint
    pub async fn get_current_identity(
        &self,
        engine_id: &EngineId,
    ) -> Result<Option<crate::infra::local_engine::port::ServiceIdentityInput>, LocalEngineError>
    {
        self.validate_engine_id(engine_id)?;
        let entries = self.entries.read().await;
        let entry = entries.get(engine_id).ok_or_else(|| {
            LocalEngineError::with_detail(
                LocalEngineErrorCode::Unsupported,
                ErrorPhase::Request,
                "未知引擎",
                format!("engine_id '{}' 不在 registry 中", engine_id),
            )
        })?;
        Ok(entry.current_identity().await)
    }

    /// 返回当前运行实例的 InstanceToken（用于条件停止）。
    ///
    /// 如果引擎未运行，返回 `None`。
    #[allow(dead_code)] // D 包迁移后 ONNX in-process 模式不再查询进程 token
    pub async fn get_current_instance_token(
        &self,
        engine_id: &EngineId,
    ) -> Result<Option<crate::infra::local_engine::state::InstanceToken>, LocalEngineError> {
        self.validate_engine_id(engine_id)?;
        let entries = self.entries.read().await;
        let entry = entries.get(engine_id).ok_or_else(|| {
            LocalEngineError::with_detail(
                LocalEngineErrorCode::Unsupported,
                ErrorPhase::Request,
                "未知引擎",
                format!("engine_id '{}' 不在 registry 中", engine_id),
            )
        })?;
        let mp = entry.managed_process.lock().await;
        match mp.as_ref() {
            Some(managed) => Ok(Some(managed.current_token().await)),
            None => Ok(None),
        }
    }

    /// 返回当前运行实例冻结的模型 id（0.22.7）。
    ///
    /// 模型身份在 start 时从 active 部署 manifest 冻结到 `LaunchSnapshot`。
    /// 此方法供模型切换事务判断"运行中模型是否与新选择一致"。
    /// 引擎未运行或无模型合同时返回 `None`。
    pub async fn get_current_model_id(
        &self,
        engine_id: &EngineId,
    ) -> Result<Option<String>, LocalEngineError> {
        self.validate_engine_id(engine_id)?;
        let entries = self.entries.read().await;
        let entry = entries.get(engine_id).ok_or_else(|| {
            LocalEngineError::with_detail(
                LocalEngineErrorCode::Unsupported,
                ErrorPhase::Request,
                "引擎未注册",
                format!("engine_id={engine_id}"),
            )
        })?;
        let launch = entry.current_launch().await;
        Ok(launch.and_then(|l| l.model.map(|m| m.model_id)))
    }

    /// 返回当前运行实例冻结的 implementation（0.22.9，只读投影）。
    ///
    /// 读取 start 时冻结的 launch snapshot（进程型引擎）。in-process 引擎
    /// （OCR）没有 launch snapshot——其 active implementation 只投影在
    /// `EngineStatus.active_implementation`（start_inprocess 提交）。
    /// `None` = 引擎未运行、in-process 引擎，或引擎无 implementation 声明。
    /// selected 变化不影响此值。
    // 当前由测试消费；per-implementation deployment（0.22.9 后续 handoff）
    // 将以 launch snapshot 冻结的 implementation 为主键。
    #[allow(dead_code)]
    pub async fn get_current_implementation(
        &self,
        engine_id: &EngineId,
    ) -> Result<Option<ImplementationId>, LocalEngineError> {
        self.validate_engine_id(engine_id)?;
        let entries = self.entries.read().await;
        let entry = entries.get(engine_id).ok_or_else(|| {
            LocalEngineError::with_detail(
                LocalEngineErrorCode::Unsupported,
                ErrorPhase::Request,
                "引擎未注册",
                format!("engine_id={engine_id}"),
            )
        })?;
        let launch = entry.current_launch().await;
        Ok(launch.and_then(|l| l.implementation))
    }

    /// 返回引擎诊断信息。
    pub async fn get_diagnostics(
        &self,
        engine_id: &EngineId,
    ) -> Result<EngineDiagnostic, LocalEngineError> {
        self.validate_engine_id(engine_id)?;
        let entries = self.entries.read().await;
        let entry = entries.get(engine_id).ok_or_else(|| {
            LocalEngineError::with_detail(
                LocalEngineErrorCode::Unsupported,
                ErrorPhase::Request,
                "未知引擎",
                format!("engine_id '{}' 不在 registry 中", engine_id),
            )
        })?;
        Ok(entry.adapter.diagnostics())
    }

    /// 返回当前运行实例的受限连接快照。
    ///
    /// 0.22.3 Task A: STT transcription client 必须通过此方法获取
    /// endpoint + token + 身份信息，在请求中携带 `X-Engine-Token` 鉴权。
    ///
    /// stop 或重启后旧 connection 的 token 不匹配新实例——
    /// Python server 会拒绝旧 token 的请求（401）。
    /// 无运行实例时返回 None。
    ///
    /// 0.22.7：StdioWorker 引擎额外附带 worker transport（来自 start 时
    /// ready 握手建立的 NDJSON 客户端）；管道随实例生命周期销毁。
    pub async fn get_connection(
        &self,
        engine_id: &EngineId,
    ) -> Result<Option<LocalEngineConnection>, LocalEngineError> {
        self.validate_engine_id(engine_id)?;
        let entry = self.get_entry(engine_id).await?;
        let ci = entry.current_identity().await;
        let worker = entry.worker_client.lock().await.clone().map(|client| {
            let audio_dir = super::super::funasr::worker::engine_audio_tmp_dir(engine_id);
            // Handoff 02 §4：参数传播证据链——从 SttConfig 构建 TranscribeOptions
            // 传播路径：SttConfig.local_engine → FunasrEngineConfig → TranscribeOptions → worker NDJSON
            // 0.22.7 契约收口：use_itn 已删除（GGUF worker 不消费；SenseVoice 内置 ITN 不可控）
            let options = crate::infra::local_engine::worker_proto::TranscribeOptions {
                // language: 从模型能力声明推导（模型支持多语言时传入用户配置的 language hint；
                // 当前协议只对 SenseVoice 语义有效，worker 按需消费）
                language: None, // 语言提示暂不开放（模型自动检测），保留协议能力位
            };
            std::sync::Arc::new(
                super::super::funasr::worker::GgufSttTransport::with_options(
                    client, audio_dir, options,
                ),
            ) as std::sync::Arc<dyn crate::domain::stt::SttTransport>
        });
        Ok(ci.as_ref().map(|identity| LocalEngineConnection {
            endpoint: identity.endpoint.base_url(),
            engine_id: identity.engine_id.clone(),
            instance_id: identity.instance_id.clone(),
            worker,
        }))
    }

    /// 取消操作。
    ///
    /// 只取消完全匹配 `operation_id` 的活跃 claim token。
    /// **取消是正常协议语义**——返回 `CancelOutcome`，不用错误类型表达：
    /// - claim 由 worker 的 RAII guard 持有——cancel 后 claim 不释放，
    ///   直到 worker 真正结束才允许下一个操作；
    /// - 已完成的 operation 不再是 busy state → `NoActiveOperation`；
    /// - 错配的 operation_id → `Mismatched`，不触发任何 token。
    pub async fn cancel_operation(
        &self,
        engine_id: &EngineId,
        operation_id: &str,
    ) -> CancelOutcome {
        if let Err(e) = self.validate_engine_id(engine_id) {
            tracing::warn!(engine = %engine_id, %e, "取消请求 engine_id 无效");
            return CancelOutcome::NoActiveOperation;
        }

        let outcome = self.coordinator.cancel(engine_id, operation_id);
        if outcome.is_cancelled() {
            tracing::info!(
                engine = %engine_id,
                op = %operation_id,
                "操作取消信号已发送（worker 结束前 claim 不释放）"
            );
        } else {
            tracing::info!(
                engine = %engine_id,
                op = %operation_id,
                outcome = ?outcome,
                "取消请求未命中活跃操作"
            );
        }
        outcome
    }

    // ── provider descriptor / provider 访问（0.22.5 H1）─────────────────────

    /// 返回指定引擎的 `ProviderDescriptor` 引用。
    ///
    /// 用于 catalog 兼容性检查——commands 从 `ProviderDescriptor.profiles`
    /// + `RuntimeProvider::check_compatibility` 获取真源兼容性。
    pub fn provider_descriptor_for_engine(
        &self,
        engine_id: &EngineId,
    ) -> Option<&ProviderDescriptor> {
        self.provider_descriptors.get(engine_id)
    }

    /// 返回 `PythonVenvProvider` 引用。
    ///
    /// 用于 catalog 兼容性检查——commands 调用
    /// `RuntimeProvider::check_compatibility` 判定本机兼容性。
    pub fn python_provider(&self) -> &PythonVenvProvider {
        &self.python_provider
    }

    // ── mark_needs_rebuild（0.22.5 H2）──────────────────────────────────────

    /// 标记引擎环境为 `NeedsRebuild`。
    ///
    /// 当用户在偏好页面切换 compute profile（如 CPU → CUDA）时，
    /// 旧 generation 不能继续当作新 profile Ready。
    /// 此方法将环境投影为 `NeedsRebuild`，并广播状态事件。
    ///
    /// **不启动安装、不停止进程**——只投影状态。
    /// 用户点击修复/重建后走现有事务生成新 generation。
    pub async fn mark_needs_rebuild(&self, engine_id: &EngineId) -> Result<(), LocalEngineError> {
        self.commit_status_internal(engine_id, None, |status| {
            status.environment = EnvironmentHealth::NeedsRebuild;
        })
        .await
    }

    // ── 内部辅助 ────────────────────────────────────────────────────────────

    /// 验证 engine_id 在 registry 中。
    pub(super) fn validate_engine_id(&self, engine_id: &EngineId) -> Result<(), LocalEngineError> {
        match self.registry.lookup(engine_id) {
            crate::app::local_engine::registry::RegistryLookup::Found(_) => Ok(()),
            crate::app::local_engine::registry::RegistryLookup::UnknownEngine { requested } => {
                Err(LocalEngineError::with_detail(
                    LocalEngineErrorCode::Unsupported,
                    ErrorPhase::Request,
                    "未知引擎",
                    format!("engine_id '{}' 不在编译期 allowlist 中", requested),
                ))
            }
        }
    }

    /// 获取引擎 entry。
    pub(super) async fn get_entry(
        &self,
        engine_id: &EngineId,
    ) -> Result<Arc<EngineEntry>, LocalEngineError> {
        self.validate_engine_id(engine_id)?;
        let entries = self.entries.read().await;
        entries.get(engine_id).cloned().ok_or_else(|| {
            LocalEngineError::with_detail(
                LocalEngineErrorCode::Internal,
                ErrorPhase::Request,
                "内部错误",
                format!("engine_id '{}' 通过 registry 验证但未找到 entry", engine_id),
            )
        })
    }

    /// 状态提交统一入口。
    ///
    /// 1. 验证 epoch
    /// 2. 验证 operation_id（busy 真源 = `EngineOperationCoordinator` 的 claim）
    /// 3. revision +1
    /// 4. 广播完整 snapshot
    pub(super) async fn commit_status_internal(
        &self,
        engine_id: &EngineId,
        operation_id: Option<&str>,
        updater: impl FnOnce(&mut EngineStatus),
    ) -> Result<(), LocalEngineError> {
        let entry = self.get_entry(engine_id).await?;
        let mut status = entry.status.write().await;

        // 验证 epoch
        if status.service_epoch != self.epoch {
            // 新 epoch——重置状态
            *status = EngineStatus {
                service_epoch: self.epoch.clone(),
                ..Default::default()
            };
        }

        // 验证 operation_id（fail-closed）——活跃 claim 唯一真源是协调器
        let current_op_id = self.coordinator.active_operation(engine_id);
        match (&current_op_id, operation_id) {
            (Some(current), Some(submitted)) => {
                if submitted != current.as_str() {
                    return Err(LocalEngineError::with_detail(
                        LocalEngineErrorCode::Rejected,
                        ErrorPhase::Request,
                        "操作已过期",
                        format!(
                            "operation_id 不匹配: expected={}, got={}",
                            current, submitted
                        ),
                    ));
                }
            }
            // 有活跃操作但提交未携带 operation_id → 拒绝
            (Some(current), None) => {
                return Err(LocalEngineError::with_detail(
                    LocalEngineErrorCode::Rejected,
                    ErrorPhase::Request,
                    "操作进行中，请等待",
                    format!("有活跃操作但提交未携带 operation_id (current={current})"),
                ));
            }
            // 无活跃操作但提交携带 operation_id → fail-closed 拒绝
            // 防止迟到的任务（已取消/已失败的 operation）覆写新状态
            (None, Some(submitted)) => {
                tracing::warn!(
                    engine = %engine_id,
                    submitted_op = %submitted,
                    "提交携带 operation_id 但无活跃操作，拒绝（fail-closed）"
                );
                return Err(LocalEngineError::with_detail(
                    LocalEngineErrorCode::Rejected,
                    ErrorPhase::Request,
                    "操作已过期",
                    format!(
                        "提交携带 operation_id={submitted} 但无活跃操作（可能已取消/完成/失败）"
                    ),
                ));
            }
            // 无活跃操作且提交不携带 operation_id → 允许（非操作状态转换）
            (None, None) => {}
        }

        // revision +1
        let new_revision = status.revision + 1;

        // 应用更新
        updater(&mut status);
        status.revision = new_revision;

        // 广播
        let snapshot = EngineStatusSnapshot {
            engine_id: engine_id.clone(),
            service_epoch: self.epoch.clone(),
            revision: new_revision,
            status: status.clone(),
        };
        self.event_port.emit_status(&snapshot);

        Ok(())
    }
}
