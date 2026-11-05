#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]
use jykre_core::*;
use jykre_driver_databricks::{ComputeKind, DatabricksConfig, DatabricksDriver};
use serde_json::{Value, json};

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
    let driver: &dyn Driver = &DatabricksDriver;
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
            driver: DriverKind::Databricks,
            options,
        },
        credentials: Credentials::default(),
    }
}

#[test]
fn metadata_configuration_does_not_require_a_warehouse() {
    let config = DatabricksConfig::parse(&json!({"profile": "DEV"})).unwrap();
    assert_eq!(config.warehouse_id, None);
    assert_eq!(config.cli_path, None);
    assert_eq!(config.compute, ComputeKind::Warehouse);
    assert_eq!(
        config.validate_sql().unwrap_err().code,
        ErrorCode::Validation
    );
    let config = DatabricksConfig::parse(&json!({
        "profile": "DEV", "warehouseId": "abc123", "cliPath": "C:/tools/databricks.exe",
        "defaultCatalog": "analytics", "defaultSchema": "sales"
    }))
    .unwrap();
    config.validate_sql().unwrap();
    assert_eq!(
        DatabricksConfig::parse(&serde_json::to_value(&config).unwrap()).unwrap(),
        config
    );
}

#[test]
fn invalid_configuration_and_credentials_are_rejected_without_echoing_input() {
    for (field, value) in [
        ("profile", json!(" ")),
        ("profile", json!("DEV\n")),
        ("cliPath", json!("")),
        ("warehouseId", json!("")),
        ("defaultCatalog", json!(" ")),
        ("defaultSchema", json!("")),
        ("token", json!("private-sentinel")),
        ("host", json!("https://private-sentinel")),
        ("password", json!("private-sentinel")),
    ] {
        let mut input = json!({"profile": "DEV"});
        input[field] = value;
        let error = DatabricksConfig::parse(&input).unwrap_err();
        assert_eq!(error.code, ErrorCode::Validation);
        assert!(!format!("{error:?}").contains("private-sentinel"));
    }
    assert!(DatabricksConfig::parse(&json!({})).is_err());
}

#[test]
fn classic_cluster_configuration_roundtrips_and_excludes_warehouse_ids() {
    let input = json!({"profile":"DEV","compute":"cluster","clusterId":"1234-cluster","computeName":"Analytics",
        "cliPath":"C:/tools/databricks.exe","defaultCatalog":"main","defaultSchema":"sales"});
    let config = DatabricksConfig::parse(&input).unwrap();
    config.validate_sql().unwrap();
    assert_eq!(config.compute_id(), Some("1234-cluster"));
    assert_eq!(
        DatabricksConfig::parse(&serde_json::to_value(&config).unwrap()).unwrap(),
        config
    );
    for invalid in [
        json!({"profile":"DEV","clusterId":"cluster"}),
        json!({"profile":"DEV","compute":"cluster","warehouseId":"warehouse"}),
        json!({"profile":"DEV","compute":"cluster","clusterId":"cluster","warehouseId":"warehouse"}),
        json!({"profile":"DEV","compute":"cluster","clusterId":"../private-sentinel"}),
        json!({"profile":"DEV","compute":"unknown"}),
    ] {
        let failure = DatabricksConfig::parse(&invalid).unwrap_err();
        assert_eq!(failure.code, ErrorCode::Validation);
        assert!(!format!("{failure:?}").contains("private-sentinel"));
    }
    assert!(
        DatabricksConfig::parse(&json!({"profile":"DEV","compute":"cluster"}))
            .unwrap()
            .validate_sql()
            .is_err()
    );
}

#[tokio::test]
async fn external_operations_validate_before_io_and_honor_cancellation() {
    let valid = connection(json!({"profile": "DEV", "warehouseId": "abc123"}));

    assert_eq!(
        operation_codes(&connection(json!({})), CancellationToken::new()).await,
        [ErrorCode::Validation; 3]
    );
    let mut wrong_driver = connection(json!({"profile": "DEV"}));
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
