//! Batches, the bounded channel between reading and writing, and an import
//! run all or nothing
//! ([ADR-0040](../../docs/adr/0040-importing-files-into-a-table.md#1-where-it-lives)).
//!
//! Reading and decoding run on the runtime's blocking pool
//! ([`read_batches`]); writing runs on the runtime through a
//! [`BatchWriter`]. A channel of [`CHANNEL_BATCHES`] batches joins them, so a
//! slow database slows reading down and memory stays at about two batches
//! plus the reader's buffers. A batch is at most [`BATCH_ROWS`] rows or
//! [`BATCH_BYTES`] of values, whichever comes first.
//!
//! [`run`] opens the writer's transaction, writes every batch into it and
//! commits once the file is read to its end; a failure, a cancellation or
//! the time limit rolls it back, so the table is as it was. Nothing is ever
//! retried. The writer is the seam ADR-0039's write path fills
//! (`Session::begin_writes`, `write` with `WriteScope::Caller`,
//! `end_writes`), which this part of A11 does not have yet.
use super::mapping::Mapping;
use super::{Flow, ImportValue, ReadEvent, ReadOptions, read};
use crate::drivers::{CancellationToken, DriverError, DriverResult, ErrorCode, async_trait};
use crate::tr;
use std::path::PathBuf;
use std::time::Duration;
use tokio::sync::{mpsc, watch};

/// Rows a batch holds at most.
pub const BATCH_ROWS: usize = 1_000;
/// Bytes of values a batch holds at most, each value counted with its
/// in-memory overhead.
pub const BATCH_BYTES: usize = 8 * 1024 * 1024;
/// Batches the channel between the reader and the writer holds.
pub const CHANNEL_BATCHES: usize = 2;
/// How long a cancelled writer has to come back before the rollback.
const CANCEL_GRACE: Duration = Duration::from_secs(10);

/// Rows of the file, mapped to the table's columns, written together.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Batch {
    /// One row per entry, with one value per [`Mapping::columns`].
    pub rows: Vec<Vec<ImportValue>>,
    /// The file's line (or sheet row) of each row, so a value the database
    /// rejects is named by where it is in the file.
    pub lines: Vec<u64>,
    /// The bytes the rows hold, as [`BATCH_BYTES`] counts them.
    pub bytes: usize,
}

/// Cuts rows into batches of [`BATCH_ROWS`] rows or [`BATCH_BYTES`] bytes.
#[derive(Debug)]
pub struct Batcher {
    max_rows: usize,
    max_bytes: usize,
    current: Batch,
}

impl Default for Batcher {
    fn default() -> Self {
        Self::new(BATCH_ROWS, BATCH_BYTES)
    }
}

impl Batcher {
    pub fn new(max_rows: usize, max_bytes: usize) -> Self {
        Self {
            max_rows: max_rows.max(1),
            max_bytes,
            current: Batch::default(),
        }
    }

    /// Adds the row read at `line`; returns the batch it completed.
    pub fn push(&mut self, line: u64, row: Vec<ImportValue>) -> Option<Batch> {
        let overhead = std::mem::size_of::<ImportValue>();
        self.current.bytes += row
            .iter()
            .map(|value| value.size() + overhead)
            .sum::<usize>();
        self.current.rows.push(row);
        self.current.lines.push(line);
        if self.current.rows.len() >= self.max_rows || self.current.bytes >= self.max_bytes {
            return Some(std::mem::take(&mut self.current));
        }
        None
    }

    /// The last batch, unless it is empty.
    pub fn finish(self) -> Option<Batch> {
        (!self.current.rows.is_empty()).then_some(self.current)
    }
}

/// What an import reads and where its values go.
#[derive(Clone, Debug, PartialEq)]
pub struct ImportPlan {
    pub path: PathBuf,
    pub options: ReadOptions,
    pub mapping: Mapping,
}

/// How far an import got, published as it goes.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Progress {
    /// Rows read from the file and batched.
    pub rows_read: u64,
    /// Rows the writer wrote (uncommitted until the end).
    pub rows_written: u64,
    /// The share of the file read, from 0 to 1, when it can be told.
    pub read: Option<f64>,
}

/// How long an import runs before its progress shows, and before its
/// elapsed time and **Cancel** do ([User experience](../../docs/reference/ux.md#always-visible-states)).
pub const PROGRESS_AFTER: Duration = Duration::from_millis(300);
pub const CANCEL_AFTER: Duration = Duration::from_secs(2);

