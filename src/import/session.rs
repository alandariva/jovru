//! An import's own session, and the writer that puts the batches into the
//! table through it ([ADR-0040](../../docs/adr/0040-importing-files-into-a-table.md#4-session-transaction-and-outcome)).
//!
//! The import opens a session of its own in the table's context, as a SQL
//! Job does, never a tab's. The driver describes the table there
//! ([`Session::table_target`]), the mapping is checked against that
//! description, and [`super::pipeline::run`] writes every batch inside one
//! transaction ([`Session::begin_writes`], [`WriteScope::Caller`],
//! [`Session::end_writes`]). Values are bound by the driver. Only the
//! outcome is logged: counts, the table's path, the time and the error's
//! code, never a value.
use super::ImportValue;
use super::mapping::{Mapping, TargetColumn};
use super::pipeline::{Batch, BatchWriter, ImportPlan, Outcome, Progress, run as run_pipeline};
use crate::drivers::write::{
    EditTarget, EditableTable, ReadOnlyReason, RowChange, WriteOutcome, WriteRequest, WriteScope,
    WriteValue,
};
use crate::drivers::{
    CancellationToken, DriverError, DriverResult, ErrorCode, ExecutionOptions, ObjectPath,
    ResolvedConnection, Session, async_trait, builtin_registry,
};
use crate::tr;
use std::time::{Duration, Instant};
use tokio::sync::watch;

/// How long opening the import's session may take.
const OPEN_TIMEOUT: Duration = Duration::from_secs(30);

/// The table's columns as the driver describes them: what the mapping is
/// checked against when the import starts. A column the database must be
/// given (NOT NULL without a default, writable) is required; one it
/// computes is generated.
pub fn target_columns(target: &EditableTable) -> Vec<TargetColumn> {
    target
        .columns
        .iter()
        .flatten()
        .map(|column| TargetColumn {
            name: column.name.clone(),
            data_type: Some(column.declared_type.clone()),
            required: !column.nullable && !column.has_default && column.writable,
            generated: !column.writable,
        })
        .collect()
}

/// Why rows cannot be imported into the table, in words.
pub fn read_only_text(reason: &ReadOnlyReason) -> String {
    match reason {
        ReadOnlyReason::NotATable { kind } => tr!(
            "Rows cannot be imported here: it is a {0}, not a table.",
            kind
        ),
        ReadOnlyReason::NotTransactional => tr!(
            "Rows cannot be imported into this table: its engine has no transactions, so a failed import could not be undone."
        )
        .to_owned(),
        ReadOnlyReason::TableNotFound { name } => tr!(
            "The database has no table {0}. Refresh the metadata and choose the table again.",
            name
        ),
        _ => tr!("This connection's driver cannot import rows.").to_owned(),
    }
}

/// The value a file's value is bound as.
fn write_value(value: ImportValue) -> WriteValue {
    match value {
        ImportValue::Null => WriteValue::Null,
        ImportValue::Text(text) => WriteValue::Text(text),
        ImportValue::Boolean(flag) => WriteValue::Boolean(flag),
    }
}

/// A [`BatchWriter`] over the import's own session: each batch is one
/// [`WriteRequest`] of inserts with [`WriteScope::Caller`].
pub struct SessionWriter {
    session: Box<dyn Session>,
    target: EditableTable,
    /// The table columns, by position in `target.columns`, that the
    /// batches' values fill, in order.
    columns: Vec<usize>,
    /// The statement timeout of each batch.
    timeout: Option<Duration>,
}

impl SessionWriter {
    pub fn new(
        session: Box<dyn Session>,
        target: EditableTable,
        columns: Vec<usize>,
        timeout: Option<Duration>,
    ) -> Self {
        Self {
            session,
            target,
            columns,
            timeout,
        }
    }

    /// The session, to close it.
    pub fn into_session(self) -> Box<dyn Session> {
        self.session
    }

    fn request(&self, batch: &Batch) -> WriteRequest {
        let changes = batch
            .rows
            .iter()
            .map(|row| RowChange::Insert {
                values: self
                    .columns
                    .iter()
                    .copied()
                    .zip(row.iter().cloned().map(write_value))
                    .collect(),
            })
            .collect();
        WriteRequest {
            target: self.target.clone(),
            changes,
            timeout: self.timeout,
            scope: WriteScope::Caller,
        }
    }
}

#[async_trait]
impl BatchWriter for SessionWriter {
    async fn begin(&mut self, cancel: CancellationToken) -> DriverResult<()> {
        self.session.begin_writes(cancel).await
    }

    /// A value the database rejects fails with the file's line.
    async fn write(&mut self, batch: &Batch, cancel: CancellationToken) -> DriverResult<u64> {
        let request = self.request(batch);
        match self.session.write(&request, cancel).await {
            Ok(WriteOutcome::Written { rows, .. }) => Ok(rows),
            Ok(WriteOutcome::RolledBack { index, matched }) => Err(DriverError::validation(tr!(
                "The row affected {0} rows of the table, not one.",
                matched
            ))
            .at_change(index)),
            Ok(_) => Err(DriverError::new(
                ErrorCode::Internal,
                tr!("The database answered the write in a way the import does not know."),
            )),
            Err(error) => Err(at_line(error, batch)),
        }
    }

