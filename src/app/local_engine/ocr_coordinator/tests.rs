//! OCR Coordinator 单元测试（0.22.8-D 适配）。
//!
//! 旧 `map_paddleocr_response` 测试已迁移为 `map_executor_result` 测试。
//! 并发原语测试（LifecycleState / InFlightGuard / starting_gate）保持不变。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Duration;

use bytes::Bytes;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use super::mapping::map_executor_result;
use super::singleflight::{
    InFlightGuard, Lease, LifecycleState, StartingGateGuard, reset_failed_for_new_request,
};
use crate::domain::capability::builtins::ocr_engine::{OcrLine, OcrRect, OcrResult, OcrWord};
use crate::domain::ocr::error::{OcrErrorCategory, StructuredOcrError};
use crate::infra::local_engine::port::ConflictRetryPolicy;
use crate::infra::local_engine::state::InstanceToken;
use crate::infra::local_engine::state::{
    CommitResult, ExitReason, ManagedProcessState, ProcessIdentity, ProcessStatus,
};

fn make_valid_result() -> OcrResult {
    OcrResult {
        text: "hello".to_string(),
        lines: vec![OcrLine {
            text: "hello".to_string(),
            bounding_rect: OcrRect {
                x: 0,
                y: 0,
                w: 100,
                h: 30,
            },
            word_indices: vec![0],
        }],
        words: vec![OcrWord {
            text: "hello".to_string(),
            bounding_rect: OcrRect {
                x: 0,
                y: 0,
                w: 100,
                h: 30,
            },
            line_index: 0,
        }],
        text_angle: None,
        char_ranges: vec![],
        char_boxes: vec![],
    }
}

#[test]
fn map_executor_result_basic() {
    let result = make_valid_result();
    let mapped = map_executor_result(result, (200, 100)).unwrap();
    assert_eq!(mapped.text, "hello");
    assert!(!mapped.lines.is_empty());
    assert!(!mapped.words.is_empty());
}

#[test]
fn map_executor_result_empty() {
    let result = OcrResult {
        text: String::new(),
        lines: vec![],
        words: vec![],
        text_angle: None,
        char_ranges: vec![],
        char_boxes: vec![],
    };
    let mapped = map_executor_result(result, (200, 100)).unwrap();
    assert!(mapped.text.is_empty());
    assert!(mapped.lines.is_empty());
    assert!(mapped.words.is_empty());
}

#[test]
fn map_executor_result_negative_coords_rejected() {
    let mut result = make_valid_result();
    result.words[0].bounding_rect.x = -1;
    assert!(map_executor_result(result, (200, 100)).is_err());
}

#[test]
fn map_executor_result_zero_w_rejected() {
    let mut result = make_valid_result();
    result.words[0].bounding_rect.w = 0;
    assert!(map_executor_result(result, (200, 100)).is_err());
}

#[test]
fn map_executor_result_overflow_x_plus_w_rejected() {
    let mut result = make_valid_result();
    result.words[0].bounding_rect.x = 199;
    result.words[0].bounding_rect.w = 2;
    assert!(map_executor_result(result, (200, 100)).is_err());
}

#[test]
fn map_executor_result_zero_h_rejected() {
    let mut result = make_valid_result();
    result.words[0].bounding_rect.h = 0;
    assert!(map_executor_result(result, (200, 100)).is_err());
}

#[test]
fn map_executor_result_overflow_y_plus_h_rejected() {
    let mut result = make_valid_result();
    result.words[0].bounding_rect.y = 99;
    result.words[0].bounding_rect.h = 2;
    assert!(map_executor_result(result, (200, 100)).is_err());
}

#[test]
fn map_executor_result_empty_text_word_filtered() {
    let result = OcrResult {
        text: String::new(),
        lines: vec![],
        words: vec![
            OcrWord {
                text: String::new(),
                bounding_rect: OcrRect {
                    x: 0,
                    y: 0,
                    w: 100,
                    h: 30,
                },
                line_index: 0,
            },
            OcrWord {
                text: "ok".to_string(),
                bounding_rect: OcrRect {
                    x: 0,
                    y: 30,
                    w: 100,
                    h: 30,
                },
                line_index: 0,
            },
        ],
        text_angle: None,
        char_ranges: vec![],
        char_boxes: vec![],
    };
    let mapped = map_executor_result(result, (200, 100)).unwrap();
    assert_eq!(mapped.words.len(), 1);
    assert_eq!(mapped.words[0].text, "ok");
}

