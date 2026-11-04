//! The Reports section of the rail and the report tabs
//! ([ADR-0024](../../docs/adr/0024-reports.md)): the saved reports, their
//! list, and the tabs that edit and run them.
//!
//! Report tabs are tracked apart from query tabs (`report_tabs`), live in
//! their connection's tab group and are not reopened after a restart. The
//! library is written to `config.db` under its own setting, never with a
//! result.
use super::report_view::{ReportEvent, ReportMode, ReportTarget, ReportView};
use super::*;
use jykre::reports::{ConnectionRef, Report, ReportLibrary};

/// Changes are written once they settle.
const SAVE_DELAY: std::time::Duration = std::time::Duration::from_millis(500);

/// A report's tab and what listens to it.
pub(super) struct ReportTab {
    pub(super) view: Entity<ReportView>,
    _subscriptions: Vec<Subscription>,
}

/// Seconds since the epoch.
fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

impl AppView {
    /// Reads the stored library. A value this version cannot read is kept
    /// untouched: nothing is written over it until the problem is solved.
    pub(super) fn load_reports(
        &mut self,
        value: DriverResult<Option<serde_json::Value>>,
        cx: &App,
    ) -> Option<String> {
        let problem = match value.map(ReportLibrary::from_value) {
            Ok(Ok(library)) => {
                self.reports = library;
                self.persist_reports = true;
                None
            }
            Ok(Err(unreadable)) => Some(tr!(
                "{0}. They are kept as they are and nothing is saved over them.",
                unreadable
            )),
            Err(error) => Some(tr!("Could not read the saved reports: {0}", error)),
        };
        self.filter_library(cx);
        problem
    }

    /// The connection a report names: by its identity, then by its name.
    fn report_connection(&self, connection: &ConnectionRef) -> Option<usize> {
        connection
            .id
            .and_then(|id| {
                self.connections.iter().position(|candidate| {
                    candidate
                        .profile
                        .as_ref()
                        .is_some_and(|profile| profile.id == id)
                })
            })
            .or_else(|| {
                self.connections
                    .iter()
                    .position(|candidate| candidate.name == connection.name)
            })
    }

    fn report_target(&self, index: usize) -> Option<ReportTarget> {
        let connection = self.connections.get(index)?;
        Some(ReportTarget {
            name: connection.name.clone(),
            id: connection.profile.as_ref().map(|profile| profile.id),
            input: connection.input.clone(),
            row_limit: connection.row_limit,
            environment: connection.appearance.environment,
        })
    }

    /// Hands every report tab its connection and the ones it can switch to.
    pub(super) fn sync_reports(&self, cx: &mut Context<Self>) {
        let names: Vec<String> = self.connections.iter().map(|c| c.name.clone()).collect();
        let tabs: Vec<Entity<ReportView>> = self
            .report_tabs
            .values()
            .map(|tab| tab.view.clone())
            .collect();
        for view in tabs {
            let connection = view.read(cx).connection();
            let target = self
                .report_connection(&connection)
                .and_then(|index| self.report_target(index));
            let names = names.clone();
            view.update(cx, |view, cx| {
                view.set_connection_choices(names, cx);
                view.set_target(target, cx);
            });
        }
    }

    pub(super) fn report_tab_of(&self, view: &Entity<ReportView>) -> Option<TabId> {
        self.report_tabs
            .iter()
            .find(|(_, tab)| tab.view == *view)
            .map(|(id, _)| *id)
    }

    /// **New report**: a draft on the connection of the tab on screen, or
    /// the first one.
    pub(super) fn new_report(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let active = self.tabs.read(cx).active_tab();
        let name = active
            .and_then(|tab| {
                self.panels
                    .get(&tab)
                    .map(|panel| panel.read(cx).connection_name().to_owned())
                    .or_else(|| {
                        self.report_tabs
                            .get(&tab)
                            .map(|tab| tab.view.read(cx).connection_name().to_owned())
                    })
            })
            .filter(|name| self.connections.iter().any(|c| c.name == *name))
            .or_else(|| self.connections.first().map(|c| c.name.clone()))
            .unwrap_or_default();
        let id = self
            .connections
            .iter()
            .find(|c| c.name == name)
            .and_then(|c| c.profile.as_ref().map(|profile| profile.id));
        let report = Report::draft(
            tr!("Report {0}", self.reports.next_id),
            ConnectionRef { id, name },
            "",
        );
        self.open_report_tab(report, ReportMode::Edit, window, cx);
    }

