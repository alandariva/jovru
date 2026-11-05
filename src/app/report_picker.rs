//! The dialog that adds a query tab's SQL to a saved report
//! ([ADR-0025](../../docs/adr/0025-report-panels.md)): it lists the saved
//! reports, with a search, and the one chosen gets a new panel with the SQL
//! on the tab's connection and in its database. Nothing is stored until the
//! report is saved.
use super::*;
use jykre::reports::ConnectionRef;

/// The open dialog and what it adds.
pub(super) struct ReportPicker {
    sql: String,
    context: SessionContext,
    connection: ConnectionRef,
    search: Entity<InputState>,
    focus: FocusHandle,
    scroll: UniformListScrollHandle,
    /// The identities of the reports on the connection that match the
    /// search, in the library's order.
    shown: Vec<u64>,
    /// The search takes the keyboard on the next frame.
    pub(super) needs_focus: bool,
    _search: Subscription,
}

impl ReportPicker {
    pub(super) fn search(&self) -> &Entity<InputState> {
        &self.search
    }
}

impl AppView {
    /// Opens the dialog for `sql`, to run in `context` of `connection`.
    pub(super) fn open_report_picker(
        &mut self,
        connection: &str,
        sql: String,
        context: SessionContext,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.dialog_open() || self.password_prompt.is_some() || self.connection_open {
            return;
        }
        self.command_open = false;
        let id = self
            .connections
            .iter()
            .find(|candidate| candidate.name == connection)
            .and_then(|candidate| candidate.profile.as_ref().map(|profile| profile.id));
        let search =
            cx.new(|cx| InputState::new(window, cx).placeholder(tr!("Search the saved reports")));
        let subscription = cx.subscribe_in(
            &search,
            window,
            |this, _, event: &InputEvent, window, cx| match event {
                InputEvent::Change => this.filter_report_picker(cx),
                InputEvent::PressEnter { .. } => {
                    let first = this
                        .report_picker
                        .as_ref()
                        .and_then(|picker| picker.shown.first().copied());
                    if let Some(id) = first {
                        this.pick_report(id, window, cx);
                    }
                }
                _ => {}
            },
        );
        self.report_picker = Some(ReportPicker {
            sql,
            context,
            connection: ConnectionRef {
                id,
                name: connection.to_owned(),
            },
            search,
            focus: cx.focus_handle(),
            scroll: UniformListScrollHandle::new(),
            shown: Vec::new(),
            needs_focus: true,
            _search: subscription,
        });
        self.filter_report_picker(cx);
    }

    /// The reports of the dialog's connection that match its search.
    fn filter_report_picker(&mut self, cx: &mut Context<Self>) {
        let Some(picker) = &self.report_picker else {
            return;
        };
        let query = picker.search.read(cx).value().to_string();
        let shown = self
            .reports
            .reports
            .iter()
            .filter(|report| report.matches(&query))
            .map(|report| report.id)
            .collect();
        if let Some(picker) = &mut self.report_picker {
            picker.shown = shown;
        }
        cx.notify();
    }

    /// Closes the dialog; nothing is added.
    pub(super) fn close_report_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.report_picker.take().is_some() {
            self.focus_active_editor(window, cx);
            cx.notify();
        }
    }

    fn pick_report(&mut self, id: u64, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(picker) = self.report_picker.take() {
            self.add_to_report(
                id,
                picker.connection,
                picker.sql,
                picker.context,
                window,
                cx,
            );
        }
    }

    fn picker_entry(&self, position: usize, cx: &mut Context<Self>) -> Option<Stateful<Div>> {
        let id = *self.report_picker.as_ref()?.shown.get(position)?;
        let report = self.reports.get(id)?;
        let detail = if report.panels.len() == 1 {
            tr!("{0} · 1 panel", report.connections_label())
        } else {
            tr!(
                "{0} · {1} panels",
                report.connections_label(),
                report.panels.len()
            )
        };
        Some(
            column()
                .id(("picker-entry", position))
                .role(Role::Button)
                .aria_label(tr!("Add to the report {0}", report.title))
                .focusable()
                .tab_index(0)
                .focus_visible(|s| s.bg(rgb(HOVER)).border_color(rgb(MINT)))
                .border_1()
                .border_color(transparent_black())
                .w_full()
                .h(px(LIBRARY_ROW_HEIGHT))
                .px_2()
                .justify_center()
                .gap_1()
                .rounded(px(6.))
                .cursor_pointer()
                .hover(|s| s.bg(rgb(HOVER)))
                .child(
                    row().min_w_0().child(icon(I::FileChartColumn, MINT)).child(
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
                .on_click(cx.listener(move |this, _, window, cx| this.pick_report(id, window, cx))),
        )
    }

    /// The modal dialog over the window.
    pub(super) fn report_picker_dialog(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(picker) = &self.report_picker else {
            return div().into_any_element();
        };
        let title = tr!("Add to a report");
        let empty = picker.shown.is_empty();
        let no_reports = self.reports.reports.is_empty();
        self.overlay("picker-backdrop", cx)
            .child(
                column()
                    .id("report-picker")
                    .role(Role::Dialog)
                    .aria_label(title)
                    .w(px(480.))
                    .mt(px(120.))
                    .p_5()
                    .gap_3()
                    .rounded(px(10.))
                    .bg(rgb(PANEL))
                    .border_1()
                    .border_color(rgb(0x424c57))
                    .shadow_lg()
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .child(
                        div()
                            .text_size(px(16.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(title),
                    )
                    .child(label(tr!(
                        "The query becomes a new panel of the report, on this tab's connection and database. Save the report to keep it."
                    )))
                    .child(
                        Input::new(&picker.search)
                            .prefix(I::Search)
                            .w_full()
                            .aria_label(tr!("Search the saved reports")),
                    )
                    .child(if empty {
                        div()
                            .p_3()
                            .text_size(px(14.))
                            .text_color(rgb(MUTED))
                            .child(if no_reports {
                                tr!(
                                    "There is no saved report yet. Save the query as a new report instead."
                                )
                            } else {
                                tr!("Nothing matches the search.")
                            })
                            .into_any_element()
                    } else {
                        div()
                            .relative()
                            .h(px(280.))
                            .child(
                                uniform_list(
                                    "picker-entries",
                                    picker.shown.len(),
                                    cx.processor(move |this, range: Range<usize>, _, cx| {
                                        range
                                            .filter_map(|position| this.picker_entry(position, cx))
                                            .collect::<Vec<_>>()
                                    }),
                                )
                                .size_full()
                                .track_scroll(&picker.scroll),
                            )
                            .vertical_scrollbar(&picker.scroll)
                            .into_any_element()
                    })
                    .child(
                        row().justify_end().gap_2().child(
                            button("picker-cancel", I::X, tr!("Cancel")).on_click(cx.listener(
                                |this, _, window, cx| this.close_report_picker(window, cx),
                            )),
                        ),
                    )
                    // Tab stays inside the dialog while it is open.
                    .focus_trap("picker-focus-trap", &picker.focus),
            )
            .into_any_element()
    }
}
