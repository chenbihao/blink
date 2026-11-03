//! GIF's 10ms clock and browser minimum delay require cumulative adaptation.
//! Dense source frames are grouped, and rounding never accumulates per frame.
use super::timeline::{FrameView, PlanFrame};
use blink_infra::media::FrameList;
use std::sync::Mutex;

#[derive(Clone, Copy, Default)]
struct Cursor {
    index: usize,
    elapsed_ms: u64,
    emitted_cs: u64,
}

fn next(frames: &FrameList<FrameView>, total_cs: u64, cursor: &mut Cursor) -> Option<PlanFrame> {
    let chosen = frames.get(cursor.index)?.src_idx;
    while let Some(frame) = frames.get(cursor.index) {
        cursor.index += 1;
        cursor.elapsed_ms = cursor.elapsed_ms.saturating_add(frame.dur_ms as u64);
        let target = (cursor.elapsed_ms / 10).min(total_cs);
        if target >= cursor.emitted_cs + 2 || cursor.index == frames.len() {
            let target = if total_cs.saturating_sub(target) < 2 {
                cursor.index = frames.len();
                total_cs
            } else {
                target
            };
            let delay = target.saturating_sub(cursor.emitted_cs).max(2);
            cursor.emitted_cs = target;
            return Some(PlanFrame {
                src_idx: chosen,
                delay_cs: delay.min(u32::MAX as u64) as u32,
            });
        }
    }
    None
}

pub(super) fn frames(source: &FrameList<FrameView>) -> FrameList<PlanFrame> {
    if source.is_empty() {
        return Vec::new().into();
    }
    let total_cs = (source.iter().map(|f| f.dur_ms as u64).sum::<u64>() / 10).max(2);
    let mut starts = Vec::new();
    let mut cursor = Cursor::default();
    let mut count = 0;
    while cursor.index < source.len() {
        if count % 128 == 0 {
            starts.push(cursor);
        }
        if next(source, total_cs, &mut cursor).is_none() {
            break;
        }
        count += 1;
    }
    let source = source.clone();
    let cache = Mutex::new((usize::MAX, Vec::<PlanFrame>::new()));
    FrameList::generated(count, move |i| {
        let page = i / 128;
        let mut cache = cache.lock().unwrap_or_else(|e| e.into_inner());
        if cache.0 != page {
            let mut cursor = starts[page];
            cache.1.clear();
            for _ in 0..128 {
                match next(&source, total_cs, &mut cursor) {
                    Some(f) => cache.1.push(f),
                    None => break,
                }
            }
            cache.0 = page;
        }
        cache.1[i % 128]
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fractional_and_sixty_fps_clocks_keep_total_duration() {
        for fps in [24, 30, 60] {
            let source = FrameList::generated(fps * 120, move |i| FrameView {
                src_idx: i as u32,
                dur_ms: (((i + 1) * 1000 / fps) - (i * 1000 / fps)) as u32,
                held: false,
            });
            let output = frames(&source);
            assert_eq!(
                output.iter().map(|f| f.delay_cs as u64).sum::<u64>(),
                12_000
            );
            assert!(output.iter().all(|f| f.delay_cs >= 2));
            assert!(output.len() <= source.len());
        }
    }
    #[test]
    fn tiny_tail_and_long_static_frames_are_not_lost() {
        let source: FrameList<_> = vec![
            FrameView {
                src_idx: 0,
                dur_ms: 20,
                held: false,
            },
            FrameView {
                src_idx: 1,
                dur_ms: 10,
                held: false,
            },
        ]
        .into();
        let out = frames(&source);
        assert_eq!(out.len(), 1);
        assert_eq!(out.get(0).unwrap().delay_cs, 3);
        let long: FrameList<_> = vec![FrameView {
            src_idx: 7,
            dur_ms: 1_800_000,
            held: false,
        }]
        .into();
        assert_eq!(frames(&long).get(0).unwrap().delay_cs, 180_000);
    }
}
