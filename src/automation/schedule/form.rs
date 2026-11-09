//! The schedule editor's form as text and choices, turned into a [`Rule`]
//! and back (specification, section 9.5.2: the form is the grammar).
//! GPUI-free, so the view only holds the fields.
use super::rule::{Days, LocalTime, MonthDay, Rule, RuleProblem, next_minute};
use crate::automation::UtcMillis;

/// Which days a calendar rule runs on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DaysChoice {
    EveryDay,
    Weekdays,
    MonthDays,
}

/// What the form's fields hold.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuleForm {
    /// An interval rather than a calendar.
    pub interval: bool,
    /// Minutes, as typed.
    pub every_minutes: String,
    /// The interval's first time.
    pub anchor: UtcMillis,
    /// Local times, as typed: `09:00, 17:30`.
    pub times: String,
    pub days: DaysChoice,
    /// Monday first.
    pub weekdays: [bool; 7],
    /// As typed: `1, 15, last`.
    pub month_days: String,
    /// January first; none chosen means every month.
    pub months: [bool; 12],
}

impl RuleForm {
    /// A new schedule's form: every day at 09:00; an interval would start at
    /// the next whole minute.
    pub fn new(now: UtcMillis) -> Self {
        Self {
            interval: false,
            every_minutes: "60".into(),
            anchor: next_minute(now),
            times: "09:00".into(),
            days: DaysChoice::EveryDay,
            weekdays: [true, true, true, true, true, false, false],
            month_days: "1".into(),
            months: [false; 12],
        }
    }

    /// The form of a stored rule.
    pub fn of(rule: &Rule, now: UtcMillis) -> Self {
        let mut form = Self::new(now);
        match rule {
            Rule::Interval {
                every_minutes,
                anchor,
            } => {
                form.interval = true;
                form.every_minutes = every_minutes.to_string();
                form.anchor = *anchor;
            }
            Rule::Calendar { at, days, months } => {
                form.times = at
                    .iter()
                    .map(|time| time.text())
                    .collect::<Vec<_>>()
                    .join(", ");
                match days {
                    Days::EveryDay => form.days = DaysChoice::EveryDay,
                    Days::Weekdays(days) => {
                        form.days = DaysChoice::Weekdays;
                        form.weekdays = [false; 7];
                        for day in days {
                            if let Some(slot) =
                                form.weekdays.get_mut(usize::from(*day).wrapping_sub(1))
                            {
                                *slot = true;
                            }
                        }
                    }
                    Days::MonthDays(days) => {
                        form.days = DaysChoice::MonthDays;
                        form.month_days = days
                            .iter()
                            .map(|day| match day {
                                MonthDay::Day(day) => day.to_string(),
                                MonthDay::Last => "last".to_owned(),
                            })
                            .collect::<Vec<_>>()
                            .join(", ");
                    }
                }
                for month in months {
                    if let Some(slot) = form.months.get_mut(usize::from(*month).wrapping_sub(1)) {
                        *slot = true;
                    }
                }
            }
        }
        form
    }

    /// The rule the form describes, or what to fix.
    pub fn rule(&self) -> Result<Rule, Vec<RuleProblem>> {
        let rule = if self.interval {
            let every = self
                .every_minutes
                .trim()
                .parse::<u32>()
                .map_err(|_| vec![RuleProblem::Interval])?;
            Rule::Interval {
                every_minutes: every,
                anchor: self.anchor,
            }
        } else {
            let at = parse_times(&self.times).ok_or_else(|| vec![RuleProblem::Times])?;
            let days = match self.days {
                DaysChoice::EveryDay => Days::EveryDay,
                DaysChoice::Weekdays => Days::Weekdays(
                    (1..=7u8)
                        .filter(|day| self.weekdays.get(usize::from(*day - 1)) == Some(&true))
                        .collect(),
                ),
                DaysChoice::MonthDays => Days::MonthDays(
                    parse_month_days(&self.month_days)
                        .ok_or_else(|| vec![RuleProblem::DayRange])?,
                ),
            };
            let months = (1..=12u8)
                .filter(|month| self.months.get(usize::from(*month - 1)) == Some(&true))
                .collect();
            Rule::Calendar { at, days, months }.normalised()
        };
        let problems = rule.problems();
        if problems.is_empty() {
            Ok(rule)
        } else {
            Err(problems)
        }
    }
}

