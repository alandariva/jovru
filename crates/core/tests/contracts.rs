#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]
use jykre_core::*;
use serde_json::json;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

#[derive(Debug)]
struct TestDriver {
    name: &'static str,
    requests: Arc<Mutex<Vec<QueryRequest>>>,
    closed: Arc<AtomicBool>,
}

impl TestDriver {
    fn new(name: &'static str) -> Self {
        Self {
            name,
            requests: Arc::default(),
            closed: Arc::default(),
        }
    }

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
impl Driver for TestDriver {
    fn descriptor(&self) -> DriverDescriptor {
        DriverDescriptor {
            kind: DriverKind::Postgres,
            display_name: self.name.into(),
            dialect: SqlDialect::Postgres,
            status: DriverStatus::Ready,
            capabilities: DriverCapabilities {
                sql: true,
                metadata: true,
                transactions: false,
                hierarchy: vec![ObjectKind::Table, ObjectKind::Column],
                default_port: None,
                identifier_quote: '"',
            },
        }
    }

    fn validate_config(&self, options: &serde_json::Value) -> DriverResult<()> {
        if !options.is_object() {
            return Err(DriverError::validation("Expected configuration object"));
        }
        Ok(())
    }

    async fn test(
        &self,
        connection: &ResolvedConnection,
        cancel: CancellationToken,
    ) -> DriverResult<ServerInfo> {
        self.prepare(connection, &cancel)?;
        Ok(ServerInfo {
            version: Some("fixture".into()),
        })
    }

    async fn open_session(
        &self,
        connection: &ResolvedConnection,
        cancel: CancellationToken,
    ) -> DriverResult<Box<dyn Session>> {
        self.prepare(connection, &cancel)?;
        Ok(Box::new(TestSession {
            requests: self.requests.clone(),
            closed: self.closed.clone(),
        }))
    }

    async fn introspect(
        &self,
        connection: &ResolvedConnection,
        sink: &mut dyn IntrospectionSink,
        cancel: CancellationToken,
    ) -> DriverResult<IntrospectionReport> {
        self.prepare(connection, &cancel)?;
        let table = MetadataObject::new(ObjectKind::Table, &ObjectPath::default(), "sales.2026");
        let column = MetadataObject::new(ObjectKind::Column, &table.path, "total");
        sink.batch(vec![table]).await?;
        sink.batch(vec![column]).await?;
        Ok(IntrospectionReport::default())
    }
}

#[derive(Debug)]
struct TestSession {
    requests: Arc<Mutex<Vec<QueryRequest>>>,
    closed: Arc<AtomicBool>,
}

#[async_trait]
impl Session for TestSession {
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
        self.requests.lock().unwrap().push(request.clone());
        let columns = vec![ColumnInfo {
            name: "value".into(),
            data_type: "DECIMAL".into(),
            nullable: Some(true),
        }];
        if sink.columns(columns) == SinkControl::Stop {
            return Ok(DriverOutcome::default());
        }
        let values = [Cell::Decimal("9007199254740993.01".into()), Cell::Null];
        for value in values.into_iter().take(request.options.max_rows as usize) {
            if sink.row(vec![value]) == SinkControl::Stop {
                break;
            }
        }
        Ok(DriverOutcome::default())
    }

    async fn close(self: Box<Self>) -> DriverResult<()> {
        self.closed.store(true, Ordering::SeqCst);
        Ok(())
    }
}

#[derive(Default)]
struct Rows {
    columns: Vec<ColumnInfo>,
    rows: Vec<Row>,
    stop_at_columns: bool,
    stop_after: Option<usize>,
}

impl RowSink for Rows {
    fn columns(&mut self, columns: Vec<ColumnInfo>) -> SinkControl {
        self.columns = columns;
        if self.stop_at_columns {
            SinkControl::Stop
        } else {
            SinkControl::Continue
        }
    }

    fn row(&mut self, row: Row) -> SinkControl {
        assert!(!self.columns.is_empty(), "columns must precede rows");
        self.rows.push(row);
        if self
            .stop_after
            .is_some_and(|limit| self.rows.len() >= limit)
        {
            SinkControl::Stop
        } else {
            SinkControl::Continue
        }
    }
}

#[derive(Default)]
struct Metadata {
    batches: Vec<Vec<MetadataObject>>,
    reject: bool,
}

