//! Scenario tests for `onset::planner`: a simulated display (two frame
//! buffers, vsync on) drives the planner exactly the way the checkerboard's
//! frame loop does. Nothing is drawn; it's all arithmetic.
//!
//! The simulated display, per frame k:
//!   acquire(k) = when frame k-1 latched (getting a frame buffer frees frame
//!                k-2's then), or right away for the first two frames
//!   render_start(k) = acquire + CPU wake-up jitter
//!   ready(k) = render_start + work (+ extra for slow frames)
//!   slot(k) = max(slot of previous shown frame + 1,
//!                 first vsync >= ready + latch margin)
//!   presentedTime = vsync time of slot (+ noise), or 0 for hidden frames
//!   feedback arrives at vsync time + handler latency (+ extra delay)
//! The planner never sees the truth; it only gets render_start and the
//! feedback that has arrived by then.

use onset::planner::{Config, Feedback, FramePlan, FrameRow, Pacing, Planner, Status};

const P120: f64 = 1.0 / 120.0;
const LATCH_MARGIN: f64 = 0.0005;

struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Rng(seed.max(1))
    }
    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    fn unit(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }
    fn range(&mut self, lo: f64, hi: f64) -> f64 {
        lo + (hi - lo) * self.unit()
    }
}

type Delivery = Box<dyn Fn(usize, &mut Rng) -> Option<f64>>;

struct Scenario {
    frames: usize,
    pulse_frame: usize,
    period: Box<dyn Fn(i64) -> f64>,
    jitter_frac: f64,
    work_s: f64,
    extra_work: Vec<(usize, f64)>,
    /// presentedTime 0, but the slot is used (stale image stays up).
    hidden: Vec<usize>,
    /// presentedTime 0, and the next frame takes this frame's slot.
    replaced: Vec<usize>,
    latency: (f64, f64),
    noise_s: f64,
    /// Added to presentedTime (simulates a wrong timebase).
    timebase_offset_s: f64,
    seed: u64,
    /// Extra feedback delay per frame; `None` drops the feedback.
    delivery: Delivery,
}

impl Scenario {
    fn steady(frames: usize, pulse_frame: usize) -> Self {
        Scenario {
            frames,
            pulse_frame,
            period: Box::new(|_| P120),
            jitter_frac: 0.1,
            work_s: 0.001,
            extra_work: Vec::new(),
            hidden: Vec::new(),
            replaced: Vec::new(),
            latency: (0.001, 0.003),
            noise_s: 0.0,
            timebase_offset_s: 0.0,
            seed: 0x5eed,
            delivery: Box::new(|_, _| Some(0.0)),
        }
    }
}

struct Truth {
    render_start: Vec<f64>,
    slot: Vec<Option<i64>>,
    presented: Vec<f64>,
    arrival: Vec<Option<f64>>,
    vt: Vec<f64>,
}

fn simulate(s: &Scenario) -> Truth {
    let mut rng = Rng::new(s.seed);
    let mut drng = Rng::new(s.seed ^ 0x9e37_79b9_7f4a_7c15);
    let nv = s.frames * 3 + 1000;
    let mut vt = Vec::with_capacity(nv);
    let mut t = 5000.0;
    for n in 0..nv {
        vt.push(t);
        t += (s.period)(n as i64);
    }
    let first_at_or_after = |x: f64| vt.partition_point(|&v| v < x) as i64;

    let mut tr = Truth {
        render_start: Vec::new(),
        slot: Vec::new(),
        presented: Vec::new(),
        arrival: Vec::new(),
        vt: Vec::new(),
    };
    let mut last_slot: Option<i64> = None;
    let mut prev_end = vt[0] + 0.0001;
    for k in 0..s.frames {
        let p = (s.period)(last_slot.unwrap_or(0));
        let prev_replaced = k >= 1 && s.replaced.contains(&(k - 1));
        let acquire = if k < 2 || prev_replaced {
            prev_end
        } else {
            match tr.slot[k - 1] {
                Some(n) => vt[n as usize],
                None => tr.render_start[k - 1] + p,
            }
        }
        .max(prev_end);
        let rs = acquire + rng.unit() * s.jitter_frac * p;
        let extra: f64 = s.extra_work.iter().filter(|e| e.0 == k).map(|e| e.1).sum();
        let ready = rs + s.work_s + extra;
        prev_end = rs + s.work_s;
        let earliest = first_at_or_after(ready + LATCH_MARGIN);
        let n = last_slot.map_or(earliest, |l| earliest.max(l + 1));
        let replaced = s.replaced.contains(&k);
        let hidden = s.hidden.contains(&k);
        tr.render_start.push(rs);
        if replaced {
            tr.slot.push(None);
        } else {
            tr.slot.push(Some(n));
            last_slot = Some(n);
        }
        let shown_at = vt[n as usize];
        tr.presented.push(if replaced || hidden {
            0.0
        } else {
            shown_at + rng.range(-s.noise_s, s.noise_s) + s.timebase_offset_s
        });
        let base = shown_at + drng.range(s.latency.0, s.latency.1);
        tr.arrival
            .push((s.delivery)(k, &mut drng).map(|d| base + d));
    }
    tr.vt = vt;
    tr
}

