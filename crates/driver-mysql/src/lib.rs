//! MySQL driver with dedicated SQL sessions and information_schema metadata.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod config;
pub use config::{MySqlConfig, MySqlSslMode};

use jykre_core::*;
mod runtime;

#[derive(Debug, Default)]
pub struct MySqlDriver;

impl MySqlDriver {
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
impl Driver for MySqlDriver {
    fn descriptor(&self) -> DriverDescriptor {
        DriverDescriptor {
            kind: DriverKind::MySql,
            display_name: "MySQL".into(),
            dialect: SqlDialect::MySql,
            status: DriverStatus::Ready,
            capabilities: DriverCapabilities {
                sql: true,
                metadata: true,
                transactions: true,
                hierarchy: vec![
                    ObjectKind::Database,
                    ObjectKind::Table,
                    ObjectKind::View,
                    ObjectKind::Column,
                ],
                default_port: Some(3306),
                identifier_quote: '`',
            },
        }
    }

    fn validate_config(&self, options: &serde_json::Value) -> DriverResult<()> {
        MySqlConfig::parse(options).map(|_| ())
    }

    /// The configured database, when there is one.
    fn default_namespaces(&self, config: &ConnectionConfig) -> Vec<ObjectPath> {
        config.options["database"]
            .as_str()
            .map(|database| vec![ObjectPath::default().child(ObjectKind::Database, database)])
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
