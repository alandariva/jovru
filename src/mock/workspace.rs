use super::data::{OPTIMIZED_SQL, ResultData, SALES, SALES_SQL, sample_result};
use super::layout::{ASSISTANT_WIDTH, EDITOR_MIN_WIDTH, splitter};
use super::ui::*;
use gpui_kit::assets::IconName as I;
use gpui_kit::component::input::{Editor, EditorState, Input, InputEvent, InputState};
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

#[derive(Clone, Copy, PartialEq)]
enum ResultView {
    Table,
    Chart,
    Messages,
}

#[derive(Clone, Copy, Default, PartialEq)]
enum QueryStatus {
    #[default]
    Ready,
    Running,
    Success,
    Error,
    Cancelled,
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
    dialect: &'static str,
    database: String,
    connection_color: u32,
    title: SharedString,
    result: ResultData,
    result_view: ResultView,
    assistant_open: bool,
    messages: Vec<Message>,
    executed: bool,
    running: bool,
    executions: usize,
    query_statuses: Vec<QueryStatus>,
    execution_id: usize,
    editor_sql: String,
    execution_error: Option<String>,
    saved: bool,
    notice: Option<String>,
    selected_cell: Option<(usize, usize)>,
    descending: bool,
    filter_open: bool,
    filter: Entity<InputState>,
    chat_scroll: ScrollHandle,
    result_scroll: ScrollHandle,
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
        let filter_subscription = cx.subscribe(&filter, |_, _, _: &InputEvent, cx| cx.notify());
        let resize_subscription = cx.observe(&assistant_split, |_, _, cx| cx.notify());
        let query_split = cx.new(|_| ResizableState::default());
        let query_resize = cx.observe(&query_split, |_, _, cx| cx.notify());
        let editor_observation = cx.observe(&editor, |this, editor, cx| {
            let sql = editor.read(cx).text().to_string();
            if this.editor_sql != sql {
                this.editor_sql = sql;
                this.cancellation.cancel();
                this.execution_id += 1;
                this.running = false;
                this.query_statuses.clear();
                this.execution_error = None;
                this.executed = false;
                this.notice = None;
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
            dialect: if connection.contains("Databricks") {
                "Databricks SQL"
            } else if connection.contains("MySQL") {
                "MySQL"
            } else {
                "PostgreSQL"
            },
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
            execution_id: 0,
            editor_sql: sql.to_string(),
            execution_error: None,
            saved: false,
            notice: None,
            selected_cell: None,
            descending: true,
            filter_open: false,
            chat_scroll: ScrollHandle::new(),
            result_scroll: ScrollHandle::new(),
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
        self.cancellation.cancel();
        self.execution_id += 1;
        self.running = false;
        for status in &mut self.query_statuses {
            if *status == QueryStatus::Running {
                *status = QueryStatus::Cancelled;
            }
        }
        self.notice = Some(tr!("Execution cancelled.").into());
        cx.notify();
    }

    fn execute_queries(&mut self, only: Option<usize>, cx: &mut Context<Self>) {
        if self.running {
            return;
        }
        let sql = self.editor.read(cx).text().to_string();
        let ranges = super::sql::statements(&sql);
        let queries: Vec<_> = ranges
            .iter()
            .enumerate()
            .filter(|(index, _)| only.is_none_or(|only| *index == only))
            .map(|(index, range)| (index, sql[range.clone()].to_string()))
            .collect();
        if queries.is_empty() {
            return;
        }
        self.query_statuses.resize(ranges.len(), QueryStatus::Ready);
        self.execution_id += 1;
        let execution_id = self.execution_id;
        self.running = true;
        self.execution_error = None;
        self.notice = None;
        cx.notify();
        if let Some(input) = self.input.clone() {
            self.cancellation = CancellationToken::new();
            let cancel = self.cancellation.clone();
            for (index, _) in &queries {
                self.query_statuses[*index] = QueryStatus::Running;
            }
            self.executed = false;
            cx.spawn(async move |weak, cx| {
                let requests = queries.iter().map(|(_, sql)| QueryRequest { sql: sql.clone(), options: ExecutionOptions::default() }).collect();
                let result = backend::run(backend::query(input, requests, cancel)).await;
                let _ = weak.update(cx, |this, cx| {
                    if this.execution_id != execution_id { return; }
                    this.running = false;
                    match result {
                        Ok(batch) => {
                            this.elapsed_ms = batch.completed.iter().map(|result| result.elapsed_ms).sum();
                            let completed = batch.completed.len();
                            for ((index, sql), result) in queries.iter().zip(batch.completed) {
                                this.query_statuses[*index] = QueryStatus::Success;
                                this.rows_affected = result.outcome.rows_affected;
                                this.notice = Some(if result.limited { tr!("Limit of 1,000 rows reached. Refine the SQL to get other rows.").into() } else { tr!("Query completed. {0} rows; {1} rows affected.", result.rows.len(), result.outcome.rows_affected.unwrap_or(0)) });
                                this.result = ResultData {
                                    columns: result.columns.into_iter().map(|column| (column.name, column.data_type)).collect(),
                                    rows: result.rows.iter().map(|row| row.iter().map(backend::cell_text).collect()).collect(),
                                    sales: false,
                                    nulls: result.rows.iter().enumerate().flat_map(|(row_index, row)| row.iter().enumerate().filter_map(move |(column, cell)| matches!(cell, Cell::Null).then_some((row_index, column)))).collect(),
                                };
                                this.executed = true;
                                this.executions += 1;
                                this.selected_cell = None;
                                this.result_view = ResultView::Table;
                                cx.emit(WorkspaceEvent::Executed { title: this.title.to_string(), connection: this.connection.to_string(), sql: sql.clone() });
                            }
                            if let Some(error) = batch.error {
                                for (position, (index, _)) in queries.iter().enumerate().skip(completed) {
                                    this.query_statuses[*index] = if position == completed { QueryStatus::Error } else { QueryStatus::Ready };
                                }
                                this.execution_error = Some(error.to_string());
                                this.notice = Some(tr!("{0} statement(s) completed before the failure: {1}. Previous statements were not rolled back automatically.", completed, error));
                            }
                        }
                        Err(error) => {
                            let status = if error.code == ErrorCode::Cancelled { QueryStatus::Cancelled } else { QueryStatus::Error };
                            for (index, _) in queries { this.query_statuses[index] = status; }
                            this.execution_error = Some(error.to_string());
                            this.notice = Some(error.to_string());
                        }
                    }
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
                        this.query_statuses[index] = QueryStatus::Running;
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
                        if let Some(error) = super::sql::preview_error(&sql) {
                            this.query_statuses[index] = QueryStatus::Error;
                            this.execution_error = Some(error.into());
                            this.notice = Some(tr!("Error in query {0}: {1}", index + 1, error));
                            this.executed = false;
                            this.running = false;
                            cx.notify();
                            return false;
                        }
                        this.query_statuses[index] = QueryStatus::Success;
                        this.result = sample_result(&sql);
                        this.selected_cell = None;
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
                this.notice =
                    Some(tr!("Simulated query completed. No real database was accessed.").into());
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
                            let formatted = super::sql::format(&sql);
                            this.editor
                                .update(cx, |editor, cx| editor.set_value(formatted, window, cx));
                            this.notice = Some(tr!("SQL formatted.").into());
                            cx.notify();
                        }),
                    )),
            )
            .child(
                row()
                    .child(
                        icon_button(
                            "save-query",
                            if self.saved {
                                I::BookmarkCheck
                            } else {
                                I::Bookmark
                            },
                            tr!("Save query"),
                        )
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.saved = !this.saved;
                            cx.emit(WorkspaceEvent::Saved {
                                saved: this.saved,
                                title: this.title.to_string(),
                                connection: this.connection.to_string(),
                                sql: this.editor.read(cx).text().to_string(),
                            });
                            this.notice = Some(
                                if this.saved {
                                    tr!("Query saved in this session.")
                                } else {
                                    tr!("Query removed from saved queries.")
                                }
                                .into(),
                            );
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
        let buttons: Vec<_> = super::sql::statements(&sql)
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
                            .child(label(tr!(
                                "{0} rows",
                                self.editor.read(cx).text().to_string().lines().count()
                            ))),
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
                                .when(view == ResultView::Table && self.executed, |s| {
                                    s.child(badge(self.result.rows.len().to_string(), MINT))
                                })
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
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.filter_open = !this.filter_open;
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
                                        this.notice = Some(
                                            tr!("CSV copied. Paste it into a spreadsheet or file.")
                                                .into(),
                                        );
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
                            .child(icon(I::LoaderCircle, MINT))
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
                            .child(icon(I::CircleX, 0xef8b91))
                            .child(label(error.clone()))
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
                    .child(
                        row()
                            .gap_2()
                            .child(icon(
                                if self.running {
                                    I::Square
                                } else if self.execution_error.is_some() {
                                    I::CircleX
                                } else if self.executed {
                                    I::CircleCheck
                                } else {
                                    I::Clock
                                },
                                if self.execution_error.is_some() {
                                    0xef8b91
                                } else {
                                    MINT
                                },
                            ))
                            .child(label(if self.running {
                                tr!("Running…").into()
                            } else if self.execution_error.is_some() {
                                tr!("Query error").into()
                            } else if self.executed {
                                tr!("{0} rows", self.result.rows.len())
                            } else {
                                tr!("Waiting to run").into()
                            }))
                            .child(label("·"))
                            .child(label(if self.executed {
                                format!("{} ms", self.elapsed_ms)
                            } else {
                                "—".into()
                            })),
                    )
                    .child(
                        row()
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

    fn result_table(&self, cx: &mut Context<Self>) -> AnyElement {
        let query = self.filter.read(cx).value().to_lowercase();
        let mut rows: Vec<_> = self
            .result
            .rows
            .iter()
            .enumerate()
            .filter(|(_, cells)| {
                cells
                    .iter()
                    .any(|cell| cell.to_lowercase().contains(&query))
            })
            .collect();
        if !self.descending {
            rows.reverse();
        }
        let empty = rows.is_empty();
        let numeric = |kind: &str| cell_format::family(kind) == ValueFamily::Number;
        let widths: Vec<_> = self
            .result
            .columns
            .iter()
            .enumerate()
            .map(|(column, (name, kind))| {
                let family = cell_format::family(kind);
                let content = self
                    .result
                    .rows
                    .iter()
                    .take(200)
                    .enumerate()
                    .map(|(row, _)| {
                        cell_format::display(
                            self.result.value(row, column),
                            family,
                            self.numbers_pt_br,
                        )
                        .chars()
                        .count()
                    })
                    .max()
                    .unwrap_or(0);
                ((content.max(name.chars().count() + 4) as f32 * 8.) + 32.).clamp(110., 360.)
            })
            .collect();
        let grid_width = widths.iter().sum::<f32>() + 35.;
        div()
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
                                    let tooltip = format!("{name} · {kind}");
                                    row()
                                        .id(("result-header", index))
                                        .w(px(widths[index]))
                                        .flex_none()
                                        .h_full()
                                        .px_3()
                                        .gap_2()
                                        .when(self.result.sales, |s| {
                                            s.flex_grow([1.5, 0.8, 1.2, 1.1, 0.65][index])
                                        })
                                        .border_l_1()
                                        .border_color(rgb(0x2a303b))
                                        .text_size(px(14.))
                                        .text_color(rgb(0xa0acbf))
                                        .tooltip(move |window, cx| {
                                            Tooltip::new(tooltip.clone()).build(window, cx)
                                        })
                                        .child(super::type_icons::header_icon(kind))
                                        .child(div().flex_1().truncate().child(name.clone()))
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
                    .child(
                        column()
                            .id("result-rows")
                            .flex_1()
                            .overflow_y_scroll()
                            .track_scroll(&self.result_scroll)
                            .vertical_scrollbar(&self.result_scroll)
                            .children(rows.into_iter().map(|(index, cells)| {
                                row()
                                    .id(("data-row", index))
                                    .role(Role::Row)
                                    .aria_label(cells.join(", "))
                                    .h(px(34.))
                                    .flex_none()
                                    .gap_0()
                                    .child(
                                        div()
                                            .w(px(35.))
                                            .flex_none()
                                            .h_full()
                                            .flex()
                                            .items_center()
                                            .justify_center()
                                            .bg(rgb(0x1b2028))
                                            .border_b_1()
                                            .border_color(rgb(0x2a303b))
                                            .text_center()
                                            .text_size(px(12.))
                                            .text_color(rgb(FAINT))
                                            .child((index + 1).to_string()),
                                    )
                                    .children(cells.iter().enumerate().map(|(col, value)| {
                                        let family =
                                            cell_format::family(&self.result.columns[col].1);
                                        let raw = self.result.value(index, col);
                                        let displayed =
                                            cell_format::display(raw, family, self.numbers_pt_br);
                                        let is_null = raw.is_none();
                                        let tooltip = raw.unwrap_or("NULL").to_owned();
                                        div()
                                            .id(ElementId::Name(
                                                format!("result-cell-{index}-{col}").into(),
                                            ))
                                            .role(Role::GridCell)
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
                                            .when(self.selected_cell != Some((index, col)), |s| {
                                                s.hover(|s| s.bg(rgb(0x222a36)))
                                            })
                                            .when(self.selected_cell == Some((index, col)), |s| {
                                                s.bg(rgb(0x283e65)).border_color(rgb(0x7fa8fa))
                                            })
                                            .on_click(cx.listener(move |this, _, _, cx| {
                                                this.selected_cell = Some((index, col));
                                                this.notice =
                                                    Some(this.result.rows[index][col].clone());
                                                cx.notify();
                                            }))
                                            .when(self.result.sales, |s| {
                                                s.flex_grow([1.5, 0.8, 1.2, 1.1, 0.65][col])
                                            })
                                            .font_family("Cascadia Mono")
                                            .text_size(px(13.))
                                            .when(is_null, |s| s.italic())
                                            .when(family == ValueFamily::Boolean, |s| {
                                                s.justify_center()
                                            })
                                            .tooltip(move |window, cx| {
                                                Tooltip::new(tooltip.clone()).build(window, cx)
                                            })
                                            .when(numeric(&self.result.columns[col].1), |s| {
                                                s.justify_end()
                                                    .font_family("Cascadia Mono")
                                                    .text_size(px(13.))
                                            })
                                            .text_color(rgb(if is_null {
                                                0x64748b
                                            } else if numeric(&self.result.columns[col].1) {
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
                            }))
                            .when(empty, |s| {
                                s.child(
                                    column()
                                        .p_6()
                                        .gap_2()
                                        .child(label(tr!("No results for this selection.")))
                                        .child(label(tr!(
                                            "The query or the current filter returned no rows."
                                        ))),
                                )
                            }),
                    ),
            )
            .into_any_element()
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
                                    .on_click(cx.listener(move |this, _, _, cx| { cx.write_to_clipboard(ClipboardItem::new_string(suggested.into())); this.notice = Some(tr!("Suggested SQL copied.").into()); cx.notify(); }))))
                            .child(div().px_3().py_3().font_family("Cascadia Mono").text_size(px(12.)).line_height(relative(1.6)).text_color(rgb(BLUE))
                                .child(if suggested == OPTIMIZED_SQL { "WITH sales AS (\n  SELECT product_id,\n         SUM(total) AS revenue\n  FROM orders\n  …\n)" } else { "SELECT c.name AS category,\n       SUM(o.total) AS revenue\nFROM orders o\nJOIN products p ON …\nGROUP BY c.name;" }))
                            .child(button(("apply-ai", index), I::CornerDownLeft, tr!("Insert into editor")).w_full().justify_center().border_t_1().border_color(rgb(BORDER)).text_color(rgb(MINT))
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.editor.update(cx, |editor, cx| editor.set_value(suggested, window, cx));
                                    this.notice = Some(tr!("Suggestion inserted. Review the SQL before running it.").into()); cx.notify();
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
            self.result = sample_result("");
        }
        self.notice =
            Some(tr!("The tab's connection changed. Review the SQL before running it.").into());
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
        if self.running {
            self.stop_execution(cx);
        }
        self.connection = name.into();
        self.set_connection(input);
        cx.notify();
    }

    pub fn set_connection(&mut self, input: ResolvedConnection) {
        self.dialect = match input.config.driver {
            DriverKind::MySql => "MySQL",
            DriverKind::Databricks => "Databricks SQL",
            _ => "PostgreSQL",
        };
        self.database = input.config.options["database"]
            .as_str()
            .or(input.config.options["defaultCatalog"].as_str())
            .unwrap_or("—")
            .to_owned();
        self.input = Some(input);
        self.assistant_open = false;
        self.executed = false;
        self.result = ResultData {
            columns: Vec::new(),
            rows: Vec::new(),
            sales: false,
            nulls: Default::default(),
        };
        self.messages.clear();
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
            .when_some(self.notice.clone(), |s, text| {
                s.child(
                    row()
                        .h(px(38.))
                        .flex_none()
                        .px_3()
                        .bg(rgb(0x21322b))
                        .justify_between()
                        .child(
                            div()
                                .text_size(px(14.))
                                .text_color(rgb(MINT))
                                .truncate()
                                .child(text),
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