struct Outcome {
    planner: Planner,
    plans: Vec<FramePlan>,
    truth: Truth,
    pulse_s: f64,
}

fn run(s: &Scenario, cfg: Config) -> Outcome {
    let truth = simulate(s);
    let mut planner = Planner::new(cfg);
    let mut events: Vec<(f64, u64, f64)> = (0..s.frames)
        .filter_map(|k| truth.arrival[k].map(|a| (a, k as u64, truth.presented[k])))
        .collect();
    events.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut next = 0;
    let mut plans = Vec::new();
    let mut pulse_s = f64::NAN;
    for k in 0..s.frames {
        let now = truth.render_start[k];
        while next < events.len() && events[next].0 <= now {
            planner.on_feedback(events[next].1, events[next].2);
            next += 1;
        }
        planner.expire(now);
        if k == s.pulse_frame {
            pulse_s = now - 0.002;
            planner.start_run(pulse_s);
        }
        plans.push(planner.begin_frame(now));
    }
    // After the last frame the app waits out the timeout for stragglers.
    for e in &events[next..] {
        planner.on_feedback(e.1, e.2);
    }
    planner.expire(truth.render_start[s.frames - 1] + 1.0);
    Outcome {
        planner,
        plans,
        truth,
        pulse_s,
    }
}

fn run_plans(o: &Outcome, pulse_frame: usize) -> Vec<u64> {
    o.plans[pulse_frame..]
        .iter()
        .map(|p| p.planned_index.unwrap())
        .collect()
}

fn assert_contiguous(planned: &[u64]) {
    for (i, &p) in planned.iter().enumerate() {
        assert_eq!(
            p,
            i as u64,
            "planned index {p} at run frame {i}: {:?}",
            &planned[i.saturating_sub(3)..(i + 3).min(planned.len())]
        );
    }
}

/// Planner's grid index minus the simulator's slot: must be one constant
/// for every frame the planner saw as shown.
fn assert_vsyncs_match_truth(o: &Outcome) {
    let mut offset = None;
    for (k, slot) in o.truth.slot.iter().enumerate() {
        if let (Some(Feedback::Shown { vsync, presented_s }), Some(slot)) =
            (o.planner.feedback(k as u64), slot)
        {
            let d = vsync - slot;
            assert_eq!(
                *offset.get_or_insert(d),
                d,
                "frame {k}: vsync {vsync}, true slot {slot}"
            );
            assert_eq!(presented_s, o.truth.presented[k]);
        }
    }
    assert!(offset.is_some(), "no frame was shown");
}

fn rows_from(o: &Outcome) -> Vec<FrameRow> {
    let rows = o.planner.rows();
    for r in &rows {
        if let Some(p) = r.presented_s {
            let want = o.truth.presented[r.id as usize] - o.pulse_s;
            assert!(
                (p - want).abs() < 1e-9,
                "frame {}: presented {p} vs {want}",
                r.id
            );
        }
    }
    rows
}

// ----------------------------------------------------------------------

