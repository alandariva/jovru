#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]
use jykre_core::*;
use jykre_driver_sqlserver::{SqlServerConfig, SqlServerDriver, SqlServerEncryption};
use serde_json::{Value, json};

fn options() -> Value {
    json!({"host": "localhost", "user": "app", "encrypt": "mandatory"})
}

#[derive(Default)]
struct MetadataCounter(usize);

#[async_trait]
impl IntrospectionSink for MetadataCounter {
    async fn batch(&mut self, _: Vec<MetadataObject>) -> DriverResult<()> {
        self.0 += 1;
        Ok(())
    }
}

async fn operation_codes(
    connection: &ResolvedConnection,
    cancel: CancellationToken,
) -> [ErrorCode; 4] {
    let driver: &dyn Driver = &SqlServerDriver;
    let mut sink = MetadataCounter::default();
    let codes = [
        driver
            .test(connection, cancel.clone())
            .await
            .unwrap_err()
            .code,
        driver
            .open_session(connection, cancel.clone())
            .await
            .unwrap_err()
            .code,
        driver
            .introspect(connection, &mut sink, cancel.clone())
            .await
            .unwrap_err()
            .code,
        driver
            .discover_namespaces(connection, &ObjectPath::default(), cancel)
            .await
            .unwrap_err()
            .code,
    ];
    assert_eq!(sink.0, 0);
    codes
}

fn connection(options: Value) -> ResolvedConnection {
    ResolvedConnection {
        config: ConnectionConfig {
            driver: DriverKind::SqlServer,
            options,
        },
        credentials: Credentials::default(),
    }
}

#[test]
fn configuration_defaults_the_port_and_requires_an_explicit_encryption() {
    let config = SqlServerConfig::parse(&options()).unwrap();
    assert_eq!(config.port, 1433);
    assert_eq!(config.database, None);
    assert_eq!(config.encrypt, SqlServerEncryption::Mandatory);
    assert!(!config.trust_server_certificate);
    // A certificate check left on is not written out.
    assert_eq!(
        serde_json::to_value(&config)
            .unwrap()
            .get("trustServerCertificate"),
        None
    );
    assert_eq!(
        SqlServerConfig::parse(&serde_json::to_value(&config).unwrap()).unwrap(),
        config
    );
    let mut custom = options();
    custom["port"] = json!(11433);
    custom["database"] = json!("commerce");
    custom["encrypt"] = json!("strict");
    custom["trustServerCertificate"] = json!(true);
    let config = SqlServerConfig::parse(&custom).unwrap();
    assert_eq!(config.port, 11433);
    assert_eq!(config.database.as_deref(), Some("commerce"));
    assert_eq!(config.encrypt, SqlServerEncryption::Strict);
    assert!(config.trust_server_certificate);
    let mut no_tls = options();
    no_tls.as_object_mut().unwrap().remove("encrypt");
    assert_eq!(
        SqlServerConfig::parse(&no_tls).unwrap_err().code,
        ErrorCode::Validation
    );
}

#[test]
fn invalid_configuration_and_embedded_credentials_are_rejected_without_echoing_input() {
    for (field, value) in [
        ("host", json!("")),
        ("host", json!("sqlserver://user:private-sentinel@host")),
        ("host", json!("user:private-sentinel@host")),
        ("host", json!("server\\private-sentinel")),
        ("host", json!("host,1433")),
        ("port", json!(0)),
        ("port", json!(-1)),
        ("database", json!(" ")),
        ("user", json!("")),
        ("encrypt", json!("private-sentinel")),
        ("password", json!("private-sentinel")),
    ] {
        let mut input = options();
        input[field] = value;
        let error = SqlServerConfig::parse(&input).unwrap_err();
        assert_eq!(error.code, ErrorCode::Validation);
        assert!(!format!("{error:?}").contains("private-sentinel"));
    }
}

#[test]
fn descriptor_names_databases_then_schemas_and_quotes_like_the_standard() {
    let driver = SqlServerDriver;
    let descriptor = driver.descriptor();
    assert_eq!(descriptor.kind, DriverKind::SqlServer);
    assert_eq!(descriptor.dialect, SqlDialect::SqlServer);
    assert_eq!(descriptor.status, DriverStatus::Ready);
    assert_eq!(descriptor.capabilities.default_port, Some(1433));
    assert_eq!(
        descriptor.capabilities.namespace_levels(),
        [ObjectKind::Database, ObjectKind::Schema]
    );
    assert_eq!(driver.quote_identifier("a\"b"), "\"a\"\"b\"");
    let table = ObjectPath::default()
        .child(ObjectKind::Database, "sales")
        .child(ObjectKind::Schema, "dbo")
        .child(ObjectKind::Table, "orders");
    assert_eq!(driver.relation_name(&table), "\"sales\".\"dbo\".\"orders\"");
    let mut config = ConnectionConfig {
        driver: DriverKind::SqlServer,
        options: options(),
    };
    assert!(driver.default_namespaces(&config).is_empty());
    config.options["database"] = json!("sales");
    assert_eq!(
        driver.default_namespaces(&config),
        [table.parent().unwrap()]
    );
    // The default database and TLS do not change what the catalog shows.
    let identity = driver.cache_identity(&config);
    config.options["encrypt"] = json!("strict");
    config.options["database"] = json!("other");
    assert_eq!(driver.cache_identity(&config), identity);
}

#[tokio::test]
async fn external_operations_validate_before_io_and_honor_cancellation() {
    let valid = connection(options());
    assert_eq!(
        operation_codes(&connection(json!({})), CancellationToken::new()).await,
        [ErrorCode::Validation; 4]
    );
    let mut wrong_driver = connection(options());
    wrong_driver.config.driver = DriverKind::Postgres;
    assert_eq!(
        operation_codes(&wrong_driver, CancellationToken::new()).await,
        [ErrorCode::Validation; 4]
    );
    let cancel = CancellationToken::new();
    cancel.cancel();
    assert_eq!(
        operation_codes(&valid, cancel).await,
        [ErrorCode::Cancelled; 4]
    );
}
