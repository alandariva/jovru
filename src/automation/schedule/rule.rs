//! A schedule's rule and its due times in a time zone (specification,
//! sections 9.5.2 and 9.5.3). The form is the grammar: a rule is an interval
//! or times of day on chosen days, stored as canonical JSON and evaluated
//! with `jiff`'s time-zone database. Pure: no clock, no I/O beyond the zone
//! database `jiff` reads.
//!
//! A local time in a daylight-saving gap moves forward by the gap's length,
//! and one in a fold runs once, at the earlier instant (`jiff`'s
//! `Disambiguation::Compatible`, RFC 5545's rule).
use crate::automation::UtcMillis;
use crate::automation::definition::canonical_json;
use jiff::Timestamp;
use jiff::civil::{Date, Time};
use jiff::tz::{AmbiguousOffset, TimeZone};
use serde_json::{Value, json};
use std::time::Duration;

/// The format of a stored rule.
pub const RULE_SCHEMA_VERSION: &str = "1.0";
/// The shortest interval (Q-39).
pub const MIN_INTERVAL_MINUTES: u32 = 1;
/// The longest interval: longer periods are calendar rules (Q-39).
pub const MAX_INTERVAL_MINUTES: u32 = 24 * 60;
/// The most times a day of a calendar rule (Q-39).
pub const MAX_TIMES_A_DAY: usize = 24;
/// How far back missed times are counted for a calendar rule: about ten
/// years. Older ones are neither counted nor caught up.
pub const MAX_SCAN_DAYS: i64 = 3_660;
/// How far ahead the next time of a calendar rule is looked for: four years
/// and a day cover the 29th of February.
const LOOK_AHEAD_DAYS: i64 = 4 * 366 + 1;
const MINUTE_MS: i64 = 60_000;

/// A local time of day, `HH:MM`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LocalTime {
    pub hour: u8,
    pub minute: u8,
}

impl LocalTime {
    /// `HH:MM` (or `H:MM`), 00:00 to 23:59.
    pub fn parse(text: &str) -> Option<Self> {
        let (hour, minute) = text.trim().split_once(':')?;
        if minute.len() != 2 || hour.is_empty() || hour.len() > 2 {
            return None;
        }
        let hour: u8 = hour.parse().ok()?;
        let minute: u8 = minute.parse().ok()?;
        (hour < 24 && minute < 60).then_some(Self { hour, minute })
    }

    pub fn text(self) -> String {
        format!("{:02}:{:02}", self.hour, self.minute)
    }
}

/// A day of a month: a number, or the month's last day.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum MonthDay {
    /// 1 to 31; a month without that day has no run (it is not moved).
    Day(u8),
    Last,
}

/// The days a calendar rule runs on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Days {
    EveryDay,
    /// ISO weekdays: 1 is Monday, 7 is Sunday. Sorted, without repeats.
    Weekdays(Vec<u8>),
    /// Sorted, without repeats.
    MonthDays(Vec<MonthDay>),
}

/// A schedule's rule.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Rule {
    /// Every `every_minutes` from `anchor`, in absolute time: unaffected by
    /// daylight saving.
    Interval {
        every_minutes: u32,
        anchor: UtcMillis,
    },
    /// At the local times `at`, on `days`, in `months` (empty: every month).
    Calendar {
        at: Vec<LocalTime>,
        days: Days,
        months: Vec<u8>,
    },
}

/// Why a rule cannot be saved. Each names what the form must fix.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RuleProblem {
    /// Not a rule this version reads.
    Malformed,
    /// The interval is outside 1 minute to 24 hours.
    Interval,
    /// No time of day, or a time that is not `HH:MM`.
    Times,
    /// More than 24 times a day.
    TooManyTimes,
    /// Weekdays or month days chosen, but none of them.
    NoDays,
    /// A month day outside 1–31, or a weekday outside Monday–Sunday.
    DayRange,
    /// A month outside 1–12.
    Months,
    /// The days and months chosen never happen (the 31st in April only).
    Never,
}

/// How daylight saving moved one due time from the rule's local time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shift {
    None,
    /// The local time fell in a gap: it runs this many minutes later.
    Gap {
        minutes: i64,
    },
    /// The local time happens twice: it runs once, at the earlier.
    Fold,
}

