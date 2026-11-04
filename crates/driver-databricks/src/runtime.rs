use crate::{ComputeKind, DatabricksConfig, auth::Auth};
use jykre_core::*;
use reqwest::{Method, Url};
use secrecy::ExposeSecret;
use serde_json::{Value, json};
use std::{collections::HashSet, sync::Arc, time::Duration};

mod cluster;

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
pub(super) fn protocol() -> DriverError {
    error(ErrorCode::Protocol, "Invalid Databricks API response")
}
async fn bounded<T>(
    cancel: &CancellationToken,
    limit: Duration,
    work: impl std::future::Future<Output = DriverResult<T>>,
) -> DriverResult<T> {
    tokio::select! { biased; _=cancel.cancelled()=>Err(DriverError::cancelled()), result=tokio::time::timeout(limit,work)=>result.map_err(|_|error(ErrorCode::Timeout,"Databricks operation timed out"))? }
}

#[derive(Debug)]
struct Client {
    config: DatabricksConfig,
    origin: Url,
    http: reqwest::Client,
    auth: Arc<Auth>,
    #[cfg(test)]
    token: Option<SecretString>,
}
impl Client {
    async fn new(input: &ResolvedConnection) -> DriverResult<Self> {
        Self::in_config_file(input, crate::auth::config_path()?).await
    }

