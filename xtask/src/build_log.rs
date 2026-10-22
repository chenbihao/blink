use std::io::IsTerminal;
use std::time::Instant;

struct LocalTime;

impl tracing_subscriber::fmt::time::FormatTime for LocalTime {
    fn format_time(
        &self,
        writer: &mut tracing_subscriber::fmt::format::Writer<'_>,
    ) -> std::fmt::Result {
        write!(
            writer,
            "{}",
            chrono::Local::now().format("%Y-%m-%d %H:%M:%S")
        )
    }
}

pub fn init() {
    tracing_subscriber::fmt()
        .with_timer(LocalTime)
        .with_target(false)
        .with_ansi(std::io::stderr().is_terminal())
        .with_writer(std::io::stderr)
        .init();
}

/// 用单调时钟统计阶段耗时；失败展开时也保留耗时和失败标记。
pub struct BuildTimer {
    title: String,
    started: Instant,
}

impl BuildTimer {
    pub fn start(title: impl Into<String>) -> Self {
        let title = title.into();
        tracing::info!("{title} 开始");
        Self {
            title,
            started: Instant::now(),
        }
    }
}

impl Drop for BuildTimer {
    fn drop(&mut self) {
        let elapsed_secs = self.started.elapsed().as_secs_f64();
        if std::thread::panicking() {
            tracing::error!(
                elapsed_secs = format_args!("{elapsed_secs:.3}"),
                "{} 失败",
                self.title
            );
        } else {
            tracing::info!(
                elapsed_secs = format_args!("{elapsed_secs:.3}"),
                "{} 完成",
                self.title
            );
        }
    }
}
