//! The budgets a performance run checks, as
//! [Performance](../../docs/standards/performance.md#budgets) and the
//! automation specification (section 8.2) set them, and how a measured
//! value is judged against one. Nothing here changes a budget: each one
//! names the row it comes from.
use serde_json::{Value, json};

/// How a metric of the run came out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Pass,
    Fail,
    /// The run could not measure it; the metric says why.
    NotMeasured,
    /// Measured, with no budget to hold it to.
    Info,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Pass => "pass",
            Status::Fail => "fail",
            Status::NotMeasured => "not measured",
            Status::Info => "info",
        }
    }
}

/// An upper bound: a value passes when it is at most `limit`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Budget {
    pub limit: f64,
    /// As the report writes it.
    pub text: &'static str,
    /// Where it comes from.
    pub source: &'static str,
}

/// The result grid's frame budget.
pub const GRID_FRAME_P95: Budget = Budget {
    limit: 8.0,
    text: "≤ 8 ms p95",
    source: "Performance: frame while scrolling, hovering or selecting in the result grid",
};

/// Any other interaction's frame budget, which A9 and A15 must stay within.
pub const OTHER_FRAME_P95: Budget = Budget {
    limit: 8.0,
    text: "≤ 8 ms p95",
    source: "Performance: frame for any other interaction in the main window",
};

/// A9 and A15 must stay within the frame budget: their UI-thread work per
/// keystroke or jump is held to it.
pub const WITHIN_FRAME: Budget = Budget {
    limit: 8.0,
    text: "≤ 8 ms p95",
    source: "Within the frame budget (0.3.0 A9, A15)",
};

pub const STALL: Budget = Budget {
    limit: 16.0,
    text: "≤ 16 ms",
    source: "Performance: longest UI-thread stall while a query runs",
};

pub const AUTOMATION_STALL: Budget = Budget {
    limit: 16.0,
    text: "≤ 16 ms",
    source: "Automation specification 8.2: no stall over 16 ms",
};

pub const AUTOMATION_FRAME_P95: Budget = Budget {
    limit: 8.0,
    text: "≤ 8 ms p95",
    source: "Automation specification 8.2: frame time at most 8 ms at p95",
};

pub const STATE_PUBLICATION: Budget = Budget {
    limit: 2.0,
    text: "≤ 2 ms",
    source: "Automation specification 8.2: state publication at most 2 ms",
};

pub const PUBLISH_RESULT: Budget = Budget {
    limit: 2.0,
    text: "≤ 2 ms",
    source: "Performance: UI-thread work to publish a finished result",
};

pub const FIRST_FRAME_AFTER_RESULT: Budget = Budget {
    limit: 16.0,
    text: "≤ 16 ms",
    source: "Performance: first frame after a result arrives",
};

pub const GRID_VIEW_REBUILD: Budget = Budget {
    limit: 50.0,
    text: "≤ 50 ms",
    source: "Performance: rebuilding the grid view after a filter keystroke or a header sort, 100,000 rows",
};

/// How late after its threshold a long operation's progress may first be
/// drawn: one of the import dialog's redraws, which come every 250 ms
/// after the first at 300 ms. Later than that, the UI thread held it back.
pub const SHOWN_SLACK_MS: f64 = 250.0;

pub const PROGRESS_SHOWN: Budget = Budget {
    limit: 300.0 + SHOWN_SLACK_MS,
    text: "after 300 ms, within one 250 ms redraw",
    source: "User experience: anything that may take more than 300 ms shows progress",
};

pub const CANCEL_SHOWN: Budget = Budget {
    limit: 2000.0 + SHOWN_SLACK_MS,
    text: "past 2 s, within one 250 ms redraw",
    source: "User experience: past 2 s, the elapsed time and a way to cancel",
};

/// A cancelled import leaves the table as it was.
pub const ROWS_LEFT_BY_CANCEL: Budget = Budget {
    limit: 0.0,
    text: "0 rows (all or nothing)",
    source: "ADR-0040: a cancellation rolls the import back",
};

/// One measured number of a path.
#[derive(Clone, Debug, PartialEq)]
pub struct Metric {
    pub name: String,
    pub value: Option<f64>,
    pub unit: &'static str,
    pub budget: Option<Budget>,
    /// Why it was not measured.
    pub reason: Option<String>,
}

