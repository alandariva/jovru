use super::demo::{self, OPTIMIZED_SQL, ResultData, SALES, SALES_SQL, sample_result};
use super::layout::{ASSISTANT_WIDTH, EDITOR_MIN_WIDTH, splitter};
use super::ui::*;
use gpui_kit::assets::IconName as I;
use gpui_kit::component::input::{Editor, EditorState, Input, InputEvent, InputState};
use gpui_kit::component::menu::{ContextMenuExt as _, PopupMenuItem};
use gpui_kit::component::resizable::{ResizableState, h_resizable, resizable_panel, v_resizable};
use gpui_kit::component::scroll::ScrollableElement;
use gpui_kit::component::{ActiveTheme, Sizable, tooltip::Tooltip};
use gpui_kit::{prelude::FluentBuilder as _, *};
use jykre::tr;
use jykre::{
    backend,
    cell_format::{self, ValueFamily},
    drivers::*,
};
use std::{cell::RefCell, ops::Range, rc::Rc};

#[derive(Clone, Copy, PartialEq)]
enum ResultView {
    Table,
    Chart,
    Messages,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
enum QueryStatus {
    #[default]
    Ready,
    Running,
    Success,
    Error,
    Cancelled,
}

/// A rectangular block of result cells, from the cell clicked first (anchor)
/// to the one reached with Shift (focus). Rows are indices into the result
/// data, so sorting or filtering the view does not change what is selected.
#[derive(Clone, Copy, Debug, PartialEq)]
struct CellSelection {
    anchor: (usize, usize),
    focus: (usize, usize),
}

/// Position of each selected cell in the displayed grid: rows as positions in
/// `visible`, columns as indices. `None` when the selection is hidden.
fn selection_block(
    selection: CellSelection,
    visible: &[usize],
) -> Option<(
    std::ops::RangeInclusive<usize>,
    std::ops::RangeInclusive<usize>,
)> {
    let anchor = visible.iter().position(|row| *row == selection.anchor.0)?;
    let focus = visible.iter().position(|row| *row == selection.focus.0)?;
    let (left, right) = (selection.anchor.1, selection.focus.1);
    Some((
        anchor.min(focus)..=anchor.max(focus),
        left.min(right)..=left.max(right),
    ))
}

/// What the grid shows for one result, filter, sort and number format. It is
/// rebuilt only when one of them changes, not on every frame, so scrolling,
/// hovering and selecting cost only the rows on screen.
struct GridView {
    key: GridKey,
    /// Data rows in display order, after the open filter and the sort.
    rows: Vec<usize>,
    widths: Vec<f32>,
    families: Vec<ValueFamily>,
}

#[derive(Clone, Debug, PartialEq)]
struct GridKey {
    /// Changes with every new result.
    result: u64,
    /// Lowercase text of the open filter; empty when the bar is closed.
    filter: String,
    descending: bool,
    numbers_pt_br: bool,
}

impl GridView {
    fn new(result: &ResultData, key: GridKey) -> Self {
        let mut rows: Vec<_> = result
            .rows
            .iter()
            .enumerate()
            .filter(|(_, cells)| {
                key.filter.is_empty()
                    || cells
                        .iter()
                        .any(|cell| cell.to_lowercase().contains(&key.filter))
            })
            .map(|(index, _)| index)
            .collect();
        if !key.descending {
            rows.reverse();
        }
        let families: Vec<_> = result
            .columns
            .iter()
            .map(|(_, kind)| cell_format::family(kind))
            .collect();
        // Widths follow the first rows only, so a long result sizes as fast
        // as a short one.
        let widths = result
            .columns
            .iter()
            .zip(&families)
            .enumerate()
            .map(|(column, ((name, kind), family))| {
                let content = (0..result.rows.len().min(200))
                    .map(|row| {
                        cell_format::display(result.value(row, column), *family, key.numbers_pt_br)
                            .chars()
                            .count()
                    })
                    .max()
                    .unwrap_or(0);
                let header = name.chars().count() + 3 + kind.chars().count();
                ((content.max(header) as f32 * 8.) + 32.).clamp(110., 360.)
            })
            .collect();
        Self {
            key,
            rows,
            widths,
            families,
        }
    }
}

/// The last [`GridView`], reused while its key is unchanged.
#[derive(Default)]
struct GridCache(RefCell<Option<Rc<GridView>>>);

impl GridCache {
    fn get(&self, result: &ResultData, key: GridKey) -> Rc<GridView> {
        let mut cached = self.0.borrow_mut();
        match cached.as_ref() {
            Some(grid) if grid.key == key => grid.clone(),
            _ => cached.insert(Rc::new(GridView::new(result, key))).clone(),
        }
    }
}

struct Message {
    user: bool,
    text: String,
    sql: Option<&'static str>,
}

pub struct Workspace {
    input: Option<ResolvedConnection>,
    cancellation: CancellationToken,
    elapsed_ms: u128,
    rows_affected: Option<u64>,
    numbers_pt_br: bool,
    editor: Entity<EditorState>,
    assistant_split: Entity<ResizableState>,
    query_split: Entity<ResizableState>,
    prompt: Entity<InputState>,
    connection: SharedString,
    dialect: SharedString,
    database: String,
    connection_color: u32,
    title: SharedString,
    result: ResultData,
    /// Bumped by [`Workspace::set_result`] so the grid knows to rebuild.
    result_version: u64,
    grid: GridCache,
    result_view: ResultView,
    assistant_open: bool,
    messages: Vec<Message>,
    executed: bool,
    running: bool,
    executions: usize,
    query_statuses: Vec<QueryStatus>,
    /// Normalized text of each statement when its status was recorded. A
    /// status only survives edits while its statement is unchanged.
    status_sql: Vec<String>,
    execution_id: usize,
    editor_sql: String,
    execution_error: Option<String>,
    cancelled: bool,
    /// SQL as last saved to the library, if this tab is saved.
    saved_sql: Option<String>,
    notice: Option<Feedback>,
    /// The last statement hit the row limit.
    limited: bool,
    selection: Option<CellSelection>,
    grid_focus: FocusHandle,
    descending: bool,
    filter_open: bool,
    filter: Entity<InputState>,
    chat_scroll: ScrollHandle,
    result_scroll: UniformListScrollHandle,
    _subscriptions: Vec<Subscription>,
}

impl Workspace {
    pub fn new(
        title: &str,
        connection: &str,
        sql: &str,
        assistant_split: Entity<ResizableState>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let editor = cx.new(|cx| {
            EditorState::new(window, cx)
                .language("sql")
                .line_number(true)
                .default_value(sql)
        });
        editor.update(cx, |editor, cx| {
            editor.set_highlighter_factory(
                std::rc::Rc::new(|_| Some(Box::new(super::sql::SqlHighlighter::default()))),
                cx,
            )
        });
        let prompt =
            cx.new(|cx| InputState::new(window, cx).placeholder(tr!("Ask about your data…")));
        let filter = cx.new(|cx| InputState::new(window, cx).placeholder(tr!("Filter results…")));
        let prompt_subscription = cx.subscribe_in(
            &prompt,
            window,
            |this, _, event: &InputEvent, window, cx| {
                if let InputEvent::PressEnter { .. } = event {
                    this.submit_prompt(window, cx);
                }
            },
        );
        let filter_subscription = cx.subscribe(&filter, |this, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                this.selection = None;
            }
            cx.notify()
        });
        let resize_subscription = cx.observe(&assistant_split, |_, _, cx| cx.notify());
        let query_split = cx.new(|_| ResizableState::default());
        let query_resize = cx.observe(&query_split, |_, _, cx| cx.notify());
        // Editing the SQL never discards results, messages or a running
        // execution. Only the gutter status of statements that changed resets.
        let editor_observation = cx.observe(&editor, |this, editor, cx| {
            let sql = editor.read(cx).text().to_string();
            if this.editor_sql != sql {
                this.editor_sql = sql;
                this.sync_statement_statuses();
            }
            cx.notify();
        });
        Self {
            input: None,
            cancellation: CancellationToken::new(),
            elapsed_ms: 0,
            rows_affected: None,
            numbers_pt_br: false,
            editor,
            assistant_split,
            query_split,
            prompt,
            filter,
            connection: connection.to_string().into(),
            dialect: demo::dialect(connection).into(),
            database: if connection.contains("Databricks") {
                "analytics".into()
            } else if connection.contains("MySQL") {
                "finance".into()
            } else {
                "commerce".into()
            },
            connection_color: if connection.contains("Databricks") {
                BLUE
            } else if connection.contains("MySQL") {
                0xd995a7
            } else {
                MINT
            },
            title: title.to_string().into(),
            result: sample_result(sql),
            result_version: 0,
            grid: GridCache::default(),
            result_view: ResultView::Table,
            assistant_open: true,
            messages: if sql == SALES_SQL {
                vec![
                    Message {
                        user: true,
                        text: tr!("Which categories generated the most revenue last quarter?")
                            .into(),
                        sql: None,
                    },
                    Message {
                        user: false,
                        text: tr!(
                            "I prepared a query for Q3 2026, considering only completed orders."
                        )
                        .into(),
                        sql: Some(SALES_SQL),
                    },
                ]
            } else {
                Vec::new()
            },
            executed: sql == SALES_SQL,
            running: false,
            executions: 0,
            query_statuses: Vec::new(),
            status_sql: Vec::new(),
            execution_id: 0,
            editor_sql: sql.to_string(),
            execution_error: None,
            cancelled: false,
            saved_sql: None,
            notice: None,
            limited: false,
            selection: None,
            grid_focus: cx.focus_handle(),
            descending: true,
            filter_open: false,
            chat_scroll: ScrollHandle::new(),
            result_scroll: UniformListScrollHandle::new(),
            _subscriptions: vec![
                prompt_subscription,
                filter_subscription,
                resize_subscription,
                query_resize,
                editor_observation,
            ],
        }
    }

    pub fn execute(&mut self, cx: &mut Context<Self>) {
        if self.running {
            self.stop_execution(cx);
        } else {
            self.execute_queries(None, cx);
        }
    }

    fn stop_execution(&mut self, cx: &mut Context<Self>) {
        if !self.running {
            return;
        }
        self.cancellation.cancel();
        self.execution_id += 1;
        self.running = false;
        self.cancelled = true;
        for status in &mut self.query_statuses {
            if *status == QueryStatus::Running {
                *status = QueryStatus::Cancelled;
            }
        }
        // The results footer reports the cancellation; no second message.
        self.sync_statement_statuses();
        cx.notify();
    }

    /// Keeps the status of statements whose text is unchanged and resets the
    /// others. Statuses are left alone while an execution owns them.
    fn sync_statement_statuses(&mut self) {
        if self.running {
            return;
        }
        let statements = jykre::sql::normalized_statements(&self.editor_sql);
        self.query_statuses = carry_statuses(&self.status_sql, &self.query_statuses, &statements);
        self.status_sql = statements;
    }

    fn set_status(&mut self, index: usize, status: QueryStatus) {
        if let Some(slot) = self.query_statuses.get_mut(index) {
            *slot = status;
        }
    }

    fn has_results(&self) -> bool {
        self.executed && self.execution_error.is_none()
    }

    fn execute_queries(&mut self, only: Option<usize>, cx: &mut Context<Self>) {
        if self.running {
            return;
        }
        let sql = self.editor.read(cx).text().to_string();
        let ranges = jykre::sql::statements(&sql);
        let queries: Vec<_> = ranges
            .iter()
            .enumerate()
            .filter(|(index, _)| only.is_none_or(|only| *index == only))
            .map(|(index, range)| (index, sql[range.clone()].to_string()))
            .collect();
        if queries.is_empty() {
            self.notice = Some(Feedback::new(
                Tone::Info,
                tr!("There is no SQL statement to run."),
            ));
            cx.notify();
            return;
        }
        self.sync_statement_statuses();
        self.query_statuses.resize(ranges.len(), QueryStatus::Ready);
        self.execution_id += 1;
        let execution_id = self.execution_id;
        self.running = true;
        self.cancelled = false;
        self.execution_error = None;
        self.notice = None;
        cx.notify();
        if let Some(input) = self.input.clone() {
            self.cancellation = CancellationToken::new();
            let cancel = self.cancellation.clone();
            for (index, _) in &queries {
                self.set_status(*index, QueryStatus::Running);
            }
            cx.spawn(async move |weak, cx| {
                let requests = queries.iter().map(|(_, sql)| QueryRequest { sql: sql.clone(), options: ExecutionOptions::default() }).collect();
                // Rows become display text on the database runtime: that work,
                // and dropping the driver cells, grows with the result.
                let result = backend::run(async move {
                    let mut batch = backend::query(input, requests, cancel).await?;
                    let data: Vec<_> = batch.completed.iter_mut().map(result_data).collect();
                    Ok((batch, data))
                }).await;
                let _ = weak.update(cx, |this, cx| {
                    if this.execution_id != execution_id { return; }
                    this.running = false;
                    match result {
                        Ok((batch, data)) => {
                            this.elapsed_ms = batch.completed.iter().map(|result| result.elapsed_ms).sum();
                            let completed = batch.completed.len();
                            for (((index, sql), result), data) in queries.iter().zip(batch.completed).zip(data) {
                                this.set_status(*index, QueryStatus::Success);
                                this.rows_affected = result.outcome.rows_affected;
                                this.limited = result.limited;
                                this.set_result(data);
                                this.executed = true;
                                this.executions += 1;
                                this.selection = None;
                                this.result_view = ResultView::Table;
                                cx.emit(WorkspaceEvent::Executed { title: this.title.to_string(), connection: this.connection.to_string(), sql: sql.clone() });
                            }
                            if let Some(error) = batch.error {
                                for (position, (index, _)) in queries.iter().enumerate().skip(completed) {
                                    this.set_status(*index, if position == completed { QueryStatus::Error } else { QueryStatus::Ready });
                                }
                                this.execution_error = Some(if completed == 0 {
                                    error.to_string()
                                } else {
                                    tr!("{0} statement(s) completed before the failure: {1}. Previous statements were not rolled back automatically.", completed, error)
                                });
                            }
                        }
                        Err(error) if error.code == ErrorCode::Cancelled => {
                            for (index, _) in queries { this.set_status(index, QueryStatus::Cancelled); }
                            this.cancelled = true;
                        }
                        Err(error) => {
                            for (index, _) in queries { this.set_status(index, QueryStatus::Error); }
                            this.execution_error = Some(error.to_string());
                        }
                    }
                    this.sync_statement_statuses();
                    cx.notify();
                });
            }).detach();
            return;
        }
        cx.spawn(async move |weak, cx| {
            for (index, sql) in queries {
                let active = weak
                    .update(cx, |this, cx| {
                        if this.execution_id != execution_id {
                            return false;
                        }
                        this.set_status(index, QueryStatus::Running);
                        cx.notify();
                        true
                    })
                    .unwrap_or(false);
                if !active {
                    return;
                }
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(550))
                    .await;
                let keep_running = weak
                    .update(cx, |this, cx| {
                        if this.execution_id != execution_id {
                            return false;
                        }
                        this.executions += 1;
                        if let Some(error) = jykre::sql::preview_error(&sql) {
                            this.set_status(index, QueryStatus::Error);
                            this.execution_error =
                                Some(tr!("Error in query {0}: {1}", index + 1, error));
                            this.executed = false;
                            this.running = false;
                            this.sync_statement_statuses();
                            cx.notify();
                            return false;
                        }
                        this.set_status(index, QueryStatus::Success);
                        this.set_result(sample_result(&sql));
                        this.limited = false;
                        this.rows_affected = None;
                        this.selection = None;
                        this.executed = true;
                        this.result_view = ResultView::Table;
                        cx.emit(WorkspaceEvent::Executed {
                            title: this.title.to_string(),
                            connection: this.connection.to_string(),
                            sql,
                        });
                        cx.notify();
                        true
                    })
                    .unwrap_or(false);
                if !keep_running {
                    return;
                }
            }
            let _ = weak.update(cx, |this, cx| {
                if this.execution_id != execution_id {
                    return;
                }
                this.running = false;
                this.sync_statement_statuses();
                cx.notify();
            });
        })
        .detach();
    }

    fn submit_prompt(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.prompt.read(cx).value().to_string();
        if text.trim().is_empty() {
            return;
        }
        self.ask(text, cx);
        self.prompt
            .update(cx, |input, cx| input.set_value("", window, cx));
    }

    fn ask(&mut self, text: String, cx: &mut Context<Self>) {
        let lower = text.to_lowercase();
        let (answer, sql) = if lower.contains("optimi") || lower.contains("slow") {
            (
                tr!(
                    "One alternative is to aggregate the orders before the JOINs. This can reduce intermediate rows. Validate the difference with EXPLAIN ANALYZE on your database; this mock does not measure performance."
                ),
                Some(OPTIMIZED_SQL),
            )
        } else if lower.contains("expla") {
            (
                tr!(
                    "The query filters completed orders between July and September 2026, joins products and categories, and groups the values. COUNT counts orders, SUM computes revenue, and AVG returns the average ticket. The ordering shows the highest revenue first."
                ),
                None,
            )
        } else if lower.contains("chart") || lower.contains("graph") {
            self.result_view = ResultView::Chart;
            (
                tr!(
                    "I opened the bar chart. Electronics leads with R$ 248,760.00. Compare the bar lengths to see how revenue is concentrated by category."
                ),
                None,
            )
        } else if lower.contains("clean") {
            (
                tr!(
                    "To investigate data quality, start by checking null values in product_id, orders without a category, and duplicate IDs. Also compare the orders count before and after the JOINs."
                ),
                None,
            )
        } else {
            (
                tr!(
                    "In this sample, Electronics accounts for 32.4% of revenue. Together with Home and decor, the two categories make up 52.7% of the total. You can explore the chart, ask for an explanation of the query, or try an optimization suggestion."
                ),
                None,
            )
        };
        self.messages.push(Message {
            user: true,
            text,
            sql: None,
        });
        self.messages.push(Message {
            user: false,
            text: if self.result.sales { answer.into() } else { tr!("The query runs in the context of {0}. The sample preview has {1} columns and {2} rows. Use the editor to adjust the filters and run it to see the sample. This demo's assistant has a complete analysis flow in the Revenue by category tab.", self.connection, self.result.columns.len(), self.result.rows.len()) },
            sql: if self.result.sales { sql } else { None },
        });
        self.assistant_open = true;
        self.chat_scroll.scroll_to_bottom();
        cx.notify();
    }

    fn toolbar(&self, cx: &mut Context<Self>) -> Div {
        row()
            .h(px(48.))
            .flex_none()
            .px_4()
            .justify_between()
            .border_b_1()
            .border_color(rgb(BORDER))
            .child(
                row()
                    .gap_3()
                    .child(
                        primary_button(
                            "execute",
                            if self.running { I::Square } else { I::Play },
                            if self.running {
                                tr!("Stop")
                            } else {
                                tr!("Run")
                            },
                        )
                        .tooltip(|window, cx| {
                            Tooltip::new(tr!("Run or stop queries · Ctrl+Enter")).build(window, cx)
                        })
                        .on_click(cx.listener(|this, _, _, cx| this.execute(cx))),
                    )
                    .child(divider())
                    .child(button("explain", I::ScanLine, tr!("Explain")).on_click(
                        cx.listener(|this, _, _, cx| {
                            this.ask(tr!("Explain this query").into(), cx)
                        }),
                    ))
                    .child(button("format", I::TextAlignStart, tr!("Format")).on_click(
                        cx.listener(|this, _, window, cx| {
                            let sql = this.editor.read(cx).text().to_string();
                            let formatted = jykre::sql::format(&sql);
                            if formatted == sql {
                                this.notice = Some(Feedback::new(
                                    Tone::Info,
                                    tr!("SQL is already formatted."),
                                ));
                            } else {
                                // `replace_all` keeps the undo history, so Ctrl+Z restores
                                // the previous layout.
                                this.editor.update(cx, |editor, cx| {
                                    editor.replace_all(formatted, window, cx)
                                });
                                this.notice = Some(Feedback::new(
                                    Tone::Success,
                                    tr!("SQL formatted. Use Ctrl+Z to undo."),
                                ));
                            }
                            cx.notify();
                        }),
                    )),
            )
            .child(
                row()
                    .child(
                        icon_button(
                            "save-query",
                            if self.saved_sql.as_deref() == Some(self.editor_sql.as_str()) {
                                I::BookmarkCheck
                            } else {
                                I::Bookmark
                            },
                            match &self.saved_sql {
                                Some(saved) if *saved == self.editor_sql => {
                                    tr!("Remove from saved queries")
                                }
                                Some(_) => tr!("Update saved query"),
                                None => tr!("Save query"),
                            },
                        )
                        .on_click(cx.listener(|this, _, _, cx| {
                            let sql = this.editor.read(cx).text().to_string();
                            // A saved tab whose SQL changed since then is updated
                            // instead of removed, so later edits are not lost.
                            let (saved, notice) = match &this.saved_sql {
                                Some(saved) if *saved == sql => (
                                    false,
                                    Feedback::new(
                                        Tone::Info,
                                        tr!("Query removed from saved queries."),
                                    ),
                                ),
                                Some(_) => (
                                    true,
                                    Feedback::new(Tone::Success, tr!("Saved query updated.")),
                                ),
                                None => (
                                    true,
                                    Feedback::new(
                                        Tone::Success,
                                        tr!("Query saved in this session."),
                                    ),
                                ),
                            };
                            this.saved_sql = saved.then(|| sql.clone());
                            cx.emit(WorkspaceEvent::Saved {
                                saved,
                                title: this.title.to_string(),
                                connection: this.connection.to_string(),
                                sql,
                            });
                            this.notice = Some(notice);
                            cx.notify();
                        })),
                    )
                    .child(divider())
                    .child(
                        icon_button(
                            "toggle-ai",
                            I::PanelRight,
                            tr!("Show or hide the assistant"),
                        )
                        .when(self.assistant_open, |s| s.bg(rgb(SURFACE)))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.assistant_open = !this.assistant_open;
                            cx.notify();
                        })),
                    ),
            )
    }

    fn query_gutter(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let editor = self.editor.clone();
        let sql = editor.read(cx).text().to_string();
        let buttons: Vec<_> = jykre::sql::statements(&sql)
            .into_iter()
            .enumerate()
            .map(|(index, range)| {
                let status = self.query_statuses.get(index).copied().unwrap_or_default();
                let (symbol, color, title) = match status {
                    QueryStatus::Ready => (I::Play, MINT, tr!("Run this query")),
                    QueryStatus::Running => (I::Square, 0xe7bd7b, tr!("Stop execution")),
                    QueryStatus::Success => (I::CircleCheck, MINT, tr!("Success · run again")),
                    QueryStatus::Error => (I::CircleX, 0xef8b91, tr!("Error · run again")),
                    QueryStatus::Cancelled => (I::Play, FAINT, tr!("Cancelled · run again")),
                };
                let button = div()
                    .id(("query-run", index))
                    .role(Role::Button)
                    .aria_label(tr!("Query {0}: {1}", index + 1, title))
                    .cursor_pointer()
                    .size(px(22.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(4.))
                    .hover(|s| s.bg(rgb(HOVER)))
                    .tooltip(move |window, cx| Tooltip::new(title).build(window, cx))
                    .child(icon(symbol, color))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if this.query_statuses.get(index) == Some(&QueryStatus::Running) {
                            this.stop_execution(cx);
                        } else {
                            this.execute_queries(Some(index), cx);
                        }
                    }))
                    .into_any_element();
                (
                    sql[..range.start].bytes().filter(|ch| *ch == b'\n').count(),
                    button,
                )
            })
            .collect();
        canvas(
            move |bounds, window, cx| {
                let mut visible = Vec::new();
                for (line, mut button) in buttons {
                    let geometry = {
                        let state = editor.read(cx);
                        if state.display_map().is_buffer_line_hidden(line) {
                            continue;
                        }
                        // Paint-time bounds still belong to the previous frame here.
                        // Use the editor's fresh prepaint snapshot so the gutter and
                        // its hitboxes follow the same scroll offset as the SQL.
                        state.prepaint_caret_geometry().map(|geometry| {
                            let height = geometry.line_height();
                            geometry.input_bounds().top()
                                + geometry.scroll_offset().y
                                + height
                                    * state.display_map().buffer_line_to_display_row(line) as f32
                                + (height - px(22.)) / 2.
                        })
                    };
                    if let Some(top) = geometry {
                        if top < bounds.top() || top + px(22.) > bounds.bottom() {
                            continue;
                        }
                        button.layout_as_root(AvailableSpace::min_size(), window, cx);
                        button.prepaint_at(point(bounds.left(), top), window, cx);
                        visible.push(button);
                    }
                }
                visible
            },
            |_, mut buttons, window, cx| {
                for button in &mut buttons {
                    button.paint(window, cx);
                }
            },
        )
        .w(px(24.))
        .h_full()
        .flex_none()
    }

    fn editor_panel(&self, cx: &mut Context<Self>) -> Div {
        column()
            .size_full()
            .bg(rgb(BG))
            .child(
                row()
                    .h(px(38.))
                    .flex_none()
                    .px_4()
                    .justify_between()
                    .child(
                        row()
                            .gap_2()
                            .child(icon(I::Database, self.connection_color))
                            .child(label(self.connection.clone()))
                            .child(label("/"))
                            .child(label(self.database.clone()))
                            .child(label("/"))
                            .child(label(if self.input.is_some() {
                                "SQL"
                            } else {
                                "public"
                            })),
                    )
                    .child(row().child(dot(MINT)).child(label(if self.input.is_some() {
                        tr!("Real connection")
                    } else {
                        tr!("Demo")
                    }))),
            )
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_h_0()
                    .px_2()
                    .overflow_hidden()
                    .relative()
                    .pl(px(24.))
                    .child(
                        div().flex_1().min_w_0().h_full().child(
                            Editor::new(&self.editor)
                                .appearance(false)
                                .bordered(false)
                                .font_family("Cascadia Code")
                                .text_size(cx.theme().mono_font_size)
                                .w_full()
                                .h_full()
                                .aria_label(tr!("SQL query editor")),
                        ),
                    )
                    .child(
                        div()
                            .absolute()
                            .left(px(8.))
                            .top_0()
                            .h_full()
                            .child(deferred(self.query_gutter(cx))),
                    ),
            )
            .child(
                row()
                    .h(px(26.))
                    .flex_none()
                    .px_4()
                    .justify_between()
                    .child(label(format!("SQL · {}", self.dialect)))
                    .child(
                        row()
                            .gap_3()
                            .child(label("UTF-8"))
                            .child(label(tr!("Spaces: 4")))
                            .child(label(match self.editor_sql.split('\n').count() {
                                1 => tr!("1 line").to_string(),
                                lines => tr!("{0} lines", lines),
                            })),
                    ),
            )
    }

    fn result_panel(&self, cx: &mut Context<Self>) -> Div {
        let tabs = [
            (ResultView::Table, I::Table2, tr!("Results")),
            (ResultView::Chart, I::ChartColumn, tr!("Chart")),
            (ResultView::Messages, I::Terminal, tr!("Messages")),
        ];
        column()
            .size_full()
            .border_t_1()
            .border_color(rgb(BORDER))
            .bg(rgb(PANEL))
            .child(
                row()
                    .h(px(42.))
                    .flex_none()
                    .px_3()
                    .justify_between()
                    .border_b_1()
                    .border_color(rgb(BORDER))
                    .child(row().gap_1().children(tabs.into_iter().enumerate().map(
                        |(i, (view, name, title))| {
                            button(("result-mode", i), name, title)
                                .when(self.result_view == view, |s| {
                                    s.bg(rgb(SURFACE)).text_color(rgb(TEXT))
                                })
                                .when(
                                    view == ResultView::Table
                                        && self.has_results()
                                        && !self.running,
                                    |s| s.child(badge(self.result.rows.len().to_string(), MINT)),
                                )
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.result_view = view;
                                    cx.notify();
                                }))
                        },
                    )))
                    .child(
                        row()
                            .gap_1()
                            .child(
                                button("format-numbers", I::Settings, tr!("pt-BR numbers"))
                                    .when(self.numbers_pt_br, |s| {
                                        s.bg(rgb(HOVER)).text_color(rgb(MINT))
                                    })
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.numbers_pt_br = !this.numbers_pt_br;
                                        cx.notify();
                                    })),
                            )
                            .child(
                                icon_button("filter-results", I::ListFilter, tr!("Filter results"))
                                    .aria_selected(self.filter_open)
                                    .when(self.filter_open, |s| s.bg(rgb(HOVER)))
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.filter_open = !this.filter_open;
                                        if this.filter_open {
                                            this.filter
                                                .update(cx, |input, cx| input.focus(window, cx));
                                        }
                                        cx.notify();
                                    })),
                            )
                            .child(
                                icon_button(
                                    "export-results",
                                    I::Download,
                                    tr!("Copy results as CSV"),
                                )
                                .on_click(cx.listener(
                                    |this, _, _, cx| {
                                        if !this.has_results() {
                                            this.notice = Some(Feedback::new(
                                                Tone::Info,
                                                tr!("Run a query before copying the results."),
                                            ));
                                            cx.notify();
                                            return;
                                        }
                                        let mut csv = this
                                            .result
                                            .columns
                                            .iter()
                                            .map(|(name, _)| {
                                                format!("\"{}\"", name.replace('"', "\"\""))
                                            })
                                            .collect::<Vec<_>>()
                                            .join(";");
                                        for (row_index, row) in this.result.rows.iter().enumerate()
                                        {
                                            csv.push('\n');
                                            csv.push_str(
                                                &row.iter()
                                                    .enumerate()
                                                    .map(|(column, _)| {
                                                        cell_format::csv_field(
                                                            this.result.value(row_index, column),
                                                        )
                                                    })
                                                    .collect::<Vec<_>>()
                                                    .join(";"),
                                            );
                                        }
                                        cx.write_to_clipboard(ClipboardItem::new_string(csv));
                                        this.notice = Some(Feedback::new(
                                            Tone::Success,
                                            tr!("CSV copied. Paste it into a spreadsheet or file."),
                                        ));
                                        cx.notify();
                                    },
                                )),
                            )
                            .child(
                                icon_button(
                                    "refresh-results",
                                    I::RefreshCw,
                                    tr!("Run query again"),
                                )
                                .on_click(cx.listener(|this, _, _, cx| this.execute(cx))),
                            ),
                    ),
            )
            .when(self.filter_open, |s| {
                s.child(
                    div()
                        .px_3()
                        .py_2()
                        .child(Input::new(&self.filter).small().prefix(I::Search).w_full()),
                )
            })
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .overflow_hidden()
                    .child(if self.running {
                        column()
                            .size_full()
                            .items_center()
                            .justify_center()
                            .gap_3()
                            .child(spinning_icon("running-spinner", I::LoaderCircle, MINT))
                            .child(label(if self.input.is_some() {
                                tr!("Running on the server…")
                            } else {
                                tr!("Preparing the demo…")
                            }))
                            .into_any_element()
                    } else if let Some(error) = &self.execution_error {
                        column()
                            .size_full()
                            .items_center()
                            .justify_center()
                            .gap_3()
                            .p_4()
                            .child(icon(I::CircleX, RED))
                            .child(
                                div()
                                    .text_size(px(12.))
                                    .text_color(rgb(RED))
                                    .child(error.clone()),
                            )
                            .into_any_element()
                    } else if !self.executed {
                        column()
                            .size_full()
                            .items_center()
                            .justify_center()
                            .gap_3()
                            .child(icon(I::Terminal, FAINT))
                            .child(div().text_size(px(14.)).child(tr!("Ready to explore")))
                            .child(label(tr!("Run the query to see the results.")))
                            .child(
                                button("empty-run", I::Play, tr!("Run query"))
                                    .on_click(cx.listener(|this, _, _, cx| this.execute(cx))),
                            )
                            .into_any_element()
                    } else {
                        match self.result_view {
                            ResultView::Table => self.result_table(cx),
                            ResultView::Chart => self.result_chart(cx),
                            ResultView::Messages => self.result_messages(),
                        }
                    }),
            )
            .child(
                row()
                    .h(px(36.))
                    .flex_none()
                    .px_3()
                    .border_t_1()
                    .border_color(rgb(BORDER))
                    .justify_between()
                    .child({
                        let rows = match self.result.rows.len() {
                            1 => tr!("1 row").to_string(),
                            count => tr!("{0} rows", count),
                        };
                        // The footer is the single place that reports how the last run
                        // ended; errors also fill the results area with their message.
                        let (symbol, color, text, detail): (I, u32, String, Option<&str>) = if self
                            .running
                        {
                            (I::LoaderCircle, AMBER, tr!("Running…").into(), None)
                        } else if self.execution_error.is_some() {
                            (I::CircleX, RED, tr!("Query error").into(), None)
                        } else if self.cancelled {
                            (I::Ban, AMBER, tr!("Execution cancelled").into(), None)
                        } else if self.executed && self.limited {
                            (
                                I::TriangleAlert,
                                AMBER,
                                tr!("{0} · row limit reached", rows),
                                Some(tr!(
                                    "Limit of 1,000 rows reached. Refine the SQL to get other rows."
                                )),
                            )
                        } else if self.executed {
                            let text = match self.rows_affected {
                                Some(affected) => {
                                    tr!("{0} · {1} rows affected", rows, affected)
                                }
                                None => rows,
                            };
                            (I::CircleCheck, MINT, text, None)
                        } else {
                            (I::Clock, FAINT, tr!("Waiting to run").into(), None)
                        };
                        row()
                            .id("run-status")
                            .role(Role::Status)
                            .aria_label(text.clone())
                            .gap_2()
                            .when_some(detail, |s, detail| {
                                s.tooltip(move |window, cx| Tooltip::new(detail).build(window, cx))
                            })
                            .map(|s| {
                                if self.running {
                                    s.child(spinning_icon("run-status-spinner", symbol, color))
                                } else {
                                    s.child(icon(symbol, color))
                                }
                            })
                            .child(div().text_size(px(12.)).text_color(rgb(color)).child(text))
                            .when(
                                self.has_results() && !self.running && !self.cancelled,
                                |s| {
                                    s.child(label("·"))
                                        .child(label(format!("{} ms", self.elapsed_ms)))
                                },
                            )
                    })
                    .child(
                        row()
                            .when(self.has_results() && !self.running, |s| {
                                let selected = self.selected_count(cx);
                                s.when(selected > 1, |s| {
                                    s.child(label(tr!("{0} cells selected", selected)))
                                        .child(label("·"))
                                })
                            })
                            .child(label(if self.input.is_some() {
                                tr!("Server results")
                            } else {
                                tr!("Demo data")
                            }))
                            .child(badge(
                                if self.input.is_some() { "SQL" } else { "MOCK" },
                                FAINT,
                            )),
                    ),
            )
    }

    /// The grid for the current result, filter, sort and number format.
    fn grid(&self, cx: &App) -> Rc<GridView> {
        let key = GridKey {
            result: self.result_version,
            // A closed filter bar must not keep hiding rows.
            filter: if self.filter_open {
                self.filter.read(cx).value().to_lowercase()
            } else {
                String::new()
            },
            descending: self.descending,
            numbers_pt_br: self.numbers_pt_br,
        };
        self.grid.get(&self.result, key)
    }

    fn select_cell(&mut self, row: usize, col: usize, extend: bool) {
        self.selection = Some(match self.selection {
            Some(selection) if extend => CellSelection {
                anchor: selection.anchor,
                focus: (row, col),
            },
            _ => CellSelection {
                anchor: (row, col),
                focus: (row, col),
            },
        });
    }

    fn select_all(&mut self, cx: &App) {
        let grid = self.grid(cx);
        let visible = &grid.rows;
        let columns = self.result.columns.len();
        if let (Some(first), Some(last)) = (visible.first(), visible.last())
            && columns > 0
        {
            self.selection = Some(CellSelection {
                anchor: (*first, 0),
                focus: (*last, columns - 1),
            });
        }
    }

    /// Arrow-key navigation; with Shift the block grows from the anchor.
    fn move_selection(&mut self, rows: isize, cols: isize, extend: bool, cx: &App) {
        let grid = self.grid(cx);
        let visible = &grid.rows;
        let columns = self.result.columns.len();
        if visible.is_empty() || columns == 0 {
            return;
        }
        let (position, col) = self
            .selection
            .and_then(|selection| {
                let position = visible.iter().position(|row| *row == selection.focus.0)?;
                Some((position, selection.focus.1))
            })
            .unwrap_or((0, 0));
        let position = position.saturating_add_signed(rows).min(visible.len() - 1);
        let col = col.saturating_add_signed(cols).min(columns - 1);
        self.select_cell(visible[position], col, extend && self.selection.is_some());
        self.result_scroll
            .scroll_to_item(position, ScrollStrategy::Nearest);
    }

    fn selected_count(&self, cx: &App) -> usize {
        let grid = self.grid(cx);
        self.selection
            .and_then(|selection| selection_block(selection, &grid.rows))
            .map_or(0, |(rows, cols)| rows.count() * cols.count())
    }

    /// Copies the selected block as tab-separated text (a single cell as its
    /// exact value), optionally with the column names first.
    fn copy_selection(&mut self, headers: bool, cx: &mut Context<Self>) {
        let grid = self.grid(cx);
        let visible = &grid.rows;
        let Some((rows, cols)) = self
            .selection
            .and_then(|selection| selection_block(selection, visible))
        else {
            self.notice = Some(Feedback::new(Tone::Info, tr!("Select cells to copy.")));
            cx.notify();
            return;
        };
        let names: Vec<&str> = cols
            .clone()
            .map(|col| self.result.columns[col].0.as_str())
            .collect();
        let data: Vec<Vec<Option<&str>>> = rows
            .map(|position| {
                cols.clone()
                    .map(|col| self.result.value(visible[position], col))
                    .collect()
            })
            .collect();
        let count = data.len() * names.len();
        let text = cell_format::copy_cells(headers.then_some(names.as_slice()), &data);
        cx.write_to_clipboard(ClipboardItem::new_string(text));
        self.notice = Some(Feedback::new(
            Tone::Success,
            if count == 1 {
                tr!("Cell copied.").to_string()
            } else {
                tr!("{0} cells copied.", count)
            },
        ));
        cx.notify();
    }

    fn grid_key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let keystroke = &event.keystroke;
        let shift = keystroke.modifiers.shift;
        let handled = if keystroke.modifiers.control || keystroke.modifiers.platform {
            match keystroke.key.as_str() {
                "c" => {
                    self.copy_selection(shift, cx);
                    true
                }
                "a" => {
                    self.select_all(cx);
                    true
                }
                _ => false,
            }
        } else {
            match keystroke.key.as_str() {
                "up" => self.move_selection(-1, 0, shift, cx),
                "down" => self.move_selection(1, 0, shift, cx),
                "left" => self.move_selection(0, -1, shift, cx),
                "right" => self.move_selection(0, 1, shift, cx),
                "escape" if self.selection.is_some() => self.selection = None,
                _ => return,
            }
            true
        };
        if handled {
            window.prevent_default();
            cx.stop_propagation();
            cx.notify();
        }
    }

    fn result_table(&self, cx: &mut Context<Self>) -> AnyElement {
        let grid = self.grid(cx);
        let widths = &grid.widths;
        let grid_width = widths.iter().sum::<f32>() + 35.;
        let weak = cx.weak_entity();
        let table = div()
            .id("result-table-scroll")
            .size_full()
            .bg(rgb(0x13171d))
            .text_color(rgb(0xe2e8f0))
            .overflow_x_scrollbar()
            .child(
                column()
                    .w(px(grid_width))
                    .min_w_full()
                    .h_full()
                    .child(
                        row()
                            .h(px(34.))
                            .flex_none()
                            .gap_0()
                            .bg(rgb(0x1b2028))
                            .border_b_1()
                            .border_color(rgb(0x2a303b))
                            .child(div().w(px(35.)).flex_none().text_center().child(label("#")))
                            .children(self.result.columns.iter().enumerate().map(
                                |(index, (name, kind))| {
                                    let kind = kind.to_lowercase();
                                    let tooltip = format!("{name} : {kind}");
                                    row()
                                        .id(("result-header", index))
                                        .w(px(widths[index]))
                                        .flex_none()
                                        .h_full()
                                        .px_3()
                                        .gap_1()
                                        .when(self.result.sales, |s| {
                                            s.flex_grow([1.5, 0.8, 1.2, 1.1, 0.65][index])
                                        })
                                        .border_l_1()
                                        .border_color(rgb(0x2a303b))
                                        .text_size(px(14.))
                                        .text_color(rgb(0xc2cbd8))
                                        .tooltip(move |window, cx| {
                                            Tooltip::new(tooltip.clone()).build(window, cx)
                                        })
                                        .child(
                                            div()
                                                .flex_shrink(1.)
                                                .min_w_0()
                                                .truncate()
                                                .child(name.clone()),
                                        )
                                        .child(
                                            div()
                                                .flex_1()
                                                .min_w_0()
                                                .truncate()
                                                .text_size(px(12.))
                                                .text_color(rgb(0x6f7a8a))
                                                .child(format!(": {kind}")),
                                        )
                                        .when(index == 2 && self.result.sales, |s| {
                                            s.cursor_pointer()
                                                .focusable()
                                                .tab_index(0)
                                                .focus_visible(|s| s.bg(rgb(HOVER)))
                                        })
                                        .when(index == 2 && self.result.sales, |s| {
                                            s.child(icon(
                                                if self.descending {
                                                    I::ArrowDown
                                                } else {
                                                    I::ArrowUp
                                                },
                                                MINT,
                                            ))
                                        })
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            if index == 2 && this.result.sales {
                                                this.descending = !this.descending;
                                                cx.notify();
                                            }
                                        }))
                                },
                            )),
                    )
                    .child(if grid.rows.is_empty() {
                        column()
                            .p_6()
                            .gap_2()
                            .child(label(tr!("No results for this selection.")))
                            .child(label(tr!(
                                "The query or the current filter returned no rows."
                            )))
                            .into_any_element()
                    } else {
                        // Only the rows on screen are built, so a frame costs
                        // the same for 30 rows or 100,000.
                        div()
                            .relative()
                            .flex_1()
                            .min_h_0()
                            .child(
                                uniform_list(
                                    "result-rows",
                                    grid.rows.len(),
                                    cx.processor(|this, range: Range<usize>, _, cx| {
                                        this.result_rows(range, cx)
                                    }),
                                )
                                .size_full()
                                .track_scroll(&self.result_scroll),
                            )
                            .vertical_scrollbar(&self.result_scroll)
                            .into_any_element()
                    }),
            );
        div()
            .id("result-grid")
            .size_full()
            .track_focus(&self.grid_focus)
            .on_key_down(cx.listener(Self::grid_key_down))
            .child(table)
            .context_menu(move |menu, _, cx| {
                let has_selection = weak
                    .upgrade()
                    .is_some_and(|this| this.read(cx).selection.is_some());
                let action =
                    |weak: WeakEntity<Workspace>,
                     run: fn(&mut Workspace, &mut Context<Workspace>)| {
                        move |_: &ClickEvent, _: &mut Window, cx: &mut App| {
                            let _ = weak.update(cx, |this, cx| {
                                run(this, cx);
                                cx.notify();
                            });
                        }
                    };
                menu.item(
                    PopupMenuItem::new(tr!("Copy"))
                        .disabled(!has_selection)
                        .on_click(action(weak.clone(), |this, cx| {
                            this.copy_selection(false, cx)
                        })),
                )
                .item(
                    PopupMenuItem::new(tr!("Copy with headers"))
                        .disabled(!has_selection)
                        .on_click(action(weak.clone(), |this, cx| {
                            this.copy_selection(true, cx)
                        })),
                )
                .separator()
                .item(
                    PopupMenuItem::new(tr!("Select all"))
                        .on_click(action(weak.clone(), |this, cx| this.select_all(cx))),
                )
            })
            .into_any_element()
    }

    /// The data rows at `range` of the displayed order.
    fn result_rows(&self, range: Range<usize>, cx: &mut Context<Self>) -> Vec<Stateful<Div>> {
        let grid = self.grid(cx);
        let widths = &grid.widths;
        let block = self
            .selection
            .and_then(|selection| selection_block(selection, &grid.rows));
        let focus = self.selection.map(|selection| selection.focus);
        let last_column = self.result.columns.len().saturating_sub(1);
        range
            .filter_map(|position| Some((position, *grid.rows.get(position)?)))
            .map(|(position, index)| {
                let cells = &self.result.rows[index];
                let row_selected = block.as_ref().is_some_and(|(rows, cols)| {
                    rows.contains(&position) && *cols.start() == 0 && *cols.end() == last_column
                });
                row()
                    .id(("data-row", index))
                    .role(Role::Row)
                    .aria_label(cells.join(", "))
                    .w_full()
                    .h(px(34.))
                    .flex_none()
                    .gap_0()
                    .child(
                        div()
                            .id(("row-number", index))
                            .w(px(35.))
                            .flex_none()
                            .h_full()
                            .flex()
                            .items_center()
                            .justify_center()
                            .cursor_pointer()
                            .bg(rgb(if row_selected { 0x283e65 } else { 0x1b2028 }))
                            .hover(|s| s.bg(rgb(0x222a36)))
                            .border_b_1()
                            .border_color(rgb(0x2a303b))
                            .text_center()
                            .text_size(px(12.))
                            .text_color(rgb(FAINT))
                            .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                                let extend = event.modifiers().shift;
                                if !extend || this.selection.is_none() {
                                    this.select_cell(index, 0, false);
                                } else if let Some(selection) = &mut this.selection {
                                    selection.anchor.1 = 0;
                                }
                                this.select_cell(index, last_column, true);
                                window.focus(&this.grid_focus, cx);
                                cx.notify();
                            }))
                            .child((index + 1).to_string()),
                    )
                    .children(cells.iter().enumerate().map(|(col, value)| {
                        let family = grid.families[col];
                        let raw = self.result.value(index, col);
                        // A cell is one line: line breaks show as ↵ and the
                        // full value stays in the tooltip and in copies.
                        let displayed = cell_format::display(raw, family, self.numbers_pt_br)
                            .replace("\r\n", "↵")
                            .replace(['\n', '\r'], "↵");
                        let is_null = raw.is_none();
                        // Only values that do not fit get a tooltip, so it
                        // does not cover the context menu or neighbours.
                        let tooltip = raw
                            .filter(|value| {
                                value.contains(['\n', '\r'])
                                    || displayed.chars().count() as f32 * 7.8 > widths[col] - 18.
                            })
                            .map(str::to_owned);
                        let selected = block.as_ref().is_some_and(|(rows, cols)| {
                            rows.contains(&position) && cols.contains(&col)
                        });
                        let focused = focus == Some((index, col));
                        div()
                            .id(("result-cell", col))
                            .role(Role::GridCell)
                            .aria_selected(selected)
                            .aria_label(tr!(
                                "Row {0}, {1}: {2}",
                                index + 1,
                                self.result.columns[col].0,
                                value
                            ))
                            .cursor_pointer()
                            .w(px(widths[col]))
                            .flex_none()
                            .min_w_0()
                            .h_full()
                            .flex()
                            .items_center()
                            .px(px(8.))
                            .border_1()
                            .border_color(rgb(0x2a303b))
                            .when(!selected, |s| s.hover(|s| s.bg(rgb(0x222a36))))
                            .when(selected, |s| s.bg(rgb(0x283e65)))
                            .when(focused, |s| s.border_color(rgb(0x7fa8fa)))
                            .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                                this.select_cell(index, col, event.modifiers().shift);
                                window.focus(&this.grid_focus, cx);
                                cx.notify();
                            }))
                            // A right click outside the selection selects the
                            // cell first, so the menu acts on what is under it.
                            .on_mouse_down(
                                MouseButton::Right,
                                cx.listener(move |this, _, window, cx| {
                                    if !selected {
                                        this.select_cell(index, col, false);
                                    }
                                    window.focus(&this.grid_focus, cx);
                                    cx.notify();
                                }),
                            )
                            .when(self.result.sales, |s| {
                                s.flex_grow([1.5, 0.8, 1.2, 1.1, 0.65][col])
                            })
                            .font_family("Cascadia Mono")
                            .text_size(px(13.))
                            .when(is_null, |s| s.italic())
                            .when(family == ValueFamily::Boolean, |s| s.justify_center())
                            .when_some(tooltip, |s, tooltip| {
                                s.tooltip(move |window, cx| {
                                    Tooltip::new(tooltip.clone()).build(window, cx)
                                })
                            })
                            .when(family == ValueFamily::Number, |s| s.justify_end())
                            .text_color(rgb(if is_null {
                                0x64748b
                            } else if family == ValueFamily::Number {
                                0x60a5fa
                            } else if family == ValueFamily::Temporal {
                                0xc4b5fd
                            } else if family == ValueFamily::Binary {
                                0x94a3b8
                            } else {
                                0xe2e8f0
                            }))
                            .child(div().truncate().child(displayed))
                    }))
            })
            .collect()
    }

    fn result_chart(&self, _: &mut Context<Self>) -> AnyElement {
        if !self.result.sales {
            return column()
                .size_full()
                .items_center()
                .justify_center()
                .gap_3()
                .p_5()
                .child(icon(I::ChartColumn, MINT))
                .child(
                    div()
                        .text_size(px(14.))
                        .child(tr!("Give your results a view")),
                )
                .child(label(tr!(
                    "Chart available only in the Revenue by category demo."
                )))
                .into_any_element();
        }
        column()
            .id("revenue-chart")
            .size_full()
            .overflow_y_scroll()
            .p_5()
            .gap_3()
            .child(
                row()
                    .justify_between()
                    .child(
                        column()
                            .gap_1()
                            .child(div().text_size(px(14.)).child(tr!("Revenue by category")))
                            .child(label(tr!("July — September 2026 · BRL"))),
                    )
                    .child(badge("R$ 769.000,00", MINT)),
            )
            .children(SALES.iter().enumerate().map(|(index, data)| {
                row()
                    .h(px(24.))
                    .flex_none()
                    .gap_3()
                    .child(
                        div()
                            .w(px(125.))
                            .flex_none()
                            .text_size(px(12.))
                            .text_color(rgb(MUTED))
                            .child(data.category),
                    )
                    .child(
                        div()
                            .flex_1()
                            .h(px(16.))
                            .rounded(px(3.))
                            .bg(rgb(SURFACE))
                            .child(
                                div()
                                    .w(relative(data.amount / SALES[0].amount))
                                    .h_full()
                                    .rounded(px(3.))
                                    .bg(rgb(if index == 0 { MINT } else { 0x4c8274 })),
                            ),
                    )
                    .child(
                        div()
                            .w(px(94.))
                            .text_right()
                            .font_family("Cascadia Mono")
                            .text_size(px(12.))
                            .child(data.revenue),
                    )
            }))
            .into_any_element()
    }

    fn result_messages(&self) -> AnyElement {
        column()
            .p_5()
            .gap_3()
            .child(
                row()
                    .child(icon(I::CircleCheck, MINT))
                    .child(tr!("Query completed")),
            )
            .child(label(tr!(
                "{0} run(s) in this session · {1} rows · {2} ms",
                self.executions,
                self.result.rows.len(),
                self.elapsed_ms
            )))
            .child(label(if self.input.is_some() {
                tr!("Server execution · limit of 1,000 rows per statement · 300 s timeout")
            } else {
                tr!("Visual demo results.")
            }))
            .child(label(self.execution_error.clone().unwrap_or_else(|| {
                if self.input.is_some() {
                    tr!("{0} rows affected", self.rows_affected.unwrap_or(0))
                } else {
                    tr!("No changes to a real database.").into()
                }
            })))
            .into_any_element()
    }

    fn assistant(&self, cx: &mut Context<Self>) -> Div {
        column().size_full().bg(rgb(0x191d23)).border_l_1().border_color(rgb(BORDER))
            .child(row().h(px(48.)).flex_none().px_4().justify_between().border_b_1().border_color(rgb(BORDER))
                .child(row().child(icon(I::Sparkles, MINT)).child(div().text_size(px(14.)).font_weight(FontWeight::SEMIBOLD).child(tr!("Assistant")))
                    .child(badge("IA", MINT)))
                .child(row().gap_0()
                    .child(icon_button("reset-chat", I::SquarePen, tr!("New conversation"))
                        .on_click(cx.listener(|this, _, _, cx| { this.messages.clear(); cx.notify(); })))
                    .child(icon_button("close-ai", I::PanelRightClose, tr!("Collapse assistant"))
                        .on_click(cx.listener(|this, _, _, cx| { this.assistant_open = false; cx.notify(); })))))
            .child(row().px_4().h(px(37.)).flex_none().gap_2().border_b_1().border_color(rgb(BORDER))
                .child(icon(I::Layers, FAINT)).child(label(tr!("Context")))
                .child(badge(tr!("Current SQL"), MUTED)).child(badge(if self.result.sales { tr!("3 tables") } else { tr!("1 query") }, MUTED)))
            .child(column().id("ai-conversation").flex_1().overflow_y_scroll().track_scroll(&self.chat_scroll).vertical_scrollbar(&self.chat_scroll).px_4().py_4().gap_5()
                .child(row().gap_2().child(div().h(px(1.)).flex_1().bg(rgb(BORDER)))
                    .child(div().text_size(px(12.)).text_color(rgb(FAINT)).child(tr!("Context of this query")))
                    .child(div().h(px(1.)).flex_1().bg(rgb(BORDER))))
                .when(self.messages.is_empty(), |s| s.child(column().py_5().gap_3()
                    .child(icon(I::Sparkles, MINT)).child(div().text_size(px(16.)).child(tr!("One question, a new path.")))
                    .child(label(tr!("Explore data, understand queries, and turn questions into SQL.")))))
                .children(self.messages.iter().enumerate().map(|(index, message)| {
                    column().gap_2()
                        .child(row().gap_2()
                            .child(div().size(px(20.)).rounded(px(5.)).bg(rgb(if message.user { HOVER } else { 0x263b34 }))
                                .flex().items_center().justify_center().child(icon(if message.user { I::UserRound } else { I::Sparkles }, if message.user { MUTED } else { MINT })))
                            .child(div().text_size(px(12.)).font_weight(FontWeight::SEMIBOLD).text_color(rgb(if message.user { MUTED } else { MINT })).child(if message.user { tr!("You") } else { "Jykre Studio" })))
                        .child(div().text_size(px(14.)).line_height(relative(1.6)).text_color(rgb(if message.user { TEXT } else { 0xaeb8c5 })).child(message.text.clone()))
                        .when_some(message.sql, |s, suggested| s.child(column().mt_1().rounded(px(6.)).border_1().border_color(rgb(BORDER)).bg(rgb(BG)).overflow_hidden()
                            .child(row().px_3().h(px(28.)).justify_between().border_b_1().border_color(rgb(BORDER)).child(label(tr!("SQL · suggestion")))
                                .child(icon_button(("copy-ai", index), I::Copy, tr!("Copy suggested SQL"))
                                    .on_click(cx.listener(move |this, _, _, cx| { cx.write_to_clipboard(ClipboardItem::new_string(suggested.into())); this.notice = Some(Feedback::new(Tone::Success, tr!("Suggested SQL copied."))); cx.notify(); }))))
                            .child(div().px_3().py_3().font_family("Cascadia Mono").text_size(px(12.)).line_height(relative(1.6)).text_color(rgb(BLUE))
                                .child(if suggested == OPTIMIZED_SQL { "WITH sales AS (\n  SELECT product_id,\n         SUM(total) AS revenue\n  FROM orders\n  …\n)" } else { "SELECT c.name AS category,\n       SUM(o.total) AS revenue\nFROM orders o\nJOIN products p ON …\nGROUP BY c.name;" }))
                            .child(button(("apply-ai", index), I::CornerDownLeft, tr!("Insert into editor")).w_full().justify_center().border_t_1().border_color(rgb(BORDER)).text_color(rgb(MINT))
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.editor.update(cx, |editor, cx| editor.replace_all(suggested, window, cx));
                                    this.notice = Some(Feedback::new(Tone::Info, tr!("Suggestion inserted. Review the SQL before running it."))); cx.notify();
                                })))))
                }))
                .when(self.executed && self.result.sales, |s| s.child(column().gap_2().p_3().rounded(px(7.)).border_1().border_color(rgb(0x34473f)).bg(rgb(0x1c2a26))
                    .child(row().child(icon(I::Lightbulb, MINT)).child(div().text_size(px(14.)).text_color(rgb(MINT)).font_weight(FontWeight::MEDIUM).child(tr!("A look at the data"))))
                    .child(div().text_size(px(14.)).line_height(relative(1.55)).text_color(rgb(0xb8c8c1)).child(tr!("Electronics and Home account for 52.7% of the quarter's revenue.")))
                    .child(button("ai-show-chart", I::ChartColumn, tr!("View distribution")).px_0().text_color(rgb(MINT))
                        .on_click(cx.listener(|this, _, _, cx| { this.result_view = ResultView::Chart; cx.notify(); }))))))
            .child(column().flex_none().p_3().gap_2().border_t_1().border_color(rgb(BORDER))
                .child(row().gap_1().children([("explain-chip", tr!("Explain SQL")), ("optimize-chip", tr!("Optimize")), ("chart-chip", tr!("Create chart"))].into_iter().map(|(id, text)| {
                    div().id(id).role(Role::Button).aria_label(text).focusable().tab_index(0).focus_visible(|s| s.border_color(rgb(MINT))).px_2().py(px(5.)).rounded(px(5.)).border_1().border_color(rgb(BORDER))
                        .text_size(px(12.)).text_color(rgb(MUTED)).cursor_pointer().hover(|s| s.bg(rgb(HOVER)).text_color(rgb(TEXT)))
                        .child(text).on_click(cx.listener(move |this, _, _, cx| this.ask(text.into(), cx)))
                })))
                .child(column().p_2().gap_2().rounded(px(7.)).border_1().border_color(rgb(0x3b4945)).bg(rgb(BG))
                    .child(Input::new(&self.prompt).appearance(false).bordered(false).small().w_full().aria_label(tr!("Message to the AI assistant")))
                    .child(row().justify_between().child(row().gap_1().child(icon(I::AtSign, FAINT)).child(label(tr!("Query context"))))
                        .child(icon_button("send-ai", I::ArrowUp, tr!("Send message")).bg(rgb(0x304a40))
                            .on_click(cx.listener(|this, _, window, cx| this.submit_prompt(window, cx))))))
                .child(div().text_center().text_size(px(12.)).text_color(rgb(FAINT)).child(tr!("Simulated AI · Always review the suggestions."))))
    }
}