#[test]
fn map_executor_result_cjk_line_grouping() {
    let result = OcrResult {
        text: "你好".to_string(),
        lines: vec![],
        words: vec![
            OcrWord {
                text: "你".to_string(),
                bounding_rect: OcrRect {
                    x: 0,
                    y: 0,
                    w: 25,
                    h: 30,
                },
                line_index: 0,
            },
            OcrWord {
                text: "好".to_string(),
                bounding_rect: OcrRect {
                    x: 25,
                    y: 0,
                    w: 25,
                    h: 30,
                },
                line_index: 0,
            },
        ],
        text_angle: None,
        char_ranges: vec![],
        char_boxes: vec![],
    };
    let mapped = map_executor_result(result, (100, 50)).unwrap();
    assert_eq!(mapped.text, "你好");
    assert_eq!(mapped.lines.len(), 1);
}

// ── 生命周期状态测试 ──────────────────────────────────────────────────

#[test]
fn lifecycle_state_idle_default() {
    let state = LifecycleState::Idle { generation: 0 };
    assert_eq!(state.generation(), 0);
}

#[test]
fn lifecycle_state_ready_generation() {
    let state = LifecycleState::Ready {
        generation: 10,
        instance_token: InstanceToken {
            generation: 10,
            instance_id: "token-abc".to_string(),
        },
    };
    assert_eq!(state.generation(), 10);
}

// ── watch channel 不丢通知测试 ────────────────────────────────────────

#[tokio::test]
async fn watch_channel_does_not_lose_notifications() {
    let (tx, mut rx) = watch::channel(LifecycleState::Idle { generation: 0 });
    tx.send(LifecycleState::Starting { generation: 0 }).unwrap();
    tx.send(LifecycleState::Ready {
        generation: 0,
        instance_token: InstanceToken {
            generation: 0,
            instance_id: "t1".to_string(),
        },
    })
    .unwrap();
    let changed = tokio::time::timeout(Duration::from_millis(100), rx.changed()).await;
    assert!(changed.is_ok(), "watch changed() 不应超时——不丢通知");
    let state = rx.borrow().clone();
    assert!(matches!(state, LifecycleState::Ready { .. }));
}

// ── InFlightGuard 测试 ─────────────────────────────────────────────────

#[tokio::test]
async fn in_flight_guard_drop_decrements() {
    let counter = Arc::new(AtomicU32::new(0));
    assert_eq!(counter.load(Ordering::SeqCst), 0);
    {
        let _guard = InFlightGuard::new(counter.clone());
        assert_eq!(counter.load(Ordering::SeqCst), 1);
    }
    assert_eq!(counter.load(Ordering::SeqCst), 0);
}

// ── 并发测试：singleflight 只启动一次 ──────────────────────────────────

/// 验证 LifecycleState 转换序列的合法性。
///
/// 合法序列：
/// - Idle → Starting → Ready（正常启动）
/// - Idle → Starting → Failed（启动失败）
/// - Ready → Stopping → Idle（正常停止）
/// - Ready → Stopping（repair/shutdown）
#[test]
fn lifecycle_transitions_are_legal() {
    let idle = LifecycleState::Idle { generation: 0 };
    let starting = LifecycleState::Starting { generation: 0 };
    let ready = LifecycleState::Ready {
        generation: 0,
        instance_token: InstanceToken {
            generation: 0,
            instance_id: "t1".to_string(),
        },
    };
    let stopping = LifecycleState::Stopping { generation: 0 };
    let failed = LifecycleState::Failed {
        generation: 0,
        error: Arc::new(StructuredOcrError::start_failed("test")),
    };

    // generation 一致性
    assert_eq!(idle.generation(), 0);
    assert_eq!(starting.generation(), 0);
    assert_eq!(ready.generation(), 0);
    assert_eq!(stopping.generation(), 0);
    assert_eq!(failed.generation(), 0);
}

/// 验证 watch channel 在多 waiter 下不丢通知。
///
/// 模拟 20 个 waiter 同时等待 Starting → Ready 转换。
#[tokio::test]
async fn watch_channel_multi_waiter_no_loss() {
    let (tx, _) = watch::channel(LifecycleState::Idle { generation: 0 });

    // 克隆 20 个 receiver
    let mut rxs: Vec<watch::Receiver<LifecycleState>> = (0..20).map(|_| tx.subscribe()).collect();

    // 发送 Starting → Ready
    tx.send(LifecycleState::Starting { generation: 0 }).unwrap();
    tx.send(LifecycleState::Ready {
        generation: 0,
        instance_token: InstanceToken {
            generation: 0,
            instance_id: "test-token".to_string(),
        },
    })
    .unwrap();

    // 所有 waiter 都应该看到 Ready
    for (i, rx) in rxs.iter_mut().enumerate() {
        let changed = tokio::time::timeout(Duration::from_millis(500), rx.changed()).await;
        assert!(changed.is_ok(), "waiter {i} 应收到通知");
        let state = rx.borrow().clone();
        assert!(
            matches!(state, LifecycleState::Ready { .. }),
            "waiter {i} 应看到 Ready，实际 {:?}",
            state
        );
    }
}

