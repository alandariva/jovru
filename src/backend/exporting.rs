//! Export of a result to a file, on the database runtime
//! ([Results](../../docs/reference/results.md#export)).
//!
//! The rows go to the file in parts: first the pages the tab already holds,
//! then the rest of the result, read from the session in parts of about
//! [`PART_BYTES`], or everything the statement returns when it runs again.
//! Each part is written on the runtime's blocking pool and dropped there, so
//! an export holds one part at a time and never touches the UI thread.
use super::{close_session, session_context, session_for_run, transaction_status};
use crate::drivers::*;
use crate::export::{ExportFile, ExportOptions, SqlTarget, Written};
use crate::result::{
    DEFAULT_MEMORY_CEILING, MAX_ROW_LIMIT, ResultBuilder, ResultColumn, ResultData, ResultLimit,
};
use crate::tr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::watch;
use tracing::Instrument as _;

/// How long writing one part to disk may take. A disk that stops answering
/// (a network share that went away) fails the export instead of holding it
/// ([Reliability](../../docs/standards/reliability.md#timeouts)).
pub const FILE_TIMEOUT: Duration = Duration::from_secs(60);

/// About how much one part read from the session holds.
const PART_BYTES: usize = 16 * 1024 * 1024;

/// The rows of the first part when nothing tells how large rows are.
const FIRST_PART_ROWS: u32 = 1_000;

/// Where the rows after the held pages come from.
pub enum ExportRead {
    /// The held pages are the whole result.
    Held,
    /// The rest of the result the tab's session keeps open. Reading it to
    /// the file uses it up: the grid can no longer page past what it holds.
    Rest {
        session: Box<dyn Session>,
        options: ExecutionOptions,
    },
    /// The statement runs again in the tab's session (opened when there is
    /// none) and everything it returns goes to the file; the held pages are
    /// not written.
    Again {
        input: ResolvedConnection,
        session: Option<Box<dyn Session>>,
        sql: String,
        /// The `:name` values bound when the result ran, bound again.
        parameters: Vec<QueryParameter>,
        options: ExecutionOptions,
    },
}

/// What to export and where.
pub struct ExportRequest {
    /// The file the user chose; it is replaced only once the export ends.
    pub target: PathBuf,
    pub options: ExportOptions,
    /// The result's columns, as its first page has them.
    pub columns: Vec<ResultColumn>,
    /// The result columns the file holds, in order: the grid's shown
    /// columns in display order. `None` writes every column in the
    /// result's order.
    pub shown: Option<Vec<usize>>,
    pub sql: SqlTarget,
    /// The pages the tab holds, from the first row of the result, in order.
    pub pages: Vec<Arc<ResultData>>,
    pub read: ExportRead,
}

/// What the file holds so far, sent after every part.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ExportProgress {
    pub rows: u64,
    pub bytes: u64,
}

/// A finished export.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Exported {
    pub rows: u64,
    /// The size of the file.
    pub bytes: u64,
    pub elapsed_ms: u128,
    /// The driver stopped the result before its end (a server-side cap): the
    /// file holds every row the server returned, not the whole result.
    pub truncated: bool,
}

/// What an export leaves behind.
pub struct ExportRun {
    pub outcome: DriverResult<Exported>,
    /// The tab's session, unless it was lost; `None` also when the export
    /// had no session.
    pub session: Option<Box<dyn Session>>,
    /// Where unqualified names resolve, asked once the result was read to
    /// its end.
    pub context: Option<SessionContext>,
    /// The session's transaction state, asked when `context` is.
    pub transaction: Option<TransactionStatus>,
    /// The tab's session no longer answered and the statement ran in a new
    /// one ([`ExportRead::Again`]).
    pub reopened: bool,
    /// The session was lost during the export.
    pub ended: bool,
}