#[test]
fn steady_120hz() {
    let s = Scenario::steady(1500, 300);
    let o = run(&s, Config::new(P120));
    assert_contiguous(&run_plans(&o, 300));
    assert_vsyncs_match_truth(&o);
    let rows = rows_from(&o);
    assert_eq!(rows.len(), 1200);
    for r in &rows {
        assert_eq!(r.status, Status::Shown);
        assert_eq!(r.vsync_index, Some(r.planned_index as i64));
        assert_eq!(r.missed_vsyncs, Some(0));
        assert_eq!(r.late_vsyncs, Some(0));
    }
    // Frame 0 reaches the screen 1-2 frames after the pulse.
    let first = rows[0].presented_s.unwrap();
    assert!(first > 0.0 && first < 3.0 * P120, "{first}");
    // Successive frames are exactly one period apart.
    let last = rows.last().unwrap();
    let span = last.presented_s.unwrap() - first;
    assert!((span - 1199.0 * P120).abs() < 1e-9, "{span}");
    let sum = o.planner.summary();
    assert_eq!(
        (
            sum.missed_vsyncs,
            sum.skipped_indices,
            sum.holds,
            o.planner.anomalies()
        ),
        (0, 0, 0, 0)
    );
    assert!((sum.period_s / P120 - 1.0).abs() < 1e-9, "{}", sum.period_s);
}

/// The old rule (round the CPU-measured gap between frames to whole
/// vsyncs, frames.rs + checkerboard.rs) on the same timeline.
fn legacy_indices(render_starts: &[f64], frame_s: f64) -> Vec<u64> {
    let mut out = Vec::new();
    let mut last: Option<(u64, f64)> = None;
    for &t in render_starts {
        let idx = match last {
            None => 0,
            Some((i, prev)) => i + (((t - prev) / frame_s).round() as u64).max(1),
        };
        out.push(idx);
        last = Some((idx, t));
    }
    out
}

#[test]
fn cpu_jitter_never_skips_but_the_old_rule_does() {
    let mut s = Scenario::steady(3000, 300);
    // Wake-ups anywhere in the first 70 % of the frame, as when the window
    // is composited. No frame is actually late (checked below).
    s.jitter_frac = 0.7;
    let o = run(&s, Config::new(P120));
    let slots: Vec<i64> = o.truth.slot.iter().map(|s| s.unwrap()).collect();
    assert!(
        slots.windows(2).all(|w| w[1] == w[0] + 1),
        "the simulated display dropped a frame"
    );
    assert_contiguous(&run_plans(&o, 300));
    assert_vsyncs_match_truth(&o);
    assert_eq!(o.planner.summary().missed_vsyncs, 0);

    let legacy = legacy_indices(&o.truth.render_start[300..], P120);
    let legacy_skips: u64 = legacy.windows(2).map(|w| w[1] - w[0] - 1).sum();
    assert!(legacy_skips > 10, "old rule skipped only {legacy_skips}");
    println!("old CPU-gap rule: {legacy_skips} false skips in 2700 frames; planner: 0");
}

#[test]
fn one_late_frame_catches_up() {
    let mut s = Scenario::steady(1500, 300);
    let late = 500;
    s.extra_work = vec![(late, 1.2 * P120)];
    let o = run(&s, Config::new(P120));
    // The display really showed a 2-vsync gap there.
    let gap = o.truth.slot[late].unwrap() - o.truth.slot[late - 1].unwrap();
    assert_eq!(gap, 2);
    assert_vsyncs_match_truth(&o);
    let rows = rows_from(&o);
    let row = |id: usize| rows.iter().find(|r| r.id == id as u64).unwrap();
    assert_eq!(row(late).missed_vsyncs, Some(1));
    assert_eq!(row(late).late_vsyncs, Some(1));
    // Exactly one stimulus frame is skipped, 1-3 frames after the late one.
    let skips: Vec<&FramePlan> = o.plans.iter().filter(|p| p.skipped > 0).collect();
    assert_eq!(skips.len(), 1, "{skips:?}");
    assert_eq!(skips[0].skipped, 1);
    let catch_up = skips[0].id as usize;
    assert!(
        (late + 1..=late + 3).contains(&catch_up),
        "caught up at {catch_up}"
    );
    // Back on the scanner's clock from then on.
    for r in rows.iter().filter(|r| r.id as usize >= catch_up) {
        assert_eq!(r.late_vsyncs, Some(0), "frame {}", r.id);
        assert_eq!(r.missed_vsyncs, Some(0), "frame {}", r.id);
    }
    for r in rows
        .iter()
        .filter(|r| (late + 1..catch_up).contains(&(r.id as usize)))
    {
        assert_eq!(
            r.late_vsyncs,
            Some(1),
            "in-flight frame {} shows its content one vsync late",
            r.id
        );
    }
    let sum = o.planner.summary();
    assert_eq!(
        (sum.missed_vsyncs, sum.skipped_indices, sum.holds),
        (1, 1, 0)
    );
    println!("late frame {late}: caught up at frame {catch_up}");
}

