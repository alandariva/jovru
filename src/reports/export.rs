//! A report's print document: one self-contained HTML file with its title,
//! description and parameters, then each panel with its chart or numbers
//! and table, laid out in two columns like the report, which the browser
//! prints or saves as PDF (ADR-0024, ADR-0025).
//!
//! It is the one place results reach the disk, and only at a path the user
//! chose; nothing here picks a path or writes a temporary file. The page
//! loads nothing (a content security policy forbids it) and every text from
//! the database is escaped.
use super::chart::{ChartData, Indicator, Visual};
use super::{ChartKind, Signal, Width};
use crate::result::ResultData;
use std::path::{Path, PathBuf};

/// What the document shows.
pub struct Document<'a> {
    pub title: &'a str,
    pub description: &'a str,
    /// The connections the panels run on, as the meta line names them.
    pub connections: Vec<String>,
    pub panels: Vec<PanelDocument<'a>>,
    /// When the report ran, as UTC text.
    pub generated: String,
}

/// One panel of the document.
pub struct PanelDocument<'a> {
    pub title: String,
    /// Where the panel ran, as the report shows it: its connection and
    /// database.
    pub source: String,
    pub width: Width,
    /// The last run's result; `None` when the panel did not run.
    pub result: Option<&'a ResultData>,
    /// The result stopped at the row limit or the memory ceiling.
    pub limited: bool,
    pub visual: Option<&'a Visual>,
    /// The result's table is printed.
    pub table: bool,
    /// Why the panel shows less: its run's error, or a chart that cannot be
    /// drawn.
    pub note: Option<String>,
    /// Each parameter the panel's run used: its label and the value bound
    /// (`None` is NULL).
    pub bound: Vec<(String, Option<String>)>,
}

/// Escapes text for HTML content and attribute values.
pub fn escape(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&#39;"),
            ch => escaped.push(ch),
        }
    }
    escaped
}

const STYLE: &str = "\
body{font:13px/1.45 system-ui,-apple-system,'Segoe UI',sans-serif;color:#1d2127;margin:32px;background:#fff}\
h1{font-size:22px;margin:0 0 4px}h2{font-size:15px;margin:0 0 2px}p.meta{color:#5d6670;margin:0 0 16px}\
section p.meta{margin:0 0 8px;font-size:12px}\
p.description{margin:0 0 16px;white-space:pre-wrap}\
dl{display:grid;grid-template-columns:max-content 1fr;gap:2px 12px;margin:0 0 16px}dt{color:#5d6670}dd{margin:0}\
.panels{display:grid;grid-template-columns:repeat(2,minmax(0,1fr));gap:24px 20px}.full{grid-column:1/-1}\
section{min-width:0}\
figure{margin:0 0 12px;break-inside:avoid}figcaption{color:#5d6670;font-size:12px}svg text{font-size:11px;fill:#3a414a}\
.numbers{display:flex;flex-wrap:wrap;gap:8px 28px;break-inside:avoid;margin:4px 0 12px}.number{min-width:0}\
.number .value{font-size:32px;font-weight:600;font-variant-numeric:tabular-nums;overflow-wrap:anywhere}\
.number .value.null{color:#8a939c;font-style:italic}.number .label{display:block;color:#5d6670}\
.number.green .value{color:#1f7a4d}.number.red .value{color:#c0392b}.number .mark{margin-right:4px}\
table{border-collapse:collapse;width:100%;font-size:12px}thead{display:table-header-group}\
th,td{border:1px solid #d5dae0;padding:3px 6px;text-align:left;vertical-align:top;overflow-wrap:anywhere}th{background:#f1f3f5}\
tr{break-inside:avoid}td.null{color:#8a939c;font-style:italic}td.number{text-align:right;font-variant-numeric:tabular-nums}\
.note{color:#8a5a00;margin:8px 0}.no-print{margin:0 0 16px}button{font:inherit;padding:6px 12px}\
@page{size:A4;margin:14mm}@media print{.no-print{display:none}body{margin:0}}";

