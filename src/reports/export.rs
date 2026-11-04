//! A report's print document: one self-contained HTML file with its title,
//! description, parameters, chart and table, which the browser prints or
//! saves as PDF (ADR-0024).
//!
//! It is the one place results reach the disk, and only at a path the user
//! chose; nothing here picks a path or writes a temporary file. The page
//! loads nothing (a content security policy forbids it) and every text from
//! the database is escaped.
use super::ChartKind;
use super::chart::ChartData;
use crate::result::ResultData;
use std::path::{Path, PathBuf};

/// What the document shows.
pub struct Document<'a> {
    pub title: &'a str,
    pub description: &'a str,
    pub connection: &'a str,
    /// Each parameter's label and the value the run bound (`None` is NULL).
    pub parameters: Vec<(String, Option<String>)>,
    pub result: &'a ResultData,
    /// The result stopped at the row limit or the memory ceiling.
    pub limited: bool,
    pub chart: Option<&'a ChartData>,
    /// When the report ran, as UTC text.
    pub generated: String,
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
h1{font-size:22px;margin:0 0 4px}p.meta{color:#5d6670;margin:0 0 16px}\
p.description{margin:0 0 16px;white-space:pre-wrap}\
dl{display:grid;grid-template-columns:max-content 1fr;gap:2px 12px;margin:0 0 16px}dt{color:#5d6670}dd{margin:0}\
figure{margin:0 0 20px;break-inside:avoid}figcaption{color:#5d6670;font-size:12px}svg text{font-size:11px;fill:#3a414a}\
table{border-collapse:collapse;width:100%;font-size:12px}thead{display:table-header-group}\
th,td{border:1px solid #d5dae0;padding:3px 6px;text-align:left;vertical-align:top}th{background:#f1f3f5}\
tr{break-inside:avoid}td.null{color:#8a939c;font-style:italic}td.number{text-align:right;font-variant-numeric:tabular-nums}\
.note{color:#8a5a00;margin:8px 0}.no-print{margin:0 0 16px}button{font:inherit;padding:6px 12px}\
@page{size:A4;margin:14mm}@media print{.no-print{display:none}body{margin:0}}";

/// Colours of the chart's series, in order.
const SERIES: [&str; 6] = [
    "#2f9e77", "#3b7dd8", "#8e5bd0", "#d9822b", "#d04f6c", "#3aa7b8",
];

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
            document.connection,
            document.generated
        ))
    ));
    if !document.description.trim().is_empty() {
        page.push_str(&format!(
            "<p class=\"description\">{}</p>",
            escape(document.description)
        ));
    }
    if !document.parameters.is_empty() {
        page.push_str("<dl>");
        for (label, value) in &document.parameters {
            page.push_str(&format!(
                "<dt>{}</dt><dd>{}</dd>",
                escape(label),
                value.as_deref().map_or_else(|| "NULL".to_owned(), escape)
            ));
        }
        page.push_str("</dl>");
    }
    if let Some(chart) = document.chart {
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
    table(&mut page, document.result);
    if document.limited {
        page.push_str(&format!(
            "<p class=\"note\">{}</p>",
            escape(&crate::tr!(
                "The result stopped at {0} rows; more rows exist.",
                document.result.row_count()
            ))
        ));
    }
    page.push_str("</body></html>");
    page
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
        ChartKind::Bar | ChartKind::Line => axes(&mut svg, chart),
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

/// Writes the document where the user chose.
pub async fn write(path: PathBuf, contents: String) -> std::io::Result<()> {
    tokio::fs::write(path, contents).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reports::chart::ChartPoint;
    use crate::result::ResultColumn;

    fn document<'a>(result: &'a ResultData, chart: Option<&'a ChartData>) -> Document<'a> {
        Document {
            title: "Sales <2024>",
            description: "By \"region\"",
            connection: "warehouse",
            parameters: vec![
                ("Start".into(), Some("2024-01-01".into())),
                ("End".into(), None),
            ],
            result,
            limited: true,
            chart,
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

    #[test]
    fn the_document_escapes_every_text_and_loads_nothing() {
        let result = sample();
        let page = html(&document(&result, None));
        assert!(!page.contains("<script>"));
        assert!(page.contains("&lt;script&gt;alert(1)&lt;/script&gt;"));
        assert!(page.contains("North &amp; South"));
        assert!(page.contains("<title>Sales &lt;2024&gt;</title>"));
        assert!(page.contains("By &quot;region&quot;"));
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
        let chart = ChartData {
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
        };
        let page = html(&document(&result, Some(&chart)));
        assert_eq!(page.matches("<svg").count(), 1);
        assert!(page.contains("<title>&lt;b&gt;: 10.50</title>"));
        assert!(!page.contains("<b>"));
        assert!(page.contains("<figcaption>"));
        assert!(!page.contains("src=") && !page.contains("href="));
        for kind in [ChartKind::Line, ChartKind::Pie] {
            let drawn = svg(&ChartData {
                kind,
                ..chart.clone()
            });
            assert!(drawn.starts_with("<svg") && drawn.ends_with("</svg>"));
        }
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
