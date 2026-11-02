use crate::DatabricksConfig;
use jykre_core::*;
use reqwest::{Method, Url};
use secrecy::ExposeSecret;
use serde_json::{Value, json};
use std::{collections::HashSet, process::Stdio, time::Duration};
use tokio::{io::AsyncReadExt, process::Command};

fn error(code: ErrorCode, message: &str) -> DriverError {
    DriverError::new(code, message)
}
fn protocol() -> DriverError {
    error(ErrorCode::Protocol, "Invalid Databricks API response")
}
async fn bounded<T>(
    cancel: &CancellationToken,
    limit: Duration,
    work: impl std::future::Future<Output = DriverResult<T>>,
) -> DriverResult<T> {
    tokio::select! { biased; _=cancel.cancelled()=>Err(DriverError::cancelled()), result=tokio::time::timeout(limit,work)=>result.map_err(|_|error(ErrorCode::Timeout,"Databricks operation timed out"))? }
}

async fn cli(config: &DatabricksConfig) -> DriverResult<SecretString> {
    let executable = config
        .cli_path
        .clone()
        .unwrap_or_else(|| "databricks".into());
    if executable
        .extension()
        .is_some_and(|v| v.eq_ignore_ascii_case("bat") || v.eq_ignore_ascii_case("cmd"))
    {
        return Err(DriverError::validation(
            "Use the native Databricks executable",
        ));
    }
    let mut command = Command::new(executable);
    command
        .args(["auth", "token", "--profile", &config.profile])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    #[cfg(windows)]
    command.creation_flags(0x08000000);
    let mut child = command.spawn().map_err(|_| {
        error(
            ErrorCode::Authentication,
            "Cannot launch Databricks CLI; install it and authenticate the profile",
        )
    })?;
    let stdout = child.stdout.take().ok_or_else(protocol)?;
    let mut bytes = Vec::new();
    stdout
        .take(65537)
        .read_to_end(&mut bytes)
        .await
        .map_err(|_| protocol())?;
    if bytes.len() > 65536 {
        return Err(protocol());
    }
    if !child.wait().await.map_err(|_| protocol())?.success() {
        return Err(error(
            ErrorCode::Authentication,
            "Databricks CLI profile requires authentication",
        ));
    }
    let mut value: Value = serde_json::from_slice(&bytes).map_err(|_| protocol())?;
    let token = value["access_token"].take();
    let token = token
        .as_str()
        .filter(|v| !v.is_empty())
        .ok_or_else(protocol)?;
    Ok(SecretString::from(token.to_owned()))
}

