//! Statistics of the selected cells, of a page and of a column, computed
//! without a window and off the UI thread.
//!
//! [`block`] reads a block of a result in place: the pages are shared across
//! threads ([ADR-0022](../docs/adr/0022-result-pages-shared-across-threads.md)),
//! so the grid hands it the page and the positions to read. For a column the
//! grid copies the values into a [`Sample`] a few cells at a time, and
//! [`column`] summarizes it on another thread. Numbers are added, averaged
//! and ordered exactly ([`crate::decimal`]), never as floats.
use crate::cell_format::ValueFamily;
use crate::decimal::{self, Number, Sum, compare_numbers};
use crate::result::ResultData;
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};

/// Decimals an average or a median keeps beyond those of the values.
pub const EXTRA_DECIMALS: u32 = 6;

/// How many of the most frequent values a column summary lists.
pub const FREQUENT: usize = 5;

/// How many bars a number column's distribution has at most.
pub const HISTOGRAM_BINS: usize = 20;

/// One bar of a distribution: the values in an equal slice of the range
/// from the smallest to the largest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Bin {
    pub count: usize,
    /// The smallest and largest values in it, as the database wrote them;
    /// `None` for an empty bar.
    pub min: Option<String>,
    pub max: Option<String>,
}

/// A value as its float, for ordering, and its text, which is reported.
type End<'a> = (f64, &'a str);

/// The distribution of the numbers among `values` in up to `bins` equal
/// slices of their range, for drawing. Only the slice a value falls in is
/// found through floating point; what a bar reports (its smallest and
/// largest values) stays the database's text. Empty when there is no
/// finite number; one bar when every number is the same.
pub fn histogram<'a>(values: impl IntoIterator<Item = &'a str>, bins: usize) -> Vec<Bin> {
    let numbers: Vec<(f64, &str)> = values
        .into_iter()
        .filter(|value| decimal::is_number(value))
        .filter_map(|value| {
            let float = value.trim().parse::<f64>().ok()?;
            float.is_finite().then_some((float, value.trim()))
        })
        .collect();
    let (Some(low), Some(high)) = (
        numbers.iter().map(|(float, _)| *float).reduce(f64::min),
        numbers.iter().map(|(float, _)| *float).reduce(f64::max),
    ) else {
        return Vec::new();
    };
    let count = if high > low { bins.max(1) } else { 1 };
    // Each bar's ends as (float, text). Rounding to a float keeps order, so
    // floats that differ order their texts; only equal floats need the
    // exact comparison.
    let order = |left: End, right: End| {
        left.0
            .partial_cmp(&right.0)
            .filter(|order| *order != Ordering::Equal)
            .unwrap_or_else(|| compare_numbers(left.1, right.1))
    };
    let mut ends: Vec<Option<(End, End)>> = vec![None; count];
    let mut counts = vec![0; count];
    let width = (high - low) / count as f64;
    for (float, text) in numbers {
        let index = if width > 0.0 {
            (((float - low) / width) as usize).min(count - 1)
        } else {
            0
        };
        counts[index] += 1;
        let value = (float, text);
        ends[index] = Some(match ends[index] {
            None => (value, value),
            Some((min, max)) => (
                if order(value, min) == Ordering::Less {
                    value
                } else {
                    min
                },
                if order(value, max) == Ordering::Greater {
                    value
                } else {
                    max
                },
            ),
        });
    }
    let out: Vec<Bin> = counts
        .into_iter()
        .zip(ends)
        .map(|(count, ends)| Bin {
            count,
            min: ends.map(|(min, _)| min.1.to_owned()),
            max: ends.map(|(_, max)| max.1.to_owned()),
        })
        .collect();
    out
}

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
    // Each decimal is read once; ordering them compares what was read.
    let mut decimals: Vec<Number> = Vec::new();
    for value in values {
        let number = Number::new(value);
        if let Some(decimal) = &number.decimal {
            sum.add_decimal(decimal);
            decimals.push(number);
        }
    }
    let count = decimals.len();
    if count == 0 {
        return None;
    }
    let min = decimals.iter().min_by(|left, right| left.cmp(right))?.text;
    let max = decimals.iter().max_by(|left, right| left.cmp(right))?.text;
    let middle = count / 2;
    let (lower, upper, _) = decimals.select_nth_unstable_by(middle, |left, right| left.cmp(right));
    let upper = upper.text;
    let median = if count % 2 == 1 {
        Some(upper.trim().to_owned())
    } else {
        // The mean of the two middle values; `lower` holds the one below.
        let below = lower.iter().max_by(|left, right| left.cmp(right))?.text;
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

/// One column of a block of cells ([`block`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlockColumn {
    /// The column's position in the result.
    pub column: usize,
    /// Cells that are not NULL.
    pub values: usize,
    pub nulls: usize,
    /// Distinct values that are not NULL, numbers by value; `None` when the
    /// block was summarized without its details.
    pub distinct: Option<usize>,
    /// The numbers of a number column; `None` without details.
    pub numbers: Option<NumberSummary>,
}

