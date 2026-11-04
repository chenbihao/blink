//! 准备态按源试音：更换一台设备不重启另一条流；状态读取不等待设备打开/join。
use super::{AudioMode, AudioOptions, AudioStatus, Session, SharedAudio, shared};
use std::sync::{
    Mutex,
    atomic::{AtomicU64, Ordering},
};

#[derive(Clone, Default)]
struct Published {
    shared: Option<SharedAudio>,
    error: Option<String>,
}
#[derive(Default)]
struct Control {
    options: AudioOptions,
    sessions: [Option<Session>; 2],
}
#[derive(Default)]
pub struct Monitor {
    control: Mutex<Control>,
    published: Mutex<[Published; 2]>,
    generation: AtomicU64,
}
impl Monitor {
    pub fn configure(&self, options: AudioOptions) -> u64 {
        self.configure_with(options, |options| {
            let mut session = Session::new(shared(options), None);
            session.resume(0)?;
            Ok(session)
        })
    }

    fn configure_with(
        &self,
        options: AudioOptions,
        mut open: impl FnMut(AudioOptions) -> Result<Session, String>,
    ) -> u64 {
        let mut control = self.control.lock().unwrap_or_else(|e| e.into_inner());
        let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        for source in 0..2 {
            let selected = |o: &AudioOptions| {
                if source == 0 {
                    o.microphone.clone()
                } else {
                    o.system.clone()
                }
            };
            let enabled = options.enabled(source);
            let changed = enabled != control.options.enabled(source)
                || (enabled
                    && (selected(&options) != selected(&control.options)
                        || control.sessions[source]
                            .as_ref()
                            .is_none_or(|s| s.fault().is_some())));
            if !changed {
                continue;
            }
            // 仅撤下正在更换的源；另一源的发布状态与采集线程保持不动。
            self.published.lock().unwrap_or_else(|e| e.into_inner())[source] = Published::default();
            drop(control.sessions[source].take());
            if enabled {
                let mut single = options.clone();
                single.mode = if source == 0 {
                    AudioMode::Microphone
                } else {
                    AudioMode::System
                };
                match open(single) {
                    Ok(session) => {
                        self.published.lock().unwrap_or_else(|e| e.into_inner())[source].shared =
                            Some(session.shared.clone());
                        control.sessions[source] = Some(session);
                        tracing::debug!(source, generation, "录制试音来源已独立更新");
                    }
                    Err(error) => {
                        tracing::warn!(source, %error, "录制试音设备打开失败，其他来源继续");
                        self.published.lock().unwrap_or_else(|e| e.into_inner())[source].error =
                            Some(error);
                    }
                }
            }
        }
        control.options = options;
        generation
    }

    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::SeqCst)
    }

    pub fn stop(&self) {
        let mut control = self.control.lock().unwrap_or_else(|e| e.into_inner());
        self.stop_locked(&mut control);
    }

    pub fn stop_if_generation(&self, expected: u64) -> bool {
        let mut control = self.control.lock().unwrap_or_else(|e| e.into_inner());
        if self.generation() != expected {
            return false;
        }
        self.stop_locked(&mut control);
        true
    }

    fn stop_locked(&self, control: &mut Control) {
        self.generation.fetch_add(1, Ordering::SeqCst);
        *self.published.lock().unwrap_or_else(|e| e.into_inner()) = Default::default();
        for session in &mut control.sessions {
            drop(session.take());
        }
        control.options = AudioOptions::default();
    }

    pub fn snapshot(&self) -> AudioStatus {
        // 只复制已发布句柄，不取得可能正等待设备响应的 control 锁。
        let sources = self
            .published
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let mut result = AudioStatus::default();
        let mut errors: [Option<String>; 2] = Default::default();
        for (source, published) in sources.into_iter().enumerate() {
            errors[source] = published.error;
            if let Some(shared) = published.shared {
                let status = shared.lock().unwrap_or_else(|e| e.into_inner()).snapshot();
                result.active |= status.active;
                result.rms[source] = status.rms[source];
                result.peak[source] = status.peak[source];
                result.tracks.extend(status.tracks);
                errors[source] = status.error;
            }
        }
        let messages: Vec<_> = errors.iter().flatten().cloned().collect();
        result.error = (!messages.is_empty()).then(|| messages.join("；"));
        result.source_errors = Some(errors);
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gif::audio::TrackInfo;
    use std::{sync::Arc, time::Instant};

    fn fake(options: AudioOptions) -> Result<Session, String> {
        let source = usize::from(options.mode == AudioMode::System);
        let state = shared(options);
        {
            let mut gate = state.lock().unwrap();
            gate.status.active = true;
            gate.status.tracks.push(TrackInfo {
                source,
                ..Default::default()
            });
            gate.status.rms[source] = 0.25;
            gate.status.peak[source] = 0.5;
            gate.meter_updated[source] = Some(Instant::now());
        }
        Ok(Session::new(state, None))
    }

    #[test]
    fn changing_one_device_preserves_the_other_stream_and_meter_even_while_opening() {
        let monitor = Monitor::default();
        let mut options = AudioOptions {
            mode: AudioMode::Both,
            ..Default::default()
        };
        let first = monitor.configure_with(options.clone(), fake);
        let unchanged = monitor.published.lock().unwrap()[1].shared.clone().unwrap();
        options.microphone = Some("replacement".into());
        let mut opens = 0;
        let second = monitor.configure_with(options, |options| {
            opens += 1;
            assert_eq!(options.mode, AudioMode::Microphone);
            // configure 持锁时仍能读取另一源；无需等待本次设备初始化完成。
            assert_eq!(monitor.snapshot().rms[1], 0.25);
            fake(options)
        });
        assert_eq!(opens, 1);
        assert!(Arc::ptr_eq(
            &unchanged,
            monitor.published.lock().unwrap()[1]
                .shared
                .as_ref()
                .unwrap()
        ));
        assert!(
            !monitor.stop_if_generation(first),
            "旧窗口巡检不能关闭新配置"
        );
        assert!(monitor.stop_if_generation(second));
        assert!(!monitor.snapshot().active);
    }

    #[test]
    fn one_device_failure_does_not_stop_the_other_source_and_can_retry() {
        let monitor = Monitor::default();
        let options = AudioOptions {
            mode: AudioMode::Both,
            ..Default::default()
        };
        monitor.configure_with(options.clone(), |options| {
            if options.mode == AudioMode::Microphone {
                Err("mic missing".into())
            } else {
                fake(options)
            }
        });
        let status = monitor.snapshot();
        assert!(status.active);
        assert_eq!(status.rms, [0.0, 0.25]);
        assert_eq!(
            status.source_errors.unwrap(),
            [Some("mic missing".into()), None]
        );
        let mut opens = 0;
        monitor.configure_with(options, |options| {
            opens += 1;
            fake(options)
        });
        assert_eq!(opens, 1);
        assert!(monitor.snapshot().error.is_none());
    }
}