    /// **Save as report** from a query tab: a draft with its SQL, saved only
    /// when the user saves it.
    pub(super) fn report_from_query(
        &mut self,
        title: &str,
        connection: &str,
        sql: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let id = self
            .connections
            .iter()
            .find(|c| c.name == connection)
            .and_then(|c| c.profile.as_ref().map(|profile| profile.id));
        let report = Report::draft(
            title,
            ConnectionRef {
                id,
                name: connection.to_owned(),
            },
            sql,
        );
        self.open_report_tab(report, ReportMode::Edit, window, cx);
    }

    /// Opens a saved report: its tab when one is open, otherwise a new one.
    pub(super) fn open_report(&mut self, id: u64, window: &mut Window, cx: &mut Context<Self>) {
        let open = self
            .report_tabs
            .iter()
            .find(|(_, tab)| tab.view.read(cx).saved_id() == Some(id))
            .map(|(tab, _)| *tab);
        if let Some(tab) = open {
            self.show_tab(tab, window, cx);
            return;
        }
        if let Some(report) = self.reports.get(id).cloned() {
            self.open_report_tab(report, ReportMode::View, window, cx);
        }
    }

    fn open_report_tab(
        &mut self,
        report: Report,
        mode: ReportMode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<TabId> {
        let index = self.report_connection(&report.connection);
        let group = match index {
            Some(index) => self.ensure_group(index, cx),
            None => self.group_for(&report.connection.name, cx),
        };
        let title = if report.title.trim().is_empty() {
            tr!("Untitled report").to_owned()
        } else {
            report.title.clone()
        };
        let view = cx.new(|cx| ReportView::new(report, mode, window, cx));
        let target = index.and_then(|index| self.report_target(index));
        let names: Vec<String> = self.connections.iter().map(|c| c.name.clone()).collect();
        let catalogs = self.sql_catalogs.clone();
        view.update(cx, |view, cx| {
            view.set_catalogs(catalogs, cx);
            view.set_connection_choices(names, cx);
            view.set_target(target, cx);
        });
        let events = cx.subscribe_in(
            &view,
            window,
            |this, view, event: &ReportEvent, window, cx| {
                this.report_event(view, event, window, cx);
            },
        );
        let status = cx.observe(&view, |this, view, cx| {
            let status = view.read(cx).tab_status();
            if let Some(tab) = this.report_tab_of(&view) {
                let _ = this
                    .tabs
                    .update(cx, |tabs, cx| tabs.set_tab_status(tab, status, cx));
            }
        });
        let Ok(tab) = self
            .tabs
            .update(cx, |tabs, cx| tabs.add_tab(group, title, view.clone(), cx))
        else {
            tracing::error!("could not add a report tab: its group does not exist");
            return None;
        };
        self.report_tabs.insert(
            tab,
            ReportTab {
                view: view.clone(),
                _subscriptions: vec![events, status],
            },
        );
        view.update(cx, |view, cx| view.focus(window, cx));
        cx.notify();
        Some(tab)
    }

    fn report_event(
        &mut self,
        view: &Entity<ReportView>,
        event: &ReportEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            ReportEvent::Changed => {}
            ReportEvent::Save(report) => {
                let id = self.reports.save((**report).clone(), now());
                if let Some(stored) = self.reports.get(id).cloned() {
                    let title = stored.title.clone();
                    view.update(cx, |view, cx| view.saved(stored, cx));
                    if let Some(tab) = self.report_tab_of(view) {
                        let _ = self
                            .tabs
                            .update(cx, |tabs, cx| tabs.rename_tab(tab, title, cx));
                    }
                }
                self.filter_library(cx);
                self.save_reports_soon(cx);
            }
            ReportEvent::PasswordRequired => {
                let connection = view.read(cx).connection();
                if let Some(index) = self.report_connection(&connection) {
                    let resume = view.clone();
                    let cancel = view.clone();
                    self.ask_password(
                        index,
                        Box::new(move |_, _, cx| {
                            resume.update(cx, |view, cx| view.resume_pending(cx))
                        }),
                        Box::new(move |_, cx| cancel.update(cx, |view, _| view.cancel_pending())),
                        cx,
                    );
                }
            }
            ReportEvent::ConfirmRisk {
                connection,
                description,
            } => {
                let view = view.downgrade();
                confirm(
                    window,
                    cx,
                    Confirmation {
                        title: tr!("Run on {0}?", connection),
                        description: description.clone(),
                        ok: tr!("Run"),
                        cancel: tr!("Cancel"),
                        focus: ConfirmFocus::Dialog,
                    },
                    move |_, cx| {
                        let _ = view.update(cx, |view, cx| view.confirm_risk(cx));
                    },
                );
            }
            ReportEvent::ConnectionRequested(name) => {
                if let Some(index) = self.connections.iter().position(|c| c.name == *name)
                    && let Some(tab) = self.report_tab_of(view)
                {
                    let group = self.ensure_group(index, cx);
                    let _ = self
                        .tabs
                        .update(cx, |tabs, cx| tabs.move_tab(tab, group, usize::MAX, cx));
                    let target = self.report_target(index);
                    view.update(cx, |view, cx| view.set_target(target, cx));
                }
            }
        }
        cx.notify();
    }

