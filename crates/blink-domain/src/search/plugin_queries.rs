//! 输入搜索按插件串行：保留一个在途请求与一个最新待执行请求。
//! 不取消尚在插件进程里执行的查询，避免提前释放并发许可造成实际工作堆积。

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

type QueryJob = Pin<Box<dyn Future<Output = ()> + Send>>;

#[derive(Default)]
pub(super) struct PluginQueries {
    /// key 存在表示已有执行者；value 只保留最新待执行任务，空闲立即移除。
    pending: Mutex<HashMap<String, Option<QueryJob>>>,
}

impl PluginQueries {
    pub fn submit(
        self: &Arc<Self>,
        plugin_id: String,
        seq: u64,
        latest_seq: &AtomicU64,
        job: impl Future<Output = ()> + Send + 'static,
    ) {
        let mut pending = self.pending.lock().unwrap();
        // 陈旧 search 可能在 await 后才到这里，不能覆盖较新输入的待执行任务。
        if latest_seq.load(Ordering::SeqCst) != seq {
            return;
        }
        let job: QueryJob = Box::pin(job);
        if let Some(next) = pending.get_mut(&plugin_id) {
            *next = Some(job);
            tracing::trace!(plugin = %plugin_id, seq, "插件搜索合并为最新待执行输入");
            return;
        }
        pending.insert(plugin_id.clone(), None);
        drop(pending);
        let queries = Arc::clone(self);
        tokio::spawn(async move {
            let mut job = job;
            loop {
                if let Err(error) = tokio::spawn(job).await {
                    tracing::warn!(plugin = %plugin_id, %error, "插件搜索任务异常结束，继续处理最新输入");
                }
                let mut pending = queries.pending.lock().unwrap();
                match pending.get_mut(&plugin_id).and_then(Option::take) {
                    Some(next) => job = next,
                    None => {
                        pending.remove(&plugin_id);
                        break;
                    }
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::{mpsc, oneshot};
    use std::time::Duration;

    #[tokio::test]
    async fn slow_query_keeps_only_latest_input_and_plugins_run_independently() {
        let queries = Arc::new(PluginQueries::default());
        let seq = AtomicU64::new(1);
        let (events, mut received) = mpsc::unbounded_channel();
        let (release, wait) = oneshot::channel();
        let slots = Arc::new(tokio::sync::Semaphore::new(1));
        let active_slots = slots.clone();
        let started = events.clone();
        queries.submit("dictionary".into(), 1, &seq, async move {
            let _permit = active_slots.try_acquire_owned().unwrap();
            started.send(1).unwrap();
            wait.await.unwrap();
        });
        assert_eq!(tokio::time::timeout(Duration::from_secs(2), received.recv()).await.unwrap(), Some(1));
        seq.store(2, Ordering::SeqCst);
        let skipped = events.clone();
        queries.submit("dictionary".into(), 2, &seq, async move { skipped.send(2).unwrap(); });
        seq.store(3, Ordering::SeqCst);
        let latest = events.clone();
        queries.submit("dictionary".into(), 3, &seq, async move {
            let _permit = slots.try_acquire_owned().expect("最新输入应等待旧查询释放许可，而非 Busy");
            latest.send(3).unwrap();
        });
        // 迟到的旧 search 不得反过来替换最新任务。
        queries.submit("dictionary".into(), 2, &seq, async { panic!("stale job ran"); });
        queries.submit("weather".into(), 3, &seq, async move { events.send(4).unwrap(); });
        assert_eq!(tokio::time::timeout(Duration::from_secs(2), received.recv()).await.unwrap(), Some(4));
        release.send(()).unwrap();
        assert_eq!(tokio::time::timeout(Duration::from_secs(2), received.recv()).await.unwrap(), Some(3));
        assert!(received.try_recv().is_err());
        tokio::time::timeout(Duration::from_secs(2), async {
            while !queries.pending.lock().unwrap().is_empty() { tokio::task::yield_now().await; }
        }).await.unwrap();
    }
}
