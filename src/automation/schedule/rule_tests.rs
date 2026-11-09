//! Rules against real zones from `jiff`'s database (specification, section
//! 9.5.10, "Rules").
use super::*;

fn utc(text: &str) -> UtcMillis {
    UtcMillis(text.parse::<Timestamp>().unwrap().as_millisecond())
}

fn zone(name: &str) -> TimeZone {
    time_zone(name).unwrap()
}

fn at(text: &str) -> LocalTime {
    LocalTime::parse(text).unwrap()
}

fn daily(times: &[&str]) -> Rule {
    Rule::Calendar {
        at: times.iter().map(|time| at(time)).collect(),
        days: Days::EveryDay,
        months: Vec::new(),
    }
}

fn times(occurrences: &[Occurrence]) -> Vec<UtcMillis> {
    occurrences.iter().map(|occurrence| occurrence.at).collect()
}

#[test]
fn an_interval_stays_n_apart_in_utc_across_a_dst_change() {
    let berlin = zone("Europe/Berlin");
    let rule = Rule::Interval {
        every_minutes: 60,
        anchor: utc("2026-03-28T23:00:00Z"),
    };
    let next = rule.next_times(&berlin, utc("2026-03-28T23:30:00Z"), 4);
    assert_eq!(
        times(&next),
        vec![
            utc("2026-03-29T00:00:00Z"),
            utc("2026-03-29T01:00:00Z"),
            utc("2026-03-29T02:00:00Z"),
            utc("2026-03-29T03:00:00Z"),
        ]
    );
    // The local clock jumps from 02:00 to 03:00: the hours shift, the
    // spacing does not.
    let hours: Vec<i8> = next
        .iter()
        .map(|occurrence| local(occurrence.at, &berlin).unwrap().hour)
        .collect();
    assert_eq!(hours, vec![1, 3, 4, 5]);
    assert!(
        next.iter()
            .all(|occurrence| occurrence.shift == Shift::None)
    );
    // Before its anchor, the anchor is the next time.
    assert_eq!(
        rule.next_after(&berlin, utc("2026-01-01T00:00:00Z")),
        Some(utc("2026-03-28T23:00:00Z"))
    );
}

#[test]
fn berlin_spring_gap_runs_once_shifted_and_autumn_fold_once_at_the_earlier() {
    let berlin = zone("Europe/Berlin");
    let rule = daily(&["02:30"]);
    let spring = rule.next_times(&berlin, utc("2026-03-28T00:00:00Z"), 3);
    assert_eq!(
        times(&spring),
        vec![
            utc("2026-03-28T01:30:00Z"), // 02:30 CET
            utc("2026-03-29T01:30:00Z"), // 02:30 does not exist: 03:30 CEST
            utc("2026-03-30T00:30:00Z"), // 02:30 CEST
        ]
    );
    assert_eq!(spring[1].shift, Shift::Gap { minutes: 60 });
    assert_eq!(local(spring[1].at, &berlin).unwrap().hour, 3);
    let autumn = rule.next_times(&berlin, utc("2026-10-24T00:00:00Z"), 3);
    assert_eq!(
        times(&autumn),
        vec![
            utc("2026-10-24T00:30:00Z"), // 02:30 CEST
            utc("2026-10-25T00:30:00Z"), // the first 02:30, CEST, once
            utc("2026-10-26T01:30:00Z"), // 02:30 CET
        ]
    );
    assert_eq!(autumn[1].shift, Shift::Fold);
    let fold = local(autumn[1].at, &berlin).unwrap();
    assert_eq!((fold.hour, fold.minute, fold.offset_seconds), (2, 30, 7200));
    assert_eq!(fold.abbreviation, "CEST");
    // The fold's day has one due time in all.
    let due = rule.due_in(
        &berlin,
        utc("2026-10-24T22:00:00Z"),
        utc("2026-10-25T22:00:00Z"),
        4,
    );
    assert_eq!(due.count, 1);
}

#[test]
fn new_york_both_changes() {
    let new_york = zone("America/New_York");
    let spring = daily(&["02:30"]).next_times(&new_york, utc("2026-03-07T00:00:00Z"), 3);
    assert_eq!(
        times(&spring),
        vec![
            utc("2026-03-07T07:30:00Z"),
            utc("2026-03-08T07:30:00Z"), // 03:30 EDT
            utc("2026-03-09T06:30:00Z"),
        ]
    );
    assert_eq!(spring[1].shift, Shift::Gap { minutes: 60 });
    let autumn = daily(&["01:30"]).next_times(&new_york, utc("2026-10-31T00:00:00Z"), 3);
    assert_eq!(
        times(&autumn),
        vec![
            utc("2026-10-31T05:30:00Z"),
            utc("2026-11-01T05:30:00Z"), // 01:30 EDT, the earlier
            utc("2026-11-02T06:30:00Z"),
        ]
    );
    assert_eq!(autumn[1].shift, Shift::Fold);
}