    /// Closes a report tab, asking first when it is running (closing stops
    /// it) or its definition is not saved (closing loses it).
    pub(super) fn request_report_close(
        &mut self,
        tab: TabId,
        busy: bool,
        unsaved: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let tabs = self.tabs.downgrade();
        let close = move |window: &mut Window, cx: &mut App| {
            let _ = tabs.update(cx, |tabs, cx| {
                let _ = tabs.close_tab(tab, cx);
                tabs.focus_active(window, cx);
            });
        };
        let (title, description, ok) = match (busy, unsaved) {
            (false, false) => {
                close(window, cx);
                return;
            }
            (true, false) => (
                tr!("The report is running in this tab."),
                tr!("Closing the tab stops it."),
                tr!("Close and stop"),
            ),
            (false, true) => (
                tr!("Close the report without saving it?"),
                tr!("Its changes are not in the reports, so closing the tab loses them."),
                tr!("Close without saving"),
            ),
            (true, true) => (
                tr!("The report is running in this tab."),
                tr!(
                    "Closing the tab stops it and loses its changes, which are not in the reports."
                ),
                tr!("Close and stop"),
            ),
        };
        confirm(
            window,
            cx,
            Confirmation {
                title: title.to_owned(),
                description: description.to_owned(),
                ok,
                cancel: tr!("Keep the tab"),
                focus: ConfirmFocus::Dialog,
            },
            move |window, cx| close(window, cx),
        );
    }

    /// A report tab moved to another group runs on that group's connection.
    pub(super) fn report_moved(&mut self, tab: TabId, group: TabGroupId, cx: &mut Context<Self>) {
        let Some(view) = self.report_tabs.get(&tab).map(|tab| tab.view.clone()) else {
            return;
        };
        let target = self
            .connections
            .iter()
            .position(|connection| connection.group == group)
            .and_then(|index| self.report_target(index));
        view.update(cx, |view, cx| view.set_target(target, cx));
    }

    /// Removes a saved report after the user confirms. A tab that has it
    /// open keeps it as a draft.
    pub(super) fn remove_report(&mut self, id: u64, window: &mut Window, cx: &mut Context<Self>) {
        let Some(report) = self.reports.get(id) else {
            return;
        };
        let view = cx.entity().downgrade();
        confirm(
            window,
            cx,
            Confirmation {
                title: tr!("Remove the report {0}?", report.title),
                description: tr!(
                    "It leaves the reports. A tab that has it open keeps it, which is then not saved."
                )
                .to_string(),
                ok: tr!("Remove"),
                cancel: tr!("Keep it"),
                focus: ConfirmFocus::Dialog,
            },
            move |_, cx| {
                let _ = view.update(cx, |this, cx| {
                    this.reports.remove(id);
                    let tabs: Vec<_> = this.report_tabs.values().map(|tab| tab.view.clone()).collect();
                    for view in tabs {
                        view.update(cx, |view, cx| view.forget_saved(id, cx));
                    }
                    this.filter_library(cx);
                    this.save_reports_soon(cx);
                    cx.notify();
                });
            },
        );
    }

    /// Opens an unsaved copy of a report.
    fn duplicate_report(&mut self, id: u64, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(report) = self.reports.get(id) {
            let copy = report.duplicate(tr!("{0} (copy)", report.title));
            self.open_report_tab(copy, ReportMode::Edit, window, cx);
        }
    }

