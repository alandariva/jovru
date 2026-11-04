//! The data a report's chart draws, derived from its result.
//!
//! Exact numbers never pass through floating point where they are shown:
//! `f64` serves only the geometry (bar heights, line positions, slice
//! angles), while tooltips, the table and exports keep the database's text,
//! and a pie's remainder is summed exactly ([`crate::decimal::Sum`]).
use super::{ChartKind, ChartSpec};
use crate::cell_format::{ValueFamily, family};
use crate::decimal::{Sum, is_number};
use crate::result::ResultData;

/// The most bars a bar chart draws; more would not fit a label each.
pub const MAX_BARS: usize = 50;
/// The most points a line chart draws.
pub const MAX_POINTS: usize = 1_000;
/// The most slices a pie draws before the rest become one "Other" slice.
pub const MAX_SLICES: usize = 10;

#[derive(Clone, Debug, PartialEq)]
pub struct ChartPoint {
    pub label: String,
    /// One per series, for the geometry; `None` where the cell is NULL.
    pub values: Vec<Option<f64>>,
    /// One per series, as the database wrote it.
    pub texts: Vec<Option<String>>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ChartData {
    pub kind: ChartKind,
    /// The value columns' names.
    pub series: Vec<String>,
    pub points: Vec<ChartPoint>,
    /// The result's rows, of which `points` shows the first ones.
    pub total: usize,
    /// A pie's slice for the rows past [`MAX_SLICES`], summed exactly.
    pub other: Option<ChartPoint>,
}

impl ChartData {
    /// Rows the chart leaves out (a pie counts its "Other" slice as
    /// showing them).
    pub fn left_out(&self) -> usize {
        if self.other.is_some() {
            0
        } else {
            self.total.saturating_sub(self.points.len())
        }
    }

    /// The largest value drawn, for a common scale; `None` without values.
    pub fn max(&self) -> Option<f64> {
        self.points
            .iter()
            .chain(self.other.iter())
            .flat_map(|point| point.values.iter().flatten().copied())
            .reduce(f64::max)
    }

