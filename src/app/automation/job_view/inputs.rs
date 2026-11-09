//! The job tab's **Inputs** (the run form, the run's progress and the
//! latest SQL result) and **Runs** (this job's history).
use super::*;
use crate::app::report_view::invalid_hint;
use gpui_kit::assets::IconName as I;
use gpui_kit::component::Sizable;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::Input;
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use std::time::Instant;

use super::super::history::line_element as line;

impl JobView {
    pub(super) fn inputs_panel(&self, cx: &mut Context<Self>) -> Stateful<Div> {
        let definition = &self.definition;
        let specs = definition.inputs().to_vec();
        let running = self.running();
        column()
            .id("job-inputs")
            .role(Role::TabPanel)
            .aria_label(tr!("Inputs"))
            .flex_1()
            .min_h_0()
            .child(
                column()
                    .id("job-inputs-form")
                    .flex_none()
                    .max_h(px(360.))
                    .overflow_y_scroll()
                    .track_scroll(&self.inputs_scroll)
                    .border_b_1()
                    .border_color(rgb(colors().border))
                    .child(self.form_rows(&specs, running, cx)),
            )
            .child(self.run_status(cx))
            .when(logic::shows_result(self.has_result, running), |s| {
                s.child(
                    div()
                        .flex_1()
                        .min_h(px(160.))
                        .border_t_1()
                        .border_color(rgb(colors().border))
                        .child(
                            self.grid
                                .clone()
                                .cached(StyleRefinement::default().size_full()),
                        ),
                )
            })
    }

    /// The form's fields, **Run** or **Stop**, and the note that refused
    /// runs leave no history: at their own height inside the scrolled form.
    fn form_rows(&self, specs: &[ParameterSpec], running: bool, cx: &mut Context<Self>) -> Div {
        let definition = &self.definition;
        column()
            .flex_none()
            .p_3()
            .gap_2()
                    .when(!takes_inputs(definition), |s| {
                        s.child(label(tr!(
                            "A program without a protocol takes no inputs: it runs with its fixed arguments."
                        )))
                    })
                    .when(takes_inputs(definition) && specs.is_empty(), |s| {
                        s.child(label(tr!("This job asks for no values.")))
                    })
                    .when_some(self.prefill_note.clone(), |s, text| {
                        s.child(
                            div()
                                .id("job-prefill")
                                .role(Role::Status)
                                .child(feedback_line(&Feedback::new(Tone::Info, text)).text_size(px(13.))),
                        )
                    })
                    .child(
                        row()
                            .flex_wrap()
                            .items_end()
                            .gap_3()
                            .children(self.values.iter().enumerate().filter_map(|(index, field)| {
                                let spec = specs.iter().find(|spec| spec.name == field.name)?;
                                Some(self.value_field(index, spec, field, cx))
                            })),
                    )
                    .child(
                        row()
                            .gap_2()
                            .child(if running {
                                button("inputs-stop", I::Square, tr!("Stop"))
                                    .on_click(cx.listener(|this, _, _, cx| this.stop(cx)))
                            } else {
                                primary_button("inputs-run", I::Play, tr!("Run"))
                                    .on_click(cx.listener(|this, _, _, cx| this.run(cx)))
                            })
                            .when(self.unsaved, |s| s.child(label(words::draft_run())))
                            .when(self.pending.is_some() && !running, |s| {
                                s.child(label(tr!("Waiting for your answer…")))
                            }),
                    )
                    .child(note_line(words::refused_note()))
                    .when(self.kind == JobKind::Process, |s| s.child(note_line(words::stop_note())))
    }

    fn value_field(
        &self,
        index: usize,
        spec: &ParameterSpec,
        field: &ValueField,
        cx: &mut Context<Self>,
    ) -> Div {
        let problem = self.value_errors.get(&field.name);
        let control = if spec.choices.is_empty() {
            Input::new(&field.input)
                .small()
                .aria_label(spec.shown_label().to_owned())
                .into_any_element()
        } else {
            self.choice_menu(index, spec, field, cx).into_any_element()
        };
        column()
            .w(px(220.))
            .gap_1()
            .child(label(if spec.required {
                format!("{} *", spec.shown_label())
            } else {
                spec.shown_label().to_owned()
            }))
            .child(
                div()
                    .rounded(px(5.))
                    .border_1()
                    .border_color(rgb(if problem.is_some() {
                        colors().red
                    } else {
                        colors().border
                    }))
                    .child(control),
            )
            .when_some(problem, |s, problem| {
                s.child(
                    div()
                        .text_size(px(12.))
                        .text_color(rgb(colors().red))
                        .child(match problem {
                            ParameterProblem::Missing => tr!("Required"),
                            ParameterProblem::Invalid => invalid_hint(spec.kind),
                            ParameterProblem::NotAChoice => tr!("Choose one of the list"),
                        }),
                )
            })
    }

