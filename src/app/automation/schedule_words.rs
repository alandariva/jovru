//! What schedules say, in words (specification, sections 9.5.2–9.5.8): the
//! rule as a sentence, due times in the schedule's zone, the policies, the
//! consent, a suspension and the last outcome. GPUI-free, so each sentence
//! is tested without a window.
use crate::app::ui::environment_name;
use jiff::tz::TimeZone;
use jykre::automation::schedule::rule::{
    Days, Local, MonthDay, Occurrence, Rule, RuleProblem, Shift, local,
};
use jykre::automation::schedule::{BadEnd, Consent, ScheduleError};
use jykre::automation::{Cause, JobKind, MissedPolicy, OverlapPolicy, ScheduleOutcome, UtcMillis};
use jykre::tr;
use jykre_storage::StoredSchedule;

fn weekday_name(day: i8) -> &'static str {
    match day {
        1 => tr!("Mon"),
        2 => tr!("Tue"),
        3 => tr!("Wed"),
        4 => tr!("Thu"),
        5 => tr!("Fri"),
        6 => tr!("Sat"),
        _ => tr!("Sun"),
    }
}

fn weekday_plural(day: u8) -> &'static str {
    match day {
        1 => tr!("Mondays"),
        2 => tr!("Tuesdays"),
        3 => tr!("Wednesdays"),
        4 => tr!("Thursdays"),
        5 => tr!("Fridays"),
        6 => tr!("Saturdays"),
        _ => tr!("Sundays"),
    }
}

pub fn month_name(month: u8) -> &'static str {
    match month {
        1 => tr!("January"),
        2 => tr!("February"),
        3 => tr!("March"),
        4 => tr!("April"),
        5 => tr!("May"),
        6 => tr!("June"),
        7 => tr!("July"),
        8 => tr!("August"),
        9 => tr!("September"),
        10 => tr!("October"),
        11 => tr!("November"),
        _ => tr!("December"),
    }
}

/// `UTC+01:00`.
fn offset(seconds: i32) -> String {
    let sign = if seconds < 0 { '-' } else { '+' };
    let minutes = seconds.unsigned_abs() / 60;
    format!("UTC{sign}{:02}:{:02}", minutes / 60, minutes % 60)
}

/// `Mon 2026-11-09 09:00`.
fn stamp(local: &Local) -> String {
    format!(
        "{} {:04}-{:02}-{:02} {:02}:{:02}",
        weekday_name(local.weekday),
        local.year,
        local.month,
        local.day,
        local.hour,
        local.minute
    )
}

/// A due time in the schedule's zone, with its abbreviation and offset:
/// `Mon 2026-11-09 09:00 CET (UTC+01:00)`.
pub fn when(at: UtcMillis, zone: &TimeZone) -> String {
    match local(at, zone) {
        Some(local) => format!(
            "{} {} ({})",
            stamp(&local),
            local.abbreviation,
            offset(local.offset_seconds)
        ),
        None => tr!("an instant out of range").to_owned(),
    }
}

/// [`when`], and the same instant on this computer's clock when its zone
/// shows another time (section 9.5.3).
pub fn when_here(at: UtcMillis, zone: &TimeZone, here: &TimeZone) -> String {
    let there = when(at, zone);
    match (local(at, zone), local(at, here)) {
        (Some(theirs), Some(ours)) if theirs.offset_seconds != ours.offset_seconds => {
            tr!("{0}; {1} {2} here", there, stamp(&ours), ours.abbreviation)
        }
        _ => there,
    }
}

/// What daylight saving did to one due time, when it did anything.
pub fn shift_note(occurrence: &Occurrence) -> Option<String> {
    match occurrence.shift {
        Shift::None => None,
        Shift::Gap { minutes } => Some(tr!(
            "the clocks jump forward that day: the time does not exist, so it runs {0} minutes later",
            minutes
        )),
        Shift::Fold => Some(
            tr!("the clocks go back that day: the time happens twice and runs once, at the first")
                .to_owned(),
        ),
    }
}

fn list(items: &[String]) -> String {
    match items {
        [] => String::new(),
        [one] => one.clone(),
        [rest @ .., last] => tr!("{0} and {1}", rest.join(", "), last),
    }
}