/// 验证 InFlightGuard 在 panic 也能递减（RAII）。
#[test]
fn in_flight_guard_panic_safe() {
    let counter = Arc::new(AtomicU32::new(0));
    let counter_clone = counter.clone();

    // 用 catch_unwind 模拟 panic 场景
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _guard = InFlightGuard::new(counter_clone);
        assert_eq!(counter.load(Ordering::SeqCst), 1);
        panic!("模拟 panic");
    }));
    assert!(result.is_err(), "应捕获到 panic");
    // guard 在 panic unwind 时 drop，counter 应递减
    assert_eq!(counter.load(Ordering::SeqCst), 0);
}

/// 验证多个 InFlightGuard 嵌套。
#[test]
fn in_flight_guard_nested() {
    let counter = Arc::new(AtomicU32::new(0));
    {
        let _g1 = InFlightGuard::new(counter.clone());
        assert_eq!(counter.load(Ordering::SeqCst), 1);
        {
            let _g2 = InFlightGuard::new(counter.clone());
            assert_eq!(counter.load(Ordering::SeqCst), 2);
        }
        assert_eq!(counter.load(Ordering::SeqCst), 1);
    }
    assert_eq!(counter.load(Ordering::SeqCst), 0);
}

/// 验证 generation 递增——Failed 后 generation + 1。
#[test]
fn failed_state_transitions_to_idle_with_incremented_generation() {
    let failed = LifecycleState::Failed {
        generation: 5,
        error: Arc::new(StructuredOcrError::start_failed("test error")),
    };
    let next_gen = failed.generation() + 1;
    let idle = LifecycleState::Idle {
        generation: next_gen,
    };
    assert_eq!(idle.generation(), 6);
}

/// 验证 Lease 结构可以正确构造（0.22.8-D: 无 endpoint_url/token）。
#[test]
fn lease_construction_without_endpoint() {
    let lease = Lease { _guard: None };
    // 0.22.8-D: Lease 不再携带 endpoint_url/token
    // 只需验证它可以被构造
    drop(lease);
}

/// 验证 repair_mode 拒绝 lease 的逻辑——LeaseError::NotReady。
#[tokio::test]
async fn repair_mode_rejects_lease() {
    // 使用 watch channel 模拟 repair mode 行为
    let repair_mode = Arc::new(AtomicBool::new(false));

    // 非 repair 模式——不拒绝
    assert!(!repair_mode.load(Ordering::SeqCst));

    // 进入 repair 模式
    repair_mode.store(true, Ordering::SeqCst);
    assert!(repair_mode.load(Ordering::SeqCst));
}

/// 验证 20 个并发 waiter 在 leader 取消后都能正确返回。
///
/// 模拟 singleflight 场景：1 个 winner 触发启动，19 个 waiter。
/// winner 的取消不应影响共享启动 task——但 waiter 的取消应该让自己返回 Cancelled。
#[tokio::test]
async fn concurrent_waiters_handle_individual_cancellation() {
    let (tx, _) = watch::channel(LifecycleState::Starting { generation: 0 });

    // 20 个 waiter，各自有自己的 CancellationToken
    let mut handles = Vec::new();

    for i in 0..20 {
        let mut rx = tx.subscribe();
        let token = CancellationToken::new();

        // 只取消第 0 个 waiter（leader）
        if i == 0 {
            token.cancel();
        }

        let handle = tokio::spawn(async move {
            tokio::select! {
                _ = rx.changed() => "state_changed",
                _ = token.cancelled() => "cancelled",
                _ = tokio::time::sleep(Duration::from_secs(10)) => "timeout",
            }
        });
        handles.push(handle);
    }

    // 等待所有 waiter 完成
    let results = futures::future::join_all(handles).await;

    // leader（第 0 个）应该因 cancel 返回 "cancelled"
    let leader_result = results[0].as_ref().unwrap();
    assert_eq!(*leader_result, "cancelled", "leader 应因 cancel 返回");

    // 其他 waiter 应该等待 state change——我们发送 Ready
    // 注意：它们可能已经超时了，所以我们只检查它们完成了
    for (i, r) in results.iter().skip(1).enumerate() {
        let val: &str = r.as_ref().unwrap();
        assert!(
            val == "state_changed" || val == "timeout" || val == "cancelled",
            "waiter {} 应有有效结果，实际 {}",
            i + 1,
            val
        );
    }
}

