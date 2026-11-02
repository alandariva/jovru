use jykre_core::*;
use jykre_driver_postgres::{PostgresConfig, PostgresDriver};
use serde_json::{Value, json};

fn options() -> Value {
    json!({"host": "localhost", "database": "commerce", "user": "postgres", "sslMode": "require"})
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
) -> [ErrorCode; 3] {
    let driver: &dyn Driver = &PostgresDriver;
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
            .introspect(connection, &mut sink, cancel)
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
            driver: DriverKind::Postgres,
            options,
        },
        credentials: Credentials::default(),
    }
}

#[test]
fn configuration_defaults_only_the_port_and_round_trips() {
    let config = PostgresConfig::parse(&options()).unwrap();
    assert_eq!(config.port, 5432);
    assert_eq!(
        PostgresConfig::parse(&serde_json::to_value(&config).unwrap()).unwrap(),
        config
    );
    let mut custom = options();
    custom["host"] = json!("::1");
    custom["port"] = json!(55432);
    assert_eq!(PostgresConfig::parse(&custom).unwrap().port, 55432);
    let mut no_tls = options();
    no_tls.as_object_mut().unwrap().remove("sslMode");
    assert_eq!(
        PostgresConfig::parse(&no_tls).unwrap_err().code,
        ErrorCode::Validation
    );
}

#[test]
fn invalid_configuration_and_embedded_credentials_are_rejected_without_echoing_input() {
    for (field, value) in [
        ("host", json!(" ")),
        ("host", json!("postgres://user:private-sentinel@host/db")),
        ("host", json!("user:private-sentinel@host")),
        ("port", json!(0)),
        ("port", json!(65536)),
        ("database", json!("")),
        ("user", json!("\n")),
        ("sslMode", json!("invalid")),
        ("password", json!("private-sentinel")),
    ] {
        let mut input = options();
        input[field] = value;
        let error = PostgresConfig::parse(&input).unwrap_err();
        assert_eq!(error.code, ErrorCode::Validation);
        assert!(!format!("{error:?}").contains("private-sentinel"));
    }
    let mut missing_database = options();
    missing_database.as_object_mut().unwrap().remove("database");
    assert!(PostgresConfig::parse(&missing_database).is_err());
}

#[tokio::test]
async fn external_operations_validate_before_io_and_honor_cancellation() {
    let valid = connection(options());

    assert_eq!(
        operation_codes(&connection(json!({})), CancellationToken::new()).await,
        [ErrorCode::Validation; 3]
    );
    let mut wrong_driver = connection(options());
    wrong_driver.config.driver = DriverKind::MySql;
    assert_eq!(
        operation_codes(&wrong_driver, CancellationToken::new()).await,
        [ErrorCode::Validation; 3]
    );
    let cancel = CancellationToken::new();
    cancel.cancel();
    assert_eq!(
        operation_codes(&valid, cancel).await,
        [ErrorCode::Cancelled; 3]
    );
}