/// Takes the columns and rows out of a driver result as display text. Its cost
/// grows with the rows, so it runs off the UI thread.
fn result_data(result: &mut backend::QueryResult) -> ResultData {
    let rows = std::mem::take(&mut result.rows);
    ResultData {
        columns: std::mem::take(&mut result.columns)
            .into_iter()
            .map(|column| (column.name, column.data_type))
            .collect(),
        rows: rows
            .iter()
            .map(|row| row.iter().map(backend::cell_text).collect())
            .collect(),
        sales: false,
        nulls: rows
            .iter()
            .enumerate()
            .flat_map(|(row_index, row)| {
                row.iter().enumerate().filter_map(move |(column, cell)| {
                    matches!(cell, Cell::Null).then_some((row_index, column))
                })
            })
            .collect(),
    }
}

/// The status each statement should show after an edit: it is kept only when
/// the statement at the same position still has the same text.
fn carry_statuses(
    old_sql: &[String],
    old_statuses: &[QueryStatus],
    statements: &[String],
) -> Vec<QueryStatus> {
    statements
        .iter()
        .enumerate()
        .map(|(index, sql)| {
            if old_sql.get(index) == Some(sql) {
                old_statuses.get(index).copied().unwrap_or_default()
            } else {
                QueryStatus::Ready
            }
        })
        .collect()
}