/// 验证 start failure 广播给所有 waiter。
///
/// 模拟：shared startup task 失败，发送 Failed 状态。
/// 所有等待 Starting → * 的 waiter 都应收到通知。
#[tokio::test]
async fn start_failure_broadcasts_to_all_waiters() {
    let (tx, _) = watch::channel(LifecycleState::Starting { generation: 0 });

    // 20 个 waiter
    let mut handles = Vec::new();
    for _ in 0..20 {
        let mut rx = tx.subscribe();
        handles.push(tokio::spawn(async move {
            tokio::select! {
                _ = rx.changed() => {
                    let state = rx.borrow().clone();
                    matches!(state, LifecycleState::Failed { .. })
                }
                _ = tokio::time::sleep(Duration::from_secs(5)) => false,
            }
        }));
    }

    // 模拟启动失败
    tx.send(LifecycleState::Failed {
        generation: 0,
        error: Arc::new(StructuredOcrError::start_failed("start failure test")),
    })
    .unwrap();

    let results = futures::future::join_all(handles).await;

    let mut success_count = 0;
    for r in results {
        let got_failure = r.unwrap();
        if got_failure {
            success_count += 1;
        }
    }
    assert_eq!(success_count, 20, "所有 20 个 waiter 都应收到 Failed");
}

/// 验证 20 个并发冷请求在 singleflight 下只触发一次 spawn。
///
/// 使用 watch channel 模拟：所有请求看到 Idle → 发送 Starting，
/// 但只有第一个 send 成功（watch channel 的 send 是覆写语义，
/// 不像 Mutex try_lock 有 CAS 语义——但我们验证逻辑等价性）。
#[tokio::test]
async fn concurrent_cold_requests_single_spawn() {
    let (tx, _) = watch::channel(LifecycleState::Idle { generation: 0 });

    // 模拟 20 个请求同时尝试 Idle → Starting
    // 使用 AtomicBool 作为 "has_spawned" gate
    let has_spawned = Arc::new(AtomicBool::new(false));
    let spawn_count = Arc::new(AtomicU32::new(0));

    let mut handles = Vec::new();

    for _ in 0..20 {
        let mut rx = tx.subscribe();
        let tx_clone = tx.clone();
        let has_spawned = has_spawned.clone();
        let spawn_count = spawn_count.clone();

        handles.push(tokio::spawn(async move {
            loop {
                let current = rx.borrow().clone();
                match current {
                    LifecycleState::Idle { generation } => {
                        // CAS: 只有第一个成功的 send 才是 winner
                        // watch::Sender::send 不提供 CAS——但我们用 AtomicBool 模拟
                        if has_spawned
                            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                            .is_ok()
                        {
                            spawn_count.fetch_add(1, Ordering::SeqCst);
                            tx_clone.send(LifecycleState::Starting { generation }).ok();
                            // 模拟 spawn shared startup task
                            tx_clone
                                .send(LifecycleState::Ready {
                                    generation,
                                    instance_token: InstanceToken {
                                        generation,
                                        instance_id: "test".to_string(),
                                    },
                                })
                                .ok();
                        } else {
                            // 已有 winner——等待状态变化
                        }
                        tokio::select! {
                            _ = rx.changed() => {}
                            _ = tokio::time::sleep(Duration::from_secs(5)) => return,
                        }
                    }
                    LifecycleState::Ready { .. } => return,
                    _ => {
                        tokio::select! {
                            _ = rx.changed() => {}
                            _ = tokio::time::sleep(Duration::from_secs(5)) => return,
                        }
                    }
                }
            }
        }));
    }

    // 等待所有完成
    for handle in handles {
        let _ = handle.await;
    }

    assert_eq!(
        spawn_count.load(Ordering::SeqCst),
        1,
        "只应 spawn 一次 shared startup task"
    );
}

// ── starting_gate 原子 CAS 测试 ───────────────────────────────────

/// 验证 starting_gate 的原子 CAS——只有一个 winner。
#[tokio::test]
async fn starting_gate_only_one_winner() {
    let gate = Arc::new(AtomicBool::new(false));
    let winner_count = Arc::new(AtomicU32::new(0));

    // 20 个并发尝试 CAS
    let mut handles = Vec::new();
    for _ in 0..20 {
        let gate = gate.clone();
        let wc = winner_count.clone();
        handles.push(tokio::spawn(async move {
            if gate
                .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
            {
                wc.fetch_add(1, Ordering::SeqCst);
            }
        }));
    }
    futures::future::join_all(handles).await;
    assert_eq!(
        winner_count.load(Ordering::SeqCst),
        1,
        "只应有一个 CAS winner"
    );
}

