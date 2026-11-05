//! The frame probe and heartbeat behind `JYKRE_PROBE=1` (ADR-0010).
//!
//! A frame is timed from the start of the root view's `render` to a canvas
//! painted last. The heartbeat is a UI-thread task that sleeps 5 ms in a
//! loop; any extra delay is time the UI thread was busy. Every few seconds
//! the summary (median, p95 and maximum frame, longest stall and which views
//! rendered) is logged at `info` under the `jykre::probe` target.
use gpui_kit::*;
use jykre::probe::{self, HEARTBEAT, REPORT_EVERY, Samples};
use std::{cell::RefCell, collections::BTreeMap, time::Instant};

#[derive(Default)]
struct Probe {
    frame_started: Option<Instant>,
    frames: Samples,
    stalls: Samples,
    renders: BTreeMap<&'static str, usize>,
    last_report: Option<Instant>,
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
/// heartbeat. Call once, on the UI thread.
pub fn start(cx: &mut App) {
    if !probe::enabled() {
        return;
    }
    PROBE.with(|probe| *probe.borrow_mut() = Some(Probe::default()));
    tracing::info!("frame probe on");
    // Lives as long as the application; it only measures.
    cx.spawn(async move |cx| {
        loop {
            let slept = Instant::now();
            cx.background_executor().timer(HEARTBEAT).await;
            let late = slept.elapsed().saturating_sub(HEARTBEAT);
            with(|probe| probe.stalls.push(late));
        }
    })
    .detach();
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