pub enum WorkspaceEvent {
    Saved {
        saved: bool,
        title: String,
        connection: String,
        sql: String,
    },
    Executed {
        title: String,
        connection: String,
        sql: String,
    },
}
impl EventEmitter<WorkspaceEvent> for Workspace {}

impl Workspace {
    pub fn rebind_connection(
        &mut self,
        name: String,
        input: Option<ResolvedConnection>,
        cx: &mut Context<Self>,
    ) {
        self.stop_execution(cx);
        self.connection = name.into();
        if let Some(input) = input {
            self.set_connection(input);
        } else {
            self.input = None;
            self.executed = false;
            self.set_result(sample_result(""));
        }
        self.notice = Some(Feedback::new(
            Tone::Warning,
            tr!("The tab's connection changed. Review the SQL before running it."),
        ));
        cx.notify();
    }

    pub fn connection_name(&self) -> &str {
        self.connection.as_ref()
    }

    pub fn update_connection(
        &mut self,
        name: String,
        input: ResolvedConnection,
        cx: &mut Context<Self>,
    ) {
        self.connection = name.into();
        // A rename or a new password keeps the server, so results, messages and
        // a running execution stay valid. Only a different server resets them.
        if self
            .input
            .as_ref()
            .is_some_and(|current| current.config == input.config)
        {
            self.input = Some(input);
        } else {
            self.stop_execution(cx);
            self.set_connection(input);
        }
        cx.notify();
    }

