//! imekit TSF Composition 文本注入实现（0.10.5）。
//!
//! 通过 [imekit](https://crates.io/crates/imekit) crate 封装的 TSF Composition + IMM32 + SendInput
//! 三级回退链注入文本。详见 `docs/production-design/phases/0.10.5-tsf-composition.md`。
//!
//! ## 线程模型
//!
//! `ITfThreadMgr` 是 `!Send`/`!Sync`（COM STA 线程亲和），imekit 的 `InputMethod` 内部持有它。
//! 因此创建一个长期存活的专用 STA 线程，通过 mpsc channel 通信：
//!
//! ```text
//! 调用方（spawn_blocking）
//!   → ImekitInjector::commit_string(text)
//!       → mpsc::send(Commit { text, resp })
//!       → resp recv_timeout（超时 3s）
//!
//! STA 线程（blink-tsf-sta）
//!   → imekit::InputMethod::new()（CoInit STA + ITfThreadMgr::Activate）
//!   → loop { recv → im.commit_string(text) → resp.send(result) }
//! ```

use std::sync::{mpsc, OnceLock};
use std::thread;
use std::time::Duration;

use super::InjectError;

// ── STA 线程指令 ──────────────────────────────────────────────────────────

/// 发给 STA 线程的指令。
enum InjectCommand {
    /// 一次性注入文本（Phase 1）。
    Commit {
        text: String,
        resp: mpsc::Sender<Result<(), String>>,
    },
    /// 关闭 STA 线程（Blink 退出时调用）。
    Shutdown,
}

// ── 全局单例 ──────────────────────────────────────────────────────────────

/// 全局 ImekitInjector 单例。
///
/// 首次 `commit_string()` 调用时懒初始化（创建 STA 线程）。
/// `shutdown()` 时仅发送 Shutdown 命令，不 join（避免阻塞退出）。
static INJECTOR: OnceLock<ImekitInjector> = OnceLock::new();

/// imekit TSF 注入器。
///
/// 封装专用 STA 线程 + imekit InputMethod 实例。
/// 所有方法阻塞等待 STA 线程执行完毕（同步语义，调用方在 spawn_blocking 中）。
pub struct ImekitInjector {
    /// STA 线程的指令发送端。
    tx: mpsc::Sender<InjectCommand>,
}

impl ImekitInjector {
    /// 获取全局单例（首次调用时创建 STA 线程）。
    fn get_or_init() -> &'static ImekitInjector {
        INJECTOR.get_or_init(|| {
            let (tx, rx) = mpsc::channel::<InjectCommand>();

            thread::Builder::new()
                .name("blink-tsf-sta".into())
                .spawn(move || sta_thread_main(rx))
                .expect("failed to spawn TSF STA thread");

            ImekitInjector { tx }
        })
    }

    /// 一次性注入文本（Phase 1）。
    ///
    /// 走 imekit `commit_string`（TSF → IMM32 → SendInput 三级回退）。
    /// 调用方在 `spawn_blocking` 中，阻塞等待结果。
    /// 超时 3s 保护：STA 线程卡死时不阻塞语音管线，回退 SendInput/Clipboard。
    pub fn commit_string(text: &str) -> Result<(), InjectError> {
        let injector = Self::get_or_init();
        let (resp_tx, resp_rx) = mpsc::channel();

        injector
            .tx
            .send(InjectCommand::Commit {
                text: text.to_string(),
                resp: resp_tx,
            })
            .map_err(|_| InjectError::Tsf("TSF STA 线程已关闭".into()))?;

        match resp_rx.recv_timeout(Duration::from_secs(3)) {
            Ok(Ok(())) => Ok(()),
            Ok(Err(msg)) => {
                tracing::debug!(%msg, "imekit commit_string 返回错误");
                Err(InjectError::Tsf(msg))
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                tracing::warn!("TSF 注入超时（3s），回退");
                Err(InjectError::Tsf("TSF 注入超时（3s）".into()))
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                tracing::warn!("TSF STA 线程已断开，回退");
                Err(InjectError::Tsf("TSF STA 线程已断开".into()))
            }
        }
    }

    /// 关闭 STA 线程（Blink 退出时调用）。
    ///
    /// 仅发送 Shutdown 命令，不 join——避免阻塞 app 退出。
    /// 如果 STA 线程尚未创建（用户从未用过 TSF 注入），则无操作。
    pub fn shutdown() {
        if let Some(injector) = INJECTOR.get() {
            let _ = injector.tx.send(InjectCommand::Shutdown);
            tracing::debug!("TSF STA 线程已发送 Shutdown 命令");
        }
    }
}