impl Metric {
    /// `value` held to `budget`.
    pub fn checked(
        name: impl Into<String>,
        value: f64,
        unit: &'static str,
        budget: Budget,
    ) -> Self {
        Self {
            name: name.into(),
            value: Some(value),
            unit,
            budget: Some(budget),
            reason: None,
        }
    }

    /// A number without a budget.
    pub fn info(name: impl Into<String>, value: f64, unit: &'static str) -> Self {
        Self {
            name: name.into(),
            value: Some(value),
            unit,
            budget: None,
            reason: None,
        }
    }

    /// A number the run could not measure, and why.
    pub fn missing(
        name: impl Into<String>,
        unit: &'static str,
        budget: Option<Budget>,
        reason: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            value: None,
            unit,
            budget,
            reason: Some(reason.into()),
        }
    }

    /// `value` held to `budget` when there is one, or why it is missing.
    pub fn maybe(
        name: impl Into<String>,
        value: Option<f64>,
        unit: &'static str,
        budget: Option<Budget>,
        reason: &str,
    ) -> Self {
        let name = name.into();
        match value {
            Some(value) => Self {
                name,
                value: Some(value),
                unit,
                budget,
                reason: None,
            },
            None => Self::missing(name, unit, budget, reason),
        }
    }

    pub fn status(&self) -> Status {
        match (self.value, self.budget) {
            (None, _) => Status::NotMeasured,
            (Some(_), None) => Status::Info,
            (Some(value), Some(budget)) if value.is_finite() && value <= budget.limit => {
                Status::Pass
            }
            (Some(_), Some(_)) => Status::Fail,
        }
    }

    /// The value as the report writes it.
    pub fn value_text(&self) -> String {
        match self.value {
            None => "—".into(),
            Some(value) => format!("{} {}", number(value, self.unit), self.unit)
                .trim_end()
                .to_owned(),
        }
    }

    pub fn to_json(&self) -> Value {
        json!({
            "metric": self.name,
            "value": self.value,
            "unit": self.unit,
            "budget": self.budget.map(|budget| budget.text),
            "budget_limit": self.budget.map(|budget| budget.limit),
            "budget_source": self.budget.map(|budget| budget.source),
            "status": self.status().as_str(),
            "reason": self.reason,
        })
    }
}

/// Milliseconds and sizes with two decimals; counts and rates whole.
fn number(value: f64, unit: &str) -> String {
    match unit {
        "ms" | "MiB" | "MiB/s" | "s" => format!("{value:.2}"),
        _ => format!("{value:.0}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_value_passes_at_its_limit_and_fails_above_it() {
        assert_eq!(
            Metric::checked("frame", 8.0, "ms", GRID_FRAME_P95).status(),
            Status::Pass
        );
        assert_eq!(
            Metric::checked("frame", 8.01, "ms", GRID_FRAME_P95).status(),
            Status::Fail
        );
        assert_eq!(
            Metric::checked("frame", f64::NAN, "ms", GRID_FRAME_P95).status(),
            Status::Fail
        );
        assert_eq!(Metric::info("rows", 3.0, "rows/s").status(), Status::Info);
        let missing = Metric::missing("frame", "ms", Some(STALL), "no probe");
        assert_eq!(missing.status(), Status::NotMeasured);
        assert_eq!(missing.value_text(), "—");
        assert_eq!(
            Metric::maybe("x", None, "ms", Some(STALL), "why")
                .reason
                .as_deref(),
            Some("why")
        );
    }

    #[test]
    fn the_report_shows_the_value_budget_and_status() {
        let metric = Metric::checked("Rows left", 0.0, "rows", ROWS_LEFT_BY_CANCEL);
        assert_eq!(metric.value_text(), "0 rows");
        let value = metric.to_json();
        assert_eq!(value["status"], "pass");
        assert_eq!(value["budget_limit"], 0.0);
        assert_eq!(Metric::info("p95", 1.234, "ms").value_text(), "1.23 ms");
        // The UX thresholds keep one redraw of slack, not more.
        assert_eq!(PROGRESS_SHOWN.limit, 550.0);
        assert_eq!(CANCEL_SHOWN.limit, 2250.0);
    }
}
