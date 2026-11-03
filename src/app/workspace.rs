use super::demo::{self, OPTIMIZED_SQL, SALES, SALES_SQL, sample_result};
use super::layout::{ASSISTANT_WIDTH, EDITOR_MIN_WIDTH, splitter};
use super::result_grid::{ResultGrid, ResultGridEvent};
use super::ui::*;
use gpui_kit::assets::IconName as I;
use gpui_kit::component::input::{Editor, EditorState, Input, InputEvent, InputState};
use gpui_kit::component::resizable::{ResizableState, h_resizable, resizable_panel, v_resizable};
use gpui_kit::component::scroll::ScrollableElement;
use gpui_kit::component::{ActiveTheme, Sizable, tooltip::Tooltip};
use gpui_kit::{prelude::FluentBuilder as _, *};
use jykre::tr;
use jykre::{
    backend, cell_format,
    drivers::*,
    query::{Outcome, QueryRun, RunState, StatementStatus},
    result::{DEFAULT_MEMORY_CEILING, DEFAULT_ROW_LIMIT, ResultData, ResultLimit},
};
use std::rc::Rc;

#[derive(Clone, Copy, PartialEq)]
enum ResultView {
    Table,
    Chart,
    Messages,
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
    /// Shared with the views that read it; replaced, never copied.
    result: Rc<ResultData>,
    /// The demo result has a chart and an assistant analysis.
    sales: bool,
    /// Rows kept per statement, from the connection.
    row_limit: u32,
    /// The result grid, its own entity ([ADR-0012](../../docs/adr/0012-result-grid-as-a-cached-entity.md)).
    grid: Entity<ResultGrid>,
    /// Visible cells selected in the grid, as it last reported.
    selected_cells: usize,
    result_view: ResultView,
    assistant_open: bool,
    messages: Vec<Message>,
    /// A result is loaded (from a run or the demo), whatever the run state.
    has_result: bool,
    /// The run state and the status of each statement in the editor.
    run: QueryRun,
    editor_sql: String,
    /// The line where each statement of `editor_sql` starts and its line
    /// count, kept when the SQL changes so a frame never splits the script.
    statement_lines: Rc<[usize]>,
    line_count: usize,
    /// SQL as last saved to the library, if this tab is saved.
    saved_sql: Option<String>,
    notice: Option<Feedback>,
    /// The limit that stopped the last statement's result, if any.
    limit: Option<ResultLimit>,
    filter_open: bool,
    filter: Entity<InputState>,
    chat_scroll: ScrollHandle,
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
            if matches!(event, InputEvent::Change) && this.filter_open {
                let filter = this.filter.read(cx).value().to_string();
                this.grid
                    .update(cx, |grid, cx| grid.set_filter(&filter, cx));
            }
        });
        let result = Rc::new(sample_result(sql));
        let sales = demo::is_sales(sql);
        let grid = cx.new(|cx| ResultGrid::new(result.clone(), sales, cx));
        // The grid notifies only itself; the workspace keeps the little it
        // shows: the selected count and notices.
        let grid_subscription =
            cx.subscribe(&grid, |this, _, event: &ResultGridEvent, cx| match event {
                ResultGridEvent::SelectionChanged { cells } => {
                    this.selected_cells = *cells;
                    cx.notify();
                }
                ResultGridEvent::Notice(notice) => {
                    this.notice = Some(notice.clone());
                    cx.notify();
                }
            });
        let resize_subscription = cx.observe(&assistant_split, |_, _, cx| cx.notify());
        let query_split = cx.new(|_| ResizableState::default());
        let query_resize = cx.observe(&query_split, |_, _, cx| cx.notify());
        // Editing the SQL never discards results, messages or a running
        // execution. Only the gutter status of statements that changed resets.
        let editor_observation = cx.observe(&editor, |this, editor, cx| {
            let sql = editor.read(cx).text().to_string();
            if this.editor_sql != sql {
                this.run.edit(&sql);
                this.statement_lines = jykre::sql::statement_lines(&sql).into();
                this.line_count = sql.split('\n').count();
                this.editor_sql = sql;
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
            result,
            sales,
            row_limit: DEFAULT_ROW_LIMIT,
            grid,
            selected_cells: 0,
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
            has_result: sql == SALES_SQL,
            run: QueryRun::new(sql),
            editor_sql: sql.to_string(),
            statement_lines: jykre::sql::statement_lines(sql).into(),
            line_count: sql.split('\n').count(),
            saved_sql: None,
            notice: None,
            limit: None,
            filter_open: false,
            chat_scroll: ScrollHandle::new(),
            _subscriptions: vec![
                prompt_subscription,
                filter_subscription,
                grid_subscription,
                resize_subscription,
                query_resize,
                editor_observation,
            ],
        }
    }

    pub fn execute(&mut self, cx: &mut Context<Self>) {
        if self.run.is_running() {
            self.stop_execution(cx);
        } else {
            self.execute_queries(None, cx);
        }
    }

    /// Asks the run to stop. The footer reports the cancellation; there is
    /// no second message.
    fn stop_execution(&mut self, cx: &mut Context<Self>) {
        if self.run.cancel() {
            self.cancellation.cancel();
            cx.notify();
        }
    }

    /// A result is shown: loaded, and the last run did not fail.
    fn has_results(&self) -> bool {
        self.has_result && !matches!(self.run.state(), RunState::Failed { .. })
    }

    fn execute_queries(&mut self, only: Option<usize>, cx: &mut Context<Self>) {
        let sql = self.editor.read(cx).text().to_string();
        let Some(plan) = self.run.start(&sql, only) else {
            if !self.run.is_running() {
                self.notice = Some(Feedback::new(
                    Tone::Info,
                    tr!("There is no SQL statement to run."),
                ));
                cx.notify();
            }
            return;
        };
        let id = plan.id;
        let queries = plan.statements;
        // A new run releases the previous result before the next one arrives.
        self.sales = false;
        self.set_result(ResultData::default(), cx);
        self.has_result = false;
        self.limit = None;
        self.notice = None;
        cx.notify();
        if let Some(input) = self.input.clone() {
            // A superseded run that is still stopping keeps its own token.
            self.cancellation = CancellationToken::new();
            let cancel = self.cancellation.clone();
            self.run.begin_all(id);
            let options = ExecutionOptions {
                max_rows: self.row_limit,
                ..ExecutionOptions::default()
            };
            cx.spawn(async move |weak, cx| {
                let requests = queries
                    .iter()
                    .map(|(_, sql)| QueryRequest {
                        sql: sql.clone(),
                        options: options.clone(),
                    })
                    .collect();
                // Rows become compact display text on the database runtime, row
                // by row, and the driver cells are dropped there.
                let result = backend::run(backend::query(
                    input,
                    requests,
                    DEFAULT_MEMORY_CEILING,
                    cancel,
                ))
                .await;
                let _ = weak.update(cx, |this, cx| {
                    let _timer = jykre::logging::UiTimer::start("publish result");
                    match result {
                        Ok(batch) => {
                            let completed = batch.completed.len();
                            let elapsed_ms =
                                batch.completed.iter().map(|result| result.elapsed_ms).sum();
                            let outcome = Outcome::Batch {
                                completed,
                                error: batch.error,
                            };
                            if this.run.finish(id, outcome) {
                                this.elapsed_ms = elapsed_ms;
                                for ((_, sql), result) in queries.iter().zip(batch.completed) {
                                    this.rows_affected = result.outcome.rows_affected;
                                    this.limit = result.limit;
                                    this.set_result(result.data, cx);
                                    this.has_result = true;
                                    this.result_view = ResultView::Table;
                                    cx.emit(WorkspaceEvent::Executed {
                                        title: this.title.to_string(),
                                        connection: this.connection.to_string(),
                                        sql: sql.clone(),
                                    });
                                }
                            }
                        }
                        Err(error) => {
                            this.run.finish(id, Outcome::Failed(error));
                        }
                    }
                    cx.notify();
                });
            })
            .detach();
            return;
        }
        cx.spawn(async move |weak, cx| {
            for (index, sql) in queries {
                let active = weak
                    .update(cx, |this, cx| {
                        cx.notify();
                        this.run.begin_statement(id, index)
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
                        cx.notify();
                        let error = jykre::sql::preview_error(&sql);
                        if !this.run.finish_statement(id, index, error) {
                            if error.is_some() {
                                this.has_result = false;
                            }
                            return false;
                        }
                        this.sales = demo::is_sales(&sql);
                        this.set_result(sample_result(&sql), cx);
                        this.limit = None;
                        this.rows_affected = None;
                        this.has_result = true;
                        this.result_view = ResultView::Table;
                        cx.emit(WorkspaceEvent::Executed {
                            title: this.title.to_string(),
                            connection: this.connection.to_string(),
                            sql,
                        });
                        true
                    })
                    .unwrap_or(false);
                if !keep_running {
                    return;
                }
            }
            let _ = weak.update(cx, |this, cx| {
                this.run.complete(id);
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
            text: if self.sales { answer.into() } else { tr!("The query runs in the context of {0}. The sample preview has {1} columns and {2} rows. Use the editor to adjust the filters and run it to see the sample. This demo's assistant has a complete analysis flow in the Revenue by category tab.", self.connection, self.result.column_count(), self.result.row_count()) },
            sql: if self.sales { sql } else { None },
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
                            if self.run.is_running() {
                                I::Square
                            } else {
                                I::Play
                            },
                            if self.run.is_running() {
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

    /// Run buttons beside the first line of each statement. Only statements
    /// on screen get a button, and their lines come from `statement_lines`,
    /// so a frame costs the same for a short query or a long script.
    fn query_gutter(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let editor = self.editor.clone();
        let lines = self.statement_lines.clone();
        let workspace = cx.weak_entity();
        canvas(
            move |bounds, window, cx| {
                let on_screen: Vec<(usize, Pixels)> = {
                    let state = editor.read(cx);
                    // Paint-time bounds still belong to the previous frame here.
                    // Use the editor's fresh prepaint snapshot so the gutter and
                    // its hitboxes follow the same scroll offset as the SQL.
                    let Some(geometry) = state.prepaint_caret_geometry() else {
                        return Vec::new();
                    };
                    let height = geometry.line_height();
                    let first = geometry.input_bounds().top()
                        + geometry.scroll_offset().y
                        + (height - px(22.)) / 2.;
                    let map = state.display_map();
                    lines
                        .iter()
                        .enumerate()
                        .filter(|(_, line)| !map.is_buffer_line_hidden(**line))
                        .map(|(index, line)| {
                            (
                                index,
                                first + height * map.buffer_line_to_display_row(*line) as f32,
                            )
                        })
                        // Statements are in line order: after the first one
                        // below the editor, none is on screen.
                        .take_while(|(_, top)| *top < bounds.bottom())
                        .filter(|(_, top)| {
                            *top >= bounds.top() && *top + px(22.) <= bounds.bottom()
                        })
                        .collect()
                };
                let mut visible = Vec::with_capacity(on_screen.len());
                for (index, top) in on_screen {
                    let Some(this) = workspace.upgrade() else {
                        break;
                    };
                    let status = this.read(cx).run.status(index);
                    let mut button = run_button(index, status, workspace.clone());
                    button.layout_as_root(AvailableSpace::min_size(), window, cx);
                    button.prepaint_at(point(bounds.left(), top), window, cx);
                    visible.push(button);
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
                            .child(label(match self.line_count {
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
                                        && !self.run.is_running(),
                                    |s| s.child(badge(self.result.row_count().to_string(), MINT)),
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
                                        let numbers_pt_br = this.numbers_pt_br;
                                        this.grid.update(cx, |grid, cx| {
                                            grid.set_number_format(numbers_pt_br, cx)
                                        });
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
                                        // A closed filter bar must not keep hiding rows.
                                        let filter = if this.filter_open {
                                            this.filter.read(cx).value().to_string()
                                        } else {
                                            String::new()
                                        };
                                        this.grid
                                            .update(cx, |grid, cx| grid.set_filter(&filter, cx));
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
                                        let _timer = jykre::logging::UiTimer::start("copy CSV");
                                        let csv = cell_format::result_csv(&this.result);
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
                    .child(if self.run.is_running() {
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
                    } else if let RunState::Failed { error } = self.run.state() {
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
                    } else if !self.has_result {
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
                            // A cached view reuses the grid's last frame
                            // while only the workspace changes.
                            ResultView::Table => self
                                .grid
                                .clone()
                                .cached(StyleRefinement::default().size_full())
                                .into_any_element(),
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
                        let rows = match self.result.row_count() {
                            1 => tr!("1 row").to_string(),
                            count => tr!("{0} rows", count),
                        };
                        // The footer is the single place that reports how the last run
                        // ended; errors also fill the results area with their message.
                        let state = self.run.state();
                        let (symbol, color, text, detail): (I, u32, String, Option<String>) = if let RunState::Running { .. } = state
                        {
                            (I::LoaderCircle, AMBER, tr!("Running…").into(), None)
                        } else if *state == RunState::Cancelling {
                            (I::LoaderCircle, AMBER, tr!("Cancelling…").into(), None)
                        } else if let RunState::Failed { .. } = state {
                            (I::CircleX, RED, tr!("Query error").into(), None)
                        } else if *state == RunState::Cancelled {
                            (I::Ban, AMBER, tr!("Execution cancelled").into(), None)
                        } else if let (true, Some(limit)) = (self.has_result, self.limit) {
                            // The footer says which limit stopped reading.
                            let (text, detail) = match limit {
                                ResultLimit::Rows(rows_limit) => (
                                    tr!("{0} · row limit reached", rows),
                                    tr!(
                                        "Limit of {0} rows reached. Refine the SQL to get other rows.",
                                        cell_format::count(rows_limit as usize)
                                    ),
                                ),
                                ResultLimit::Memory(bytes) => (
                                    tr!("{0} · memory limit reached", rows),
                                    tr!(
                                        "The result reached the memory limit of {0} MB before the row limit. Refine the SQL to get other rows.",
                                        bytes / (1024 * 1024)
                                    ),
                                ),
                            };
                            (I::TriangleAlert, AMBER, text, Some(detail))
                        } else if self.has_result {
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
                                s.tooltip(move |window, cx| {
                                    Tooltip::new(detail.clone()).build(window, cx)
                                })
                            })
                            .map(|s| {
                                if self.run.is_active() {
                                    s.child(spinning_icon("run-status-spinner", symbol, color))
                                } else {
                                    s.child(icon(symbol, color))
                                }
                            })
                            .child(div().text_size(px(12.)).text_color(rgb(color)).child(text))
                            .when(
                                self.has_results() && !self.run.is_active() && *self.run.state() != RunState::Cancelled,
                                |s| {
                                    s.child(label("·"))
                                        .child(label(format!("{} ms", self.elapsed_ms)))
                                },
                            )
                    })
                    .child(
                        row()
                            .when(self.has_results() && !self.run.is_running(), |s| {
                                let selected = self.selected_cells;
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

    fn result_chart(&self, _: &mut Context<Self>) -> AnyElement {
        if !self.sales {
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
                self.run.executions(),
                self.result.row_count(),
                self.elapsed_ms
            )))
            .child(label(if self.input.is_some() {
                tr!(
                    "Server execution · limit of {0} rows per statement · 300 s timeout",
                    cell_format::count(self.row_limit as usize)
                )
            } else {
                tr!("Visual demo results.").into()
            }))
            .child(label(match self.run.state() {
                RunState::Failed { error } => error.clone(),
                _ if self.input.is_some() => {
                    tr!("{0} rows affected", self.rows_affected.unwrap_or(0))
                }
                _ => tr!("No changes to a real database.").into(),
            }))
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
                .child(badge(tr!("Current SQL"), MUTED)).child(badge(if self.sales { tr!("3 tables") } else { tr!("1 query") }, MUTED)))
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
                .when(self.has_result && self.sales, |s| s.child(column().gap_2().p_3().rounded(px(7.)).border_1().border_color(rgb(0x34473f)).bg(rgb(0x1c2a26))
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
        row_limit: u32,
        cx: &mut Context<Self>,
    ) {
        self.connection = name.into();
        if let Some(input) = input {
            self.set_connection(input, row_limit, cx);
        } else {
            self.abandon_run();
            self.input = None;
            self.has_result = false;
            self.sales = false;
            self.set_result(sample_result(""), cx);
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
        row_limit: u32,
        cx: &mut Context<Self>,
    ) {
        self.connection = name.into();
        // The new limit applies from the next run.
        self.row_limit = row_limit;
        // A rename or a new password keeps the server, so results, messages and
        // a running execution stay valid. Only a different server resets them.
        if self
            .input
            .as_ref()
            .is_some_and(|current| current.config == input.config)
        {
            self.input = Some(input);
        } else {
            self.set_connection(input, row_limit, cx);
        }
        cx.notify();
    }

    /// Stops the run and forgets it: its outcome no longer applies.
    fn abandon_run(&mut self) {
        self.cancellation.cancel();
        self.run.reset();
    }

    /// Binds the tab to another server. Any run is abandoned and the
    /// previous result cleared.
    pub fn set_connection(
        &mut self,
        input: ResolvedConnection,
        row_limit: u32,
        cx: &mut Context<Self>,
    ) {
        self.abandon_run();
        self.row_limit = row_limit;
        if let Ok(driver) =
            builtin_registry().and_then(|registry| registry.get(input.config.driver))
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
        self.has_result = false;
        self.limit = None;
        self.sales = false;
        self.set_result(ResultData::default(), cx);
        self.messages.clear();
    }

    /// Replaces the result. The previous one is freed on the database runtime
    /// when nothing else holds it, so a large result never stalls a frame.
    fn set_result(&mut self, result: ResultData, cx: &mut Context<Self>) {
        let result = Rc::new(result);
        let sales = self.sales;
        // The grid lets go of the previous result first, so this is its last
        // holder.
        self.grid
            .update(cx, |grid, cx| grid.set_result(result.clone(), sales, cx));
        let previous = std::mem::replace(&mut self.result, result);
        if let Ok(previous) = Rc::try_unwrap(previous) {
            backend::release(previous);
        }
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

/// The gutter button of the statement at `index`: it runs that statement,
/// or stops the run while the statement is running.
fn run_button(
    index: usize,
    status: StatementStatus,
    workspace: WeakEntity<Workspace>,
) -> AnyElement {
    let (symbol, color, title) = match status {
        StatementStatus::Ready => (I::Play, MINT, tr!("Run this query")),
        StatementStatus::Running => (I::Square, 0xe7bd7b, tr!("Stop execution")),
        StatementStatus::Success => (I::CircleCheck, MINT, tr!("Success · run again")),
        StatementStatus::Error => (I::CircleX, 0xef8b91, tr!("Error · run again")),
        StatementStatus::Cancelled => (I::Play, FAINT, tr!("Cancelled · run again")),
    };
    div()
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
        .on_click(move |_, _, cx| {
            let _ = workspace.update(cx, |this, cx| {
                if this.run.status(index) == StatementStatus::Running {
                    this.stop_execution(cx);
                } else {
                    this.execute_queries(Some(index), cx);
                }
            });
        })
        .into_any_element()
}

impl Render for Workspace {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        super::probe::rendered("Workspace");
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
