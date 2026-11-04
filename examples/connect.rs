//! Configuration through JYKRE_CONNECTION (non-sensitive JSON) and
//! JYKRE_PASSWORD; optionally JYKRE_SQL and JYKRE_METADATA=1.
// A command-line example: printing what the driver returns is its output.
#![allow(clippy::print_stdout)]
use jykre::drivers::*;

struct Output;
impl RowSink for Output {
    fn columns(&mut self, columns: Vec<ColumnInfo>) -> SinkControl {
        println!("Columns: {columns:?}");
        SinkControl::Continue
    }
    fn row(&mut self, row: Row) -> SinkControl {
        println!("{row:?}");
        SinkControl::Continue
    }
}
#[derive(Default)]
struct MetadataCount(usize);
#[async_trait]
impl IntrospectionSink for MetadataCount {
    async fn batch(&mut self, objects: Vec<MetadataObject>) -> DriverResult<()> {
        self.0 += objects.len();
        Ok(())
    }
}
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let json = std::env::var("JYKRE_CONNECTION")
        .map_err(|_| "Set JYKRE_CONNECTION to a non-sensitive ConnectionConfig JSON")?;
    let config: ConnectionConfig =
        serde_json::from_str(&json).map_err(|_| "Invalid connection configuration JSON")?;
    let driver = builtin_registry()?.get(config.driver)?;
    driver.validate_connection(&config)?;
    let input = ResolvedConnection {
        config,
        credentials: Credentials {
            password: std::env::var("JYKRE_PASSWORD").ok().map(SecretString::from),
        },
    };
    let cancel = CancellationToken::new();
    println!(
        "Connected: {:?}",
        driver.test(&input, cancel.clone()).await?
    );
    if std::env::var("JYKRE_METADATA").as_deref() == Ok("1") {
        let mut count = MetadataCount::default();
        let report = driver
            .introspect(&input, &mut count, cancel.clone())
            .await?;
        println!(
            "Metadata: {} objects, {} warnings",
            count.0,
            report.warnings.len()
        );
    }
    if let Ok(sql) = std::env::var("JYKRE_SQL") {
        let mut session = driver.open_session(&input, cancel.clone()).await?;
        let outcome = session
            .execute(
                &QueryRequest {
                    parameters: Vec::new(),
                    sql,
                    options: ExecutionOptions::default(),
                },
                &mut Output,
                cancel,
            )
            .await;
        session.close().await?;
        println!("Execution: {:?}", outcome?);
    }
    Ok(())
}