/// Colours of the chart's series, in order.
const SERIES: [&str; 6] = [
    "#2f9e77", "#3b7dd8", "#8e5bd0", "#d9822b", "#d04f6c", "#3aa7b8",
];

/// The parameters the panels ran with, each once, as the first panel that
/// used it bound it.
fn shared_parameters(panels: &[PanelDocument]) -> Vec<(String, Option<String>)> {
    let mut shared: Vec<(String, Option<String>)> = Vec::new();
    for (label, value) in panels.iter().flat_map(|panel| panel.bound.iter()) {
        if !shared.iter().any(|(known, _)| known == label) {
            shared.push((label.clone(), value.clone()));
        }
    }
    shared
}

fn parameter_list(page: &mut String, parameters: &[(String, Option<String>)]) {
    if parameters.is_empty() {
        return;
    }
    page.push_str("<dl>");
    for (label, value) in parameters {
        page.push_str(&format!(
            "<dt>{}</dt><dd>{}</dd>",
            escape(label),
            value.as_deref().map_or_else(|| "NULL".to_owned(), escape)
        ));
    }
    page.push_str("</dl>");
}

/// The document as one HTML file.
pub fn html(document: &Document) -> String {
    let mut page = String::new();
    page.push_str("<!doctype html><html><head><meta charset=\"utf-8\">");
    page.push_str(
        "<meta http-equiv=\"Content-Security-Policy\" content=\"default-src 'none'; \
         style-src 'unsafe-inline'; script-src 'unsafe-inline'; img-src data:\">",
    );
    page.push_str("<meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">");
    page.push_str(&format!("<title>{}</title>", escape(document.title)));
    page.push_str(&format!("<style>{STYLE}</style></head><body>"));
    page.push_str(&format!(
        "<div class=\"no-print\"><button onclick=\"window.print()\">{}</button></div>",
        escape(crate::tr!("Print or save as PDF"))
    ));
    page.push_str(&format!("<h1>{}</h1>", escape(document.title)));
    page.push_str(&format!(
        "<p class=\"meta\">{}</p>",
        escape(&crate::tr!(
            "{0} · generated {1} UTC",
            document.connections.join(", "),
            document.generated
        ))
    ));
    if !document.description.trim().is_empty() {
        page.push_str(&format!(
            "<p class=\"description\">{}</p>",
            escape(document.description)
        ));
    }
    let shared = shared_parameters(&document.panels);
    parameter_list(&mut page, &shared);
    page.push_str("<main class=\"panels\">");
    for panel in &document.panels {
        section(&mut page, panel, &shared);
    }
    page.push_str("</main></body></html>");
    page
}

fn section(page: &mut String, panel: &PanelDocument, shared: &[(String, Option<String>)]) {
    page.push_str(match panel.width {
        Width::Half => "<section class=\"half\">",
        Width::Full => "<section class=\"full\">",
    });
    page.push_str(&format!("<h2>{}</h2>", escape(&panel.title)));
    if !panel.source.is_empty() {
        page.push_str(&format!("<p class=\"meta\">{}</p>", escape(&panel.source)));
    }
    // A panel run again with other values says which.
    let own: Vec<(String, Option<String>)> = panel
        .bound
        .iter()
        .filter(|bound| !shared.contains(bound))
        .cloned()
        .collect();
    parameter_list(page, &own);
    match panel.visual {
        Some(Visual::Chart(chart)) => figure(page, chart),
        Some(Visual::Number(numbers)) => indicators(page, numbers),
        None => {}
    }
    if let Some(note) = &panel.note {
        page.push_str(&format!("<p class=\"note\">{}</p>", escape(note)));
    }
    match panel.result {
        Some(result) if panel.table && result.column_count() > 0 => {
            table(page, result);
            if panel.limited {
                page.push_str(&format!(
                    "<p class=\"note\">{}</p>",
                    escape(&crate::tr!(
                        "The result stopped at {0} rows; more rows exist.",
                        result.row_count()
                    ))
                ));
            }
        }
        Some(_) => {}
        None if panel.note.is_none() => page.push_str(&format!(
            "<p class=\"note\">{}</p>",
            escape(crate::tr!("This panel did not run."))
        )),
        None => {}
    }
    page.push_str("</section>");
}

fn figure(page: &mut String, chart: &ChartData) {
    page.push_str("<figure>");
    page.push_str(&svg(chart));
    if chart.left_out() > 0 {
        page.push_str(&format!(
            "<figcaption>{}</figcaption>",
            escape(&crate::tr!(
                "The chart shows the first {0} of {1} rows.",
                chart.points.len(),
                chart.total
            ))
        ));
    }
    page.push_str("</figure>");
}

/// The numbers side by side. A coloured one also carries a mark, so it
/// reads the same printed in black and white.
fn indicators(page: &mut String, numbers: &[Indicator]) {
    page.push_str("<div class=\"numbers\">");
    for number in numbers {
        let value = match &number.text {
            Some(text) => format!("<span class=\"value\">{}</span>", escape(text)),
            None => "<span class=\"value null\">NULL</span>".to_owned(),
        };
        let (class, mark) = match number.signal {
            Some(Signal::Green) => (" green", "<span class=\"mark\">\u{2713}</span>"),
            Some(Signal::Red) => (" red", "<span class=\"mark\">\u{2715}</span>"),
            None => ("", ""),
        };
        page.push_str(&format!(
            "<div class=\"number{class}\">{value}<span class=\"label\">{mark}{}</span></div>",
            escape(&number.column)
        ));
    }
    page.push_str("</div>");
}

fn table(page: &mut String, result: &ResultData) {
    let numbers: Vec<bool> = result
        .columns()
        .iter()
        .map(|column| {
            crate::cell_format::family(&column.data_type) == crate::cell_format::ValueFamily::Number
        })
        .collect();
    page.push_str("<table><thead><tr>");
    for column in result.columns() {
        page.push_str(&format!("<th>{}</th>", escape(&column.name)));
    }
    page.push_str("</tr></thead><tbody>");
    for row in 0..result.row_count() {
        page.push_str("<tr>");
        for (index, value) in result.row(row).enumerate() {
            match value {
                None => page.push_str("<td class=\"null\">NULL</td>"),
                Some(value) if numbers.get(index).copied().unwrap_or(false) => {
                    page.push_str(&format!("<td class=\"number\">{}</td>", escape(value)));
                }
                Some(value) => page.push_str(&format!("<td>{}</td>", escape(value))),
            }
        }
        page.push_str("</tr>");
    }
    page.push_str("</tbody></table>");
}

const WIDTH: f64 = 720.0;
const HEIGHT: f64 = 280.0;
const LEFT: f64 = 56.0;
const BOTTOM: f64 = 36.0;
const TOP: f64 = 12.0;

/// The chart as inline SVG, laid out like the application's.
pub fn svg(chart: &ChartData) -> String {
    let mut svg = format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 {WIDTH} {HEIGHT}\" \
         width=\"100%\" role=\"img\">"
    );
    match chart.kind {
        ChartKind::Pie => pie(&mut svg, chart),
        ChartKind::Bar | ChartKind::Line | ChartKind::Number => axes(&mut svg, chart),
    }
    legend(&mut svg, chart);
    svg.push_str("</svg>");
    svg
}

