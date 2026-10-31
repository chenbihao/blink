//! 编辑器媒体任务身份与终态。无任意命令执行入口，重型任务不排队。
use blink_infra::media::{MediaError, TaskLease, acquire_task_lease};
use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
};
use tokio_util::sync::CancellationToken;

#[derive(Clone, Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JobSnapshot {
    pub job_id: String,
    pub session_id: String,
    pub revision: u64,
    pub format: String,
    pub runtime_build: Option<String>,
    pub sequence: u64,
    pub stage: String,
    pub frames_done: u32,
    pub frames_total: u32,
    pub stats: Option<serde_json::Value>,
    pub error: Option<String>,
    pub error_code: Option<String>,
}
impl JobSnapshot {
    fn terminal(&self) -> bool {
        matches!(self.stage.as_str(), "done" | "cancelled" | "failed")
    }
}
struct Active {
    snapshot: JobSnapshot,
    cancel: CancellationToken,
    gif_cancel: Arc<AtomicBool>,
}
#[derive(Default)]
struct State {
    active: Option<Active>,
    history: VecDeque<JobSnapshot>,
}
#[derive(Default)]
pub struct MediaService {
    state: Mutex<State>,
}
static SERVICE: OnceLock<Arc<MediaService>> = OnceLock::new();
pub fn shared() -> Arc<MediaService> {
    SERVICE
        .get_or_init(|| Arc::new(MediaService::default()))
        .clone()
}
pub fn cancel_all() {
    if let Some(service) = SERVICE.get() {
        service.cancel(None, None);
    }
}
pub async fn shutdown() {
    cancel_all();
    if let Some(service) = SERVICE.get() {
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if service
                    .state
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .active
                    .is_none()
                {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
        })
        .await;
    }
}
type Notify = Arc<dyn Fn(JobSnapshot) + Send + Sync>;

impl MediaService {
    /// 文件锁只在阻塞线程申请；调用方随 Job 持有至输入/进程/验证全部结束。
    pub fn reserve(
        self: &Arc<Self>,
        session_id: String,
        revision: u64,
        format: &str,
        runtime_build: Option<String>,
        notify: Notify,
    ) -> Result<Job, MediaError> {
        let lease = acquire_task_lease()?;
        self.reserve_with_lease(session_id, revision, format, runtime_build, notify, lease)
    }
    fn reserve_with_lease(
        self: &Arc<Self>,
        session_id: String,
        revision: u64,
        format: &str,
        runtime_build: Option<String>,
        notify: Notify,
        lease: TaskLease,
    ) -> Result<Job, MediaError> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.active.is_some() {
            return Err(MediaError::Busy);
        }
        let job_id = format!(
            "media-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        );
        let snapshot = JobSnapshot {
            job_id: job_id.clone(),
            session_id,
            revision,
            format: format.into(),
            runtime_build,
            sequence: 1,
            stage: "preparing".into(),
            frames_done: 0,
            frames_total: 0,
            stats: None,
            error: None,
            error_code: None,
        };
        let cancel = CancellationToken::new();
        let gif_cancel = Arc::new(AtomicBool::new(false));
        state.active = Some(Active {
            snapshot: snapshot.clone(),
            cancel: cancel.clone(),
            gif_cancel: gif_cancel.clone(),
        });
        drop(state);
        tracing::info!(%job_id, format, revision, "媒体任务已创建");
        notify(snapshot);
        Ok(Job {
            service: self.clone(),
            job_id,
            cancel,
            gif_cancel,
            lease: Some(lease),
            notify,
            finished: false,
        })
    }
    pub fn snapshot(&self, session: Option<&str>, job_id: Option<&str>) -> Option<JobSnapshot> {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let matches = |s: &&JobSnapshot| {
            session.is_none_or(|id| s.session_id == id) && job_id.is_none_or(|id| s.job_id == id)
        };
        state
            .active
            .as_ref()
            .map(|a| &a.snapshot)
            .filter(matches)
            .or_else(|| state.history.iter().rev().find(matches))
            .cloned()
    }
    pub fn cancel(&self, session: Option<&str>, job_id: Option<&str>) {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(active) = &state.active {
            if session.is_none_or(|id| active.snapshot.session_id == id)
                && job_id.is_none_or(|id| active.snapshot.job_id == id)
            {
                active.cancel.cancel();
                active.gif_cancel.store(true, Ordering::Relaxed);
            }
        }
    }
    fn update(&self, job_id: &str, update: impl FnOnce(&mut JobSnapshot)) -> Option<JobSnapshot> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let active = state
            .active
            .as_mut()
            .filter(|a| a.snapshot.job_id == job_id)?;
        if active.snapshot.terminal() {
            return None;
        }
        update(&mut active.snapshot);
        active.snapshot.sequence += 1;
        Some(active.snapshot.clone())
    }
}

