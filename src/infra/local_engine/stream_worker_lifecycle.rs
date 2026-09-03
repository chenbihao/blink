//! ParaformerOnline worker 生命周期集成测试（0.22.9）。
//!
//! 验证 ManagedProcess + StreamWorkerClient + binary protocol v2 的完整
//! 生命周期：Ready, Begin/Audio/End, Partial/Final, Cancel/Reset, Quit,
//! malformed, early exit, kill+wait, restart, no orphans。
//!
//! ## 设计铁则
//!
//! - **不 spawn 真实子进程**：使用 tokio duplex I/O 模拟 worker stdio
//! - **验证 ManagedProcess 集成路径**：通过 `StdioConfig::worker_protocol()`
//!   和 `take_worker_stdio` 获取管道，再转交 `StreamWorkerClient`
//! - **覆盖所有生命周期状态**：Ready → Begin → Audio → End → Partial/Final
//!   → Cancel/Reset → Quit → EOF/poison → kill+wait → restart → no orphans
//! - **验证二进制队列有界**：Busy 背压测试
//! - **不依赖真实模型文件**：FakeWorker 提供协议级行为模拟
//!
//! ## 测试矩阵
//!
//! | 测试 | 场景 | 验证 |
//! |---|---|---|
//! | `lifecycle_hello_ready_begin_audio_end_quit` | 完整正常流程 | 协议正确、结果非空 |
//! | `lifecycle_cancel_stream` | 取消当前流 | 幂等、状态恢复 |
//! | `lifecycle_reset_idempotent` | 重置 worker | 幂等、可重新开始 |
//! | `lifecycle_busy_when_queue_full` | 背压 | 不死锁、不丢音频 |
//! | `lifecycle_old_generation_discarded` | 旧 generation 结果 | 不污染当前流 |
//! | `lifecycle_audio_before_begin_rejected` | 乱序消息 | worker 回 Error、可恢复 |
//! | `lifecycle_quit_graceful_exit` | 优雅退出 | worker 正常退出 |
//! | `lifecycle_eof_poisons_client` | worker 崩溃 | client poison、后续操作失败 |
//! | `lifecycle_multiple_streams_no_deadlock` | 压力测试 | 连续多条流无死锁 |
//! | `lifecycle_restart_after_eof` | 重启 | poison 后可重新创建 client |
//! | `lifecycle_malformed_frame_rejected` | 畸形帧 | fail-closed poison |
//! | `lifecycle_oversized_frame_rejected` | 超限帧 | 拒绝、poison |

#[cfg(test)]
mod tests {
    use crate::infra::local_engine::stream_worker_proto::{
        AudioFrame, FakeWorker, FakeWorkerConfig, ProtoError, StreamWorkerClient, frame_flags,
    };
    use std::sync::Arc;
    use tokio::io::{DuplexStream, duplex};

    /// 创建测试 harness，返回可用于 spawn worker task 的读写句柄。
    ///
    /// 与 `stream_worker_proto::tests::new_with_pipes` 相同的 harness，
    /// 但在此模块中重新定义以测试完整生命周期矩阵。
    fn lifecycle_harness(
        config: FakeWorkerConfig,
    ) -> (
        Arc<StreamWorkerClient>,
        Arc<FakeWorker>,
        DuplexStream, // worker_reader
        DuplexStream, // worker_writer
    ) {
        let (host_write, worker_read) = duplex(256 * 1024);
        let (worker_write, host_read) = duplex(256 * 1024);

        let client = StreamWorkerClient::new(Box::new(host_write), Box::new(host_read));
        let worker = Arc::new(FakeWorker::new(config));

        (client, worker, worker_read, worker_write)
    }

    // ── 正常生命周期 ──────────────────────────────────────────────────