/// What a running import shows after `elapsed`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Shown {
    /// Nothing yet: a short import never flickers.
    Nothing,
    /// The rows written and the share of the file read.
    Progress,
    /// The progress, the elapsed time and **Cancel**.
    ProgressAndCancel,
}

impl Shown {
    pub fn after(elapsed: Duration) -> Self {
        if elapsed >= CANCEL_AFTER {
            Self::ProgressAndCancel
        } else if elapsed >= PROGRESS_AFTER {
            Self::Progress
        } else {
            Self::Nothing
        }
    }
}

/// Reads the plan's file into batches of mapped rows and sends them, in
/// order, through `sender`, publishing the rows read in `progress`. Waits
/// whenever the channel is full, so it reads no faster than the writer
/// writes. Refuses a file whose columns no longer fit the mapping. Stops
/// with the writer: when `cancel` fires or the receiver is gone. Returns the
/// rows read. Blocks: run it on the blocking pool, never inside an async
/// task (`blocking_send` would panic there).
pub fn read_batches(
    plan: &ImportPlan,
    sender: &mpsc::Sender<Batch>,
    progress: &watch::Sender<Progress>,
    cancel: &CancellationToken,
) -> DriverResult<u64> {
    let mut batcher = Batcher::default();
    let mut rows: u64 = 0;
    let send = |batch: Batch| {
        sender
            .blocking_send(batch)
            .map_err(|_| DriverError::cancelled())
    };
    read(&plan.path, &plan.options, cancel, &mut |event| {
        match event {
            ReadEvent::Columns(columns) => {
                if let Some(problem) = plan.mapping.problems(columns.len()).first() {
                    return Err(DriverError::validation(problem.message()));
                }
            }
            ReadEvent::Row(record) => {
                rows += 1;
                let line = record.line;
                let read = record.read;
                if let Some(batch) = batcher.push(line, plan.mapping.project(record.values)) {
                    send(batch)?;
                }
                progress.send_modify(|progress| {
                    progress.rows_read = rows;
                    progress.read = read.or(progress.read);
                });
            }
        }
        if cancel.is_cancelled() {
            return Err(DriverError::cancelled());
        }
        Ok(Flow::Continue)
    })?;
    if let Some(batch) = batcher.finish() {
        send(batch)?;
    }
    Ok(rows)
}

/// Where an import's rows go: ADR-0039's write path in the import's own
/// session (ADR-0040, section 4). Every call may be cancelled through its
/// token, which interrupts the statement as for a run.
#[async_trait]
pub trait BatchWriter: Send {
    /// Opens the transaction the batches join (`Session::begin_writes`).
    async fn begin(&mut self, cancel: CancellationToken) -> DriverResult<()>;

    /// Inserts the batch's rows into the mapped columns inside that
    /// transaction (`write` with `WriteScope::Caller`), values bound by the
    /// driver. Returns the rows written, which must be the rows sent. A
    /// value the database rejects fails with the file's line
    /// ([`Batch::lines`]) and the server's message.
    async fn write(&mut self, batch: &Batch, cancel: CancellationToken) -> DriverResult<u64>;

    /// Commits (`commit = true`) or rolls back the transaction
    /// (`Session::end_writes`).
    async fn end(&mut self, commit: bool, cancel: CancellationToken) -> DriverResult<()>;
}

/// The whole import's time limit, chosen in the dialog like a tab's
/// options. Each batch also has the connection's statement timeout.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum TimeLimit {
    FifteenMinutes,
    #[default]
    OneHour,
    FourHours,
    None,
}

impl TimeLimit {
    pub const ALL: [Self; 4] = [
        Self::FifteenMinutes,
        Self::OneHour,
        Self::FourHours,
        Self::None,
    ];

    pub fn duration(self) -> Option<Duration> {
        match self {
            Self::FifteenMinutes => Some(Duration::from_secs(15 * 60)),
            Self::OneHour => Some(Duration::from_secs(60 * 60)),
            Self::FourHours => Some(Duration::from_secs(4 * 60 * 60)),
            Self::None => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::FifteenMinutes => tr!("15 min"),
            Self::OneHour => tr!("1 h"),
            Self::FourHours => tr!("4 h"),
            Self::None => tr!("No limit"),
        }
    }
}

