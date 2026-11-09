//! The job tab's **Schedule** (specification, sections 9.5.2–9.5.8): the
//! rule as a form, the zone, the published version it runs, both policies,
//! the opt-in notification, and the schedule's state with **Turn on**,
//! **Confirm again**, **Turn off** and **Delete**. Saving or turning it on
//! opens the one consent dialog; the engine records the grant.
use super::*;
use crate::app::automation::schedule_words as says;
use gpui_kit::assets::IconName as I;
use gpui_kit::component::Sizable;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::Input;
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use jykre::automation::schedule::form::{DaysChoice, RuleForm, parse_limit};
use jykre::automation::schedule::rule::{self, Rule};
use jykre::automation::schedule::{Consent, DEFAULT_MISSED_LIMIT, missing_defaults};
use jykre::automation::{MissedPolicy, OverlapPolicy, ScheduleId};
use jykre_storage::{ScheduleDraft, StoredSchedule};

/// The Schedule tab's fields and what was read.
pub(super) struct ScheduleFields {
    /// The stored schedule; `None` until one is saved.
    pub stored: Option<StoredSchedule>,
    pub loaded: bool,
    pub form: RuleForm,
    pub every: Entity<InputState>,
    pub times: Entity<InputState>,
    pub month_days: Entity<InputState>,
    pub zone: Entity<InputState>,
    pub limit: Entity<InputState>,
    /// The published version it runs; a chain runs itself.
    pub version: Option<i64>,
    pub missed: MissedPolicy,
    pub overlap: OverlapPolicy,
    pub notify: bool,
    /// Computed when a field changes, never while drawing.
    pub preview: Preview,
    /// The consent waiting for the user's answer.
    pub consent: Option<Consent>,
    pub notice: Option<Feedback>,
    pub generation: u64,
}

/// What the form says now: the sentence, the next times, what to fix.
#[derive(Default)]
pub(super) struct Preview {
    pub sentence: Option<String>,
    pub next: Vec<String>,
    pub problems: Vec<String>,
    /// The stored schedule's next run, in its zone.
    pub next_run: Option<String>,
    pub status: Vec<String>,
}

fn choice(id: impl Into<ElementId>, on: bool, title: &'static str) -> Stateful<Div> {
    button(id, if on { I::CircleCheck } else { I::Circle }, title)
        .role(Role::RadioButton)
        .aria_toggled(if on { Toggled::True } else { Toggled::False })
        .when(on, |s| s.text_color(rgb(colors().mint)))
}

fn toggle(id: impl Into<ElementId>, on: bool, title: &'static str) -> Stateful<Div> {
    button(id, if on { I::SquareCheck } else { I::Square }, title)
        .role(Role::CheckBox)
        .aria_toggled(if on { Toggled::True } else { Toggled::False })
        .when(on, |s| s.text_color(rgb(colors().mint)))
}

fn line(text: impl Into<SharedString>) -> Div {
    div()
        .text_size(px(12.))
        .text_color(rgb(colors().muted))
        .child(text.into())
}

impl JobView {
    /// The Schedule tab's fields, made once per view.
    pub(super) fn make_schedule_fields(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let now = self.engine.clock().now();
        let form = RuleForm::new(now);
        let (zone, _) = rule::system_zone();
        let every = input(window, cx, form.every_minutes.clone(), tr!("Minutes"));
        let times = input(window, cx, form.times.clone(), tr!("09:00, 17:30"));
        let month_days = input(window, cx, form.month_days.clone(), tr!("1, 15, last"));
        let zone = input(window, cx, zone, tr!("Europe/Berlin"));
        let limit = input(window, cx, DEFAULT_MISSED_LIMIT.to_string(), tr!("2 to 24"));
        for field in [&every, &times, &month_days, &zone, &limit] {
            self.field_subscriptions.push(cx.subscribe(
                field,
                |this, _, event: &InputEvent, cx| {
                    if matches!(event, InputEvent::Change) {
                        this.schedule_changed(cx);
                    }
                },
            ));
        }
        self.schedule = Some(ScheduleFields {
            stored: None,
            loaded: false,
            form,
            every,
            times,
            month_days,
            zone,
            limit,
            version: self.versions.first().map(|version| version.version),
            missed: MissedPolicy::Ignore,
            overlap: OverlapPolicy::Skip,
            notify: false,
            preview: Preview::default(),
            consent: None,
            notice: None,
            generation: 0,
        });
        self.schedule_changed(cx);
    }