/// One due time, with what daylight saving did to it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Occurrence {
    pub at: UtcMillis,
    pub shift: Shift,
}

/// The due times in a span: how many, and the most recent ones, oldest
/// first.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DueTimes {
    pub count: u64,
    pub latest: Vec<UtcMillis>,
}

/// A due time as a clock in its zone shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Local {
    pub year: i16,
    pub month: i8,
    pub day: i8,
    /// ISO: 1 is Monday.
    pub weekday: i8,
    pub hour: i8,
    pub minute: i8,
    /// `CET`, `-03`, …
    pub abbreviation: String,
    /// Seconds east of UTC.
    pub offset_seconds: i32,
}

/// The zone of an IANA name, from the system's database (Linux, macOS) or
/// the one bundled with the application (Windows). `None` when unknown.
pub fn time_zone(name: &str) -> Option<TimeZone> {
    let name = name.trim();
    if name.is_empty() {
        return None;
    }
    TimeZone::get(name).ok()
}

/// The system's zone name; `UTC` and `false` when the system reports none
/// the database knows.
pub fn system_zone() -> (String, bool) {
    match TimeZone::try_system()
        .ok()
        .and_then(|zone| zone.iana_name().map(str::to_owned))
    {
        Some(name) if time_zone(&name).is_some() => (name, true),
        _ => ("UTC".to_owned(), false),
    }
}

/// `at` in `zone`.
pub fn local(at: UtcMillis, zone: &TimeZone) -> Option<Local> {
    let zoned = Timestamp::from_millisecond(at.0)
        .ok()?
        .to_zoned(zone.clone());
    let info = zone.to_offset_info(zoned.timestamp());
    Some(Local {
        year: zoned.year(),
        month: zoned.month(),
        day: zoned.day(),
        weekday: zoned.weekday().to_monday_one_offset(),
        hour: zoned.hour(),
        minute: zoned.minute(),
        abbreviation: info.abbreviation().to_owned(),
        offset_seconds: info.offset().seconds(),
    })
}

/// The next whole minute after `now`: the default anchor of an interval.
pub fn next_minute(now: UtcMillis) -> UtcMillis {
    UtcMillis((now.0.div_euclid(MINUTE_MS) + 1) * MINUTE_MS)
}

const WEEKDAYS: [&str; 7] = ["mon", "tue", "wed", "thu", "fri", "sat", "sun"];

impl Rule {
    /// Reads a stored rule and normalises it (times, days and months sorted
    /// without repeats). It may still have [`Rule::problems`].
    pub fn from_json(text: &str) -> Result<Rule, RuleProblem> {
        let value: Value = serde_json::from_str(text).map_err(|_| RuleProblem::Malformed)?;
        if value.get("schema_version").and_then(Value::as_str) != Some(RULE_SCHEMA_VERSION) {
            return Err(RuleProblem::Malformed);
        }
        match value.get("kind").and_then(Value::as_str) {
            Some("interval") => {
                let every = value
                    .get("every_minutes")
                    .and_then(Value::as_u64)
                    .and_then(|every| u32::try_from(every).ok())
                    .ok_or(RuleProblem::Interval)?;
                let anchor = value
                    .get("anchor")
                    .and_then(Value::as_str)
                    .and_then(|text| text.parse::<Timestamp>().ok())
                    .ok_or(RuleProblem::Malformed)?;
                Ok(Rule::Interval {
                    every_minutes: every,
                    anchor: UtcMillis(anchor.as_millisecond()),
                })
            }
            Some("calendar") => {
                let mut at = Vec::new();
                for time in value
                    .get("at")
                    .and_then(Value::as_array)
                    .ok_or(RuleProblem::Times)?
                {
                    at.push(
                        time.as_str()
                            .and_then(LocalTime::parse)
                            .ok_or(RuleProblem::Times)?,
                    );
                }
                let days = match value.get("days") {
                    Some(Value::String(text)) if text == "every_day" => Days::EveryDay,
                    Some(Value::Object(object)) => {
                        if let Some(weekdays) = object.get("weekdays").and_then(Value::as_array) {
                            let mut days = Vec::new();
                            for day in weekdays {
                                let index = day
                                    .as_str()
                                    .and_then(|day| WEEKDAYS.iter().position(|name| *name == day))
                                    .ok_or(RuleProblem::DayRange)?;
                                days.push(
                                    u8::try_from(index + 1).map_err(|_| RuleProblem::DayRange)?,
                                );
                            }
                            Days::Weekdays(days)
                        } else if let Some(month_days) =
                            object.get("month_days").and_then(Value::as_array)
                        {
                            let mut days = Vec::new();
                            for day in month_days {
                                days.push(match day {
                                    Value::String(text) if text == "last" => MonthDay::Last,
                                    other => MonthDay::Day(
                                        other
                                            .as_u64()
                                            .and_then(|day| u8::try_from(day).ok())
                                            .ok_or(RuleProblem::DayRange)?,
                                    ),
                                });
                            }
                            Days::MonthDays(days)
                        } else {
                            return Err(RuleProblem::Malformed);
                        }
                    }
                    _ => return Err(RuleProblem::Malformed),
                };
                let mut months = Vec::new();
                for month in value
                    .get("months")
                    .and_then(Value::as_array)
                    .ok_or(RuleProblem::Months)?
                {
                    months.push(
                        month
                            .as_u64()
                            .and_then(|month| u8::try_from(month).ok())
                            .ok_or(RuleProblem::Months)?,
                    );
                }
                Ok(Rule::Calendar { at, days, months }.normalised())
            }
            _ => Err(RuleProblem::Malformed),
        }
    }

