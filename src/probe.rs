//! Measurements for the UI budgets in [Performance](../docs/performance.md#how-to-measure)
//! (ADR-0010), turned on with `JYKRE_PROBE=1`.
//!
//! The application records the time of each frame (from the root view's
//! `render` to the end of paint) and how late a 5 ms heartbeat on the UI
//! thread wakes up. [`Samples`] keeps those durations and summarises them as
//! median, p95 and maximum; the summary is logged every few seconds.
use std::time::Duration;

/// The environment variable that turns the probe on.
pub const VARIABLE: &str = "JYKRE_PROBE";

/// How often the heartbeat wakes; any extra delay is time the UI thread was
/// busy.
pub const HEARTBEAT: Duration = Duration::from_millis(5);

/// How often a summary is logged.
pub const REPORT_EVERY: Duration = Duration::from_secs(5);

/// Whether `JYKRE_PROBE` is set to anything but empty or `0`.
pub fn enabled() -> bool {
    std::env::var_os(VARIABLE).is_some_and(|value| !value.is_empty() && value != "0")
}

/// Durations collected since the last summary.
#[derive(Debug, Default)]
pub struct Samples(Vec<Duration>);

/// Median, 95th percentile and maximum of a set of samples.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Summary {
    pub count: usize,
    pub median: Duration,
    pub p95: Duration,
    pub max: Duration,
}

impl Samples {
    pub fn push(&mut self, sample: Duration) {
        self.0.push(sample);
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Summarises the samples and starts over. `None` without samples.
    pub fn take_summary(&mut self) -> Option<Summary> {
        let mut samples = std::mem::take(&mut self.0);
        samples.sort_unstable();
        let rank = |fraction: f64| {
            let index = ((samples.len() as f64 * fraction).ceil() as usize).saturating_sub(1);
            samples.get(index).copied()
        };
        Some(Summary {
            count: samples.len(),
            median: rank(0.5)?,
            p95: rank(0.95)?,
            max: *samples.last()?,
        })
    }
}

/// Milliseconds with fractions, for log fields.
pub fn ms(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summaries_report_median_p95_and_max_then_start_over() {
        let mut samples = Samples::default();
        assert_eq!(samples.take_summary(), None);
        for ms in (1..=100).rev() {
            samples.push(Duration::from_millis(ms));
        }
        assert_eq!(samples.len(), 100);
        assert_eq!(
            samples.take_summary(),
            Some(Summary {
                count: 100,
                median: Duration::from_millis(50),
                p95: Duration::from_millis(95),
                max: Duration::from_millis(100),
            })
        );
        assert!(samples.is_empty());
        samples.push(Duration::from_millis(7));
        let one = samples.take_summary().expect("one sample");
        let seven = Duration::from_millis(7);
        assert_eq!(
            (one.count, one.median, one.p95, one.max),
            (1, seven, seven, seven)
        );
    }
}