/// How an import ended, which the dialog says in words.
#[derive(Clone, Debug, PartialEq)]
pub enum Outcome {
    /// Every row was written and committed.
    Imported { rows: u64 },
    /// The transaction was rolled back (or its session lost before the
    /// commit, which rolls it back too): the table is as it was. `reason`
    /// is the failure, the cancellation or the time limit.
    NothingImported { reason: DriverError },
    /// The connection was lost while committing: the rows may or may not be
    /// in the table.
    Unknown { error: DriverError },
}

impl Outcome {
    /// What the dialog says, naming the table as `table` (its qualified
    /// name).
    pub fn message(&self, table: &str) -> String {
        match self {
            Self::Imported { rows } => tr!("Imported {0} rows into {1}.", rows, table),
            Self::NothingImported { .. } => {
                tr!("Nothing was imported: {0} is as it was.", table)
            }
            Self::Unknown { .. } => tr!(
                "The connection was lost while committing: the rows may or may not have been imported. Check the table before importing again."
            )
            .to_owned(),
        }
    }
}

/// Runs the import of `plan` into `writer`, all or nothing (decision I-1):
/// begins, writes every batch as the reader sends it, and commits once the
/// file is read to its end. A failure, `cancel` or `limit` (the chosen
/// [`TimeLimit::duration`]) rolls back. The rollback and the commit run
/// with tokens of their own, so a cancellation cannot leave them half done.
/// Must run inside the runtime.
pub async fn run(
    plan: ImportPlan,
    writer: &mut dyn BatchWriter,
    limit: Option<Duration>,
    cancel: CancellationToken,
    progress: watch::Sender<Progress>,
) -> Outcome {
    let work = cancel.child_token();
    let (sender, mut receiver) = mpsc::channel(CHANNEL_BATCHES);
    let reader = {
        let work = work.clone();
        let progress = progress.clone();
        tokio::task::spawn_blocking(move || read_batches(&plan, &sender, &progress, &work))
    };
    let writing = async {
        writer.begin(work.clone()).await?;
        let mut written: u64 = 0;
        while let Some(batch) = receiver.recv().await {
            let sent = batch.rows.len() as u64;
            let wrote = writer.write(&batch, work.clone()).await?;
            if wrote != sent {
                return Err(DriverError::new(
                    ErrorCode::Internal,
                    tr!(
                        "The database reported {0} rows written for {1} rows sent.",
                        wrote,
                        sent
                    ),
                ));
            }
            written += wrote;
            progress.send_modify(|progress| progress.rows_written = written);
        }
        Ok::<u64, DriverError>(written)
    };
    let deadline = async {
        match limit {
            Some(duration) => tokio::time::sleep(duration).await,
            None => std::future::pending().await,
        }
    };
    let written = {
        let mut writing = std::pin::pin!(writing);
        let stopped = tokio::select! {
            result = &mut writing => Ok(result),
            () = cancel.cancelled() => Err(DriverError::cancelled()),
            () = deadline => Err(DriverError::new(
                ErrorCode::Timeout,
                tr!("The import passed its time limit."),
            )),
        };
        match stopped {
            Ok(result) => result,
            Err(reason) => {
                // The writer interrupts its statement when the token fires;
                // it gets a moment to bring its session back to a known
                // state before the rollback.
                work.cancel();
                let _ = tokio::time::timeout(CANCEL_GRACE, &mut writing).await;
                Err(reason)
            }
        }
    };
    // The reader stops with the writer: on a failure the token and the
    // closed channel end it; otherwise it has already sent its last batch.
    if written.is_err() {
        work.cancel();
    }
    drop(receiver);
    let read = match reader.await {
        Ok(read) => read,
        Err(_) => Err(DriverError::new(
            ErrorCode::Internal,
            tr!("Reading the file stopped unexpectedly."),
        )),
    };
    let failure = match (written, read) {
        (Err(error), _) | (Ok(_), Err(error)) => Some(error),
        (Ok(_), Ok(_)) => None,
    };
    if let Some(reason) = failure {
        // A rollback that fails leaves a session the writer closes, which
        // rolls the transaction back as well.
        let _ = writer.end(false, CancellationToken::new()).await;
        return Outcome::NothingImported { reason };
    }
    let rows = progress.borrow().rows_written;
    match writer.end(true, CancellationToken::new()).await {
        Ok(()) => Outcome::Imported { rows },
        Err(error) if matches!(error.code, ErrorCode::Connection | ErrorCode::Timeout) => {
            Outcome::Unknown { error }
        }
        // The database refused the commit (a deferred constraint): it
        // rolled the transaction back.
        Err(reason) => Outcome::NothingImported { reason },
    }
}
