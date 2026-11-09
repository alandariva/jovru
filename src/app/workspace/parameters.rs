//! The `:name` parameters of a query tab: one field per marker of its SQL,
//! above the editor, whose values the driver binds apart from the SQL
//! ([ADR-0026](../../../docs/adr/0026-named-parameters-in-sql-tabs.md)).
//!
//! Which names the SQL holds, how a value is checked and the requests that
//! carry them live in `jykre::parameters`; this view keeps the fields and
//! shows their problems.
use super::super::report_view::{invalid_hint, kind_name, placeholder};
use super::*;
use jykre::parameters::{self, KINDS, KindChoice, ParameterError, ParameterProblem, TabParameter};
use std::collections::HashMap;

/// Fields kept for names that left the SQL, so they come back with their
/// value (typing `:id` passes through `:i`).
const MEMORY: usize = 50;

/// The input of one field.
pub(super) struct ParameterInput {
    name: String,
    input: Entity<InputState>,
    _subscription: Subscription,
}

/// The parameter fields of a tab.
#[derive(Default)]
pub(super) struct ParameterForm {
    /// One per name of the SQL, in the order they first appear: the source
    /// of truth for what runs and what is kept with the tab.
    pub(super) fields: Vec<TabParameter>,
    /// Fields of names no longer in the SQL, newest first.
    memory: Vec<TabParameter>,
    /// The inputs of `fields`, built when the window is at hand.
    inputs: Vec<ParameterInput>,
    /// The names of the SQL as its outline last read them.
    names: Vec<String>,
    errors: HashMap<String, ParameterProblem>,
}

impl ParameterForm {
    fn field_mut(&mut self, name: &str) -> Option<&mut TabParameter> {
        self.fields.iter_mut().find(|field| field.name == name)
    }
}

impl Workspace {
    /// Takes the names of the SQL, read with its outline after it or its
    /// dialect changed ([`jykre::script_outline::Outline`]); the fields
    /// change only when the names did.
    pub(super) fn apply_parameter_names(&mut self, names: Vec<String>) {
        self.parameters.names = names;
        self.sync_parameters();
    }

    /// Matches the fields to the names last read.
    fn sync_parameters(&mut self) {
        let form = &mut self.parameters;
        let names = &form.names;
        if form.fields.len() == names.len()
            && form
                .fields
                .iter()
                .zip(names)
                .all(|(field, name)| &field.name == name)
        {
            return;
        }
        let mut known = std::mem::take(&mut form.fields);
        known.extend(std::mem::take(&mut form.memory));
        form.fields = parameters::reconcile_fields(&known, names);
        form.memory = known
            .into_iter()
            .filter(|field| !names.contains(&field.name))
            .take(MEMORY)
            .collect();
        form.errors.retain(|name, _| names.contains(name));
    }

    /// Restores the fields kept with the tab.
    pub(super) fn restore_parameters(&mut self, kept: &[TabParameter]) {
        self.parameters.memory = kept.to_vec();
        self.parameters.fields.clear();
        self.parameters.inputs.clear();
        self.sync_parameters();
    }