/// What the statistics panel shows about a block of cells: the selection,
/// or every row the filter shows of the page.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlockSummary {
    pub rows: usize,
    /// Cells that are not NULL.
    pub values: usize,
    pub nulls: usize,
    /// The numbers among the cells of number columns.
    pub numbers: Option<NumberSummary>,
    /// One entry per column of the block, in its order.
    pub columns: Vec<BlockColumn>,
}

impl BlockSummary {
    pub fn cells(&self) -> usize {
        self.values + self.nulls
    }

    /// What the footer says about it.
    pub fn selection(&self) -> SelectionSummary {
        SelectionSummary {
            values: self.values,
            numbers: self.numbers.clone(),
        }
    }
}

/// The cells of `rows` × `columns` (each with its family) of `result`:
/// their values, NULLs and the numbers of all number columns together,
/// summed exactly. `detail` also summarizes each column's numbers and counts
/// its distinct values, which the statistics panel shows and the footer does
/// not need. Values are read in place, never copied.
pub fn block(
    result: &ResultData,
    rows: &[usize],
    columns: &[(usize, ValueFamily)],
    detail: bool,
) -> BlockSummary {
    let mut all_numbers: Vec<&str> = Vec::new();
    let mut summary = BlockSummary {
        rows: rows.len(),
        values: 0,
        nulls: 0,
        numbers: None,
        columns: Vec::with_capacity(columns.len()),
    };
    for (column, family) in columns {
        let number = *family == ValueFamily::Number;
        let mut values: Vec<&str> = Vec::new();
        let mut nulls = 0;
        for row in rows {
            match result.value(*row, *column) {
                Some(value) => values.push(value),
                None => nulls += 1,
            }
        }
        let distinct = detail.then(|| {
            if number {
                values
                    .iter()
                    .map(|value| decimal::canonical(value))
                    .collect::<HashSet<_>>()
                    .len()
            } else {
                values.iter().copied().collect::<HashSet<_>>().len()
            }
        });
        summary.values += values.len();
        summary.nulls += nulls;
        let numbers = if number {
            all_numbers.extend(values.iter().copied());
            if detail {
                numbers(values.iter().copied())
            } else {
                None
            }
        } else {
            None
        };
        summary.columns.push(BlockColumn {
            column: *column,
            values: values.len(),
            nulls,
            distinct,
            numbers,
        });
    }
    summary.numbers = numbers(all_numbers);
    summary
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
    /// A number column's distribution ([`histogram`]); empty otherwise.
    pub histogram: Vec<Bin>,
}