    /// Reads the job's schedule again.
    pub(super) fn load_schedule(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.revision.is_none() {
            return;
        }
        if self.schedule.is_none() {
            self.make_schedule_fields(window, cx);
        }
        let Some(repository) = self.engine.repository() else {
            return;
        };
        let Some(fields) = self.schedule.as_mut() else {
            return;
        };
        fields.generation += 1;
        let generation = fields.generation;
        let job = self.id;
        cx.spawn_in(window, async move |weak, cx| {
            let read = backend::run(async move { repository.schedule_of_job(job).await }).await;
            let _ = weak.update_in(cx, |this, window, cx| {
                let Some(fields) = this.schedule.as_mut() else {
                    return;
                };
                if fields.generation != generation {
                    return;
                }
                fields.loaded = true;
                match read {
                    Ok(stored) => {
                        let first = fields.stored.is_none();
                        fields.stored = stored;
                        if first {
                            this.fill_schedule_fields(window, cx);
                        }
                    }
                    Err(error) => {
                        fields.notice = Some(Feedback::new(Tone::Error, error.to_string()));
                    }
                }
                this.schedule_changed(cx);
            });
        })
        .detach();
    }

    /// Reads the stored schedule again after the engine changed; the fields
    /// the user is editing stay as they are.
    pub(super) fn refresh_schedule(&mut self, cx: &mut Context<Self>) {
        let Some(repository) = self.engine.repository() else {
            return;
        };
        let Some(fields) = self.schedule.as_mut() else {
            return;
        };
        fields.generation += 1;
        let generation = fields.generation;
        let job = self.id;
        cx.spawn(async move |weak, cx| {
            let read = backend::run(async move { repository.schedule_of_job(job).await }).await;
            let _ = weak.update(cx, |this, cx| {
                let Some(fields) = this.schedule.as_mut() else {
                    return;
                };
                if fields.generation != generation {
                    return;
                }
                if let Ok(stored) = read {
                    fields.stored = stored;
                }
                this.schedule_changed(cx);
            });
        })
        .detach();
    }

    /// The version the schedule runs: the one chosen, else the newest.
    fn schedule_version(&self) -> Option<i64> {
        self.schedule
            .as_ref()
            .and_then(|fields| fields.version)
            .or_else(|| self.versions.first().map(|version| version.version))
    }

    /// Puts the stored schedule in the fields.
    fn fill_schedule_fields(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let now = self.engine.clock().now();
        let Some(fields) = self.schedule.as_mut() else {
            return;
        };
        let Some(stored) = fields.stored.clone() else {
            return;
        };
        if let Ok(rule) = Rule::from_json(&stored.draft.rule_json) {
            fields.form = RuleForm::of(&rule, now);
        }
        fields.version = stored.draft.job_version.or(fields.version);
        fields.missed = stored.draft.missed_policy;
        fields.overlap = stored.draft.overlap_policy;
        fields.notify = stored.draft.notify;
        let form = fields.form.clone();
        let values = [
            (fields.every.clone(), form.every_minutes.clone()),
            (fields.times.clone(), form.times.clone()),
            (fields.month_days.clone(), form.month_days.clone()),
            (fields.zone.clone(), stored.draft.time_zone.clone()),
            (
                fields.limit.clone(),
                stored
                    .draft
                    .missed_limit
                    .unwrap_or(DEFAULT_MISSED_LIMIT)
                    .to_string(),
            ),
        ];
        for (field, value) in values {
            field.update(cx, |input, cx| input.set_value(value, window, cx));
        }
    }

