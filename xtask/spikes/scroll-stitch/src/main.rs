//! 长截图拼接算法 spike：合成场景 + 真值评测。
//!
//! 跑法：cargo run --release（在 xtask/spikes/scroll-stitch 下）

mod algo;
mod frame;

use algo::{Estimate, PairCtx, Status};
use frame::{DocStyle, OverlaySpec};

/// 位移估计器统一签名
type Estimator = fn(&frame::Frame, &frame::Frame, &PairCtx) -> Estimate;

const W: usize = 1200;
const VH: usize = 800;
const MAX_PAIRS: usize = 20;

struct Scenario {
    name: &'static str,
    style: DocStyle,
    shift: usize,
    overlays: OverlaySpec,
    noise: u8,
    doc_h: usize,
}

fn scenarios() -> Vec<Scenario> {
    let none = OverlaySpec::default();
    let sticky = OverlaySpec { sticky_header: true, bottom_bar: true, ..none };
    let floats = OverlaySpec { float_button: true, cursor_blink: true, ..none };
    let all = OverlaySpec {
        sticky_header: true,
        bottom_bar: true,
        float_button: true,
        cursor_blink: true,
    };
    let mk = |name: &'static str, style, shift, overlays, doc_h| Scenario {
        name, style, shift, overlays, noise: 2, doc_h,
    };
    vec![
        mk("plain-70(易)", DocStyle::Article, 240, none, 7000),
        mk("sticky-70", DocStyle::Article, 240, sticky, 7000),
        mk("plain-31", DocStyle::Article, 550, none, 13000),
        mk("sticky-31", DocStyle::Article, 550, sticky, 13000),
        mk("float-31", DocStyle::Article, 550, floats, 13000),
        mk("all-31(断崖现场)", DocStyle::Article, 550, all, 13000),
        mk("all-20(超范围)", DocStyle::Article, 640, all, 15000),
        mk("lowdetail-31", DocStyle::LowDetail, 550, none, 13000),
        mk("repeated-31(多义)", DocStyle::RepeatedList(96), 550, none, 13000),
        mk("all-80(小步进)", DocStyle::Article, 80, all, 4000),
    ]
}

#[derive(Default, Clone)]
struct Tally {
    exact: usize,
    near: usize,
    wrong: usize,
    rejected: usize,
    ambiguous: usize,
    sub_err_sum: f64,
    sub_n: usize,
}

impl Tally {
    fn record(&mut self, est: &Estimate, gt: i32) {
        match est.status {
            Status::Matched => {
                let err = (est.shift.round() as i32 - gt).abs();
                match err {
                    0 => self.exact += 1,
                    1 => self.near += 1,
                    _ => self.wrong += 1,
                }
                if err <= 1 {
                    self.sub_err_sum += (est.shift - gt as f64).abs();
                    self.sub_n += 1;
                }
            }
            Status::Unchanged => self.wrong += 1, // gt>0 却报 0
            Status::NoMatch => {
                self.rejected += 1;
                if est.ambiguous {
                    self.ambiguous += 1;
                }
            }
        }
    }

    fn ok(&self) -> usize {
        self.exact + self.near
    }
}

fn run() {
    let algos: Vec<(&str, Estimator)> = vec![
        ("baseline", |p, c, ctx| algo::baseline::estimate(p, c, ctx.same_score)),
        ("baseline-fix", |p, c, ctx| algo::baseline::estimate_fixed(p, c, ctx.same_score)),
        ("robust", |p, c, ctx| algo::robust::estimate(p, c, ctx)),
        ("phase", |p, c, ctx| algo::phase::estimate(p, c, ctx, false)),
        ("phase-mask", |p, c, ctx| algo::phase::estimate(p, c, ctx, true)),
        ("hybrid", |p, c, ctx| algo::hybrid::estimate(p, c, ctx)),
    ];

    let mut summary: Vec<(String, Vec<(String, Tally)>)> = Vec::new();

    for sc in scenarios() {
        let doc = frame::render_doc(&sc.style, W, sc.doc_h, 42);
        let frames =
            frame::make_frames(&doc, W, sc.doc_h, VH, sc.shift, &sc.overlays, sc.noise, MAX_PAIRS);
        let gt = sc.shift as i32;
        let pairs = frames.len() - 1;
        println!("◆ {}  shift={} 对数={}", sc.name, sc.shift, pairs);

        let mut row: Vec<(String, Tally)> = Vec::new();
        for (name, est) in &algos {
            let mut t = Tally::default();
            for w in frames.windows(2) {
                let ctx = PairCtx::new(&w[0], &w[1]);
                let e = est(&w[0], &w[1], &ctx);
                t.record(&e, gt);
            }
            let sub = if t.sub_n > 0 {
                format!("{:.2}", t.sub_err_sum / t.sub_n as f64)
            } else {
                "-".into()
            };
            println!(
                "    {:<11} ✓{:>2} (ex {:>2} near {:>2})  ✗wrong {:>2}  rej {:>2} (amb {})  μ|Δsub| {}",
                name,
                t.ok(),
                t.exact,
                t.near,
                t.wrong,
                t.rejected,
                t.ambiguous,
                sub
            );
            row.push(((*name).to_string(), t));
        }
        summary.push((sc.name.to_string(), row));
    }

    // 汇总：各算法跨场景累计
    println!("\n══ 汇总（累计）══");
    let n_algos = algos.len();
    let mut totals = vec![Tally::default(); n_algos];
    for (_, row) in &summary {
        for (i, (_, t)) in row.iter().enumerate() {
            totals[i].exact += t.exact;
            totals[i].near += t.near;
            totals[i].wrong += t.wrong;
            totals[i].rejected += t.rejected;
            totals[i].ambiguous += t.ambiguous;
        }
    }
    for (i, (name, _)) in algos.iter().enumerate() {
        let t = &totals[i];
        let total = t.ok() + t.wrong + t.rejected;
        println!(
            "  {:<11} 成功率 {:>5.1}%  (ex {:>3} near {:>3} wrong {:>3} rej {:>3} amb {:>3})",
            name,
            t.ok() as f64 / total as f64 * 100.0,
            t.exact,
            t.near,
            t.wrong,
            t.rejected,
            t.ambiguous
        );
    }
}

/// FFT 相位相关的符号约定自检：b(y)=a(y+s) ⟹ 峰在 k=s。
fn self_test() {
    let n = 512usize;
    let s = 137i32;
    let mut rng = frame::Rng(7);
    let a: Vec<f64> = (0..n)
        .map(|i| {
            // 平滑随机信号：低频三角级数
            (i as f64 * 0.05).sin() * 3.0
                + (i as f64 * 0.013 + 1.7).cos() * 5.0
                + (rng.next_u64() % 100) as f64 * 0.01
        })
        .collect();
    let b: Vec<f64> = (0..n)
        .map(|y| {
            let src = y as i32 + s;
            if src >= 0 && (src as usize) < n { a[src as usize] } else { a[n - 1] }
        })
        .collect();
    let out = algo::phase::phase_correlate(&a, &b, None);
    assert!(
        (out.shift - s as f64).abs() < 0.6,
        "phase 自检失败: 期望 {s} 实得 {:.2}",
        out.shift
    );
    println!("✓ phase 自检通过（shift={} 估得 {:.2}）", s, out.shift);
}

fn main() {
    self_test();
    run();
}
