//! The frame probe and heartbeat behind `JYKRE_PROBE=1` (ADR-0010).
//!
//! A frame is timed from the start of the root view's `render` to a canvas
//! painted last. The heartbeat is a UI-thread task that sleeps 5 ms in a
//! loop; any extra delay is time the UI thread was busy. Every few seconds
//! the summary (median, p95 and maximum frame, longest stall and which views
//! rendered) is logged at `info` under the `jykre::probe` target. The
//! performance run (`--test-performance`) also keeps each phase's frames and
//! stalls apart ([`begin_phase`], [`end_phase`]) for its report.
use gpui_kit::*;
use jykre::probe::{self, HEARTBEAT, REPORT_EVERY, Samples, Summary};
use std::{cell::RefCell, collections::BTreeMap, time::Instant};

#[derive(Default)]
struct Probe {
    frame_started: Option<Instant>,
    frames: Samples,
    stalls: Samples,
    renders: BTreeMap<&'static str, usize>,
    last_report: Option<Instant>,
    /// The performance run's phase being recorded, if any.
    phase: Option<Phase>,
}

/// One phase's frames and stalls.
#[derive(Default)]
struct Phase {
    frames: Samples,
    stalls: Samples,
}

/// What the probe saw during a phase of the performance run.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PhaseSummary {
    pub frames: Option<Summary>,
    pub stalls: Option<Summary>,
}

thread_local! {
    /// `None` unless the probe is on; the UI runs on one thread.
    static PROBE: RefCell<Option<Probe>> = const { RefCell::new(None) };
}

fn with(update: impl FnOnce(&mut Probe)) {
    PROBE.with(|probe| {
        if let Some(probe) = probe.borrow_mut().as_mut() {
            update(probe);
        }
    });
}

/// Turns the probe on when `JYKRE_PROBE` asks for it and starts the
/// heartbeat. Call once, on the UI thread. Returns whether it is on.
pub fn start(cx: &mut App) -> bool {
    if !probe::enabled() {
        return false;
    }
    PROBE.with(|probe| *probe.borrow_mut() = Some(Probe::default()));
    tracing::info!("frame probe on");
    // Lives as long as the application; it only measures.
    cx.spawn(async move |cx| {
        loop {
            let slept = Instant::now();
            cx.background_executor().timer(HEARTBEAT).await;
            let late = slept.elapsed().saturating_sub(HEARTBEAT);
            with(|probe| {
                probe.stalls.push(late);
                if let Some(phase) = &mut probe.phase {
                    phase.stalls.push(late);
                }
            });
        }
    })
    .detach();
    true
}

/// Starts recording a phase's frames and stalls apart from the periodic
/// summary; a phase already being recorded starts over. Nothing happens
/// while the probe is off.
pub fn begin_phase() {
    with(|probe| probe.phase = Some(Phase::default()));
}

/// Ends the phase and summarizes it; `None` while the probe is off.
pub fn end_phase() -> Option<PhaseSummary> {
    PROBE.with(|probe| {
        let mut probe = probe.borrow_mut();
        let probe = probe.as_mut()?;
        let mut phase = probe.phase.take().unwrap_or_default();
        Some(PhaseSummary {
            frames: phase.frames.take_summary(),
            stalls: phase.stalls.take_summary(),
        })
    })
}

/// Marks the start of a frame. Called first in the root view's `render`.
pub fn frame_started() {
    with(|probe| probe.frame_started = Some(Instant::now()));
}

/// Counts a `render` of `view`, so a summary shows which views a scenario
/// redraws.
pub fn rendered(view: &'static str) {
    with(|probe| *probe.renders.entry(view).or_default() += 1);
}

/// An empty element whose paint ends the frame. Add it last to the root view.
pub fn frame_end() -> impl IntoElement {
    canvas(|_, _, _| (), |_, _, _, _| frame_ended())
        .absolute()
        .size_0()
}

fn frame_ended() {
    with(|probe| {
        let now = Instant::now();
        if let Some(started) = probe.frame_started.take() {
            probe.frames.push(now - started);
            if let Some(phase) = &mut probe.phase {
                phase.frames.push(now - started);
            }
        }
        let due = probe
            .last_report
            .is_none_or(|last| now - last >= REPORT_EVERY);
        if due && !probe.frames.is_empty() {
            probe.last_report = Some(now);
            report(probe);
        }
    });
}

fn report(probe: &mut Probe) {
    let renders = std::mem::take(&mut probe.renders);
    let stall = probe.stalls.take_summary();
    if let Some(frames) = probe.frames.take_summary() {
        tracing::info!(
            target: "jykre::probe",
            frames = frames.count,
            median_ms = probe::ms(frames.median),
            p95_ms = probe::ms(frames.p95),
            max_ms = probe::ms(frames.max),
            stall_max_ms = stall.map_or(0.0, |stall| probe::ms(stall.max)),
            renders = ?renders,
            "frame probe"
        );
    }
}