    /// Reads the text fields into the form and computes what it says.
    pub(super) fn schedule_changed(&mut self, cx: &mut Context<Self>) {
        let now = self.engine.clock().now();
        let Some(fields) = self.schedule.as_mut() else {
            return;
        };
        fields.form.every_minutes = fields.every.read(cx).value().to_string();
        fields.form.times = fields.times.read(cx).value().to_string();
        fields.form.month_days = fields.month_days.read(cx).value().to_string();
        let zone_name = fields.zone.read(cx).value().trim().to_owned();
        let here = jiff::tz::TimeZone::try_system().unwrap_or(jiff::tz::TimeZone::UTC);
        let mut preview = Preview::default();
        let zone = rule::time_zone(&zone_name);
        if zone.is_none() {
            preview.problems.push(says::schedule_error(
                &jykre::automation::schedule::ScheduleError::UnknownZone,
            ));
        }
        match fields.form.rule() {
            Ok(rule) => {
                preview.sentence = Some(says::rule_sentence(&rule, &zone_name));
                if let Some(zone) = &zone {
                    preview.next = rule
                        .next_times(zone, now, 3)
                        .iter()
                        .map(|occurrence| match says::shift_note(occurrence) {
                            Some(note) => {
                                format!("{} — {note}", says::when_here(occurrence.at, zone, &here))
                            }
                            None => says::when_here(occurrence.at, zone, &here),
                        })
                        .collect();
                }
            }
            Err(problems) => preview.problems.extend(
                problems
                    .into_iter()
                    .map(|problem| says::rule_problem(problem).to_owned()),
            ),
        }
        if fields.missed == MissedPolicy::Queue
            && parse_limit(&fields.limit.read(cx).value()).is_none()
        {
            preview.problems.push(says::schedule_error(
                &jykre::automation::schedule::ScheduleError::MissedLimit,
            ));
        }
        if let Some(stored) = &fields.stored {
            let stored_zone = rule::time_zone(&stored.draft.time_zone);
            preview.status = says::status_lines(stored, stored_zone.as_ref());
            if stored.enabled
                && stored.suspended.is_none()
                && let (Some(zone), Ok(rule)) =
                    (&stored_zone, Rule::from_json(&stored.draft.rule_json))
            {
                let after = stored.last_due_at.map_or(now, |last| last.max(now));
                preview.next_run = rule
                    .next_after(zone, after)
                    .map(|at| says::when_here(at, zone, &here));
            }
        }
        fields.preview = preview;
        cx.notify();
    }

    /// The draft the fields describe, or what to fix.
    fn schedule_draft(&self, cx: &App) -> Result<ScheduleDraft, String> {
        let fields = self.schedule.as_ref().ok_or_default()?;
        let rule = fields.form.rule().map_err(|problems| {
            problems
                .into_iter()
                .map(|problem| says::rule_problem(problem).to_owned())
                .collect::<Vec<_>>()
                .join(" ")
        })?;
        let version = match self.kind {
            JobKind::Chain => None,
            _ => Some(self.schedule_version().ok_or_else(|| {
                says::schedule_error(&jykre::automation::schedule::ScheduleError::Missing)
            })?),
        };
        let missed_limit = match fields.missed {
            MissedPolicy::Queue => {
                Some(parse_limit(&fields.limit.read(cx).value()).ok_or_else(|| {
                    says::schedule_error(&jykre::automation::schedule::ScheduleError::MissedLimit)
                })?)
            }
            _ => None,
        };
        Ok(ScheduleDraft {
            id: fields
                .stored
                .as_ref()
                .map_or_else(ScheduleId::new, |stored| stored.draft.id),
            job_id: self.id,
            job_version: version,
            rule_json: rule.to_json(),
            time_zone: fields.zone.read(cx).value().trim().to_owned(),
            missed_policy: fields.missed,
            missed_limit,
            overlap_policy: fields.overlap,
            notify: fields.notify,
        })
    }

    fn schedule_notice(&mut self, tone: Tone, text: String, cx: &mut Context<Self>) {
        if let Some(fields) = self.schedule.as_mut() {
            fields.notice = Some(Feedback::new(tone, text));
        }
        cx.notify();
    }

