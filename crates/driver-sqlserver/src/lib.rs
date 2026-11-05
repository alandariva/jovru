//! SQL Server driver over TDS (tiberius), with sessions kept in a task of
//! their own and metadata from each database's catalog views.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod config;
pub use config::{SqlServerConfig, SqlServerEncryption};

use jykre_core::*;
mod metadata;
mod net;
mod session;
mod values;

#[derive(Debug, Default)]
pub struct SqlServerDriver;

impl SqlServerDriver {
    fn prepare(
        &self,
        connection: &ResolvedConnection,
        cancel: &CancellationToken,
    ) -> DriverResult<()> {
        self.validate_connection(&connection.config)?;
        if cancel.is_cancelled() {
            return Err(DriverError::cancelled());
        }
        Ok(())
    }
}

#[async_trait]
impl Driver for SqlServerDriver {
    fn descriptor(&self) -> DriverDescriptor {
        DriverDescriptor {
            kind: DriverKind::SqlServer,
            display_name: "SQL Server".into(),
            dialect: SqlDialect::SqlServer,
            status: DriverStatus::Ready,
            capabilities: DriverCapabilities {
                named_parameters: true,
                sql: true,
                metadata: true,
                transactions: true,
                hierarchy: vec![
                    ObjectKind::Database,
                    ObjectKind::Schema,
                    ObjectKind::Table,
                    ObjectKind::View,
                    ObjectKind::Column,
                ],
                default_port: Some(1433),
                // Sessions run with QUOTED_IDENTIFIER ON, so the standard
                // quote names objects as brackets do.
                identifier_quote: '"',
            },
        }
    }

    fn validate_config(&self, options: &serde_json::Value) -> DriverResult<()> {
        SqlServerConfig::parse(options).map(|_| ())
    }

    /// The server and the login decide which databases the catalog shows;
    /// the default database and TLS do not.
    fn cache_identity(&self, config: &ConnectionConfig) -> serde_json::Value {
        match SqlServerConfig::parse(&config.options) {
            Ok(parsed) => serde_json::json!({
                "host": parsed.host,
                "port": parsed.port,
                "user": parsed.user,
            }),
            Err(_) => config.options.clone(),
        }
    }

    /// `dbo` of the configured database, when there is one.
    fn default_namespaces(&self, config: &ConnectionConfig) -> Vec<ObjectPath> {
        config.options["database"]
            .as_str()
            .map(|database| {
                vec![
                    ObjectPath::default()
                        .child(ObjectKind::Database, database)
                        .child(ObjectKind::Schema, "dbo"),
                ]
            })
            .unwrap_or_default()
    }

    async fn test(
        &self,
        connection: &ResolvedConnection,
        cancel: CancellationToken,
    ) -> DriverResult<ServerInfo> {
        self.prepare(connection, &cancel)?;
        session::test(connection, cancel).await
    }

    async fn open_session(
        &self,
        connection: &ResolvedConnection,
        cancel: CancellationToken,
    ) -> DriverResult<Box<dyn Session>> {
        self.prepare(connection, &cancel)?;
        session::open(connection, cancel).await
    }

    async fn introspect(
        &self,
        connection: &ResolvedConnection,
        sink: &mut dyn IntrospectionSink,
        cancel: CancellationToken,
    ) -> DriverResult<IntrospectionReport> {
        self.prepare(connection, &cancel)?;
        metadata::introspect(
            connection,
            &ObjectPath::default(),
            IntrospectionLevel::Full,
            sink,
            cancel,
        )
        .await
    }
    async fn discover_namespaces(
        &self,
        connection: &ResolvedConnection,
        parent: &ObjectPath,
        cancel: CancellationToken,
    ) -> DriverResult<Vec<MetadataObject>> {
        self.prepare(connection, &cancel)?;
        parent.validate_scope(&self.descriptor().capabilities.hierarchy)?;
        metadata::discover(connection, parent, cancel).await
    }
    async fn introspect_scoped(
        &self,
        connection: &ResolvedConnection,
        scope: &ObjectPath,
        sink: &mut dyn IntrospectionSink,
        cancel: CancellationToken,
    ) -> DriverResult<IntrospectionReport> {
        self.prepare(connection, &cancel)?;
        scope.validate_scope(&self.descriptor().capabilities.hierarchy)?;
        metadata::introspect(connection, scope, IntrospectionLevel::Full, sink, cancel).await
    }
    async fn introspect_level(
        &self,
        connection: &ResolvedConnection,
        scope: &ObjectPath,
        level: IntrospectionLevel,
        sink: &mut dyn IntrospectionSink,
        cancel: CancellationToken,
    ) -> DriverResult<IntrospectionReport> {
        self.prepare(connection, &cancel)?;
        scope.validate_scope(&self.descriptor().capabilities.hierarchy)?;
        metadata::introspect(connection, scope, level, sink, cancel).await
    }
    async fn inspect(
        &self,
        connection: &ResolvedConnection,
        path: &ObjectPath,
        cancel: CancellationToken,
    ) -> DriverResult<Inspection> {
        self.prepare(connection, &cancel)?;
        metadata::inspect(connection, path, cancel).await
    }
    async fn definition(
        &self,
        connection: &ResolvedConnection,
        path: &ObjectPath,
        cancel: CancellationToken,
    ) -> DriverResult<String> {
        self.prepare(connection, &cancel)?;
        metadata::definition(connection, path, cancel).await
    }
}
