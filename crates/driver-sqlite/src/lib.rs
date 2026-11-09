//! SQLite driver: a database file opened through SQLx's bundled SQLite, with
//! a dedicated connection per session and metadata read from SQLite's
//! pragma functions (ADR-0027).
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod config;
pub use config::SqliteConfig;

use jykre_core::*;
mod edit;
mod runtime;

/// The schema every SQLite connection has: the database file itself.
/// Attached databases are schemas beside it; `temp` is left out.
pub const MAIN_SCHEMA: &str = "main";

#[derive(Debug, Default)]
pub struct SqliteDriver;

impl SqliteDriver {
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
impl Driver for SqliteDriver {
    fn descriptor(&self) -> DriverDescriptor {
        DriverDescriptor {
            kind: DriverKind::Sqlite,
            display_name: "SQLite".into(),
            dialect: SqlDialect::Sqlite,
            status: DriverStatus::Ready,
            capabilities: DriverCapabilities {
                named_parameters: true,
                row_writes: true,
                sql: true,
                metadata: true,
                transactions: true,
                hierarchy: vec![
                    ObjectKind::Schema,
                    ObjectKind::Table,
                    ObjectKind::View,
                    ObjectKind::Column,
                ],
                default_port: None,
                identifier_quote: '"',
            },
        }
    }

    fn validate_config(&self, options: &serde_json::Value) -> DriverResult<()> {
        SqliteConfig::parse(options).map(|_| ())
    }

    /// The file decides what the catalog holds; opening it read-only does
    /// not.
    fn cache_identity(&self, config: &ConnectionConfig) -> serde_json::Value {
        match SqliteConfig::parse(&config.options) {
            Ok(parsed) => serde_json::json!({ "path": parsed.path }),
            Err(_) => config.options.clone(),
        }
    }

    /// `main`, the database file itself.
    fn default_namespaces(&self, config: &ConnectionConfig) -> Vec<ObjectPath> {
        let _ = config;
        vec![ObjectPath::default().child(ObjectKind::Schema, MAIN_SCHEMA)]
    }

    async fn test(
        &self,
        connection: &ResolvedConnection,
        cancel: CancellationToken,
    ) -> DriverResult<ServerInfo> {
        self.prepare(connection, &cancel)?;
        runtime::test(connection, cancel).await
    }

    async fn open_session(
        &self,
        connection: &ResolvedConnection,
        cancel: CancellationToken,
    ) -> DriverResult<Box<dyn Session>> {
        self.prepare(connection, &cancel)?;
        runtime::open(connection, cancel).await
    }

    async fn introspect(
        &self,
        connection: &ResolvedConnection,
        sink: &mut dyn IntrospectionSink,
        cancel: CancellationToken,
    ) -> DriverResult<IntrospectionReport> {
        self.prepare(connection, &cancel)?;
        runtime::introspect(
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
        runtime::discover(connection, parent, cancel).await
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
        runtime::introspect(connection, scope, IntrospectionLevel::Full, sink, cancel).await
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
        runtime::introspect(connection, scope, level, sink, cancel).await
    }

    async fn inspect(
        &self,
        connection: &ResolvedConnection,
        path: &ObjectPath,
        cancel: CancellationToken,
    ) -> DriverResult<Inspection> {
        self.prepare(connection, &cancel)?;
        runtime::inspect(connection, path, cancel).await
    }

    async fn definition(
        &self,
        connection: &ResolvedConnection,
        path: &ObjectPath,
        cancel: CancellationToken,
    ) -> DriverResult<String> {
        self.prepare(connection, &cancel)?;
        runtime::definition(connection, path, cancel).await
    }
}