    async fn end(&mut self, commit: bool, cancel: CancellationToken) -> DriverResult<()> {
        self.session.end_writes(commit, cancel).await
    }
}

/// `error` with the file's line of the change it happened at, when the
/// driver says which; the server's own message and diagnostic stay.
pub fn at_line(mut error: DriverError, batch: &Batch) -> DriverError {
    if let Some(line) = error
        .change()
        .and_then(|index| batch.lines.get(index))
        .copied()
    {
        error.message = tr!("Line {0} of the file: {1}", line, error.message);
    }
    error
}

/// What an import does.
#[derive(Clone, Debug)]
pub struct ImportJob {
    pub connection: ResolvedConnection,
    /// The table, as the metadata cache names it.
    pub table: ObjectPath,
    pub plan: ImportPlan,
    /// The whole import's time limit.
    pub limit: Option<Duration>,
}

/// Runs `job`: opens its own session in the table's context, reads the
/// table from the driver, checks the mapping against it, writes every row
/// all or nothing and closes the session. Cancelling or the time limit
/// rolls back. Must run inside the runtime.
pub async fn run(
    job: ImportJob,
    cancel: CancellationToken,
    progress: watch::Sender<Progress>,
) -> Outcome {
    let started = Instant::now();
    let table = crate::explorer::qualified_title(&job.table);
    let outcome = run_in(job, cancel, progress).await;
    let elapsed_ms = started.elapsed().as_millis();
    match &outcome {
        Outcome::Imported { rows } => {
            tracing::info!(rows, elapsed_ms, table = %table, "import committed");
        }
        Outcome::NothingImported { reason } => {
            tracing::info!(error_code = ?reason.code, elapsed_ms, table = %table, "import rolled back");
        }
        Outcome::Unknown { error } => {
            tracing::warn!(error_code = ?error.code, elapsed_ms, table = %table, "import commit unknown");
        }
    }
    outcome
}

async fn run_in(
    job: ImportJob,
    cancel: CancellationToken,
    progress: watch::Sender<Progress>,
) -> Outcome {
    let ImportJob {
        connection,
        table,
        mut plan,
        limit,
    } = job;
    let opened = open(&connection, &table, &cancel).await;
    let mut session = match opened {
        Ok(session) => session,
        Err(reason) => return Outcome::NothingImported { reason },
    };
    let prepared = prepare(session.as_mut(), &table, &plan.mapping, &cancel).await;
    let (target, mapping) = match prepared {
        Ok(prepared) => prepared,
        Err(reason) => {
            close(session).await;
            return Outcome::NothingImported { reason };
        }
    };
    let columns = mapping.mapped();
    plan.mapping = mapping;
    let mut writer = SessionWriter::new(
        session,
        target,
        columns,
        ExecutionOptions::default().timeout,
    );
    let outcome = run_pipeline(plan, &mut writer, limit, cancel, progress).await;
    close(writer.into_session()).await;
    outcome
}

/// The import's own session, in the context of the table's namespace.
async fn open(
    connection: &ResolvedConnection,
    table: &ObjectPath,
    cancel: &CancellationToken,
) -> DriverResult<Box<dyn Session>> {
    let driver = builtin_registry()?.get(connection.config.driver)?;
    let namespace = table.parent().unwrap_or_default();
    let context = crate::ddl::namespace_context(&namespace);
    let connection = driver.session_connection(connection, &context)?;
    match tokio::time::timeout(
        OPEN_TIMEOUT,
        driver.open_session(&connection, cancel.clone()),
    )
    .await
    {
        Ok(session) => session,
        Err(_) => Err(DriverError::new(
            ErrorCode::Timeout,
            tr!("The connection did not answer in time."),
        )),
    }
}

/// The table as the driver describes it now, and the mapping checked
/// against it.
async fn prepare(
    session: &mut dyn Session,
    table: &ObjectPath,
    mapping: &Mapping,
    cancel: &CancellationToken,
) -> DriverResult<(EditableTable, Mapping)> {
    let target = match session.table_target(table, cancel.clone()).await? {
        EditTarget::Editable(target) => target,
        EditTarget::ReadOnly(reason) => {
            return Err(DriverError::validation(read_only_text(&reason)));
        }
        _ => {
            return Err(DriverError::validation(tr!(
                "This connection's driver cannot import rows."
            )));
        }
    };
    let fresh = mapping
        .with_targets(target_columns(&target))
        .map_err(|problem| DriverError::validation(problem.message()))?;
    // The file's columns were checked with the preview; the reader checks
    // them again against the file as it is read.
    if let Some(problem) = fresh.problems(usize::MAX).first() {
        return Err(DriverError::validation(problem.message()));
    }
    Ok((target, fresh))
}

/// Closes the import's session; one that is gone has nothing to close, and
/// closing rolls back anything left open.
async fn close(session: Box<dyn Session>) {
    if let Err(error) = session.close().await {
        tracing::debug!(error_code = ?error.code, "import session closed with an error");
    }
}