    /// One entry of the reports list: its title, connection and the start
    /// of its SQL, with buttons to copy and remove it.
    pub(super) fn report_entry(
        &self,
        position: usize,
        cx: &mut Context<Self>,
    ) -> Option<Stateful<Div>> {
        let index = *self.library_shown.get(position)?;
        let report = self.reports.reports.get(index)?;
        let id = report.id;
        let detail = format!("{} · {}", report.connection.name, report.preview(2));
        let symbol = match report.chart.as_ref().map(|chart| chart.kind) {
            Some(jykre::reports::ChartKind::Line) => I::ChartLine,
            Some(jykre::reports::ChartKind::Pie) => I::ChartPie,
            Some(jykre::reports::ChartKind::Bar) => I::ChartColumn,
            None => I::FileChartColumn,
        };
        Some(
            row()
                .id(("report-entry", position))
                .w_full()
                .h(px(LIBRARY_ROW_HEIGHT))
                .gap_1()
                .child(
                    column()
                        .id("open")
                        .role(Role::Button)
                        .aria_label(tr!("Open the report {0}", report.title))
                        .focusable()
                        .tab_index(0)
                        .focus_visible(|s| s.bg(rgb(HOVER)))
                        .flex_1()
                        .min_w_0()
                        .h_full()
                        .px_2()
                        .justify_center()
                        .gap_1()
                        .rounded(px(6.))
                        .cursor_pointer()
                        .hover(|s| s.bg(rgb(HOVER)))
                        .child(
                            row().min_w_0().child(icon(symbol, MINT)).child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .text_size(px(14.))
                                    .child(report.title.clone()),
                            ),
                        )
                        .child(
                            div()
                                .pl_6()
                                .min_w_0()
                                .truncate()
                                .text_size(px(12.))
                                .text_color(rgb(FAINT))
                                .child(detail),
                        )
                        .on_click(
                            cx.listener(move |this, _, window, cx| {
                                this.open_report(id, window, cx)
                            }),
                        ),
                )
                .child(
                    icon_button("duplicate", I::Copy, tr!("Duplicate the report")).on_click(
                        cx.listener(move |this, _, window, cx| {
                            this.duplicate_report(id, window, cx)
                        }),
                    ),
                )
                .child(
                    icon_button("remove", I::Trash, tr!("Remove from the reports")).on_click(
                        cx.listener(move |this, _, window, cx| this.remove_report(id, window, cx)),
                    ),
                ),
        )
    }

    /// Adds the Reports section to the sidebar's column `s`, as the saved
    /// queries are added, so its rows take the sidebar's width.
    pub(super) fn reports_list(&self, s: Div, cx: &mut Context<Self>) -> Div {
        let empty = self.reports.reports.is_empty();
        s.child(label(tr!("Reports built from queries")))
            .when(!empty, |s| {
                s.child(
                    Input::new(&self.library_search)
                        .small()
                        .prefix(I::Search)
                        .w_full()
                        .aria_label(tr!("Search the reports")),
                )
            })
            .map(|s| {
                if empty {
                    s.child(
                        div()
                            .p_3()
                            .text_size(px(14.))
                            .text_color(rgb(MUTED))
                            .child(tr!(
                                "Create a report with the + button, or save a query tab as a report. Use :name in the SQL for values to fill in before each run."
                            )),
                    )
                } else if self.library_shown.is_empty() {
                    s.child(
                        div()
                            .p_3()
                            .text_size(px(14.))
                            .text_color(rgb(MUTED))
                            .child(tr!("Nothing matches the search.")),
                    )
                } else {
                    s.child(
                        div()
                            .relative()
                            .flex_1()
                            .min_h_0()
                            .child(
                                uniform_list(
                                    "report-entries",
                                    self.library_shown.len(),
                                    cx.processor(move |this, range: Range<usize>, _, cx| {
                                        range
                                            .filter_map(|position| this.report_entry(position, cx))
                                            .collect::<Vec<_>>()
                                    }),
                                )
                                .size_full()
                                .track_scroll(&self.library_scroll),
                            )
                            .vertical_scrollbar(&self.library_scroll),
                    )
                }
            })
    }

    /// Writes the library once changes settle. Nothing is written over a
    /// value that could not be read, nor in the demo.
    pub(super) fn save_reports_soon(&mut self, cx: &mut Context<Self>) {
        if !self.persist_reports || super::demo::enabled() {
            return;
        }
        self.reports_revision += 1;
        let revision = self.reports_revision;
        cx.spawn(async move |weak, cx| {
            cx.background_executor().timer(SAVE_DELAY).await;
            let Ok(Some(value)) = weak.update(cx, |this, _| {
                (this.reports_revision == revision).then(|| this.reports.to_value())
            }) else {
                return;
            };
            if let Err(error) = write_reports(value).await {
                let _ = weak.update(cx, |this, cx| {
                    this.app_notice = Some(Feedback::new(
                        Tone::Error,
                        tr!("Could not save the reports: {0}", error),
                    ));
                    cx.notify();
                });
            }
        })
        .detach();
    }

    /// Writes the library now, for quitting.
    pub(super) fn save_reports_now(&self) -> impl std::future::Future<Output = ()> + use<> {
        let value =
            (self.persist_reports && !super::demo::enabled()).then(|| self.reports.to_value());
        async move {
            if let Some(value) = value
                && let Err(error) = write_reports(value).await
            {
                tracing::error!(error_code = ?error.code, "could not save the reports on quit");
            }
        }
    }
}

async fn write_reports(value: serde_json::Value) -> DriverResult<()> {
    let dir = connection_store::data_dir();
    backend::run(async move {
        let repository = ConfigRepository::open(&dir).await?;
        let result = repository
            .set_setting(jykre::reports::SETTINGS_KEY, &value)
            .await;
        repository.close().await;
        result
    })
    .await
}