    pub fn set_connection(&mut self, input: ResolvedConnection) {
        if let Ok(driver) = builtin_registry().and_then(|registry| registry.get(input.config.driver))
        {
            self.dialect = driver.descriptor().display_name.into();
        }
        self.database = input.config.options["database"]
            .as_str()
            .or(input.config.options["defaultCatalog"].as_str())
            .unwrap_or("—")
            .to_owned();
        self.input = Some(input);
        self.assistant_open = false;
        self.executed = false;
        self.execution_error = None;
        self.selection = None;
        self.limited = false;
        self.set_result(ResultData {
            columns: Vec::new(),
            rows: Vec::new(),
            sales: false,
            nulls: Default::default(),
        });
        self.messages.clear();
    }

    fn set_result(&mut self, result: ResultData) {
        self.result = result;
        self.result_version += 1;
    }

    pub fn toggle_assistant(&mut self, cx: &mut Context<Self>) {
        self.assistant_open = !self.assistant_open;
        cx.notify();
    }

    fn query_area(&self, cx: &mut Context<Self>) -> Div {
        column()
            .size_full()
            .overflow_hidden()
            .child(self.toolbar(cx))
            .child(
                div().flex_1().min_h_0().child(
                    v_resizable("query-results-layout")
                        .with_state(&self.query_split)
                        .with_handle_appearance(splitter(
                            &self.query_split,
                            0,
                            tr!("SQL editor height. Use the arrows to adjust."),
                            200.0..900.0,
                        ))
                        .child(
                            resizable_panel()
                                .size(px(360.))
                                .size_range(px(200.)..Pixels::MAX)
                                .child(self.editor_panel(cx)),
                        )
                        .child(
                            resizable_panel()
                                .size_range(px(210.)..Pixels::MAX)
                                .child(self.result_panel(cx)),
                        ),
                ),
            )
            .when_some(self.notice.clone(), |s, notice| {
                s.child(
                    row()
                        .id("query-notice")
                        .role(if notice.tone == Tone::Error {
                            Role::Alert
                        } else {
                            Role::Status
                        })
                        .aria_label(notice.text.clone())
                        .h(px(38.))
                        .flex_none()
                        .px_3()
                        .bg(rgb(notice.tone.background()))
                        .justify_between()
                        .child(
                            row()
                                .flex_1()
                                .child(icon(notice.tone.icon(), notice.tone.color()))
                                .child(
                                    div()
                                        .id("query-notice-text")
                                        .flex_1()
                                        .text_size(px(14.))
                                        .text_color(rgb(notice.tone.color()))
                                        .truncate()
                                        .tooltip({
                                            let text = notice.text.clone();
                                            move |window, cx| {
                                                Tooltip::new(text.clone()).build(window, cx)
                                            }
                                        })
                                        .child(notice.text),
                                ),
                        )
                        .child(
                            icon_button("dismiss-notice", I::X, tr!("Close notice")).on_click(
                                cx.listener(|this, _, _, cx| {
                                    this.notice = None;
                                    cx.notify();
                                }),
                            ),
                        ),
                )
            })
    }
}