#[derive(Debug)]
struct Client {
    config: DatabricksConfig,
    origin: Url,
    http: reqwest::Client,
    #[cfg(test)]
    token: Option<SecretString>,
}
impl Client {
    async fn new(input: &ResolvedConnection) -> DriverResult<Self> {
        let config = DatabricksConfig::parse(&input.config.options)?;
        let path = std::env::var_os("DATABRICKS_CONFIG_FILE")
            .map(std::path::PathBuf::from)
            .or_else(|| {
                std::env::var_os("USERPROFILE")
                    .or_else(|| std::env::var_os("HOME"))
                    .map(|v| std::path::PathBuf::from(v).join(".databrickscfg"))
            })
            .ok_or_else(|| {
                error(
                    ErrorCode::Validation,
                    "Cannot locate Databricks CLI configuration",
                )
            })?;
        let content = tokio::fs::read_to_string(path).await.map_err(|_| {
            error(
                ErrorCode::Authentication,
                "Cannot read Databricks CLI profiles",
            )
        })?;
        let mut selected = false;
        let mut host = None;
        for line in content.lines().map(str::trim) {
            if line.starts_with('#') || line.starts_with(';') {
                continue;
            }
            if let Some(section) = line.strip_prefix('[').and_then(|v| v.strip_suffix(']')) {
                selected = section.trim() == config.profile;
            } else if selected
                && let Some((key, value)) = line.split_once('=')
                && key.trim() == "host"
            {
                host = Some(value.trim().to_owned());
            }
        }
        let host = host.ok_or_else(|| {
            error(
                ErrorCode::Validation,
                "CLI profile must specify a workspace host",
            )
        })?;
        let host = if host.contains("://") {
            host
        } else {
            format!("https://{host}")
        };
        let origin = Url::parse(&host).map_err(|_| protocol())?;
        if origin.scheme() != "https"
            || origin.host_str().is_none()
            || origin.path() != "/"
            || !origin.username().is_empty()
            || origin.password().is_some()
            || origin.query().is_some()
            || origin.fragment().is_some()
        {
            return Err(DriverError::validation(
                "Databricks host must be an HTTPS workspace origin",
            ));
        }
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|_| protocol())?;
        Ok(Self {
            config,
            origin,
            http,
            #[cfg(test)]
            token: None,
        })
    }
    async fn token(&self) -> DriverResult<SecretString> {
        #[cfg(test)]
        if let Some(token) = &self.token {
            return Ok(token.clone());
        }
        tokio::time::timeout(Duration::from_secs(60), cli(&self.config))
            .await
            .map_err(|_| error(ErrorCode::Timeout, "CLI authentication timed out"))?
    }
    async fn request(
        &self,
        method: Method,
        path: &str,
        query: &[(String, String)],
        body: Option<Value>,
    ) -> DriverResult<Value> {
        if !path.starts_with("/api/") || path.contains("..") || path.contains('\\') {
            return Err(protocol());
        }
        let url = self.origin.join(path).map_err(|_| protocol())?;
        if url.origin() != self.origin.origin() {
            return Err(protocol());
        }
        let token = self.token().await?;
        let mut request = self
            .http
            .request(method, url)
            .query(query)
            .bearer_auth(token.expose_secret());
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request.send().await.map_err(|e| {
            error(
                if e.is_timeout() {
                    ErrorCode::Timeout
                } else {
                    ErrorCode::Connection
                },
                "Databricks request failed",
            )
        })?;
        if !response.status().is_success() {
            return Err(error(
                match response.status().as_u16() {
                    401 => ErrorCode::Authentication,
                    403 => ErrorCode::PermissionDenied,
                    404 => ErrorCode::NotFound,
                    429 => ErrorCode::Connection,
                    _ => ErrorCode::Database,
                },
                "Databricks rejected the API request",
            ));
        }
        response.json().await.map_err(|_| protocol())
    }
    async fn pages(
        &self,
        path: &str,
        key: &str,
        mut query: Vec<(String, String)>,
    ) -> DriverResult<Vec<Value>> {
        query.push(("max_results".into(), "100".into()));
        let mut result = Vec::new();
        let mut seen = HashSet::new();
        loop {
            let value = self.request(Method::GET, path, &query, None).await?;
            if let Some(items) = value.get(key) {
                result.extend(items.as_array().ok_or_else(protocol)?.iter().cloned());
            }
            let Some(next) = value["next_page_token"].as_str().filter(|v| !v.is_empty()) else {
                break;
            };
            if !seen.insert(next.to_owned()) {
                return Err(protocol());
            }
            query.retain(|(key, _)| key != "page_token");
            query.push(("page_token".into(), next.into()));
        }
        Ok(result)
    }
}
pub async fn test(
    input: &ResolvedConnection,
    cancel: CancellationToken,
) -> DriverResult<ServerInfo> {
    bounded(&cancel, Duration::from_secs(90), async {
        let client = Client::new(input).await?;
        client
            .request(
                Method::GET,
                "/api/2.1/unity-catalog/catalogs",
                &[("max_results".into(), "1".into())],
                None,
            )
            .await?;
        Ok(ServerInfo {
            version: Some("Databricks REST API".into()),
        })
    })
    .await
}
pub async fn open(
    input: &ResolvedConnection,
    cancel: CancellationToken,
) -> DriverResult<Box<dyn Session>> {
    bounded(&cancel, Duration::from_secs(90), async {
        let client = Client::new(input).await?;
        let _ = tokio::time::timeout(Duration::from_secs(60), cli(&client.config))
            .await
            .map_err(|_| error(ErrorCode::Timeout, "CLI timed out"))??;
        Ok(Box::new(DatabricksSession { client }) as Box<dyn Session>)
    })
    .await
}
fn name(value: &Value) -> DriverResult<&str> {
    value["name"]
        .as_str()
        .filter(|v| !v.is_empty())
        .ok_or_else(protocol)
}
pub async fn introspect(
    input: &ResolvedConnection,
    scope: &ObjectPath,
    sink: &mut dyn IntrospectionSink,
    cancel: CancellationToken,
) -> DriverResult<IntrospectionReport> {
    bounded(&cancel, Duration::from_secs(300), async {
        let client = Client::new(input).await?;
        client.introspect(scope, sink).await
    })
    .await
}