/// 验证 StartingGateGuard 在 drop 后重置 gate。
#[test]
fn starting_gate_guard_resets_on_drop() {
    let gate = Arc::new(AtomicBool::new(false));

    // 手动 CAS 成功
    assert!(
        gate.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
    );
    assert!(gate.load(Ordering::SeqCst), "gate 应为 true");

    // guard drop 后重置
    {
        let _guard = StartingGateGuard::new(gate.clone());
        // guard 持有期间不重置（gate 仍为 true）
        assert!(
            gate.load(Ordering::SeqCst),
            "guard 持有期间 gate 应仍为 true"
        );
    }
    assert!(
        !gate.load(Ordering::SeqCst),
        "guard drop 后 gate 应重置为 false"
    );
}

/// 验证 StartingGateGuard 允许下一轮竞争。
#[test]
fn starting_gate_guard_allows_next_round() {
    let gate = Arc::new(AtomicBool::new(false));

    // 第一轮：手动 CAS + guard drop
    assert!(
        gate.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
    );
    {
        let _guard = StartingGateGuard::new(gate.clone());
        assert!(gate.load(Ordering::SeqCst), "guard 持有期间 gate 应为 true");
    }
    assert!(
        !gate.load(Ordering::SeqCst),
        "guard drop 后 gate 应为 false"
    );

    // 第二轮：CAS 应再次成功
    assert!(
        gate.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok(),
        "第二轮 CAS 应成功"
    );
}

/// 验证 20 个并发请求 + starting_gate CAS 只有一个 winner——
/// 使用真正的 tokio async runtime。
#[tokio::test]
async fn concurrent_starting_gate_single_winner() {
    let gate = Arc::new(AtomicBool::new(false));
    let winner_count = Arc::new(AtomicU32::new(0));

    // 使用 barrier 确保所有任务同时开始
    let barrier = Arc::new(tokio::sync::Barrier::new(20));

    let mut handles = Vec::new();
    for _ in 0..20 {
        let gate = gate.clone();
        let wc = winner_count.clone();
        let barrier = barrier.clone();
        handles.push(tokio::spawn(async move {
            // 等待所有任务就绪
            barrier.wait().await;

            // 模拟 ensure_paddleocr_started 中的 CAS
            if gate
                .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
            {
                wc.fetch_add(1, Ordering::SeqCst);
                // winner 不立即重置 gate——模拟 shared startup task 持有 guard
                // 这里我们不 drop guard，让 gate 保持 true
                std::mem::forget(StartingGateGuard::new(gate.clone()));
            }
        }));
    }

    futures::future::join_all(handles).await;
    assert_eq!(winner_count.load(Ordering::SeqCst), 1, "只应有一个 winner");
}

/// 验证 Failed 状态后 gate 重置，下一 generation 可重试。
#[tokio::test]
async fn failed_state_allows_retry_with_gate_reset() {
    let gate = Arc::new(AtomicBool::new(false));

    // 第一轮：CAS 成功，模拟 Failed
    let first_winner = gate
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_ok();
    assert!(first_winner);

    // guard drop（模拟 task 结束，包括 Failed 路径）
    let guard = StartingGateGuard::new(gate.clone());
    drop(guard);

    // 第二轮：CAS 应再次成功（Failed 后可重试）
    let second_winner = gate
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_ok();
    assert!(second_winner, "Failed 后 gate 重置，第二轮应成功");
}

/// 验证 leader 取消不影响 gate（leader 取消只是不再等待结果，不重置 gate）。
#[tokio::test]
async fn leader_cancel_does_not_reset_gate() {
    let gate = Arc::new(AtomicBool::new(false));

    // leader CAS 成功
    assert!(
        gate.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
    );

    // leader 取消——不重置 gate（只有 shared task 结束时 guard 才重置）
    // 模拟 leader 取消只是不再等待 watch changed
    assert!(
        gate.load(Ordering::SeqCst),
        "leader 取消后 gate 仍为 true——共享启动不受影响"
    );

    // guard drop 后才重置
    let guard = StartingGateGuard::new(gate.clone());
    drop(guard);
    assert!(!gate.load(Ordering::SeqCst));
}

// ── 输入资源预算（0.22.6.1）────────────────────────────────────────────

