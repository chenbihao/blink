//! Two staggered GDI readers overlap native capture latency. There are at most
//! two requested frames; delivery stays ordered and carries the capture clock.
use super::capture::FrameSource;
use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, SyncSender},
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

pub struct Captured {
    pub pixels: Vec<u8>,
    pub at_ms: u64,
}
pub struct ReadAhead {
    requests: [SyncSender<u64>; 2],
    results: Receiver<(u64, Result<Captured, String>)>,
    pending: BTreeMap<u64, Result<Captured, String>>,
    next: u64,
    stop: Arc<AtomicBool>,
    workers: Vec<JoinHandle<()>>,
}
impl ReadAhead {
    pub fn new(
        factory: Arc<dyn Fn() -> Box<dyn FrameSource> + Send + Sync>,
        fps: u32,
        anchor: Instant,
        offset: u64,
    ) -> Result<Self, String> {
        let period = Duration::from_nanos(1_000_000_000 / fps.max(1) as u64);
        let stop = Arc::new(AtomicBool::new(false));
        let (tx, results) = mpsc::sync_channel(2);
        let mut requests = Vec::new();
        let mut workers = Vec::new();
        for lane in 0..2 {
            let (request, rx) = mpsc::sync_channel::<u64>(1);
            requests.push(request);
            let tx = tx.clone();
            let stop = stop.clone();
            let factory = factory.clone();
            let worker = std::thread::Builder::new()
                .name(format!("recording-capture-{lane}"))
                .spawn(move || {
                    let mut source = factory();
                    while !stop.load(Ordering::Acquire) {
                        let sequence = match rx.recv_timeout(Duration::from_millis(5)) {
                            Ok(n) => n,
                            Err(mpsc::RecvTimeoutError::Timeout) => continue,
                            Err(_) => break,
                        };
                        let Some(due) = period
                            .checked_mul(u32::try_from(sequence).unwrap_or(u32::MAX))
                            .and_then(|d| anchor.checked_add(d))
                        else {
                            break;
                        };
                        while !stop.load(Ordering::Acquire) && Instant::now() < due {
                            std::thread::sleep(
                                due.saturating_duration_since(Instant::now())
                                    .min(Duration::from_millis(2)),
                            );
                        }
                        if stop.load(Ordering::Acquire) {
                            break;
                        }
                        let started = Instant::now();
                        let result = source.capture().map(|pixels| Captured {
                            pixels,
                            at_ms: offset.saturating_add(
                                started.saturating_duration_since(anchor).as_millis() as u64,
                            ),
                        });
                        if tx.send((sequence, result)).is_err() {
                            break;
                        }
                    }
                })
                .map_err(|_| "无法启动高帧率采集线程")?;
            workers.push(worker);
        }
        drop(tx);
        let this = Self {
            requests: requests.try_into().map_err(|_| "采集线程配置无效")?,
            results,
            pending: BTreeMap::new(),
            next: 0,
            stop,
            workers,
        };
        this.requests[0].send(0).map_err(|_| "采集线程已结束")?;
        this.requests[1].send(1).map_err(|_| "采集线程已结束")?;
        Ok(this)
    }
    pub fn capture(&mut self) -> Result<Captured, String> {
        let deadline = Instant::now() + Duration::from_secs(2);
        let result = loop {
            if let Some(result) = self.pending.remove(&self.next) {
                break result;
            }
            let timeout = deadline.saturating_duration_since(Instant::now());
            let (sequence, result) = self
                .results
                .recv_timeout(timeout)
                .map_err(|_| "高帧率采集超时或线程已结束")?;
            if sequence == self.next {
                break result;
            }
            if self.pending.insert(sequence, result).is_some() || self.pending.len() > 1 {
                return Err("高帧率采集顺序无效".into());
            }
        };
        let result = result?;
        let sequence = self.next;
        self.next += 1;
        self.requests[(sequence % 2) as usize]
            .send(sequence + 2)
            .map_err(|_| "采集线程已结束")?;
        Ok(result)
    }
}
impl Drop for ReadAhead {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        for worker in self.workers.drain(..) {
            let _ = worker.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ordered_staggered_reads_are_bounded_and_stopped_on_drop() {
        struct Slow;
        impl FrameSource for Slow {
            fn capture(&mut self) -> Result<Vec<u8>, String> {
                std::thread::sleep(Duration::from_millis(20));
                Ok(vec![1; 16])
            }
            fn frame_bytes(&self) -> usize {
                16
            }
        }
        let anchor = Instant::now();
        let mut ahead = ReadAhead::new(Arc::new(|| Box::new(Slow)), 60, anchor, 500).unwrap();
        let mut last = 0;
        for _ in 0..12 {
            let frame = ahead.capture().unwrap();
            assert_eq!(frame.pixels.len(), 16);
            assert!(frame.at_ms >= last);
            last = frame.at_ms;
            assert!(ahead.pending.len() <= 1);
        }
        assert!(last >= 650);
        drop(ahead);
    }
}
