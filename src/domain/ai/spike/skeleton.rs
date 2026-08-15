//! 抗延迟骨架的实测断言。
//!
//! **一律用 mock server（127.0.0.1:0），不打真实供应商**——spike 门槛验的是我们自己，
//! 供应商延迟不在验收范围（详见 §4.4）。

use std::future::Future;
use std::time::{Duration, Instant};
use tokio::sync::oneshot;

use super::mock_server::MockServer;

/// 验收 1：**硬超时精确 abort**
///
/// mock server 延迟 5s 才回，客户端超时 300ms。
/// 断言：调用应在 300ms 附近（±80ms 抖动）主动返回超时，绝不能等到 5s。
///
/// **为什么关键**：SSE 长连接场景下，若不主动 abort in-flight request，
/// tokio task 会一直挂着,占用 slot;硬超时是「静默 fallback」的兜底基石。
#[tokio::test]
async fn hard_timeout_aborts_precisely_at_configured_ms() {
    // mock server 故意慢 5s
    let server = MockServer::start(Duration::from_secs(5)).await.unwrap();

    let client = reqwest::Client::builder()
        // 客户端硬超时 300ms
        .timeout(Duration::from_millis(300))
        .build()
        .unwrap();

    let start = Instant::now();
    let result = client.get(&server.base_url).send().await;
    let elapsed = start.elapsed();

    assert!(result.is_err(), "预期硬超时错误，实际拿到响应");
    let err = result.unwrap_err();
    assert!(
        err.is_timeout(),
        "预期 reqwest timeout 错误，实际类型: {err:?}"
    );

    // 允许 ±80ms 抖动：CI runner + tokio scheduler 抖动实测很少超 40ms
    assert!(
        elapsed >= Duration::from_millis(280) && elapsed <= Duration::from_millis(380),
        "硬超时时机偏差过大：实际 {elapsed:?}，期望 300ms±80ms"
    );
}

/// 验收 2：**用户中断 100ms 内取消 in-flight**
///
/// 场景：用户按 ESC 或换 query → 后端必须能立即 abort 当前 LLM 调用，
/// 不能等到 timeout。tokio 通过 drop future 实现结构化取消，spike 验证这条通路走通。
///
/// **具体做法**：起一个 sleep 5s 的 mock server，主 task 用 `tokio::select!` 让
/// 「HTTP 请求」和「中断信号」赛跑；50ms 后发中断信号，验证请求 task 在总耗时 <150ms
/// 内退出（含调度抖动）。
#[tokio::test]
async fn user_interrupt_cancels_inflight_within_100ms() {
    let server = MockServer::start(Duration::from_secs(5)).await.unwrap();

    let client = reqwest::Client::builder()
        // 硬超时给 5s，确保这次不是硬超时兜底,是中断真取消了
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();

    let (interrupt_tx, interrupt_rx) = oneshot::channel::<()>();

    let start = Instant::now();

    // 主 task：请求 vs 中断赛跑
    let race_result = tokio::spawn(async move {
        tokio::select! {
            _ = client.get(server.base_url.clone()).send() => "http_finished",
            _ = interrupt_rx => "interrupted",
        }
    });

    // 50ms 后发中断
    tokio::time::sleep(Duration::from_millis(50)).await;
    let _ = interrupt_tx.send(());

    // 主 task 应立即退出（select 分支落定即 drop 另一分支的 future → reqwest 请求被 abort）
    let outcome = race_result.await.unwrap();
    let elapsed = start.elapsed();

    assert_eq!(outcome, "interrupted", "预期赢在中断分支，实际 HTTP 完成了");
    // 50ms（等待）+ 100ms 缓冲（调度 + drop 传播）
    assert!(
        elapsed < Duration::from_millis(150),
        "中断响应过慢：实际 {elapsed:?}，期望 <150ms"
    );
}