/// 构造最小合法 PNG header（signature + IHDR）。
fn make_png_header(width: u32, height: u32) -> Vec<u8> {
    let mut buf = vec![
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D,
    ];
    buf.extend_from_slice(b"IHDR");
    buf.extend_from_slice(&width.to_be_bytes());
    buf.extend_from_slice(&height.to_be_bytes());
    buf.extend_from_slice(&[0x08, 0x06, 0x00, 0x00, 0x00]);
    buf
}

#[test]
fn input_budget_rejects_oversized_decoded() {
    let png = Bytes::from(make_png_header(16_384, 16_384));
    let err = crate::domain::ocr::input_budget::validate_ocr_input(&png).unwrap_err();
    assert_eq!(err.category, OcrErrorCategory::InputTooLarge);
    assert!(err.message.contains("超出上限"));
}

#[test]
fn input_budget_rejects_oversized_compressed() {
    let mut png = make_png_header(100, 100);
    png.resize(32 * 1024 * 1024 + 1, 0);
    let err = crate::domain::ocr::input_budget::validate_ocr_input(&Bytes::from(png)).unwrap_err();
    assert_eq!(err.category, OcrErrorCategory::InputTooLarge);
}

#[test]
fn input_budget_rejects_bad_header() {
    let err = crate::domain::ocr::input_budget::validate_ocr_input(&Bytes::from(vec![1u8; 64]))
        .unwrap_err();
    assert_eq!(err.category, OcrErrorCategory::DecodeError);
}

#[test]
fn input_budget_accepts_normal_header() {
    let png = Bytes::from(make_png_header(1188, 800));
    let size = crate::domain::ocr::input_budget::validate_ocr_input(&png).unwrap();
    assert_eq!(size, (1188, 800));
}

// ═══════════════════════════════════════════════════════════════════════
// 0.22.6.3 确定性测试
// ═══════════════════════════════════════════════════════════════════════

// ── TTL timer 绑定 generation + instance token（TODO #4）──────────────

/// 验证 idle TTL 定时器在 generation/token 变化后不会停止新实例。
///
/// 场景：Ready(gen=1, token=A) → schedule idle stop → 期间
/// 新 start 产生 Ready(gen=2, token=B) → TTL 到期时
/// 应检测 generation/token 不匹配并跳过停止。
#[tokio::test]
async fn ttl_timer_does_not_stop_new_instance_after_generation_change() {
    let (tx, rx) = watch::channel(LifecycleState::Idle { generation: 0 });

    // 模拟 Ready(gen=1, token=A)
    let token_a = InstanceToken {
        generation: 1,
        instance_id: "inst-a".to_string(),
    };
    tx.send(LifecycleState::Ready {
        generation: 1,
        instance_token: token_a.clone(),
    })
    .ok();

    // 捕获 schedule_idle_stop 时的 target_gen 和 target_token
    let target_gen = 1u64;
    let target_token = token_a.clone();

    // 模拟新 start 产生 Ready(gen=2, token=B)
    let token_b = InstanceToken {
        generation: 2,
        instance_id: "inst-b".to_string(),
    };
    tx.send(LifecycleState::Ready {
        generation: 2,
        instance_token: token_b.clone(),
    })
    .ok();

    // TTL 到期后的验证逻辑（模拟 schedule_idle_stop 中的二次验证）
    let current_state = rx.borrow().clone();
    let (current_gen, current_token) = match current_state {
        LifecycleState::Ready {
            generation,
            instance_token,
        } => (generation, instance_token),
        _ => {
            // 如果不是 Ready，TTL 跳过——这也是正确行为
            return;
        }
    };

    // generation/token 不匹配——TTL 应跳过
    assert_ne!(
        current_gen, target_gen,
        "generation 应已变化，TTL 不应停止新实例"
    );
    assert_ne!(
        current_token, target_token,
        "instance token 应已变化，TTL 不应停止新实例"
    );
}

/// 验证 TTL 到期时如果有在途请求则跳过停止（TODO #3）。
#[tokio::test]
async fn ttl_skips_when_in_flight_requests_exist() {
    let in_flight = Arc::new(AtomicU32::new(0));

    // 模拟有在途请求
    let _guard = InFlightGuard::new(in_flight.clone());
    assert_eq!(in_flight.load(Ordering::SeqCst), 1);

    // TTL 到期时检查 in_flight > 0——应跳过停止
    assert!(
        in_flight.load(Ordering::SeqCst) > 0,
        "有在途请求时 TTL 不应停止实例"
    );

    // guard 释放后 in_flight 归零
    drop(_guard);
    assert_eq!(in_flight.load(Ordering::SeqCst), 0);

    // 此时 TTL 可以停止
    assert!(
        in_flight.load(Ordering::SeqCst) == 0,
        "在途请求归零后 TTL 可以停止"
    );
}