fn legend(svg: &mut String, chart: &ChartData) {
    if chart.kind == ChartKind::Pie {
        return;
    }
    for (index, name) in chart.series.iter().enumerate() {
        let x = LEFT + index as f64 * 140.0;
        svg.push_str(&format!(
            "<rect x=\"{x}\" y=\"{}\" width=\"10\" height=\"10\" fill=\"{}\"/>\
             <text x=\"{}\" y=\"{}\">{}</text>",
            HEIGHT - 12.0,
            SERIES[index % SERIES.len()],
            x + 14.0,
            HEIGHT - 3.0,
            escape(name)
        ));
    }
}

/// Bars or lines over a value axis from the smallest value (or zero) to the
/// largest.
fn axes(svg: &mut String, chart: &ChartData) {
    let (Some(max), Some(min)) = (chart.max(), chart.min()) else {
        return;
    };
    let low = min.min(0.0);
    let high = if max > low { max.max(0.0) } else { low + 1.0 };
    let plot = HEIGHT - BOTTOM - TOP - 16.0;
    let y = |value: f64| TOP + plot * (high - value) / (high - low);
    let width = WIDTH - LEFT - 8.0;
    svg.push_str(&format!(
        "<line x1=\"{LEFT}\" y1=\"{0}\" x2=\"{1}\" y2=\"{0}\" stroke=\"#c5ccd3\"/>",
        y(0.0),
        WIDTH - 8.0
    ));
    for tick in [low, (low + high) / 2.0, high] {
        svg.push_str(&format!(
            "<text x=\"{}\" y=\"{}\" text-anchor=\"end\">{}</text>",
            LEFT - 6.0,
            y(tick) + 4.0,
            short(tick)
        ));
    }
    let count = chart.points.len().max(1) as f64;
    let band = width / count;
    let label_every = (chart.points.len() / 12).max(1);
    for (index, point) in chart.points.iter().enumerate() {
        if index % label_every == 0 {
            let label: String = point.label.chars().take(14).collect();
            svg.push_str(&format!(
                "<text x=\"{}\" y=\"{}\" text-anchor=\"middle\">{}</text>",
                LEFT + band * (index as f64 + 0.5),
                HEIGHT - BOTTOM + 2.0,
                escape(&label)
            ));
        }
    }
    let series = chart.series.len().max(1);
    if chart.kind == ChartKind::Bar {
        let bar = band * 0.8 / series as f64;
        for (index, point) in chart.points.iter().enumerate() {
            for (serie, value) in point.values.iter().enumerate() {
                let Some(value) = value else { continue };
                let x = LEFT + band * index as f64 + band * 0.1 + bar * serie as f64;
                let (top, bottom) = (y(value.max(0.0)), y(value.min(0.0)));
                svg.push_str(&format!(
                    "<rect x=\"{x:.2}\" y=\"{top:.2}\" width=\"{bar:.2}\" height=\"{:.2}\" fill=\"{}\">\
                     <title>{}: {}</title></rect>",
                    (bottom - top).max(0.5),
                    SERIES[serie % SERIES.len()],
                    escape(&point.label),
                    escape(point.texts.get(serie).and_then(Option::as_deref).unwrap_or("NULL"))
                ));
            }
        }
    } else {
        for serie in 0..series {
            let points: Vec<String> = chart
                .points
                .iter()
                .enumerate()
                .filter_map(|(index, point)| {
                    let value = point.values.get(serie).copied().flatten()?;
                    Some(format!(
                        "{:.2},{:.2}",
                        LEFT + band * (index as f64 + 0.5),
                        y(value)
                    ))
                })
                .collect();
            svg.push_str(&format!(
                "<polyline fill=\"none\" stroke=\"{}\" stroke-width=\"2\" points=\"{}\"/>",
                SERIES[serie % SERIES.len()],
                points.join(" ")
            ));
        }
    }
}

