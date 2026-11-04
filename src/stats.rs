//! Statistics of the selected cells and of a column, computed without a
//! window and off the UI thread.
//!
//! The grid copies the values it needs into a [`Sample`] (a few cells at a
//! time, so no frame waits for it) and hands the sample to another thread,
//! where [`selection`] and [`column`] compute their summaries. Numbers are
//! added, averaged and ordered exactly ([`crate::decimal`]), never as
//! floats.
use crate::cell_format::ValueFamily;
use crate::decimal::{self, Sum, compare_numbers};
use std::cmp::Ordering;
use std::collections::HashMap;

/// Decimals an average or a median keeps beyond those of the values.
pub const EXTRA_DECIMALS: u32 = 6;

/// How many of the most frequent values a column summary lists.
pub const FREQUENT: usize = 5;

/// Values copied out of a result, so their statistics can be computed on
/// another thread: the text of each value in one buffer and the NULLs
/// counted apart.
#[derive(Clone, Debug, Default)]
pub struct Sample {
    text: String,
    ends: Vec<usize>,
    nulls: usize,
}

impl Sample {
    pub fn push(&mut self, value: Option<&str>) {
        match value {
            Some(value) => {
                self.text.push_str(value);
                self.ends.push(self.text.len());
            }
            None => self.nulls += 1,
        }
    }

    /// The values that are not NULL.
    pub fn len(&self) -> usize {
        self.ends.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ends.is_empty() && self.nulls == 0
    }

    pub fn nulls(&self) -> usize {
        self.nulls
    }

    pub fn values(&self) -> impl Iterator<Item = &str> {
        let starts = std::iter::once(0).chain(self.ends.iter().copied());
        starts
            .zip(self.ends.iter().copied())
            .map(|(start, end)| &self.text[start..end])
    }
}

/// What the numbers among some values add up to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NumberSummary {
    /// How many values were decimals.
    pub count: usize,
    /// The exact total; `None` when a value was too large to add.
    pub sum: Option<String>,
    pub average: Option<String>,
    pub median: Option<String>,
    /// The smallest and largest values, as the database wrote them.
    pub min: String,
    pub max: String,
}

/// The numbers among `values`, or `None` when none is a decimal. Text such
/// as `NaN` is skipped.
pub fn numbers<'a>(values: impl IntoIterator<Item = &'a str>) -> Option<NumberSummary> {
    let mut sum = Sum::default();
    let mut decimals: Vec<&str> = Vec::new();
    for value in values {
        if sum.add(value) {
            decimals.push(value);
        }
    }
    let count = decimals.len();
    if count == 0 {
        return None;
    }
    let min = decimals
        .iter()
        .copied()
        .min_by(|left, right| compare_numbers(left, right))?;
    let max = decimals
        .iter()
        .copied()
        .max_by(|left, right| compare_numbers(left, right))?;
    let middle = count / 2;
    let (lower, upper, _) =
        decimals.select_nth_unstable_by(middle, |left, right| compare_numbers(left, right));
    let upper: &str = upper;
    let median = if count % 2 == 1 {
        Some(upper.trim().to_owned())
    } else {
        // The mean of the two middle values; `lower` holds the one below.
        let below = lower
            .iter()
            .copied()
            .max_by(|left, right| compare_numbers(left, right))?;
        let mut pair = Sum::default();
        pair.add(below);
        pair.add(upper);
        pair.divided(2, 1)
    };
    Some(NumberSummary {
        count,
        average: sum.divided(count, EXTRA_DECIMALS),
        sum: sum.total(),
        median,
        min: min.trim().to_owned(),
        max: max.trim().to_owned(),
    })
}

/// What the footer says about the selected cells.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SelectionSummary {
    /// Selected cells that are not NULL.
    pub values: usize,
    /// The numbers among the cells of number columns.
    pub numbers: Option<NumberSummary>,
}

/// A summary of the selected cells: `numbers` holds the cells of number
/// columns, `values` counts every cell that is not NULL.
pub fn selection(numbers: &Sample, values: usize) -> SelectionSummary {
    SelectionSummary {
        values,
        numbers: self::numbers(numbers.values()),
    }
}

/// What a column's statistics show.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ColumnSummary {
    /// Rows looked at: the rows of the page the filter shows.
    pub rows: usize,
    pub nulls: usize,
    /// Values that are empty text.
    pub empty: usize,
    /// Distinct values that are not NULL; numbers by value (`1.50` and
    /// `1.5` are one).
    pub distinct: usize,
    /// The smallest and largest values in the order the header sorts by.
    pub min: Option<String>,
    pub max: Option<String>,
    /// The length in characters of the shortest and longest value.
    pub shortest: Option<usize>,
    pub longest: Option<usize>,
    pub numbers: Option<NumberSummary>,
    /// The most frequent values with how often they occur, most frequent
    /// first; ties keep the order they first appear in.
    pub frequent: Vec<(String, usize)>,
}