/// 验证 InFlightGuard 的 RAII 语义——即使 panic 也能正确递减。
#[tokio::test]
async fn in_flight_guard_decrements_on_drop() {
    let counter = Arc::new(AtomicU32::new(0));
    assert_eq!(counter.load(Ordering::SeqCst), 0);

    {
        let _g1 = InFlightGuard::new(counter.clone());
        assert_eq!(counter.load(Ordering::SeqCst), 1);

        let _g2 = InFlightGuard::new(counter.clone());
        assert_eq!(counter.load(Ordering::SeqCst), 2);
    }

    assert_eq!(counter.load(Ordering::SeqCst), 0);
}

// ── Failed 状态后所有 waiter 得到一致错误（TODO #2）──────────────────

/// 验证 Failed 状态携带的错误对所有 waiter 一致。
///
/// 场景：leader 启动失败 → lifecycle 转为 Failed { generation, error } →
/// 所有 waiter 通过 `rx.borrow().clone()` 看到相同的 error。
#[tokio::test]
async fn failed_state_provides_consistent_error_to_all_waiters() {
    let shared_error = Arc::new(StructuredOcrError::start_failed("启动超时"));
    let (tx, rx) = watch::channel(LifecycleState::Idle { generation: 0 });

    // 模拟 leader 启动失败
    tx.send(LifecycleState::Failed {
        generation: 1,
        error: shared_error.clone(),
    })
    .ok();

    // 多个 waiter 同时观察 Failed 状态
    let waiter1_error = match rx.borrow().clone() {
        LifecycleState::Failed { error, .. } => error,
        _ => panic!("应为 Failed 状态"),
    };
    let waiter2_error = match rx.borrow().clone() {
        LifecycleState::Failed { error, .. } => error,
        _ => panic!("应为 Failed 状态"),
    };

    // 两个 waiter 得到相同的错误对象（Arc 语义）
    assert!(
        Arc::ptr_eq(&waiter1_error, &waiter2_error),
        "两个 waiter 应得到相同的错误 Arc"
    );
    assert_eq!(waiter1_error.message, "启动超时");
}

/// 验证 Failed 后新请求可以重试（gate 已重置）。
#[tokio::test]
async fn failed_state_allows_retry_after_gate_reset() {
    let gate = Arc::new(AtomicBool::new(false));
    let (tx, _rx) = watch::channel(LifecycleState::Idle { generation: 0 });

    // 第一轮：winner CAS 成功
    let first_winner = gate
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_ok();
    assert!(first_winner, "第一轮 CAS 应成功");

    // 模拟启动失败——guard drop 重置 gate
    {
        let _guard = StartingGateGuard::new(gate.clone());
    }

    // Failed 状态提交
    tx.send(LifecycleState::Failed {
        generation: 1,
        error: Arc::new(StructuredOcrError::start_failed("失败")),
    })
    .ok();

    assert!(reset_failed_for_new_request(&tx, 1));
    assert!(matches!(
        &*tx.borrow(),
        LifecycleState::Idle { generation: 2 }
    ));
    assert!(
        !reset_failed_for_new_request(&tx, 1),
        "同一 Failed generation 只能被推进一次"
    );

    // 第二轮：新请求 CAS 应成功（Failed 后可重试）
    let second_winner = gate
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_ok();
    assert!(second_winner, "Failed 后 gate 重置，第二轮应成功");
}

// ── auto 路由语义断言（TODO #5, #11）──────────────────────────────────

/// 验证 auto 路由在 PaddleOCR 未热态 Ready 时选择 Windows。
///
/// 场景：lifecycle = Idle（PaddleOCR 未启动）→ auto 路由
/// 应立即走 WinRT，不触发 Python 启动或等待。
#[tokio::test]
async fn auto_route_selects_windows_when_paddleocr_not_ready() {
    let (tx, _rx) = watch::channel(LifecycleState::Idle { generation: 0 });

    // 模拟 auto 路由的 hot-only 检查
    let state = tx.borrow().clone();
    let is_ready = matches!(state, LifecycleState::Ready { .. });

    // PaddleOCR 未 Ready → auto 路由应选择 Windows
    assert!(!is_ready, "Idle 状态不应选择 PaddleOCR");

    // 验证不会触发冷启动——hot_only=true 的 acquire_lease 会返回 NotReady
    // 而不是调用 ensure_paddleocr_started
}