#[test]
fn long_stall_catches_up_before_feedback() {
    let mut s = Scenario::steady(1500, 300);
    let late = 500;
    s.extra_work = vec![(late, 4.5 * P120)];
    let o = run(&s, Config::new(P120));
    let gap = o.truth.slot[late].unwrap() - o.truth.slot[late - 1].unwrap();
    assert_eq!(gap, 5);
    // The very next frame starts drawing after its predicted vsync has
    // passed, so the causal bound moves most of the way at once.
    assert!(o.plans[late + 1].skipped >= 3, "{:?}", o.plans[late + 1]);
    let sum = o.planner.summary();
    assert_eq!(sum.skipped_indices, 4);
    assert_eq!(sum.missed_vsyncs, 4);
    let rows = rows_from(&o);
    for r in rows.iter().filter(|r| r.id as usize >= late + 4) {
        assert_eq!(r.late_vsyncs, Some(0), "frame {}", r.id);
    }
}

#[test]
fn presented_time_zero_with_slot_wasted() {
    let mut s = Scenario::steady(1500, 300);
    s.hidden = vec![600];
    let o = run(&s, Config::new(P120));
    assert_eq!(o.planner.feedback(600), Some(Feedback::NotShown));
    assert_contiguous(&run_plans(&o, 300));
    assert_vsyncs_match_truth(&o);
    let rows = rows_from(&o);
    let row = |id: u64| rows.iter().find(|r| r.id == id).unwrap();
    assert_eq!(row(600).status, Status::NotShown);
    assert_eq!(row(600).presented_s, None);
    // The refresh where frame 600 should have been showed stale content.
    assert_eq!(row(601).missed_vsyncs, Some(1));
    assert!(rows.iter().all(|r| r.late_vsyncs.unwrap_or(0) == 0));
    let sum = o.planner.summary();
    assert_eq!(
        (
            sum.not_shown,
            sum.skipped_indices,
            sum.holds,
            o.planner.anomalies()
        ),
        (1, 0, 0, 0)
    );
}

#[test]
fn presented_time_zero_with_next_frame_early_holds_once() {
    let mut s = Scenario::steady(1500, 300);
    s.replaced = vec![600];
    let o = run(&s, Config::new(P120));
    assert_eq!(o.planner.feedback(600), Some(Feedback::NotShown));
    // Frame 601 took 600's slot: it is one vsync early.
    assert_eq!(o.truth.slot[601].unwrap(), o.truth.slot[599].unwrap() + 1);
    assert_vsyncs_match_truth(&o);
    let sum = o.planner.summary();
    assert_eq!(sum.skipped_indices, 0, "never skip because of a 0");
    assert_eq!(sum.holds, 1, "hold one index to get back on the clock");
    let rows = rows_from(&o);
    let hold = rows.iter().find(|r| r.held).unwrap().id;
    assert!((601..=604).contains(&hold), "held at {hold}");
    for r in rows.iter().filter(|r| r.id >= hold) {
        assert_eq!(r.late_vsyncs.unwrap_or(0), 0, "frame {}", r.id);
    }
}