/// The rule in one sentence, with its zone: "Every day at 09:00 and 17:30,
/// Europe/Berlin".
pub fn rule_sentence(rule: &Rule, zone: &str) -> String {
    match rule {
        Rule::Interval { every_minutes, .. } => {
            let every = if every_minutes % 60 == 0 {
                if *every_minutes == 60 {
                    tr!("Every hour").to_owned()
                } else {
                    tr!("Every {0} hours", every_minutes / 60)
                }
            } else if *every_minutes == 1 {
                tr!("Every minute").to_owned()
            } else {
                tr!("Every {0} minutes", every_minutes)
            };
            tr!(
                "{0}, in absolute time ({1} only shows when): daylight saving moves the clock times, not the spacing",
                every,
                zone
            )
        }
        Rule::Calendar { at, days, months } => {
            let times = list(&at.iter().map(|time| time.text()).collect::<Vec<_>>());
            let days = match days {
                Days::EveryDay => tr!("Every day").to_owned(),
                Days::Weekdays(days) if days.as_slice() == [1, 2, 3, 4, 5] => {
                    tr!("Weekdays").to_owned()
                }
                Days::Weekdays(days) => list(
                    &days
                        .iter()
                        .map(|day| weekday_plural(*day).to_owned())
                        .collect::<Vec<_>>(),
                ),
                Days::MonthDays(days) => tr!(
                    "On day {0} of the month",
                    list(
                        &days
                            .iter()
                            .map(|day| match day {
                                MonthDay::Day(day) => day.to_string(),
                                MonthDay::Last => tr!("the last").to_owned(),
                            })
                            .collect::<Vec<_>>()
                    )
                ),
            };
            let sentence = tr!("{0} at {1}, {2}", days, times, zone);
            if months.is_empty() {
                sentence
            } else {
                tr!(
                    "{0}; only in {1}",
                    sentence,
                    list(
                        &months
                            .iter()
                            .map(|month| month_name(*month).to_owned())
                            .collect::<Vec<_>>()
                    )
                )
            }
        }
    }
}

pub fn rule_problem(problem: RuleProblem) -> &'static str {
    match problem {
        RuleProblem::Malformed => tr!("The stored rule cannot be read by this version."),
        RuleProblem::Interval => {
            tr!("The interval must be from 1 minute to 24 hours (1440 minutes).")
        }
        RuleProblem::Times => tr!("Write one or more times of day as HH:MM, such as 09:00, 17:30."),
        RuleProblem::TooManyTimes => tr!("At most 24 times a day."),
        RuleProblem::NoDays => tr!("Choose at least one day."),
        RuleProblem::DayRange => tr!("Days of the month are 1 to 31, or last."),
        RuleProblem::Months => tr!("Months are January to December."),
        RuleProblem::Never => tr!(
            "Those days never happen in the months chosen, so it would never run. A day a month lacks is skipped, not moved."
        ),
    }
}

pub fn missed_policy(policy: MissedPolicy, limit: Option<i64>) -> String {
    match policy {
        MissedPolicy::Ignore => tr!(
            "Times missed while Jykre Studio was closed, the computer slept or another window held automation are not run; they are counted and shown."
        )
        .to_owned(),
        MissedPolicy::RunOnce => tr!(
            "Times missed while Jykre Studio was closed, the computer slept or another window held automation run once, as one catch-up run, when it can run again."
        )
        .to_owned(),
        MissedPolicy::Queue => tr!(
            "The {0} most recent times missed while Jykre Studio was closed, the computer slept or another window held automation run as catch-up runs, one after another; older ones are only counted.",
            limit.unwrap_or(0)
        ),
    }
}

pub fn overlap_policy(policy: OverlapPolicy) -> &'static str {
    match policy {
        OverlapPolicy::Skip => tr!(
            "A time that comes while the previous run is still queued or running is skipped and recorded."
        ),
        OverlapPolicy::QueueOne => tr!(
            "A time that comes while the previous run is still queued or running waits for it to end; at most one waits, later ones are skipped."
        ),
        OverlapPolicy::Concurrent => tr!(
            "A time that comes while the previous run is still going starts another beside it, still within the queue and the limits: 2 runs at once, 1 per connection."
        ),
    }
}

