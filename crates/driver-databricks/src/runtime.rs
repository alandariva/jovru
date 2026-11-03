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
/// A failed statement keeps the warehouse's own message and error class;
/// the SQLSTATE comes from the message when Databricks includes it.
fn statement_error(status: &Value) -> DriverError {
    let Some(message) = status["message"].as_str().filter(|m| !m.trim().is_empty()) else {
        return error(ErrorCode::Database, "Databricks SQL statement failed");
    };
    let sqlstate = message.split_once("SQLSTATE: ").and_then(|(_, rest)| {
        let state: String = rest
            .chars()
            .take_while(char::is_ascii_alphanumeric)
            .collect();
        (state.len() == 5).then_some(state)
    });
    let vendor_code = status["error_code"].as_str().map(str::to_owned);
    let code = match vendor_code.as_deref() {
        Some("PERMISSION_DENIED") => ErrorCode::PermissionDenied,
        Some("UNAUTHENTICATED") => ErrorCode::Authentication,
        _ => ErrorCode::Database,
    };
    DriverError::from_server(
        code,
        message.to_owned(),
        ServerDiagnostic {
            sqlstate,
            vendor_code,
            position: None,
        },
    )
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

/// How long a token from the CLI is reused before asking the CLI again. The
/// CLI hands out tokens valid for much longer and refreshes them itself; a
/// 401 also asks again, once.
const TOKEN_REUSE: Duration = Duration::from_secs(300);

#[derive(Debug)]
struct Client {
    config: DatabricksConfig,
    origin: Url,
    http: reqwest::Client,
    /// The last token from the CLI and when it was obtained.
    cached: std::sync::Mutex<Option<(SecretString, tokio::time::Instant)>>,
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
            cached: std::sync::Mutex::new(None),
            #[cfg(test)]
            token: None,
        })
    }
    /// The CLI's token, launching the CLI only when the cached one is older
    /// than [`TOKEN_REUSE`] or was rejected.
    async fn token(&self, fresh: bool) -> DriverResult<SecretString> {
        #[cfg(test)]
        if let Some(token) = &self.token {
            return Ok(token.clone());
        }
        if !fresh
            && let Ok(cached) = self.cached.lock()
            && let Some((token, at)) = cached.as_ref()
            && at.elapsed() < TOKEN_REUSE
        {
            return Ok(token.clone());
        }
        let token = tokio::time::timeout(Duration::from_secs(60), cli(&self.config))
            .await
            .map_err(|_| error(ErrorCode::Timeout, "CLI authentication timed out"))??;
        if let Ok(mut cached) = self.cached.lock() {
            *cached = Some((token.clone(), tokio::time::Instant::now()));
        }
        Ok(token)
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
        // A rejected token may only have expired: ask the CLI again, once.
        // A 401 means the request was not processed, so repeating it is safe.
        let mut response = self.send(&method, &url, query, &body, false).await?;
        if response.status().as_u16() == 401 {
            response = self.send(&method, &url, query, &body, true).await?;
        }
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
    async fn send(
        &self,
        method: &Method,
        url: &Url,
        query: &[(String, String)],
        body: &Option<Value>,
        fresh_token: bool,
    ) -> DriverResult<reqwest::Response> {
        let token = self.token(fresh_token).await?;
        let mut request = self
            .http
            .request(method.clone(), url.clone())
            .query(query)
            .bearer_auth(token.expose_secret());
        if let Some(body) = body {
            request = request.json(body);
        }
        request.send().await.map_err(|e| {
            error(
                if e.is_timeout() {
                    ErrorCode::Timeout
                } else {
                    ErrorCode::Connection
                },
                "Databricks request failed",
            )
        })
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
        Ok(Box::new(DatabricksSession::new(client)) as Box<dyn Session>)
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

/// Statement Execution keeps no state between requests, so the session keeps
/// the current catalog and schema itself, starting from the profile's
/// defaults, and follows the `USE` statements that succeed.
#[derive(Debug)]
struct DatabricksSession {
    client: Client,
    catalog: Option<String>,
    schema: Option<String>,
    /// The rows of the last result not delivered yet, while it has more.
    pending: Option<Pending>,
}

/// A result being read page by page. Its chunks stay on the server; only the
/// rows of the current chunk are held here.
#[derive(Debug)]
struct Pending {
    column_count: usize,
    rows: std::collections::VecDeque<Value>,
    next: Option<String>,
    seen: HashSet<String>,
    /// The server stopped at the requested row limit.
    truncated: bool,
}

/// Results are read inline, which the API caps at 25 MiB, so a statement
/// asks the server for this many pages at most.
const SERVER_PAGES: u32 = 5;
/// How long the first request waits for the statement before polling.
const FIRST_WAIT: &str = "5s";
/// Status polls start fast for short statements and slow down to this.
const POLL_MAX: Duration = Duration::from_secs(2);

impl DatabricksSession {
    fn new(client: Client) -> Self {
        Self {
            catalog: client.config.default_catalog.clone(),
            schema: client.config.default_schema.clone(),
            client,
            pending: None,
        }
    }

    /// Delivers up to `max_rows` rows of `pending`, reading further chunks
    /// as needed, and keeps what is left for the next page.
    async fn page(
        &self,
        pending: &mut Pending,
        max_rows: u32,
        sink: &mut dyn RowSink,
    ) -> DriverResult<(DriverOutcome, bool)> {
        let mut count = 0;
        loop {
            while let Some(row) = pending.rows.pop_front() {
                if count >= max_rows {
                    pending.rows.push_front(row);
                    let outcome = DriverOutcome {
                        has_more: true,
                        ..DriverOutcome::default()
                    };
                    return Ok((outcome, true));
                }
                let values = row.as_array().ok_or_else(protocol)?;
                if values.len() != pending.column_count {
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
                if sink.row(cells) == SinkControl::Stop {
                    return Ok((DriverOutcome::default(), false));
                }
            }
            let Some(next) = pending.next.take() else {
                let outcome = DriverOutcome {
                    truncated: pending.truncated,
                    ..DriverOutcome::default()
                };
                return Ok((outcome, false));
            };
            if !pending.seen.insert(next.clone()) {
                return Err(protocol());
            }
            let chunk = self.client.request(Method::GET, &next, &[], None).await?;
            pending.absorb(chunk)?;
        }
    }
}

impl Pending {
    fn absorb(&mut self, mut chunk: Value) -> DriverResult<()> {
        if let Some(rows) = chunk["data_array"].as_array_mut() {
            self.rows.extend(rows.drain(..));
        }
        self.next = chunk["next_chunk_internal_link"]
            .as_str()
            .filter(|v| !v.is_empty())
            .map(str::to_owned);
        Ok(())
    }
}

impl DatabricksSession {
    fn apply(&mut self, change: UseStatement) {
        match change {
            UseStatement::Catalog(catalog) => {
                self.catalog = Some(catalog);
                // Setting the catalog also resets the current schema.
                self.schema = Some("default".to_owned());
            }
            UseStatement::Schema { catalog, schema } => {
                if catalog.is_some() {
                    self.catalog = catalog;
                }
                self.schema = Some(schema);
            }
        }
    }
}

/// A statement that changes the current catalog or schema.
#[derive(Debug, PartialEq, Eq)]
enum UseStatement {
    Catalog(String),
    Schema {
        catalog: Option<String>,
        schema: String,
    },
}

/// Recognises `USE CATALOG c`, `USE [SCHEMA | DATABASE] [c.]s` and the
/// `SET CATALOG` alias. Names may be quoted with backticks; anything else
/// is not a context change.
fn use_statement(sql: &str) -> Option<UseStatement> {
    let sql = sql.trim().trim_end_matches(';').trim_end();
    let (keyword, rest) = split_word(sql)?;
    let catalog_alias = keyword.eq_ignore_ascii_case("set");
    if !keyword.eq_ignore_ascii_case("use") && !catalog_alias {
        return None;
    }
    let (kind, after_kind) = split_word(rest).unwrap_or(("", rest));
    let (catalog, names) = if kind.eq_ignore_ascii_case("catalog") {
        (true, after_kind)
    } else if catalog_alias {
        return None;
    } else if kind.eq_ignore_ascii_case("schema") || kind.eq_ignore_ascii_case("database") {
        (false, after_kind)
    } else {
        (false, rest)
    };
    let mut parts = qualified_name(names)?;
    if catalog {
        return (parts.len() == 1).then(|| UseStatement::Catalog(parts.remove(0)));
    }
    match parts.len() {
        1 => Some(UseStatement::Schema {
            catalog: None,
            schema: parts.remove(0),
        }),
        2 => {
            let schema = parts.remove(1);
            Some(UseStatement::Schema {
                catalog: Some(parts.remove(0)),
                schema,
            })
        }
        _ => None,
    }
}

/// The first bare word of `text` and what follows it.
fn split_word(text: &str) -> Option<(&str, &str)> {
    let text = text.trim_start();
    let end = text
        .find(|c: char| !c.is_ascii_alphabetic())
        .unwrap_or(text.len());
    (end > 0).then(|| (&text[..end], &text[end..]))
}

/// `a`, `` `a.b` `` or `a.b`, with nothing after it.
fn qualified_name(text: &str) -> Option<Vec<String>> {
    let mut parts = Vec::new();
    let mut rest = text.trim();
    loop {
        if let Some(quoted) = rest.strip_prefix('`') {
            let mut name = String::new();
            let mut chars = quoted.char_indices();
            let mut end = None;
            while let Some((index, c)) = chars.next() {
                if c == '`' {
                    if quoted[index + 1..].starts_with('`') {
                        name.push('`');
                        chars.next();
                    } else {
                        end = Some(index + 1);
                        break;
                    }
                } else {
                    name.push(c);
                }
            }
            parts.push(name);
            rest = &quoted[end?..];
        } else {
            let end = rest
                .find(|c: char| !(c.is_alphanumeric() || c == '_'))
                .unwrap_or(rest.len());
            if end == 0 {
                return None;
            }
            parts.push(rest[..end].to_owned());
            rest = &rest[end..];
        }
        match rest.strip_prefix('.') {
            Some(next) => rest = next,
            None => break,
        }
    }
    rest.trim().is_empty().then_some(parts)
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
        // A new statement ends the previous result; its chunks expire on the
        // server.
        self.pending = None;
        if cancel.is_cancelled() {
            return Err(DriverError::cancelled());
        }
        let row_limit = request
            .options
            .max_rows
            .saturating_mul(SERVER_PAGES)
            .min(100_000);
        let mut body = json!({"warehouse_id":self.client.config.warehouse_id,"statement":request.sql,"wait_timeout":FIRST_WAIT,"on_wait_timeout":"CONTINUE","format":"JSON_ARRAY","disposition":"INLINE","row_limit":row_limit});
        if let Some(catalog) = request
            .options
            .context
            .catalog
            .as_ref()
            .or(self.catalog.as_ref())
        {
            body["catalog"] = json!(catalog);
        }
        if let Some(schema) = request
            .options
            .context
            .schema
            .as_ref()
            .or(self.schema.as_ref())
        {
            body["schema"] = json!(schema);
        }
        let started = tokio::time::Instant::now();
        // The first request waits up to FIRST_WAIT for the statement, so
        // short statements need no polling. A cancellation meanwhile still
        // waits for the statement id, so the statement can be cancelled.
        let (mut response, cancelled) = {
            let submit =
                self.client
                    .request(Method::POST, "/api/2.0/sql/statements", &[], Some(body));
            tokio::pin!(submit);
            tokio::select! {
                biased;
                _ = cancel.cancelled() => (submit.await?, true),
                response = &mut submit => (response?, false),
            }
        };
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
            if cancelled {
                return Err(DriverError::cancelled());
            }
            let mut wait = Duration::from_millis(200);
            loop {
                match response["status"]["state"].as_str() {
                    Some("SUCCEEDED") => break,
                    Some("PENDING" | "RUNNING") => {
                        tokio::time::sleep(wait).await;
                        wait = (wait * 2).min(POLL_MAX);
                        response = self.client.request(Method::GET, &path, &[], None).await?;
                    }
                    Some("CANCELED") => return Err(DriverError::cancelled()),
                    _ => return Err(statement_error(&response["status"]["error"])),
                }
            }
            // Statements without a result (USE, DDL) may leave the column
            // list out of the manifest.
            let manifest = &response["manifest"]["schema"];
            if !manifest.is_object() {
                return Err(protocol());
            }
            let columns = manifest["columns"]
                .as_array()
                .map(Vec::as_slice)
                .unwrap_or_default()
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
            let mut pending = Pending {
                column_count: columns.len(),
                rows: Default::default(),
                next: None,
                seen: HashSet::new(),
                truncated: response["manifest"]["truncated"].as_bool().unwrap_or(false),
            };
            if sink.columns(columns) == SinkControl::Stop {
                return Ok((DriverOutcome::default(), None));
            }
            pending.absorb(response["result"].take())?;
            let (outcome, more) = self
                .page(&mut pending, request.options.max_rows, sink)
                .await?;
            Ok((outcome, more.then_some(pending)))
        };
        let outcome = match limit {
            Some(limit) => bounded(&cancel, limit, work).await,
            None => {
                tokio::select! {biased;_=cancel.cancelled()=>Err(DriverError::cancelled()),result=work=>result}
            }
        };
        if outcome.is_ok()
            && let Some(change) = use_statement(&request.sql)
        {
            self.apply(change);
        }
        match outcome {
            Ok((outcome, pending)) => {
                self.pending = pending;
                Ok(outcome)
            }
            Err(failure) => {
                let cancel_path = format!("{path}/cancel");
                let cleanup = self
                    .client
                    .request(Method::POST, &cancel_path, &[], Some(json!({})));
                let _ = tokio::time::timeout(Duration::from_secs(5), cleanup).await;
                Err(failure)
            }
        }
    }
    async fn fetch(
        &mut self,
        options: &ExecutionOptions,
        sink: &mut dyn RowSink,
        cancel: CancellationToken,
    ) -> DriverResult<DriverOutcome> {
        let Some(mut pending) = self.pending.take() else {
            return Err(DriverError::validation("No result has more rows to read"));
        };
        if cancel.is_cancelled() {
            return Err(DriverError::cancelled());
        }
        let page = self.page(&mut pending, options.max_rows, sink);
        let result = match options.timeout {
            Some(limit) => bounded(&cancel, limit, page).await,
            None => {
                tokio::select! {biased;_=cancel.cancelled()=>Err(DriverError::cancelled()),result=page=>result}
            }
        };
        let (outcome, more) = result?;
        if more {
            self.pending = Some(pending);
        }
        Ok(outcome)
    }
    async fn ping(&mut self) -> DriverResult<()> {
        Ok(())
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
                cached: std::sync::Mutex::new(None),
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
                unreachable!("pending statement has no columns")
            }
            fn row(&mut self, _: Row) -> SinkControl {
                unreachable!("pending statement has no rows")
            }
        }
        let mut session = DatabricksSession::new(client);
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
        let mut session = DatabricksSession::new(client);
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

    #[tokio::test]
    async fn results_are_read_page_by_page_across_chunks() {
        let first = json!({"statement_id":"abc","status":{"state":"SUCCEEDED"},"manifest":{"truncated":true,"schema":{"columns":[{"name":"n","type_text":"INT"}]}},"result":{"data_array":[["1"],["2"],["3"]],"next_chunk_internal_link":"/api/2.0/sql/statements/abc/result/chunks/1"}});
        let (client, task) = fixture(vec![
            (200, first),
            (401, json!({})),
            (200, json!({"data_array":[["4"],["5"]]})),
        ])
        .await;
        #[derive(Default)]
        struct Rows(Vec<Row>);
        impl RowSink for Rows {
            fn columns(&mut self, _: Vec<ColumnInfo>) -> SinkControl {
                SinkControl::Continue
            }
            fn row(&mut self, r: Row) -> SinkControl {
                self.0.push(r);
                SinkControl::Continue
            }
        }
        let options = ExecutionOptions {
            max_rows: 2,
            ..ExecutionOptions::default()
        };
        let mut session = DatabricksSession::new(client);
        let mut rows = Rows::default();
        let request = QueryRequest {
            sql: "SELECT n".into(),
            options: options.clone(),
        };
        let first = session
            .execute(&request, &mut rows, CancellationToken::new())
            .await
            .unwrap();
        assert!(first.has_more && rows.0.len() == 2);
        // The rest of the first chunk, then the next chunk after a 401 that
        // asks for a fresh token once.
        let second = session
            .fetch(&options, &mut rows, CancellationToken::new())
            .await
            .unwrap();
        assert!(second.has_more && rows.0.len() == 4);
        let third = session
            .fetch(&options, &mut rows, CancellationToken::new())
            .await
            .unwrap();
        assert!(!third.has_more && third.truncated);
        let values: Vec<_> = rows.0.iter().map(|row| row[0].clone()).collect();
        let expected: Vec<_> = (1..=5).map(|n| Cell::Text(n.to_string())).collect();
        assert_eq!(values, expected);
        assert!(
            session
                .fetch(&options, &mut rows, CancellationToken::new())
                .await
                .is_err()
        );
        let requests = task.await.unwrap();
        let body: Value =
            serde_json::from_str(&requests[0][requests[0].find("\r\n\r\n").unwrap() + 4..])
                .unwrap();
        assert_eq!(body["row_limit"], 2 * SERVER_PAGES);
        assert_eq!(body["wait_timeout"], FIRST_WAIT);
        assert!(requests[1].starts_with("GET /api/2.0/sql/statements/abc/result/chunks/1 "));
        assert!(requests[2].starts_with("GET /api/2.0/sql/statements/abc/result/chunks/1 "));
    }

    #[test]
    fn use_statements_change_catalog_or_schema() {
        let schema = |catalog: Option<&str>, schema: &str| UseStatement::Schema {
            catalog: catalog.map(str::to_owned),
            schema: schema.to_owned(),
        };
        assert_eq!(
            use_statement("USE CATALOG main;"),
            Some(UseStatement::Catalog("main".into()))
        );
        assert_eq!(
            use_statement("set catalog `my cat`"),
            Some(UseStatement::Catalog("my cat".into()))
        );
        assert_eq!(use_statement("use sales"), Some(schema(None, "sales")));
        assert_eq!(
            use_statement("USE SCHEMA main.`a``b` ;"),
            Some(schema(Some("main"), "a`b"))
        );
        assert_eq!(
            use_statement("Use Database finance"),
            Some(schema(None, "finance"))
        );
        for other in [
            "SELECT 1",
            "USE",
            "USE CATALOG a.b",
            "USE a.b.c",
            "USE sales; SELECT 1",
            "SET spark.sql.ansi.enabled = true",
            "USE `unterminated",
            "users",
        ] {
            assert_eq!(use_statement(other), None, "{other}");
        }
    }

    #[test]
    fn a_failed_statement_keeps_the_warehouse_message_and_codes() {
        let error = statement_error(&json!({
            "error_code": "BAD_REQUEST",
            "message": "[TABLE_OR_VIEW_NOT_FOUND] The table or view `missing` cannot be found. SQLSTATE: 42P01; line 1 pos 14"
        }));
        assert_eq!(error.code, ErrorCode::Database);
        assert!(error.message.contains("`missing` cannot be found"));
        let server = error.server.unwrap();
        assert_eq!(server.sqlstate.as_deref(), Some("42P01"));
        assert_eq!(server.vendor_code.as_deref(), Some("BAD_REQUEST"));
        let denied = statement_error(&json!({"error_code":"PERMISSION_DENIED","message":"no"}));
        assert_eq!(denied.code, ErrorCode::PermissionDenied);
        // Without a message the error stays generic.
        let bare = statement_error(&json!(null));
        assert_eq!(bare.message, "Databricks SQL statement failed");
        assert_eq!(bare.server, None);
    }

    #[tokio::test]
    async fn a_successful_use_carries_its_context_to_later_statements() {
        let done = json!({"statement_id":"abc","status":{"state":"SUCCEEDED"},"manifest":{"schema":{"column_count":0}}});
        let failed = json!({"statement_id":"abd","status":{"state":"FAILED"}});
        let (client, task) = fixture(vec![
            (200, done.clone()),
            (200, done.clone()),
            (200, failed),
            (200, json!({})),
            (200, done.clone()),
            (200, done),
        ])
        .await;
        struct Ignore;
        impl RowSink for Ignore {
            fn columns(&mut self, _: Vec<ColumnInfo>) -> SinkControl {
                SinkControl::Continue
            }
            fn row(&mut self, _: Row) -> SinkControl {
                SinkControl::Continue
            }
        }
        let mut session = DatabricksSession::new(client);
        for sql in [
            "USE CATALOG main",
            "USE SCHEMA sales",
            "USE SCHEMA missing",
            "SELECT 1",
            "USE other.finance",
        ] {
            let request = QueryRequest {
                sql: sql.into(),
                options: ExecutionOptions::default(),
            };
            let result = session
                .execute(&request, &mut Ignore, CancellationToken::new())
                .await;
            assert_eq!(result.is_err(), sql.contains("missing"), "{sql}");
        }
        assert_eq!(session.catalog.as_deref(), Some("other"));
        assert_eq!(session.schema.as_deref(), Some("finance"));
        let requests = task.await.unwrap();
        let body = |index: usize| -> Value {
            let request: &String = &requests[index];
            serde_json::from_str(&request[request.find("\r\n\r\n").unwrap() + 4..]).unwrap()
        };
        // USE SCHEMA runs with the catalog chosen before it; the failed USE
        // and its cancel request leave the context as it was.
        assert_eq!(body(1)["catalog"], "main");
        assert_eq!(body(1)["schema"], "default");
        assert_eq!(body(4)["catalog"], "main");
        assert_eq!(body(4)["schema"], "sales");
        assert!(requests[3].starts_with("POST /api/2.0/sql/statements/abd/cancel "));
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