#[async_trait]
impl IntrospectionSink for Metadata {
    async fn batch(&mut self, objects: Vec<MetadataObject>) -> DriverResult<()> {
        if self.reject {
            return Err(DriverError::new(ErrorCode::Internal, "Sink rejected batch"));
        }
        self.batches.push(objects);
        Ok(())
    }
}

fn connection() -> ResolvedConnection {
    ResolvedConnection {
        config: ConnectionConfig {
            driver: DriverKind::Postgres,
            options: json!({}),
        },
        credentials: Credentials::default(),
    }
}

fn request() -> QueryRequest {
    QueryRequest {
        sql: "SELECT value FROM sales".into(),
        options: ExecutionOptions {
            context: SessionContext {
                catalog: Some("analytics".into()),
                schema: Some("public".into()),
            },
            ..ExecutionOptions::default()
        },
    }
}

#[test]
fn registry_rejects_duplicates_without_replacing_the_original() {
    let original: Arc<dyn Driver> = Arc::new(TestDriver::new("Original"));
    let mut registry = DriverRegistry::default();
    assert!(registry.descriptors().is_empty());
    assert_eq!(
        registry.get(DriverKind::Postgres).unwrap_err().code,
        ErrorCode::DriverNotRegistered
    );
    registry.register(original.clone()).unwrap();
    let duplicate = registry
        .register(Arc::new(TestDriver::new("Replacement")))
        .unwrap_err();
    assert_eq!(duplicate.code, ErrorCode::DuplicateDriver);
    assert!(Arc::ptr_eq(
        &original,
        &registry.get(DriverKind::Postgres).unwrap()
    ));
    assert_eq!(registry.descriptors()[0].display_name, "Original");
    assert_eq!(
        registry.get(DriverKind::MySql).unwrap_err().code,
        ErrorCode::DriverNotRegistered
    );
}

#[test]
fn credentials_are_redacted_and_configuration_round_trips_without_them() {
    let mut connection = connection();
    connection.credentials.password =
        Some(SecretString::from("unique-password-sentinel".to_owned()));
    let debug = format!("{connection:?}");
    assert!(!debug.contains("unique-password-sentinel"));
    assert!(!format!("{:?}", connection.credentials).contains("unique-password-sentinel"));
    let serialized = serde_json::to_string(&connection.config).unwrap();
    assert!(!serialized.contains("password"));
    assert_eq!(
        serde_json::from_str::<ConnectionConfig>(&serialized).unwrap(),
        connection.config
    );
    assert!(
        serde_json::from_value::<ConnectionConfig>(json!({
            "driver": "postgres", "options": {}, "password": "secret"
        }))
        .is_err()
    );
}

#[test]
fn metadata_paths_preserve_exact_names_and_object_kinds() {
    let root = ObjectPath::default();
    let schema = root.child(ObjectKind::Schema, "a.b");
    let table = MetadataObject::new(ObjectKind::Table, &schema, "sales/2026:%\\");
    let column = MetadataObject::new(ObjectKind::Column, &table.path, "Preço.総額");
    let encoded = serde_json::to_string(&column).unwrap();
    assert_eq!(
        serde_json::from_str::<MetadataObject>(&encoded).unwrap(),
        column
    );
    assert_eq!(column.path.parent(), Some(table.path.clone()));
    assert_eq!(schema.parent(), Some(root.clone()));
    assert_eq!(root.parent(), None);
    assert_ne!(table.path, schema.child(ObjectKind::View, table.name));
    assert_ne!(
        schema,
        root.child(ObjectKind::Schema, "a")
            .child(ObjectKind::Schema, "b")
    );
}

#[test]
fn query_validation_rejects_empty_sql_and_invalid_limits() {
    let mut query = request();
    query.validate().unwrap();
    query.options.max_rows = 0;
    assert_eq!(query.validate().unwrap_err().code, ErrorCode::Validation);
    query.options.max_rows = 1;
    query.options.timeout = Some(Duration::ZERO);
    assert_eq!(query.validate().unwrap_err().code, ErrorCode::Validation);
    query.options.timeout = None;
    query.validate().unwrap();
    query.sql = " \n\t".into();
    assert_eq!(query.validate().unwrap_err().code, ErrorCode::Validation);
}