    /// **Save**: the schedule, off, then the consent dialog (unless only
    /// the notification changed on a schedule that is on).
    fn save_schedule(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.writable() {
            self.schedule_notice(
                Tone::Error,
                says::schedule_error(&jykre::automation::schedule::ScheduleError::Unavailable),
                cx,
            );
            return;
        }
        let draft = match self.schedule_draft(cx) {
            Ok(draft) => draft,
            Err(text) => {
                self.schedule_notice(Tone::Error, text, cx);
                return;
            }
        };
        let expected = self
            .schedule
            .as_ref()
            .and_then(|fields| fields.stored.as_ref())
            .map(|stored| stored.revision);
        let engine = self.engine.clone();
        cx.spawn_in(window, async move |weak, cx| {
            let saved =
                backend::run(async move { Ok(engine.save_schedule(&draft, expected).await) }).await;
            let _ = weak.update_in(cx, |this, window, cx| match saved {
                Ok(Ok(stored)) => {
                    let ask = !stored.enabled;
                    let id = stored.draft.id;
                    if let Some(fields) = this.schedule.as_mut() {
                        fields.stored = Some(stored);
                    }
                    this.schedule_changed(cx);
                    if ask {
                        this.ask_consent(id, window, cx);
                    } else {
                        this.schedule_notice(Tone::Success, tr!("Saved.").to_owned(), cx);
                    }
                }
                Ok(Err(error)) => {
                    this.schedule_notice(Tone::Error, says::schedule_error(&error), cx)
                }
                Err(error) => this.schedule_notice(Tone::Error, error.to_string(), cx),
            });
        })
        .detach();
    }

    /// **Turn on** or **Confirm again**: the consent dialog for what the
    /// schedule would run now.
    fn ask_consent(&mut self, schedule: ScheduleId, window: &mut Window, cx: &mut Context<Self>) {
        let engine = self.engine.clone();
        let _ = window;
        cx.spawn(async move |weak, cx| {
            let consent = backend::run(async move { Ok(engine.consent(schedule).await) }).await;
            let _ = weak.update(cx, |this, cx| match consent {
                Ok(Ok(consent)) => {
                    let risks = this.consent_risks(&consent);
                    let here = jiff::tz::TimeZone::try_system().unwrap_or(jiff::tz::TimeZone::UTC);
                    let (title, description) = says::consent(&consent, &here, &risks);
                    if let Some(fields) = this.schedule.as_mut() {
                        fields.consent = Some(consent);
                    }
                    cx.emit(JobEvent::Confirm {
                        title,
                        description,
                        ok: tr!("Consent and schedule"),
                        what: Ask::ScheduleConsent,
                    });
                }
                Ok(Err(error)) => {
                    this.schedule_notice(Tone::Error, says::schedule_error(&error), cx)
                }
                Err(error) => this.schedule_notice(Tone::Error, error.to_string(), cx),
            });
        })
        .detach();
    }

    /// Each SQL step on a Production connection with its `guard::risk`
    /// findings: the consent is its production confirmation.
    fn consent_risks(&self, consent: &Consent) -> Vec<(String, String)> {
        let production: Vec<ConnectionId> = consent
            .connections
            .iter()
            .filter(|connection| {
                connection.environment
                    == Some(jykre::connection_appearance::Environment::Production)
            })
            .map(|connection| connection.id)
            .collect();
        consent
            .steps
            .iter()
            .filter_map(|step| {
                let JobBody::Sql(sql) = &step.definition.body else {
                    return None;
                };
                let connection = sql.connection.filter(|id| production.contains(id))?;
                let dialect =
                    source_of(&self.sources.sources, connection).and_then(|source| source.dialect);
                let risk = jykre::guard::risk(&sql.sql, dialect)?;
                let description = crate::app::workspace::risk_description(&[(0, risk)], 1);
                Some((step.job_name.clone(), description))
            })
            .collect()
    }

    /// The user consented in the dialog: record exactly what it showed.
    pub(super) fn consented(&mut self, cx: &mut Context<Self>) {
        let Some(consent) = self
            .schedule
            .as_mut()
            .and_then(|fields| fields.consent.take())
        else {
            return;
        };
        let engine = self.engine.clone();
        cx.spawn(async move |weak, cx| {
            let confirmed =
                backend::run(async move { Ok(engine.confirm_schedule(&consent).await) }).await;
            let _ = weak.update(cx, |this, cx| match confirmed {
                Ok(Ok(stored)) => {
                    if let Some(fields) = this.schedule.as_mut() {
                        fields.stored = Some(stored);
                    }
                    this.schedule_notice(
                        Tone::Success,
                        tr!("Scheduled. It runs only while Jykre Studio is open.").to_owned(),
                        cx,
                    );
                    this.schedule_changed(cx);
                }
                Ok(Err(error)) => {
                    this.schedule_notice(Tone::Error, says::schedule_error(&error), cx)
                }
                Err(error) => this.schedule_notice(Tone::Error, error.to_string(), cx),
            });
        })
        .detach();
    }