#[test]
fn lord_howe_moves_by_thirty_minutes() {
    let lord_howe = zone("Australia/Lord_Howe");
    let rule = daily(&["02:15"]);
    let next = rule.next_times(&lord_howe, utc("2026-10-03T00:00:00Z"), 1);
    // 02:00 jumps to 02:30 on 4 October 2026: 02:15 runs at 02:45 (+11:00).
    assert_eq!(times(&next), vec![utc("2026-10-03T15:45:00Z")]);
    assert_eq!(next[0].shift, Shift::Gap { minutes: 30 });
    let shown = local(next[0].at, &lord_howe).unwrap();
    assert_eq!(
        (shown.hour, shown.minute, shown.offset_seconds),
        (2, 45, 39_600)
    );
}

#[test]
fn month_days_skip_short_months_and_last_is_each_months_end() {
    let utc_zone = zone("UTC");
    let thirty_first = Rule::Calendar {
        at: vec![at("09:00")],
        days: Days::MonthDays(vec![MonthDay::Day(31)]),
        months: Vec::new(),
    };
    // April has 30 days: no run that month, and none moved to the 30th.
    assert_eq!(
        thirty_first.next_after(&utc_zone, utc("2026-04-01T00:00:00Z")),
        Some(utc("2026-05-31T09:00:00Z"))
    );
    let last = Rule::Calendar {
        at: vec![at("23:00")],
        days: Days::MonthDays(vec![MonthDay::Last]),
        months: Vec::new(),
    };
    assert_eq!(
        times(&last.next_times(&utc_zone, utc("2026-02-01T00:00:00Z"), 3)),
        vec![
            utc("2026-02-28T23:00:00Z"),
            utc("2026-03-31T23:00:00Z"),
            utc("2026-04-30T23:00:00Z"),
        ]
    );
    let in_months = Rule::Calendar {
        at: vec![at("09:00")],
        days: Days::MonthDays(vec![MonthDay::Day(1)]),
        months: vec![1, 7],
    };
    assert_eq!(
        in_months.next_after(&utc_zone, utc("2026-02-01T00:00:00Z")),
        Some(utc("2026-07-01T09:00:00Z"))
    );
    let weekdays = Rule::Calendar {
        at: vec![at("17:30"), at("09:00")],
        days: Days::Weekdays(vec![1, 2, 3, 4, 5]),
        months: Vec::new(),
    }
    .normalised();
    // 2026-11-06 is a Friday: then Monday.
    assert_eq!(
        times(&weekdays.next_times(&utc_zone, utc("2026-11-06T12:00:00Z"), 3)),
        vec![
            utc("2026-11-06T17:30:00Z"),
            utc("2026-11-09T09:00:00Z"),
            utc("2026-11-09T17:30:00Z"),
        ]
    );
}

#[test]
fn due_times_in_a_span_are_counted_and_the_latest_kept() {
    let utc_zone = zone("UTC");
    let anchor = utc("2026-01-01T00:00:00Z");
    let every_quarter = Rule::Interval {
        every_minutes: 15,
        anchor,
    };
    let due = every_quarter.due_in(&utc_zone, anchor, utc("2026-01-01T01:00:00Z"), 2);
    assert_eq!(due.count, 4, "the end is included, the start is not");
    assert_eq!(
        due.latest,
        vec![utc("2026-01-01T00:45:00Z"), utc("2026-01-01T01:00:00Z")]
    );
    assert_eq!(
        every_quarter
            .due_in(&utc_zone, anchor, anchor.plus(minutes(14)), 3)
            .count,
        0
    );
    let nine = daily(&["09:00"]);
    let due = nine.due_in(
        &utc_zone,
        utc("2026-01-01T09:00:00Z"),
        utc("2026-01-11T09:00:00Z"),
        3,
    );
    assert_eq!(due.count, 10);
    assert_eq!(
        due.latest,
        vec![
            utc("2026-01-09T09:00:00Z"),
            utc("2026-01-10T09:00:00Z"),
            utc("2026-01-11T09:00:00Z"),
        ]
    );
    // A span longer than the scan counts at most its last ten years.
    let long = nine.due_in(
        &utc_zone,
        utc("1990-01-01T00:00:00Z"),
        utc("2026-01-01T10:00:00Z"),
        1,
    );
    assert!(long.count <= (MAX_SCAN_DAYS + 2) as u64);
    assert_eq!(long.latest, vec![utc("2026-01-01T09:00:00Z")]);
}