#[tokio::test]
async fn dynamic_driver_and_session_forward_context_stream_values_and_close() {
    let implementation = TestDriver::new("Test");
    let observed = implementation.requests.clone();
    let closed = implementation.closed.clone();
    let driver: Arc<dyn Driver> = Arc::new(implementation);
    assert_eq!(
        driver
            .test(&connection(), CancellationToken::new())
            .await
            .unwrap()
            .version
            .as_deref(),
        Some("fixture")
    );
    let mut session: Box<dyn Session> = driver
        .open_session(&connection(), CancellationToken::new())
        .await
        .unwrap();
    let mut rows = Rows::default();
    let query = request();
    session
        .execute(&query, &mut rows, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(observed.lock().unwrap().as_slice(), &[query]);
    assert_eq!(rows.columns.len(), 1);
    assert_eq!(
        rows.rows,
        vec![
            vec![Cell::Decimal("9007199254740993.01".into())],
            vec![Cell::Null]
        ]
    );
    assert_ne!(Cell::Null, Cell::Text("NULL".into()));
    assert_ne!(Cell::Null, Cell::Text(String::new()));
    session.close().await.unwrap();
    assert!(closed.load(Ordering::SeqCst));
}

#[tokio::test]
async fn receivers_can_stop_at_columns_or_rows_and_requests_can_limit_rows() {
    let driver: Arc<dyn Driver> = Arc::new(TestDriver::new("Test"));
    let mut session = driver
        .open_session(&connection(), CancellationToken::new())
        .await
        .unwrap();
    let mut stopped = Rows {
        stop_at_columns: true,
        ..Rows::default()
    };
    session
        .execute(&request(), &mut stopped, CancellationToken::new())
        .await
        .unwrap();
    assert!(stopped.rows.is_empty());
    let mut stopped = Rows {
        stop_after: Some(1),
        ..Rows::default()
    };
    session
        .execute(&request(), &mut stopped, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(stopped.rows.len(), 1);
    let mut limited = request();
    limited.options.max_rows = 1;
    let mut rows = Rows::default();
    session
        .execute(&limited, &mut rows, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(rows.rows.len(), 1);
}

#[tokio::test]
async fn cancelled_execution_delivers_no_data() {
    let driver: Arc<dyn Driver> = Arc::new(TestDriver::new("Test"));
    let mut session = driver
        .open_session(&connection(), CancellationToken::new())
        .await
        .unwrap();
    let cancel = CancellationToken::new();
    cancel.cancel();
    let mut rows = Rows::default();
    assert_eq!(
        session
            .execute(&request(), &mut rows, cancel)
            .await
            .unwrap_err()
            .code,
        ErrorCode::Cancelled
    );
    assert!(rows.columns.is_empty());
    assert!(rows.rows.is_empty());
}

#[tokio::test]
async fn dynamic_introspection_delivers_batches_and_propagates_sink_failures() {
    let driver: Arc<dyn Driver> = Arc::new(TestDriver::new("Test"));
    let mut metadata = Metadata::default();
    driver
        .introspect(&connection(), &mut metadata, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(metadata.batches.len(), 2);
    assert_eq!(
        metadata.batches[1][0].path.parent(),
        Some(metadata.batches[0][0].path.clone())
    );
    let mut rejecting = Metadata {
        reject: true,
        ..Metadata::default()
    };
    assert_eq!(
        driver
            .introspect(&connection(), &mut rejecting, CancellationToken::new())
            .await
            .unwrap_err()
            .code,
        ErrorCode::Internal
    );
    assert!(rejecting.batches.is_empty());
}

#[test]
fn scopes_are_structural_and_validate_driver_hierarchies() {
    let hierarchy = [
        ObjectKind::Database,
        ObjectKind::Schema,
        ObjectKind::Table,
        ObjectKind::View,
        ObjectKind::Column,
    ];
    let path = ObjectPath::default()
        .child(ObjectKind::Database, "db.a")
        .child(ObjectKind::Schema, "sch")
        .child(ObjectKind::View, "view");
    path.validate_scope(&hierarchy).unwrap();
    assert!(path.child(ObjectKind::Column, "a.b").is_within(&path));
    assert!(!path.is_within(&ObjectPath::default().child(ObjectKind::Database, "db")));
    assert!(
        ObjectPath::default()
            .child(ObjectKind::Table, "x")
            .validate_scope(&hierarchy)
            .is_err()
    );
    assert!(
        path.child(ObjectKind::Schema, "wrong")
            .validate_scope(&hierarchy)
            .is_err()
    );
}

#[test]
fn generated_names_quote_every_structural_segment_with_the_driver_quote() {
    let driver = TestDriver::new("test");
    assert_eq!(driver.quote_identifier("a\"b.c"), "\"a\"\"b.c\"");
    assert_eq!(driver.quote_identifier(""), "\"\"");
    let path = ObjectPath::default()
        .child(ObjectKind::Catalog, "c.1")
        .child(ObjectKind::Schema, "s")
        .child(ObjectKind::Table, "t")
        .child(ObjectKind::Column, "ignored");
    assert_eq!(driver.relation_name(&path), "\"c.1\".\"s\".\"t\"");
}

#[tokio::test]
async fn namespace_levels_end_at_relations_and_discovery_defaults_to_not_implemented() {
    let capabilities = |hierarchy: Vec<ObjectKind>| DriverCapabilities {
        sql: true,
        metadata: true,
        transactions: false,
        hierarchy,
        default_port: None,
        identifier_quote: '"',
    };
    use ObjectKind::*;
    assert_eq!(
        capabilities(vec![Database, Schema, Table, View, Column]).namespace_levels(),
        [Database, Schema]
    );
    assert_eq!(
        capabilities(vec![Catalog, Schema, Table, View, Column]).namespace_levels(),
        [Catalog, Schema]
    );
    assert_eq!(
        capabilities(vec![Database, Table, View, Column]).namespace_levels(),
        [Database]
    );
    assert!(
        capabilities(vec![Table, Column])
            .namespace_levels()
            .is_empty()
    );
    let driver: Arc<dyn Driver> = Arc::new(TestDriver::new("Test"));
    let error = driver
        .discover_namespaces(
            &connection(),
            &ObjectPath::default(),
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::NotImplemented);
}

#[test]
fn objects_cached_before_keys_existed_still_read_and_new_kinds_keep_their_level() {
    let old = json!({
        "kind": "column",
        "path": [{"kind": "table", "name": "orders"}, {"kind": "column", "name": "id"}],
        "name": "id", "ordinal": 1, "data_type": "integer", "nullable": false, "comment": null
    });
    let column: MetadataObject = serde_json::from_value(old).unwrap();
    assert_eq!(column.primary_key, None);
    assert_eq!(column.references, None);
    // Empty additions are left out of the stored form.
    let stored = serde_json::to_value(&column).unwrap();
    assert!(stored.get("primary_key").is_none() && stored.get("references").is_none());
    let hierarchy = [
        ObjectKind::Database,
        ObjectKind::Schema,
        ObjectKind::Table,
        ObjectKind::View,
        ObjectKind::Column,
    ];
    let schema = ObjectPath::default()
        .child(ObjectKind::Database, "db")
        .child(ObjectKind::Schema, "sales");
    let summary = schema.child(ObjectKind::MaterializedView, "summary");
    summary.validate_scope(&hierarchy).unwrap();
    assert_eq!(
        serde_json::to_value(ObjectKind::MaterializedView).unwrap(),
        "materialized_view"
    );
    assert_eq!(summary.relation(), Some("summary"));
    // A column or key reads its table; a partition reads itself.
    let events = schema.child(ObjectKind::Table, "events");
    assert_eq!(
        events.child(ObjectKind::ForeignKey, "fk").relation_path(),
        Some(events.clone())
    );
    assert_eq!(
        events
            .child(ObjectKind::Partition, "events_2026")
            .relation_path(),
        Some(schema.child(ObjectKind::Table, "events_2026"))
    );
    assert_eq!(schema.relation_path(), None);
    let driver = TestDriver::new("Test");
    assert_eq!(
        driver.relation_name(&events.child(ObjectKind::Partition, "events_2026")),
        "\"db\".\"sales\".\"events_2026\""
    );
    assert!(driver.default_namespaces(&connection().config).is_empty());
}

#[tokio::test]
async fn a_session_that_knows_no_context_reports_an_empty_one() {
    let mut session: Box<dyn Session> = Box::new(TestSession {
        requests: Arc::new(Mutex::new(Vec::new())),
        closed: Arc::new(AtomicBool::new(false)),
    });
    assert_eq!(
        session.context(CancellationToken::new()).await.unwrap(),
        SessionContext::default()
    );
}
