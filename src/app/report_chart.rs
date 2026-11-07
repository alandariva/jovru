//! A report panel's chart or numbers, its own entity embedded with
//! `.cached`, so the panel around it redraws without laying the chart out
//! again ([ADR-0012](../../docs/adr/0012-result-grid-as-a-cached-entity.md)).
//! It draws what [`jykre::reports::chart::visual`] computed when the result
//! or the chart changed, never while rendering.
use super::ui::*;
use gpui_kit::assets::IconName as I;
use gpui_kit::component::chart::{AreaChart, BarChart, PieChart};
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::{prelude::FluentBuilder as _, *};
use jykre::reports::chart::{ChartData, ChartPoint, Indicator, Visual};
use jykre::reports::{ChartKind, Signal};
use jykre::tr;
use std::rc::Rc;

/// The colours of the series, in order, in the theme on screen.
pub fn chart_series() -> [u32; 6] {
    let c = colors();
    [c.mint, c.blue, c.purple, c.amber, c.pink, c.teal]
}

/// A point's label, unique by its position, so two rows with the same label
/// stay two bars.
#[derive(Clone, PartialEq, Eq, Hash)]
struct Band(usize, SharedString);

impl From<Band> for SharedString {
    fn from(band: Band) -> Self {
        band.1
    }
}

/// One point to draw: its position and the data it belongs to.
#[derive(Clone)]
struct Datum {
    index: usize,
    data: Rc<ChartData>,
}

impl Datum {
    fn point(&self) -> Option<&ChartPoint> {
        self.data.points.get(self.index).or_else(|| {
            self.data
                .other
                .as_ref()
                .filter(|_| self.index == self.data.points.len())
        })
    }

    fn band(&self) -> Band {
        Band(
            self.index,
            self.point()
                .map_or_else(SharedString::default, |point| point.label.clone().into()),
        )
    }

    fn value(&self, serie: usize) -> f64 {
        self.point()
            .and_then(|point| point.values.get(serie).copied().flatten())
            .unwrap_or(0.0)
    }

    fn text(&self, serie: usize) -> SharedString {
        self.point()
            .and_then(|point| point.texts.get(serie).cloned().flatten())
            .unwrap_or_else(|| "NULL".to_owned())
            .into()
    }
}

pub struct ReportChart {
    data: Option<Rc<ChartData>>,
    numbers: Vec<Indicator>,
    /// The most numbers on one line; the panel's width decides it.
    per_row: usize,
}

impl ReportChart {
    pub fn new() -> Self {
        Self {
            data: None,
            numbers: Vec::new(),
            per_row: 1,
        }
    }

    /// Draws `visual` from now on; `None` draws nothing.
    pub fn set_visual(&mut self, visual: Option<Visual>, cx: &mut Context<Self>) {
        (self.data, self.numbers) = match visual {
            Some(Visual::Chart(data)) => (Some(Rc::new(data)), Vec::new()),
            Some(Visual::Number(numbers)) => (None, numbers),
            None => (None, Vec::new()),
        };
        cx.notify();
    }

    /// Lays numbers out `per_row` to a line from now on.
    pub fn set_per_row(&mut self, per_row: usize, cx: &mut Context<Self>) {
        let per_row = per_row.max(1);
        if self.per_row != per_row {
            self.per_row = per_row;
            cx.notify();
        }
    }

    /// One value, large, as the database wrote it: NULL in the NULL style,
    /// green or red (with a mark, so colour is not the only sign) when one
    /// of its conditions holds.
    fn indicator(index: usize, number: &Indicator, size: f32) -> Stateful<Div> {
        let text = number.text.clone().unwrap_or_else(|| "NULL".to_owned());
        let (color, mark, spoken) = match (&number.text, number.signal) {
            (None, _) => (
                colors().value_null,
                None,
                tr!("{0}: {1}", number.column, text),
            ),
            (Some(_), Some(Signal::Green)) => (
                colors().mint,
                Some(I::CircleCheck),
                tr!("{0}: {1}, green", number.column, text),
            ),
            (Some(_), Some(Signal::Red)) => (
                colors().red,
                Some(I::CircleX),
                tr!("{0}: {1}, red", number.column, text),
            ),
            (Some(_), None) => (colors().text, None, tr!("{0}: {1}", number.column, text)),
        };
        let whole = tr!("{0}: {1}", number.column, text);
        column()
            .id(("report-number", index))
            .role(Role::Status)
            .aria_label(spoken)
            .flex_1()
            .min_w_0()
            .h_full()
            .justify_center()
            .px_2()
            .gap_1()
            .tooltip(move |window, cx| Tooltip::new(whole.clone()).build(window, cx))
            .child(
                div()
                    .flex_none()
                    .text_size(px(size))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(rgb(color))
                    .truncate()
                    .child(text),
            )
            .child(
                row()
                    .gap_1()
                    .min_w_0()
                    .when_some(mark, |s, mark| s.child(icon(mark, color)))
                    .child(label(number.column.clone()).truncate()),
            )
    }