    /// A list input's value: one of its choices, or none when optional.
    fn choice_menu(
        &self,
        index: usize,
        spec: &ParameterSpec,
        field: &ValueField,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let current = field.input.read(cx).value().to_string();
        let shown = if current.is_empty() {
            tr!("Choose").to_owned()
        } else {
            current.clone()
        };
        let view = cx.entity().downgrade();
        let input = field.input.clone();
        let choices = spec.choices.clone();
        let required = spec.required;
        Button::new(("job-value", index))
            .ghost()
            .small()
            .w_full()
            .label(shown)
            .dropdown_caret(true)
            .accessibility_label(spec.shown_label().to_owned())
            .dropdown_menu(move |mut menu, _, _| {
                let pick = |value: String| {
                    let view = view.clone();
                    let input = input.clone();
                    move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                        input.update(cx, |input, cx| input.set_value(value.clone(), window, cx));
                        let _ = view.update(cx, |this, cx| {
                            this.value_errors.clear();
                            cx.notify();
                        });
                    }
                };
                if !required {
                    menu = menu.item(
                        PopupMenuItem::new(tr!("No value"))
                            .checked(current.is_empty())
                            .on_click(pick(String::new())),
                    );
                }
                for choice in &choices {
                    menu = menu.item(
                        PopupMenuItem::new(choice.clone())
                            .checked(*choice == current)
                            .on_click(pick(choice.clone())),
                    );
                }
                menu
            })
    }

    /// The run's state: queued with its place, limit and time left; its
    /// phase and rows; then how it ended, or why it did not run.
    fn run_status(&self, cx: &mut Context<Self>) -> Stateful<Div> {
        let mut lines: Vec<Line> = Vec::new();
        let mut working = false;
        let progress_line = |status: &RunStatus, kind: JobKind| -> Option<Line> {
            match status {
                RunStatus::Queued {
                    position,
                    leaves_at,
                    blocked,
                } => Some(Line {
                    level: Level::Warning,
                    text: words::queued(
                        *position,
                        leaves_at.saturating_duration_since(Instant::now()),
                        blocked,
                        &self.engine.holders(blocked),
                    ),
                }),
                RunStatus::Active(progress) => Some(Line {
                    level: Level::Info,
                    text: words::progress(progress, kind),
                }),
                _ => None,
            }
        };
        match &self.active {
            Active::Run { status, .. } => {
                if let Some(progress) = progress_line(status, self.kind) {
                    working = true;
                    lines.push(progress);
                }
            }
            Active::Chain {
                status,
                step_status,
                step_ids,
                ..
            } => {
                if let ChainStatus::Running { step, .. } = status {
                    working = true;
                    let id = step_ids.get(*step).cloned().unwrap_or_default();
                    lines.push(Line {
                        level: Level::Info,
                        text: tr!("Running step {0} of {1}: {2}", step + 1, step_ids.len(), id),
                    });
                    let kind = self
                        .chain
                        .as_ref()
                        .and_then(|chain| chain.plan.as_ref())
                        .and_then(|plan| plan.steps.get(*step))
                        .map_or(JobKind::Sql, |planned| planned.definition.kind());
                    if let Some(progress) =
                        step_status.as_ref().and_then(|s| progress_line(s, kind))
                    {
                        lines.push(progress);
                    }
                }
            }
            Active::Idle => {}
        }
        lines.extend(self.run_lines.iter().cloned());
        let alert = lines.iter().any(|line| line.level == Level::Error);
        column()
            .id("job-run-status")
            .role(if alert { Role::Alert } else { Role::Status })
            .aria_label(
                lines
                    .first()
                    .map_or_else(|| tr!("No run yet").to_owned(), |line| line.text.clone()),
            )
            .flex_none()
            .p_3()
            .gap_1()
            .when(lines.is_empty(), |s| {
                s.child(label(tr!(
                    "Run starts the job with these values. A connection without a remembered password asks for it first; a production connection asks before a risky statement."
                )))
            })
            .when(working, |s| {
                s.child(
                    row()
                        .gap_2()
                        .child(spinning_icon("job-run-spinner", I::LoaderCircle, colors().mint))
                        .child(label(tr!("Active"))),
                )
            })
            .children(lines.iter().map(line))
            .when_some(self.changed_file.clone(), |s, _| {
                s.child(
                    button("job-republish", I::Upload, tr!("Publish"))
                        .on_click(cx.listener(|this, _, _, cx| this.publish(cx))),
                )
            })
    }

    pub(super) fn runs_panel(&self, _cx: &mut Context<Self>) -> Div {
        column()
            .flex_1()
            .min_h_0()
            .when(self.revision.is_none(), |s| {
                s.child(div().p_3().child(label(tr!(
                    "This job is not saved yet: its draft runs are listed in Runs, in the Automation section, labelled as drafts."
                ))))
            })
            .child(self.history.clone())
    }
}

fn note_line(text: &'static str) -> Div {
    row()
        .items_start()
        .text_size(px(12.))
        .text_color(rgb(colors().faint))
        .child(icon(I::Info, colors().faint))
        .child(div().flex_1().min_w_0().child(text))
}