fn pie(svg: &mut String, chart: &ChartData) {
    let slices: Vec<_> = chart
        .points
        .iter()
        .chain(chart.other.iter())
        .filter_map(|point| {
            let value = point.values.first().copied().flatten()?;
            (value > 0.0).then_some((point, value))
        })
        .collect();
    let total: f64 = slices.iter().map(|(_, value)| value).sum();
    if total <= 0.0 {
        return;
    }
    let (cx, cy, r) = (HEIGHT / 2.0, HEIGHT / 2.0 - 8.0, HEIGHT / 2.0 - 24.0);
    let mut angle = -std::f64::consts::FRAC_PI_2;
    for (index, (point, value)) in slices.iter().enumerate() {
        let sweep = value / total * std::f64::consts::TAU;
        let colour = SERIES[index % SERIES.len()];
        let title = format!(
            "<title>{}: {}</title>",
            escape(&point.label),
            escape(point.texts.first().and_then(Option::as_deref).unwrap_or(""))
        );
        if slices.len() == 1 {
            svg.push_str(&format!(
                "<circle cx=\"{cx}\" cy=\"{cy}\" r=\"{r}\" fill=\"{colour}\">{title}</circle>"
            ));
        } else {
            let (x1, y1) = (cx + r * angle.cos(), cy + r * angle.sin());
            let end = angle + sweep;
            let (x2, y2) = (cx + r * end.cos(), cy + r * end.sin());
            let large = i32::from(sweep > std::f64::consts::PI);
            svg.push_str(&format!(
                "<path d=\"M{cx},{cy} L{x1:.2},{y1:.2} A{r},{r} 0 {large} 1 {x2:.2},{y2:.2} Z\" \
                 fill=\"{colour}\" stroke=\"#fff\">{title}</path>"
            ));
            angle = end;
        }
        let label: String = point.label.chars().take(28).collect();
        svg.push_str(&format!(
            "<rect x=\"{}\" y=\"{}\" width=\"10\" height=\"10\" fill=\"{colour}\"/>\
             <text x=\"{}\" y=\"{}\">{} · {}</text>",
            HEIGHT + 8.0,
            20.0 + index as f64 * 20.0,
            HEIGHT + 24.0,
            29.0 + index as f64 * 20.0,
            escape(&label),
            escape(point.texts.first().and_then(Option::as_deref).unwrap_or(""))
        ));
    }
}

