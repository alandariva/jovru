//! The job tab's toolbar, its inner tabs and its **Editor**: a SQL Job, a
//! program or a chain as a form, each stating the engine's rules where
//! they apply (specification, section 8.1.1).
use super::*;
use crate::app::report_panel::{PickSource, source_menu};
use crate::app::report_view::{invalid_hint, kind_name as parameter_kind};
use crate::app::workspace::SQL_EDITOR_CONTEXT;
use gpui_kit::assets::IconName as I;
use gpui_kit::component::Sizable;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Editor, Input};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use jykre::automation::definition::{
    DEFAULT_CHAIN_TIMEOUT_MS, DEFAULT_TIMEOUT_MS, MAX_OUTPUTS, MAX_STEPS, MIN_STEPS, RowRule,
};
use jykre::parameters::KINDS;
use jykre::reports::ConnectionRef;

const VERSION_ROW: f32 = 44.;

/// A section of the form: a title and its rows.
fn section(title: &'static str) -> Div {
    column()
        .flex_none()
        .gap_2()
        .p_3()
        .rounded(px(8.))
        .border_1()
        .border_color(rgb(colors().border))
        .child(
            div()
                .text_size(px(13.))
                .font_weight(FontWeight::SEMIBOLD)
                .child(title),
        )
}

/// A rule the user must be able to see, where it applies.
fn note(text: impl Into<SharedString>) -> Div {
    row()
        .items_start()
        .text_size(px(12.))
        .text_color(rgb(colors().muted))
        .child(div().pt(px(1.)).child(icon(I::Info, colors().faint)))
        .child(div().flex_1().min_w_0().child(text.into()))
}

fn warning(text: impl Into<SharedString>) -> Div {
    row()
        .items_start()
        .text_size(px(12.))
        .text_color(rgb(colors().amber))
        .child(
            div()
                .pt(px(1.))
                .child(icon(I::TriangleAlert, colors().amber)),
        )
        .child(div().flex_1().min_w_0().child(text.into()))
}

fn field(label_text: &'static str, child: impl IntoElement) -> Div {
    column().gap_1().child(label(label_text)).child(child)
}

fn check(id: impl Into<ElementId>, on: bool, title: &'static str) -> Stateful<Div> {
    button(id, if on { I::SquareCheck } else { I::Square }, title)
        .role(Role::CheckBox)
        .aria_toggled(if on { Toggled::True } else { Toggled::False })
        .when(on, |s| s.text_color(rgb(colors().mint)))
}

