use crate::{
    CancellationToken, ConnectionConfig, DriverError, DriverKind, DriverOutcome, DriverResult,
    ExecutionOptions, Inspection, IntrospectionLevel, IntrospectionReport, MetadataObject,
    ObjectKind, ObjectPath, QueryRequest, ResolvedConnection, RowSink, ServerInfo, SessionContext,
    SqlDialect,
};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::fmt::Debug;

/// Planned support, not a guarantee of readiness. Check DriverDescriptor::status.
/// Table and View are alternative relation kinds within the hierarchy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DriverCapabilities {
    pub sql: bool,
    pub metadata: bool,
    pub transactions: bool,
    pub hierarchy: Vec<ObjectKind>,
    pub default_port: Option<u16>,
    pub identifier_quote: char,
}

impl DriverCapabilities {
    /// The levels of the hierarchy that hold relations rather than being
    /// relations: databases, catalogs and schemas. The user chooses what to
    /// introspect among them; the last one is the unit a refresh reads.
    pub fn namespace_levels(&self) -> Vec<ObjectKind> {
        self.hierarchy
            .iter()
            .copied()
            .take_while(|kind| {
                matches!(
                    kind,
                    ObjectKind::Database | ObjectKind::Catalog | ObjectKind::Schema
                )
            })
            .collect()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DriverStatus {
    Stub,
    Ready,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DriverDescriptor {
    pub kind: DriverKind,
    pub display_name: String,
    pub dialect: SqlDialect,
    pub status: DriverStatus,
    pub capabilities: DriverCapabilities,
}

/// A SQL session that a tab keeps between runs. Relational drivers keep one
/// physical connection, so session state (current schema, temporary tables,
/// an open transaction) carries over between statements; Databricks keeps a
/// logical catalog/schema context and sends it with each request.
///
/// A failed statement does not end the session: the driver restores the
/// protocol to a known state (cancelling server-side work when needed) and
/// keeps the session, or closes it when that is not possible. The caller
/// asks [`Session::is_open`] before reusing it.
#[async_trait]
pub trait Session: Debug + Send {
    /// Implementations validate the request, honor cancellation, timeout and
    /// row limits, and stop producing data when the sink returns Stop.
    async fn execute(
        &mut self,
        request: &QueryRequest,
        sink: &mut dyn RowSink,
        cancel: CancellationToken,
    ) -> DriverResult<DriverOutcome>;

    /// Reads the next page, up to `options.max_rows` rows, of the result the
    /// last `execute` or `fetch` left open (`DriverOutcome::has_more`). Rows
    /// arrive through `sink` as in `execute`, without columns. Fails with
    /// `Validation` when no result is open.
    async fn fetch(
        &mut self,
        options: &ExecutionOptions,
        sink: &mut dyn RowSink,
        cancel: CancellationToken,
    ) -> DriverResult<DriverOutcome> {
        let _ = (options, sink, cancel);
        Err(DriverError::validation("No result has more rows to read"))
    }

    /// Whether the session can run another statement. `false` once the
    /// connection was lost or could not be brought back to a known state;
    /// the caller then opens a new session, whose state starts over.
    fn is_open(&self) -> bool {
        true
    }

    /// Where unqualified names resolve now, as the server reports it after
    /// the statements run so far, including a `USE` or `SET` the user ran and
    /// indirect changes (a procedure, a rolled back `SET`). The reported
    /// context becomes the one the session considers applied, so a request
    /// carrying it is not applied again. Asking discards a result still open
    /// for paging: callers ask only when none is. The default knows nothing
    /// and returns an empty context.
    async fn context(&mut self, cancel: CancellationToken) -> DriverResult<SessionContext> {
        let _ = cancel;
        Ok(SessionContext::default())
    }

    /// Checks, before a kept session is reused, that the server still
    /// answers, within a short deadline. A failure closes the session
    /// ([`Session::is_open`] turns `false`). Sessions without a connection of
    /// their own have nothing to check.
    async fn ping(&mut self) -> DriverResult<()> {
        Ok(())
    }

    async fn close(self: Box<Self>) -> DriverResult<()>;
}

/// Receives batches without requiring the driver to retain the full catalog.
/// A sink error stops introspection; a failed operation may have emitted batches.
/// Consumers should only publish a complete snapshot after successful completion.
#[async_trait]
pub trait IntrospectionSink: Send {
    async fn batch(&mut self, objects: Vec<MetadataObject>) -> DriverResult<()>;
}

/// Protocol-specific code depends only on these object-safe contracts.
/// Operations validate configuration before cancellation or external I/O.
#[async_trait]
pub trait Driver: Debug + Send + Sync + 'static {
    fn descriptor(&self) -> DriverDescriptor;

    fn validate_config(&self, options: &serde_json::Value) -> DriverResult<()>;

    fn validate_connection(&self, config: &ConnectionConfig) -> DriverResult<()> {
        if config.driver != self.descriptor().kind {
            return Err(DriverError::validation(
                "Configuration belongs to another driver",
            ));
        }
        self.validate_config(&config.options)
    }

    /// Quotes one identifier for SQL generated by the application (previews,
    /// session commands), doubling the quote character inside it. Names are
    /// never split on dots or other separators.
    fn quote_identifier(&self, name: &str) -> String {
        let quote = self.descriptor().capabilities.identifier_quote;
        let mut quoted = String::with_capacity(name.len() + 2);
        quoted.push(quote);
        for ch in name.chars() {
            if ch == quote {
                quoted.push(quote);
            }
            quoted.push(ch);
        }
        quoted.push(quote);
        quoted
    }

    /// The qualified, quoted name of the relation at `path`, as generated SQL
    /// writes it: the namespaces and the relation
    /// ([`ObjectPath::relation_path`]), joined by dots. Drivers whose
    /// sessions are bound to one level of the hierarchy (a PostgreSQL
    /// database) leave that level out.
    fn relation_name(&self, path: &ObjectPath) -> String {
        path.relation_path()
            .unwrap_or_else(|| path.clone())
            .0
            .iter()
            .filter(|part| part.kind != ObjectKind::Column)
            .map(|part| self.quote_identifier(&part.name))
            .collect::<Vec<_>>()
            .join(".")
    }

    /// The namespaces a connection most likely works in (its configured
    /// database, default schema or catalog), offered as the first choice of
    /// what to introspect. Only suggested; nothing is read until the user
    /// saves a choice. Paths that the server does not list are ignored.
    fn default_namespaces(&self, config: &ConnectionConfig) -> Vec<ObjectPath> {
        let _ = config;
        Vec::new()
    }

    /// The options that decide which catalog a metadata cache describes: a
    /// change of anything else (compute, defaults, TLS) keeps the cache. The
    /// default is every option.
    fn cache_identity(&self, config: &ConnectionConfig) -> serde_json::Value {
        config.options.clone()
    }

    /// Namespaces that choosing everything leaves out until they are chosen
    /// by name, such as a catalog of system tables. The default is none.
    fn left_out_of_everything(&self, path: &ObjectPath) -> bool {
        let _ = path;
        false
    }

    async fn test(
        &self,
        connection: &ResolvedConnection,
        cancel: CancellationToken,
    ) -> DriverResult<ServerInfo>;

    async fn open_session(
        &self,
        connection: &ResolvedConnection,
        cancel: CancellationToken,
    ) -> DriverResult<Box<dyn Session>>;

    /// Metadata must not require an existing SQL session. In particular,
    /// Databricks Unity Catalog must not start or require a warehouse.
    async fn introspect(
        &self,
        connection: &ResolvedConnection,
        sink: &mut dyn IntrospectionSink,
        cancel: CancellationToken,
    ) -> DriverResult<IntrospectionReport>;

    /// Lists the namespaces directly below `parent` (the root, a database or
    /// a catalog) without reading their relations or columns, so the user can
    /// choose what to introspect. Only namespace levels
    /// ([`DriverCapabilities::namespace_levels`]) are returned, ordered by
    /// name; below the last namespace level the list is empty. Like
    /// introspection, it does not require a SQL session or start compute.
    async fn discover_namespaces(
        &self,
        connection: &ResolvedConnection,
        parent: &ObjectPath,
        cancel: CancellationToken,
    ) -> DriverResult<Vec<MetadataObject>> {
        let _ = (connection, parent, cancel);
        Err(DriverError::not_implemented(
            &self.descriptor().display_name,
            "namespace discovery",
        ))
    }

    /// Refresh a structural subtree. Empty scope means the complete connection.
    /// Only objects inside the scope are emitted. A successful empty result
    /// removes a deleted object; errors must leave the consumer's snapshot intact.
    async fn introspect_scoped(
        &self,
        connection: &ResolvedConnection,
        scope: &ObjectPath,
        sink: &mut dyn IntrospectionSink,
        cancel: CancellationToken,
    ) -> DriverResult<IntrospectionReport> {
        if scope.0.is_empty() {
            self.introspect(connection, sink, cancel).await
        } else {
            Err(DriverError::not_implemented(
                &self.descriptor().display_name,
                "scoped metadata",
            ))
        }
    }

    /// Reads `scope` at `level`, with the same rules as
    /// [`Driver::introspect_scoped`]. At [`IntrospectionLevel::Names`] only
    /// namespaces and relations are delivered, so they show before their
    /// columns. The default reads everything at either level.
    async fn introspect_level(
        &self,
        connection: &ResolvedConnection,
        scope: &ObjectPath,
        level: IntrospectionLevel,
        sink: &mut dyn IntrospectionSink,
        cancel: CancellationToken,
    ) -> DriverResult<IntrospectionReport> {
        let _ = level;
        self.introspect_scoped(connection, scope, sink, cancel)
            .await
    }

    /// What the relation or namespace at `path` has beyond the refreshed
    /// tree (indexes, constraints, triggers; routines, sequences, types),
    /// read only when the user asks. Like introspection it needs no SQL
    /// session, honours cancellation and has a time limit. The default
    /// reports `NotImplemented`.
    async fn inspect(
        &self,
        connection: &ResolvedConnection,
        path: &ObjectPath,
        cancel: CancellationToken,
    ) -> DriverResult<Inspection> {
        let _ = (connection, path, cancel);
        Err(DriverError::not_implemented(
            &self.descriptor().display_name,
            "inspection",
        ))
    }

    /// The DDL or source of the relation or routine at `path`, read only
    /// when the user asks. The default reports `NotImplemented`.
    async fn definition(
        &self,
        connection: &ResolvedConnection,
        path: &ObjectPath,
        cancel: CancellationToken,
    ) -> DriverResult<String> {
        let _ = (connection, path, cancel);
        Err(DriverError::not_implemented(
            &self.descriptor().display_name,
            "definitions",
        ))
    }
}