    /// Sorted and without repeats, as stored.
    pub fn normalised(self) -> Rule {
        match self {
            Rule::Calendar {
                mut at,
                days,
                mut months,
            } => {
                at.sort_unstable();
                at.dedup();
                months.sort_unstable();
                months.dedup();
                let days = match days {
                    Days::EveryDay => Days::EveryDay,
                    Days::Weekdays(mut days) => {
                        days.sort_unstable();
                        days.dedup();
                        Days::Weekdays(days)
                    }
                    Days::MonthDays(mut days) => {
                        days.sort_unstable();
                        days.dedup();
                        Days::MonthDays(days)
                    }
                };
                Rule::Calendar { at, days, months }
            }
            interval => interval,
        }
    }

    /// The canonical JSON stored in `schedules.rule_json`.
    pub fn to_json(&self) -> String {
        let value = match self.clone().normalised() {
            Rule::Interval {
                every_minutes,
                anchor,
            } => json!({
                "schema_version": RULE_SCHEMA_VERSION,
                "kind": "interval",
                "every_minutes": every_minutes,
                "anchor": Timestamp::from_millisecond(anchor.0)
                    .map(|anchor| anchor.to_string())
                    .unwrap_or_default(),
            }),
            Rule::Calendar { at, days, months } => json!({
                "schema_version": RULE_SCHEMA_VERSION,
                "kind": "calendar",
                "at": at.iter().map(|time| time.text()).collect::<Vec<_>>(),
                "days": match days {
                    Days::EveryDay => json!("every_day"),
                    Days::Weekdays(days) => json!({
                        "weekdays": days
                            .iter()
                            .filter_map(|day| WEEKDAYS.get(usize::from(*day).wrapping_sub(1)))
                            .collect::<Vec<_>>(),
                    }),
                    Days::MonthDays(days) => json!({
                        "month_days": days
                            .iter()
                            .map(|day| match day {
                                MonthDay::Day(day) => json!(day),
                                MonthDay::Last => json!("last"),
                            })
                            .collect::<Vec<_>>(),
                    }),
                },
                "months": months,
            }),
        };
        canonical_json(&value)
    }