    /// Builds the inputs of fields that have none yet: they need the window,
    /// which the places that change the SQL do not have. Runs only when the
    /// names changed.
    pub(super) fn ensure_parameter_inputs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let form = &self.parameters;
        if form.inputs.len() == form.fields.len()
            && form
                .inputs
                .iter()
                .zip(&form.fields)
                .all(|(input, field)| input.name == field.name)
        {
            return;
        }
        let mut old = std::mem::take(&mut self.parameters.inputs);
        let mut inputs = Vec::with_capacity(self.parameters.fields.len());
        for field in &self.parameters.fields {
            if let Some(at) = old.iter().position(|input| input.name == field.name) {
                inputs.push(old.swap_remove(at));
                continue;
            }
            let text = field.text.clone();
            let hint = field_placeholder(field.kind);
            let input = cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder(hint)
                    .default_value(text)
            });
            let name = field.name.clone();
            let subscription = cx.subscribe_in(
                &input,
                window,
                move |this, input, event: &InputEvent, _, cx| match event {
                    InputEvent::Change => {
                        let text = input.read(cx).value().to_string();
                        if let Some(field) = this.parameters.field_mut(&name)
                            && field.text != text
                        {
                            field.text = text;
                            this.parameters.errors.remove(&name);
                            cx.emit(WorkspaceEvent::Changed);
                            cx.notify();
                        }
                    }
                    // Enter runs what Ctrl+Enter runs in the editor.
                    InputEvent::PressEnter { .. } => this.execute_current(cx),
                    _ => {}
                },
            );
            inputs.push(ParameterInput {
                name: field.name.clone(),
                input,
                _subscription: subscription,
            });
        }
        self.parameters.inputs = inputs;
    }

    /// The request for each statement, binding its markers from the fields;
    /// on a problem, the fields show it and nothing runs.
    pub(super) fn parameter_requests(
        &mut self,
        statements: &[String],
        options: &ExecutionOptions,
        cx: &mut Context<Self>,
    ) -> Option<Vec<QueryRequest>> {
        let dialect = self.run.dialect();
        let fields = &self.parameters.fields;
        let mut requests = Vec::with_capacity(statements.len());
        let mut errors: Vec<ParameterError> = Vec::new();
        for statement in statements {
            match parameters::tab_request(statement, dialect, fields, options) {
                Ok(request) => requests.push(request),
                Err(more) => errors.extend(more),
            }
        }
        if errors.is_empty() {
            if !self.parameters.errors.is_empty() {
                self.parameters.errors.clear();
                cx.notify();
            }
            return Some(requests);
        }
        self.parameters.errors = errors
            .into_iter()
            .map(|error| (error.name, error.problem))
            .collect();
        self.notice = Some(Feedback::new(
            Tone::Warning,
            tr!("Fill in the parameters marked above the editor."),
        ));
        cx.notify();
        None
    }

    fn set_parameter_kind(
        &mut self,
        name: &str,
        kind: KindChoice,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(field) = self.parameters.field_mut(name) else {
            return;
        };
        field.kind = kind;
        self.parameters.errors.remove(name);
        if let Some(input) = self
            .parameters
            .inputs
            .iter()
            .find(|input| input.name == name)
        {
            input.input.update(cx, |input, cx| {
                input.set_placeholder(field_placeholder(kind), window, cx)
            });
        }
        cx.emit(WorkspaceEvent::Changed);
        cx.notify();
    }

    fn toggle_parameter_null(&mut self, name: &str, cx: &mut Context<Self>) {
        let Some(field) = self.parameters.field_mut(name) else {
            return;
        };
        field.null = !field.null;
        self.parameters.errors.remove(name);
        cx.emit(WorkspaceEvent::Changed);
        cx.notify();
    }

    /// One field per `:name` of the SQL, above the editor; nothing when the
    /// SQL has none.
    pub(super) fn parameter_form(&self, cx: &mut Context<Self>) -> Option<Div> {
        let form = &self.parameters;
        if form.fields.is_empty() {
            return None;
        }
        let view = cx.entity();
        let fields = form
            .fields
            .iter()
            .zip(&form.inputs)
            .enumerate()
            .map(|(index, (field, input))| {
                let problem = form.errors.get(&field.name).copied();
                let shown = field.kind.resolve(&field.text);
                let kind_label: SharedString = match field.kind {
                    KindChoice::Auto => tr!("Auto · {0}", kind_name(shown)).into(),
                    KindChoice::Fixed(kind) => kind_name(kind).into(),
                };
                let name = field.name.clone();
                let choice = field.kind;
                let menu_view = view.clone();
                let null_view = view.clone();
                let null_name = field.name.clone();
                column()
                    .gap_1()
                    .child(
                        row()
                            .gap_1()
                            .child(
                                div()
                                    .text_size(px(12.))
                                    .text_color(rgb(colors().sql_parameter))
                                    .child(format!(":{}", field.name)),
                            )
                            .child(
                                Button::new(("parameter-kind", index))
                                    .ghost()
                                    .xsmall()
                                    .label(kind_label)
                                    .dropdown_caret(true)
                                    .accessibility_label(tr!("Type of {0}", field.name))
                                    .dropdown_menu(move |mut menu, _, _| {
                                        let choices = std::iter::once(KindChoice::Auto)
                                            .chain(KINDS.map(KindChoice::Fixed));
                                        for option in choices {
                                            let view = menu_view.clone();
                                            let name = name.clone();
                                            let title = match option {
                                                KindChoice::Auto => tr!("Automatic"),
                                                KindChoice::Fixed(kind) => kind_name(kind),
                                            };
                                            menu = menu.item(
                                                PopupMenuItem::new(title)
                                                    .checked(option == choice)
                                                    .on_click(move |_, window, cx| {
                                                        view.update(cx, |this, cx| {
                                                            this.set_parameter_kind(
                                                                &name, option, window, cx,
                                                            )
                                                        });
                                                    }),
                                            );
                                        }
                                        menu
                                    }),
                            )
                            .child(
                                button(
                                    ("parameter-null", index),
                                    if field.null {
                                        I::SquareCheck
                                    } else {
                                        I::Square
                                    },
                                    "NULL",
                                )
                                .h(px(24.))
                                .text_size(px(12.))
                                .role(Role::CheckBox)
                                .aria_label(tr!("{0} is NULL", field.name))
                                .aria_toggled(if field.null {
                                    Toggled::True
                                } else {
                                    Toggled::False
                                })
                                .when(field.null, |s| s.text_color(rgb(colors().mint)))
                                .on_click(move |_, _, cx| {
                                    null_view.update(cx, |this, cx| {
                                        this.toggle_parameter_null(&null_name, cx)
                                    });
                                }),
                            ),
                    )
                    .child(
                        div()
                            .w(px(200.))
                            .rounded(px(5.))
                            .border_1()
                            .border_color(rgb(if problem.is_some() {
                                colors().red
                            } else {
                                colors().border
                            }))
                            .child(
                                Input::new(&input.input)
                                    .small()
                                    .disabled(field.null)
                                    .aria_label(tr!("Value of {0}", field.name)),
                            ),
                    )
                    .when_some(problem, |s, problem| {
                        s.child(
                            div()
                                .id(("parameter-problem", index))
                                .role(Role::Alert)
                                .text_size(px(12.))
                                .text_color(rgb(colors().red))
                                .child(match problem {
                                    ParameterProblem::Missing => {
                                        tr!("Type a value, or choose NULL")
                                    }
                                    ParameterProblem::Invalid => invalid_hint(shown),
                                    // Only a report's list parameters have choices.
                                    ParameterProblem::NotAChoice => invalid_hint(shown),
                                }),
                        )
                    })
            })
            .collect::<Vec<_>>();
        Some(
            row()
                .flex_none()
                .flex_wrap()
                .items_start()
                .gap_3()
                .px_3()
                .py_2()
                .border_b_1()
                .border_color(rgb(colors().border))
                .child(
                    div()
                        .id("query-parameters")
                        .pt(px(2.))
                        .child(label(tr!("Parameters")))
                        .tooltip(|window, cx| {
                            Tooltip::new(tr!(
                                "Parameter values are sent apart from the SQL and bound by the database."
                            ))
                            .build(window, cx)
                        }),
                )
                .children(fields),
        )
    }
}

fn field_placeholder(kind: KindChoice) -> &'static str {
    match kind {
        KindChoice::Auto => tr!("Value"),
        KindChoice::Fixed(kind) => placeholder(kind),
    }
}
