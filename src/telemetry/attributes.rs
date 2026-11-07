//! What a span may carry when it leaves this computer
//! ([ADR-0029](../../docs/adr/0029-otlp-export-of-logs-and-traces.md)): an
//! allowlist of OpenTelemetry semantic-convention names, plus a few of
//! Jykre's own under `jykre.`. Every other field a span records stays in the
//! local log and is removed before the span is exported, so a field added
//! by mistake (SQL text, a host, a path) never reaches a collector.
use crate::drivers::{DriverError, DriverKind, DriverResult, ErrorCode, SqlDialect};
use crate::sql::{TokenKind, tokens};

/// The span attributes that are exported. `otel.name`, `otel.kind` and
/// `otel.status_code` are not attributes: the bridge turns them into the
/// span's name, kind and status.
pub const ALLOWED: &[&str] = &[
    // Any operation.
    "error.type",
    // Databases.
    "db.system.name",
    "db.namespace",
    "db.operation.name",
    "db.response.status_code",
    "db.response.returned_rows",
    // HTTP.
    "http.request.method",
    "http.response.status_code",
    "http.request.resend_count",
    "url.template",
    // Processes.
    "process.executable.name",
    "process.exit.code",
    // AI providers.
    "gen_ai.operation.name",
    "gen_ai.provider.name",
    "gen_ai.request.model",
    "gen_ai.usage.input_tokens",
    "gen_ai.usage.output_tokens",
    // Jykre's own.
    "jykre.connection.id",
    "jykre.statements",
    "jykre.statements.completed",
    "jykre.rows.limit",
    "jykre.bytes",
    "jykre.session.reopened",
    "jykre.metadata.depth",
    "jykre.metadata.objects",
    "jykre.namespaces.refreshed",
    "jykre.namespaces.failed",
    "jykre.export.format",
    "jykre.export.rows",
    "jykre.export.truncated",
    "jykre.files",
    "jykre.cli.command",
    "jykre.vault.operation",
    "jykre.vault.item",
    "jykre.config.operation",
    "jykre.assistant.tool",
    "jykre.assistant.tool_calls",
];

/// Whether a span attribute is exported.
pub fn allowed(key: &str) -> bool {
    ALLOWED.contains(&key)
}

/// `db.system.name` of a driver.
pub fn db_system(kind: DriverKind) -> &'static str {
    match kind {
        DriverKind::Postgres => "postgresql",
        DriverKind::MySql => "mysql",
        DriverKind::Sqlite => "sqlite",
        DriverKind::Databricks => "databricks",
        _ => "other_sql",
    }
}

/// `error.type` of an error code: the code's own name, never its message.
pub fn error_type(code: ErrorCode) -> &'static str {
    match code {
        ErrorCode::Validation => "validation",
        ErrorCode::NotImplemented => "not_implemented",
        ErrorCode::DriverNotRegistered => "driver_not_registered",
        ErrorCode::DuplicateDriver => "duplicate_driver",
        ErrorCode::Cancelled => "cancelled",
        ErrorCode::Connection => "connection",
        ErrorCode::Database => "database",
        ErrorCode::NotFound => "not_found",
        ErrorCode::Protocol => "protocol",
        ErrorCode::Authentication => "authentication",
        ErrorCode::PermissionDenied => "permission_denied",
        ErrorCode::Timeout => "timeout",
        ErrorCode::Internal => "internal",
        _ => "other",
    }
}

/// The first words a statement may start with that are exported as
/// `db.operation.name`. Anything else is `OTHER`, so nothing the user typed
/// beyond one of these keywords leaves the computer.
const OPERATIONS: &[&str] = &[
    "SELECT",
    "INSERT",
    "UPDATE",
    "DELETE",
    "MERGE",
    "UPSERT",
    "REPLACE",
    "WITH",
    "VALUES",
    "TABLE",
    "CREATE",
    "ALTER",
    "DROP",
    "TRUNCATE",
    "RENAME",
    "COMMENT",
    "GRANT",
    "REVOKE",
    "BEGIN",
    "START",
    "COMMIT",
    "END",
    "ROLLBACK",
    "SAVEPOINT",
    "RELEASE",
    "SET",
    "RESET",
    "USE",
    "SHOW",
    "DESCRIBE",
    "DESC",
    "EXPLAIN",
    "CALL",
    "EXEC",
    "EXECUTE",
    "DO",
    "COPY",
    "LOCK",
    "UNLOCK",
    "ANALYZE",
    "ANALYSE",
    "VACUUM",
    "OPTIMIZE",
    "REFRESH",
    "CLUSTER",
    "REINDEX",
    "PRAGMA",
    "ATTACH",
    "DETACH",
    "DECLARE",
    "PREPARE",
    "DEALLOCATE",
    "LOAD",
    "CACHE",
    "UNCACHE",
    "MSCK",
    "LISTEN",
    "NOTIFY",
    "CHECKPOINT",
    "DISCARD",
    "KILL",
    "FLUSH",
    "HANDLER",
];

/// `db.operation.name` of a statement: its first keyword, upper case, when
/// it is one of a fixed list; `OTHER` otherwise. Comments are skipped.
pub fn operation_name(sql: &str, dialect: Option<SqlDialect>) -> &'static str {
    let first = tokens(sql, dialect)
        .into_iter()
        .find(|(_, kind)| *kind != TokenKind::Comment);
    match first {
        Some((range, TokenKind::Word)) => sql
            .get(range)
            .and_then(|word| {
                OPERATIONS
                    .iter()
                    .find(|operation| operation.eq_ignore_ascii_case(word))
            })
            .copied()
            .unwrap_or("OTHER"),
        _ => "OTHER",
    }
}

/// `db.namespace` of a session context: the catalog and the schema, or
/// whichever of them is known.
pub fn namespace(context: &crate::drivers::SessionContext) -> Option<String> {
    match (context.catalog.as_deref(), context.schema.as_deref()) {
        (Some(catalog), Some(schema)) => Some(format!("{catalog}.{schema}")),
        (Some(name), None) | (None, Some(name)) => Some(name.to_owned()),
        (None, None) => None,
    }
}

/// Records how an operation ended on its span: `error.type` and an error
/// status for a failure, only `error.type` for a cancellation. The span
/// must declare `error.type` and `otel.status_code` (as `Empty`).
pub fn record_error(span: &tracing::Span, error: &DriverError) {
    span.record("error.type", error_type(error.code));
    if error.code != ErrorCode::Cancelled {
        span.record("otel.status_code", "error");
    }
    if let Some(sqlstate) = error
        .server
        .as_ref()
        .and_then(|server| server.sqlstate.as_deref())
    {
        span.record("db.response.status_code", sqlstate);
    }
}

/// [`record_error`] for a result that failed; nothing for a success.
pub fn record_outcome<T>(span: &tracing::Span, result: &DriverResult<T>) {
    if let Err(error) = result {
        record_error(span, error);
    }
}

/// `db.namespace` of a metadata path: its names joined by dots (a catalog
/// and a schema, a database).
pub fn path_namespace(path: &crate::drivers::ObjectPath) -> String {
    path.0
        .iter()
        .map(|segment| segment.name.as_str())
        .collect::<Vec<_>>()
        .join(".")
}

/// A count or size as a span attribute. The bridge records unsigned
/// integers as text, so counts are recorded as `i64` (saturating).
pub fn count(value: impl TryInto<i64>) -> i64 {
    value.try_into().unwrap_or(i64::MAX)
}