    fn turn_off_schedule(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.schedule_id() else {
            return;
        };
        let engine = self.engine.clone();
        cx.spawn(async move |weak, cx| {
            let done = backend::run(async move { Ok(engine.disable_schedule(id).await) }).await;
            let _ = weak.update(cx, |this, cx| match done {
                Ok(Ok(())) => this.schedule_notice(
                    Tone::Info,
                    tr!("Turned off: its consent was revoked. Turning it on asks again.")
                        .to_owned(),
                    cx,
                ),
                Ok(Err(error)) => {
                    this.schedule_notice(Tone::Error, says::schedule_error(&error), cx)
                }
                Err(error) => this.schedule_notice(Tone::Error, error.to_string(), cx),
            });
        })
        .detach();
    }

    /// Deletes the schedule after the dialog; its runs stay in the history.
    pub(super) fn delete_schedule(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.schedule_id() else {
            return;
        };
        let engine = self.engine.clone();
        cx.spawn(async move |weak, cx| {
            let done = backend::run(async move { Ok(engine.delete_schedule(id).await) }).await;
            let _ = weak.update(cx, |this, cx| match done {
                Ok(Ok(())) => {
                    if let Some(fields) = this.schedule.as_mut() {
                        fields.stored = None;
                    }
                    this.schedule_notice(
                        Tone::Info,
                        tr!("The schedule was deleted.").to_owned(),
                        cx,
                    );
                    this.schedule_changed(cx);
                }
                Ok(Err(error)) => {
                    this.schedule_notice(Tone::Error, says::schedule_error(&error), cx)
                }
                Err(error) => this.schedule_notice(Tone::Error, error.to_string(), cx),
            });
        })
        .detach();
    }

    fn schedule_id(&self) -> Option<ScheduleId> {
        self.schedule
            .as_ref()
            .and_then(|fields| fields.stored.as_ref())
            .map(|stored| stored.draft.id)
    }

    // -----------------------------------------------------------------------
    // Drawing.

    pub(super) fn schedule_panel(&self, cx: &mut Context<Self>) -> Stateful<Div> {
        let panel = column()
            .id("job-schedule")
            .role(Role::TabPanel)
            .aria_label(tr!("Schedule"))
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .track_scroll(&self.schedule_scroll)
            .p_3()
            .gap_3();
        let Some(fields) = &self.schedule else {
            return panel.child(line(tr!(
                "Save the job first: a schedule runs a published version, or a saved chain."
            )));
        };
        let missing = missing_defaults(self.definition.inputs());
        let unpublished = self.kind != JobKind::Chain && self.versions.is_empty();
        panel
            .child(self.schedule_state(fields, cx))
            .when(!missing.is_empty(), |s| {
                s.child(
                    div().id("schedule-inputs").role(Role::Alert).child(warning_line(
                        says::schedule_error(
                            &jykre::automation::schedule::ScheduleError::Inputs(missing),
                        ),
                    )),
                )
            })
            .when(unpublished, |s| {
                s.child(warning_line(says::schedule_error(
                    &jykre::automation::schedule::ScheduleError::Missing,
                )))
            })
            .child(self.schedule_rule(fields, cx))
            .child(self.schedule_policies(fields, cx))
            .child(
                row()
                    .gap_2()
                    .child(
                        primary_button("schedule-save", I::Save, tr!("Save the schedule"))
                            .on_click(cx.listener(|this, _, window, cx| this.save_schedule(window, cx))),
                    )
                    .child(line(tr!(
                        "Saving asks for your consent to exactly what will run; nothing is scheduled before it."
                    ))),
            )
    }