pub fn suspended_reason(code: &str) -> &'static str {
    match code {
        "connection_changed" => tr!("its connection was edited since you consented"),
        "environment_changed" => tr!("its connection's environment label changed"),
        "connection_deleted" => tr!("a connection it uses was deleted or cannot be read"),
        "chain_changed" => tr!("the chain was edited since you consented"),
        "target_changed" => tr!("what it runs changed since you consented"),
        "program_changed" => tr!("the program's file changed since its version was published"),
        "time_zone_unknown" => tr!("its time zone is no longer known"),
        _ => tr!("something it depends on changed"),
    }
}

pub fn refused_reason(code: &str) -> &'static str {
    match code {
        "queue_full" => tr!("the queue was full"),
        "queue_timeout" => tr!("it waited in the queue longer than 60 s"),
        "input" => tr!("a default value no longer passes its check"),
        "connection" => tr!("its connection cannot run now"),
        "history" => tr!("its history could not be written"),
        "program" => tr!("its program cannot start"),
        "target" => tr!("what it runs is not stored any more"),
        _ => tr!("automation could not run it"),
    }
}

/// The schedule's state, last outcome and missed times, for its line.
pub fn status_lines(schedule: &StoredSchedule, zone: Option<&TimeZone>) -> Vec<String> {
    let mut lines = Vec::new();
    if !schedule.enabled {
        lines.push(tr!("Off: nothing is scheduled until you turn it on and consent.").to_owned());
    } else if let Some(reason) = &schedule.suspended {
        lines.push(tr!(
            "Suspended: {0}. Nothing runs until you confirm it again.",
            suspended_reason(reason)
        ));
    }
    let when_due =
        |at: UtcMillis| zone.map_or_else(|| tr!("a past time").to_owned(), |zone| when(at, zone));
    if let (Some(outcome), Some(due)) = (schedule.last_outcome, schedule.last_due_at) {
        let due = when_due(due);
        lines.push(match outcome {
            ScheduleOutcome::Started => tr!("Last: started on time for {0}.", due),
            ScheduleOutcome::CaughtUp => tr!("Last: a catch-up run started for {0}.", due),
            ScheduleOutcome::Waiting => tr!("Last: {0} waits for the run still going to end.", due),
            ScheduleOutcome::SkippedRunning => {
                if schedule.last_reason.as_deref() == Some("coalesced") {
                    tr!(
                        "Last: {0} was skipped; one time already waits for the run still going.",
                        due
                    )
                } else {
                    tr!("Last: skipped {0}, the previous run was still going.", due)
                }
            }
            ScheduleOutcome::Missed => tr!("Last: {0} was missed.", due),
            ScheduleOutcome::Refused => tr!(
                "Last: {0} did not run: {1}. No run was recorded.",
                due,
                refused_reason(schedule.last_reason.as_deref().unwrap_or_default())
            ),
            ScheduleOutcome::Suspended => tr!("Last: suspended at {0}.", due),
        });
    }
    if schedule.missed_count > 0 {
        lines.push(tr!(
            "Missed {0} times while Jykre Studio was closed, asleep or without automation (last: {1}).",
            schedule.missed_count,
            schedule
                .missed_last_at
                .map_or_else(String::new, when_due)
        ));
    }
    lines
}

/// What a schedule could not do, in words.
pub fn schedule_error(error: &ScheduleError) -> String {
    match error {
        ScheduleError::Unavailable => {
            tr!("Automation is read-only or off here, so schedules cannot change.").to_owned()
        }
        ScheduleError::Rule(problems) => problems
            .iter()
            .map(|problem| rule_problem(*problem))
            .collect::<Vec<_>>()
            .join(" "),
        ScheduleError::UnknownZone => tr!(
            "That time zone is not known. Write an IANA name, such as Europe/Berlin, America/Sao_Paulo or UTC."
        )
        .to_owned(),
        ScheduleError::MissedLimit => tr!("Choose how many missed times to catch up: 2 to 24.").to_owned(),
        ScheduleError::Missing => tr!(
            "Only a published version or a saved chain can be scheduled. Publish the job (or save the chain) first."
        )
        .to_owned(),
        ScheduleError::Inputs(names) => tr!(
            "It cannot be scheduled: a scheduled run uses the defaults only, and {0} has no default. Give it one, or bind it in a chain.",
            names.join(", ")
        ),
        ScheduleError::Chain => tr!("The chain does not match the versions it names.").to_owned(),
        ScheduleError::NoConnection => tr!(
            "A connection it uses is not available. Open the connection list and check it first."
        )
        .to_owned(),
        ScheduleError::Changed => tr!(
            "Something it binds changed while the dialog was open. Nothing was consented to; try again."
        )
        .to_owned(),
        ScheduleError::Taken => tr!("This job already has a schedule.").to_owned(),
        ScheduleError::Storage(message) => tr!("Could not write the schedule: {0}", message),
    }
}