/// A tick label: whole numbers as they are, others with two decimals, large
/// ones with a k/M/B suffix.
fn short(value: f64) -> String {
    let magnitude = value.abs();
    let (scaled, suffix) = if magnitude >= 1e9 {
        (value / 1e9, "B")
    } else if magnitude >= 1e6 {
        (value / 1e6, "M")
    } else if magnitude >= 1e4 {
        (value / 1e3, "k")
    } else {
        (value, "")
    };
    if scaled.fract() == 0.0 {
        format!("{scaled:.0}{suffix}")
    } else {
        format!("{scaled:.2}{suffix}")
    }
}

/// A file name for `title` with `extension`: characters Windows forbids
/// become `_`, a reserved device name gets a `_` in front, and an empty
/// title becomes "report".
pub fn file_name(title: &str, extension: &str) -> String {
    let cleaned: String = title
        .chars()
        .map(|ch| {
            if ch.is_control() || matches!(ch, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*')
            {
                '_'
            } else {
                ch
            }
        })
        .collect();
    let trimmed = cleaned.trim().trim_end_matches(['.', ' ']);
    let mut stem: String = if trimmed.is_empty() {
        "report".to_owned()
    } else {
        trimmed.chars().take(120).collect()
    };
    let upper = stem.to_ascii_uppercase();
    let device = upper.split('.').next().unwrap_or_default();
    let reserved = matches!(device, "CON" | "PRN" | "AUX" | "NUL")
        || (device.len() == 4
            && (device.starts_with("COM") || device.starts_with("LPT"))
            && device.ends_with(|ch: char| ch.is_ascii_digit() && ch != '0'));
    if reserved {
        stem.insert(0, '_');
    }
    format!("{stem}.{extension}")
}

/// `path` with `extension` when it has none: a save dialog may not add it.
pub fn with_extension(path: &Path, extension: &str) -> PathBuf {
    if path.extension().is_some() {
        path.to_path_buf()
    } else {
        path.with_extension(extension)
    }
}

/// Now, as the UTC text the document shows.
pub fn now_utc() -> String {
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs());
    crate::error_report::utc(seconds)
}

