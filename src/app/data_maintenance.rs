//! 启动与长期运行共用清理策略；串行执行、阻塞 IO 离开主链路。
use super::{editor_draft::EditorDraftStore, gif_service::GifService};
use std::{sync::Arc, time::Duration};

pub fn spawn(gif: Arc<GifService>, drafts: Arc<EditorDraftStore>) {
    tauri::async_runtime::spawn(async move {
        // 首轮延后，避免与快捷键/窗口等启动初始化争用磁盘。
        tokio::time::sleep(Duration::from_secs(30)).await;
        let mut ticks = tokio::time::interval(Duration::from_secs(3600));
        ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            ticks.tick().await;
            drafts.cleanup().await;
            let gif = gif.clone();
            if let Err(error) = tauri::async_runtime::spawn_blocking(move || {
                gif.cleanup_expired();
                crate::infra::utils::logging::cleanup_old_logs(
                    &crate::infra::utils::logging::log_dir(),
                );
            })
            .await
            {
                tracing::warn!(%error, "数据目录定期清理任务失败");
            }
        }
    });
}