    /// The smallest value drawn.
    pub fn min(&self) -> Option<f64> {
        self.points
            .iter()
            .chain(self.other.iter())
            .flat_map(|point| point.values.iter().flatten().copied())
            .reduce(f64::min)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChartProblem {
    /// The chart names no value column.
    NoValues,
    /// The result has no column with this name.
    MissingColumn(String),
    /// This value column holds something other than numbers.
    NotNumeric(String),
    /// A pie shows a single value column.
    PieNeedsOneValue,
}

fn column(result: &ResultData, name: &str) -> Result<usize, ChartProblem> {
    result
        .columns()
        .iter()
        .position(|column| column.name == name)
        .ok_or_else(|| ChartProblem::MissingColumn(name.to_owned()))
}

/// Whether column `index` holds numbers: its type says so, or every value
/// that is not NULL is a number.
fn numeric(result: &ResultData, index: usize) -> bool {
    let typed = result
        .columns()
        .get(index)
        .is_some_and(|column| family(&column.data_type) == ValueFamily::Number);
    typed
        || (0..result.row_count())
            .filter_map(|row| result.value(row, index))
            .all(is_number)
}

/// The columns a chart can take as values.
pub fn numeric_columns(result: &ResultData) -> Vec<String> {
    (0..result.column_count())
        .filter(|index| numeric(result, *index))
        .filter_map(|index| {
            result
                .columns()
                .get(index)
                .map(|column| column.name.clone())
        })
        .collect()
}

/// What `spec` draws of `result`.
pub fn derive(result: &ResultData, spec: &ChartSpec) -> Result<ChartData, ChartProblem> {
    if spec.values.is_empty() {
        return Err(ChartProblem::NoValues);
    }
    if spec.kind == ChartKind::Pie && spec.values.len() > 1 {
        return Err(ChartProblem::PieNeedsOneValue);
    }
    let label = column(result, &spec.label)?;
    let values = spec
        .values
        .iter()
        .map(|name| {
            let index = column(result, name)?;
            if numeric(result, index) {
                Ok(index)
            } else {
                Err(ChartProblem::NotNumeric(name.clone()))
            }
        })
        .collect::<Result<Vec<_>, _>>()?;
    let limit = match spec.kind {
        ChartKind::Bar => MAX_BARS,
        ChartKind::Line => MAX_POINTS,
        ChartKind::Pie => MAX_SLICES,
    };
    let point = |row: usize| ChartPoint {
        label: result.value(row, label).unwrap_or("NULL").to_owned(),
        values: values
            .iter()
            .map(|index| result.value(row, *index).and_then(|text| text.parse().ok()))
            .collect(),
        texts: values
            .iter()
            .map(|index| result.value(row, *index).map(str::to_owned))
            .collect(),
    };
    let total = result.row_count();
    let points = (0..total.min(limit)).map(point).collect();
    let other = (spec.kind == ChartKind::Pie && total > limit)
        .then(|| {
            let mut sum = Sum::default();
            for row in limit..total {
                if let Some(text) = values.first().and_then(|index| result.value(row, *index)) {
                    sum.add(text);
                }
            }
            let text = sum.total()?;
            Some(ChartPoint {
                label: crate::tr!("Other").to_owned(),
                values: vec![text.parse().ok()],
                texts: vec![Some(text)],
            })
        })
        .flatten();
    Ok(ChartData {
        kind: spec.kind,
        series: spec.values.clone(),
        points,
        total,
        other,
    })
}

/// A chart that fits `result`: the first column that is not a number labels
/// the points and up to three number columns give values; a date or time
/// label draws a line, anything else bars. `None` without a number column
/// beside the label.
pub fn suggest(result: &ResultData) -> Option<ChartSpec> {
    let columns = result.columns();
    let numbers = numeric_columns(result);
    let label = columns
        .iter()
        .find(|column| !numbers.contains(&column.name))
        .or_else(|| columns.first())?;
    let values: Vec<String> = numbers
        .into_iter()
        .filter(|name| *name != label.name)
        .take(3)
        .collect();
    if values.is_empty() {
        return None;
    }
    let kind = if family(&label.data_type) == ValueFamily::Temporal {
        ChartKind::Line
    } else {
        ChartKind::Bar
    };
    Some(ChartSpec {
        kind,
        label: label.name.clone(),
        values,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::result::ResultColumn;

    fn result(columns: &[(&str, &str)], rows: &[Vec<Option<&str>>]) -> ResultData {
        ResultData::from_rows(
            columns
                .iter()
                .map(|(name, kind)| ResultColumn::new(*name, *kind))
                .collect(),
            rows.iter().map(|row| row.iter().copied()),
        )
    }

    fn spec(kind: ChartKind, label: &str, values: &[&str]) -> ChartSpec {
        ChartSpec {
            kind,
            label: label.into(),
            values: values.iter().map(|name| (*name).to_owned()).collect(),
        }
    }

    #[test]
    fn columns_are_found_by_name_and_values_keep_their_text() {
        let data = result(
            &[("total", "numeric"), ("month", "text"), ("count", "text")],
            &[
                vec![Some("10.50"), Some("Jan"), Some("3")],
                vec![None, Some("Feb"), Some("1e3")],
            ],
        );
        let chart = derive(&data, &spec(ChartKind::Bar, "month", &["total", "count"])).unwrap();
        assert_eq!(chart.series, ["total", "count"]);
        assert_eq!(chart.points[0].label, "Jan");
        assert_eq!(chart.points[0].values, [Some(10.5), Some(3.0)]);
        assert_eq!(chart.points[0].texts[0].as_deref(), Some("10.50"));
        assert_eq!(chart.points[1].values, [None, Some(1000.0)]);
        assert_eq!(chart.max(), Some(1000.0));
        assert_eq!(chart.min(), Some(3.0));
        assert_eq!(chart.left_out(), 0);
    }

    #[test]
    fn problems_name_the_column_at_fault() {
        let data = result(
            &[("a", "text"), ("b", "int4")],
            &[vec![Some("x"), Some("1")]],
        );
        assert_eq!(
            derive(&data, &spec(ChartKind::Bar, "a", &[])),
            Err(ChartProblem::NoValues)
        );
        assert_eq!(
            derive(&data, &spec(ChartKind::Bar, "z", &["b"])),
            Err(ChartProblem::MissingColumn("z".into()))
        );
        assert_eq!(
            derive(&data, &spec(ChartKind::Bar, "b", &["a"])),
            Err(ChartProblem::NotNumeric("a".into()))
        );
        assert_eq!(
            derive(&data, &spec(ChartKind::Pie, "a", &["b", "b"])),
            Err(ChartProblem::PieNeedsOneValue)
        );
    }

    #[test]
    fn bars_stop_at_their_limit_and_a_pie_sums_the_rest_exactly() {
        let labels: Vec<String> = (0..60).map(|n| format!("r{n}")).collect();
        let rows: Vec<Vec<Option<&str>>> = labels
            .iter()
            .map(|label| vec![Some(label.as_str()), Some("0.1")])
            .collect();
        let data = result(&[("l", "text"), ("v", "numeric")], &rows);
        let bars = derive(&data, &spec(ChartKind::Bar, "l", &["v"])).unwrap();
        assert_eq!(
            (bars.points.len(), bars.total, bars.left_out()),
            (MAX_BARS, 60, 10)
        );
        let pie = derive(&data, &spec(ChartKind::Pie, "l", &["v"])).unwrap();
        assert_eq!(pie.points.len(), MAX_SLICES);
        let other = pie.other.clone().unwrap();
        // 50 × 0.1 is exactly 5.0, which adding floats would miss.
        assert_eq!(other.texts, [Some("5.0".to_owned())]);
        assert_eq!(pie.left_out(), 0);
    }

    #[test]
    fn suggestions_pick_a_label_and_up_to_three_numbers() {
        let data = result(
            &[
                ("day", "date"),
                ("a", "int8"),
                ("b", "numeric"),
                ("c", "float8"),
                ("d", "int4"),
                ("note", "text"),
            ],
            &[vec![
                Some("2024-01-01"),
                Some("1"),
                Some("2"),
                Some("3"),
                Some("4"),
                Some("x"),
            ]],
        );
        assert_eq!(
            suggest(&data),
            Some(spec(ChartKind::Line, "day", &["a", "b", "c"]))
        );
        let words = result(&[("name", "text")], &[vec![Some("x")]]);
        assert_eq!(suggest(&words), None);
        let numbers = result(
            &[("n", "text"), ("m", "text")],
            &[vec![Some("1"), Some("2")]],
        );
        assert_eq!(suggest(&numbers), Some(spec(ChartKind::Bar, "n", &["m"])));
    }
}
