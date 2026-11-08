//! A list of runs and chain runs from `automation.db`, newest first, read a
//! page at a time by its keyset (`HistoryFilter::before`), with the
//! details of the one chosen: its state and cause in words, counters,
//! outputs, a program's logs (read only when opened) and, for a chain run,
//! each step. A job's **Runs** tab and the global **Runs** list both use it.
//!
//! Nothing here holds a value: the history says what ran, when and how it
//! ended (specification, section 6.4).
use super::logic::{self, SnapshotStep, StepLine};
use super::program_bits::{self, Stream};
use super::words::{self, Level, Line};
use super::{Jobs, kind_icon};
use crate::app::ui::*;
use gpui_kit::assets::IconName as I;
use gpui_kit::component::Sizable;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::scroll::ScrollableElement;
use gpui_kit::{prelude::FluentBuilder as _, *};
use jykre::automation::definition::{JobBody, JobDefinition};
use jykre::automation::{Engine, JobId, JobKind, RunId, RunState, UtcMillis};
use jykre::drivers::*;
use jykre::{backend, tr};
use jykre_core::automation::{ChainRunRecord, RunOutput, RunRecord};
use jykre_storage::{HistoryEntry, HistoryFilter};
use std::collections::HashMap;
use std::ops::Range;
use std::rc::Rc;

/// Entries read per page.
const PAGE: u32 = 50;
const ROW_HEIGHT: f32 = 52.;
const LOG_LINE: f32 = 18.;
/// A log line longer than this is cut where it is shown.
const LOG_LINE_CHARS: usize = 2_000;

#[derive(Clone)]
pub enum HistoryEvent {
    /// A chain step run again on its own: its job's form, prefilled from
    /// what the earlier steps stored (ADR-0035). It never runs by itself.
    OpenJob {
        job: JobId,
        values: HashMap<String, String>,
        missing: Vec<String>,
    },
}

impl EventEmitter<HistoryEvent> for HistoryList {}

/// The connections' names and dialects, by identity.
pub type Connections = Rc<HashMap<ConnectionId, (String, Option<SqlDialect>)>>;

/// What was read about the entry chosen.
enum Detail {
    Run {
        record: Box<RunRecord>,
        outputs: Vec<RunOutput>,
        name: String,
    },
    Chain {
        record: Box<ChainRunRecord>,
        name: String,
        lines: Vec<StepLine>,
        /// The chain and its steps as confirmed.
        chain: Option<Box<JobDefinition>>,
        steps: Vec<SnapshotStep>,
        /// What each step stored, by step ID.
        stored: HashMap<String, Vec<RunOutput>>,
        /// The step whose run is shown.
        open: Option<usize>,
    },
}

/// A program run's log, as it was read when opened.
struct LogView {
    run: RunId,
    stream: Stream,
    /// `None` while it is read.
    lines: Option<Rc<Vec<SharedString>>>,
    dropped: Option<u64>,
    missing: bool,
    scroll: UniformListScrollHandle,
}

pub struct HistoryList {
    engine: Engine,
    job: Option<JobId>,
    state: Option<RunState>,
    entries: Vec<HistoryEntry>,
    more: bool,
    loading: bool,
    generation: u64,
    scroll: UniformListScrollHandle,
    selected: Option<String>,
    detail: Option<Detail>,
    detail_generation: u64,
    detail_scroll: ScrollHandle,
    log: Option<LogView>,
    jobs: Jobs,
    connections: Connections,
    error: Option<String>,
    notice: Option<Feedback>,
    focus: FocusHandle,
}

impl HistoryList {
    /// The history of `job`, or every run when `None`.
    pub fn new(
        engine: Engine,
        job: Option<JobId>,
        jobs: Jobs,
        connections: Connections,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut list = Self {
            engine,
            job,
            state: None,
            entries: Vec::new(),
            more: false,
            loading: false,
            generation: 0,
            scroll: UniformListScrollHandle::new(),
            selected: None,
            detail: None,
            detail_generation: 0,
            detail_scroll: ScrollHandle::new(),
            log: None,
            jobs,
            connections,
            error: None,
            notice: None,
            focus: cx.focus_handle(),
        };
        list.load(true, cx);
        list
    }