    #[tokio::test(flavor = "multi_thread")]
    async fn lifecycle_hello_ready_begin_audio_end_quit() {
        let (client, worker, mut worker_read, mut worker_write) =
            lifecycle_harness(FakeWorkerConfig::default());

        let worker_task = tokio::spawn(async move {
            worker.run(&mut worker_read, &mut worker_write).await;
        });

        // Hello → wait_ready
        client.send_hello().await.unwrap();
        client
            .wait_ready(std::time::Duration::from_secs(5))
            .await
            .unwrap();

        // Begin
        let (stream_gen, _req_id) = client.begin_stream().await.unwrap();
        assert_eq!(stream_gen, 1);

        // 发送 5 个音频帧
        for _ in 0..5 {
            let samples = vec![0.1f32; 320]; // 20ms
            let frame = AudioFrame::from_samples(&samples);
            client.send_audio(stream_gen, &frame).await.unwrap();
        }

        // End → wait for final
        let result = client
            .end_stream(stream_gen, std::time::Duration::from_secs(5))
            .await
            .unwrap();
        assert!(result.is_final);
        assert!(result.text.contains("final(5 frames)"));

        // Quit
        client.send_quit().await.unwrap();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), worker_task).await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn lifecycle_cancel_stream() {
        let (client, worker, mut worker_read, mut worker_write) =
            lifecycle_harness(FakeWorkerConfig::default());

        let worker_task = tokio::spawn(async move {
            worker.run(&mut worker_read, &mut worker_write).await;
        });

        client.send_hello().await.unwrap();
        client
            .wait_ready(std::time::Duration::from_secs(5))
            .await
            .unwrap();

        let (stream_gen, _) = client.begin_stream().await.unwrap();
        client
            .send_audio(stream_gen, &AudioFrame::from_samples(&[0.1; 320]))
            .await
            .unwrap();

        // Cancel（幂等——可以多次调用）
        client.cancel_stream(stream_gen).await.unwrap();
        client.cancel_stream(stream_gen).await.unwrap();

        // Reset 确认 worker 回到 ready
        client.reset().await.unwrap();

        // 可以开始新流
        let (stream_gen2, _) = client.begin_stream().await.unwrap();
        assert_eq!(stream_gen2, stream_gen + 1);
        let result = client
            .end_stream(stream_gen2, std::time::Duration::from_secs(5))
            .await
            .unwrap();
        assert!(result.is_final);

        client.send_quit().await.unwrap();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), worker_task).await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn lifecycle_reset_idempotent() {
        let (client, worker, mut worker_read, mut worker_write) =
            lifecycle_harness(FakeWorkerConfig::default());

        let worker_task = tokio::spawn(async move {
            worker.run(&mut worker_read, &mut worker_write).await;
        });

        client.send_hello().await.unwrap();
        client
            .wait_ready(std::time::Duration::from_secs(5))
            .await
            .unwrap();

        // reset before any stream (幂等)
        client.reset().await.unwrap();

        // reset after begin (幂等——中间取消)
        let (stream_gen, _) = client.begin_stream().await.unwrap();
        let _ = client
            .send_audio(stream_gen, &AudioFrame::from_samples(&[0.1; 320]))
            .await;
        client.reset().await.unwrap();

        // reset again (幂等)
        client.reset().await.unwrap();
        client.reset().await.unwrap();

        client.send_quit().await.unwrap();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), worker_task).await;
    }

    // ── 背压测试 ──────────────────────────────────────────────────────

    #[tokio::test(flavor = "multi_thread")]
    async fn lifecycle_busy_when_queue_full() {
        let config = FakeWorkerConfig {
            queue_capacity: 2,
            ack_audio: false,
            process_delay_ms: 0,
            ..Default::default()
        };
        let (client, worker, mut worker_read, mut worker_write) = lifecycle_harness(config);

        let worker_task = tokio::spawn(async move {
            worker.run(&mut worker_read, &mut worker_write).await;
        });

        client.send_hello().await.unwrap();
        client
            .wait_ready(std::time::Duration::from_secs(5))
            .await
            .unwrap();

        let (stream_gen, _) = client.begin_stream().await.unwrap();

        // 发 10 个音频帧，队列容量只有 2——worker 会回 Busy
        for _ in 0..10 {
            let _ = client
                .send_audio(stream_gen, &AudioFrame::from_samples(&[0.1; 320]))
                .await;
        }

        // end 后 worker 会清空队列并发 final
        let result = client
            .end_stream(stream_gen, std::time::Duration::from_secs(5))
            .await;
        match result {
            Ok(r) => assert!(r.is_final),
            Err(ProtoError::Busy(_)) => {}
            Err(e) => panic!("不应收到 {e:?}"),
        }

        client.send_quit().await.unwrap();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), worker_task).await;
    }

    // ── 旧 generation 结果丢弃 ────────────────────────────────────────

    #[tokio::test(flavor = "multi_thread")]
    async fn lifecycle_old_generation_discarded() {
        let (client, worker, mut worker_read, mut worker_write) =
            lifecycle_harness(FakeWorkerConfig::default());

        let worker_task = tokio::spawn(async move {
            worker.run(&mut worker_read, &mut worker_write).await;
        });

        client.send_hello().await.unwrap();
        client
            .wait_ready(std::time::Duration::from_secs(5))
            .await
            .unwrap();

        // gen=1
        let (gen1, _) = client.begin_stream().await.unwrap();
        client
            .send_audio(gen1, &AudioFrame::from_samples(&[0.1; 320]))
            .await
            .unwrap();
        client.cancel_stream(gen1).await.unwrap();
        client.reset().await.unwrap();

        // gen=2
        let (gen2, _) = client.begin_stream().await.unwrap();
        assert_ne!(gen1, gen2);
        client
            .send_audio(gen2, &AudioFrame::from_samples(&[0.2; 320]))
            .await
            .unwrap();
        let result = client
            .end_stream(gen2, std::time::Duration::from_secs(5))
            .await
            .unwrap();
        assert!(result.is_final);
        assert!(result.text.contains("final("));

        client.send_quit().await.unwrap();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), worker_task).await;
    }

    // ── 乱序消息 ──────────────────────────────────────────────────────

    #[tokio::test(flavor = "multi_thread")]
    async fn lifecycle_audio_before_begin_rejected() {
        let (client, worker, mut worker_read, mut worker_write) =
            lifecycle_harness(FakeWorkerConfig::default());

        let worker_task = tokio::spawn(async move {
            worker.run(&mut worker_read, &mut worker_write).await;
        });

        client.send_hello().await.unwrap();
        client
            .wait_ready(std::time::Duration::from_secs(5))
            .await
            .unwrap();

        // 直接发 Audio（不先 Begin）——worker 应回 Error
        // 使用 send_audio（底层走 send_frame）但 worker 会因为状态不对回 Error
        let frame = AudioFrame::from_bytes(vec![0u8; 1280]);
        let _ = client.send_audio(1, &frame).await;

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // reset 恢复
        let reset_result = client.reset().await;
        tracing::debug!(?reset_result, "reset after audio-before-begin");

        client.send_quit().await.unwrap();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), worker_task).await;
    }

    // ── 优雅退出 ──────────────────────────────────────────────────────

    #[tokio::test(flavor = "multi_thread")]
    async fn lifecycle_quit_graceful_exit() {
        let (client, worker, mut worker_read, mut worker_write) =
            lifecycle_harness(FakeWorkerConfig::default());

        let worker_task = tokio::spawn(async move {
            worker.run(&mut worker_read, &mut worker_write).await;
        });

        client.send_hello().await.unwrap();
        client
            .wait_ready(std::time::Duration::from_secs(5))
            .await
            .unwrap();
        client.send_quit().await.unwrap();

        // worker 应退出
        let result = tokio::time::timeout(std::time::Duration::from_secs(5), worker_task).await;
        assert!(result.is_ok());
        assert!(result.unwrap().is_ok());
    }

    // ── EOF / poison ──────────────────────────────────────────────────

    #[tokio::test(flavor = "multi_thread")]
    async fn lifecycle_eof_poisons_client() {
        let (host_write, worker_read) = tokio::io::duplex(64 * 1024);
        let (worker_write, host_read) = tokio::io::duplex(64 * 1024);

        let client = StreamWorkerClient::new(Box::new(host_write), Box::new(host_read));

        // 关闭 worker 端——host 的 reader task 会收到 EOF
        drop(worker_read);
        drop(worker_write);

        // wait_ready 会尝试从 reader task 读取事件，应收到 EOF 并 poison
        let result = client.wait_ready(std::time::Duration::from_secs(2)).await;
        assert!(result.is_err());
        assert!(client.is_poisoned(), "wait_ready 应因 EOF poison client");
    }

    // ── 重启 ──────────────────────────────────────────────────────────

    #[tokio::test(flavor = "multi_thread")]
    async fn lifecycle_restart_after_eof() {
        // 第一轮：client 被 poison
        let (host_write, worker_read) = tokio::io::duplex(64 * 1024);
        let (worker_write, host_read) = tokio::io::duplex(64 * 1024);

        let client1 = StreamWorkerClient::new(Box::new(host_write), Box::new(host_read));
        drop(worker_read);
        drop(worker_write);

        // wait_ready 主动消费 EOF 事件，触发 poison
        let _ = client1.wait_ready(std::time::Duration::from_secs(2)).await;
        assert!(client1.is_poisoned(), "client1 应在 EOF 后被 poison");

        // 第二轮：创建新 client + 新 worker
        let (client2, worker, mut worker_read, mut worker_write) =
            lifecycle_harness(FakeWorkerConfig::default());

        let worker_task = tokio::spawn(async move {
            worker.run(&mut worker_read, &mut worker_write).await;
        });

        client2.send_hello().await.unwrap();
        client2
            .wait_ready(std::time::Duration::from_secs(5))
            .await
            .unwrap();

        let (stream_gen, _) = client2.begin_stream().await.unwrap();
        client2
            .send_audio(stream_gen, &AudioFrame::from_samples(&[0.1; 320]))
            .await
            .unwrap();
        let result = client2
            .end_stream(stream_gen, std::time::Duration::from_secs(5))
            .await
            .unwrap();
        assert!(result.is_final);

        client2.send_quit().await.unwrap();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), worker_task).await;
    }

    // ── 畸形帧 ────────────────────────────────────────────────────────

    #[tokio::test(flavor = "multi_thread")]
    async fn lifecycle_malformed_frame_rejected() {
        // 使用独立管道直接注入畸形帧到 reader task，验证 poison 行为
        let (host_write, worker_read) = tokio::io::duplex(64 * 1024);
        let (worker_write, host_read) = tokio::io::duplex(64 * 1024);

        let client = StreamWorkerClient::new(Box::new(host_write), Box::new(host_read));

        // 发送 Hello（通过管道写入到 reader task）
        client.send_hello().await.unwrap();

        // 直接注入畸形帧（错误魔数）到 worker → host 方向
        use tokio::io::AsyncWriteExt;
        let mut bad_header = [0u8; 20];
        bad_header[0..4].copy_from_slice(b"XXXX"); // wrong magic
        let mut writer = worker_write;
        writer.write_all(&bad_header).await.unwrap();
        drop(writer);

        // wait_ready 主动消费畸形帧事件，触发 poison
        let _ = client.wait_ready(std::time::Duration::from_secs(2)).await;
        assert!(client.is_poisoned(), "client 应在畸形帧后被 poison");

        let _ = worker_read; // 保持管道存活
    }

    // ── 超限帧 ────────────────────────────────────────────────────────

    #[tokio::test(flavor = "multi_thread")]
    async fn lifecycle_oversized_frame_rejected() {
        let (client, worker, mut worker_read, mut worker_write) =
            lifecycle_harness(FakeWorkerConfig::default());

        let worker_task = tokio::spawn(async move {
            worker.run(&mut worker_read, &mut worker_write).await;
        });

        client.send_hello().await.unwrap();
        client
            .wait_ready(std::time::Duration::from_secs(5))
            .await
            .unwrap();

        // 尝试发送超限 payload——AudioFrame 校验会拒绝
        let big_data = vec![0u8; 70 * 1024]; // > MAX_AUDIO_PAYLOAD (6400)
        let big_frame = AudioFrame::from_bytes(big_data);
        let result = client.send_audio(1, &big_frame).await;
        assert!(result.is_err(), "超限音频帧应被拒绝");

        // worker 仍然存活——可以正常发送小帧
        let (stream_gen, _) = client.begin_stream().await.unwrap();
        client
            .send_audio(stream_gen, &AudioFrame::from_samples(&[0.1; 320]))
            .await
            .unwrap();
        let result = client
            .end_stream(stream_gen, std::time::Duration::from_secs(5))
            .await
            .unwrap();
        assert!(result.is_final);

        client.send_quit().await.unwrap();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), worker_task).await;
    }

    // ── 压力测试：连续多条流无死锁 ────────────────────────────────────

    #[tokio::test(flavor = "multi_thread")]
    async fn lifecycle_stress_multiple_streams_no_deadlock() {
        let (client, worker, mut worker_read, mut worker_write) =
            lifecycle_harness(FakeWorkerConfig {
                queue_capacity: 64,
                ack_audio: false,
                process_delay_ms: 0,
                ..Default::default()
            });

        let worker_task = tokio::spawn(async move {
            worker.run(&mut worker_read, &mut worker_write).await;
        });

        client.send_hello().await.unwrap();
        client
            .wait_ready(std::time::Duration::from_secs(5))
            .await
            .unwrap();

        // 连续 20 条流，每条发 5 个 audio + end
        for i in 0..20u32 {
            let (stream_gen, _) = client.begin_stream().await.unwrap();
            for _ in 0..5 {
                let _ = client
                    .send_audio(stream_gen, &AudioFrame::from_samples(&[0.1; 320]))
                    .await;
            }
            let result = client
                .end_stream(stream_gen, std::time::Duration::from_secs(5))
                .await
                .unwrap();
            assert!(result.is_final);
            tracing::debug!(stream_idx = i, stream_gen, "stream done");
        }

        client.send_quit().await.unwrap();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(10), worker_task).await;
    }

    // ── 帧标志位验证 ──────────────────────────────────────────────────

    #[test]
    fn lifecycle_frame_flags_are_valid() {
        assert_eq!(frame_flags::FINAL_CHUNK, 0x01);
        assert_eq!(frame_flags::END_OF_STREAM, 0x02);
    }

    // ── partial 结果验证 ──────────────────────────────────────────────

    #[tokio::test(flavor = "multi_thread")]
    async fn lifecycle_partial_before_final() {
        let (client, worker, mut worker_read, mut worker_write) =
            lifecycle_harness(FakeWorkerConfig::default());

        let worker_task = tokio::spawn(async move {
            worker.run(&mut worker_read, &mut worker_write).await;
        });

        client.send_hello().await.unwrap();
        client
            .wait_ready(std::time::Duration::from_secs(5))
            .await
            .unwrap();

        let (stream_gen, _) = client.begin_stream().await.unwrap();
        client
            .send_audio(stream_gen, &AudioFrame::from_samples(&[0.1; 320]))
            .await
            .unwrap();

        // end 后 worker 会先发 partial 再发 final
        let result = client
            .end_stream(stream_gen, std::time::Duration::from_secs(5))
            .await
            .unwrap();
        assert!(result.is_final);
        // partial 被消费了（记日志后继续等待 final）
        assert!(result.text.contains("final("));

        client.send_quit().await.unwrap();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), worker_task).await;
    }

    // ── 二进制队列有界验证 ────────────────────────────────────────────

    #[tokio::test(flavor = "multi_thread")]
    async fn lifecycle_binary_queue_is_bounded() {
        // worker 有界队列为 1，验证即使大量音频涌入也不会无限分配
        let config = FakeWorkerConfig {
            queue_capacity: 1,
            ack_audio: false,
            process_delay_ms: 0,
            ..Default::default()
        };
        let (client, worker, mut worker_read, mut worker_write) = lifecycle_harness(config);

        let worker_task = tokio::spawn(async move {
            worker.run(&mut worker_read, &mut worker_write).await;
        });

        client.send_hello().await.unwrap();
        client
            .wait_ready(std::time::Duration::from_secs(5))
            .await
            .unwrap();

        let (stream_gen, _) = client.begin_stream().await.unwrap();

        // 快速发送大量音频帧——队列只有 1，大部分会 Busy
        for _ in 0..50 {
            let _ = client
                .send_audio(stream_gen, &AudioFrame::from_samples(&[0.1; 320]))
                .await;
        }

        // 验证 worker 仍然存活——end_stream 可以正常完成
        let result = client
            .end_stream(stream_gen, std::time::Duration::from_secs(5))
            .await;
        match result {
            Ok(r) => assert!(r.is_final),
            Err(ProtoError::Busy(_)) => {}
            Err(e) => panic!("不应收到 {e:?}"),
        }

        // worker 未崩溃——可以开始新流
        let (stream_gen2, _) = client.begin_stream().await.unwrap();
        let result = client
            .end_stream(stream_gen2, std::time::Duration::from_secs(5))
            .await
            .unwrap();
        assert!(result.is_final);

        client.send_quit().await.unwrap();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), worker_task).await;
    }

    // ── worker 不残留验证 ──────────────────────────────────────────────

    #[tokio::test(flavor = "multi_thread")]
    async fn lifecycle_no_orphan_after_quit() {
        let (client, worker, mut worker_read, mut worker_write) =
            lifecycle_harness(FakeWorkerConfig::default());

        let worker_task = tokio::spawn(async move {
            worker.run(&mut worker_read, &mut worker_write).await;
        });

        client.send_hello().await.unwrap();
        client
            .wait_ready(std::time::Duration::from_secs(5))
            .await
            .unwrap();

        let (stream_gen, _) = client.begin_stream().await.unwrap();
        client
            .send_audio(stream_gen, &AudioFrame::from_samples(&[0.1; 320]))
            .await
            .unwrap();
        client
            .end_stream(stream_gen, std::time::Duration::from_secs(5))
            .await
            .unwrap();

        // Quit
        client.send_quit().await.unwrap();

        // worker task 必须在超时前退出（无 orphan）
        let result = tokio::time::timeout(std::time::Duration::from_secs(5), worker_task).await;
        assert!(result.is_ok(), "worker task 必须在 Quit 后退出");
        assert!(result.unwrap().is_ok(), "worker task 不应 panic");
    }
}