/// A summary of one column's values, compared as its family sorts them.
pub fn column(sample: &Sample, family: ValueFamily) -> ColumnSummary {
    let number = family == ValueFamily::Number;
    let order = |left: &&str, right: &&str| -> Ordering {
        if number {
            compare_numbers(left, right)
        } else {
            left.cmp(right)
        }
    };
    let mut counts: HashMap<String, (usize, usize, &str)> = HashMap::new();
    let (mut empty, mut shortest, mut longest) = (0, None::<usize>, None::<usize>);
    for (position, value) in sample.values().enumerate() {
        if value.is_empty() {
            empty += 1;
        }
        let length = value.chars().count();
        shortest = Some(shortest.map_or(length, |shortest| shortest.min(length)));
        longest = Some(longest.map_or(length, |longest| longest.max(length)));
        let key = if number {
            decimal::canonical(value)
        } else {
            value.to_owned()
        };
        counts.entry(key).or_insert((0, position, value)).0 += 1;
    }
    let mut frequent: Vec<_> = counts.values().copied().collect();
    frequent.sort_by(|left, right| right.0.cmp(&left.0).then(left.1.cmp(&right.1)));
    ColumnSummary {
        rows: sample.len() + sample.nulls(),
        nulls: sample.nulls(),
        empty,
        distinct: counts.len(),
        min: sample.values().min_by(order).map(str::to_owned),
        max: sample.values().max_by(order).map(str::to_owned),
        shortest,
        longest,
        numbers: if number {
            numbers(sample.values())
        } else {
            None
        },
        frequent: frequent
            .into_iter()
            .take(FREQUENT)
            .map(|(count, _, value)| (value.to_owned(), count))
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn sample(values: &[Option<&str>]) -> Sample {
        let mut sample = Sample::default();
        for value in values {
            sample.push(*value);
        }
        sample
    }

    #[test]
    fn a_sample_keeps_values_and_counts_nulls() {
        let sample = sample(&[Some("a"), None, Some(""), Some("ção")]);
        assert_eq!(sample.values().collect::<Vec<_>>(), ["a", "", "ção"]);
        assert_eq!((sample.len(), sample.nulls()), (3, 1));
        assert!(Sample::default().is_empty());
    }

    #[test]
    fn numbers_are_summed_averaged_and_ordered_exactly() {
        let summary = numbers(["10.50", "2", "NaN", "-3", "0.1"]).expect("numbers");
        assert_eq!(summary.count, 4);
        assert_eq!(summary.sum.as_deref(), Some("9.60"));
        assert_eq!(summary.average.as_deref(), Some("2.40"));
        assert_eq!(
            (summary.min.as_str(), summary.max.as_str()),
            ("-3", "10.50")
        );
        // An even count takes the mean of the two middle values: 0.1 and 2.
        assert_eq!(summary.median.as_deref(), Some("1.05"));
        let odd = numbers(["3", "1", "1e3"]).expect("numbers");
        assert_eq!(odd.median.as_deref(), Some("3"));
        assert_eq!(odd.max, "1e3");
        assert_eq!(
            numbers(["2", "4"])
                .and_then(|summary| summary.median)
                .as_deref(),
            Some("3")
        );
        assert_eq!(
            numbers(["1", "2"])
                .and_then(|summary| summary.median)
                .as_deref(),
            Some("1.5")
        );
        assert_eq!(numbers(["abc", "NaN"]), None);
    }

    #[test]
    fn a_selection_counts_values_and_sums_its_numbers() {
        let summary = selection(&sample(&[Some("1"), Some("2.5"), None]), 7);
        assert_eq!(summary.values, 7);
        let numbers = summary.numbers.expect("numbers");
        assert_eq!(numbers.sum.as_deref(), Some("3.5"));
        assert_eq!(selection(&Sample::default(), 2).numbers, None);
    }

    #[test]
    fn a_column_summary_counts_distinct_and_frequent_values() {
        let text = column(
            &sample(&[Some("b"), Some("a"), None, Some("b"), Some(""), Some("ccc")]),
            ValueFamily::Text,
        );
        assert_eq!(
            (text.rows, text.nulls, text.empty, text.distinct),
            (6, 1, 1, 4)
        );
        assert_eq!(
            (text.min.as_deref(), text.max.as_deref()),
            (Some(""), Some("ccc"))
        );
        assert_eq!((text.shortest, text.longest), (Some(0), Some(3)));
        assert_eq!(text.frequent[0], ("b".to_owned(), 2));
        assert_eq!(text.frequent[1], ("a".to_owned(), 1));
        assert_eq!(text.numbers, None);
        // Numbers count by value and order by value, not as text.
        let amounts = column(
            &sample(&[Some("1.50"), Some("1.5"), Some("100"), Some("9")]),
            ValueFamily::Number,
        );
        assert_eq!(amounts.distinct, 3);
        assert_eq!(amounts.frequent[0], ("1.50".to_owned(), 2));
        assert_eq!(amounts.max.as_deref(), Some("100"));
        assert_eq!(
            amounts.numbers.and_then(|numbers| numbers.sum).as_deref(),
            Some("112.00")
        );
        let nothing = column(&sample(&[None, None]), ValueFamily::Number);
        assert_eq!((nothing.rows, nothing.distinct, nothing.min), (2, 0, None));
    }

    #[test]
    fn a_large_column_is_summarized_within_budget() {
        // 500,000 cells: a whole 100,000-row page of five number columns.
        let mut values = Sample::default();
        for row in 0..500_000 {
            values.push(Some(&format!("{}.{:02}", row * 7 % 100_003, row % 100)));
        }
        let start = Instant::now();
        let summary = column(&values, ValueFamily::Number);
        let elapsed = start.elapsed();
        assert_eq!(summary.numbers.map(|numbers| numbers.count), Some(500_000));
        assert!(elapsed < Duration::from_secs(10), "took {elapsed:?}");
    }
}