/// The consent dialog (section 9.5.6): its title and what it says. `risks`
/// holds each SQL step on a Production connection with its findings.
pub fn consent(consent: &Consent, here: &TimeZone, risks: &[(String, String)]) -> (String, String) {
    let title = if consent.suspended.is_some() {
        tr!("Confirm the schedule of {0} again?", consent.name)
    } else {
        tr!("Schedule {0}?", consent.name)
    };
    let mut parts = Vec::new();
    if let Some(reason) = &consent.suspended {
        let mut changed = tr!("It was suspended: {0}.", suspended_reason(reason));
        if !consent.changed.is_empty() {
            changed = tr!("{0} Changed: {1}.", changed, consent.changed.join(", "));
        }
        parts.push(changed);
    }
    parts.push(match consent.kind {
        JobKind::Chain => tr!(
            "Runs the chain {0}: {1}.",
            consent.name,
            consent
                .steps
                .iter()
                .map(|step| tr!("{0} version {1}", step.job_name, step.version))
                .collect::<Vec<_>>()
                .join(" → ")
        ),
        _ => tr!(
            "Runs version {0} of {1}.",
            consent.steps.first().map_or(0, |step| step.version),
            consent.name
        ),
    });
    parts.push(rule_sentence(&consent.rule, &consent.zone));
    if let Some(zone) = jykre::automation::schedule::rule::time_zone(&consent.zone) {
        let next: Vec<String> = consent
            .next
            .iter()
            .map(|occurrence| match shift_note(occurrence) {
                Some(note) => tr!("{0} ({1})", when_here(occurrence.at, &zone, here), note),
                None => when_here(occurrence.at, &zone, here),
            })
            .collect();
        if !next.is_empty() {
            parts.push(tr!("Next: {0}.", next.join("; ")));
        }
    }
    parts.push(
        tr!(
            "It runs only while Jykre Studio is open, in this window; nothing runs while it is closed."
        )
        .to_owned(),
    );
    parts.push(missed_policy(
        consent.schedule.draft.missed_policy,
        consent.schedule.draft.missed_limit,
    ));
    parts.push(overlap_policy(consent.schedule.draft.overlap_policy).to_owned());
    for connection in &consent.connections {
        let label = connection.environment.map_or_else(
            || tr!("no environment").to_owned(),
            |environment| environment_name(environment).to_owned(),
        );
        let mut line = tr!("Connection {0} ({1}).", connection.name, label);
        if !connection.remembers_password {
            line = tr!(
                "{0} It does not remember its password: a scheduled run never asks, so it would fail with Authentication.",
                line
            );
        }
        parts.push(line);
    }
    for (step, risk) in risks {
        parts.push(tr!(
            "On Production, {0}: {1} Consenting here is the production confirmation: scheduled runs do not ask again.",
            step,
            risk
        ));
    }
    if consent.production && risks.is_empty() {
        parts.push(
            tr!("A connection is labelled Production: consenting here is its confirmation; scheduled runs do not ask again.")
                .to_owned(),
        );
    }
    for step in &consent.steps {
        if let jykre::automation::definition::JobBody::Process(process) = &step.definition.body
            && let Some(pinned) = &process.pinned
        {
            parts.push(tr!(
                "{0} runs {1} ({2}) as trusted code: it is not sandboxed, and a changed file suspends the schedule.",
                step.job_name,
                pinned.path,
                pinned.sha256
            ));
        }
    }
    if consent.inputs.is_empty() {
        parts.push(tr!("It takes no inputs.").to_owned());
    } else {
        parts.push(tr!(
            "Inputs: the defaults only ({0}).",
            consent
                .inputs
                .iter()
                .map(|(name, value)| match value {
                    Some(value) => format!("{name} = {value}"),
                    None => tr!("{0} empty", name),
                })
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    parts.push(
        tr!("Any change to the connections, their labels or what it runs suspends it until you confirm again.")
            .to_owned(),
    );
    (title, parts.join("\n\n"))
}

/// A scheduled run that ended badly, for the notification and the status
/// bar: the schedule's name, the outcome and the cause, never a value or a
/// server message (Q-40).
pub fn bad_end(end: &BadEnd) -> (String, String) {
    let title = tr!("Scheduled run of {0}", end.name);
    let body = match (end.refused, end.cause) {
        (Some(reason), _) => tr!("Did not run: {0}.", refused_reason(reason)),
        (None, Some(cause)) => tr!("Failed: {0}.", cause_word(cause)),
        (None, None) => tr!("Ended badly.").to_owned(),
    };
    (title, body)
}

fn cause_word(cause: Cause) -> String {
    super::words::state_label(jykre::automation::RunState::Failed, Some(cause), None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use jykre::automation::schedule::rule::{LocalTime, time_zone};

    fn at(text: &str) -> UtcMillis {
        UtcMillis(text.parse::<jiff::Timestamp>().unwrap().as_millisecond())
    }

    #[test]
    fn due_times_read_in_the_zone_with_the_local_equivalent() {
        let berlin = time_zone("Europe/Berlin").unwrap();
        let sao_paulo = time_zone("America/Sao_Paulo").unwrap();
        let due = at("2026-11-09T08:00:00Z");
        assert_eq!(when(due, &berlin), "Mon 2026-11-09 09:00 CET (UTC+01:00)");
        assert_eq!(
            when_here(due, &berlin, &sao_paulo),
            "Mon 2026-11-09 09:00 CET (UTC+01:00); Mon 2026-11-09 05:00 -03 here"
        );
        assert_eq!(when_here(due, &berlin, &berlin), when(due, &berlin));
    }

    #[test]
    fn rules_read_as_one_sentence() {
        let calendar = Rule::Calendar {
            at: vec![
                LocalTime::parse("09:00").unwrap(),
                LocalTime::parse("17:30").unwrap(),
            ],
            days: Days::Weekdays(vec![1, 2, 3, 4, 5]),
            months: vec![],
        };
        assert_eq!(
            rule_sentence(&calendar, "Europe/Berlin"),
            "Weekdays at 09:00 and 17:30, Europe/Berlin"
        );
        let monthly = Rule::Calendar {
            at: vec![LocalTime::parse("06:00").unwrap()],
            days: Days::MonthDays(vec![MonthDay::Day(1), MonthDay::Day(15), MonthDay::Last]),
            months: vec![3, 6],
        };
        assert_eq!(
            rule_sentence(&monthly, "UTC"),
            "On day 1, 15 and the last of the month at 06:00, UTC; only in March and June"
        );
        let hourly = Rule::Interval {
            every_minutes: 120,
            anchor: UtcMillis(0),
        };
        assert!(rule_sentence(&hourly, "UTC").starts_with("Every 2 hours, in absolute time"));
    }

    #[test]
    fn dst_notes_and_policies_are_stated() {
        assert!(
            shift_note(&Occurrence {
                at: UtcMillis(0),
                shift: Shift::Gap { minutes: 60 }
            })
            .unwrap()
            .contains("60 minutes later")
        );
        assert!(
            shift_note(&Occurrence {
                at: UtcMillis(0),
                shift: Shift::Fold
            })
            .unwrap()
            .contains("runs once")
        );
        assert!(missed_policy(MissedPolicy::Queue, Some(3)).contains("The 3 most recent"));
        assert!(missed_policy(MissedPolicy::Ignore, None).contains("are not run"));
        assert!(overlap_policy(OverlapPolicy::Concurrent).contains("1 per connection"));
        assert_eq!(
            suspended_reason("connection_changed"),
            "its connection was edited since you consented"
        );
        assert_eq!(suspended_reason("later"), "something it depends on changed");
    }
}
