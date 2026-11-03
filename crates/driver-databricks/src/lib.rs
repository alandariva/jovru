//! CLI OAuth, SQL Warehouse and classic cluster execution, and
//! independent Unity Catalog REST metadata.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod config;
pub use auth::{CliProfile, list_profiles};
pub use config::{ComputeKind, DatabricksConfig};
pub use runtime::{ComputeResource, list_compute};

use jykre_core::*;
mod auth;
mod runtime;

#[derive(Debug, Default)]
pub struct DatabricksDriver;

impl DatabricksDriver {
    fn prepare(
        &self,
        connection: &ResolvedConnection,
        sql: bool,
        cancel: &CancellationToken,
    ) -> DriverResult<()> {
        self.validate_connection(&connection.config)?;
        if sql {
            DatabricksConfig::parse(&connection.config.options)?.validate_sql()?;
        }
        if cancel.is_cancelled() {
            return Err(DriverError::cancelled());
        }
        Ok(())
    }
}

#[async_trait]
impl Driver for DatabricksDriver {
    fn descriptor(&self) -> DriverDescriptor {
        DriverDescriptor {
            kind: DriverKind::Databricks,
            display_name: "Databricks".into(),
            dialect: SqlDialect::DatabricksSql,
            status: DriverStatus::Ready,
            capabilities: DriverCapabilities {
                sql: true,
                metadata: true,
                transactions: false,
                hierarchy: vec![
                    ObjectKind::Catalog,
                    ObjectKind::Schema,
                    ObjectKind::Table,
                    ObjectKind::View,
                    ObjectKind::Column,
                ],
                default_port: None,
                identifier_quote: '`',
            },
        }
    }

    fn validate_config(&self, options: &serde_json::Value) -> DriverResult<()> {
        DatabricksConfig::parse(options).map(|_| ())
    }

    /// The profile's default catalog and schema, when both are set.
    fn default_namespaces(&self, config: &ConnectionConfig) -> Vec<ObjectPath> {
        match (
            config.options["defaultCatalog"].as_str(),
            config.options["defaultSchema"].as_str(),
        ) {
            (Some(catalog), Some(schema)) => vec![
                ObjectPath::default()
                    .child(ObjectKind::Catalog, catalog)
                    .child(ObjectKind::Schema, schema),
            ],
            _ => Vec::new(),
        }
    }

    async fn test(
        &self,
        connection: &ResolvedConnection,
        cancel: CancellationToken,
    ) -> DriverResult<ServerInfo> {
        self.prepare(connection, false, &cancel)?;
        runtime::test(connection, cancel).await
    }

    async fn open_session(
        &self,
        connection: &ResolvedConnection,
        cancel: CancellationToken,
    ) -> DriverResult<Box<dyn Session>> {
        self.prepare(connection, true, &cancel)?;
        runtime::open(connection, cancel).await
    }

    async fn introspect(
        &self,
        connection: &ResolvedConnection,
        sink: &mut dyn IntrospectionSink,
        cancel: CancellationToken,
    ) -> DriverResult<IntrospectionReport> {
        self.prepare(connection, false, &cancel)?;
        runtime::introspect(connection, &ObjectPath::default(), sink, cancel).await
    }
    async fn discover_namespaces(
        &self,
        connection: &ResolvedConnection,
        parent: &ObjectPath,
        cancel: CancellationToken,
    ) -> DriverResult<Vec<MetadataObject>> {
        self.prepare(connection, false, &cancel)?;
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
        self.prepare(connection, false, &cancel)?;
        scope.validate_scope(&self.descriptor().capabilities.hierarchy)?;
        runtime::introspect(connection, scope, sink, cancel).await
    }
}