#[test]
fn rules_round_trip_and_name_their_problems() {
    let rules = [
        Rule::Interval {
            every_minutes: 15,
            anchor: utc("2026-11-09T09:00:00Z"),
        },
        Rule::Calendar {
            at: vec![at("09:00"), at("17:30")],
            days: Days::Weekdays(vec![1, 2, 3, 4, 5]),
            months: Vec::new(),
        },
        Rule::Calendar {
            at: vec![at("06:05")],
            days: Days::MonthDays(vec![MonthDay::Day(1), MonthDay::Day(15), MonthDay::Last]),
            months: vec![3, 6],
        },
    ];
    for rule in rules {
        let json = rule.to_json();
        assert_eq!(Rule::from_json(&json), Ok(rule.clone()), "{json}");
        assert!(rule.problems().is_empty());
    }
    assert_eq!(
        Rule::Interval {
            every_minutes: 15,
            anchor: utc("2026-11-09T09:00:00Z"),
        }
        .to_json(),
        r#"{"anchor":"2026-11-09T09:00:00Z","every_minutes":15,"kind":"interval","schema_version":"1.0"}"#
    );
    assert_eq!(
        daily(&["17:30", "09:00", "09:00"]).normalised().to_json(),
        r#"{"at":["09:00","17:30"],"days":"every_day","kind":"calendar","months":[],"schema_version":"1.0"}"#
    );
    let interval = |every_minutes| Rule::Interval {
        every_minutes,
        anchor: UtcMillis(0),
    };
    assert_eq!(interval(0).problems(), vec![RuleProblem::Interval]);
    assert_eq!(
        interval(24 * 60 + 1).problems(),
        vec![RuleProblem::Interval]
    );
    assert!(interval(24 * 60).problems().is_empty());
    assert_eq!(daily(&[]).problems(), vec![RuleProblem::Times]);
    let many: Vec<String> = (0..25).map(|minute| format!("00:{minute:02}")).collect();
    let many: Vec<&str> = many.iter().map(String::as_str).collect();
    assert_eq!(daily(&many).problems(), vec![RuleProblem::TooManyTimes]);
    let calendar = |days, months| Rule::Calendar {
        at: vec![at("09:00")],
        days,
        months,
    };
    assert_eq!(
        calendar(Days::Weekdays(vec![]), vec![]).problems(),
        vec![RuleProblem::NoDays]
    );
    assert_eq!(
        calendar(Days::MonthDays(vec![MonthDay::Day(32)]), vec![]).problems(),
        vec![RuleProblem::DayRange]
    );
    assert_eq!(
        calendar(Days::EveryDay, vec![13]).problems(),
        vec![RuleProblem::Months]
    );
    assert_eq!(
        calendar(Days::MonthDays(vec![MonthDay::Day(31)]), vec![4, 6]).problems(),
        vec![RuleProblem::Never]
    );
    assert!(
        calendar(Days::MonthDays(vec![MonthDay::Day(29)]), vec![2])
            .problems()
            .is_empty(),
        "the 29th of February happens in leap years"
    );
    for text in [
        "",
        "{}",
        "[]",
        r#"{"schema_version":"2.0","kind":"interval"}"#,
    ] {
        assert_eq!(Rule::from_json(text), Err(RuleProblem::Malformed), "{text}");
    }
    assert_eq!(LocalTime::parse("9:05"), Some(at("09:05")));
    for text in ["24:00", "09:60", "0900", "09:5", "x:00"] {
        assert_eq!(LocalTime::parse(text), None, "{text}");
    }
}

#[test]
fn unknown_zones_are_refused_and_the_system_zone_is_named() {
    assert!(time_zone("Mars/Olympus_Mons").is_none());
    assert!(time_zone("").is_none());
    assert!(time_zone("UTC").is_some());
    assert!(time_zone("Etc/GMT+3").is_some());
    let (name, _) = system_zone();
    assert!(time_zone(&name).is_some(), "{name}");
    assert_eq!(
        next_minute(utc("2026-11-08T10:15:30Z")),
        utc("2026-11-08T10:16:00Z")
    );
    assert_eq!(
        next_minute(utc("2026-11-08T10:15:00Z")),
        utc("2026-11-08T10:16:00Z")
    );
}