    /// What keeps the rule from being saved; empty when it can be.
    pub fn problems(&self) -> Vec<RuleProblem> {
        let mut problems = Vec::new();
        match self {
            Rule::Interval { every_minutes, .. } => {
                if !(MIN_INTERVAL_MINUTES..=MAX_INTERVAL_MINUTES).contains(every_minutes) {
                    problems.push(RuleProblem::Interval);
                }
            }
            Rule::Calendar { at, days, months } => {
                if at.is_empty() {
                    problems.push(RuleProblem::Times);
                }
                if at.len() > MAX_TIMES_A_DAY {
                    problems.push(RuleProblem::TooManyTimes);
                }
                match days {
                    Days::EveryDay => {}
                    Days::Weekdays(days) => {
                        if days.is_empty() {
                            problems.push(RuleProblem::NoDays);
                        } else if days.iter().any(|day| !(1..=7).contains(day)) {
                            problems.push(RuleProblem::DayRange);
                        }
                    }
                    Days::MonthDays(days) => {
                        if days.is_empty() {
                            problems.push(RuleProblem::NoDays);
                        } else if days
                            .iter()
                            .any(|day| matches!(day, MonthDay::Day(day) if !(1..=31).contains(day)))
                        {
                            problems.push(RuleProblem::DayRange);
                        }
                    }
                }
                if months.iter().any(|month| !(1..=12).contains(month)) {
                    problems.push(RuleProblem::Months);
                }
                if problems.is_empty() && !self.can_happen() {
                    problems.push(RuleProblem::Never);
                }
            }
        }
        problems
    }