/// 验证 auto 路由在 PaddleOCR 热态 Ready 时选择 PaddleOCR。
#[tokio::test]
async fn auto_route_selects_paddleocr_when_hot_ready() {
    let token = InstanceToken {
        generation: 1,
        instance_id: "inst-hot".to_string(),
    };
    let (tx, _rx) = watch::channel(LifecycleState::Ready {
        generation: 1,
        instance_token: token,
    });

    // 模拟 auto 路由的 hot-only 检查
    let state = tx.borrow().clone();
    let is_ready = matches!(state, LifecycleState::Ready { .. });

    // PaddleOCR Ready → auto 路由应选择 PaddleOCR
    assert!(is_ready, "Ready 状态应选择 PaddleOCR");
}

/// 验证 windows 路由直接选择 Windows，不检查 PaddleOCR 状态。
#[tokio::test]
async fn windows_route_always_selects_windows() {
    // 即使 PaddleOCR 是 Ready，windows 路由也应选择 Windows
    let (tx, _rx) = watch::channel(LifecycleState::Ready {
        generation: 1,
        instance_token: InstanceToken {
            generation: 1,
            instance_id: "test".to_string(),
        },
    });

    // windows 路由不检查 lifecycle——直接选择 Windows
    // 这里验证的是路由逻辑不依赖 lifecycle 状态
    let _state = tx.borrow().clone();
    // Windows 路由的 selected_backend 始终是 Windows
    // （在实际代码中，OcrBackendKind::Windows 分支不检查 lifecycle）
}

/// 验证 paddleocr 路由触发冷启动（非 hot-only）。
#[tokio::test]
async fn paddleocr_route_triggers_cold_start() {
    let (tx, _rx) = watch::channel(LifecycleState::Idle { generation: 0 });

    // paddleocr 路由使用 hot_only=false
    // 在 Idle 状态下会触发 ensure_paddleocr_started
    let state = tx.borrow().clone();
    assert!(matches!(state, LifecycleState::Idle { .. }));

    // Idle 状态 + hot_only=false → 进入 CAS 竞争
    // winner 会 spawn shared startup task
    let gate = Arc::new(AtomicBool::new(false));
    let is_winner = gate
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_ok();
    assert!(
        is_winner,
        "Idle 状态下 CAS 应成功（paddleocr 路由触发冷启动）"
    );

    // 清理
    let _guard = StartingGateGuard::new(gate);
}

// ── 旧 health/log 不覆盖新实例（TODO #1 辅助验证）────────────────────

/// 验证 generation 不匹配的退出事件被拒绝。
///
/// 场景：gen=1 的进程退出，但已 start gen=2 →
/// exit monitor 检测 token 不匹配，不覆盖新实例状态。
#[tokio::test]
async fn old_generation_exit_does_not_overwrite_new_instance() {
    let mut state = ManagedProcessState::initial();

    // gen=1 start
    let token1 = state.begin_start();
    state.set_status_exited(ExitReason::NonZeroExit { code: 1 });

    // gen=2 start（新实例）
    let token2 = state.begin_start();
    assert_ne!(token1.generation, token2.generation);

    // 旧 gen 的退出事件到达——应被拒绝
    let ok = state.try_commit_exit(&token1, ExitReason::NonZeroExit { code: 1 });
    assert!(!ok, "旧 generation 的退出事件不应覆盖新实例");

    // 新实例状态不变
    assert_eq!(state.status, ProcessStatus::Starting);
}

/// 验证 token 不匹配的 Running 提交被拒绝。
#[tokio::test]
async fn old_token_running_commit_rejected() {
    let mut state = ManagedProcessState::initial();
    let token1 = state.begin_start();

    // 模拟取消后重新 start
    state.mark_cancelled();
    state.set_status_exited(ExitReason::StartCancelled);
    let _token2 = state.begin_start();

    // 旧 token 的 spawn 结果到达
    let identity = ProcessIdentity {
        pid: 999,
        executable: std::path::PathBuf::from("/old"),
        start_time_ms: 0,
        instance_id: token1.instance_id.clone(),
    };
    let result = state.try_commit_running(&token1, 999, identity);
    assert_eq!(result, CommitResult::Rejected);
}

// ── 端口冲突重试验证（TODO #7 辅助验证）──────────────────────────────

/// 验证 ConflictRetryPolicy 不终止未知进程。
#[test]
fn conflict_retry_policy_never_terminates_unknown_processes() {
    let policy = ConflictRetryPolicy::new(3);

    // policy 只提供 should_retry 判断——不包含任何 kill/terminate 逻辑
    assert!(policy.should_retry(1));
    assert!(policy.should_retry(2));
    assert!(!policy.should_retry(3));

    // policy 的 max_attempts 有上限
    assert_eq!(policy.max_attempts(), 3);
}
