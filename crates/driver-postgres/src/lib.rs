//! PostgreSQL driver with dedicated SQL sessions and information_schema metadata.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod config;
pub use config::{PostgresConfig, PostgresSslMode};

use jykre_core::*;
mod runtime;

#[derive(Debug, Default)]
pub struct PostgresDriver;

impl PostgresDriver {
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
impl Driver for PostgresDriver {
    fn descriptor(&self) -> DriverDescriptor {
        DriverDescriptor {
            kind: DriverKind::Postgres,
            display_name: "PostgreSQL".into(),
            dialect: SqlDialect::Postgres,
            status: DriverStatus::Ready,
            capabilities: DriverCapabilities {
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
                default_port: Some(5432),
                identifier_quote: '"',
            },
        }
    }

    fn validate_config(&self, options: &serde_json::Value) -> DriverResult<()> {
        PostgresConfig::parse(options).map(|_| ())
    }

    /// A session is bound to one database and PostgreSQL rejects
    /// cross-database names, so the database is left out.
    fn relation_name(&self, path: &ObjectPath) -> String {
        path.relation_path()
            .unwrap_or_else(|| path.clone())
            .0
            .iter()
            .filter(|part| !matches!(part.kind, ObjectKind::Database | ObjectKind::Column))
            .map(|part| self.quote_identifier(&part.name))
            .collect::<Vec<_>>()
            .join(".")
    }

    /// `public` of the configured database: where unqualified names go
    /// by default.
    fn default_namespaces(&self, config: &ConnectionConfig) -> Vec<ObjectPath> {
        config.options["database"]
            .as_str()
            .map(|database| {
                vec![
                    ObjectPath::default()
                        .child(ObjectKind::Database, database)
                        .child(ObjectKind::Schema, "public"),
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
        runtime::introspect(connection, &ObjectPath::default(), sink, cancel).await
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
        runtime::introspect(connection, scope, sink, cancel).await
    }
}
