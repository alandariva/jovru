//! Cancellable clipboard encoding over shared, immutable result snapshots.
use super::*;
use crate::drivers::{CancellationToken, DriverError, ErrorCode};
use crate::grid::GridView;
use std::ops::{Range, RangeInclusive};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// The exact page or displayed selection captured when Copy was requested.
pub enum CopySource {
    /// Every row and column, regardless of the grid's filter or sort.
    Page,
    /// A selection in the captured display order.
    Selection {
        view: Arc<GridView>,
        selection: CellSelection,
    },
    /// One cell's whole value, exactly as stored (the value inspector's
    /// Copy): never the part the inspector shows, and no copy settings.
    Value { row: usize, column: usize },
}

/// Owned copy inputs, safe to send to a worker without cloning cell values.
pub struct CopyRequest {
    pub result: Arc<ResultData>,
    pub source: CopySource,
    pub options: CopyOptions,
    pub dialect: Option<SqlDialect>,
    pub table: String,
    /// Names already quoted by the editor's dialect, one per result column.
    pub quoted_columns: Vec<String>,
}

/// Encoding progress; only counts, never database values.
#[derive(Clone, Copy, Debug, Default)]
pub struct Progress {
    pub rows: usize,
    pub total: usize,
}

impl CopyRequest {
    fn bounds(&self) -> Option<(Range<usize>, RangeInclusive<usize>)> {
        match &self.source {
            CopySource::Page => Some((
                0..self.result.row_count(),
                0..=self.result.column_count().checked_sub(1)?,
            )),
            CopySource::Selection { view, selection } => selection_block(*selection, &view.rows)
                .map(|(rows, cols)| (*rows.start()..rows.end() + 1, cols)),
            CopySource::Value { .. } => None,
        }
    }

    fn row(&self, position: usize) -> Option<usize> {
        match &self.source {
            CopySource::Page => Some(position),
            CopySource::Selection { view, .. } => view.rows.get(position).copied(),
            CopySource::Value { .. } => None,
        }
    }

    /// The whole value of a [`CopySource::Value`]; `None` for NULL or a
    /// cell the page does not have.
    fn value(&self) -> Option<&str> {
        match self.source {
            CopySource::Value { row, column } => self.result.value(row, column),
            _ => None,
        }
    }

    /// Estimates the unescaped size before encoding. Run on a worker: this
    /// reads cell lengths and honours cancellation and a 60 s deadline.
    /// Column names alone never need confirmation for unselected values.
    pub fn estimate(&self, cancel: &CancellationToken) -> DriverResult<usize> {
        let started = Instant::now();
        checkpoint(cancel, started)?;
        if matches!(self.source, CopySource::Value { .. }) {
            return Ok(self.value().map_or(0, str::len));
        }
        let Some((rows, cols)) = self.bounds() else {
            return Ok(0);
        };
        let mut bytes = cols
            .clone()
            .filter_map(|col| self.result.columns().get(col))
            .map(|column| column.name.len() + 3)
            .sum::<usize>();
        if self.options.format != CopyFormat::ColumnNames {
            for position in rows {
                checkpoint(cancel, started)?;
                if let Some(row) = self.row(position) {
                    for col in cols.clone() {
                        bytes = bytes
                            .saturating_add(self.result.value(row, col).map_or(0, str::len) + 3);
                    }
                }
            }
        }
        checkpoint(cancel, started)?;
        Ok(bytes)
    }
}

fn checkpoint(cancel: &CancellationToken, started: Instant) -> DriverResult<()> {
    if cancel.is_cancelled() {
        Err(DriverError::new(ErrorCode::Cancelled, "Copy cancelled"))
    } else if started.elapsed() >= Duration::from_secs(60) {
        Err(DriverError::new(ErrorCode::Timeout, "Copy timed out"))
    } else {
        Ok(())
    }
}

