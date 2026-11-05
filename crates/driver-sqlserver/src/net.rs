//! Connecting, and classifying what tiberius reports.

use crate::SqlServerConfig;
use jykre_core::*;
use secrecy::ExposeSecret;
use std::future::Future;
use std::time::Duration;
use tiberius::error::Error as TdsError;
use tokio::net::TcpStream;
use tokio_util::compat::{Compat, TokioAsyncWriteCompatExt};

/// How long connecting, logging in and preparing the session may take.
pub const CONNECT: Duration = Duration::from_secs(15);

/// Options every connection starts with: the ones SQL Server Management
/// Studio sets, so `"name"` is a name, comparisons with NULL follow the
/// standard and long text is not cut.
const SESSION_DEFAULTS: &str = "SET QUOTED_IDENTIFIER ON; SET ANSI_NULLS ON; SET ANSI_PADDING ON; \
SET ANSI_WARNINGS ON; SET ANSI_NULL_DFLT_ON ON; SET CONCAT_NULL_YIELDS_NULL ON; SET ARITHABORT ON; \
SET NUMERIC_ROUNDABORT OFF; SET TEXTSIZE 2147483647";

pub type Client = tiberius::Client<Compat<TcpStream>>;

/// A logged-in connection.
#[derive(Debug)]
pub struct Connection {
    pub client: Client,
}

impl Connection {
    /// Runs `sql` and reads its first result whole. For small queries the
    /// driver itself runs.
    pub async fn rows(&mut self, sql: &str) -> DriverResult<Vec<tiberius::Row>> {
        self.client
            .simple_query(sql)
            .await
            .map_err(map_error)?
            .into_first_result()
            .await
            .map_err(map_error)
    }

    /// Runs `sql` with `parameters` bound as `@P1`, `@P2`… and reads its
    /// first result whole.
    pub async fn query(
        &mut self,
        sql: &str,
        parameters: &[Option<&str>],
    ) -> DriverResult<Vec<tiberius::Row>> {
        let mut query = tiberius::Query::new(sql);
        for parameter in parameters {
            query.bind(*parameter);
        }
        query
            .query(&mut self.client)
            .await
            .map_err(map_error)?
            .into_first_result()
            .await
            .map_err(map_error)
    }

    pub async fn close(self) -> DriverResult<()> {
        self.client.close().await.map_err(map_error)
    }
}

/// Classifies a tiberius error. A server error keeps the server's own
/// message and error number; errors from the connection keep the operating
/// system's or TLS's reason. Anything else gets a fixed message.
pub fn map_error(error: TdsError) -> DriverError {
    match error {
        TdsError::Server(token) => {
            let code = match token.code() {
                // Login failed, password expired or must change.
                18452 | 18456 | 18470 | 18486 | 18487 | 18488 => ErrorCode::Authentication,
                // Permission denied on an object, a database or a server
                // state; the login cannot open the database it asked for.
                229 | 230 | 262 | 297 | 300 | 916 | 4060 | 15247 => ErrorCode::PermissionDenied,
                _ => ErrorCode::Database,
            };
            DriverError::from_server(
                code,
                token.message().to_owned(),
                ServerDiagnostic {
                    sqlstate: None,
                    vendor_code: Some(token.code().to_string()),
                    position: None,
                },
            )
        }
        TdsError::Io { message, .. } => DriverError::new(
            ErrorCode::Connection,
            format!("Could not reach the server: {message}"),
        ),
        TdsError::Tls(message) => DriverError::new(
            ErrorCode::Connection,
            format!("The TLS connection failed: {message}"),
        ),
        TdsError::Routing { .. } => DriverError::new(
            ErrorCode::Connection,
            "The server redirected the connection more than once",
        ),
        _ => DriverError::new(
            ErrorCode::Protocol,
            "Database operation failed; check connectivity, permissions and SQL",
        ),
    }
}

pub fn timed_out() -> DriverError {
    DriverError::new(ErrorCode::Timeout, "Database operation timed out")
}

/// Runs `work` unless `cancel` fires or `timeout` passes first.
pub async fn bounded<T>(
    cancel: &CancellationToken,
    timeout: Option<Duration>,
    work: impl Future<Output = DriverResult<T>>,
) -> DriverResult<T> {
    tokio::select! { biased;
        _ = cancel.cancelled() => Err(DriverError::cancelled()),
        result = async {
            match timeout {
                Some(limit) => tokio::time::timeout(limit, work).await.map_err(|_| timed_out())?,
                None => work.await,
            }
        } => result,
    }
}

/// Opens a connection, logs in and sets the session's options. A server
/// that redirects the login (Azure SQL's gateway) is followed once.
pub async fn connect(
    input: &ResolvedConnection,
    cancel: &CancellationToken,
) -> DriverResult<Connection> {
    let config = SqlServerConfig::parse(&input.config.options)?;
    let password = input
        .credentials
        .password
        .as_ref()
        .map(|password| password.expose_secret())
        .unwrap_or_default();
    bounded(cancel, Some(CONNECT), async {
        let mut target = (config.socket_host().to_owned(), config.port);
        let mut redirected = false;
        let mut connection = loop {
            let mut options = tiberius::Config::new();
            options.host(&target.0);
            options.port(target.1);
            if let Some(database) = &config.database {
                options.database(database);
            }
            options.application_name("Jykre Studio");
            options.encryption(match config.encrypt {
                crate::SqlServerEncryption::Off => tiberius::EncryptionLevel::Off,
                crate::SqlServerEncryption::Mandatory => tiberius::EncryptionLevel::Required,
                crate::SqlServerEncryption::Strict => tiberius::EncryptionLevel::Strict,
            });
            if config.trust_server_certificate {
                options.trust_cert();
            }
            options.authentication(tiberius::AuthMethod::sql_server(&config.user, password));
            // Statements have their own limits; tiberius' 30 seconds would
            // cut long queries and leave the connection out of step.
            options.command_timeout(None);
            let tcp = TcpStream::connect((target.0.as_str(), target.1))
                .await
                .map_err(|error| {
                    DriverError::new(
                        ErrorCode::Connection,
                        format!("Could not reach the server: {error}"),
                    )
                })?;
            let _ = tcp.set_nodelay(true);
            match tiberius::Client::connect(options, tcp.compat_write()).await {
                Ok(client) => break Connection { client },
                Err(TdsError::Routing { host, port }) if !redirected => {
                    redirected = true;
                    target = (host, port);
                }
                Err(error) => return Err(map_error(error)),
            }
        };
        connection.rows(SESSION_DEFAULTS).await?;
        Ok(connection)
    })
    .await
}

/// Quotes a name with brackets, for SQL the driver writes itself.
pub fn bracketed(name: &str) -> String {
    format!("[{}]", name.replace(']', "]]"))
}