/// Writes the result of `request` to its file. The file appears only when
/// every row is in it; a failure or a cancellation leaves the destination as
/// it was. The export is logged at `info` with its format, rows, bytes and
/// duration, never the path.
pub async fn export(
    request: ExportRequest,
    cancel: CancellationToken,
    progress: Option<watch::Sender<ExportProgress>>,
) -> ExportRun {
    export_with(None, request, DEFAULT_MEMORY_CEILING, cancel, progress).await
}

/// [`export`] with a given driver for [`ExportRead::Again`] (the registry's
/// when `None`) and a given memory ceiling per part.
pub async fn export_with(
    driver: Option<Arc<dyn Driver>>,
    request: ExportRequest,
    memory_ceiling: usize,
    cancel: CancellationToken,
    progress: Option<watch::Sender<ExportProgress>>,
) -> ExportRun {
    let span = crate::client_span!(
        "export",
        format = ?request.options.format,
        jykre.export.format = format!("{:?}", request.options.format).to_lowercase(),
        jykre.export.rows = tracing::field::Empty,
        jykre.export.truncated = tracing::field::Empty,
        jykre.bytes = tracing::field::Empty
    );
    let started = Instant::now();
    let mut run = ExportRun {
        outcome: Err(DriverError::cancelled()),
        session: None,
        context: None,
        transaction: None,
        reopened: false,
        ended: false,
    };
    let outcome = write(
        driver,
        request,
        memory_ceiling,
        &cancel,
        progress.as_ref(),
        &mut run,
    )
    .instrument(span.clone())
    .await;
    match &outcome {
        Ok(exported) => {
            span.record(
                "jykre.export.rows",
                crate::telemetry::attributes::count(exported.rows),
            );
            span.record(
                "jykre.bytes",
                crate::telemetry::attributes::count(exported.bytes),
            );
            span.record("jykre.export.truncated", exported.truncated);
        }
        Err(error) => crate::telemetry::attributes::record_error(&span, error),
    }
    let _entered = span.enter();
    let elapsed_ms = started.elapsed().as_millis();
    match &outcome {
        Ok(exported) => tracing::info!(
            rows = exported.rows,
            bytes = exported.bytes,
            truncated = exported.truncated,
            elapsed_ms = elapsed_ms as u64,
            "export finished"
        ),
        Err(error) => tracing::info!(
            error_code = ?error.code,
            elapsed_ms = elapsed_ms as u64,
            "export failed"
        ),
    }
    run.outcome = outcome.map(|exported| Exported {
        elapsed_ms,
        ..exported
    });
    run
}

/// The whole export. The session, however the export ends, is left in
/// `run` for the tab, unless it was lost.
async fn write(
    driver: Option<Arc<dyn Driver>>,
    request: ExportRequest,
    memory_ceiling: usize,
    cancel: &CancellationToken,
    progress: Option<&watch::Sender<ExportProgress>>,
    run: &mut ExportRun,
) -> DriverResult<Exported> {
    let ExportRequest {
        target,
        options,
        columns,
        shown,
        sql,
        pages,
        read,
    } = request;
    let created = {
        let columns = columns.clone();
        on_disk(move || {
            ExportFile::create_shown(&target, &options, &columns, shown.as_deref(), &sql)
        })
        .await
    };
    let file = match created {
        Ok(file) => file,
        Err(error) => {
            // Nothing was read: the session goes back as it was.
            run.session = match read {
                ExportRead::Held => None,
                ExportRead::Rest { session, .. } => Some(session),
                ExportRead::Again { session, .. } => session,
            };
            return Err(error);
        }
    };
    let mut parts = Parts {
        file: Some(file),
        columns,
        memory_ceiling,
        cancel,
        progress,
    };
    let truncated = match read {
        ExportRead::Held => {
            parts.held(pages).await?;
            false
        }
        ExportRead::Rest { session, options } => {
            let rows = pages
                .last()
                .map_or(FIRST_PART_ROWS, |page| rows_per_part(page, FIRST_PART_ROWS));
            let session = run.session.insert(session);
            let read = match parts.held(pages).await {
                Ok(()) => parts.rest(&mut **session, &options, rows).await,
                Err(error) => Err(error),
            };
            finish_session(run, read.is_ok(), cancel).await;
            read?
        }
        ExportRead::Again {
            input,
            session,
            sql,
            parameters,
            options,
        } => {
            drop(pages);
            let driver = match driver.map_or_else(
                || builtin_registry().and_then(|registry| registry.get(input.config.driver)),
                Ok,
            ) {
                Ok(driver) => driver,
                Err(error) => {
                    run.session = session;
                    return Err(error);
                }
            };
            // Only a read runs again, so a lost session is replaced; the tab
            // says whether a transaction went with it.
            let (session, reopened) =
                session_for_run(&*driver, &input, session, cancel, true).await;
            run.reopened = reopened;
            let session = run.session.insert(session?);
            let read = parts
                .again(&mut **session, &sql, parameters, &options)
                .await;
            finish_session(run, read.is_ok(), cancel).await;
            read?
        }
    };
    let written = parts.finish().await?;
    Ok(Exported {
        rows: written.rows,
        bytes: written.bytes,
        elapsed_ms: 0,
        truncated,
    })
}

