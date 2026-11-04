//! A report's chart, its own entity embedded with `.cached`, so the report
//! around it redraws without laying the chart out again
//! ([ADR-0012](../../docs/adr/0012-result-grid-as-a-cached-entity.md)).
//! It draws what [`jykre::reports::chart::derive`] computed when the result
//! or the chart changed, never while rendering.
use super::ui::*;
use gpui_kit::component::chart::{AreaChart, BarChart, PieChart};
use gpui_kit::{prelude::FluentBuilder as _, *};
use jykre::reports::ChartKind;
use jykre::reports::chart::{ChartData, ChartPoint};
use jykre::tr;
use std::rc::Rc;

/// The colours of the series, in order.
pub const CHART_SERIES: [u32; 6] = [MINT, BLUE, PURPLE, AMBER, PINK, TEAL];

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
}

impl ReportChart {
    pub fn new() -> Self {
        Self { data: None }
    }

    /// Draws `data` from now on; `None` draws nothing.
    pub fn set_data(&mut self, data: Option<Rc<ChartData>>, cx: &mut Context<Self>) {
        self.data = data;
        cx.notify();
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
        let Some(data) = self.data.clone() else {
            return div().id("report-chart");
        };
        let points = self.data(&data);
        let chart: AnyElement = match data.kind {
            ChartKind::Bar => row()
                .size_full()
                .gap_3()
                .children(data.series.iter().enumerate().map(|(serie, name)| {
                    let color = CHART_SERIES[serie % CHART_SERIES.len()];
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
                    let color = Hsla::from(rgb(CHART_SERIES[serie % CHART_SERIES.len()]));
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
                                .child(dot(CHART_SERIES[serie % CHART_SERIES.len()]))
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
                .color(|datum: &Datum| rgb(CHART_SERIES[datum.index % CHART_SERIES.len()]))
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
