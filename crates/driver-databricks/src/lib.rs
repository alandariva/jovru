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
                named_parameters: true,
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

    /// The CLI profile names the workspace and the identity that sees it;
    /// compute and defaults do not change the catalog.
    fn cache_identity(&self, config: &ConnectionConfig) -> serde_json::Value {
        match config.options.get("profile") {
            Some(profile) => serde_json::json!({ "profile": profile }),
            None => config.options.clone(),
        }
    }

    /// Unity Catalog's `system` catalog holds the platform's own tables;
    /// choosing everything leaves it out until it is chosen by name.
    fn left_out_of_everything(&self, path: &ObjectPath) -> bool {
        matches!(path.0.as_slice(), [catalog] if catalog.kind == ObjectKind::Catalog && catalog.name == "system")
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
        self.prepare(connection, false, &cancel)?;
        scope.validate_scope(&self.descriptor().capabilities.hierarchy)?;
        runtime::introspect(connection, scope, level, sink, cancel).await
    }
    async fn inspect(
        &self,
        connection: &ResolvedConnection,
        path: &ObjectPath,
        cancel: CancellationToken,
    ) -> DriverResult<Inspection> {
        self.prepare(connection, false, &cancel)?;
        runtime::inspect(connection, path, cancel).await
    }
    async fn definition(
        &self,
        connection: &ResolvedConnection,
        path: &ObjectPath,
        cancel: CancellationToken,
    ) -> DriverResult<String> {
        self.prepare(connection, false, &cancel)?;
        runtime::definition(connection, path, cancel).await
    }
}