/// A summary of one column's values, compared as its family sorts them.
pub fn column(sample: &Sample, family: ValueFamily) -> ColumnSummary {
    let number = family == ValueFamily::Number;
    // In the order the header sorts by.
    let min_max = if number {
        decimal::min_max_numbers(sample.values())
    } else {
        sample.values().min().zip(sample.values().max())
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
        min: min_max.map(|(min, _)| min.to_owned()),
        max: min_max.map(|(_, max)| max.to_owned()),
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
        histogram: if number {
            histogram(sample.values(), HISTOGRAM_BINS)
        } else {
            Vec::new()
        },
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
    fn a_block_sums_each_number_column_and_all_of_them() {
        let result = ResultData::from_rows(
            vec![
                crate::result::ResultColumn::new("id", "int4"),
                crate::result::ResultColumn::new("name", "text"),
                crate::result::ResultColumn::new("amount", "numeric"),
            ],
            [
                [Some("1"), Some("a"), Some("1.50")],
                [Some("2"), None, Some("1.5")],
                [Some("3"), Some("a"), None],
            ],
        );
        let columns = [
            (0, ValueFamily::Number),
            (1, ValueFamily::Text),
            (2, ValueFamily::Number),
        ];
        let summary = block(&result, &[0, 1, 2], &columns, true);
        assert_eq!((summary.rows, summary.values, summary.nulls), (3, 7, 2));
        assert_eq!(summary.cells(), 9);
        let all = summary.numbers.as_ref().expect("numbers");
        assert_eq!((all.count, all.sum.as_deref()), (5, Some("9.00")));
        let name = &summary.columns[1];
        assert_eq!((name.column, name.values, name.nulls), (1, 2, 1));
        assert_eq!((name.distinct, name.numbers.as_ref()), (Some(1), None));
        // Numbers count by value: 1.50 and 1.5 are one.
        let amount = &summary.columns[2];
        assert_eq!(amount.distinct, Some(1));
        assert_eq!(
            amount
                .numbers
                .as_ref()
                .and_then(|numbers| numbers.sum.as_deref()),
            Some("3.00")
        );
        // The footer's part, and a block read without distinct counts.
        assert_eq!(summary.selection().values, 7);
        let plain = block(&result, &[1], &columns[1..], false);
        assert_eq!(plain.columns[0].distinct, None);
        assert_eq!(plain.columns[1].numbers, None);
        assert_eq!((plain.values, plain.nulls), (1, 1));
        assert_eq!(
            plain.numbers.and_then(|numbers| numbers.sum).as_deref(),
            Some("1.5")
        );
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
    fn a_histogram_slices_the_range_and_keeps_each_bars_exact_ends() {
        let bins = histogram(["0", "1", "1.0", "9.99", "10", "abc", "NaN", "1e500"], 4);
        assert_eq!(bins.len(), 4);
        assert_eq!(
            bins.iter().map(|bin| bin.count).collect::<Vec<_>>(),
            [3, 0, 0, 2]
        );
        assert_eq!(
            (bins[0].min.as_deref(), bins[0].max.as_deref()),
            (Some("0"), Some("1"))
        );
        assert_eq!((bins[1].min.as_deref(), bins[1].count), (None, 0));
        // The largest value falls in the last bar, not past it.
        assert_eq!(bins[3].max.as_deref(), Some("10"));
        // One value repeated: one bar; nothing numeric: no bars.
        let same = histogram(["2.50", "2.5"], 4);
        assert_eq!((same.len(), same[0].count), (1, 2));
        assert!(histogram(["x"], 4).is_empty());
        // Equal as floats, apart as decimals: the exact text decides.
        let close = histogram(["0.1000000000000000001", "0.1", "5"], 2);
        assert_eq!(
            (close[0].min.as_deref(), close[0].max.as_deref()),
            (Some("0.1"), Some("0.1000000000000000001"))
        );
        // A column summary of numbers carries it; of text, not.
        let amounts = column(&sample(&[Some("1"), Some("3"), None]), ValueFamily::Number);
        assert_eq!(
            amounts.histogram.iter().map(|bin| bin.count).sum::<usize>(),
            2
        );
        let names = column(&sample(&[Some("1")]), ValueFamily::Text);
        assert!(names.histogram.is_empty());
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