fn out_of_order_delivery() -> Delivery {
    Box::new(|k, rng| {
        if k % 50 == 7 {
            Some(0.8) // later than the 0.5 s timeout
        } else {
            Some(rng.range(0.0, 3.0 * P120)) // reorders neighbours
        }
    })
}

#[test]
fn out_of_order_and_late_feedback() {
    let mut s = Scenario::steady(1500, 300);
    s.delivery = out_of_order_delivery();
    let o = run(&s, Config::new(P120));
    // Delivery really was out of order.
    let mut order: Vec<usize> = (0..s.frames).collect();
    order.sort_by(|&a, &b| {
        o.truth.arrival[a]
            .unwrap()
            .total_cmp(&o.truth.arrival[b].unwrap())
    });
    assert!(order.windows(2).filter(|w| w[0] > w[1]).count() > 100);
    assert_contiguous(&run_plans(&o, 300));
    assert_vsyncs_match_truth(&o);
    let sum = o.planner.summary();
    assert_eq!(
        sum.shown, 1200,
        "late feedback must still be accepted: {sum:?}"
    );
    assert_eq!(
        (
            sum.missed_vsyncs,
            sum.skipped_indices,
            sum.holds,
            o.planner.anomalies()
        ),
        (0, 0, 0, 0)
    );
    // Same log as with prompt, in-order feedback.
    let in_order = run(&Scenario::steady(1500, 300), Config::new(P120));
    let a: Vec<_> = o
        .planner
        .rows()
        .iter()
        .map(|r| (r.vsync_index, r.missed_vsyncs))
        .collect();
    let b: Vec<_> = in_order
        .planner
        .rows()
        .iter()
        .map(|r| (r.vsync_index, r.missed_vsyncs))
        .collect();
    assert_eq!(a, b);
}

#[test]
fn out_of_order_feedback_with_a_late_frame() {
    let mut s = Scenario::steady(1500, 300);
    s.delivery = out_of_order_delivery();
    let late = 520;
    s.extra_work = vec![(late, 1.2 * P120)];
    let o = run(&s, Config::new(P120));
    assert_vsyncs_match_truth(&o);
    let sum = o.planner.summary();
    assert_eq!(
        (sum.missed_vsyncs, sum.skipped_indices, sum.holds),
        (1, 1, 0)
    );
    let catch_up = o.plans.iter().find(|p| p.skipped > 0).unwrap().id as usize;
    assert!(
        (late + 1..=late + 6).contains(&catch_up),
        "caught up at {catch_up}"
    );
    let rows = rows_from(&o);
    for r in rows.iter().filter(|r| r.id as usize >= catch_up) {
        assert_eq!(r.late_vsyncs.unwrap_or(0), 0, "frame {}", r.id);
    }
}

#[test]
fn missing_feedback() {
    let mut s = Scenario::steady(2000, 300);
    s.delivery = Box::new(|k, rng| {
        let burst = (800..860).contains(&k);
        (!burst && rng.unit() > 0.2).then_some(0.0)
    });
    let o = run(&s, Config::new(P120));
    assert_contiguous(&run_plans(&o, 300));
    assert_vsyncs_match_truth(&o);
    let rows = rows_from(&o);
    let sum = o.planner.summary();
    assert!(sum.no_feedback > 60 + 200, "{sum:?}");
    assert_eq!(sum.pending, 0);
    for r in &rows {
        match r.status {
            Status::NoFeedback => assert_eq!(
                (r.presented_s, r.vsync_index, r.missed_vsyncs),
                (None, None, None)
            ),
            Status::Shown => {
                assert_eq!(r.vsync_index, Some(r.planned_index as i64));
                assert_eq!(r.missed_vsyncs.unwrap_or(0), 0, "false miss at {}", r.id);
            }
            other => panic!("unexpected {other:?}"),
        }
    }
    assert_eq!(
        (sum.skipped_indices, sum.holds, o.planner.anomalies()),
        (0, 0, 0)
    );
}