impl JobView {
    pub(super) fn toolbar(&self, cx: &mut Context<Self>) -> Div {
        let running = self.running();
        let writable = self.writable();
        row()
            .h(px(48.))
            .flex_none()
            .px_3()
            .gap_2()
            .border_b_1()
            .border_color(rgb(colors().border))
            .child(icon(super::super::kind_icon(self.kind), colors().mint))
            .child(
                div()
                    .text_size(px(15.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .truncate()
                    .child(self.title()),
            )
            .child(badge(words::kind_name(self.kind), colors().faint))
            .when(!writable, |s| {
                s.child(badge(tr!("Read-only"), colors().amber))
            })
            .child(div().flex_1())
            .when(self.busy, |s| {
                s.child(spinning_icon("job-busy", I::LoaderCircle, colors().amber))
            })
            .child(
                icon_button(
                    "job-save",
                    if self.unsaved { I::Save } else { I::BookmarkCheck },
                    if self.unsaved {
                        tr!("Save the job")
                    } else {
                        tr!("Saved")
                    },
                )
                .on_click(cx.listener(|this, _, _, cx| this.save(cx))),
            )
            .when(self.kind != JobKind::Chain, |s| {
                s.child(
                    button("job-publish", I::Upload, tr!("Publish"))
                        .on_click(cx.listener(|this, _, _, cx| this.publish(cx))),
                )
            })
            .child(
                icon_button("job-duplicate", I::Copy, tr!("Duplicate the job"))
                    .on_click(cx.listener(|this, _, _, cx| this.duplicate(cx))),
            )
            .child(
                icon_button("job-delete", I::Trash, tr!("Delete the job")).on_click(cx.listener(
                    |this, _, _, cx| {
                        let name = this.title();
                        cx.emit(JobEvent::Confirm {
                            title: tr!("Delete {0}?", name),
                            description: tr!(
                                "The job and its published versions are deleted; its runs stay in the history. Refused while a chain uses it, naming the chains."
                            )
                            .to_owned(),
                            ok: tr!("Delete"),
                            what: Ask::DeleteJob,
                        });
                    },
                )),
            )
            .child(divider())
            .child(if running {
                button("job-stop", I::Square, tr!("Stop"))
                    .on_click(cx.listener(|this, _, _, cx| this.stop(cx)))
            } else {
                primary_button("job-run", I::Play, tr!("Run")).on_click(cx.listener(
                    |this, _, window, cx| {
                        if this.inner != Inner::Inputs && !this.values.is_empty() {
                            this.set_inner(Inner::Inputs, window, cx);
                        }
                        this.run(cx)
                    },
                ))
            })
    }

    pub(super) fn inner_tabs(&self, cx: &mut Context<Self>) -> Stateful<Div> {
        let tab = |id: &'static str, inner: Inner, title: &'static str, this: &Self| {
            let selected = this.inner == inner;
            div()
                .id(id)
                .role(Role::Tab)
                .aria_label(title)
                .aria_selected(selected)
                .focusable()
                .tab_index(0)
                .focus_visible(|s| s.border_color(rgb(colors().mint)))
                .px_3()
                .h(px(32.))
                .flex()
                .items_center()
                .border_b_2()
                .border_color(if selected {
                    rgb(colors().mint)
                } else {
                    transparent_black().into()
                })
                .text_color(rgb(if selected {
                    colors().text
                } else {
                    colors().muted
                }))
                .cursor_pointer()
                .hover(|s| s.text_color(rgb(colors().text)))
                .child(title)
        };
        row()
            .id("job-inner-tabs")
            .role(Role::TabList)
            .aria_label(tr!("Job"))
            .flex_none()
            .px_3()
            .gap_1()
            .border_b_1()
            .border_color(rgb(colors().border))
            .child(
                tab("job-tab-editor", Inner::Editor, tr!("Editor"), self).on_click(
                    cx.listener(|this, _, window, cx| this.set_inner(Inner::Editor, window, cx)),
                ),
            )
            .child(
                tab("job-tab-inputs", Inner::Inputs, tr!("Inputs"), self).on_click(
                    cx.listener(|this, _, window, cx| this.set_inner(Inner::Inputs, window, cx)),
                ),
            )
            .child(
                tab("job-tab-runs", Inner::Runs, tr!("Runs"), self).on_click(
                    cx.listener(|this, _, window, cx| this.set_inner(Inner::Runs, window, cx)),
                ),
            )
    }

    pub(super) fn editor_panel(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let _ = window;
        let definition = &self.definition;
        column()
            .id("job-editor")
            .role(Role::TabPanel)
            .aria_label(tr!("Editor"))
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .track_scroll(&self.editor_scroll)
            .p_3()
            .gap_3()
            .child(
                section(tr!("Job"))
                    .child(field(
                        tr!("Name"),
                        Input::new(&self.name).small().aria_label(tr!("Job name")),
                    ))
                    .child(field(
                        tr!("Description"),
                        Input::new(&self.description)
                            .small()
                            .aria_label(tr!("Job description")),
                    )),
            )
            .map(|s| match &definition.body {
                JobBody::Sql(_) => s
                    .child(self.sql_section(cx))
                    .child(self.specs_section(false, cx))
                    .child(self.limits_section(cx))
                    .child(self.outputs_section(cx))
                    .child(self.versions_section(cx)),
                JobBody::Process(process) => {
                    let jjp = process.protocol == Protocol::Jjp1;
                    s.child(self.program_section(cx))
                        .child(self.environment_section(cx))
                        .child(self.protocol_section(cx))
                        .when(jjp, |s| s.child(self.specs_section(true, cx)))
                        .child(self.limits_section(cx))
                        .when(jjp, |s| s.child(self.outputs_section(cx)))
                        .child(self.versions_section(cx))
                }
                JobBody::Chain(_) => s
                    .child(self.specs_section(true, cx))
                    .child(self.limits_section(cx))
                    .child(self.steps_section(cx)),
            })
    }

    // -----------------------------------------------------------------------
    // SQL.

    fn sql_section(&self, cx: &mut Context<Self>) -> Div {
        let JobBody::Sql(body) = &self.definition.body else {
            return div();
        };
        let Some(fields) = &self.sql else {
            return div();
        };
        let source = body
            .connection
            .and_then(|id| source_of(&self.sources.sources, id));
        let where_label = match source {
            Some(source) => jykre::reports::source_label(&source.connection.name, &body.context),
            None if body.connection.is_some() => {
                tr!("A connection that no longer exists").to_owned()
            }
            None => tr!("Choose a connection").to_owned(),
        };
        let view = cx.entity().downgrade();
        let sources = self.sources.sources.clone();
        let current = source.map(|source| (source.connection.clone(), body.context.clone()));
        let pick: PickSource = Rc::new(move |connection: ConnectionRef, context, _, cx| {
            let _ = view.update(cx, |this, cx| this.set_connection(connection, context, cx));
        });
        let editor = fields.editor.clone();
        section(tr!("Statement"))
            .child(field(
                tr!("Connection and where unqualified names resolve"),
                Button::new("job-connection")
                    .ghost()
                    .small()
                    .label(where_label)
                    .dropdown_caret(true)
                    .accessibility_label(tr!("The job's connection and database"))
                    .dropdown_menu(move |menu, window, cx| {
                        source_menu(
                            menu.label(tr!("Where the job runs")),
                            window,
                            cx,
                            &sources,
                            current.clone(),
                            pick.clone(),
                        )
                    }),
            ))
            .child(
                div()
                    .h(px(220.))
                    .rounded(px(6.))
                    .border_1()
                    .border_color(rgb(if fields.extra.is_empty() {
                        colors().border
                    } else {
                        colors().amber
                    }))
                    .key_context(SQL_EDITOR_CONTEXT)
                    .on_action(cx.listener(|this, _: &crate::app::ToggleComment, window, cx| {
                        if let Some(fields) = &this.sql {
                            fields
                                .sql_editor
                                .update(cx, |editor, cx| editor.toggle_comment(window, cx));
                        }
                    }))
                    .child(Editor::new(&editor).h_full()),
            )
            .when_some(logic::statements_note(&fields.extra), |s, text| {
                s.child(
                    div()
                        .id("job-statements")
                        .role(Role::Alert)
                        .child(warning(text)),
                )
            })
            .child(note(words::one_statement_note()))
            .child(note(tr!(
                "Ctrl Enter and the other run shortcuts of a query tab never run a job: Run does, from the toolbar or the Inputs."
            )))
    }

    fn set_connection(
        &mut self,
        connection: ConnectionRef,
        context: SessionContext,
        cx: &mut Context<Self>,
    ) {
        self.store_fields(cx);
        if let JobBody::Sql(body) = &mut self.definition.body {
            body.connection = connection.id;
            body.context = context;
        }
        self.apply_language(cx);
        self.changed(cx);
    }

    // -----------------------------------------------------------------------
    // Parameters, a program's or a chain's inputs.

    fn specs_section(&self, typed_names: bool, cx: &mut Context<Self>) -> Div {
        let title = match self.kind {
            JobKind::Sql => tr!("Parameters from the :name markers"),
            JobKind::Process => tr!("Inputs (jjp/1)"),
            JobKind::Chain => tr!("Chain inputs"),
        };
        let specs = self.definition.inputs().to_vec();
        section(title)
            .when(specs.is_empty(), |s| {
                s.child(note(match self.kind {
                    JobKind::Sql => tr!(
                        "Write :name in the statement for each value the run form asks for. The values are bound by the driver, never written into the SQL."
                    ),
                    JobKind::Process => tr!(
                        "The values reach the program in jjp/1's hello, never in its arguments or environment."
                    ),
                    JobKind::Chain => tr!(
                        "The chain's form asks for these; a step's input can take one as its source."
                    ),
                }))
            })
            .children(
                specs
                    .iter()
                    .zip(&self.specs)
                    .enumerate()
                    .map(|(index, (spec, row))| self.spec_row(index, spec, row, typed_names, cx)),
            )
            .when(typed_names, |s| {
                s.child(
                    button("spec-add", I::Plus, tr!("Add an input")).on_click(cx.listener(
                        |this, _, window, cx| {
                            this.store_fields(cx);
                            let count = this.specs_mut().len();
                            let mut name = format!("input_{}", count + 1);
                            let mut n = count + 1;
                            while this.specs_mut().iter().any(|spec| spec.name == name) {
                                n += 1;
                                name = format!("input_{n}");
                            }
                            this.specs_mut().push(ParameterSpec::new(name));
                            this.rebuild(window, cx);
                            this.changed(cx);
                            this.refresh_plan(cx);
                        },
                    )),
                )
            })
    }

    fn spec_row(
        &self,
        index: usize,
        spec: &ParameterSpec,
        row_fields: &SpecRow,
        typed_names: bool,
        cx: &mut Context<Self>,
    ) -> Div {
        let view = cx.entity().downgrade();
        let current_kind = spec.kind;
        let invalid = params::invalid_choices(spec);
        column()
            .gap_1()
            .child(
                row()
                    .flex_wrap()
                    .gap_2()
                    .child(if typed_names {
                        div()
                            .w(px(140.))
                            .child(
                                Input::new(&row_fields.name)
                                    .small()
                                    .aria_label(tr!("Input name")),
                            )
                            .into_any_element()
                    } else {
                        div()
                            .w(px(140.))
                            .font_family(fonts().code.clone())
                            .text_size(px(13.))
                            .text_color(rgb(colors().sql_parameter))
                            .truncate()
                            .child(format!(":{}", spec.name))
                            .into_any_element()
                    })
                    .child(
                        div().w(px(160.)).child(
                            Input::new(&row_fields.label)
                                .small()
                                .aria_label(tr!("Label of {0}", spec.name)),
                        ),
                    )
                    .child(
                        Button::new(("spec-kind", index))
                            .ghost()
                            .small()
                            .label(parameter_kind(spec.kind))
                            .dropdown_caret(true)
                            .accessibility_label(tr!("Type of {0}", spec.name))
                            .dropdown_menu(move |mut menu, _, _| {
                                for kind in KINDS {
                                    let view = view.clone();
                                    menu = menu.item(
                                        PopupMenuItem::new(parameter_kind(kind))
                                            .checked(kind == current_kind)
                                            .on_click(move |_, window, cx| {
                                                let _ = view.update(cx, |this, cx| {
                                                    this.store_fields(cx);
                                                    if let Some(spec) =
                                                        this.specs_mut().get_mut(index)
                                                    {
                                                        spec.kind = kind;
                                                    }
                                                    this.rebuild(window, cx);
                                                    this.changed(cx);
                                                    this.refresh_plan(cx);
                                                });
                                            }),
                                    );
                                }
                                menu
                            }),
                    )
                    .child(
                        div().w(px(140.)).child(
                            Input::new(&row_fields.default)
                                .small()
                                .aria_label(tr!("Default value of {0}", spec.name)),
                        ),
                    )
                    .child(
                        check(("spec-required", index), spec.required, tr!("Required")).on_click(
                            cx.listener(move |this, _, window, cx| {
                                this.store_fields(cx);
                                if let Some(spec) = this.specs_mut().get_mut(index) {
                                    spec.required = !spec.required;
                                }
                                this.rebuild(window, cx);
                                this.changed(cx);
                            }),
                        ),
                    )
                    .child(
                        button(("spec-choices", index), I::List, tr!("List of choices"))
                            .aria_selected(row_fields.choices_open || !spec.choices.is_empty())
                            .when(!spec.choices.is_empty(), |s| {
                                s.child(badge(spec.choices.len().to_string(), colors().mint))
                            })
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if let Some(row) = this.specs.get_mut(index) {
                                    row.choices_open = !row.choices_open;
                                }
                                cx.notify();
                            })),
                    )
                    .when(typed_names, |s| {
                        s.child(
                            icon_button(("spec-remove", index), I::Trash, tr!("Remove the input"))
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.store_fields(cx);
                                    if index < this.specs_mut().len() {
                                        this.specs_mut().remove(index);
                                    }
                                    this.rebuild(window, cx);
                                    this.changed(cx);
                                    this.refresh_plan(cx);
                                })),
                        )
                    }),
            )
            .when(row_fields.choices_open, |s| {
                s.child(
                    row()
                        .id(("spec-choice-list", index))
                        .pl(px(148.))
                        .flex_wrap()
                        .gap_1()
                        .children(spec.choices.iter().enumerate().map(|(position, choice)| {
                            let bad = invalid.contains(&position);
                            row()
                                .id(("spec-choice", position))
                                .gap_1()
                                .pl_2()
                                .rounded(px(4.))
                                .border_1()
                                .border_color(rgb(if bad { colors().red } else { colors().border }))
                                .text_size(px(12.))
                                .child(choice.clone())
                                .child(
                                    icon_button(
                                        "spec-choice-remove",
                                        I::X,
                                        tr!("Remove the choice"),
                                    )
                                    .on_click(cx.listener(
                                        move |this, _, window, cx| {
                                            this.store_fields(cx);
                                            if let Some(spec) = this.specs_mut().get_mut(index)
                                                && position < spec.choices.len()
                                            {
                                                spec.choices.remove(position);
                                            }
                                            this.rebuild(window, cx);
                                            this.changed(cx);
                                        },
                                    )),
                                )
                        }))
                        .child(
                            div().w(px(160.)).child(
                                Input::new(&row_fields.choice)
                                    .small()
                                    .aria_label(tr!("A new choice of {0}", spec.name)),
                            ),
                        )
                        .child({
                            let name = spec.name.clone();
                            button(("spec-choice-add", index), I::Plus, tr!("Add")).on_click(
                                cx.listener(move |this, _, window, cx| {
                                    this.add_choice(&name, window, cx)
                                }),
                            )
                        })
                        .when(!invalid.is_empty(), |s| {
                            s.child(
                                div()
                                    .text_size(px(12.))
                                    .text_color(rgb(colors().red))
                                    .child(invalid_hint(spec.kind)),
                            )
                        }),
                )
            })
    }

    // -----------------------------------------------------------------------
    // Limits.

    fn limits_section(&self, cx: &mut Context<Self>) -> Div {
        let current = whole(&self.timeout.read(cx).value()).map_or(0, |seconds| seconds * 1000);
        let (default, note_text) = match self.kind {
            JobKind::Chain => (
                DEFAULT_CHAIN_TIMEOUT_MS,
                words::chain_timeout_note(if current == 0 {
                    DEFAULT_CHAIN_TIMEOUT_MS
                } else {
                    current
                }),
            ),
            _ => (
                DEFAULT_TIMEOUT_MS,
                words::timeout_note(
                    if current == 0 {
                        DEFAULT_TIMEOUT_MS
                    } else {
                        current
                    },
                    DEFAULT_TIMEOUT_MS,
                ),
            ),
        };
        let _ = default;
        section(tr!("Limits"))
            .child(
                row()
                    .gap_3()
                    .child(field(
                        tr!("Timeout, in seconds"),
                        div().w(px(140.)).child(
                            Input::new(&self.timeout)
                                .small()
                                .aria_label(tr!("Timeout, in seconds")),
                        ),
                    ))
                    .when_some(self.sql.as_ref(), |s, fields| {
                        s.child(field(
                            tr!("Row limit"),
                            div().w(px(140.)).child(
                                Input::new(&fields.max_rows)
                                    .small()
                                    .aria_label(tr!("Rows the run keeps")),
                            ),
                        ))
                    }),
            )
            .child(note(note_text))
            .when(self.kind == JobKind::Sql, |s| {
                s.child(note(tr!(
                    "The run keeps at most this many rows (and the memory ceiling); a query stops there, a write's other rows are read and discarded to confirm it finished."
                )))
            })
            .when(self.kind != JobKind::Chain, |s| s.child(note(words::limits_note())))
    }

    // -----------------------------------------------------------------------
    // Outputs.

    fn outputs_section(&self, cx: &mut Context<Self>) -> Div {
        let sql = self.kind == JobKind::Sql;
        let outputs: Vec<OutputSpec> = self.definition.outputs();
        let (row_rule, persist) = match &self.definition.body {
            JobBody::Sql(body) => (Some(body.row), body.persist_outputs),
            JobBody::Process(body) => (None, body.persist_outputs),
            JobBody::Chain(_) => (None, false),
        };
        section(tr!("Declared outputs"))
            .child(note(if sql {
                tr!(
                    "What a later chain step may read: each output names a result column by its position (from 1), since names may repeat."
                )
            } else {
                tr!(
                    "What a later chain step may read: the outputs of the program's complete message, each declared one present and nothing else."
                )
            }))
            .children(
                outputs
                    .iter()
                    .zip(&self.outputs)
                    .enumerate()
                    .map(|(index, (output, fields))| self.output_row(index, output, fields, sql, cx)),
            )
            .when(outputs.len() < MAX_OUTPUTS, |s| {
                s.child(
                    button("output-add", I::Plus, tr!("Add an output")).on_click(cx.listener(
                        |this, _, window, cx| {
                            this.store_fields(cx);
                            let count = this.definition.outputs().len();
                            let (sql, program) = new_output(this.kind, count);
                            match &mut this.definition.body {
                                JobBody::Sql(body) => body.outputs.extend(sql),
                                JobBody::Process(body) => body.outputs.extend(program),
                                JobBody::Chain(_) => {}
                            }
                            this.rebuild(window, cx);
                            this.changed(cx);
                        },
                    )),
                )
            })
            .when_some(row_rule, |s, rule| s.child(self.row_rule(rule, cx)))
            .child(
                check("job-store-outputs", persist, tr!("Store outputs"))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        if persist {
                            this.set_store_outputs(false, window, cx);
                            return;
                        }
                        // On a production connection, storing values asks
                        // first (ADR-0035).
                        let production = match &this.definition.body {
                            JobBody::Sql(body) => body
                                .connection
                                .and_then(|id| source_of(&this.sources.sources, id))
                                .filter(|source| {
                                    source.environment
                                        == Some(jykre::connection_appearance::Environment::Production)
                                })
                                .map(|source| source.connection.name.clone()),
                            _ => None,
                        };
                        match production {
                            Some(name) => cx.emit(JobEvent::Confirm {
                                title: tr!("Store the outputs of a production job?").to_owned(),
                                description: format!(
                                    "{} {}",
                                    words::production_outputs(&name),
                                    words::outputs_note()
                                ),
                                ok: tr!("Store outputs"),
                                what: Ask::StoreOutputs,
                            }),
                            None => this.set_store_outputs(true, window, cx),
                        }
                    })),
            )
            .child(note(words::outputs_note()))
    }

    pub(super) fn set_store_outputs(
        &mut self,
        on: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.store_fields(cx);
        match &mut self.definition.body {
            JobBody::Sql(body) => body.persist_outputs = on,
            JobBody::Process(body) => body.persist_outputs = on,
            JobBody::Chain(_) => {}
        }
        self.rebuild(window, cx);
        self.changed(cx);
    }

    fn output_row(
        &self,
        index: usize,
        output: &OutputSpec,
        fields: &OutputRow,
        sql: bool,
        cx: &mut Context<Self>,
    ) -> Div {
        let view = cx.entity().downgrade();
        let current = output.kind;
        row()
            .gap_2()
            .child(
                div().w(px(180.)).child(
                    Input::new(&fields.name)
                        .small()
                        .aria_label(tr!("Output name")),
                ),
            )
            .child(
                Button::new(("output-kind", index))
                    .ghost()
                    .small()
                    .label(parameter_kind(output.kind))
                    .dropdown_caret(true)
                    .accessibility_label(tr!("Type of the output {0}", output.name))
                    .dropdown_menu(move |mut menu, _, _| {
                        for kind in KINDS {
                            let view = view.clone();
                            menu = menu.item(
                                PopupMenuItem::new(parameter_kind(kind))
                                    .checked(kind == current)
                                    .on_click(move |_, window, cx| {
                                        let _ = view.update(cx, |this, cx| {
                                            this.store_fields(cx);
                                            match &mut this.definition.body {
                                                JobBody::Sql(body) => {
                                                    if let Some(output) =
                                                        body.outputs.get_mut(index)
                                                    {
                                                        output.kind = kind;
                                                    }
                                                }
                                                JobBody::Process(body) => {
                                                    if let Some(output) =
                                                        body.outputs.get_mut(index)
                                                    {
                                                        output.kind = kind;
                                                    }
                                                }
                                                JobBody::Chain(_) => {}
                                            }
                                            this.rebuild(window, cx);
                                            this.changed(cx);
                                        });
                                    }),
                            );
                        }
                        menu
                    }),
            )
            .when(sql, |s| {
                s.child(label(tr!("column"))).child(
                    div().w(px(80.)).child(
                        Input::new(&fields.column)
                            .small()
                            .aria_label(tr!("Result column of the output, from 1")),
                    ),
                )
            })
            .child(
                icon_button(("output-remove", index), I::Trash, tr!("Remove the output")).on_click(
                    cx.listener(move |this, _, window, cx| {
                        this.store_fields(cx);
                        match &mut this.definition.body {
                            JobBody::Sql(body) if index < body.outputs.len() => {
                                body.outputs.remove(index);
                            }
                            JobBody::Process(body) if index < body.outputs.len() => {
                                body.outputs.remove(index);
                            }
                            _ => {}
                        }
                        this.rebuild(window, cx);
                        this.changed(cx);
                    }),
                ),
            )
    }

    /// The SQL Job's row rule, with what each rule does with zero, one and
    /// several rows.
    fn row_rule(&self, rule: RowRule, cx: &mut Context<Self>) -> Div {
        let view = cx.entity().downgrade();
        column()
            .gap_1()
            .child(field(
                tr!("Which row the outputs take"),
                Button::new("job-row-rule")
                    .ghost()
                    .small()
                    .label(words::row_rule_name(rule))
                    .dropdown_caret(true)
                    .accessibility_label(tr!("Which row the outputs take"))
                    .dropdown_menu(move |mut menu, _, _| {
                        for candidate in RowRule::ALL {
                            let view = view.clone();
                            menu = menu.item(
                                PopupMenuItem::new(words::row_rule_name(candidate))
                                    .checked(candidate == rule)
                                    .on_click(move |_, _, cx| {
                                        let _ = view.update(cx, |this, cx| {
                                            this.store_fields(cx);
                                            if let JobBody::Sql(body) = &mut this.definition.body {
                                                body.row = candidate;
                                            }
                                            this.changed(cx);
                                        });
                                    }),
                            );
                        }
                        menu
                    }),
            ))
            .children(RowRule::ALL.into_iter().map(|candidate| {
                let chosen = candidate == rule;
                let cases = words::row_rule_cases(candidate);
                row()
                    .items_start()
                    .gap_2()
                    .p_1()
                    .rounded(px(4.))
                    .when(chosen, |s| s.bg(rgb(colors().accent_bg)))
                    .text_size(px(12.))
                    .child(
                        div()
                            .w(px(130.))
                            .font_weight(if chosen {
                                FontWeight::SEMIBOLD
                            } else {
                                FontWeight::NORMAL
                            })
                            .child(words::row_rule_name(candidate)),
                    )
                    .children(
                        cases
                            .into_iter()
                            .map(|case| div().flex_1().text_color(rgb(colors().muted)).child(case)),
                    )
            }))
    }

    // -----------------------------------------------------------------------
    // Versions.

    fn versions_section(&self, cx: &mut Context<Self>) -> Div {
        let count = self.versions.len();
        section(tr!("Published versions"))
            .child(note(words::versions_note()))
            .when(count == 0, |s| {
                s.child(label(tr!(
                    "Not published yet. Save the job, then Publish it to use it in a chain."
                )))
            })
            .when(count > 0, |s| {
                let rows = count.min(6) as f32;
                s.child(
                    div()
                        .id("job-versions")
                        .role(Role::List)
                        .aria_label(tr!("Published versions"))
                        .relative()
                        .h(px(rows * VERSION_ROW))
                        .child(
                            uniform_list(
                                "job-version-rows",
                                count,
                                cx.processor(|this, range: std::ops::Range<usize>, _, cx| {
                                    range
                                        .filter_map(|index| this.version_row(index, cx))
                                        .collect::<Vec<_>>()
                                }),
                            )
                            .size_full(),
                        ),
                )
            })
    }

    fn version_row(&self, index: usize, cx: &mut Context<Self>) -> Option<Stateful<Div>> {
        let version = self.versions.get(index)?;
        let number = version.version;
        let pinned = JobDefinition::from_json(&version.definition_json)
            .ok()
            .and_then(|definition| match definition.body {
                JobBody::Process(process) => process.pinned,
                _ => None,
            });
        Some(
            row()
                .id(("job-version", index))
                .role(Role::ListItem)
                .h(px(VERSION_ROW))
                .gap_2()
                .child(badge(tr!("Version {0}", number), colors().mint))
                .child(
                    column()
                        .flex_1()
                        .min_w_0()
                        .child(label(words::utc(version.published_at)))
                        .when_some(pinned, |s, pinned| {
                            s.child(
                                div()
                                    .truncate()
                                    .text_size(px(11.))
                                    .font_family(fonts().mono.clone())
                                    .text_color(rgb(colors().faint))
                                    .child(words::pinned(&pinned.path, &pinned.sha256)),
                            )
                        }),
                )
                .child(
                    icon_button(("version-delete", index), I::Trash, tr!("Delete the version"))
                        .on_click(cx.listener(move |_, _, _, cx| {
                            cx.emit(JobEvent::Confirm {
                                title: tr!("Delete version {0}?", number),
                                description: tr!(
                                    "A chain step that names it could no longer run, so deleting it is refused while a chain names it, naming the chains."
                                )
                                .to_owned(),
                                ok: tr!("Delete"),
                                what: Ask::DeleteVersion(number),
                            });
                        })),
                ),
        )
    }

    // -----------------------------------------------------------------------
    // A program.

    fn program_section(&self, cx: &mut Context<Self>) -> Div {
        let Some(fields) = &self.program else {
            return div();
        };
        section(tr!("Program"))
            .child(field(
                tr!("Program"),
                Input::new(&fields.program)
                    .small()
                    .aria_label(tr!("The program to start")),
            ))
            .child(note(tr!(
                "Started without a shell: an absolute path, or a name looked up on PATH when the run starts."
            )))
            .child(label(tr!("Arguments, one per row, passed as they are")))
            .children(fields.arguments.iter().enumerate().map(|(index, argument)| {
                row()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .child(Input::new(argument).small().aria_label(tr!("Argument"))),
                    )
                    .child(
                        icon_button(("argument-remove", index), I::Trash, tr!("Remove the argument"))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.edit_program(window, cx, |body| {
                                    if index < body.arguments.len() {
                                        body.arguments.remove(index);
                                    }
                                })
                            })),
                    )
            }))
            .child(
                button("argument-add", I::Plus, tr!("Add an argument")).on_click(cx.listener(
                    |this, _, window, cx| {
                        this.edit_program(window, cx, |body| body.arguments.push(String::new()))
                    },
                )),
            )
            .child(field(
                tr!("Entrypoint"),
                Input::new(&fields.entrypoint)
                    .small()
                    .aria_label(tr!("The entrypoint file")),
            ))
            .child(note(words::imports_note()))
            .child(field(
                tr!("Working folder"),
                Input::new(&fields.working_directory)
                    .small()
                    .aria_label(tr!("The working folder")),
            ))
            .child(note(words::working_directory_note()))
            .child(warning(words::trusted_note()))
            .child(note(words::logs_note()))
            .child(note(words::stdin_note()))
    }

    fn edit_program(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        edit: impl FnOnce(&mut definition::ProcessBody),
    ) {
        self.store_fields(cx);
        if let JobBody::Process(body) = &mut self.definition.body {
            edit(body);
        }
        self.rebuild(window, cx);
        self.changed(cx);
    }

    fn environment_section(&self, cx: &mut Context<Self>) -> Div {
        let Some(fields) = &self.program else {
            return div();
        };
        section(tr!("Environment"))
            .child(warning(words::environment_note(
                super::super::program_bits::denylist(),
            )))
            .child(label(tr!("Also removed")))
            .children(fields.unset.iter().enumerate().map(|(index, name)| {
                row()
                    .gap_2()
                    .child(
                        div().w(px(240.)).child(
                            Input::new(name)
                                .small()
                                .aria_label(tr!("Variable to remove")),
                        ),
                    )
                    .child(
                        icon_button(("unset-remove", index), I::Trash, tr!("Keep the variable"))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.edit_program(window, cx, |body| {
                                    if index < body.environment.unset.len() {
                                        body.environment.unset.remove(index);
                                    }
                                })
                            })),
                    )
            }))
            .child(
                button("unset-add", I::Plus, tr!("Remove a variable")).on_click(cx.listener(
                    |this, _, window, cx| {
                        this.edit_program(window, cx, |body| {
                            body.environment.unset.push(String::new())
                        })
                    },
                )),
            )
            .child(label(tr!(
                "Set (constants stored in the job and shown here; never secrets)"
            )))
            .children(fields.set.iter().enumerate().map(|(index, (name, value))| {
                row()
                    .gap_2()
                    .child(
                        div()
                            .w(px(200.))
                            .child(Input::new(name).small().aria_label(tr!("Variable name"))),
                    )
                    .child(
                        div()
                            .flex_1()
                            .child(Input::new(value).small().aria_label(tr!("Variable value"))),
                    )
                    .child(
                        icon_button(("set-remove", index), I::Trash, tr!("Remove the variable"))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.edit_program(window, cx, |body| {
                                    if index < body.environment.set.len() {
                                        body.environment.set.remove(index);
                                    }
                                })
                            })),
                    )
            }))
            .child(
                button("set-add", I::Plus, tr!("Set a variable")).on_click(cx.listener(
                    |this, _, window, cx| {
                        this.edit_program(window, cx, |body| {
                            body.environment.set.push((String::new(), String::new()))
                        })
                    },
                )),
            )
    }

    fn protocol_section(&self, cx: &mut Context<Self>) -> Div {
        let JobBody::Process(body) = &self.definition.body else {
            return div();
        };
        let current = body.protocol;
        let radio = |id: &'static str, protocol: Protocol, title: &'static str| {
            let on = current == protocol;
            button(id, if on { I::CircleCheck } else { I::Circle }, title)
                .role(Role::RadioButton)
                .aria_selected(on)
                .when(on, |s| s.text_color(rgb(colors().mint)))
        };
        section(tr!("Protocol"))
            .child(
                row()
                    .id("job-protocol")
                    .role(Role::RadioGroup)
                    .aria_label(tr!("Protocol"))
                    .gap_2()
                    .child(
                        radio("protocol-none", Protocol::None, tr!("No protocol")).on_click(
                            cx.listener(|this, _, window, cx| {
                                this.edit_program(window, cx, |body| {
                                    body.protocol = Protocol::None;
                                    body.inputs.clear();
                                    body.outputs.clear();
                                    body.persist_outputs = false;
                                })
                            }),
                        ),
                    )
                    .child(
                        radio("protocol-jjp", Protocol::Jjp1, "jjp/1").on_click(cx.listener(
                            |this, _, window, cx| {
                                this.edit_program(window, cx, |body| body.protocol = Protocol::Jjp1)
                            },
                        )),
                    ),
            )
            .child(note(words::protocol_note()))
    }

    // -----------------------------------------------------------------------
    // A chain.

    fn steps_section(&self, cx: &mut Context<Self>) -> Div {
        let JobBody::Chain(body) = &self.definition.body else {
            return div();
        };
        let Some(fields) = &self.chain else {
            return div();
        };
        let versions: Vec<Option<JobDefinition>> = body
            .steps
            .iter()
            .map(|step| version_of(&fields.versions, step.job, step.version))
            .collect();
        let general: Vec<String> = fields
            .plan
            .as_ref()
            .map(|plan| {
                plan.problems
                    .iter()
                    .filter(|problem| problem.step().is_empty())
                    .map(words::chain_problem)
                    .collect()
            })
            .unwrap_or_default();
        section(tr!("Steps"))
            .child(note(tr!(
                "2 to 16 steps, run in order. Each runs a published version of a SQL Job or a program, never a chain: chains do not nest. A step whose run fails stops the chain, and the steps after it do not run."
            )))
            .child(note(words::versions_note()))
            .children(general.into_iter().map(warning))
            .children(
                body.steps
                    .iter()
                    .enumerate()
                    .map(|(index, step)| self.step_card(index, step, body, &versions, cx)),
            )
            .when(body.steps.len() < MAX_STEPS, |s| {
                s.child(
                    button("step-add", I::Plus, tr!("Add a step")).on_click(cx.listener(
                        |this, _, window, cx| {
                            this.edit_chain(window, cx, |body| {
                                let step = new_step(body);
                                body.steps.push(step);
                            })
                        },
                    )),
                )
            })
            .when(body.steps.len() < MIN_STEPS, |s| {
                s.child(warning(words::problem(
                    &jykre::automation::definition::Problem::StepCount(body.steps.len()),
                )))
            })
    }

    fn step_card(
        &self,
        index: usize,
        step: &ChainStep,
        body: &definition::ChainBody,
        versions: &[Option<JobDefinition>],
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let Some(fields) = self.chain.as_ref() else {
            return div().id(("step", index));
        };
        let Some(step_fields) = fields.steps.get(index) else {
            return div().id(("step", index));
        };
        let job = self.jobs.iter().find(|job| job.id == step.job);
        let version = versions.get(index).cloned().flatten();
        let plan_step = fields
            .plan
            .as_ref()
            .and_then(|plan| plan.steps.iter().find(|planned| planned.id == step.id));
        let problems: Vec<String> = fields
            .plan
            .as_ref()
            .map(|plan| {
                plan.problems
                    .iter()
                    .filter(|problem| problem.step() == step.id)
                    .map(words::chain_problem)
                    .collect()
            })
            .unwrap_or_default();
        let published: Vec<i64> = fields
            .versions
            .get(&step.job)
            .map(|versions| versions.iter().map(|stored| stored.version).collect())
            .unwrap_or_default();
        let unpublished = published.is_empty();
        let view = cx.entity().downgrade();
        let jobs = self.jobs.clone();
        let job_label = job.map_or_else(|| tr!("Choose a job").to_owned(), |job| job.name.clone());
        let version_label = if step.version >= 1 {
            tr!("Version {0}", step.version)
        } else {
            tr!("Choose a version").to_owned()
        };
        let count = body.steps.len();
        let current_job = step.job;
        let current_version = step.version;
        column()
            .id(("step", index))
            .role(Role::Group)
            .aria_label(tr!("Step {0}", index + 1))
            .gap_2()
            .p_2()
            .rounded(px(6.))
            .bg(rgb(colors().panel))
            .border_1()
            .border_color(rgb(if problems.is_empty() {
                colors().border
            } else {
                colors().amber
            }))
            .child(
                row()
                    .flex_wrap()
                    .gap_2()
                    .child(badge(tr!("Step {0}", index + 1), colors().mint))
                    .child(
                        div().w(px(130.)).child(
                            Input::new(&step_fields.id)
                                .small()
                                .aria_label(tr!("Step ID")),
                        ),
                    )
                    .child({
                        let view = view.clone();
                        Button::new(("step-job", index))
                            .ghost()
                            .small()
                            .label(job_label)
                            .dropdown_caret(true)
                            .accessibility_label(tr!("The step's job"))
                            .dropdown_menu(move |mut menu, _, _| {
                                menu = menu
                                    .label(tr!("SQL Jobs and programs; a chain cannot be a step"))
                                    .scrollable(true)
                                    .max_h(px(360.));
                                for job in jobs.iter().filter(|job| job.kind != JobKind::Chain) {
                                    let view = view.clone();
                                    let id = job.id;
                                    menu = menu.item(
                                        PopupMenuItem::new(format!(
                                            "{} · {}",
                                            job.name,
                                            words::kind_name(job.kind)
                                        ))
                                        .icon(super::super::kind_icon(job.kind))
                                        .checked(id == current_job)
                                        .on_click(move |_, window, cx| {
                                            let _ = view.update(cx, |this, cx| {
                                                let latest = this
                                                    .chain
                                                    .as_ref()
                                                    .and_then(|chain| chain.versions.get(&id))
                                                    .and_then(|versions| {
                                                        versions.iter().map(|v| v.version).max()
                                                    })
                                                    .unwrap_or(0);
                                                this.edit_chain(window, cx, |body| {
                                                    if let Some(step) = body.steps.get_mut(index) {
                                                        step.job = id;
                                                        step.version = latest;
                                                        step.inputs.clear();
                                                    }
                                                });
                                            });
                                        }),
                                    );
                                }
                                menu
                            })
                    })
                    .child({
                        let view = view.clone();
                        Button::new(("step-version", index))
                            .ghost()
                            .small()
                            .label(version_label)
                            .dropdown_caret(true)
                            .accessibility_label(tr!("The step's published version"))
                            .dropdown_menu(move |mut menu, _, _| {
                                if published.is_empty() {
                                    return menu.label(tr!(
                                        "No published version: publish the job first."
                                    ));
                                }
                                let mut numbers = published.clone();
                                numbers.sort_unstable_by(|a, b| b.cmp(a));
                                for number in numbers {
                                    let view = view.clone();
                                    menu = menu.item(
                                        PopupMenuItem::new(tr!("Version {0}", number))
                                            .checked(number == current_version)
                                            .on_click(move |_, window, cx| {
                                                let _ = view.update(cx, |this, cx| {
                                                    this.edit_chain(window, cx, |body| {
                                                        if let Some(step) = body.steps.get_mut(index)
                                                        {
                                                            step.version = number;
                                                        }
                                                    });
                                                });
                                            }),
                                    );
                                }
                                menu
                            })
                    })
                    .when(plan_step.is_some_and(|planned| planned.changed_since), |s| {
                        s.child(badge(tr!("The job changed since this version"), colors().amber))
                    })
                    .child(div().flex_1())
                    .when(index > 0, |s| {
                        s.child(
                            icon_button(("step-up", index), I::ArrowUp, tr!("Move the step up"))
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.edit_chain(window, cx, |body| body.steps.swap(index - 1, index))
                                })),
                        )
                    })
                    .when(index + 1 < count, |s| {
                        s.child(
                            icon_button(("step-down", index), I::ArrowDown, tr!("Move the step down"))
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.edit_chain(window, cx, |body| body.steps.swap(index, index + 1))
                                })),
                        )
                    })
                    .child(
                        icon_button(("step-remove", index), I::Trash, tr!("Remove the step"))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.edit_chain(window, cx, |body| {
                                    if index < body.steps.len() {
                                        body.steps.remove(index);
                                    }
                                })
                            })),
                    ),
            )
            .when(job.is_some() && unpublished, |s| {
                s.child(warning(tr!(
                    "This job has no published version: publish it in its own tab, since a chain runs published versions only."
                )))
            })
            .when_some(version.as_ref().filter(|definition| logic::unreadable_step(definition)), |s, _| {
                s.child(note(tr!(
                    "This program uses no protocol: it takes no inputs, and later steps cannot read it."
                )))
            })
            .children(problems.into_iter().map(warning))
            .when_some(version, |s, definition| {
                let choices = logic::source_choices(body, index, versions);
                s.children(definition.inputs().iter().enumerate().map(|(position, spec)| {
                    self.step_input((index, position), spec, (step, step_fields), &choices, cx)
                }))
            })
    }

    /// One input of a step's version and where it comes from; `at` is the
    /// step's index and the input's position.
    fn step_input(
        &self,
        at: (usize, usize),
        spec: &ParameterSpec,
        (step, step_fields): (&ChainStep, &StepFields),
        choices: &[logic::SourceChoice],
        cx: &mut Context<Self>,
    ) -> Div {
        let (index, position) = at;
        let current = step.source(&spec.name).map(logic::SourceKind::of);
        let shown = choices
            .iter()
            .find(|choice| choice.source == current)
            .map_or_else(
                || tr!("Its default, or NULL").to_owned(),
                |choice| choice.label.clone(),
            );
        let view = cx.entity().downgrade();
        let choices = choices.to_vec();
        let name = spec.name.clone();
        let element = (index * 1000 + position) as u64;
        row()
            .pl_4()
            .gap_2()
            .child(
                div()
                    .w(px(180.))
                    .truncate()
                    .font_family(fonts().code.clone())
                    .text_size(px(13.))
                    .child(if spec.required {
                        format!("{} *", spec.name)
                    } else {
                        spec.name.clone()
                    }),
            )
            .child(label(parameter_kind(spec.kind)))
            .child(
                Button::new(("step-source", element))
                    .ghost()
                    .small()
                    .label(shown)
                    .dropdown_caret(true)
                    .accessibility_label(tr!("Where {0} comes from", spec.name))
                    .dropdown_menu(move |mut menu, _, _| {
                        for choice in &choices {
                            let view = view.clone();
                            let name = name.clone();
                            let picked = choice.source.clone();
                            menu = menu.item(
                                PopupMenuItem::new(choice.label.clone())
                                    .checked(choice.source == current)
                                    .on_click(move |_, window, cx| {
                                        let picked = picked.clone();
                                        let name = name.clone();
                                        let _ = view.update(cx, |this, cx| {
                                            this.edit_chain(window, cx, |body| {
                                                let Some(step) = body.steps.get_mut(index) else {
                                                    return;
                                                };
                                                step.inputs.retain(|(input, _)| *input != name);
                                                let source = match picked {
                                                    None => return,
                                                    Some(logic::SourceKind::Input(input)) => {
                                                        Source::Input(input)
                                                    }
                                                    Some(logic::SourceKind::Step {
                                                        step,
                                                        output,
                                                    }) => Source::Step { step, output },
                                                    Some(logic::SourceKind::Constant) => {
                                                        Source::Value(String::new())
                                                    }
                                                };
                                                step.inputs.push((name, source));
                                            });
                                        });
                                    }),
                            );
                        }
                        menu
                    }),
            )
            .when_some(step_fields.constants.get(&spec.name), |s, constant| {
                s.child(
                    div().w(px(200.)).child(
                        Input::new(constant)
                            .small()
                            .aria_label(tr!("The constant of {0}", spec.name)),
                    ),
                )
            })
    }
}