fn items(text: &str) -> impl Iterator<Item = &str> {
    text.split(|c: char| c == ',' || c == ';' || c.is_whitespace())
        .filter(|item| !item.is_empty())
}

/// `09:00, 17:30`; `None` when one is not a time.
pub fn parse_times(text: &str) -> Option<Vec<LocalTime>> {
    items(text).map(LocalTime::parse).collect()
}

/// `1, 15, last`; `None` when one is neither a number nor `last`.
pub fn parse_month_days(text: &str) -> Option<Vec<MonthDay>> {
    items(text)
        .map(|item| {
            if item.eq_ignore_ascii_case("last") {
                Some(MonthDay::Last)
            } else {
                item.parse::<u8>().ok().map(MonthDay::Day)
            }
        })
        .collect()
}

/// N as typed for *queue up to N*.
pub fn parse_limit(text: &str) -> Option<i64> {
    text.trim()
        .parse::<i64>()
        .ok()
        .filter(|limit| jykre_core::automation::MISSED_LIMIT_RANGE.contains(limit))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_form_reads_and_writes_each_kind_of_rule() {
        let now = UtcMillis(1_800_000_000_000);
        let mut form = RuleForm::new(now);
        assert_eq!(
            form.rule(),
            Ok(Rule::Calendar {
                at: vec![LocalTime::parse("09:00").unwrap()],
                days: Days::EveryDay,
                months: vec![],
            })
        );
        form.times = "17:30; 9:05 17:30".into();
        form.days = DaysChoice::Weekdays;
        form.months[11] = true;
        let rule = form.rule().unwrap();
        assert_eq!(
            rule,
            Rule::Calendar {
                at: vec![
                    LocalTime::parse("09:05").unwrap(),
                    LocalTime::parse("17:30").unwrap()
                ],
                days: Days::Weekdays(vec![1, 2, 3, 4, 5]),
                months: vec![12],
            }
        );
        assert_eq!(RuleForm::of(&rule, now).rule(), Ok(rule));
        form.days = DaysChoice::MonthDays;
        form.month_days = "15, LAST, 1".into();
        let rule = form.rule().unwrap();
        assert_eq!(RuleForm::of(&rule, now).rule(), Ok(rule));
        form.interval = true;
        form.every_minutes = " 15 ".into();
        let rule = form.rule().unwrap();
        assert_eq!(
            rule,
            Rule::Interval {
                every_minutes: 15,
                anchor: next_minute(now)
            }
        );
        assert_eq!(RuleForm::of(&rule, now).rule(), Ok(rule));
    }

    #[test]
    fn the_form_names_what_to_fix() {
        let mut form = RuleForm::new(UtcMillis(0));
        form.times = "9h".into();
        assert_eq!(form.rule(), Err(vec![RuleProblem::Times]));
        form.times = "09:00".into();
        form.days = DaysChoice::Weekdays;
        form.weekdays = [false; 7];
        assert_eq!(form.rule(), Err(vec![RuleProblem::NoDays]));
        form.days = DaysChoice::MonthDays;
        form.month_days = "first".into();
        assert_eq!(form.rule(), Err(vec![RuleProblem::DayRange]));
        form.interval = true;
        form.every_minutes = "0".into();
        assert_eq!(form.rule(), Err(vec![RuleProblem::Interval]));
        form.every_minutes = "often".into();
        assert_eq!(form.rule(), Err(vec![RuleProblem::Interval]));
        assert_eq!(parse_limit("3"), Some(3));
        for text in ["1", "25", "x", ""] {
            assert_eq!(parse_limit(text), None, "{text}");
        }
    }
}
