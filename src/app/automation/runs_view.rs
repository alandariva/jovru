//! The global **Runs** list (Q-22): the runs and chain runs active or
//! queued now, each with **Stop**, above every run and chain run of the
//! history, filterable by state.
use super::history::{self, Connections, HistoryEvent, HistoryList};
use super::{Jobs, kind_icon, words};
use crate::app::ui::*;
use gpui_kit::assets::IconName as I;
use gpui_kit::{prelude::FluentBuilder as _, *};
use jykre::automation::{ChainStatus, Engine, OverviewEntry, RunStatus};
use jykre::tr;
use std::time::{Duration, Instant};

pub struct RunsView {
    engine: Engine,
    /// What is active or queued, from `Engine::overview`: at most the
    /// admission limits and the queue's, so it is drawn without a list.
    active: Vec<OverviewEntry>,
    history: Entity<HistoryList>,
    focus: FocusHandle,
    tick: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<HistoryEvent> for RunsView {}

impl RunsView {
    pub fn new(
        engine: Engine,
        jobs: Jobs,
        connections: Connections,
        cx: &mut Context<Self>,
    ) -> Self {
        let history = cx.new(|cx| HistoryList::new(engine.clone(), None, jobs, connections, cx));
        let events = cx.subscribe(&history, |_, _, event: &HistoryEvent, cx| {
            cx.emit(event.clone());
        });
        let mut view = Self {
            active: engine.overview(),
            engine,
            history,
            focus: cx.focus_handle(),
            tick: None,
            _subscriptions: vec![events],
        };
        view.follow_queue(cx);
        view
    }

    pub fn focus(&self, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus, cx);
    }

    pub fn set_jobs(&mut self, jobs: Jobs, cx: &mut Context<Self>) {
        self.history
            .update(cx, |history, cx| history.set_jobs(jobs, cx));
    }

    pub fn set_connections(&mut self, connections: Connections, cx: &mut Context<Self>) {
        self.history
            .update(cx, |history, cx| history.set_connections(connections, cx));
    }

    /// A run started, waited, moved in the queue or ended.
    pub fn engine_changed(&mut self, cx: &mut Context<Self>) {
        self.active = self.engine.overview();
        self.follow_queue(cx);
        self.history.update(cx, |history, cx| history.refresh(cx));
        cx.notify();
    }

    /// While something is active or queued, its progress and the queue's
    /// time left are read again each second.
    fn follow_queue(&mut self, cx: &mut Context<Self>) {
        if self.active.is_empty() {
            self.tick = None;
            return;
        }
        if self.tick.is_some() {
            return;
        }
        self.tick = Some(cx.spawn(async move |weak, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(1)).await;
                let alive = weak
                    .update(cx, |this, cx| {
                        this.active = this.engine.overview();
                        if this.active.is_empty() {
                            this.tick = None;
                        }
                        cx.notify();
                        !this.active.is_empty()
                    })
                    .unwrap_or(false);
                if !alive {
                    break;
                }
            }
        }));
    }

    fn active_row(
        &self,
        index: usize,
        entry: &OverviewEntry,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let status = match (&entry.status, &entry.chain_status) {
            (
                Some(RunStatus::Queued {
                    position,
                    leaves_at,
                    blocked,
                }),
                _,
            ) => words::queued(
                *position,
                leaves_at.saturating_duration_since(Instant::now()),
                blocked,
                &self.engine.holders(blocked),
            ),
            (Some(RunStatus::Active(progress)), _) => words::progress(progress, entry.kind),
            (_, Some(ChainStatus::Running { step, .. })) => {
                tr!("Running step {0}.", step + 1)
            }
            _ => tr!("Ending…").to_owned(),
        };
        let run = entry.run;
        let chain = entry
            .chain
            .filter(|_| entry.step.is_none() && entry.run.is_none());
        row()
            .id(("active-run", index))
            .role(Role::ListItem)
            .aria_label(format!("{}: {}", entry.name, status))
            .gap_2()
            .px_2()
            .py_1()
            .when(entry.step.is_some(), |s| s.pl_6())
            .child(spinning_icon(
                ("active-spinner", index),
                I::LoaderCircle,
                colors().mint,
            ))
            .child(icon(kind_icon(entry.kind), colors().faint))
            .child(
                column()
                    .flex_1()
                    .min_w_0()
                    .child(div().truncate().child(match entry.step {
                        Some(step) => tr!("{0} · step {1}", entry.name, step + 1),
                        None => entry.name.clone(),
                    }))
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(rgb(colors().muted))
                            .child(status),
                    ),
            )
            .child(
                button(("active-stop", index), I::Square, tr!("Stop")).on_click(cx.listener(
                    move |this, _, _, cx| {
                        if let Some(chain) = chain {
                            this.engine.stop_chain(chain);
                        } else if let Some(run) = run {
                            this.engine.stop_run(run);
                        }
                        cx.notify();
                    },
                )),
            )
    }
}

impl Render for RunsView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        crate::app::probe::rendered("RunsView");
        let state = self.history.read(cx).state();
        let history = self.history.downgrade();
        let active: Vec<OverviewEntry> = self.active.clone();
        column()
            .id("runs")
            .key_context("AutomationRuns")
            .track_focus(&self.focus)
            .size_full()
            .bg(rgb(colors().bg))
            .child(
                row()
                    .h(px(48.))
                    .flex_none()
                    .px_3()
                    .gap_2()
                    .border_b_1()
                    .border_color(rgb(colors().border))
                    .child(icon(I::ListChecks, colors().mint))
                    .child(
                        div()
                            .text_size(px(15.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(tr!("Runs")),
                    )
                    .child(div().flex_1())
                    .child(label(tr!("State")))
                    .child(history::state_menu(history, state)),
            )
            .child(
                column()
                    .id("runs-active")
                    .role(Role::List)
                    .aria_label(tr!("Active and queued"))
                    .flex_none()
                    .p_2()
                    .gap_1()
                    .border_b_1()
                    .border_color(rgb(colors().border))
                    .child(label(if active.is_empty() {
                        tr!("Nothing is running or queued.").to_owned()
                    } else {
                        tr!("Active and queued: {0}", active.len())
                    }))
                    .child(label(words::limits_note()))
                    .children(
                        active
                            .iter()
                            .enumerate()
                            .map(|(index, entry)| self.active_row(index, entry, cx)),
                    ),
            )
            .child(div().flex_1().min_h_0().child(self.history.clone()))
    }
}