// ── STA 线程主函数 ────────────────────────────────────────────────────────

/// STA 线程主函数。
///
/// 线程启动时初始化 COM STA + imekit `InputMethod`，
/// 然后循环接收 channel 指令并执行。
fn sta_thread_main(rx: mpsc::Receiver<InjectCommand>) {
    // imekit::InputMethod::new() 内部调用 CoInitializeEx(STA) + CoCreateInstance(CLSID_TF_ThreadMgr)
    // + ITfThreadMgr::Activate()。
    let im = match imekit::InputMethod::new() {
        Ok(im) => {
            tracing::info!("TSF STA 线程就绪（imekit InputMethod 已初始化）");
            im
        }
        Err(e) => {
            // 初始化失败仍保持线程存活，对所有请求返回错误（由外层回退 SendInput/Clipboard）。
            tracing::warn!(%e, "imekit InputMethod 初始化失败，TSF 注入将不可用（回退 SendInput）");
            sta_thread_failed(rx, e);
            return;
        }
    };

    while let Ok(cmd) = rx.recv() {
        match cmd {
            InjectCommand::Commit { text, resp } => {
                // catch_unwind：imekit 内部 bug 不应杀死 STA 线程
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    im.commit_string(&text)
                }));
                let resp_result = match result {
                    Ok(Ok(())) => Ok(()),
                    Ok(Err(e)) => {
                        tracing::debug!(%e, len = text.chars().count(), "imekit commit_string 失败");
                        Err(format!("{e}"))
                    }
                    Err(_) => {
                        tracing::error!(len = text.chars().count(), "imekit commit_string panic");
                        Err("imekit commit_string panic".to_string())
                    }
                };
                let _ = resp.send(resp_result);
            }
            InjectCommand::Shutdown => break,
        }
    }

    tracing::debug!("TSF STA 线程退出");
}

/// imekit 初始化失败后的 STA 线程占位循环。
///
/// 保持线程存活以响应 channel 请求（全部返回错误），
/// 直到收到 Shutdown 命令。
fn sta_thread_failed(rx: mpsc::Receiver<InjectCommand>, init_error: imekit::Error) {
    let err_msg = format!("{init_error}");

    while let Ok(cmd) = rx.recv() {
        match cmd {
            InjectCommand::Commit { resp, .. } => {
                let _ = resp.send(Err(format!("imekit 未初始化: {err_msg}")));
            }
            InjectCommand::Shutdown => break,
        }
    }

    tracing::debug!("TSF STA 线程退出（初始化失败模式）");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inject_command_channel_roundtrip() {
        let (tx, rx) = mpsc::channel::<InjectCommand>();
        let (resp_tx, resp_rx) = mpsc::channel();

        tx.send(InjectCommand::Commit {
            text: "hello".into(),
            resp: resp_tx,
        })
        .unwrap();

        match rx.recv().unwrap() {
            InjectCommand::Commit { text, resp } => {
                assert_eq!(text, "hello");
                let _ = resp.send(Ok(()));
            }
            _ => panic!("expected Commit"),
        }

        assert!(resp_rx.recv().unwrap().is_ok());
    }

    #[test]
    fn shutdown_command_terminates_thread() {
        let (tx, rx) = mpsc::channel::<InjectCommand>();

        let handle = thread::spawn(move || {
            // 模拟 STA 线程（不做 imekit 初始化，仅测 Shutdown 语义）
            while let Ok(cmd) = rx.recv() {
                match cmd {
                    InjectCommand::Commit { resp, .. } => {
                        let _ = resp.send(Err("not initialized".into()));
                    }
                    InjectCommand::Shutdown => break,
                }
            }
        });

        tx.send(InjectCommand::Shutdown).unwrap();
        handle.join().unwrap();
    }

    #[test]
    fn mpsc_timeout_returns_error() {
        let (_tx, rx) = mpsc::channel::<Result<(), String>>();

        let result = rx.recv_timeout(Duration::from_millis(10));
        assert!(result.is_err());
    }
}