    pub fn focus(&self, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus, cx);
    }

    pub fn set_jobs(&mut self, jobs: Jobs, cx: &mut Context<Self>) {
        self.jobs = jobs;
        cx.notify();
    }

    pub fn set_connections(&mut self, connections: Connections, cx: &mut Context<Self>) {
        self.connections = connections;
        cx.notify();
    }

    pub fn set_state(&mut self, state: Option<RunState>, cx: &mut Context<Self>) {
        self.state = state;
        self.entries.clear();
        self.selected = None;
        self.detail = None;
        self.log = None;
        self.load(true, cx);
    }

    pub fn state(&self) -> Option<RunState> {
        self.state
    }

    /// Reads the pages shown again, after a run started or ended.
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        self.load(true, cx);
        if let Some(id) = self.selected.clone()
            && let Some(index) = self.entries.iter().position(|entry| entry.id_text() == id)
        {
            self.read_detail(index, false, cx);
        }
    }

    /// Reads the first page (`reset`, as many entries as are shown, at most
    /// 1,000) or the next one after the last entry shown.
    fn load(&mut self, reset: bool, cx: &mut Context<Self>) {
        let Some(repository) = self.engine.repository() else {
            return;
        };
        self.generation += 1;
        let generation = self.generation;
        let limit = if reset {
            u32::try_from(self.entries.len())
                .unwrap_or(u32::MAX)
                .clamp(PAGE, 1000)
        } else {
            PAGE
        };
        let filter = HistoryFilter {
            job: self.job,
            state: self.state,
            before: (!reset)
                .then(|| {
                    self.entries
                        .last()
                        .map(|entry| (entry.started_at(), entry.id_text()))
                })
                .flatten(),
            limit,
        };
        self.loading = true;
        cx.spawn(async move |weak, cx| {
            let read = backend::run(async move { repository.history(&filter).await }).await;
            let _ = weak.update(cx, |this, cx| {
                if this.generation != generation {
                    return;
                }
                this.loading = false;
                match read {
                    Ok(page) => {
                        this.more = page.len() == limit as usize;
                        if reset {
                            this.entries = page;
                        } else {
                            this.entries.extend(page);
                        }
                        this.error = None;
                    }
                    Err(error) => this.error = Some(tr!("Could not read the history: {0}", error)),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn job_name(&self, job: Option<JobId>) -> Option<String> {
        let job = job?;
        self.jobs
            .iter()
            .find(|candidate| candidate.id == job)
            .map(|candidate| candidate.name.clone())
    }

    /// Reads the chosen entry's details: its record again, its outputs, its
    /// snapshot's names and, for a chain run, its steps.
    fn read_detail(&mut self, index: usize, reset_log: bool, cx: &mut Context<Self>) {
        let Some(entry) = self.entries.get(index).cloned() else {
            return;
        };
        let Some(repository) = self.engine.repository() else {
            return;
        };
        self.selected = Some(entry.id_text());
        if reset_log {
            self.log = None;
        }
        self.detail_generation += 1;
        let generation = self.detail_generation;
        let known = match &entry {
            HistoryEntry::Run(run) => self.job_name(run.job_id),
            HistoryEntry::Chain(chain) => self.job_name(chain.job_id),
        };
        let open = match &self.detail {
            Some(Detail::Chain { record, open, .. })
                if Some(record.id.to_string()) == self.selected =>
            {
                *open
            }
            _ => None,
        };
        cx.spawn(async move |weak, cx| {
            let read = backend::run(async move {
                match entry {
                    HistoryEntry::Run(run) => {
                        let record = repository.run(run.id).await?.map_or(run, Box::new);
                        let outputs = repository.outputs(record.id).await?;
                        let snapshot = repository.run_snapshot(record.id).await?;
                        let name = known
                            .or_else(|| {
                                snapshot
                                    .as_deref()
                                    .and_then(logic::snapshot_definition)
                                    .map(|definition| definition.name)
                            })
                            .unwrap_or_default();
                        Ok(Detail::Run {
                            record,
                            outputs,
                            name,
                        })
                    }
                    HistoryEntry::Chain(chain) => {
                        let record = repository.chain_run(chain.id).await?.unwrap_or(chain);
                        let runs = repository.chain_steps(record.id).await?;
                        let snapshot = repository.chain_run_snapshot(record.id).await?;
                        let (chain_definition, steps) = snapshot
                            .as_deref()
                            .and_then(logic::chain_snapshot)
                            .map_or((None, Vec::new()), |(chain, steps)| (Some(chain), steps));
                        let jobs: Vec<(String, Option<JobId>)> = steps
                            .iter()
                            .map(|step| {
                                let job = chain_definition.as_ref().and_then(|chain| match &chain
                                    .body
                                {
                                    JobBody::Chain(body) => body
                                        .steps
                                        .iter()
                                        .find(|candidate| candidate.id == step.id)
                                        .map(|candidate| candidate.job),
                                    _ => None,
                                });
                                (step.id.clone(), job)
                            })
                            .collect();
                        let mut stored = HashMap::new();
                        for run in &runs {
                            let Some(index) = run.step_index.and_then(|i| usize::try_from(i).ok())
                            else {
                                continue;
                            };
                            if run.outputs_stored == Some(true)
                                && let Some((id, _)) = jobs.get(index)
                            {
                                stored.insert(id.clone(), repository.outputs(run.id).await?);
                            }
                        }
                        let lines = logic::chain_steps(&record, &jobs, &runs);
                        let name = known
                            .or_else(|| chain_definition.as_ref().map(|chain| chain.name.clone()))
                            .unwrap_or_default();
                        Ok(Detail::Chain {
                            record: Box::new(record),
                            name,
                            lines,
                            chain: chain_definition.map(Box::new),
                            steps,
                            stored,
                            open,
                        })
                    }
                }
            })
            .await;
            let _ = weak.update(cx, |this, cx| {
                if this.detail_generation != generation {
                    return;
                }
                match read {
                    Ok(detail) => this.detail = Some(detail),
                    Err(error) => this.error = Some(tr!("Could not read the run: {0}", error)),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// Opens one of a program run's logs, read from disk off the UI thread
    /// only now (ADR-0037).
    fn open_log(&mut self, run: RunId, stream: Stream, cx: &mut Context<Self>) {
        let dir = self.engine.data_dir().to_path_buf();
        self.log = Some(LogView {
            run,
            stream,
            lines: None,
            dropped: None,
            missing: false,
            scroll: UniformListScrollHandle::new(),
        });
        cx.notify();
        cx.spawn(async move |weak, cx| {
            let read = backend::run(async move {
                let log = tokio::task::spawn_blocking(move || {
                    program_bits::read_log(&dir, run, stream).map(|log| {
                        let lines: Vec<SharedString> = log
                            .text
                            .lines()
                            .map(|line| {
                                if line.len() > LOG_LINE_CHARS {
                                    let cut: String = line.chars().take(LOG_LINE_CHARS).collect();
                                    SharedString::from(format!("{cut}…"))
                                } else {
                                    SharedString::from(line.to_owned())
                                }
                            })
                            .collect();
                        (lines, log.dropped)
                    })
                })
                .await
                .ok()
                .flatten();
                Ok(log)
            })
            .await;
            let _ = weak.update(cx, |this, cx| {
                let Some(view) = this.log.as_mut() else {
                    return;
                };
                if view.run != run || view.stream != stream {
                    return;
                }
                match read {
                    Ok(Some((lines, dropped))) => {
                        view.lines = Some(Rc::new(lines));
                        view.dropped = dropped;
                    }
                    _ => {
                        view.lines = Some(Rc::default());
                        view.missing = true;
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// **Delete logs**: the run's log folder goes now.
    fn delete_logs(&mut self, run: RunId, cx: &mut Context<Self>) {
        let engine = self.engine.clone();
        self.log = None;
        cx.spawn(async move |weak, cx| {
            let _ = backend::run(async move {
                engine.delete_logs(run).await;
                Ok(())
            })
            .await;
            let _ = weak.update(cx, |this, cx| {
                this.notice = Some(Feedback::new(
                    Tone::Success,
                    tr!("The run's logs were deleted."),
                ));
                this.refresh(cx);
            });
        })
        .detach();
    }

    /// A chain step run again on its own: its job's form, prefilled.
    fn run_step_again(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(Detail::Chain {
            chain: Some(chain),
            steps,
            stored,
            ..
        }) = &self.detail
        else {
            return;
        };
        let JobBody::Chain(body) = &chain.body else {
            return;
        };
        let Some(snapshot) = steps.get(index) else {
            return;
        };
        let Some(step) = body.steps.iter().find(|step| step.id == snapshot.id) else {
            return;
        };
        let Some(target) = &snapshot.definition else {
            return;
        };
        let (values, missing) = logic::prefill(step, target, stored);
        cx.emit(HistoryEvent::OpenJob {
            job: step.job,
            values,
            missing,
        });
    }

    // -----------------------------------------------------------------------
    // Drawing.

    fn entry_row(&self, index: usize, cx: &mut Context<Self>) -> Option<Stateful<Div>> {
        if index == self.entries.len() {
            return Some(
                div().id("history-more").h(px(ROW_HEIGHT)).p_2().child(
                    button("history-load-more", I::ChevronDown, tr!("Load older runs"))
                        .on_click(cx.listener(|this, _, _, cx| this.load(false, cx))),
                ),
            );
        }
        let entry = self.entries.get(index)?;
        let selected = self.selected.as_deref() == Some(entry.id_text().as_str());
        let (kind, job, state, cause, started, finished, draft, step_of) = match entry {
            HistoryEntry::Run(run) => (
                run.kind,
                run.job_id,
                run.state,
                run.cause,
                run.started_at,
                run.finished_at,
                run.job_id.is_none(),
                None::<i64>,
            ),
            HistoryEntry::Chain(chain) => (
                JobKind::Chain,
                chain.job_id,
                chain.state,
                chain.cause,
                chain.started_at,
                chain.finished_at,
                chain.job_id.is_none(),
                None,
            ),
        };
        let _ = step_of;
        let name = self
            .job_name(job)
            .unwrap_or_else(|| tr!("Draft or deleted job").to_owned());
        let elapsed = finished.map(|end| end.since(started));
        let state_text = words::state_label(state, cause, elapsed);
        let ago = jykre::explorer::format_age(UtcMillis::now().since(started));
        let detail = match elapsed {
            Some(elapsed) => format!(
                "{} · {} · {}",
                words::kind_name(kind),
                ago,
                jykre::cell_format::duration(u64::try_from(elapsed.as_millis()).unwrap_or(0))
            ),
            None => format!("{} · {}", words::kind_name(kind), ago),
        };
        let (symbol, color) = state_mark(state);
        Some(
            row()
                .id(("history-entry", index))
                .role(Role::ListItem)
                .aria_label(format!("{name}: {state_text}, {detail}"))
                .aria_selected(selected)
                .focusable()
                .tab_index(0)
                .focus_visible(|s| s.bg(rgb(colors().hover)))
                .h(px(ROW_HEIGHT))
                .px_2()
                .rounded(px(6.))
                .cursor_pointer()
                .when(selected, |s| s.bg(rgb(colors().accent_bg)))
                .hover(|s| s.bg(rgb(colors().hover)))
                .child(icon(symbol, color))
                .child(
                    column()
                        .flex_1()
                        .min_w_0()
                        .child(
                            row()
                                .min_w_0()
                                .child(icon(kind_icon(kind), colors().faint))
                                .child(div().flex_1().min_w_0().truncate().child(name))
                                .when(draft && kind != JobKind::Chain, |s| {
                                    s.child(badge(tr!("Draft"), colors().amber))
                                }),
                        )
                        .child(
                            div()
                                .truncate()
                                .text_size(px(12.))
                                .text_color(rgb(colors().faint))
                                .child(format!("{state_text} · {detail}")),
                        ),
                )
                .on_click(cx.listener(move |this, _, _, cx| this.read_detail(index, true, cx))),
        )
    }

    fn detail_panel(&self, cx: &mut Context<Self>) -> Stateful<Div> {
        let panel = column()
            .id("history-detail")
            .role(Role::Group)
            .aria_label(tr!("The run chosen"))
            .flex_1()
            .min_w_0()
            .h_full()
            .overflow_y_scroll()
            .track_scroll(&self.detail_scroll)
            .p_3()
            .gap_2();
        match &self.detail {
            None => panel.child(label(tr!("Choose a run to see what ran and how it ended."))),
            Some(Detail::Run {
                record,
                outputs,
                name,
            }) => panel
                .child(
                    div()
                        .text_size(px(15.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(if name.is_empty() {
                            tr!("Draft or deleted job").to_owned()
                        } else {
                            name.clone()
                        }),
                )
                .child(self.run_body(record, outputs, cx)),
            Some(Detail::Chain {
                record,
                name,
                lines,
                steps,
                open,
                ..
            }) => {
                let elapsed = record.finished_at.map(|end| end.since(record.started_at));
                let mut summary = vec![Line {
                    level: match record.state {
                        RunState::Succeeded => Level::Success,
                        RunState::Running => Level::Info,
                        RunState::Cancelled | RunState::Interrupted => Level::Warning,
                        RunState::Failed => Level::Error,
                    },
                    text: match record.cause {
                        Some(cause) => format!(
                            "{}. {}",
                            words::state_label(record.state, Some(cause), elapsed),
                            words::cause_sentence(cause)
                        ),
                        None => words::state_label(record.state, None, elapsed),
                    },
                }];
                if let Some(text) = logic::plan_changed(record, |step| {
                    let connection = steps
                        .iter()
                        .find(|snapshot| snapshot.id == step)
                        .and_then(|snapshot| snapshot.definition.as_ref())
                        .and_then(|definition| match &definition.body {
                            JobBody::Sql(sql) => sql.connection,
                            _ => None,
                        })?;
                    self.connections
                        .get(&connection)
                        .map(|(name, _)| name.clone())
                }) {
                    summary.push(Line {
                        level: Level::Error,
                        text,
                    });
                }
                if record.effect_unknown {
                    summary.push(Line {
                        level: Level::Warning,
                        text: words::effect_unknown().to_owned(),
                    });
                }
                summary.push(Line {
                    level: Level::Info,
                    text: tr!("Started {0}.", words::utc(record.started_at)),
                });
                panel
                    .child(
                        row().child(icon(I::Workflow, colors().mint)).child(
                            div()
                                .text_size(px(15.))
                                .font_weight(FontWeight::SEMIBOLD)
                                .child(if name.is_empty() {
                                    tr!("Draft or deleted chain").to_owned()
                                } else {
                                    name.clone()
                                }),
                        ),
                    )
                    .children(summary.iter().map(line_element))
                    .child(label(tr!("Steps")))
                    .children(lines.iter().enumerate().map(|(index, step)| {
                        self.step_row(
                            index,
                            step,
                            steps.get(index).map(|snapshot| snapshot.version),
                            *open == Some(index),
                            cx,
                        )
                    }))
            }
        }
    }

    /// The account of a run, in the details or under a chain's step.
    fn run_body(&self, record: &RunRecord, outputs: &[RunOutput], cx: &mut Context<Self>) -> Div {
        let dialect = record
            .connection_id
            .and_then(|id| self.connections.get(&id))
            .and_then(|(_, dialect)| *dialect);
        let lines = words::record_lines(record, dialect);
        let elapsed = record.finished_at.map(|end| end.since(record.started_at));
        let mut timing = tr!("Started {0}.", words::utc(record.started_at));
        if let Some(elapsed) = elapsed {
            timing.push(' ');
            timing.push_str(&tr!(
                "Took {0}.",
                jykre::cell_format::duration(u64::try_from(elapsed.as_millis()).unwrap_or(0))
            ));
        }
        let connection = record
            .connection_id
            .and_then(|id| self.connections.get(&id))
            .map(|(name, _)| name.clone());
        let logs = [
            (Stream::Stdout, record.log_bytes_stdout),
            (Stream::Stderr, record.log_bytes_stderr),
            (Stream::Jjp, record.log_bytes_jjp),
        ];
        let has_logs = logs.iter().any(|(_, bytes)| bytes.is_some());
        let run = record.id;
        column()
            .gap_1()
            .children(lines.iter().map(line_element))
            .child(label(timing))
            .when_some(connection, |s, connection| {
                s.child(label(tr!("On {0}.", connection)))
            })
            .when_some(record.job_version, |s, version| {
                s.child(label(tr!("Ran version {0}.", version)))
            })
            .when(!outputs.is_empty(), |s| {
                s.child(label(tr!("Declared outputs")))
                    .children(outputs.iter().map(|output| {
                        let state = if output.expired {
                            tr!("value deleted after 7 days").to_owned()
                        } else if output.is_null {
                            tr!("NULL").to_owned()
                        } else {
                            tr!("{0} bytes kept", output.bytes)
                        };
                        label(format!("{} · {}", output.name, state))
                    }))
            })
            .when(record.kind == JobKind::Process, |s| {
                s.child(label(words::logs_note()))
                    .when(!has_logs, |s| {
                        s.child(label(tr!("No logs are kept for this run.")))
                    })
                    .when(has_logs, |s| {
                        s.child(
                            row()
                                .flex_wrap()
                                .gap_2()
                                .children(logs.into_iter().filter_map(|(stream, bytes)| {
                                    let bytes = u64::try_from(bytes?).unwrap_or(0);
                                    let open = self
                                        .log
                                        .as_ref()
                                        .is_some_and(|log| log.run == run && log.stream == stream);
                                    Some(
                                        button(
                                            ("history-log", stream as usize),
                                            I::ScrollText,
                                            format!(
                                                "{} · {}",
                                                stream.name(),
                                                jykre::cell_format::size(bytes)
                                            ),
                                        )
                                        .aria_selected(open)
                                        .on_click(
                                            cx.listener(move |this, _, _, cx| {
                                                this.open_log(run, stream, cx)
                                            }),
                                        ),
                                    )
                                }))
                                .child(
                                    button("history-delete-logs", I::Trash, tr!("Delete logs"))
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            this.delete_logs(run, cx)
                                        })),
                                ),
                        )
                    })
                    .when_some(self.log.as_ref().filter(|log| log.run == run), |s, log| {
                        s.child(self.log_view(log))
                    })
            })
    }

    fn log_view(&self, log: &LogView) -> Div {
        let header = tr!("{0} of this run", log.stream.name());
        let body = match &log.lines {
            None => label(tr!("Reading the log…")).into_any_element(),
            Some(_) if log.missing => label(tr!(
                "This log is no longer on disk: it was deleted, or 7 days passed."
            ))
            .into_any_element(),
            Some(lines) if lines.is_empty() => label(tr!("The log is empty.")).into_any_element(),
            Some(lines) => {
                let lines = lines.clone();
                let count = lines.len();
                let shown = count.min(16) as f32;
                div()
                    .relative()
                    .h(px(shown * LOG_LINE + 8.))
                    .rounded(px(6.))
                    .bg(rgb(colors().grid_bg))
                    .border_1()
                    .border_color(rgb(colors().border))
                    .child(
                        uniform_list(
                            "history-log-lines",
                            count,
                            move |range: Range<usize>, _, _| {
                                range
                                    .filter_map(|index| {
                                        let text = lines.get(index)?.clone();
                                        Some(
                                            div()
                                                .id(("log-line", index))
                                                .h(px(LOG_LINE))
                                                .px_2()
                                                .whitespace_nowrap()
                                                .font_family(fonts().mono.clone())
                                                .text_size(px(12.))
                                                .child(text),
                                        )
                                    })
                                    .collect::<Vec<_>>()
                            },
                        )
                        .size_full()
                        .track_scroll(&log.scroll),
                    )
                    .vertical_scrollbar(&log.scroll)
                    .into_any_element()
            }
        };
        column()
            .gap_1()
            .child(label(header))
            .child(body)
            .when_some(log.dropped, |s, dropped| {
                s.child(label(tr!(
                    "{0} bytes past the 1 MiB limit were read and dropped.",
                    dropped
                )))
            })
    }

    fn step_row(
        &self,
        index: usize,
        step: &StepLine,
        version: Option<i64>,
        open: bool,
        cx: &mut Context<Self>,
    ) -> Div {
        let state = step.run.as_ref().map(|run| run.state);
        let (symbol, color) = match state {
            Some(state) => state_mark(state),
            None => (I::CircleSlash, colors().faint),
        };
        let has_job = step.job.is_some();
        column()
            .gap_1()
            .p_2()
            .rounded(px(6.))
            .border_1()
            .border_color(rgb(colors().border))
            .child(
                row()
                    .gap_2()
                    .child(icon(symbol, color))
                    .child(
                        div()
                            .font_family(fonts().code.clone())
                            .text_size(px(13.))
                            .child(step.id.clone()),
                    )
                    .when_some(version, |s, version| {
                        s.child(badge(tr!("Version {0}", version), colors().faint))
                    })
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_size(px(13.))
                            .child(step.text.clone()),
                    )
                    .when(step.run.is_some(), |s| {
                        s.child(
                            button(
                                ("history-step-open", index),
                                if open {
                                    I::ChevronDown
                                } else {
                                    I::ChevronRight
                                },
                                tr!("Its run"),
                            )
                            .aria_selected(open)
                            .on_click(cx.listener(
                                move |this, _, _, cx| {
                                    if let Some(Detail::Chain { open, .. }) = this.detail.as_mut() {
                                        *open = if *open == Some(index) {
                                            None
                                        } else {
                                            Some(index)
                                        };
                                    }
                                    this.log = None;
                                    cx.notify();
                                },
                            )),
                        )
                    })
                    .when(has_job, |s| {
                        s.child(
                            button(
                                ("history-step-again", index),
                                I::RotateCcw,
                                tr!("Run this step again…"),
                            )
                            .on_click(
                                cx.listener(move |this, _, _, cx| this.run_step_again(index, cx)),
                            ),
                        )
                    }),
            )
            .when(open, |s| {
                let outputs = match &self.detail {
                    Some(Detail::Chain { stored, .. }) => {
                        stored.get(&step.id).cloned().unwrap_or_default()
                    }
                    _ => Vec::new(),
                };
                s.when_some(step.run.as_ref(), |s, run| {
                    s.child(self.run_body(run, &outputs, cx))
                })
            })
    }
}

/// A line of a run's account, coloured by what it says, with its icon.
pub(super) fn line_element(line: &Line) -> Div {
    let tone = match line.level {
        Level::Info => Tone::Info,
        Level::Success => Tone::Success,
        Level::Warning => Tone::Warning,
        Level::Error => Tone::Error,
    };
    feedback_line(&Feedback::new(tone, line.text.clone())).text_size(px(13.))
}

fn state_mark(state: RunState) -> (I, u32) {
    match state {
        RunState::Running => (I::LoaderCircle, colors().mint),
        RunState::Succeeded => (I::CircleCheck, colors().mint),
        RunState::Failed => (I::CircleX, colors().red),
        RunState::Cancelled => (I::Ban, colors().amber),
        RunState::Interrupted => (I::TriangleAlert, colors().amber),
    }
}

/// The states the global list filters by.
pub fn state_filters() -> [(Option<RunState>, &'static str); 6] {
    [
        (None, tr!("Every state")),
        (Some(RunState::Running), tr!("Running")),
        (Some(RunState::Succeeded), tr!("Succeeded")),
        (Some(RunState::Failed), tr!("Failed")),
        (Some(RunState::Cancelled), tr!("Stopped")),
        (Some(RunState::Interrupted), tr!("Interrupted")),
    ]
}

/// The state filter's menu.
pub fn state_menu(view: WeakEntity<HistoryList>, current: Option<RunState>) -> impl IntoElement {
    let shown = state_filters()
        .into_iter()
        .find(|(state, _)| *state == current)
        .map_or(tr!("Every state"), |(_, label)| label);
    Button::new("history-state")
        .ghost()
        .small()
        .label(shown)
        .dropdown_caret(true)
        .accessibility_label(tr!("Show runs in this state"))
        .dropdown_menu(move |mut menu, _, _| {
            for (state, label) in state_filters() {
                let view = view.clone();
                menu = menu.item(
                    PopupMenuItem::new(label)
                        .checked(state == current)
                        .on_click(move |_, _, cx| {
                            let _ = view.update(cx, |list, cx| list.set_state(state, cx));
                        }),
                );
            }
            menu
        })
}

impl Render for HistoryList {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        crate::app::probe::rendered("HistoryList");
        let count = self.entries.len() + usize::from(self.more);
        row()
            .id("history")
            .track_focus(&self.focus)
            .size_full()
            .items_stretch()
            .gap_0()
            .child(
                column()
                    .w(px(380.))
                    .flex_none()
                    .h_full()
                    .border_r_1()
                    .border_color(rgb(colors().border))
                    .child(
                        column()
                            .flex_none()
                            .p_2()
                            .gap_1()
                            .child(label(words::refused_note()))
                            .child(label(tr!(
                                "Runs are kept 90 days; stored output values 7 days and program logs 7 days."
                            )))
                            .when(self.loading, |s| s.child(label(tr!("Reading the history…"))))
                            .when_some(self.error.clone(), |s, error| {
                                s.child(
                                    div()
                                        .id("history-error")
                                        .role(Role::Alert)
                                        .child(feedback_line(&Feedback::new(Tone::Error, error))),
                                )
                            }),
                    )
                    .map(|s| {
                        if self.entries.is_empty() && !self.loading {
                            s.child(div().p_3().child(label(tr!("No runs yet."))))
                        } else {
                            s.child(
                                div()
                                    .id("history-entries")
                                    .role(Role::List)
                                    .aria_label(tr!("Runs"))
                                    .relative()
                                    .flex_1()
                                    .min_h_0()
                                    .px_1()
                                    .child(
                                        uniform_list(
                                            "history-entry-rows",
                                            count,
                                            cx.processor(|this, range: Range<usize>, _, cx| {
                                                range
                                                    .filter_map(|index| this.entry_row(index, cx))
                                                    .collect::<Vec<_>>()
                                            }),
                                        )
                                        .size_full()
                                        .track_scroll(&self.scroll),
                                    )
                                    .vertical_scrollbar(&self.scroll),
                            )
                        }
                    }),
            )
            .child(
                column()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .when_some(self.notice.clone(), |s, notice| {
                        s.child(
                            div()
                                .id("history-notice")
                                .role(Role::Status)
                                .px_3()
                                .pt_2()
                                .child(feedback_line(&notice)),
                        )
                    })
                    .child(self.detail_panel(cx)),
            )
    }
}