pub struct Job {
    service: Arc<MediaService>,
    pub job_id: String,
    pub cancel: CancellationToken,
    pub gif_cancel: Arc<AtomicBool>,
    lease: Option<TaskLease>,
    notify: Notify,
    finished: bool,
}
impl Job {
    pub fn lease(&self) -> &TaskLease {
        self.lease.as_ref().expect("active job lease")
    }
    pub fn progress_callback(&self) -> blink_infra::media::ProgressCallback {
        let service = self.service.clone();
        let id = self.job_id.clone();
        let notify = self.notify.clone();
        Arc::new(move |p| {
            if let Some(snapshot) = service.update(&id, |s| {
                s.stage = p.stage.into();
                s.frames_done = p.frames_done;
                s.frames_total = p.frames_total;
            }) {
                notify(snapshot);
            }
        })
    }
    pub fn finish(mut self, result: Result<serde_json::Value, (String, String)>) {
        self.complete(result);
    }
    fn complete(&mut self, result: Result<serde_json::Value, (String, String)>) {
        if self.finished {
            return;
        }
        // 已提交成品的成功优先于迟到取消。释放跨进程限流后再允许下一个任务。
        self.lease.take();
        let mut state = self.service.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(mut active) = state.active.take() {
            if active.snapshot.job_id != self.job_id {
                state.active = Some(active);
                self.finished = true;
                return;
            }
            active.snapshot.sequence += 1;
            match result {
                Ok(stats) => {
                    active.snapshot.stage = "done".into();
                    active.snapshot.frames_done = active.snapshot.frames_total;
                    active.snapshot.stats = Some(stats);
                }
                Err((code, error)) => {
                    active.snapshot.stage = if code == "media_cancelled" {
                        "cancelled"
                    } else {
                        "failed"
                    }
                    .into();
                    active.snapshot.error_code = Some(code);
                    active.snapshot.error = Some(error);
                }
            }
            let snapshot = active.snapshot;
            state.history.push_back(snapshot.clone());
            while state.history.len() > 16 {
                state.history.pop_front();
            }
            drop(state);
            tracing::info!(job_id = %self.job_id, stage = %snapshot.stage, "媒体任务结束");
            (self.notify)(snapshot);
        }
        self.finished = true;
    }
}
impl Drop for Job {
    fn drop(&mut self) {
        if !self.finished {
            self.cancel.cancel();
            self.gif_cancel.store(true, Ordering::Relaxed);
            self.complete(Err((
                "media_interrupted".into(),
                "导出任务中断，请重试".into(),
            )));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn job_identity_terminal_history_cancellation_and_drop() {
        let service = Arc::new(MediaService::default());
        let job = service
            .reserve(
                "one".into(),
                7,
                "mp4",
                Some("build-1".into()),
                Arc::new(|_| {}),
            )
            .unwrap();
        let first = job.job_id.clone();
        let callback = job.progress_callback();
        assert!(matches!(
            service.reserve("two".into(), 8, "gif", None, Arc::new(|_| {})),
            Err(MediaError::Busy)
        ));
        callback(blink_infra::media::Progress {
            stage: "encoding",
            frames_done: 2,
            frames_total: 3,
        });
        assert_eq!(service.snapshot(None, Some(&first)).unwrap().revision, 7);
        service.cancel(None, Some(&first));
        assert!(job.cancel.is_cancelled());
        job.finish(Ok(serde_json::json!({"bytes":3})));
        assert_eq!(
            service.snapshot(None, Some(&first)).unwrap().stage,
            "done",
            "迟到取消不能抹掉成功"
        );
        let next = service
            .reserve("two".into(), 8, "gif", None, Arc::new(|_| {}))
            .unwrap();
        let second = next.job_id.clone();
        service.cancel(None, Some(&first));
        assert!(!next.cancel.is_cancelled());
        callback(blink_infra::media::Progress {
            stage: "encoding",
            frames_done: 3,
            frames_total: 3,
        });
        assert_eq!(
            service.snapshot(None, Some(&second)).unwrap().stage,
            "preparing"
        );
        assert_eq!(service.snapshot(None, Some(&first)).unwrap().stage, "done");
        let token = next.cancel.clone();
        drop(next);
        assert!(token.is_cancelled());
        assert_eq!(
            service.snapshot(None, Some(&second)).unwrap().stage,
            "failed"
        );
        for i in 0..20 {
            service
                .reserve("three".into(), i, "gif", None, Arc::new(|_| {}))
                .unwrap()
                .finish(Ok(serde_json::json!({})));
        }
        assert_eq!(service.state.lock().unwrap().history.len(), 16);
        assert!(service.snapshot(None, Some(&first)).is_none());
    }
}
