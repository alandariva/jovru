//! PostgreSQL driver with dedicated SQL sessions and information_schema metadata.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod config;
pub use config::{PostgresConfig, PostgresSslMode};

use jykre_core::*;
mod edit;
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
                named_parameters: true,
                row_writes: true,
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

    /// The first schema of the configured `search_path` (`public` when
    /// none is set) in the configured database: where unqualified names go
    /// by default.
    fn default_namespaces(&self, config: &ConnectionConfig) -> Vec<ObjectPath> {
        let Ok(parsed) = PostgresConfig::parse(&config.options) else {
            return Vec::new();
        };
        let schema = parsed
            .search_path_schemas()
            .into_iter()
            .find(|schema| schema != "$user")
            .unwrap_or_else(|| "public".to_owned());
        vec![
            ObjectPath::default()
                .child(ObjectKind::Database, parsed.database)
                .child(ObjectKind::Schema, schema),
        ]
    }

    /// The server, the database and the user decide what the catalog shows;
    /// TLS and the search path do not.
    fn cache_identity(&self, config: &ConnectionConfig) -> serde_json::Value {
        match PostgresConfig::parse(&config.options) {
            Ok(parsed) => serde_json::json!({
                "host": parsed.host,
                "port": parsed.port,
                "database": parsed.database,
                "user": parsed.user,
            }),
            Err(_) => config.options.clone(),
        }
    }

    /// A session is bound to one database: a context in another database of
    /// the server opens its session there, when the profile offers the
    /// server's other databases. The schema is applied by the session.
    fn session_connection(
        &self,
        connection: &ResolvedConnection,
        context: &SessionContext,
    ) -> DriverResult<ResolvedConnection> {
        match context.catalog.as_deref() {
            None => Ok(connection.clone()),
            Some(database) => runtime::database_connection(
                connection,
                database,
                "This connection reads only its own database; turn on all databases in its settings",
            ),
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use secrecy::ExposeSecret as _;
    use serde_json::json;

    fn input(all_databases: bool) -> ResolvedConnection {
        ResolvedConnection {
            config: ConnectionConfig {
                driver: DriverKind::Postgres,
                options: json!({
                    "host": "db",
                    "database": "sales",
                    "user": "reader",
                    "sslMode": "require",
                    "allDatabases": all_databases,
                }),
            },
            credentials: Credentials {
                password: Some(SecretString::from("synthetic".to_owned())),
            },
        }
    }

    fn context(catalog: Option<&str>) -> SessionContext {
        SessionContext {
            catalog: catalog.map(str::to_owned),
            schema: Some("public".into()),
        }
    }

    #[test]
    fn a_session_opens_on_the_database_its_context_names() {
        let driver = PostgresDriver;
        for own in [None, Some("sales")] {
            let opened = driver
                .session_connection(&input(false), &context(own))
                .unwrap();
            assert_eq!(opened.config, input(false).config);
        }
        let other = driver
            .session_connection(&input(true), &context(Some("archive")))
            .unwrap();
        let mut expected = input(true).config;
        expected.options["database"] = json!("archive");
        assert_eq!(other.config, expected);
        assert_eq!(
            other
                .credentials
                .password
                .as_ref()
                .map(|password| password.expose_secret().to_owned()),
            Some("synthetic".to_owned())
        );
    }

    #[test]
    fn another_database_needs_a_profile_that_offers_them() {
        let error = PostgresDriver
            .session_connection(&input(false), &context(Some("archive")))
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::Validation);
    }
}