    /// The numbers, `per_row` to a line, each line as tall as the others.
    fn indicators(&self) -> Div {
        let size = if self.numbers.len() == 1 { 34. } else { 26. };
        column().flex_1().w_full().gap_2().children(
            self.numbers
                .chunks(self.per_row)
                .enumerate()
                .map(|(line, numbers)| {
                    row()
                        .flex_1()
                        .min_h_0()
                        .w_full()
                        .items_stretch()
                        .gap_3()
                        .children(numbers.iter().enumerate().map(|(index, number)| {
                            Self::indicator(line * self.per_row + index, number, size)
                        }))
                        // A short last line keeps the columns of the others.
                        .children(
                            (numbers.len()..self.per_row.min(self.numbers.len()))
                                .map(|_| div().flex_1()),
                        )
                }),
        )
    }

    fn data(&self, data: &Rc<ChartData>) -> Vec<Datum> {
        let count = data.points.len() + usize::from(data.other.is_some());
        (0..count)
            .map(|index| Datum {
                index,
                data: data.clone(),
            })
            .collect()
    }
}

impl Render for ReportChart {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        if !self.numbers.is_empty() {
            return div()
                .id("report-chart")
                .size_full()
                .flex()
                .flex_col()
                .child(self.indicators());
        }
        let Some(data) = self.data.clone() else {
            return div().id("report-chart");
        };
        let points = self.data(&data);
        let chart: AnyElement = match data.kind {
            ChartKind::Bar | ChartKind::Number => row()
                .size_full()
                .gap_3()
                .children(data.series.iter().enumerate().map(|(serie, name)| {
                    let color = chart_series()[serie % chart_series().len()];
                    column()
                        .flex_1()
                        .h_full()
                        .gap_1()
                        .child(label(name.clone()))
                        .child(
                            div().flex_1().min_h_0().child(
                                BarChart::new(points.clone())
                                    .id(("report-bars", serie))
                                    .appear(false)
                                    .band(Datum::band)
                                    .value(move |datum: &Datum| datum.value(serie))
                                    .fill(move |_, _, _, _| rgb(color))
                                    .value_axis(true)
                                    .tooltip_title(|datum: &Datum| datum.band().1)
                                    .tooltip_value(move |datum: &Datum, _| datum.text(serie)),
                            ),
                        )
                }))
                .into_any_element(),
            ChartKind::Line => {
                let mut chart = AreaChart::new(points)
                    .id("report-lines")
                    .appear(false)
                    .x(Datum::band)
                    .linear()
                    .y_axis(true)
                    .tooltip_title(|datum: &Datum| datum.band().1)
                    .tooltip_value(|datum: &Datum, serie, _| datum.text(serie));
                for serie in 0..data.series.len() {
                    let color = Hsla::from(rgb(chart_series()[serie % chart_series().len()]));
                    chart = chart
                        .y(move |datum: &Datum| datum.value(serie))
                        .stroke(color)
                        .fill(color.opacity(0.0));
                }
                column()
                    .size_full()
                    .gap_1()
                    .child(row().gap_3().children(data.series.iter().enumerate().map(
                        |(serie, name)| {
                            row()
                                .gap_1()
                                .child(dot(chart_series()[serie % chart_series().len()]))
                                .child(label(name.clone()))
                        },
                    )))
                    .child(div().flex_1().min_h_0().child(chart))
                    .into_any_element()
            }
            ChartKind::Pie => PieChart::new(points)
                .id("report-pie")
                .appear(false)
                .outer_radius(110.)
                .value(|datum: &Datum| datum.value(0).max(0.0) as f32)
                .color({
                    let series = chart_series();
                    move |datum: &Datum| rgb(series[datum.index % series.len()])
                })
                .label(|datum: &Datum| datum.band().1)
                .tooltip_name(|datum: &Datum| datum.band().1)
                .tooltip_value(|datum: &Datum, _, _| datum.text(0))
                .into_any_element(),
        };
        let left_out = data.left_out();
        div()
            .id("report-chart")
            .role(Role::Image)
            .aria_label(tr!("Chart of the result"))
            .size_full()
            .flex()
            .flex_col()
            .gap_1()
            .child(div().flex_1().min_h_0().child(chart))
            .when(left_out > 0, |s| {
                s.child(label(tr!(
                    "The chart shows the first {0} of {1} rows.",
                    data.points.len(),
                    data.total
                )))
            })
    }
}