/// After reading from the session: asks where names resolve and the
/// transaction state once the result was read to its end, and gives up a
/// session that was lost.
async fn finish_session(run: &mut ExportRun, read_to_end: bool, cancel: &CancellationToken) {
    let Some(mut session) = run.session.take() else {
        return;
    };
    if session.is_open() && read_to_end {
        run.context = session_context(&mut *session, cancel).await;
        run.transaction = transaction_status(&mut *session, cancel).await;
    }
    if session.is_open() {
        run.session = Some(session);
    } else {
        run.ended = true;
        close_session(Some(session)).await;
    }
}

/// The file being written and what writing a part needs.
struct Parts<'a> {
    /// Out on the blocking pool while a part is written.
    file: Option<ExportFile>,
    columns: Vec<ResultColumn>,
    memory_ceiling: usize,
    cancel: &'a CancellationToken,
    progress: Option<&'a watch::Sender<ExportProgress>>,
}

impl Parts<'_> {
    /// Writes the pages the tab holds.
    async fn held(&mut self, pages: Vec<Arc<ResultData>>) -> DriverResult<()> {
        for page in pages {
            self.write(page).await?;
        }
        Ok(())
    }

    /// Reads the rest of the open result from `session`, a part at a time,
    /// starting with parts of `rows`. Returns whether the driver stopped the
    /// result before its end.
    async fn rest(
        &mut self,
        session: &mut dyn Session,
        options: &ExecutionOptions,
        mut rows: u32,
    ) -> DriverResult<bool> {
        loop {
            if self.cancel.is_cancelled() {
                return Err(DriverError::cancelled());
            }
            let part = ExecutionOptions {
                max_rows: rows,
                ..options.clone()
            };
            let mut builder = ResultBuilder::page(self.columns.clone(), rows, self.memory_ceiling);
            let outcome = session
                .fetch(&part, &mut builder, self.cancel.clone())
                .await?;
            let data = self.part(builder)?;
            rows = rows_per_part(&data, rows);
            self.write(Arc::new(data)).await?;
            if !outcome.has_more {
                return Ok(outcome.truncated);
            }
        }
    }

    /// Runs `sql` again in `session` and reads all it returns. Returns
    /// whether the driver stopped the result before its end.
    async fn again(
        &mut self,
        session: &mut dyn Session,
        sql: &str,
        parameters: Vec<QueryParameter>,
        options: &ExecutionOptions,
    ) -> DriverResult<bool> {
        if self.cancel.is_cancelled() {
            return Err(DriverError::cancelled());
        }
        let request = QueryRequest {
            parameters,
            sql: sql.to_owned(),
            options: ExecutionOptions {
                max_rows: FIRST_PART_ROWS,
                ..options.clone()
            },
        };
        let mut builder = ResultBuilder::new(FIRST_PART_ROWS, self.memory_ceiling);
        let outcome = session
            .execute(&request, &mut builder, self.cancel.clone())
            .await?;
        let data = self.part(builder)?;
        let same = data.column_count() == self.columns.len()
            && data
                .columns()
                .iter()
                .zip(&self.columns)
                .all(|(now, then)| now.name == then.name);
        if !same {
            return Err(DriverError::validation(tr!(
                "The statement now returns other columns than when it ran. Run it again, then export."
            )));
        }
        let rows = rows_per_part(&data, FIRST_PART_ROWS);
        self.write(Arc::new(data)).await?;
        if outcome.has_more {
            self.rest(session, options, rows).await
        } else {
            Ok(outcome.truncated)
        }
    }

    /// A part read into `builder`. One that reached the memory ceiling left
    /// rows out, so the export cannot go on.
    fn part(&self, builder: ResultBuilder) -> DriverResult<ResultData> {
        let (data, limit) = builder.finish();
        if let Some(ResultLimit::Memory(bytes)) = limit {
            return Err(DriverError::validation(tr!(
                "Some rows are much larger than the ones before them: one part of the export went over {0} MB. Export fewer columns or filter the rows.",
                bytes / (1024 * 1024)
            )));
        }
        Ok(data)
    }

    /// Writes one part on the blocking pool, where a part read for the
    /// export is also dropped, and sends the progress.
    async fn write(&mut self, data: Arc<ResultData>) -> DriverResult<()> {
        if self.cancel.is_cancelled() {
            return Err(DriverError::cancelled());
        }
        let Some(mut file) = self.file.take() else {
            return Err(gone());
        };
        let file = on_disk(move || {
            file.write(&data)?;
            Ok(file)
        })
        .await?;
        if let Some(progress) = self.progress {
            progress.send_replace(ExportProgress {
                rows: file.rows(),
                bytes: file.bytes(),
            });
        }
        self.file = Some(file);
        Ok(())
    }

    async fn finish(mut self) -> DriverResult<Written> {
        if self.cancel.is_cancelled() {
            return Err(DriverError::cancelled());
        }
        let file = self.file.take().ok_or_else(gone)?;
        on_disk(move || file.finish()).await
    }
}