impl Client {
    async fn metadata_items(
        &self,
        endpoint: &str,
        key: &str,
        query: Vec<(String, String)>,
        selected: Option<String>,
    ) -> DriverResult<Vec<Value>> {
        if let Some(name) = selected {
            let mut url = self.origin.join(endpoint).map_err(|_| protocol())?;
            url.path_segments_mut().map_err(|_| protocol())?.push(&name);
            // Keep structural names opaque, including slashes and percent signs.
            return match self.request(Method::GET, url.path(), &[], None).await {
                Ok(value) => Ok(vec![value]),
                Err(error) if error.code == ErrorCode::NotFound => Ok(Vec::new()),
                Err(error) => Err(error),
            };
        }
        self.pages(endpoint, key, query).await
    }

    async fn introspect(
        &self,
        scope: &ObjectPath,
        sink: &mut dyn IntrospectionSink,
    ) -> DriverResult<IntrospectionReport> {
        let client = self;
        let mut report = IntrospectionReport::default();
        for catalog in client
            .metadata_items(
                "/api/2.1/unity-catalog/catalogs",
                "catalogs",
                vec![],
                scope.name_of(ObjectKind::Catalog).map(str::to_owned),
            )
            .await?
        {
            if name(&catalog)? == "hive_metastore"
                || scope
                    .name_of(ObjectKind::Catalog)
                    .is_some_and(|value| catalog["name"].as_str() != Some(value))
            {
                continue;
            }
            let cat =
                MetadataObject::new(ObjectKind::Catalog, &ObjectPath::default(), name(&catalog)?);
            if cat.path.is_within(scope) {
                sink.batch(vec![cat.clone()]).await?;
            }
            let schemas = match client
                .metadata_items(
                    "/api/2.1/unity-catalog/schemas",
                    "schemas",
                    vec![("catalog_name".into(), cat.name.clone())],
                    scope
                        .name_of(ObjectKind::Schema)
                        .map(|schema| format!("{}.{schema}", cat.name)),
                )
                .await
            {
                Ok(v) => v,
                Err(e) if e.code == ErrorCode::PermissionDenied => {
                    report.warnings.push(IntrospectionWarning {
                        path: cat.path,
                        message: e.message,
                    });
                    continue;
                }
                Err(e) => return Err(e),
            };
            for schema in schemas {
                if scope
                    .name_of(ObjectKind::Schema)
                    .is_some_and(|value| schema["name"].as_str() != Some(value))
                {
                    continue;
                }
                let sch = MetadataObject::new(ObjectKind::Schema, &cat.path, name(&schema)?);
                if sch.path.is_within(scope) {
                    sink.batch(vec![sch.clone()]).await?;
                }
                let tables = match client
                    .metadata_items(
                        "/api/2.1/unity-catalog/tables",
                        "tables",
                        vec![
                            ("catalog_name".into(), cat.name.clone()),
                            ("schema_name".into(), sch.name.clone()),
                        ],
                        scope
                            .name_of(ObjectKind::Table)
                            .or(scope.name_of(ObjectKind::View))
                            .map(|table| format!("{}.{}.{table}", cat.name, sch.name)),
                    )
                    .await
                {
                    Ok(v) => v,
                    Err(e) if e.code == ErrorCode::PermissionDenied => {
                        report.warnings.push(IntrospectionWarning {
                            path: sch.path,
                            message: e.message,
                        });
                        continue;
                    }
                    Err(e) => return Err(e),
                };
                for table in tables {
                    if scope
                        .name_of(ObjectKind::Table)
                        .or(scope.name_of(ObjectKind::View))
                        .is_some_and(|value| table["name"].as_str() != Some(value))
                    {
                        continue;
                    }
                    let mut rel = MetadataObject::new(
                        if table["table_type"]
                            .as_str()
                            .is_some_and(|v| v.contains("VIEW"))
                        {
                            ObjectKind::View
                        } else {
                            ObjectKind::Table
                        },
                        &sch.path,
                        name(&table)?,
                    );
                    rel.comment = table["comment"].as_str().map(str::to_owned);
                    let mut batch = vec![rel.clone()];
                    for column in table["columns"].as_array().into_iter().flatten() {
                        let mut object =
                            MetadataObject::new(ObjectKind::Column, &rel.path, name(column)?);
                        object.data_type = column["type_text"].as_str().map(str::to_owned);
                        object.nullable = column["nullable"].as_bool();
                        object.ordinal = column["position"]
                            .as_u64()
                            .and_then(|v| u32::try_from(v + 1).ok());
                        object.comment = column["comment"].as_str().map(str::to_owned);
                        batch.push(object);
                    }
                    batch.retain(|object| object.path.is_within(scope));
                    if !batch.is_empty() {
                        sink.batch(batch).await?;
                    }
                }
            }
        }
        Ok(report)
    }
}

