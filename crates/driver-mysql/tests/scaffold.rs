use jykre_core::*;
use jykre_driver_mysql::{MySqlConfig, MySqlDriver};
use serde_json::{Value, json};

fn options() -> Value {
    json!({"host": "localhost", "user": "app", "sslMode": "required"})
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
    let driver: &dyn Driver = &MySqlDriver;
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
            driver: DriverKind::MySql,
            options,
        },
        credentials: Credentials::default(),
    }
}

#[test]
fn configuration_defaults_the_port_and_allows_no_database() {
    let config = MySqlConfig::parse(&options()).unwrap();
    assert_eq!(config.port, 3306);
    assert_eq!(config.database, None);
    assert_eq!(
        MySqlConfig::parse(&serde_json::to_value(&config).unwrap()).unwrap(),
        config
    );
    let mut custom = options();
    custom["port"] = json!(53306);
    custom["database"] = json!("commerce");
    let config = MySqlConfig::parse(&custom).unwrap();
    assert_eq!(config.port, 53306);
    assert_eq!(config.database.as_deref(), Some("commerce"));
    let mut no_tls = options();
    no_tls.as_object_mut().unwrap().remove("sslMode");
    assert_eq!(
        MySqlConfig::parse(&no_tls).unwrap_err().code,
        ErrorCode::Validation
    );
}

#[test]
fn invalid_configuration_and_embedded_credentials_are_rejected_without_echoing_input() {
    for (field, value) in [
        ("host", json!("")),
        ("host", json!("mysql://user:private-sentinel@host/db")),
        ("host", json!("user:private-sentinel@host")),
        ("port", json!(0)),
        ("port", json!(-1)),
        ("database", json!(" ")),
        ("user", json!("")),
        ("sslMode", json!("invalid")),
        ("password", json!("private-sentinel")),
    ] {
        let mut input = options();
        input[field] = value;
        let error = MySqlConfig::parse(&input).unwrap_err();
        assert_eq!(error.code, ErrorCode::Validation);
        assert!(!format!("{error:?}").contains("private-sentinel"));
    }
}

#[tokio::test]
async fn external_operations_validate_before_io_and_honor_cancellation() {
    let valid = connection(options());

    assert_eq!(
        operation_codes(&connection(json!({})), CancellationToken::new()).await,
        [ErrorCode::Validation; 3]
    );
    let mut wrong_driver = connection(options());
    wrong_driver.config.driver = DriverKind::Postgres;
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