fn gone() -> DriverError {
    DriverError::new(ErrorCode::Internal, tr!("The export stopped unexpectedly."))
}

/// How many rows the next part reads for it to hold about [`PART_BYTES`],
/// judged by the rows of `data`; `current` when `data` has none.
fn rows_per_part(data: &ResultData, current: u32) -> u32 {
    let rows = data.row_count();
    if rows == 0 {
        return current;
    }
    let per_row = (data.memory_bytes() / rows).max(1);
    u32::try_from((PART_BYTES / per_row).clamp(1, MAX_ROW_LIMIT as usize)).unwrap_or(MAX_ROW_LIMIT)
}

/// Runs disk work on the blocking pool, within [`FILE_TIMEOUT`]. A write
/// that outlives it goes on in the background and drops what it holds when
/// it ends, which removes the temporary file; the export fails at once.
async fn on_disk<T: Send + 'static>(
    work: impl FnOnce() -> DriverResult<T> + Send + 'static,
) -> DriverResult<T> {
    match tokio::time::timeout(FILE_TIMEOUT, tokio::task::spawn_blocking(work)).await {
        Ok(Ok(result)) => result,
        Ok(Err(error)) => {
            tracing::error!(panicked = error.is_panic(), "export write failed");
            Err(gone())
        }
        Err(_) => Err(DriverError::new(
            ErrorCode::Timeout,
            tr!(
                "Writing the file took longer than {0} s. Check the disk or choose another place.",
                FILE_TIMEOUT.as_secs()
            ),
        )),
    }
}

#[cfg(test)]
#[path = "exporting_tests.rs"]
mod tests;