impl Render for Workspace {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let query = self.query_area(cx);
        let content = if self.assistant_open {
            h_resizable("assistant-layout")
                .with_state(&self.assistant_split)
                .with_handle_appearance(splitter(
                    &self.assistant_split,
                    1,
                    tr!("Assistant width. Use the arrows to adjust."),
                    280.0..560.0,
                ))
                .child(
                    resizable_panel()
                        .size_range(px(EDITOR_MIN_WIDTH)..Pixels::MAX)
                        .child(query),
                )
                .child(
                    resizable_panel()
                        .size(px(ASSISTANT_WIDTH))
                        .size_range(px(280.)..px(560.))
                        .fixed()
                        .child(self.assistant(cx)),
                )
                .into_any_element()
        } else {
            query.into_any_element()
        };
        column()
            .id("query-workspace")
            .size_full()
            .bg(rgb(BG))
            .child(content)
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        self.cancellation.cancel();
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CellSelection, GridCache, GridKey, GridView, QueryStatus, ResultData, carry_statuses,
        result_data, selection_block,
    };
    use jykre::backend;
    use jykre::cell_format::ValueFamily;
    use jykre::drivers::{Cell, ColumnInfo};
    use std::rc::Rc;
    use std::time::{Duration, Instant};

    #[test]
    fn selection_follows_the_displayed_order_and_hides_with_its_rows() {
        let selection = CellSelection {
            anchor: (4, 2),
            focus: (1, 0),
        };
        // Rows shown in reverse order: data rows 4..1 sit at positions 1..=4.
        let visible = [5, 4, 3, 2, 1, 0];
        assert_eq!(selection_block(selection, &visible), Some((1..=4, 0..=2)));
        // A filter that hides an end of the block hides the selection.
        assert_eq!(selection_block(selection, &[5, 4, 3]), None);
    }
    use jykre::sql::normalized_statements;

    #[test]
    fn edits_keep_the_status_of_unchanged_statements() {
        let before = normalized_statements("SELECT 1;\nSELECT 2;");
        let statuses = [QueryStatus::Success, QueryStatus::Error];
        // A new line after the last statement changes nothing.
        let after = normalized_statements("SELECT 1;\nSELECT 2;\n");
        assert_eq!(carry_statuses(&before, &statuses, &after), statuses);
        // Re-indenting keeps the statement; editing it resets only that one.
        let after = normalized_statements("SELECT\n  1;\nSELECT 3;");
        assert_eq!(
            carry_statuses(&before, &statuses, &after),
            [QueryStatus::Success, QueryStatus::Ready]
        );
        // A new statement starts ready; removed ones disappear.
        let after = normalized_statements("SELECT 1;\nSELECT 2;\nSELECT 4;");
        assert_eq!(
            carry_statuses(&before, &statuses, &after),
            [QueryStatus::Success, QueryStatus::Error, QueryStatus::Ready]
        );
        assert!(carry_statuses(&before, &statuses, &[]).is_empty());
    }

    /// Shaped like the `sales.event_log` seed, without a database.
    fn event_log(rows: usize) -> ResultData {
        ResultData {
            columns: [
                ("id", "int8"),
                ("level", "text"),
                ("message", "text"),
                ("occurred_at", "timestamptz"),
                ("payload", "jsonb"),
            ]
            .into_iter()
            .map(|(name, kind)| (name.into(), kind.into()))
            .collect(),
            rows: (0..rows)
                .map(|row| {
                    vec![
                        row.to_string(),
                        ["debug", "info", "warn", "error"][row % 4].into(),
                        format!("Synthetic event {row}"),
                        format!("2026-01-01 00:{:02}:00+00", row % 60),
                        format!("{{\"seq\": {row}}}"),
                    ]
                })
                .collect(),
            sales: false,
            nulls: Default::default(),
        }
    }

    fn key(result: u64, filter: &str, descending: bool) -> GridKey {
        GridKey {
            result,
            filter: filter.into(),
            descending,
            numbers_pt_br: false,
        }
    }

    #[test]
    fn grid_view_filters_sorts_and_sizes_columns_from_the_first_rows() {
        let mut result = event_log(300);
        result.rows[1][1] = "ERROR".into();
        let grid = GridView::new(&result, key(0, "", true));
        assert_eq!(grid.rows, (0..300).collect::<Vec<_>>());
        assert_eq!(
            grid.families,
            [
                ValueFamily::Number,
                ValueFamily::Text,
                ValueFamily::Text,
                ValueFamily::Temporal,
                ValueFamily::Text,
            ]
        );
        // The filter ignores case; ascending order reverses the rows.
        let grid = GridView::new(&result, key(0, "error", false));
        assert_eq!(grid.rows.len(), 76);
        assert_eq!(grid.rows.first(), Some(&299));
        assert_eq!(grid.rows.last(), Some(&1));
        // Only the first 200 rows size a column, so the cost does not grow
        // with the result.
        let width = GridView::new(&result, key(0, "", true)).widths[2];
        result.rows[250][2] = "x".repeat(80);
        assert_eq!(GridView::new(&result, key(0, "", true)).widths[2], width);
        result.rows[10][2] = "x".repeat(80);
        assert!(GridView::new(&result, key(0, "", true)).widths[2] > width);
    }

    #[test]
    fn grid_view_is_rebuilt_only_when_its_inputs_change() {
        let result = event_log(10);
        let cache = GridCache::default();
        let first = cache.get(&result, key(1, "", true));
        // Hover, scroll and selection frames change none of the inputs.
        assert!(Rc::ptr_eq(&first, &cache.get(&result, key(1, "", true))));
        for changed in [
            key(2, "", true),
            key(1, "info", true),
            key(1, "", false),
            GridKey {
                numbers_pt_br: true,
                ..key(1, "", true)
            },
        ] {
            let rebuilt = cache.get(&result, changed.clone());
            assert!(!Rc::ptr_eq(&first, &rebuilt));
            assert_eq!(rebuilt.key, changed);
        }
    }

    #[test]
    fn a_large_result_builds_its_grid_view_within_budget() {
        // A hundred times the default row limit, filtered and reversed: the
        // slowest build, paid once per change rather than once per frame.
        let result = event_log(100_000);
        let cache = GridCache::default();
        let start = Instant::now();
        let grid = cache.get(&result, key(1, "error", false));
        let elapsed = start.elapsed();
        assert_eq!(grid.rows.len(), 25_000);
        assert!(elapsed < Duration::from_secs(2), "took {elapsed:?}");
    }

    #[test]
    fn driver_rows_leave_the_query_result_as_text() {
        let column = |name: &str, data_type: &str| ColumnInfo {
            name: name.into(),
            data_type: data_type.into(),
            nullable: Some(true),
        };
        let mut result = backend::QueryResult {
            columns: vec![column("id", "int8"), column("note", "text")],
            rows: vec![
                vec![Cell::Integer(1), Cell::Text("NULL".into())],
                vec![Cell::Integer(2), Cell::Null],
            ],
            ..Default::default()
        };
        let data = result_data(&mut result);
        assert_eq!(
            data.columns,
            [("id".into(), "int8".into()), ("note".into(), "text".into())]
        );
        assert_eq!(data.value(1, 0), Some("2"));
        // The text "NULL" stays a value; only a real NULL is marked.
        assert_eq!(data.value(0, 1), Some("NULL"));
        assert_eq!(data.value(1, 1), None);
        // The driver cells moved out, so they are dropped where this ran.
        assert!(result.columns.is_empty() && result.rows.is_empty());
    }
}