    fn schedule_state(&self, fields: &ScheduleFields, cx: &mut Context<Self>) -> Div {
        let stored = fields.stored.as_ref();
        let suspended = stored.is_some_and(|stored| stored.suspended.is_some());
        let on = stored.is_some_and(|stored| stored.enabled);
        section_box(tr!("Schedule"))
            .child(note_line(tr!(
                "A schedule runs only while Jykre Studio is open, and only in the window that holds automation; nothing runs while it is closed. Its runs share the queue, the limits and the history of runs started by hand."
            )))
            .when(self.availability == Availability::ReadOnly, |s| {
                s.child(warning_line(tr!(
                    "Another Jykre Studio window holds automation: this schedule runs there, not here."
                )))
            })
            .when(stored.is_none() && fields.loaded, |s| {
                s.child(line(tr!("This job has no schedule yet.")))
            })
            .when_some(fields.preview.next_run.clone(), |s, next| {
                s.child(
                    div()
                        .id("schedule-next")
                        .role(Role::Status)
                        .text_size(px(13.))
                        .child(tr!("Next run: {0}", next)),
                )
            })
            .children(fields.preview.status.iter().enumerate().map(|(index, text)| {
                div()
                    .id(("schedule-status", index))
                    .role(Role::Status)
                    .text_size(px(12.))
                    .text_color(rgb(if suspended && index == 0 {
                        colors().amber
                    } else {
                        colors().muted
                    }))
                    .child(text.clone())
            }))
            .when_some(fields.notice.as_ref(), |s, notice| {
                s.child(
                    div()
                        .id("schedule-notice")
                        .role(if notice.tone == Tone::Error {
                            Role::Alert
                        } else {
                            Role::Status
                        })
                        .child(feedback_line(notice)),
                )
            })
            .when_some(stored, |s, stored| {
                let id = stored.draft.id;
                s.child(
                    row()
                        .gap_2()
                        .when(!on, |s| {
                            s.child(
                                button("schedule-on", I::Play, tr!("Turn on"))
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.ask_consent(id, window, cx)
                                    })),
                            )
                        })
                        .when(suspended, |s| {
                            s.child(
                                primary_button("schedule-confirm", I::ShieldCheck, tr!("Confirm again"))
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.ask_consent(id, window, cx)
                                    })),
                            )
                        })
                        .when(on, |s| {
                            s.child(
                                button("schedule-off", I::Pause, tr!("Turn off"))
                                    .on_click(cx.listener(|this, _, _, cx| this.turn_off_schedule(cx))),
                            )
                        })
                        .child(
                            button("schedule-delete", I::Trash, tr!("Delete the schedule")).on_click(
                                cx.listener(|_, _, _, cx| {
                                    cx.emit(JobEvent::Confirm {
                                        title: tr!("Delete the schedule?").to_owned(),
                                        description: tr!(
                                            "It stops running and its consent is deleted. Runs it started stay in the history."
                                        )
                                        .to_owned(),
                                        ok: tr!("Delete"),
                                        what: Ask::DeleteSchedule,
                                    });
                                }),
                            ),
                        ),
                )
            })
    }

    fn schedule_rule(&self, fields: &ScheduleFields, cx: &mut Context<Self>) -> Div {
        let form = &fields.form;
        let set = |change: fn(&mut RuleForm)| {
            cx.listener(move |this: &mut Self, _: &ClickEvent, _, cx| {
                if let Some(fields) = this.schedule.as_mut() {
                    change(&mut fields.form);
                }
                this.schedule_changed(cx);
            })
        };
        let view = cx.entity().downgrade();
        let versions = self.versions.clone();
        section_box(tr!("When"))
            .when(self.kind != JobKind::Chain, |s| {
                s.child(labelled(
                    tr!("Published version it runs"),
                    Button::new("schedule-version")
                        .ghost()
                        .small()
                        .label(match self.schedule_version() {
                            Some(version) => tr!("Version {0}", version),
                            None => tr!("No published version").to_owned(),
                        })
                        .dropdown_caret(true)
                        .accessibility_label(tr!("The published version the schedule runs"))
                        .dropdown_menu(move |mut menu, _, _| {
                            for version in &versions {
                                let number = version.version;
                                let view = view.clone();
                                menu = menu.item(PopupMenuItem::new(tr!("Version {0}", number)).on_click(
                                    move |_, _, cx| {
                                        let _ = view.update(cx, |this, cx| {
                                            if let Some(fields) = this.schedule.as_mut() {
                                                fields.version = Some(number);
                                            }
                                            this.schedule_changed(cx);
                                        });
                                    },
                                ));
                            }
                            menu
                        }),
                ))
            })
            .child(
                row()
                    .id("schedule-kind")
                    .role(Role::RadioGroup)
                    .aria_label(tr!("Kind of rule"))
                    .gap_2()
                    .child(
                        choice("schedule-calendar", !form.interval, tr!("At times of day"))
                            .on_click(set(|form| form.interval = false)),
                    )
                    .child(
                        choice("schedule-interval", form.interval, tr!("Every N minutes"))
                            .on_click(set(|form| form.interval = true)),
                    ),
            )
            .map(|s| {
                if form.interval {
                    s.child(labelled(
                        tr!("Every (minutes, 1 to 1440)"),
                        Input::new(&fields.every).small().aria_label(tr!("Interval in minutes")),
                    ))
                    .child(note_line(tr!(
                        "An interval counts absolute time from its first run: when daylight saving changes, its clock times move by an hour, its spacing does not."
                    )))
                } else {
                    s.child(labelled(
                        tr!("Times of day (HH:MM, up to 24)"),
                        Input::new(&fields.times).small().aria_label(tr!("Times of day")),
                    ))
                    .child(
                        row()
                            .id("schedule-days")
                            .role(Role::RadioGroup)
                            .aria_label(tr!("Days"))
                            .gap_2()
                            .child(
                                choice("schedule-every-day", form.days == DaysChoice::EveryDay, tr!("Every day"))
                                    .on_click(set(|form| form.days = DaysChoice::EveryDay)),
                            )
                            .child(
                                choice("schedule-weekdays", form.days == DaysChoice::Weekdays, tr!("Days of the week"))
                                    .on_click(set(|form| form.days = DaysChoice::Weekdays)),
                            )
                            .child(
                                choice("schedule-month-days", form.days == DaysChoice::MonthDays, tr!("Days of the month"))
                                    .on_click(set(|form| form.days = DaysChoice::MonthDays)),
                            ),
                    )
                    .when(form.days == DaysChoice::Weekdays, |s| {
                        s.child(row().flex_wrap().gap_1().children((0..7usize).map(|day| {
                            let on = form.weekdays.get(day).copied().unwrap_or(false);
                            let name = says_weekday(day);
                            toggle(("schedule-weekday", day), on, name).on_click(cx.listener(
                                move |this, _, _, cx| {
                                    if let Some(slot) = this
                                        .schedule
                                        .as_mut()
                                        .and_then(|fields| fields.form.weekdays.get_mut(day))
                                    {
                                        *slot = !*slot;
                                    }
                                    this.schedule_changed(cx);
                                },
                            ))
                        })))
                    })
                    .when(form.days == DaysChoice::MonthDays, |s| {
                        s.child(labelled(
                            tr!("Days of the month (1 to 31, or last)"),
                            Input::new(&fields.month_days).small().aria_label(tr!("Days of the month")),
                        ))
                        .child(note_line(tr!(
                            "A day a month lacks (the 31st in April) has no run that month; it is not moved."
                        )))
                    })
                    .child(label(tr!("Months (none chosen: every month)")))
                    .child(row().flex_wrap().gap_1().children((0..12usize).map(|month| {
                        let on = form.months.get(month).copied().unwrap_or(false);
                        let name = says::month_name(u8::try_from(month + 1).unwrap_or(1));
                        toggle(("schedule-month", month), on, name).on_click(cx.listener(
                            move |this, _, _, cx| {
                                if let Some(slot) = this
                                    .schedule
                                    .as_mut()
                                    .and_then(|fields| fields.form.months.get_mut(month))
                                {
                                    *slot = !*slot;
                                }
                                this.schedule_changed(cx);
                            },
                        ))
                    })))
                }
            })
            .child(labelled(
                tr!("Time zone (IANA name)"),
                Input::new(&fields.zone).small().aria_label(tr!("Time zone")),
            ))
            .when_some(fields.preview.sentence.clone(), |s, sentence| {
                s.child(div().text_size(px(13.)).child(sentence))
            })
            .when(!fields.preview.next.is_empty(), |s| {
                s.child(label(tr!("The next three times")))
                    .children(fields.preview.next.iter().map(|text| line(text.clone())))
            })
            .when(!fields.preview.problems.is_empty(), |s| {
                s.child(
                    div()
                        .id("schedule-problems")
                        .role(Role::Alert)
                        .child(warning_line(fields.preview.problems.join(" "))),
                )
            })
    }

    fn schedule_policies(&self, fields: &ScheduleFields, cx: &mut Context<Self>) -> Div {
        let missed = |id: &'static str, policy: MissedPolicy, title: &'static str| {
            choice(id, fields.missed == policy, title).on_click(cx.listener(
                move |this, _, _, cx| {
                    if let Some(fields) = this.schedule.as_mut() {
                        fields.missed = policy;
                    }
                    this.schedule_changed(cx);
                },
            ))
        };
        let missed_rows = column()
            .id("schedule-missed")
            .role(Role::RadioGroup)
            .aria_label(tr!("Times missed while closed"))
            .gap_1()
            .child(missed(
                "schedule-missed-ignore",
                MissedPolicy::Ignore,
                tr!("Do not run them"),
            ))
            .child(missed(
                "schedule-missed-once",
                MissedPolicy::RunOnce,
                tr!("Run once for all of them"),
            ))
            .child(missed(
                "schedule-missed-queue",
                MissedPolicy::Queue,
                tr!("Run up to N of the most recent"),
            ));
        let overlap = |id: &'static str, policy: OverlapPolicy, title: &'static str| {
            choice(id, fields.overlap == policy, title).on_click(cx.listener(
                move |this, _, _, cx| {
                    if let Some(fields) = this.schedule.as_mut() {
                        fields.overlap = policy;
                    }
                    this.schedule_changed(cx);
                },
            ))
        };
        let limit = parse_limit(&fields.limit.read(cx).value());
        section_box(tr!("Missed times and overlap"))
            .child(label(tr!("Times missed while Jykre Studio was closed, asleep or without automation")))
            .child(missed_rows)
            .when(fields.missed == MissedPolicy::Queue, |s| {
                s.child(labelled(
                    tr!("N, from 2 to 24"),
                    Input::new(&fields.limit).small().aria_label(tr!("How many missed times to catch up")),
                ))
            })
            .child(note_line(says::missed_policy(fields.missed, limit)))
            .child(note_line(tr!(
                "A catch-up is a new run, marked catch-up in the history; a run that started is never run again. Catch-ups go one at a time, and ones not started yet are dropped when Jykre Studio closes."
            )))
            .child(label(tr!("A time that comes while the previous run is still going")))
            .child(
                column()
                    .id("schedule-overlap")
                    .role(Role::RadioGroup)
                    .aria_label(tr!("Overlap"))
                    .gap_1()
                    .child(overlap("schedule-overlap-skip", OverlapPolicy::Skip, tr!("Skip it and record it")))
                    .child(overlap("schedule-overlap-queue", OverlapPolicy::QueueOne, tr!("Wait for the run to end (at most one waits)")))
                    .child(overlap("schedule-overlap-concurrent", OverlapPolicy::Concurrent, tr!("Run beside it, within the limits"))),
            )
            .child(note_line(says::overlap_policy(fields.overlap)))
            .child(note_line(tr!(
                "A time that comes during a catch-up follows this choice too. Changing a policy asks for consent again."
            )))
            .child(
                toggle("schedule-notify", fields.notify, tr!("Notify me when a scheduled run ends badly"))
                    .on_click(cx.listener(|this, _, _, cx| {
                        if let Some(fields) = this.schedule.as_mut() {
                            fields.notify = !fields.notify;
                        }
                        this.schedule_changed(cx);
                    })),
            )
            .child(note_line(tr!(
                "The notification names the job, the outcome and the cause, never a value. Every bad end also shows in the Automation section."
            )))
    }
}

fn says_weekday(day: usize) -> &'static str {
    match day {
        0 => tr!("Monday"),
        1 => tr!("Tuesday"),
        2 => tr!("Wednesday"),
        3 => tr!("Thursday"),
        4 => tr!("Friday"),
        5 => tr!("Saturday"),
        _ => tr!("Sunday"),
    }
}

fn section_box(title: &'static str) -> Div {
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

fn labelled(title: &'static str, child: impl IntoElement) -> Div {
    column().gap_1().child(label(title)).child(child)
}

fn note_line(text: impl Into<SharedString>) -> Div {
    row()
        .items_start()
        .text_size(px(12.))
        .text_color(rgb(colors().muted))
        .child(div().pt(px(1.)).child(icon(I::Info, colors().faint)))
        .child(div().flex_1().min_w_0().child(text.into()))
}

fn warning_line(text: impl Into<SharedString>) -> Div {
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

trait OkOrDefault<T> {
    fn ok_or_default(self) -> Result<T, String>;
}

impl<T> OkOrDefault<T> for Option<T> {
    fn ok_or_default(self) -> Result<T, String> {
        self.ok_or_else(String::new)
    }
}