#[test]
fn wrong_timebase_is_rejected() {
    // presentedTime in a clock that counts sleep (mach_continuous_time was
    // 108874 s ahead of CLOCK_UPTIME_RAW on the test Mac).
    let mut s = Scenario::steady(600, 200);
    s.timebase_offset_s = 108_874.47;
    let o = run(&s, Config::new(P120));
    assert_eq!(o.planner.anomalies(), 600);
    assert!(!o.planner.is_warmed_up());
    // Without trusted feedback it degrades to one index per frame.
    assert_contiguous(&run_plans(&o, 200));
}

fn drift_scenario() -> Scenario {
    let frames = 600 + 72_000; // 5 s waiting + 10 min run
    let n = 3.0 * frames as f64;
    let (pa, pb) = (1.0 / 119.96, 1.0 / 119.94);
    let mut s = Scenario::steady(frames, 600);
    s.period = Box::new(move |v| pa + (pb - pa) * (v as f64 / n).min(1.0));
    s.jitter_frac = 0.5;
    s.latency = (0.001, 0.004);
    s.noise_s = 20e-6;
    s
}

#[test]
fn drift_over_10_minutes_vsync_count() {
    let s = drift_scenario();
    let o = run(&s, Config::new(P120)); // nominal 120 Hz, display ~119.95 Hz
    assert_contiguous(&run_plans(&o, 600));
    assert_vsyncs_match_truth(&o);
    let sum = o.planner.summary();
    assert_eq!(
        (
            sum.missed_vsyncs,
            sum.skipped_indices,
            sum.holds,
            o.planner.anomalies()
        ),
        (0, 0, 0, 0)
    );
    let last = o.truth.slot.last().unwrap().unwrap() as usize;
    let true_p = o.truth.vt[last] - o.truth.vt[last - 1];
    let rel = (sum.period_s / true_p - 1.0).abs();
    assert!(rel < 1e-5, "period {} vs {true_p} ({rel:e})", sum.period_s);
    let rows = rows_from(&o);
    let lag = rows.last().unwrap().presented_s.unwrap() - 71_999.0 * P120;
    println!(
        "vsync-count: measured {:.4} Hz; the run ends {:.1} ms after a 120 Hz schedule would",
        1.0 / sum.period_s,
        lag * 1e3
    );
}

#[test]
fn drift_over_10_minutes_time_locked() {
    let s = drift_scenario();
    let mut cfg = Config::new(P120);
    cfg.pacing = Pacing::TimeLocked {
        schedule_period_s: P120,
    };
    let o = run(&s, cfg);
    assert_vsyncs_match_truth(&o);
    let rows = rows_from(&o);
    let sum = o.planner.summary();
    // On the host clock all run long: never more than ~0.6 frame off.
    let worst = rows
        .iter()
        .filter_map(|r| r.late_s)
        .fold(0.0f64, |m, l| m.max(l.abs()));
    assert!(worst <= 0.6 * P120 + 50e-6, "worst {worst}");
    // Skips only as often as the rate difference demands, spread out.
    let shown: Vec<&FrameRow> = rows.iter().filter(|r| r.presented_s.is_some()).collect();
    let span = shown.last().unwrap().presented_s.unwrap() - shown[0].presented_s.unwrap();
    let expected = (span / P120).round() as i64 - (rows.len() as i64 - 1);
    assert!(
        (sum.skipped_indices as i64 - expected).abs() <= 1,
        "skipped {} expected {expected}",
        sum.skipped_indices
    );
    let at: Vec<u64> = rows
        .iter()
        .filter(|r| r.skipped > 0)
        .map(|r| r.id)
        .collect();
    assert!(at.windows(2).all(|w| w[1] - w[0] > 1000), "{at:?}");
    assert!(rows.iter().all(|r| r.skipped <= 1));
    assert_eq!(
        (sum.holds, sum.missed_vsyncs, o.planner.anomalies()),
        (0, 0, 0)
    );
    println!(
        "time-locked: {} deliberate skips (expected {expected}), worst error {:.2} ms",
        sum.skipped_indices,
        worst * 1e3
    );
}

#[test]
fn warm_up_is_reported() {
    let s = Scenario::steady(200, 150);
    let o = run(&s, Config::new(P120));
    assert!(o.planner.is_warmed_up());
    let fresh = Planner::new(Config::new(P120));
    assert!(!fresh.is_warmed_up());
}