/// Writes the document where the user chose. Its telemetry span records the
/// size, never the path.
pub async fn write(path: PathBuf, contents: String) -> std::io::Result<()> {
    use tracing::Instrument as _;
    let span = crate::client_span!(
        "write_file",
        jykre.bytes = crate::telemetry::attributes::count(contents.len())
    );
    let written = tokio::fs::write(path, contents)
        .instrument(span.clone())
        .await;
    if written.is_err() {
        span.record("error.type", "io");
        span.record("otel.status_code", "error");
    }
    written
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reports::chart::ChartPoint;
    use crate::result::ResultColumn;

    fn panel<'a>(result: Option<&'a ResultData>, visual: Option<&'a Visual>) -> PanelDocument<'a> {
        PanelDocument {
            title: "Revenue <by region>".into(),
            source: "warehouse · shop".into(),
            width: Width::Full,
            result,
            limited: true,
            visual,
            table: true,
            note: None,
            bound: vec![
                ("Start".into(), Some("2024-01-01".into())),
                ("End".into(), None),
            ],
        }
    }

    fn document<'a>(panels: Vec<PanelDocument<'a>>) -> Document<'a> {
        Document {
            title: "Sales <2024>",
            description: "By \"region\"",
            connections: vec!["warehouse".into(), "archive <old>".into()],
            panels,
            generated: "2026-11-04 10:00:00".into(),
        }
    }

    fn sample() -> ResultData {
        ResultData::from_rows(
            vec![
                ResultColumn::new("region", "text"),
                ResultColumn::new("total", "numeric"),
            ],
            [
                vec![Some("<script>alert(1)</script>"), Some("10.50")],
                vec![Some("North & South"), None],
            ],
        )
    }

    fn chart() -> ChartData {
        ChartData {
            kind: ChartKind::Bar,
            series: vec!["total".into()],
            points: vec![
                ChartPoint {
                    label: "<b>".into(),
                    values: vec![Some(10.5)],
                    texts: vec![Some("10.50".into())],
                },
                ChartPoint {
                    label: "x".into(),
                    values: vec![Some(-2.0)],
                    texts: vec![Some("-2".into())],
                },
            ],
            total: 5,
            other: None,
        }
    }

    #[test]
    fn the_document_escapes_every_text_and_loads_nothing() {
        let result = sample();
        let page = html(&document(vec![panel(Some(&result), None)]));
        assert!(!page.contains("<script>"));
        assert!(page.contains("&lt;script&gt;alert(1)&lt;/script&gt;"));
        assert!(page.contains("North &amp; South"));
        assert!(page.contains("<title>Sales &lt;2024&gt;</title>"));
        assert!(page.contains("By &quot;region&quot;"));
        assert!(page.contains("<h2>Revenue &lt;by region&gt;</h2>"));
        assert!(page.contains("warehouse, archive &lt;old&gt; · generated"));
        assert!(page.contains("<p class=\"meta\">warehouse · shop</p>"));
        assert!(page.contains("<td class=\"null\">NULL</td>"));
        assert!(page.contains("<td class=\"number\">10.50</td>"));
        assert!(page.contains("<dd>NULL</dd>"));
        assert!(page.contains("default-src 'none'"));
        assert!(page.contains("@page{size:A4"));
        assert!(page.contains("thead{display:table-header-group}"));
        assert!(page.contains("class=\"note\""));
        assert!(!page.contains("http:") && !page.contains("https:"));
        assert!(!page.contains("<svg"));
    }

    #[test]
    fn a_chart_is_drawn_inline_with_escaped_labels() {
        let result = sample();
        let chart = Visual::Chart(chart());
        let page = html(&document(vec![panel(Some(&result), Some(&chart))]));
        assert_eq!(page.matches("<svg").count(), 1);
        assert!(page.contains("<title>&lt;b&gt;: 10.50</title>"));
        assert!(!page.contains("<b>"));
        assert!(page.contains("<figcaption>"));
        assert!(!page.contains("src=") && !page.contains("href="));
        let Visual::Chart(data) = &chart else {
            return;
        };
        for kind in [ChartKind::Line, ChartKind::Pie] {
            let drawn = svg(&ChartData {
                kind,
                ..data.clone()
            });
            assert!(drawn.starts_with("<svg") && drawn.ends_with("</svg>"));
        }
    }

    #[test]
    fn every_panel_is_a_section_laid_out_by_its_width() {
        let result = sample();
        let number = Visual::Number(vec![
            Indicator {
                column: "total <eur>".into(),
                text: Some("1234567890.123456789".into()),
                signal: Some(Signal::Green),
            },
            Indicator {
                column: "late".into(),
                text: Some("3".into()),
                signal: Some(Signal::Red),
            },
            Indicator {
                column: "open".into(),
                text: Some("7".into()),
                signal: None,
            },
        ]);
        let empty = Visual::Number(vec![Indicator {
            column: "n".into(),
            text: None,
            signal: None,
        }]);
        let mut half = panel(Some(&result), Some(&number));
        half.width = Width::Half;
        half.table = false;
        // Run again with another value: it says which.
        half.bound = vec![("Start".into(), Some("2025-01-01".into()))];
        let mut failed = panel(None, None);
        failed.note = Some("relation \"x\" does not exist".into());
        let mut skipped = panel(None, Some(&empty));
        skipped.bound.clear();
        let page = html(&document(vec![
            panel(Some(&result), None),
            half,
            failed,
            skipped,
        ]));
        assert_eq!(page.matches("<section").count(), 4);
        assert_eq!(page.matches("<section class=\"half\">").count(), 1);
        assert!(page.contains(".full{grid-column:1/-1}"));
        assert!(page.contains(
            "<div class=\"number green\"><span class=\"value\">1234567890.123456789</span>\
             <span class=\"label\"><span class=\"mark\">\u{2713}</span>total &lt;eur&gt;</span></div>"
        ));
        assert!(page.contains(
            "<div class=\"number red\"><span class=\"value\">3</span>\
             <span class=\"label\"><span class=\"mark\">\u{2715}</span>late</span></div>"
        ));
        assert!(page.contains(
            "<div class=\"number\"><span class=\"value\">7</span><span class=\"label\">open</span></div>"
        ));
        assert_eq!(page.matches("<div class=\"numbers\">").count(), 2);
        assert!(page.contains("<span class=\"value null\">NULL</span>"));
        assert!(page.contains("relation &quot;x&quot; does not exist"));
        assert!(page.contains("<dd>2025-01-01</dd>"));
        // The shared values show once, at the top.
        assert_eq!(page.matches("<dd>2024-01-01</dd>").count(), 1);
        // The table of the half panel is not printed: one table only.
        assert_eq!(page.matches("<table>").count(), 1);
    }

    #[test]
    fn file_names_avoid_what_windows_forbids() {
        assert_eq!(file_name("Sales: Q1/Q2?", "html"), "Sales_ Q1_Q2_.html");
        assert_eq!(file_name("  ", "html"), "report.html");
        assert_eq!(file_name("CON", "html"), "_CON.html");
        assert_eq!(file_name("com1.backup", "html"), "_com1.backup.html");
        assert_eq!(file_name("COM0", "html"), "COM0.html");
        assert_eq!(
            file_name("Ends with dots...", "html"),
            "Ends with dots.html"
        );
    }

    #[test]
    fn an_extension_is_added_only_when_missing() {
        assert_eq!(
            with_extension(Path::new("out/report"), "html"),
            PathBuf::from("out/report.html")
        );
        assert_eq!(
            with_extension(Path::new("out/report.htm"), "html"),
            PathBuf::from("out/report.htm")
        );
    }

    #[test]
    fn tick_labels_are_short() {
        assert_eq!(short(0.0), "0");
        assert_eq!(short(12.5), "12.50");
        assert_eq!(short(25_000.0), "25k");
        assert_eq!(short(-3_500_000.0), "-3.50M");
    }
}