/// Encodes a snapshot on a worker, checking cancellation/deadline between
/// rows and reporting progress at most every 100 ms. Never publishes partial
/// text. The caller alone writes a successful, still-current copy to the OS
/// clipboard. Empty/hidden selections return `None`.
pub fn generate(
    request: &CopyRequest,
    cancel: &CancellationToken,
    mut progress: impl FnMut(Progress),
) -> DriverResult<Option<(String, usize)>> {
    let started = Instant::now();
    checkpoint(cancel, started)?;
    if matches!(request.source, CopySource::Value { .. }) {
        // One value, whole and exact: NULL copies nothing.
        progress(Progress { rows: 0, total: 1 });
        let copied = request.value().map(|value| (value.to_owned(), 1));
        checkpoint(cancel, started)?;
        progress(Progress { rows: 1, total: 1 });
        return Ok(copied);
    }
    let Some((rows, cols)) = request.bounds() else {
        return Ok(None);
    };
    let count = rows.len() * cols.clone().count();
    let mut block = Block {
        names: cols
            .clone()
            .filter_map(|col| request.result.columns().get(col))
            .map(|column| column.name.as_str())
            .collect(),
        families: cols
            .clone()
            .filter_map(|col| request.result.columns().get(col))
            .map(|column| cell_format::family(&column.data_type))
            .collect(),
        rows: Vec::with_capacity(1),
    };
    let quote = |name: &str| {
        request
            .result
            .columns()
            .iter()
            .position(|column| column.name == name)
            .and_then(|col| request.quoted_columns.get(col))
            .cloned()
            .unwrap_or_else(|| name.to_owned())
    };
    let style = SqlStyle {
        dialect: request.dialect,
        table: &request.table,
        quote_name: &quote,
    };
    let options = request.options;
    if options.format == CopyFormat::ColumnNames {
        return Ok(Some((quoted_names(&block, &style), count)));
    }
    let mut out = String::new();
    let mut seen = HashSet::new();
    match options.format {
        CopyFormat::Json => out.push_str("[\n"),
        CopyFormat::InList => out.push('('),
        CopyFormat::InsertValues => out.push_str(&format!(
            "INSERT INTO {} ({}) VALUES\n",
            style.table,
            quoted_names(&block, &style)
        )),
        CopyFormat::Markdown => out.push_str(&markdown(&block)),
        CopyFormat::Html => {
            let header = html(&block);
            out.push_str(
                header
                    .strip_suffix("  </tbody>\n</table>")
                    .unwrap_or(&header),
            );
        }
        CopyFormat::Text | CopyFormat::Csv if options.header => {
            out.push_str(&copy_as(&options, &block, &style));
        }
        _ => {}
    }
    let keys = json_keys(block.names.iter().copied());
    let total = rows.len();
    // No rows still copy the page's CSV header; JSON's empty array is compact.
    if total == 0 {
        return Ok(Some((copy_as(&options, &block, &style), 0)));
    }
    progress(Progress { rows: 0, total });
    let mut reported = Instant::now();
    let mut values = 0;
    for (index, position) in rows.enumerate() {
        checkpoint(cancel, started)?;
        let Some(row) = request.row(position) else {
            continue;
        };
        block.rows.clear();
        // Read one row at a time; no table of borrowed cells proportional
        // to the selection and no second complete encoded string.
        let mut cells = Vec::with_capacity(block.names.len());
        for col in cols.clone() {
            checkpoint(cancel, started)?;
            cells.push(request.result.value(row, col));
        }
        block.rows.push(cells);
        match options.format {
            CopyFormat::Text | CopyFormat::Csv => {
                if index > 0 || options.header {
                    out.push('\n');
                }
                let no_header = CopyOptions {
                    header: false,
                    ..options
                };
                // A one-cell selection is exact text, but a one-column
                // multi-row selection must still quote embedded newlines.
                if options.format == CopyFormat::Text && (count != 1 || options.header) {
                    let null = null_text(&options);
                    for (col, value) in block.rows[0].iter().enumerate() {
                        if col > 0 {
                            out.push('\t');
                        }
                        match (value, null) {
                            (None, Some(null)) => out.push_str(null),
                            (Some(value), _) if value.contains(['\t', '\n', '\r', '"']) => {
                                out.push_str(&cell_format::csv_field(Some(value)))
                            }
                            (Some(value), _) => out.push_str(value),
                            (None, _) => {}
                        }
                    }
                } else {
                    out.push_str(&copy_as(&no_header, &block, &style));
                }
            }
            CopyFormat::Json | CopyFormat::JsonLines => {
                if index > 0 {
                    out.push_str(if options.format == CopyFormat::Json {
                        ",\n"
                    } else {
                        "\n"
                    });
                }
                if options.format == CopyFormat::Json {
                    out.push_str("  ");
                }
                out.push_str(&json_object(&keys, &block.families, &block.rows[0]));
            }
            CopyFormat::InList => {
                for (value, family) in block.rows[0].iter().zip(&block.families) {
                    checkpoint(cancel, started)?;
                    if let Some(value) = value {
                        let value = literal(Some(value), *family, style.dialect);
                        if seen.insert(value.clone()) {
                            if values > 0 {
                                out.push_str(", ");
                            }
                            out.push_str(&value);
                            values += 1;
                        }
                    }
                }
            }
            CopyFormat::InsertValues => {
                if index > 0 {
                    out.push_str(",\n");
                }
                out.push_str("  (");
                out.push_str(&row_literals(&block, &block.rows[0], style.dialect));
                out.push(')');
            }
            CopyFormat::Markdown => {
                out.push('\n');
                let row = markdown(&block);
                if let Some((_, row)) = row
                    .split_once('\n')
                    .and_then(|(_, rest)| rest.split_once('\n'))
                {
                    out.push_str(row);
                }
            }
            CopyFormat::Html => {
                let row = html(&block);
                if let Some((_, row)) = row.split_once("  <tbody>\n") {
                    out.push_str(row.strip_suffix("  </tbody>\n</table>").unwrap_or(row));
                }
            }
            CopyFormat::Insert | CopyFormat::ValueList => {
                if index > 0 {
                    out.push_str(if options.format == CopyFormat::Insert {
                        "\n"
                    } else {
                        ", "
                    });
                }
                out.push_str(&copy_as(&options, &block, &style));
            }
            CopyFormat::ColumnNames => {}
        }
        if reported.elapsed() >= Duration::from_millis(100) {
            progress(Progress {
                rows: index + 1,
                total,
            });
            reported = Instant::now();
        }
    }
    match options.format {
        CopyFormat::Json => out.push_str("\n]"),
        CopyFormat::InList => out.push(')'),
        CopyFormat::InsertValues => out.push(';'),
        CopyFormat::Html => out.push_str("  </tbody>\n</table>"),
        _ => {}
    }
    checkpoint(cancel, started)?;
    progress(Progress { rows: total, total });
    checkpoint(cancel, started)?;
    Ok(Some((out, count)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grid::{ColumnSort, GridKey};
    use crate::result::ResultColumn;

    fn fixture() -> Arc<ResultData> {
        Arc::new(ResultData::from_rows(
            vec![
                ResultColumn::new("id", "numeric"),
                ResultColumn::new("text", "text"),
                ResultColumn::new("flag", "bool"),
            ],
            [
                [
                    Some("1.500000000000000001"),
                    Some("a\t\"b\"\nc|<>&\\"),
                    Some("t"),
                ],
                [Some("2"), Some(""), Some("f")],
                [Some("1.500000000000000001"), Some("NULL"), None],
                [None, Some("O'Brien 😀"), Some("true")],
            ],
        ))
    }

    fn request(result: Arc<ResultData>, options: CopyOptions) -> CopyRequest {
        CopyRequest {
            quoted_columns: result
                .columns()
                .iter()
                .map(|column| format!("\"{}\"", column.name))
                .collect(),
            result,
            options,
            source: CopySource::Page,
            dialect: Some(SqlDialect::Postgres),
            table: "public.test".to_owned(),
        }
    }

    #[test]
    fn every_format_matches_the_existing_rules_in_sorted_selections() {
        for dialect in [
            None,
            Some(SqlDialect::Postgres),
            Some(SqlDialect::MySql),
            Some(SqlDialect::DatabricksSql),
        ] {
            for format in CopyFormat::ALL {
                for header in [false, true] {
                    for null in [NullText::Empty, NullText::Null] {
                        for separator in [Separator::Comma, Separator::Semicolon, Separator::Tab] {
                            for quoting in [Quoting::All, Quoting::Needed] {
                                let result = fixture();
                                let view = Arc::new(GridView::new(
                                    &result,
                                    GridKey {
                                        sort: Some(ColumnSort {
                                            column: 0,
                                            descending: true,
                                        }),
                                        ..Default::default()
                                    },
                                ));
                                let selection =
                                    CellSelection::all(&view.rows, result.column_count()).unwrap();
                                let mut request = request(
                                    result,
                                    CopyOptions {
                                        format,
                                        header,
                                        null,
                                        separator,
                                        quoting,
                                    },
                                );
                                request.dialect = dialect;
                                request.source = CopySource::Selection {
                                    view: view.clone(),
                                    selection,
                                };
                                let block = Block::new(
                                    &request.result,
                                    &view.rows,
                                    Some(selection),
                                    &view.families,
                                )
                                .unwrap();
                                let quote = |name: &str| format!("\"{name}\"");
                                let style = SqlStyle {
                                    dialect,
                                    table: &request.table,
                                    quote_name: &quote,
                                };
                                let expected = copy_as(&request.options, &block, &style);
                                let (actual, cells) =
                                    generate(&request, &CancellationToken::new(), |_| {})
                                        .unwrap()
                                        .unwrap();
                                assert_eq!(
                                    actual, expected,
                                    "{format:?}, {header}, {null:?}, {separator:?}, {quoting:?}"
                                );
                                assert_eq!(cells, block.cells());
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn single_cells_and_one_column_blocks_keep_exact_text_and_null_rules() {
        let result = fixture();
        let view = Arc::new(GridView::new(&result, GridKey::default()));
        for (anchor, focus) in [
            ((0, 1), (0, 1)),
            ((1, 1), (1, 1)),
            ((3, 0), (3, 0)),
            ((0, 1), (3, 1)),
            ((1, 0), (3, 1)),
        ] {
            let selection = CellSelection { anchor, focus };
            for header in [false, true] {
                for null in [NullText::Empty, NullText::Null] {
                    let mut request = request(
                        result.clone(),
                        CopyOptions {
                            header,
                            null,
                            ..Default::default()
                        },
                    );
                    request.source = CopySource::Selection {
                        view: view.clone(),
                        selection,
                    };
                    let block =
                        Block::new(&result, &view.rows, Some(selection), &view.families).unwrap();
                    let quote = |name: &str| name.to_owned();
                    let expected = copy_as(
                        &request.options,
                        &block,
                        &SqlStyle {
                            dialect: None,
                            table: "",
                            quote_name: &quote,
                        },
                    );
                    assert_eq!(
                        generate(&request, &CancellationToken::new(), |_| {})
                            .unwrap()
                            .unwrap()
                            .0,
                        expected
                    );
                }
            }
        }
    }

    #[test]
    fn page_csv_ignores_display_order_and_keeps_empty_pages_headers() {
        let options = CopyOptions {
            format: CopyFormat::Csv,
            header: true,
            ..Default::default()
        };
        for result in [
            fixture(),
            Arc::new(ResultData::new(vec![ResultColumn::new("a\"b", "text")])),
        ] {
            let request = request(result.clone(), options);
            assert_eq!(
                generate(&request, &CancellationToken::new(), |_| {})
                    .unwrap()
                    .unwrap()
                    .0,
                cell_format::result_csv(&result)
            );
        }
    }

    #[test]
    fn a_report_panel_copy_for_a_spreadsheet_is_tab_separated_text_with_names() {
        // The panel's **Copy for a spreadsheet** is the page as text with the
        // column names, NULL as an empty field, whatever the copy menu says.
        let result = fixture();
        let request = request(
            result.clone(),
            CopyOptions {
                format: CopyFormat::Text,
                header: true,
                ..Default::default()
            },
        );
        let names: Vec<&str> = result.columns().iter().map(|c| c.name.as_str()).collect();
        let rows: Vec<Vec<Option<&str>>> = (0..result.row_count())
            .map(|row| result.row(row).collect())
            .collect();
        assert_eq!(
            generate(&request, &CancellationToken::new(), |_| {})
                .unwrap()
                .unwrap()
                .0,
            cell_format::copy_cells(Some(&names), &rows)
        );
    }

    #[test]
    fn a_value_is_copied_whole_and_exact_whatever_the_copy_settings() {
        // Past the 1 MiB the inspector shows, with characters other formats
        // would quote: the copy is the stored value, not what is shown.
        let long = format!("{}\t\"x\"\n{}", "a".repeat(3 * 1024 * 1024), "é😀");
        let result = Arc::new(ResultData::from_rows(
            vec![ResultColumn::new("doc", "text")],
            [[Some(long.as_str())], [None]],
        ));
        let mut request = request(
            result,
            CopyOptions {
                format: CopyFormat::Csv,
                header: true,
                null: NullText::Null,
                ..Default::default()
            },
        );
        request.source = CopySource::Value { row: 0, column: 0 };
        let cancel = CancellationToken::new();
        assert_eq!(request.estimate(&cancel).unwrap(), long.len());
        let mut reports = Vec::new();
        let (text, count) = generate(&request, &cancel, |progress| reports.push(progress))
            .unwrap()
            .unwrap();
        assert_eq!((text.as_str(), count), (long.as_str(), 1));
        assert_eq!(reports.last().map(|p| (p.rows, p.total)), Some((1, 1)));
        // NULL, or a cell the page no longer has, copies nothing.
        for (row, column) in [(1, 0), (5, 0), (0, 3)] {
            request.source = CopySource::Value { row, column };
            assert_eq!(request.estimate(&cancel).unwrap(), 0);
            assert!(generate(&request, &cancel, |_| {}).unwrap().is_none());
        }
        let cancelled = CancellationToken::new();
        cancelled.cancel();
        request.source = CopySource::Value { row: 0, column: 0 };
        assert_eq!(
            generate(&request, &cancelled, |_| {}).unwrap_err().code,
            ErrorCode::Cancelled
        );
    }

    #[test]
    fn hidden_selection_is_not_copied() {
        let result = fixture();
        let view = Arc::new(GridView::new(
            &result,
            GridKey {
                filter: "no match".to_owned(),
                ..Default::default()
            },
        ));
        let mut request = request(result, CopyOptions::default());
        request.source = CopySource::Selection {
            view,
            selection: CellSelection::cell(0, 0),
        };
        assert_eq!(request.estimate(&CancellationToken::new()).unwrap(), 0);
        assert!(
            generate(&request, &CancellationToken::new(), |_| {})
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn cancellation_before_estimate_and_during_encoding_returns_no_partial_copy() {
        let request = request(fixture(), CopyOptions::default());
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert_eq!(
            request.estimate(&cancel).unwrap_err().code,
            ErrorCode::Cancelled
        );
        assert_eq!(
            generate(&request, &cancel, |_| {}).unwrap_err().code,
            ErrorCode::Cancelled
        );
        let cancel = CancellationToken::new();
        assert_eq!(
            generate(&request, &cancel, |progress| {
                if progress.rows == progress.total {
                    cancel.cancel();
                }
            })
            .unwrap_err()
            .code,
            ErrorCode::Cancelled
        );
        assert_eq!(
            checkpoint(
                &CancellationToken::new(),
                Instant::now() - Duration::from_secs(61)
            )
            .unwrap_err()
            .code,
            ErrorCode::Timeout
        );
    }

    #[test]
    fn large_copy_estimates_confirmation_and_reports_progress_within_a_generous_budget() {
        let value = "x".repeat(600);
        let result = Arc::new(ResultData::from_rows(
            vec![ResultColumn::new("payload", "text")],
            (0..100_000).map(|_| [Some(value.as_str())]),
        ));
        let request = request(
            result,
            CopyOptions {
                format: CopyFormat::Csv,
                header: true,
                ..Default::default()
            },
        );
        let cancel = CancellationToken::new();
        assert!(request.estimate(&cancel).unwrap() > crate::grid::COPY_CONFIRM_BYTES);
        let started = Instant::now();
        let mut reports = Vec::new();
        let (text, count) = generate(&request, &cancel, |progress| reports.push(progress))
            .unwrap()
            .unwrap();
        assert_eq!(count, 100_000);
        assert!(text.len() > crate::grid::COPY_CONFIRM_BYTES);
        assert_eq!(reports.first().unwrap().rows, 0);
        assert_eq!(reports.last().unwrap().rows, count);
        assert!(reports.iter().all(|progress| progress.total == count));
        assert!(started.elapsed() < Duration::from_secs(10), "{started:?}");
    }
}