#[derive(Debug)]
struct DatabricksSession {
    client: Client,
}
#[async_trait]
impl Session for DatabricksSession {
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
        let mut body = json!({"warehouse_id":self.client.config.warehouse_id,"statement":request.sql,"wait_timeout":"0s","format":"JSON_ARRAY","disposition":"INLINE","row_limit":request.options.max_rows});
        if let Some(catalog) = request.options.context.catalog.as_ref().or(self
            .client
            .config
            .default_catalog
            .as_ref())
        {
            body["catalog"] = json!(catalog);
        }
        if let Some(schema) = request.options.context.schema.as_ref().or(self
            .client
            .config
            .default_schema
            .as_ref())
        {
            body["schema"] = json!(schema);
        }
        let started = tokio::time::Instant::now();
        let mut response = self
            .client
            .request(Method::POST, "/api/2.0/sql/statements", &[], Some(body))
            .await?;
        let id = response["statement_id"]
            .as_str()
            .filter(|v| !v.is_empty() && v.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'))
            .ok_or_else(protocol)?
            .to_owned();
        let path = format!("/api/2.0/sql/statements/{id}");
        let limit = request
            .options
            .timeout
            .map(|v| v.saturating_sub(started.elapsed()));
        let work = async {
            loop {
                match response["status"]["state"].as_str() {
                    Some("SUCCEEDED") => break,
                    Some("PENDING" | "RUNNING") => {
                        tokio::time::sleep(Duration::from_secs(1)).await;
                        response = self.client.request(Method::GET, &path, &[], None).await?;
                    }
                    Some("CANCELED") => return Err(DriverError::cancelled()),
                    _ => {
                        return Err(error(
                            ErrorCode::Database,
                            "Databricks SQL statement failed",
                        ));
                    }
                }
            }
            let columns = response["manifest"]["schema"]["columns"]
                .as_array()
                .ok_or_else(protocol)?
                .iter()
                .map(|c| {
                    Ok(ColumnInfo {
                        name: name(c)?.to_owned(),
                        data_type: c["type_text"]
                            .as_str()
                            .or(c["type_name"].as_str())
                            .ok_or_else(protocol)?
                            .to_owned(),
                        nullable: None,
                    })
                })
                .collect::<DriverResult<Vec<_>>>()?;
            let column_count = columns.len();
            if sink.columns(columns) == SinkControl::Stop {
                return Ok(DriverOutcome::default());
            }
            let mut chunk = response["result"].clone();
            let mut count = 0;
            let mut seen = HashSet::new();
            loop {
                for row in chunk["data_array"].as_array().into_iter().flatten() {
                    let values = row.as_array().ok_or_else(protocol)?;
                    if values.len() != column_count {
                        return Err(protocol());
                    }
                    let cells = values
                        .iter()
                        .map(|v| {
                            if v.is_null() {
                                Cell::Null
                            } else {
                                Cell::Text(
                                    v.as_str()
                                        .map(str::to_owned)
                                        .unwrap_or_else(|| v.to_string()),
                                )
                            }
                        })
                        .collect();
                    count += 1;
                    if sink.row(cells) == SinkControl::Stop || count >= request.options.max_rows {
                        return Ok(DriverOutcome::default());
                    }
                }
                let Some(next) = chunk["next_chunk_internal_link"]
                    .as_str()
                    .filter(|v| !v.is_empty())
                else {
                    break;
                };
                if !seen.insert(next.to_owned()) {
                    return Err(protocol());
                }
                chunk = self.client.request(Method::GET, next, &[], None).await?;
            }
            Ok(DriverOutcome::default())
        };
        let outcome = match limit {
            Some(limit) => bounded(&cancel, limit, work).await,
            None => {
                tokio::select! {biased;_=cancel.cancelled()=>Err(DriverError::cancelled()),result=work=>result}
            }
        };
        if outcome.is_err() {
            let cancel_path = format!("{path}/cancel");
            let cleanup = self
                .client
                .request(Method::POST, &cancel_path, &[], Some(json!({})));
            let _ = tokio::time::timeout(Duration::from_secs(5), cleanup).await;
        }
        outcome
    }
    async fn close(self: Box<Self>) -> DriverResult<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };
    async fn fixture(
        responses: Vec<(u16, Value)>,
    ) -> (Client, tokio::task::JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let mut requests = Vec::new();
            for (status, body) in responses {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut bytes = Vec::new();
                let mut buf = [0u8; 4096];
                loop {
                    let n = stream.read(&mut buf).await.unwrap();
                    assert!(n > 0);
                    bytes.extend_from_slice(&buf[..n]);
                    if let Some(end) = bytes.windows(4).position(|v| v == b"\r\n\r\n") {
                        let headers = String::from_utf8_lossy(&bytes[..end]);
                        let length = headers
                            .lines()
                            .find_map(|l| {
                                l.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .and_then(|v| v.trim().parse::<usize>().ok())
                            })
                            .unwrap_or(0);
                        if bytes.len() >= end + 4 + length {
                            break;
                        }
                    }
                }
                requests.push(String::from_utf8(bytes).unwrap());
                let body = body.to_string();
                stream.write_all(format!("HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).as_bytes()).await.unwrap();
            }
            requests
        });
        (
            Client {
                config: DatabricksConfig::parse(
                    &json!({"profile":"fixture","warehouseId":"warehouse"}),
                )
                .unwrap(),
                origin: Url::parse(&format!("http://{address}/")).unwrap(),
                http: reqwest::Client::builder()
                    .redirect(reqwest::redirect::Policy::none())
                    .build()
                    .unwrap(),
                token: Some(SecretString::from("test-token".to_owned())),
            },
            task,
        )
    }
    #[tokio::test]
    async fn sql_timeout_requests_remote_cancellation() {
        let (client, task) = fixture(vec![
            (
                200,
                json!({"statement_id":"abc","status":{"state":"PENDING"}}),
            ),
            (200, json!({})),
        ])
        .await;
        struct NoRows;
        impl RowSink for NoRows {
            fn columns(&mut self, _: Vec<ColumnInfo>) -> SinkControl {
                panic!("pending statement has no columns")
            }
            fn row(&mut self, _: Row) -> SinkControl {
                panic!("pending statement has no rows")
            }
        }
        let mut session = DatabricksSession { client };
        let result = session
            .execute(
                &QueryRequest {
                    sql: "SELECT 1".into(),
                    options: ExecutionOptions {
                        timeout: Some(Duration::from_millis(50)),
                        ..ExecutionOptions::default()
                    },
                },
                &mut NoRows,
                CancellationToken::new(),
            )
            .await;
        assert_eq!(result.unwrap_err().code, ErrorCode::Timeout);
        let requests = task.await.unwrap();
        assert!(requests[1].starts_with("POST /api/2.0/sql/statements/abc/cancel "));
    }

    #[tokio::test]
    async fn sql_polls_streams_chunks_and_preserves_null() {
        let (client,task)=fixture(vec![(200,json!({"statement_id":"abc","status":{"state":"PENDING"}})),(200,json!({"status":{"state":"SUCCEEDED"},"manifest":{"schema":{"columns":[{"name":"amount","type_text":"DECIMAL"}]}},"result":{"data_array":[["9007199254740993.01"]],"next_chunk_internal_link":"/api/2.0/sql/statements/abc/result/chunks/1"}})),(200,json!({"data_array":[[null]]}))]).await;
        #[derive(Default)]
        struct Rows(Vec<Row>);
        impl RowSink for Rows {
            fn columns(&mut self, c: Vec<ColumnInfo>) -> SinkControl {
                assert_eq!(c.len(), 1);
                SinkControl::Continue
            }
            fn row(&mut self, r: Row) -> SinkControl {
                self.0.push(r);
                SinkControl::Continue
            }
        }
        let mut rows = Rows::default();
        let mut session = DatabricksSession { client };
        session
            .execute(
                &QueryRequest {
                    sql: "SELECT amount".into(),
                    options: ExecutionOptions::default(),
                },
                &mut rows,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(
            rows.0,
            vec![
                vec![Cell::Text("9007199254740993.01".into())],
                vec![Cell::Null]
            ]
        );
        let requests = task.await.unwrap();
        assert!(requests[0].contains("warehouse_id"));
        assert!(requests.iter().all(|v| {
            v.to_ascii_lowercase()
                .contains("authorization: bearer test-token")
        }));
    }

    #[derive(Default)]
    struct Metadata(Vec<MetadataObject>);
    #[async_trait]
    impl IntrospectionSink for Metadata {
        async fn batch(&mut self, objects: Vec<MetadataObject>) -> DriverResult<()> {
            self.0.extend(objects);
            Ok(())
        }
    }

    #[tokio::test]
    async fn table_and_column_refresh_request_only_the_selected_relation() {
        for column_only in [false, true] {
            let (client, task) = fixture(vec![
                (200, json!({"name":"cat"})),
                (200, json!({"name":"sch"})),
                (200, json!({"name":"target","table_type":"MANAGED","columns":[{"name":"id","type_text":"BIGINT","position":0},{"name":"other","type_text":"STRING","position":1}]})),
            ]).await;
            let mut scope = ObjectPath::default()
                .child(ObjectKind::Catalog, "cat")
                .child(ObjectKind::Schema, "sch")
                .child(ObjectKind::Table, "target");
            if column_only {
                scope = scope.child(ObjectKind::Column, "id");
            }
            let mut metadata = Metadata::default();
            client.introspect(&scope, &mut metadata).await.unwrap();
            assert!(
                metadata
                    .0
                    .iter()
                    .all(|object| object.path.is_within(&scope))
            );
            assert_eq!(metadata.0.len(), if column_only { 1 } else { 3 });
            let requests = task.await.unwrap();
            assert!(requests[0].starts_with("GET /api/2.1/unity-catalog/catalogs/cat "));
            assert!(requests[1].starts_with("GET /api/2.1/unity-catalog/schemas/cat.sch "));
            assert!(requests[2].starts_with("GET /api/2.1/unity-catalog/tables/cat.sch.target "));
        }
    }

    #[tokio::test]
    async fn deleted_scoped_table_returns_an_empty_successful_snapshot() {
        let (client, task) = fixture(vec![
            (200, json!({"name":"cat"})),
            (200, json!({"name":"sch"})),
            (404, json!({})),
        ])
        .await;
        let scope = ObjectPath::default()
            .child(ObjectKind::Catalog, "cat")
            .child(ObjectKind::Schema, "sch")
            .child(ObjectKind::Table, "gone");
        let mut metadata = Metadata::default();
        client.introspect(&scope, &mut metadata).await.unwrap();
        assert!(metadata.0.is_empty());
        task.await.unwrap();
    }

    #[tokio::test]
    async fn metadata_pagination_rejects_cycles_and_classifies_permission_errors() {
        let (client, task) = fixture(vec![
            (200, json!({"catalogs":[],"next_page_token":"same"})),
            (200, json!({"catalogs":[],"next_page_token":"same"})),
        ])
        .await;
        assert_eq!(
            client
                .pages("/api/2.1/unity-catalog/catalogs", "catalogs", vec![])
                .await
                .unwrap_err()
                .code,
            ErrorCode::Protocol
        );
        task.await.unwrap();
        let (client, task) = fixture(vec![(403, json!({"message":"private-server-detail"}))]).await;
        let error = client
            .request(Method::GET, "/api/2.1/unity-catalog/catalogs", &[], None)
            .await
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::PermissionDenied);
        assert!(!error.message.contains("private-server-detail"));
        task.await.unwrap();
        assert_eq!(
            client
                .request(Method::GET, "https://other.test/api/x", &[], None)
                .await
                .unwrap_err()
                .code,
            ErrorCode::Protocol
        );
    }
}