/// 验收 3：**tokio task drop 传播** —— 中断验证的补充断言
///
/// 上面 `user_interrupt_cancels_inflight_within_100ms` 用 `select!` 内部取消,
/// 这里验证外部 `AbortHandle::abort()` 也能立即取消 in-flight（用于设置页
/// "换 Provider 时取消所有 in-flight" 场景）。
#[tokio::test]
async fn abort_handle_cancels_inflight_task() {
    let server = MockServer::start(Duration::from_secs(5)).await.unwrap();
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();

    let url = server.base_url.clone();
    let handle = tokio::spawn(async move {
        let _ = client.get(&url).send().await;
        "completed"
    });

    // 让请求真正开始（进入等待）
    tokio::time::sleep(Duration::from_millis(50)).await;

    let start = Instant::now();
    handle.abort();
    let outcome = handle.await;
    let elapsed = start.elapsed();

    assert!(outcome.is_err(), "abort 后 handle 应返回 JoinError");
    assert!(
        outcome.unwrap_err().is_cancelled(),
        "预期 JoinError::is_cancelled == true"
    );
    assert!(
        elapsed < Duration::from_millis(50),
        "abort 响应过慢：实际 {elapsed:?}，期望 <50ms"
    );
}

/// 验收 4：**loading 反闪烁定时器逻辑**（后端时机侧）
///
/// UI 侧的"150ms 不显 loading"其实是"若 150ms 内响应，就不发 loading 事件"。
/// 后端职责是精确计时并在阈值处发信号。
///
/// 使用 tokio 虚拟时间 + oneshot future 精确控制请求完成时机，不依赖真实 HTTP / TCP / 墙钟：
/// - 149ms 完成 → 不应触发 loading
/// - 150ms timer 到且请求仍 pending → 应触发 loading
/// - timer 触发后再完成请求 → task 正常收尾
///
/// **为什么不用真实 HTTP**：CI 调度、线程抢占和 TCP 建连时间可能让"80ms 响应"跨过 150ms，
/// 测试验证的是 runner 调度状态，而不只是 debounce 语义。
#[tokio::test(start_paused = true)]
async fn loading_indicator_respects_150ms_debounce() {
    // ── 场景 A：149ms 完成（阈值前）→ 不触发 loading ──
    {
        let (tx, rx) = oneshot::channel::<()>();
        // 请求 future：等待 oneshot 信号才完成
        let req_future = async move {
            let _ = rx.await;
        };
        // 149ms 后发信号让请求完成
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(149)).await;
            let _ = tx.send(());
        });

        let loading_triggered = run_with_loading_timer_generic(req_future).await;
        assert!(
            !loading_triggered,
            "149ms 完成（阈值 150ms 前）不应触发 loading"
        );
    }

    // ── 场景 B：timer 到 150ms 且请求仍 pending → 触发 loading，之后请求完成 ──
    {
        let (tx, rx) = oneshot::channel::<()>();
        let req_future = async move {
            let _ = rx.await;
        };
        // 300ms 后才发信号（确保 timer 先到）
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            let _ = tx.send(());
        });

        let loading_triggered = run_with_loading_timer_generic(req_future).await;
        assert!(
            loading_triggered,
            "150ms timer 到时请求仍 pending，应触发 loading"
        );
    }

    // ── 场景 C：timer 触发后请求正常收尾 ──
    {
        let (tx, rx) = oneshot::channel::<()>();
        let req_future = async move {
            let _ = rx.await;
        };
        // 200ms 后完成（timer 150ms 先到）
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            let _ = tx.send(());
        });

        // run_with_loading_timer_generic 返回 loading=true，但内部已 await 请求完成
        let loading_triggered = run_with_loading_timer_generic(req_future).await;
        assert!(loading_triggered, "timer 先到应触发 loading");
        // 虚拟时间已推进到 200ms，请求 future 已被 await 完毕
    }
}

/// 后端"loading 反闪烁"参考实现（泛型版，可注入任意 future）：
/// - 起一个 150ms 定时器与请求 future 赛跑
/// - 定时器先到 → 触发 loading 显示（但请求仍被 await 到完成）
/// - 请求先完成 → 不触发 loading
///
/// 返回值：是否触发了 loading。
///
/// **注意**：这段逻辑 0.9.2 上真前端时会挪到 `SearchService::exec_ai_intent` 里,
/// spike 阶段用来验证时机准确。
async fn run_with_loading_timer_generic<F>(request: F) -> bool
where
    F: Future<Output = ()>,
{
    let loading_timer = tokio::time::sleep(Duration::from_millis(150));
    tokio::pin!(loading_timer);
    tokio::pin!(request);

    tokio::select! {
        _ = &mut loading_timer => {
            // 阈值到了，触发 loading —— 但请求还得继续等
            let _ = request.as_mut().await;
            true
        }
        _ = &mut request => {
            // 请求先完成，跳过 loading
            false
        }
    }
}
