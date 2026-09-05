//! 生命周期回收：idle TTL 定时停止与 StopAfterUse 立即停止。
//! 0.22.8-D: 全部改为调用 `executor.shutdown()` 替代 `engine_service.stop_if_current()`。

use std::sync::atomic::Ordering;
use std::time::Duration;

use crate::infra::local_engine::onnx_ocr::OcrExecutor;

use crate::domain::config::ocr_config::OcrRuntimeSnapshot;

use super::OcrCoordinator;
use super::singleflight::LifecycleState;

impl OcrCoordinator {
    /// schedule idle TTL 停止或立即停止（StopAfterUse）。
    pub(super) fn schedule_idle_stop(&self, snapshot: OcrRuntimeSnapshot) {
        if !snapshot.needs_paddleocr() {
            return;
        }
        use crate::domain::ocr::config::OcrLifecycle;

        match snapshot.lifecycle {
            OcrLifecycle::KeepRunning => {
                tracing::debug!("lifecycle=KeepRunning，跳过 idle TTL");
                return;
            }
            OcrLifecycle::StopAfterUse => {
                if self.in_flight.load(Ordering::SeqCst) > 0 {
                    tracing::debug!("StopAfterUse 但有在途请求，改为 OnDemand 行为");
                } else {
                    let current_state = self.lifecycle_state();
                    match current_state {
                        LifecycleState::Ready {
                            generation,
                            instance_token,
                        } => {
                            self.lifecycle_tx
                                .send(LifecycleState::Stopping { generation })
                                .ok();
                            tracing::info!("lifecycle=StopAfterUse，立即停止 ONNX executor");
                            let executor = self.executor.read().unwrap().clone();
                            let lifecycle_tx = self.lifecycle_tx.clone();
                            let start_elapsed_ms = self.start_elapsed_ms.clone();
                            let engine_service =
                                self.engine_service.get().and_then(|w| w.upgrade());
                            let engine_id = self.paddleocr_engine_id.clone();
                            let target_gen = generation;
                            let target_token = instance_token;
                            tokio::spawn(async move {
                                // 0.22.8-D: 调用 executor.shutdown()
                                if let Some(ref executor) = executor {
                                    executor.shutdown().await;
                                }
                                // 0.22.9：executor 回收 → 引擎卡片同步为 Stopped
                                super::sync_engine_card(engine_service, engine_id, false);
                                let current = lifecycle_tx.borrow().clone();
                                match &current {
                                    LifecycleState::Stopping { generation }
                                        if *generation == target_gen =>
                                    {
                                        *start_elapsed_ms.lock().unwrap() = None;
                                        lifecycle_tx
                                            .send(LifecycleState::Idle {
                                                generation: target_gen + 1,
                                            })
                                            .ok();
                                        tracing::debug!(
                                            "start_state 已重置为 Idle（StopAfterUse 后）"
                                        );
                                    }
                                    _ => {
                                        tracing::debug!(
                                            current = ?current,
                                            "StopAfterUse: lifecycle 已变化，不提交 Idle"
                                        );
                                    }
                                }
                                let _ = target_token;
                            });
                            return;
                        }
                        _ => {
                            tracing::debug!(current = ?current_state, "StopAfterUse: 非 Ready，跳过");
                            return;
                        }
                    }
                }
            }
            OcrLifecycle::OnDemand => {}
        }

        // OnDemand 路径
        let current_state = self.lifecycle_state();
        let (target_gen, target_token) = match current_state {
            LifecycleState::Ready {
                generation,
                instance_token,
            } => (generation, instance_token),
            _ => {
                tracing::debug!(current = ?current_state, "OnDemand idle stop: 非 Ready，跳过");
                return;
            }
        };

        let in_flight = self.in_flight.clone();
        let executor = self.executor.read().unwrap().clone();
        let idle_cancel = self.idle_cancel.clone();
        let lifecycle_tx = self.lifecycle_tx.clone();
        let start_elapsed_ms = self.start_elapsed_ms.clone();
        let engine_service = self.engine_service.get().and_then(|w| w.upgrade());
        let engine_id = self.paddleocr_engine_id.clone();
        let ttl = Duration::from_secs(snapshot.idle_ttl_seconds as u64);

        tokio::spawn(async move {
            tokio::select! {
                _ = idle_cancel.notified() => {
                    tracing::debug!("idle TTL 定时器被取消");
                }
                _ = tokio::time::sleep(ttl) => {
                    if in_flight.load(Ordering::SeqCst) > 0 {
                        tracing::debug!("idle TTL 到期但有在途请求，跳过停止");
                        return;
                    }
                    // 二次验证 generation + instance token
                    let current_state = lifecycle_tx.borrow().clone();
                    let (current_gen, current_token) = match current_state {
                        LifecycleState::Ready { generation, instance_token } => (generation, instance_token),
                        _ => {
                            tracing::debug!(current = ?current_state, "idle TTL: 非 Ready，跳过");
                            return;
                        }
                    };
                    if current_gen != target_gen || current_token != target_token {
                        tracing::debug!(
                            timer_gen = target_gen, current_gen,
                            "idle TTL 到期但 generation/instance 已变化，跳过停止"
                        );
                        return;
                    }
                    lifecycle_tx.send(LifecycleState::Stopping { generation: target_gen }).ok();
                    tracing::info!(ttl_s = ttl.as_secs(), generation = target_gen, "idle TTL 到期，停止 ONNX executor");
                    // 0.22.8-D: 调用 executor.shutdown()
                    if let Some(ref executor) = executor {
                        executor.shutdown().await;
                    }
                    // 0.22.9：executor 回收 → 引擎卡片同步为 Stopped
                    super::sync_engine_card(engine_service, engine_id, false);
                    let current = lifecycle_tx.borrow().clone();
                    match &current {
                        LifecycleState::Stopping { generation } if *generation == target_gen => {
                            *start_elapsed_ms.lock().unwrap() = None;
                            lifecycle_tx.send(LifecycleState::Idle { generation: target_gen + 1 }).ok();
                            tracing::debug!("start_state 已重置为 Idle（idle stop 后）");
                        }
                        _ => {
                            tracing::debug!(
                                current = ?current,
                                "idle TTL: lifecycle 已变化，不提交 Idle"
                            );
                        }
                    }
                }
            }
        });
    }
}
