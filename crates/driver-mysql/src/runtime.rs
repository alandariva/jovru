use crate::MySqlConfig;
use futures_util::TryStreamExt;
use jykre_core::*;
use secrecy::ExposeSecret;
use sqlx::mysql::{MySqlConnectOptions, MySqlConnection, MySqlSslMode};
use sqlx::{
    Column, Connection, Executor, Row as SqlRow, SqlSafeStr, Statement as _, TypeInfo, ValueRef,
};
use std::{collections::HashSet, future::Future, time::Duration};

fn map_error(error: sqlx::Error) -> DriverError {
    if matches!(&error, sqlx::Error::Protocol(message) if message.contains("RSA auth backend disabled"))
    {
        return DriverError::new(
            ErrorCode::Authentication,
            "This MySQL authentication method requires TLS; select required or verify-identity",
        );
    }
    let code = match &error {
        sqlx::Error::Database(db) => match db.code().as_deref() {
            Some("28P01" | "28000" | "1045") => ErrorCode::Authentication,
            Some("42501" | "1142" | "1044") => ErrorCode::PermissionDenied,
            _ => ErrorCode::Database,
        },
        sqlx::Error::Io(_) | sqlx::Error::Tls(_) => ErrorCode::Connection,
        _ => ErrorCode::Protocol,
    };
    DriverError::new(
        code,
        "Database operation failed; check connectivity, permissions and SQL",
    )
}
async fn bounded<T>(
    cancel: &CancellationToken,
    timeout: Option<Duration>,
    work: impl Future<Output = DriverResult<T>>,
) -> DriverResult<T> {
    tokio::select! { biased;
    _ = cancel.cancelled() => Err(DriverError::cancelled()),
    result = async { match timeout { Some(limit) => tokio::time::timeout(limit, work).await.map_err(|_| DriverError::new(ErrorCode::Timeout,"Database operation timed out"))?, None => work.await } } => result }
}
async fn connect(
    input: &ResolvedConnection,
    cancel: &CancellationToken,
) -> DriverResult<MySqlConnection> {
    let config = MySqlConfig::parse(&input.config.options)?;
    let mut options = MySqlConnectOptions::new()
        .host(&config.host)
        .port(config.port)
        .username(&config.user)
        .ssl_mode(match config.ssl_mode {
            crate::MySqlSslMode::Disabled => MySqlSslMode::Disabled,
            crate::MySqlSslMode::Preferred => MySqlSslMode::Preferred,
            crate::MySqlSslMode::Required => MySqlSslMode::Required,
            crate::MySqlSslMode::VerifyCa => MySqlSslMode::VerifyCa,
            crate::MySqlSslMode::VerifyIdentity => MySqlSslMode::VerifyIdentity,
        });
    if let Some(database) = &config.database {
        options = options.database(database);
    }
    if let Some(password) = &input.credentials.password {
        options = options.password(password.expose_secret());
    }
    use sqlx::ConnectOptions;
    options = options.disable_statement_logging();
    bounded(cancel, Some(Duration::from_secs(15)), async {
        MySqlConnection::connect_with(&options)
            .await
            .map_err(map_error)
    })
    .await
}
pub async fn test(
    input: &ResolvedConnection,
    cancel: CancellationToken,
) -> DriverResult<ServerInfo> {
    let mut connection = connect(input, &cancel).await?;
    let version = bounded(&cancel, Some(Duration::from_secs(15)), async {
        sqlx::query_scalar::<_, String>("SELECT version()")
            .fetch_one(&mut connection)
            .await
            .map_err(map_error)
    })
    .await?;
    connection.close().await.map_err(map_error)?;
    Ok(ServerInfo {
        version: Some(version),
    })
}
pub async fn open(
    input: &ResolvedConnection,
    cancel: CancellationToken,
) -> DriverResult<Box<dyn Session>> {
    let mut connection = connect(input, &cancel).await?;
    let backend = bounded(&cancel, Some(Duration::from_secs(15)), async {
        Ok(sqlx::query_scalar::<_, u64>("SELECT CONNECTION_ID()")
            .fetch_one(&mut connection)
            .await
            .map_err(map_error)? as i64)
    })
    .await?;
    Ok(Box::new(DatabaseSession {
        connection: Some(connection),
        input: input.clone(),
        backend,
    }))
}
#[derive(Debug)]
struct DatabaseSession {
    input: ResolvedConnection,
    backend: i64,
    connection: Option<MySqlConnection>,
}
#[async_trait]
impl Session for DatabaseSession {
    async fn execute(
        &mut self,
        request: &QueryRequest,
        sink: &mut dyn RowSink,
        cancel: CancellationToken,
    ) -> DriverResult<DriverOutcome> {
        request.validate()?;
        if cancel.is_cancelled() {
            return Err(DriverError::cancelled());
        }
        let mut owned = self.connection.take().ok_or_else(|| {
            DriverError::new(
                ErrorCode::Connection,
                "Session closed; reconnect before executing",
            )
        })?;
        let connection = &mut owned;
        let mut interrupted = false;
        let operation = async {
            if let Some(database) = request.options.context.schema.as_ref().or(request
                .options
                .context
                .catalog
                .as_ref())
            {
                let sql = format!("USE `{}`", database.replace('`', "``"));
                sqlx::raw_sql(sqlx::AssertSqlSafe(sql.as_str()))
                    .execute(&mut *connection)
                    .await
                    .map_err(map_error)?;
            }
            let description = connection
                .prepare(sqlx::AssertSqlSafe(request.sql.as_str()).into_sql_str())
                .await
                .map_err(map_error)?;
            let columns = description
                .columns()
                .iter()
                .map(|c| ColumnInfo {
                    name: c.name().into(),
                    data_type: c.type_info().name().into(),
                    nullable: None,
                })
                .collect::<Vec<_>>();
            if !columns.is_empty() && sink.columns(columns) == SinkControl::Stop {
                interrupted = true;
                return Ok(DriverOutcome::default());
            }
            let mut stream = sqlx::raw_sql(sqlx::AssertSqlSafe(request.sql.as_str()))
                .fetch_many(&mut *connection);
            let mut count = 0u32;
            let mut affected = None;
            while let Some(item) = stream.try_next().await.map_err(map_error)? {
                match item {
                    sqlx::Either::Left(result) => affected = Some(result.rows_affected()),
                    sqlx::Either::Right(row) => {
                        let mut cells = Vec::with_capacity(row.len());
                        for index in 0..row.len() {
                            let value = row.try_get_raw(index).map_err(map_error)?;
                            if value.is_null() {
                                cells.push(Cell::Null);
                                continue;
                            }
                            let bytes = <Vec<u8> as sqlx::Decode<sqlx::MySql>>::decode(value)
                                .map_err(|_| {
                                    DriverError::new(ErrorCode::Protocol, "Invalid database value")
                                })?;
                            let kind = row.columns()[index].type_info().name();
                            if matches!(
                                kind,
                                "BLOB"
                                    | "TINYBLOB"
                                    | "MEDIUMBLOB"
                                    | "LONGBLOB"
                                    | "BINARY"
                                    | "VARBINARY"
                                    | "BIT"
                            ) {
                                cells.push(Cell::Binary(bytes));
                            } else {
                                cells.push(Cell::Text(String::from_utf8(bytes).map_err(|_| {
                                    DriverError::new(
                                        ErrorCode::Protocol,
                                        "Invalid database text encoding",
                                    )
                                })?));
                            }
                        }
                        count += 1;
                        if sink.row(cells) == SinkControl::Stop || count >= request.options.max_rows
                        {
                            interrupted = true;
                            break;
                        }
                    }
                }
            }
            Ok(DriverOutcome {
                rows_affected: if description.columns().is_empty() {
                    affected
                } else {
                    None
                },
            })
        };
        let outcome = bounded(&cancel, request.options.timeout, operation).await;
        if outcome.is_err() || interrupted {
            let cancel_server = async {
                let mut control = connect(&self.input, &CancellationToken::new()).await?;
                let sql = format!("KILL QUERY {}", self.backend);
                sqlx::raw_sql(sqlx::AssertSqlSafe(sql.as_str()))
                    .execute(&mut control)
                    .await
                    .map_err(map_error)?;
                Ok::<_, DriverError>(())
            };
            let _ = tokio::time::timeout(Duration::from_secs(5), cancel_server).await;
            // SQLx's hard close still shuts down TLS. A server may keep that
            // handshake open; dropping the timed-out future closes the socket.
            let _ = tokio::time::timeout(Duration::from_secs(1), owned.close_hard()).await;
        } else {
            self.connection = Some(owned);
        }
        outcome
    }
    async fn close(mut self: Box<Self>) -> DriverResult<()> {
        if let Some(connection) = self.connection.take() {
            connection.close().await.map_err(map_error)?;
        }
        Ok(())
    }
}
pub async fn introspect(
    input: &ResolvedConnection,
    scope: &ObjectPath,
    sink: &mut dyn IntrospectionSink,
    cancel: CancellationToken,
) -> DriverResult<IntrospectionReport> {
    let mut connection = connect(input, &cancel).await?;
    bounded(&cancel, Some(Duration::from_secs(300)), async {
        let mut seen = HashSet::new();
        let mut rows = sqlx::query(METADATA_SQL)
            .bind(scope.name_of(ObjectKind::Database))
            .bind(scope.name_of(ObjectKind::Database))
            .bind(
                scope
                    .name_of(ObjectKind::Table)
                    .or(scope.name_of(ObjectKind::View)),
            )
            .bind(
                scope
                    .name_of(ObjectKind::Table)
                    .or(scope.name_of(ObjectKind::View)),
            )
            .bind(scope.name_of(ObjectKind::Column))
            .bind(scope.name_of(ObjectKind::Column))
            .fetch(&mut connection);
        while let Some(row) = rows.try_next().await.map_err(map_error)? {
            let database: String = row.try_get("db").map_err(map_error)?;
            let schema: String = row.try_get("ns").map_err(map_error)?;
            let table: Option<String> = row.try_get("rel").map_err(map_error)?;
            let root = ObjectPath::default();
            let db = MetadataObject::new(ObjectKind::Database, &root, database);
            let mut batch = Vec::new();
            if seen.insert(db.path.clone()) {
                batch.push(db.clone());
            }
            let parent = {
                let _ = schema;
                db.path
            };
            if let Some(table) = table {
                let relation_type: Option<String> = row.try_get("reltype").map_err(map_error)?;
                let relation = MetadataObject::new(
                    if relation_type.as_deref() == Some("VIEW") {
                        ObjectKind::View
                    } else {
                        ObjectKind::Table
                    },
                    &parent,
                    table,
                );
                if seen.insert(relation.path.clone()) {
                    batch.push(relation.clone());
                }
                if let Some(name) = row.try_get::<Option<String>, _>("col").map_err(map_error)? {
                    let mut column = MetadataObject::new(ObjectKind::Column, &relation.path, name);
                    column.data_type = row.try_get("dtype").map_err(map_error)?;
                    column.nullable = row
                        .try_get::<Option<String>, _>("isnull")
                        .map_err(map_error)?
                        .map(|v| v == "YES");
                    column.ordinal = row
                        .try_get::<Option<i64>, _>("pos")
                        .map_err(map_error)?
                        .and_then(|v| u32::try_from(v).ok());
                    batch.push(column);
                }
            }
            batch.retain(|object| object.path.is_within(scope));
            if !batch.is_empty() {
                sink.batch(batch).await?;
            }
        }
        Ok(IntrospectionReport::default())
    })
    .await
}
const METADATA_SQL: &str = r#"SELECT s.schema_name AS db,s.schema_name AS ns,t.table_name AS rel,t.table_type AS reltype,c.column_name AS col,c.column_type AS dtype,c.is_nullable AS isnull,CAST(c.ordinal_position AS SIGNED) AS pos FROM information_schema.schemata s LEFT JOIN information_schema.tables t ON t.table_schema=s.schema_name LEFT JOIN information_schema.columns c ON c.table_schema=t.table_schema AND c.table_name=t.table_name WHERE s.schema_name NOT IN ('mysql','information_schema','performance_schema','sys') AND (? IS NULL OR s.schema_name=?) AND (? IS NULL OR t.table_name=?) AND (? IS NULL OR c.column_name=?) ORDER BY s.schema_name,t.table_name,c.ordinal_position"#;