    /// Whether the chosen month days exist in the chosen months.
    fn can_happen(&self) -> bool {
        let Rule::Calendar { days, months, .. } = self else {
            return true;
        };
        let Days::MonthDays(days) = days else {
            return true;
        };
        // The longest each month can be (February in a leap year).
        const LONGEST: [u8; 12] = [31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
        let months: Vec<u8> = if months.is_empty() {
            (1..=12).collect()
        } else {
            months.clone()
        };
        days.iter().any(|day| match day {
            MonthDay::Last => true,
            MonthDay::Day(day) => months.iter().any(|month| {
                LONGEST
                    .get(usize::from(*month).wrapping_sub(1))
                    .is_some_and(|longest| day <= longest)
            }),
        })
    }

    /// The first due time strictly after `after`; `None` when there is none
    /// within four years.
    pub fn next_after(&self, zone: &TimeZone, after: UtcMillis) -> Option<UtcMillis> {
        self.next_times(zone, after, 1)
            .first()
            .map(|occurrence| occurrence.at)
    }

    /// The first `count` due times strictly after `after`, with what
    /// daylight saving did to each.
    pub fn next_times(&self, zone: &TimeZone, after: UtcMillis, count: usize) -> Vec<Occurrence> {
        let mut found = Vec::new();
        match self {
            Rule::Interval {
                every_minutes,
                anchor,
            } => {
                let period = i64::from((*every_minutes).max(1)) * MINUTE_MS;
                let first = if after.0 < anchor.0 {
                    0
                } else {
                    (after.0 - anchor.0).div_euclid(period) + 1
                };
                for k in first..first.saturating_add(count as i64) {
                    found.push(Occurrence {
                        at: UtcMillis(anchor.0.saturating_add(k.saturating_mul(period))),
                        shift: Shift::None,
                    });
                }
            }
            Rule::Calendar { .. } => {
                let Some(mut date) = local_date(after, zone) else {
                    return found;
                };
                // The day before, in case an offset change moves a time of
                // that local day past `after`.
                date = date.yesterday().unwrap_or(date);
                for _ in 0..LOOK_AHEAD_DAYS {
                    for occurrence in self.on_day(zone, date) {
                        if occurrence.at > after && found.len() < count {
                            found.push(occurrence);
                        }
                    }
                    if found.len() >= count {
                        break;
                    }
                    let Ok(next) = date.tomorrow() else {
                        break;
                    };
                    date = next;
                }
            }
        }
        found
    }

    /// The due times in `(after, upto]`: how many, and the `keep` most
    /// recent, oldest first.
    pub fn due_in(
        &self,
        zone: &TimeZone,
        after: UtcMillis,
        upto: UtcMillis,
        keep: usize,
    ) -> DueTimes {
        let mut due = DueTimes::default();
        if upto <= after {
            return due;
        }
        match self {
            Rule::Interval {
                every_minutes,
                anchor,
            } => {
                if upto.0 < anchor.0 {
                    return due;
                }
                let period = i64::from((*every_minutes).max(1)) * MINUTE_MS;
                let first = if after.0 < anchor.0 {
                    0
                } else {
                    (after.0 - anchor.0).div_euclid(period) + 1
                };
                let last = (upto.0 - anchor.0).div_euclid(period);
                if last < first {
                    return due;
                }
                due.count = u64::try_from(last - first + 1).unwrap_or(u64::MAX);
                let from = last.saturating_sub(keep as i64 - 1).max(first);
                if keep > 0 {
                    due.latest = (from..=last)
                        .map(|k| UtcMillis(anchor.0.saturating_add(k.saturating_mul(period))))
                        .collect();
                }
            }
            Rule::Calendar { .. } => {
                let (Some(start), Some(end)) = (local_date(after, zone), local_date(upto, zone))
                else {
                    return due;
                };
                let earliest = jiff::Span::new()
                    .try_days(MAX_SCAN_DAYS)
                    .and_then(|span| end.checked_sub(span))
                    .unwrap_or(start);
                let mut date = start.max(earliest).yesterday().unwrap_or(start);
                let last = end.tomorrow().unwrap_or(end);
                let mut recent = std::collections::VecDeque::with_capacity(keep);
                while date <= last {
                    for occurrence in self.on_day(zone, date) {
                        if occurrence.at > after && occurrence.at <= upto {
                            due.count += 1;
                            if keep > 0 {
                                if recent.len() == keep {
                                    recent.pop_front();
                                }
                                recent.push_back(occurrence.at);
                            }
                        }
                    }
                    let Ok(next) = date.tomorrow() else {
                        break;
                    };
                    date = next;
                }
                due.latest = recent.into_iter().collect();
            }
        }
        due
    }

    /// A calendar rule's due times on one local date, in order, each once.
    fn on_day(&self, zone: &TimeZone, date: Date) -> Vec<Occurrence> {
        let Rule::Calendar { at, days, months } = self else {
            return Vec::new();
        };
        let month = u8::try_from(date.month()).unwrap_or(0);
        if !months.is_empty() && !months.contains(&month) {
            return Vec::new();
        }
        let runs_today = match days {
            Days::EveryDay => true,
            Days::Weekdays(days) => {
                let weekday = u8::try_from(date.weekday().to_monday_one_offset()).unwrap_or(0);
                days.contains(&weekday)
            }
            Days::MonthDays(days) => days.iter().any(|day| match day {
                MonthDay::Last => date == date.last_of_month(),
                MonthDay::Day(day) => i8::try_from(*day).is_ok_and(|day| date.day() == day),
            }),
        };
        if !runs_today {
            return Vec::new();
        }
        let mut found: Vec<Occurrence> = at
            .iter()
            .filter_map(|time| {
                let time = Time::new(
                    i8::try_from(time.hour).ok()?,
                    i8::try_from(time.minute).ok()?,
                    0,
                    0,
                )
                .ok()?;
                let ambiguous = zone.to_ambiguous_zoned(date.to_datetime(time));
                let shift = match ambiguous.offset() {
                    AmbiguousOffset::Unambiguous { .. } => Shift::None,
                    AmbiguousOffset::Gap { before, after } => Shift::Gap {
                        minutes: i64::from(after.seconds() - before.seconds()) / 60,
                    },
                    AmbiguousOffset::Fold { .. } => Shift::Fold,
                };
                let zoned = ambiguous.compatible().ok()?;
                Some(Occurrence {
                    at: UtcMillis(zoned.timestamp().as_millisecond()),
                    shift,
                })
            })
            .collect();
        // A time moved by a gap may land on another time of the rule.
        found.sort_by_key(|occurrence| occurrence.at);
        found.dedup_by_key(|occurrence| occurrence.at);
        found
    }
}

fn local_date(at: UtcMillis, zone: &TimeZone) -> Option<Date> {
    Some(
        Timestamp::from_millisecond(at.0)
            .ok()?
            .to_zoned(zone.clone())
            .date(),
    )
}

/// `minutes` as a span of time.
pub fn minutes(minutes: u32) -> Duration {
    Duration::from_secs(u64::from(minutes) * 60)
}

#[cfg(test)]
#[path = "rule_tests.rs"]
mod tests;