    async fn in_config_file(
        input: &ResolvedConnection,
        path: std::path::PathBuf,
    ) -> DriverResult<Self> {
        let config = DatabricksConfig::parse(&input.config.options)?;
        let path = tokio::fs::canonicalize(path).await.map_err(|_| {
            error(
                ErrorCode::Authentication,
                "Cannot read Databricks CLI profiles",
            )
        })?;
        let origin = crate::auth::workspace_origin(&config, &path).await?;
        let auth = Auth::shared(&config, path, &origin)?;
        let http = auth.http.clone();
        Ok(Self {
            config,
            origin,
            http,
            auth,
            #[cfg(test)]
            token: None,
        })
    }
    async fn token(&self, rejected: Option<u64>) -> DriverResult<(SecretString, u64)> {
        #[cfg(test)]
        if let Some(token) = &self.token {
            return Ok((token.clone(), 0));
        }
        self.auth.token(rejected).await
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
        let (mut response, mut generation) = self.send(&method, &url, query, &body, None).await?;
        if response.status().as_u16() == 401 {
            (response, generation) = self
                .send(&method, &url, query, &body, Some(generation))
                .await?;
            if response.status().as_u16() == 401 {
                self.auth.invalidate(generation).await;
            }
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
        // Bound the wire payload too: a single large cell must not bypass the
        // result builder's memory ceiling. Inline SQL responses cap at 25 MiB.
        const MAX_RESPONSE: usize = 26 * 1024 * 1024;
        if response
            .content_length()
            .is_some_and(|size| size > MAX_RESPONSE as u64)
        {
            return Err(error(
                ErrorCode::Protocol,
                "Databricks response exceeds the inline result limit",
            ));
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| protocol())? {
            if bytes.len().saturating_add(chunk.len()) > MAX_RESPONSE {
                return Err(error(
                    ErrorCode::Protocol,
                    "Databricks response exceeds the inline result limit",
                ));
            }
            bytes.extend_from_slice(&chunk);
        }
        if path == "/api/1.2/commands/status" {
            cluster::decode_response(&bytes)
        } else {
            serde_json::from_slice(&bytes).map_err(|_| protocol())
        }
    }
    async fn send(
        &self,
        method: &Method,
        url: &Url,
        query: &[(String, String)],
        body: &Option<Value>,
        rejected: Option<u64>,
    ) -> DriverResult<(reqwest::Response, u64)> {
        let (token, generation) = self.token(rejected).await?;
        let mut request = self
            .http
            .request(method.clone(), url.clone())
            .query(query)
            .bearer_auth(token.expose_secret());
        if let Some(body) = body {
            request = request.json(body);
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
        Ok((response, generation))
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

/// A resource visible to the CLI profile. Its ID, rather than its name, is
/// used for execution; a name is only a label and can change on the server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComputeResource {
    pub id: String,
    pub name: String,
    pub state: String,
}

/// Lists warehouses or classic all-purpose clusters without starting compute.
/// The operation is cancellable and limited to 90 seconds.
pub async fn list_compute(
    options: &Value,
    cancel: CancellationToken,
) -> DriverResult<Vec<ComputeResource>> {
    let config = DatabricksConfig::parse(options)?;
    bounded(&cancel, Duration::from_secs(90), async {
        let client = Client::new(&ResolvedConnection {
            config: ConnectionConfig {
                driver: DriverKind::Databricks,
                options: options.clone(),
            },
            credentials: Credentials::default(),
        })
        .await?;
        compute_resources(&client, config.compute).await
    })
    .await
}

async fn compute_resources(
    client: &Client,
    kind: ComputeKind,
) -> DriverResult<Vec<ComputeResource>> {
    let (path, key, id_key, name_key) = match kind {
        ComputeKind::Warehouse => ("/api/2.0/sql/warehouses", "warehouses", "id", "name"),
        ComputeKind::Cluster => (
            "/api/2.1/clusters/list",
            "clusters",
            "cluster_id",
            "cluster_name",
        ),
    };
    let mut query = if kind == ComputeKind::Cluster {
        vec![("page_size".into(), "100".into())]
    } else {
        Vec::new()
    };
    let mut seen_pages = HashSet::new();
    let mut seen_ids = HashSet::new();
    let mut resources = Vec::new();
    loop {
        let response = client.request(Method::GET, path, &query, None).await?;
        if let Some(items) = response.get(key) {
            for item in items.as_array().ok_or_else(protocol)? {
                if kind == ComputeKind::Cluster && !classic_cluster(item) {
                    continue;
                }
                let id = item[id_key]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .ok_or_else(protocol)?;
                if seen_ids.insert(id.to_owned()) {
                    resources.push(ComputeResource {
                        id: id.to_owned(),
                        name: item[name_key]
                            .as_str()
                            .filter(|s| !s.is_empty())
                            .unwrap_or(id)
                            .to_owned(),
                        state: item["state"].as_str().unwrap_or("UNKNOWN").to_owned(),
                    });
                }
            }
        }
        let Some(next) = response["next_page_token"]
            .as_str()
            .filter(|s| !s.is_empty())
        else {
            break;
        };
        if !seen_pages.insert(next.to_owned()) {
            return Err(protocol());
        }
        query.retain(|(key, _)| key != "page_token");
        query.push(("page_token".into(), next.to_owned()));
    }
    resources.sort_by(|a, b| a.name.cmp(&b.name).then(a.id.cmp(&b.id)));
    Ok(resources)
}

fn classic_cluster(item: &Value) -> bool {
    matches!(item["cluster_source"].as_str(), None | Some("UI" | "API"))
        && item["kind"].as_str() != Some("SERVERLESS")
}

async fn check_compute(client: &Client) -> DriverResult<()> {
    let Some(id) = client.config.compute_id() else {
        return Ok(());
    };
    match client.config.compute {
        ComputeKind::Warehouse => {
            client
                .request(
                    Method::GET,
                    &format!("/api/2.0/sql/warehouses/{id}"),
                    &[],
                    None,
                )
                .await?;
        }
        ComputeKind::Cluster => {
            let info = client
                .request(
                    Method::GET,
                    "/api/2.1/clusters/get",
                    &[("cluster_id".into(), id.into())],
                    None,
                )
                .await?;
            if !classic_cluster(&info) {
                return Err(DriverError::validation(
                    "Command Execution requires a classic all-purpose cluster",
                ));
            }
            if !matches!(info["state"].as_str(), Some("RUNNING" | "RESIZING")) {
                return Err(error(
                    ErrorCode::Connection,
                    "Start the selected classic cluster in Databricks before running queries",
                ));
            }
        }
    }
    Ok(())
}
pub async fn test(
    input: &ResolvedConnection,
    cancel: CancellationToken,
) -> DriverResult<ServerInfo> {
    bounded(&cancel, Duration::from_secs(90), async {
        let client = Client::new(input).await?;
        check_compute(&client).await?;
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
    let client = bounded(&cancel, Duration::from_secs(90), async {
        let client = Client::new(input).await?;
        client.token(None).await?;
        if client.config.compute == ComputeKind::Cluster {
            check_compute(&client).await?;
        }
        Ok(client)
    })
    .await?;
    match client.config.compute {
        ComputeKind::Warehouse => Ok(Box::new(DatabricksSession::new(client)) as Box<dyn Session>),
        ComputeKind::Cluster => cluster::open(client, &cancel).await,
    }
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
    level: IntrospectionLevel,
    sink: &mut dyn IntrospectionSink,
    cancel: CancellationToken,
) -> DriverResult<IntrospectionReport> {
    bounded(&cancel, Duration::from_secs(300), async {
        let client = Client::new(input).await?;
        client.introspect(scope, level, sink).await
    })
    .await
}

/// The root holds the Unity Catalog catalogs (`hive_metastore` is not
/// supported) and a catalog holds its schemas. Only the listing endpoints
/// are called: no table is read and no compute starts.
pub async fn discover(
    input: &ResolvedConnection,
    parent: &ObjectPath,
    cancel: CancellationToken,
) -> DriverResult<Vec<MetadataObject>> {
    bounded(&cancel, Duration::from_secs(60), async {
        Client::new(input).await?.discover(parent).await
    })
    .await
}

impl Client {
    async fn discover(&self, parent: &ObjectPath) -> DriverResult<Vec<MetadataObject>> {
        let (items, kind) = match parent.0.as_slice() {
            [] => (
                self.pages("/api/2.1/unity-catalog/catalogs", "catalogs", vec![])
                    .await?,
                ObjectKind::Catalog,
            ),
            [catalog] => (
                self.pages(
                    "/api/2.1/unity-catalog/schemas",
                    "schemas",
                    vec![("catalog_name".into(), catalog.name.clone())],
                )
                .await?,
                ObjectKind::Schema,
            ),
            _ => return Ok(Vec::new()),
        };
        let mut found = Vec::new();
        for item in &items {
            let name = name(item)?;
            if kind == ObjectKind::Catalog && name == "hive_metastore" {
                continue;
            }
            found.push(MetadataObject::new(kind, parent, name));
        }
        found.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(found)
    }

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
        level: IntrospectionLevel,
        sink: &mut dyn IntrospectionSink,
    ) -> DriverResult<IntrospectionReport> {
        let client = self;
        let names_only = level == IntrospectionLevel::Names;
        let mut report = IntrospectionReport::default();
        // Reading one schema needs no GET of its catalog and schema first:
        // listing its tables says whether it exists.
        let namespace = match (
            scope.name_of(ObjectKind::Catalog),
            scope.name_of(ObjectKind::Schema),
        ) {
            (Some(catalog), Some(schema)) => Some((catalog.to_owned(), schema.to_owned())),
            _ => None,
        };
        let catalogs = match &namespace {
            Some((catalog, _)) => vec![json!({ "name": catalog })],
            None => {
                client
                    .metadata_items(
                        "/api/2.1/unity-catalog/catalogs",
                        "catalogs",
                        vec![],
                        scope.name_of(ObjectKind::Catalog).map(str::to_owned),
                    )
                    .await?
            }
        };
        for catalog in catalogs {
            if name(&catalog)? == "hive_metastore"
                || scope
                    .name_of(ObjectKind::Catalog)
                    .is_some_and(|value| catalog["name"].as_str() != Some(value))
            {
                continue;
            }
            let cat =
                MetadataObject::new(ObjectKind::Catalog, &ObjectPath::default(), name(&catalog)?);
            if namespace.is_none() && cat.path.is_within(scope) {
                sink.batch(vec![cat.clone()]).await?;
            }
            let schemas = match &namespace {
                Some((_, schema)) => vec![json!({ "name": schema })],
                None => match client
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
                },
            };
            for schema in schemas {
                if scope
                    .name_of(ObjectKind::Schema)
                    .is_some_and(|value| schema["name"].as_str() != Some(value))
                {
                    continue;
                }
                let sch = MetadataObject::new(ObjectKind::Schema, &cat.path, name(&schema)?);
                if namespace.is_none() && sch.path.is_within(scope) {
                    sink.batch(vec![sch.clone()]).await?;
                }
                let mut query = vec![
                    ("catalog_name".into(), cat.name.clone()),
                    ("schema_name".into(), sch.name.clone()),
                ];
                if names_only {
                    // Names first: the columns come with the full read.
                    query.push(("omit_columns".into(), "true".into()));
                }
                let tables = match client
                    .metadata_items(
                        "/api/2.1/unity-catalog/tables",
                        "tables",
                        query,
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
                    // The schema (or its catalog) no longer exists.
                    Err(e) if e.code == ErrorCode::NotFound && namespace.is_some() => continue,
                    Err(e) => return Err(e),
                };
                if namespace.is_some() {
                    // The namespace exists: its nodes come with what it holds.
                    let nodes: Vec<_> = [cat.clone(), sch.clone()]
                        .into_iter()
                        .filter(|object| object.path.is_within(scope))
                        .collect();
                    if !nodes.is_empty() {
                        sink.batch(nodes).await?;
                    }
                }
                for table in tables {
                    if scope
                        .name_of(ObjectKind::Table)
                        .or(scope.name_of(ObjectKind::View))
                        .is_some_and(|value| table["name"].as_str() != Some(value))
                    {
                        continue;
                    }
                    let kind = match table["table_type"].as_str() {
                        Some("MATERIALIZED_VIEW") => ObjectKind::MaterializedView,
                        Some(kind) if kind.contains("VIEW") => ObjectKind::View,
                        _ => ObjectKind::Table,
                    };
                    let mut rel = MetadataObject::new(kind, &sch.path, name(&table)?);
                    rel.comment = table["comment"].as_str().map(str::to_owned);
                    let mut batch = vec![rel.clone()];
                    if !names_only {
                        let constraints = Constraints::read(&table["table_constraints"]);
                        for column in table["columns"].as_array().into_iter().flatten() {
                            let mut object =
                                MetadataObject::new(ObjectKind::Column, &rel.path, name(column)?);
                            object.data_type = column["type_text"].as_str().map(str::to_owned);
                            object.nullable = column["nullable"].as_bool();
                            object.ordinal = column["position"]
                                .as_u64()
                                .and_then(|v| u32::try_from(v + 1).ok());
                            object.comment = column["comment"].as_str().map(str::to_owned);
                            object.default_value = column_default(column);
                            object.primary_key = constraints
                                .primary_key
                                .iter()
                                .position(|key| *key == object.name)
                                .and_then(|at| u32::try_from(at + 1).ok());
                            batch.push(object);
                        }
                        for (key, target) in constraints.foreign_keys {
                            let mut object =
                                MetadataObject::new(ObjectKind::ForeignKey, &rel.path, key);
                            object.references = Some(target);
                            batch.push(object);
                        }
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

    /// The table's information, read on its own.
    async fn table(&self, catalog: &str, schema: &str, table: &str) -> DriverResult<Value> {
        let full = format!("{catalog}.{schema}.{table}");
        self.metadata_items(
            "/api/2.1/unity-catalog/tables",
            "tables",
            vec![],
            Some(full),
        )
        .await?
        .into_iter()
        .next()
        .ok_or_else(|| DriverError::new(ErrorCode::NotFound, "The object no longer exists"))
    }
}

/// A column's default from its Delta metadata (`CURRENT_DEFAULT` in
/// `type_json`), which Unity Catalog reports with the column.
fn column_default(column: &Value) -> Option<String> {
    let metadata = match &column["type_json"] {
        Value::String(text) => serde_json::from_str::<Value>(text).ok()?,
        other => other.clone(),
    };
    metadata["metadata"]["CURRENT_DEFAULT"]
        .as_str()
        .map(str::to_owned)
}

/// A table's keys and column defaults, or a schema's functions with their
/// parameters. Unity Catalog has no indexes.
pub async fn inspect(
    input: &ResolvedConnection,
    path: &ObjectPath,
    cancel: CancellationToken,
) -> DriverResult<Inspection> {
    let (Some(catalog), Some(schema)) = (
        path.name_of(ObjectKind::Catalog).map(str::to_owned),
        path.name_of(ObjectKind::Schema).map(str::to_owned),
    ) else {
        return Err(DriverError::validation(
            "Choose a schema, a table or a view",
        ));
    };
    let relation = path.relation().map(str::to_owned);
    bounded(&cancel, Duration::from_secs(60), async {
        let client = Client::new(input).await?;
        let mut items = Vec::new();
        if let Some(relation) = relation {
            let table = client.table(&catalog, &schema, &relation).await?;
            for constraint in table["table_constraints"].as_array().into_iter().flatten() {
                let columns = |key: &Value| {
                    key["child_columns"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join(", ")
                };
                if let Some(key) = constraint.get("primary_key_constraint") {
                    items.push(InspectionItem {
                        kind: InspectedKind::PrimaryKey,
                        name: key["name"].as_str().unwrap_or_default().to_owned(),
                        detail: format!("({})", columns(key)),
                    });
                }
                if let Some(key) = constraint.get("foreign_key_constraint") {
                    items.push(InspectionItem {
                        kind: InspectedKind::ForeignKey,
                        name: key["name"].as_str().unwrap_or_default().to_owned(),
                        detail: format!(
                            "({}) → {}",
                            columns(key),
                            key["parent_table"].as_str().unwrap_or_default()
                        ),
                    });
                }
            }
            for column in table["columns"].as_array().into_iter().flatten() {
                if let Some(default) = column_default(column) {
                    items.push(InspectionItem {
                        kind: InspectedKind::Default,
                        name: name(column)?.to_owned(),
                        detail: default,
                    });
                }
            }
            return Ok(Inspection { items });
        }
        let functions = client
            .pages(
                "/api/2.1/unity-catalog/functions",
                "functions",
                vec![
                    ("catalog_name".into(), catalog),
                    ("schema_name".into(), schema),
                ],
            )
            .await?;
        for function in functions {
            let parameters: Vec<String> = function["input_params"]["parameters"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|parameter| {
                    format!(
                        "{} {}",
                        parameter["name"].as_str().unwrap_or_default(),
                        parameter["type_text"].as_str().unwrap_or_default()
                    )
                })
                .collect();
            let mut detail = format!("({})", parameters.join(", "));
            if let Some(returns) = function["full_data_type"].as_str() {
                detail.push_str(&format!(" → {returns}"));
            }
            items.push(InspectionItem {
                kind: InspectedKind::Function,
                name: name(&function)?.to_owned(),
                detail,
            });
        }
        Ok(Inspection { items })
    })
    .await
}

/// A view's definition from Unity Catalog. Table DDL needs SQL compute
/// (`SHOW CREATE TABLE`), which reading metadata never starts.
pub async fn definition(
    input: &ResolvedConnection,
    path: &ObjectPath,
    cancel: CancellationToken,
) -> DriverResult<String> {
    let (Some(catalog), Some(schema), Some(relation)) = (
        path.name_of(ObjectKind::Catalog).map(str::to_owned),
        path.name_of(ObjectKind::Schema).map(str::to_owned),
        path.relation().map(str::to_owned),
    ) else {
        return Err(DriverError::validation("Choose a table or a view"));
    };
    bounded(&cancel, Duration::from_secs(60), async {
        let client = Client::new(input).await?;
        let table = client.table(&catalog, &schema, &relation).await?;
        match table["view_definition"].as_str() {
            Some(definition) => Ok(format!(
                "CREATE VIEW `{catalog}`.`{schema}`.`{relation}` AS\n{definition}"
            )),
            None => Err(DriverError::validation(
                "Databricks gives a table's DDL only through SQL compute; run SHOW CREATE TABLE in a query tab",
            )),
        }
    })
    .await
}

/// The keys Unity Catalog reports for a table (`table_constraints`).
#[derive(Default)]
struct Constraints {
    primary_key: Vec<String>,
    foreign_keys: Vec<(String, ForeignKeyTarget)>,
}

impl Constraints {
    fn read(constraints: &Value) -> Self {
        let names = |value: &Value| -> Vec<String> {
            value
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|name| name.as_str().map(str::to_owned))
                .collect()
        };
        let mut read = Self::default();
        for constraint in constraints.as_array().into_iter().flatten() {
            if let Some(key) = constraint.get("primary_key_constraint") {
                read.primary_key = names(&key["child_columns"]);
            }
            let Some(key) = constraint.get("foreign_key_constraint") else {
                continue;
            };
            // Unity Catalog names cannot contain a period, so the full name
            // of the parent table splits into catalog, schema and table.
            let parent: Vec<_> = key["parent_table"]
                .as_str()
                .unwrap_or("")
                .split('.')
                .collect();
            let (Some(name), [catalog, schema, table]) = (key["name"].as_str(), parent.as_slice())
            else {
                continue;
            };
            read.foreign_keys.push((
                name.to_owned(),
                ForeignKeyTarget {
                    relation: ObjectPath::default()
                        .child(ObjectKind::Catalog, *catalog)
                        .child(ObjectKind::Schema, *schema)
                        .child(ObjectKind::Table, *table),
                    columns: names(&key["child_columns"]),
                    referenced: names(&key["parent_columns"]),
                },
            ));
        }
        read
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
    /// The context last requested by a run, or reported by
    /// [`Session::context`]: a request for it is not applied again, so a
    /// `USE` that succeeded meanwhile stays in force.
    requested: SessionContext,
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
            requested: SessionContext::default(),
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
        let context = &request.options.context;
        if *context != self.requested {
            if let Some(catalog) = &context.catalog {
                self.catalog = Some(catalog.clone());
            }
            if let Some(schema) = &context.schema {
                self.schema = Some(schema.clone());
            }
            self.requested = context.clone();
        }
        if let Some(catalog) = &self.catalog {
            body["catalog"] = json!(catalog);
        }
        if let Some(schema) = &self.schema {
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
    /// The catalog and schema the session follows, from the profile's
    /// defaults and the `USE` statements that succeeded; nothing is asked of
    /// the server, and an open result stays open.
    async fn context(&mut self, _: CancellationToken) -> DriverResult<SessionContext> {
        self.requested = SessionContext {
            catalog: self.catalog.clone(),
            schema: self.schema.clone(),
        };
        Ok(self.requested.clone())
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

    // Native CLI coverage is separate from the credential-free portable gate.
    // This test lists synthetic profiles only; it never authenticates or queries.
    #[tokio::test]
    #[ignore = "requires native Databricks CLI; cargo test -p jykre-driver-databricks native_cli_profile_syntax -- --ignored"]
    async fn native_cli_profile_syntax_resolves_bom_and_section_comments() {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir =
            std::env::temp_dir().join(format!("jykre-cli-profile-{}-{stamp}", std::process::id()));
        tokio::fs::create_dir(&dir).await.unwrap();
        let input = ResolvedConnection {
            config: ConnectionConfig {
                driver: DriverKind::Databricks,
                options: json!({"profile":"fixture"}),
            },
            credentials: Credentials::default(),
        };
        for (index, content) in [
            "\u{feff}[fixture]\nhost = https://fixture.invalid\n",
            "[fixture] ; workspace comment\nhost = https://fixture.invalid\n",
            "[__settings__]\ndefault_profile = fixture\n[fixture]\nhost = https://fixture.invalid\naccount_id = 00000000-0000-0000-0000-000000000000\nworkspace_id = 100\n",
        ]
        .into_iter()
        .enumerate()
        {
            let path = dir.join(format!("profile-{index}.cfg"));
            tokio::fs::write(&path, content).await.unwrap();
            let result = Client::in_config_file(&input, path.clone()).await;
            tokio::fs::remove_file(path).await.unwrap();
            assert_eq!(result.unwrap().origin.as_str(), "https://fixture.invalid/");
        }
        tokio::fs::remove_dir(dir).await.unwrap();
    }
    #[tokio::test]
    async fn discovery_paginates_filters_job_clusters_and_never_starts_compute() {
        let (client,task) = fixture(vec![
            (200,json!({"clusters":[
                {"cluster_id":"z","cluster_name":"Zeta","cluster_source":"UI","state":"TERMINATED"},
                {"cluster_id":"job","cluster_name":"Job","cluster_source":"JOB","state":"RUNNING"}
            ],"next_page_token":"next"})),
            (200,json!({"clusters":[
                {"cluster_id":"a","cluster_name":"Alpha","cluster_source":"API","state":"RUNNING"},
                {"cluster_id":"z","cluster_name":"Zeta","cluster_source":"UI","state":"TERMINATED"}
            ]})),
        ]).await;
        let resources = compute_resources(&client, ComputeKind::Cluster)
            .await
            .unwrap();
        assert_eq!(
            resources.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
            vec!["a", "z"]
        );
        assert_eq!(resources[1].state, "TERMINATED");
        let requests = task.await.unwrap();
        assert!(requests[1].contains("page_token=next"));
        assert!(
            requests
                .iter()
                .all(|r| r.starts_with("GET /api/2.1/clusters/list"))
        );
    }
    #[tokio::test]
    async fn warehouses_are_listed_and_discovery_rejects_cycles() {
        let (client, task) = fixture(vec![(
            200,
            json!({"warehouses":[{"id":"warehouse","name":"Analytics","state":"STOPPED"}]}),
        )])
        .await;
        assert_eq!(
            compute_resources(&client, ComputeKind::Warehouse)
                .await
                .unwrap()[0]
                .name,
            "Analytics"
        );
        assert!(task.await.unwrap()[0].starts_with("GET /api/2.0/sql/warehouses"));
        let (client, task) = fixture(vec![
            (200, json!({"clusters":[],"next_page_token":"cycle"})),
            (200, json!({"clusters":[],"next_page_token":"cycle"})),
        ])
        .await;
        assert_eq!(
            compute_resources(&client, ComputeKind::Cluster)
                .await
                .unwrap_err()
                .code,
            ErrorCode::Protocol
        );
        task.await.unwrap();
    }
    #[tokio::test]
    async fn selected_compute_is_checked_without_submitting_sql() {
        let (mut client, task) = fixture(vec![(
            200,
            json!({"state":"TERMINATED","cluster_source":"UI"}),
        )])
        .await;
        client.config = DatabricksConfig::parse(
            &json!({"profile":"fixture","compute":"cluster","clusterId":"classic"}),
        )
        .unwrap();
        assert_eq!(
            check_compute(&client).await.unwrap_err().code,
            ErrorCode::Connection
        );
        assert!(task.await.unwrap()[0].starts_with("GET /api/2.1/clusters/get?cluster_id=classic"));
    }
    #[tokio::test]
    async fn unauthorized_refresh_is_retried_once_and_other_failures_are_not_replayed() {
        let (client, task) = fixture(vec![(401, json!({})), (401, json!({}))]).await;
        assert_eq!(
            client
                .request(
                    Method::POST,
                    "/api/1.2/commands/execute",
                    &[],
                    Some(json!({}))
                )
                .await
                .unwrap_err()
                .code,
            ErrorCode::Authentication
        );
        assert_eq!(task.await.unwrap().len(), 2);
        let (client, task) = fixture(vec![(500, json!({}))]).await;
        assert_eq!(
            client
                .request(
                    Method::POST,
                    "/api/1.2/commands/execute",
                    &[],
                    Some(json!({}))
                )
                .await
                .unwrap_err()
                .code,
            ErrorCode::Database
        );
        assert_eq!(task.await.unwrap().len(), 1);
    }
    pub(super) async fn fixture(
        responses: Vec<(u16, Value)>,
    ) -> (Client, tokio::task::JoinHandle<Vec<String>>) {
        fixture_with_hook(responses, |_| {}).await
    }

    pub(super) async fn fixture_with_hook(
        responses: Vec<(u16, Value)>,
        hook: impl Fn(usize) + Send + 'static,
    ) -> (Client, tokio::task::JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let mut requests = Vec::new();
            for (status, body) in responses {
                let (mut stream, _) =
                    tokio::time::timeout(Duration::from_secs(10), listener.accept())
                        .await
                        .unwrap()
                        .unwrap();
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
                hook(requests.len());
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
                auth: Auth::shared(
                    &DatabricksConfig::parse(&json!({"profile":"fixture"})).unwrap(),
                    "fixture.cfg".into(),
                    &Url::parse("https://fixture.invalid/").unwrap(),
                )
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
    async fn discovery_lists_catalogs_and_schemas_without_reading_tables() {
        let (client, task) = fixture(vec![
            (
                200,
                json!({"catalogs":[{"name":"main"},{"name":"hive_metastore"}],"next_page_token":"p2"}),
            ),
            (200, json!({"catalogs":[{"name":"analytics"}]})),
            (
                200,
                json!({"schemas":[{"name":"sales"},{"name":"default"}]}),
            ),
        ])
        .await;
        let root = ObjectPath::default();
        let catalogs = client.discover(&root).await.unwrap();
        assert_eq!(
            catalogs.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
            ["analytics", "main"]
        );
        assert!(catalogs.iter().all(|c| c.kind == ObjectKind::Catalog));
        let main = root.child(ObjectKind::Catalog, "main");
        let schemas = client.discover(&main).await.unwrap();
        assert_eq!(
            schemas.iter().map(|s| s.path.clone()).collect::<Vec<_>>(),
            [
                main.child(ObjectKind::Schema, "default"),
                main.child(ObjectKind::Schema, "sales")
            ]
        );
        // Below the last namespace level there is nothing to list.
        let below = client
            .discover(&main.child(ObjectKind::Schema, "sales"))
            .await
            .unwrap();
        assert!(below.is_empty());
        let requests = task.await.unwrap();
        assert_eq!(requests.len(), 3);
        assert!(requests[1].contains("page_token=p2"));
        assert!(requests[2].starts_with("GET /api/2.1/unity-catalog/schemas?"));
        assert!(requests[2].contains("catalog_name=main"));
        assert!(requests.iter().all(|r| !r.contains("/tables")));
    }

    #[tokio::test]
    async fn relation_kinds_and_keys_come_from_the_table_listing() {
        // A schema is read with its table listing alone: no GET of the
        // catalog or the schema first.
        let (client, task) = fixture(vec![
            (
                200,
                json!({"tables":[
                    {"name":"orders","table_type":"MANAGED","columns":[
                        {"name":"id","type_text":"BIGINT","position":0,"nullable":false,"type_json":"{\"name\":\"id\",\"type\":\"long\",\"metadata\":{\"CURRENT_DEFAULT\":\"42\"}}"},
                        {"name":"customer_id","type_text":"BIGINT","position":1}
                    ],"table_constraints":[
                        {"primary_key_constraint":{"name":"orders_pk","child_columns":["id"]}},
                        {"foreign_key_constraint":{"name":"orders_customer","child_columns":["customer_id"],"parent_table":"cat.crm.customers","parent_columns":["id"]}}
                    ]},
                    {"name":"daily","table_type":"MATERIALIZED_VIEW","columns":[]},
                    {"name":"recent","table_type":"VIEW","columns":[]}
                ]}),
            ),
        ])
        .await;
        let schema = ObjectPath::default()
            .child(ObjectKind::Catalog, "cat")
            .child(ObjectKind::Schema, "sch");
        let mut metadata = Metadata::default();
        client
            .introspect(&schema, IntrospectionLevel::Full, &mut metadata)
            .await
            .unwrap();
        let requests = task.await.unwrap();
        assert_eq!(requests.len(), 1);
        assert!(requests[0].starts_with("GET /api/2.1/unity-catalog/tables?"));
        let find = |path: &ObjectPath| {
            metadata
                .0
                .iter()
                .find(|object| object.path == *path)
                .unwrap()
                .clone()
        };
        // The schema node still comes with what it holds.
        find(&schema);
        let orders = schema.child(ObjectKind::Table, "orders");
        let id = find(&orders.child(ObjectKind::Column, "id"));
        assert_eq!(id.primary_key, Some(1));
        assert_eq!(id.default_value.as_deref(), Some("42"));
        assert_eq!(
            find(&orders.child(ObjectKind::Column, "customer_id")).primary_key,
            None
        );
        assert_eq!(
            find(&orders.child(ObjectKind::ForeignKey, "orders_customer")).references,
            Some(ForeignKeyTarget {
                relation: ObjectPath::default()
                    .child(ObjectKind::Catalog, "cat")
                    .child(ObjectKind::Schema, "crm")
                    .child(ObjectKind::Table, "customers"),
                columns: vec!["customer_id".into()],
                referenced: vec!["id".into()],
            })
        );
        find(&schema.child(ObjectKind::MaterializedView, "daily"));
        find(&schema.child(ObjectKind::View, "recent"));
    }

    #[tokio::test]
    async fn table_and_column_refresh_request_only_the_selected_relation() {
        for column_only in [false, true] {
            let (client, task) = fixture(vec![
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
            client
                .introspect(&scope, IntrospectionLevel::Full, &mut metadata)
                .await
                .unwrap();
            assert!(
                metadata
                    .0
                    .iter()
                    .all(|object| object.path.is_within(&scope))
            );
            assert_eq!(metadata.0.len(), if column_only { 1 } else { 3 });
            let requests = task.await.unwrap();
            assert_eq!(requests.len(), 1);
            assert!(requests[0].starts_with("GET /api/2.1/unity-catalog/tables/cat.sch.target "));
        }
    }

    #[tokio::test]
    async fn deleted_scoped_table_returns_an_empty_successful_snapshot() {
        let (client, task) = fixture(vec![(404, json!({}))]).await;
        let scope = ObjectPath::default()
            .child(ObjectKind::Catalog, "cat")
            .child(ObjectKind::Schema, "sch")
            .child(ObjectKind::Table, "gone");
        let mut metadata = Metadata::default();
        client
            .introspect(&scope, IntrospectionLevel::Full, &mut metadata)
            .await
            .unwrap();
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

    #[tokio::test]
    async fn names_omit_columns_and_a_schema_gone_reads_as_empty() {
        let (client, task) = fixture(vec![(
            200,
            json!({"tables":[{"name":"orders","table_type":"MANAGED"}]}),
        )])
        .await;
        let schema = ObjectPath::default()
            .child(ObjectKind::Catalog, "cat")
            .child(ObjectKind::Schema, "sch");
        let mut metadata = Metadata::default();
        client
            .introspect(&schema, IntrospectionLevel::Names, &mut metadata)
            .await
            .unwrap();
        let requests = task.await.unwrap();
        assert!(requests[0].contains("omit_columns=true"));
        assert!(metadata.0.iter().any(|object| object.name == "orders"));
        assert!(
            !metadata
                .0
                .iter()
                .any(|object| object.kind == ObjectKind::Column)
        );
        // A schema that no longer exists publishes nothing, not even itself.
        let (client, task) =
            fixture(vec![(404, json!({"error_code":"SCHEMA_DOES_NOT_EXIST"}))]).await;
        let mut metadata = Metadata::default();
        client
            .introspect(&schema, IntrospectionLevel::Full, &mut metadata)
            .await
            .unwrap();
        task.await.unwrap();
        assert!(metadata.0.is_empty());
    }

    #[tokio::test]
    async fn inspection_lists_keys_defaults_and_functions_and_views_have_definitions() {
        let table = json!({"name":"orders","table_type":"MANAGED",
            "columns":[{"name":"status","type_text":"STRING","position":0,"type_json":{"name":"status","metadata":{"CURRENT_DEFAULT":"'new'"}}}],
            "table_constraints":[{"primary_key_constraint":{"name":"orders_pk","child_columns":["id"]}}]});
        let (client, task) = fixture(vec![(200, table)]).await;
        let found = client.table("cat", "sch", "orders").await.unwrap();
        task.await.unwrap();
        assert_eq!(
            column_default(&found["columns"][0]).as_deref(),
            Some("'new'")
        );
        let (client, task) = fixture(vec![(
            200,
            json!({"functions":[{"name":"add_one","full_data_type":"INT","input_params":{"parameters":[{"name":"x","type_text":"INT"}]}}]}),
        )])
        .await;
        let functions = client
            .pages("/api/2.1/unity-catalog/functions", "functions", vec![])
            .await
            .unwrap();
        task.await.unwrap();
        assert_eq!(name(&functions[0]).unwrap(), "add_one");
    }

    #[test]
    fn the_profile_identifies_the_cache_and_system_waits_to_be_chosen() {
        use crate::DatabricksDriver;
        let config = |warehouse: &str| ConnectionConfig {
            driver: DriverKind::Databricks,
            options: json!({"profile":"dev","warehouseId":warehouse,"defaultCatalog":"main"}),
        };
        assert_eq!(
            DatabricksDriver.cache_identity(&config("a")),
            DatabricksDriver.cache_identity(&config("b"))
        );
        let system = ObjectPath::default().child(ObjectKind::Catalog, "system");
        assert!(DatabricksDriver.left_out_of_everything(&system));
        assert!(
            !DatabricksDriver
                .left_out_of_everything(&ObjectPath::default().child(ObjectKind::Catalog, "main"))
        );
    }
}
