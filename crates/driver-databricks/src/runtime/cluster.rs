//! Classic compute keeps a real SQL context per tab.
use super::*;
use std::collections::VecDeque;
use tokio::time::Instant;

// Command Execution returns table cells as JSON scalars, including decimals.
// Convert numeric and structured cells from their original wire slices to text
// before serde_json::Value can round a decimal through f64. SQL stays unchanged.
pub(super) fn decode_response(bytes: &[u8]) -> DriverResult<Value> {
    serde_json::from_slice::<serde::de::IgnoredAny>(bytes).map_err(|_| protocol())?;
    let result = field(bytes, "results")?;
    let Some(result) = result else {
        return serde_json::from_slice(bytes).map_err(|_| protocol());
    };
    let object = bytes.get(result.clone()).ok_or_else(protocol)?;
    let kind = field(object, "resultType")?.and_then(|range| object.get(range));
    if kind
        .and_then(|raw| serde_json::from_slice::<String>(raw).ok())
        .as_deref()
        != Some("table")
    {
        return serde_json::from_slice(bytes).map_err(|_| protocol());
    }
    let data = field(object, "data")?.ok_or_else(protocol)?;
    let start = result.start + data.start;
    let end = result.start + data.end;
    let raw = bytes.get(start..end).ok_or_else(protocol)?;
    let mut output = Vec::with_capacity(bytes.len());
    output.extend_from_slice(bytes.get(..start).ok_or_else(protocol)?);
    output.push(b'[');
    for (row_index, row) in elements(raw)?.into_iter().enumerate() {
        if row_index != 0 {
            output.push(b',');
        }
        output.push(b'[');
        for (index, cell) in elements(row)?.into_iter().enumerate() {
            if index != 0 {
                output.push(b',');
            }
            match cell.first() {
                Some(b'-' | b'0'..=b'9' | b'[' | b'{') => {
                    let text = std::str::from_utf8(cell).map_err(|_| protocol())?;
                    serde_json::to_writer(&mut output, text).map_err(|_| protocol())?;
                }
                _ => output.extend_from_slice(cell),
            }
        }
        output.push(b']');
    }
    output.push(b']');
    output.extend_from_slice(bytes.get(end..).ok_or_else(protocol)?);
    serde_json::from_slice(&output).map_err(|_| protocol())
}

fn whitespace(bytes: &[u8], at: &mut usize) {
    while bytes.get(*at).is_some_and(u8::is_ascii_whitespace) {
        *at += 1;
    }
}

// Input has already been validated as JSON with IgnoredAny. Find byte spans
// without interpreting values or allocating an intermediate numeric value.
fn value_end(bytes: &[u8], start: usize) -> DriverResult<usize> {
    let mut at = start;
    match bytes.get(at) {
        Some(b'"') => {
            at += 1;
            while let Some(byte) = bytes.get(at) {
                at += 1;
                if *byte == b'\\' {
                    at += 1;
                } else if *byte == b'"' {
                    return Ok(at);
                }
            }
        }
        Some(b'[' | b'{') => {
            let close = if bytes.get(at) == Some(&b'[') {
                b']'
            } else {
                b'}'
            };
            at += 1;
            loop {
                whitespace(bytes, &mut at);
                match bytes.get(at) {
                    Some(byte) if *byte == close => return Ok(at + 1),
                    Some(b',' | b':') => at += 1,
                    Some(_) => at = value_end(bytes, at)?,
                    None => break,
                }
            }
        }
        Some(_) => {
            while bytes.get(at).is_some_and(|byte| {
                !byte.is_ascii_whitespace() && !matches!(byte, b',' | b']' | b'}')
            }) {
                at += 1;
            }
            if at > start {
                return Ok(at);
            }
        }
        None => {}
    }
    Err(protocol())
}

fn field(bytes: &[u8], name: &str) -> DriverResult<Option<std::ops::Range<usize>>> {
    let mut at = 0;
    whitespace(bytes, &mut at);
    if bytes.get(at) != Some(&b'{') {
        return Err(protocol());
    }
    at += 1;
    let mut found = None;
    loop {
        whitespace(bytes, &mut at);
        if bytes.get(at) == Some(&b'}') {
            return Ok(found);
        }
        let end = value_end(bytes, at)?;
        let key: String = serde_json::from_slice(bytes.get(at..end).ok_or_else(protocol)?)
            .map_err(|_| protocol())?;
        at = end;
        whitespace(bytes, &mut at);
        if bytes.get(at) != Some(&b':') {
            return Err(protocol());
        }
        at += 1;
        whitespace(bytes, &mut at);
        let end = value_end(bytes, at)?;
        if key == name {
            found = Some(at..end);
        }
        at = end;
        whitespace(bytes, &mut at);
        if bytes.get(at) == Some(&b',') {
            at += 1;
        }
    }
}

fn elements(bytes: &[u8]) -> DriverResult<Vec<&[u8]>> {
    let mut at = 0;
    whitespace(bytes, &mut at);
    if bytes.get(at) != Some(&b'[') {
        return Err(protocol());
    }
    at += 1;
    let mut values = Vec::new();
    loop {
        whitespace(bytes, &mut at);
        if bytes.get(at) == Some(&b']') {
            return Ok(values);
        }
        let end = value_end(bytes, at)?;
        values.push(bytes.get(at..end).ok_or_else(protocol)?);
        at = end;
        whitespace(bytes, &mut at);
        if bytes.get(at) == Some(&b',') {
            at += 1;
        }
    }
}

#[derive(Debug)]
struct ClusterSession {
    client: Client,
    cluster: String,
    context: String,
    open: bool,
    catalog: Option<String>,
    schema: Option<String>,
    /// The context last requested by a run, or reported by
    /// [`Session::context`]: a request for it is not applied again, so a
    /// `USE` that succeeded meanwhile stays in force.
    requested: SessionContext,
    pending: Option<ClusterResult>,
}

#[derive(Debug)]
struct ClusterResult {
    columns: Vec<ColumnInfo>,
    rows: VecDeque<Value>,
    truncated: bool,
}

pub(super) async fn open(
    client: Client,
    cancel: &CancellationToken,
) -> DriverResult<Box<dyn Session>> {
    if cancel.is_cancelled() {
        return Err(DriverError::cancelled());
    }
    let cluster = client
        .config
        .cluster_id
        .clone()
        .ok_or_else(|| DriverError::validation("Select a classic cluster"))?;
    // Obtain the ID even when cancelled during submission, so cleanup can
    // destroy the context instead of leaking it on the server.
    let created = client
        .request(
            Method::POST,
            "/api/1.2/contexts/create",
            &[],
            Some(json!({"clusterId":cluster,"language":"sql"})),
        )
        .await?;
    let context = created["id"]
        .as_str()
        .filter(|id| !id.is_empty())
        .ok_or_else(protocol)?
        .to_owned();
    let mut session = ClusterSession {
        client,
        cluster,
        context,
        open: true,
        catalog: None,
        schema: None,
        requested: SessionContext::default(),
        pending: None,
    };
    let deadline = Some(Instant::now() + Duration::from_secs(90));
    let ready = bounded(cancel, Duration::from_secs(90), session.wait_ready()).await;
    if let Err(failure) = ready {
        session.destroy_best_effort().await;
        return Err(failure);
    }
    let catalog = session.client.config.default_catalog.clone();
    let schema = session.client.config.default_schema.clone();
    if let Err(failure) = session
        .set_context(catalog.as_deref(), schema.as_deref(), cancel, deadline)
        .await
    {
        session.destroy_best_effort().await;
        return Err(failure);
    }
    Ok(Box::new(session))
}

impl ClusterSession {
    fn ids(&self) -> Value {
        json!({"clusterId":self.cluster,"contextId":self.context})
    }
    fn query(&self) -> Vec<(String, String)> {
        vec![
            ("clusterId".into(), self.cluster.clone()),
            ("contextId".into(), self.context.clone()),
        ]
    }
    async fn wait_ready(&self) -> DriverResult<()> {
        let mut wait = Duration::from_millis(200);
        loop {
            let status = self
                .client
                .request(Method::GET, "/api/1.2/contexts/status", &self.query(), None)
                .await?;
            match status["status"].as_str() {
                Some("Running") => return Ok(()),
                Some("Pending") => {
                    tokio::time::sleep(wait).await;
                    wait = (wait * 2).min(POLL_MAX);
                }
                Some("Error") => {
                    return Err(error(
                        ErrorCode::Connection,
                        "Databricks could not create the cluster SQL context",
                    ));
                }
                _ => return Err(protocol()),
            }
        }
    }
    async fn destroy(&self) -> DriverResult<()> {
        tokio::time::timeout(
            Duration::from_secs(5),
            self.client.request(
                Method::POST,
                "/api/1.2/contexts/destroy",
                &[],
                Some(self.ids()),
            ),
        )
        .await
        .map_err(|_| error(ErrorCode::Timeout, "Cluster context cleanup timed out"))??;
        Ok(())
    }
    async fn destroy_best_effort(&mut self) {
        self.open = false;
        // Keep the original failure. An unconfirmed context is never reused.
        let _ = self.destroy().await;
    }
    async fn recover(&mut self, id: &str) {
        let mut body = self.ids();
        body["commandId"] = json!(id);
        let mut query = self.query();
        query.push(("commandId".into(), id.into()));
        let cleanup = async {
            self.client
                .request(Method::POST, "/api/1.2/commands/cancel", &[], Some(body))
                .await?;
            loop {
                let status = self
                    .client
                    .request(Method::GET, "/api/1.2/commands/status", &query, None)
                    .await?;
                match status["status"].as_str() {
                    Some("Cancelled" | "Finished" | "Error") => return Ok::<_, DriverError>(()),
                    Some("Queued" | "Running" | "Cancelling") => {
                        tokio::time::sleep(Duration::from_millis(200)).await
                    }
                    _ => return Err(protocol()),
                }
            }
        };
        if !matches!(
            tokio::time::timeout(Duration::from_secs(5), cleanup).await,
            Ok(Ok(()))
        ) {
            self.destroy_best_effort().await;
        }
    }
    async fn run(
        &mut self,
        sql: &str,
        cancel: &CancellationToken,
        deadline: Option<Instant>,
    ) -> DriverResult<Value> {
        if cancel.is_cancelled() {
            return Err(DriverError::cancelled());
        }
        if deadline.is_some_and(|at| at <= Instant::now()) {
            return Err(error(ErrorCode::Timeout, "Databricks operation timed out"));
        }
        let mut body = self.ids();
        body["language"] = json!("sql");
        body["command"] = json!(sql);
        let response = match self
            .client
            .request(Method::POST, "/api/1.2/commands/execute", &[], Some(body))
            .await
        {
            Ok(response) => response,
            Err(failure) => {
                self.destroy_best_effort().await;
                return Err(failure);
            }
        };
        let Some(id) = response["id"].as_str().filter(|s| !s.is_empty()) else {
            self.destroy_best_effort().await;
            return Err(protocol());
        };
        let mut query = self.query();
        query.push(("commandId".into(), id.into()));
        let work = async {
            let mut wait = Duration::from_millis(200);
            loop {
                let response = self
                    .client
                    .request(Method::GET, "/api/1.2/commands/status", &query, None)
                    .await?;
                match response["status"].as_str() {
                    Some("Finished" | "Error" | "Cancelled") => return Ok(response),
                    Some("Queued" | "Running" | "Cancelling") => {
                        tokio::time::sleep(wait).await;
                        wait = (wait * 2).min(POLL_MAX);
                    }
                    _ => return Err(protocol()),
                }
            }
        };
        let result = match deadline {
            Some(at) => bounded(cancel, at.saturating_duration_since(Instant::now()), work).await,
            None => {
                tokio::select! { biased; _ = cancel.cancelled() => Err(DriverError::cancelled()), result = work => result }
            }
        };
        let response = match result {
            Ok(response) => response,
            Err(failure) => {
                self.recover(id).await;
                return Err(failure);
            }
        };
        if response["status"] == "Cancelled" {
            return Err(DriverError::cancelled());
        }
        let results = &response["results"];
        if response["status"] == "Error" || results["resultType"] == "error" {
            return Err(statement_error(
                &json!({"message": results["cause"].as_str().or(results["summary"].as_str()).unwrap_or("Databricks cluster SQL command failed")}),
            ));
        }
        Ok(response["results"].clone())
    }
    fn apply(&mut self, change: UseStatement) {
        match change {
            UseStatement::Catalog(catalog) => {
                self.catalog = Some(catalog);
                self.schema = Some("default".into());
            }
            UseStatement::Schema { catalog, schema } => {
                if catalog.is_some() {
                    self.catalog = catalog;
                }
                self.schema = Some(schema);
            }
        }
    }
    async fn set_context(
        &mut self,
        catalog: Option<&str>,
        schema: Option<&str>,
        cancel: &CancellationToken,
        deadline: Option<Instant>,
    ) -> DriverResult<()> {
        if let Some(catalog) = catalog
            && self.catalog.as_deref() != Some(catalog)
        {
            self.run(
                &format!(
                    "USE CATALOG {}",
                    crate::DatabricksDriver.quote_identifier(catalog)
                ),
                cancel,
                deadline,
            )
            .await?;
            self.apply(UseStatement::Catalog(catalog.into()));
        }
        if let Some(schema) = schema
            && self.schema.as_deref() != Some(schema)
        {
            self.run(
                &format!(
                    "USE SCHEMA {}",
                    crate::DatabricksDriver.quote_identifier(schema)
                ),
                cancel,
                deadline,
            )
            .await?;
            self.apply(UseStatement::Schema {
                catalog: None,
                schema: schema.into(),
            });
        }
        Ok(())
    }
}

impl ClusterResult {
    fn parse(mut value: Value) -> DriverResult<Self> {
        let mut columns = Vec::new();
        let rows = match value["resultType"].as_str() {
            Some("table") => {
                for column in value["schema"].as_array().ok_or_else(protocol)? {
                    columns.push(ColumnInfo {
                        name: name(column)?.into(),
                        data_type: column["type"].as_str().ok_or_else(protocol)?.into(),
                        nullable: None,
                    });
                }
                value["data"]
                    .as_array_mut()
                    .ok_or_else(protocol)?
                    .drain(..)
                    .collect()
            }
            Some("text") => {
                if value["data"].as_str().is_some_and(|s| !s.is_empty()) {
                    columns.push(ColumnInfo {
                        name: "result".into(),
                        data_type: "STRING".into(),
                        nullable: None,
                    });
                    VecDeque::from([json!([value["data"].take()])])
                } else {
                    VecDeque::new()
                }
            }
            _ => return Err(protocol()),
        };
        Ok(Self {
            columns,
            rows,
            truncated: value["truncated"].as_bool().unwrap_or(false),
        })
    }
    fn page(
        &mut self,
        options: &ExecutionOptions,
        sink: &mut dyn RowSink,
        cancel: &CancellationToken,
        deadline: Option<Instant>,
    ) -> DriverResult<DriverOutcome> {
        for _ in 0..options.max_rows {
            if cancel.is_cancelled() {
                return Err(DriverError::cancelled());
            }
            if deadline.is_some_and(|at| at <= Instant::now()) {
                return Err(error(ErrorCode::Timeout, "Databricks operation timed out"));
            }
            let Some(row) = self.rows.pop_front() else {
                break;
            };
            let values = row
                .as_array()
                .filter(|values| values.len() == self.columns.len())
                .ok_or_else(protocol)?;
            let cells = values
                .iter()
                .map(|value| {
                    if value.is_null() {
                        return Ok(Cell::Null);
                    }
                    Ok(Cell::Text(
                        value
                            .as_str()
                            .map(str::to_owned)
                            .unwrap_or_else(|| value.to_string()),
                    ))
                })
                .collect::<DriverResult<Vec<_>>>()?;
            if sink.row(cells) == SinkControl::Stop {
                self.rows.clear();
                return Ok(DriverOutcome::default());
            }
        }
        Ok(DriverOutcome {
            has_more: !self.rows.is_empty(),
            truncated: self.rows.is_empty() && self.truncated,
            ..DriverOutcome::default()
        })
    }
}

fn execution_deadline(options: &ExecutionOptions) -> DriverResult<Option<Instant>> {
    options
        .timeout
        .map(|limit| {
            Instant::now()
                .checked_add(limit)
                .ok_or_else(|| DriverError::validation("Statement timeout is too large"))
        })
        .transpose()
}

#[async_trait]
impl Session for ClusterSession {
    async fn execute(
        &mut self,
        request: &QueryRequest,
        sink: &mut dyn RowSink,
        cancel: CancellationToken,
    ) -> DriverResult<DriverOutcome> {
        request.validate()?;
        self.pending = None;
        if !self.open {
            return Err(error(
                ErrorCode::Connection,
                "Cluster SQL context is closed",
            ));
        }
        let deadline = execution_deadline(&request.options)?;
        let context = request.options.context.clone();
        if context != self.requested {
            self.set_context(
                context.catalog.as_deref(),
                context.schema.as_deref(),
                &cancel,
                deadline,
            )
            .await?;
            self.requested = context;
        }
        let value = self.run(&request.sql, &cancel, deadline).await?;
        if let Some(change) = use_statement(&request.sql) {
            self.apply(change);
        }
        let mut result = ClusterResult::parse(value)?;
        if sink.columns(result.columns.clone()) == SinkControl::Stop {
            return Ok(DriverOutcome::default());
        }
        let outcome = result.page(&request.options, sink, &cancel, deadline)?;
        if outcome.has_more {
            self.pending = Some(result);
        }
        Ok(outcome)
    }
    async fn fetch(
        &mut self,
        options: &ExecutionOptions,
        sink: &mut dyn RowSink,
        cancel: CancellationToken,
    ) -> DriverResult<DriverOutcome> {
        QueryRequest {
            parameters: Vec::new(),
            sql: "fetch".into(),
            options: options.clone(),
        }
        .validate()?;
        let mut result = self
            .pending
            .take()
            .ok_or_else(|| DriverError::validation("No result has more rows to read"))?;
        let outcome = result.page(options, sink, &cancel, execution_deadline(options)?)?;
        if outcome.has_more {
            self.pending = Some(result);
        }
        Ok(outcome)
    }
    fn is_open(&self) -> bool {
        self.open
    }
    /// The catalog and schema the cluster context is in, as the session
    /// follows it (its defaults and the `USE` statements that succeeded).
    async fn context(&mut self, _: CancellationToken) -> DriverResult<SessionContext> {
        self.requested = SessionContext {
            catalog: self.catalog.clone(),
            schema: self.schema.clone(),
        };
        Ok(self.requested.clone())
    }
    async fn ping(&mut self) -> DriverResult<()> {
        self.pending = None;
        if !self.open {
            return Err(error(
                ErrorCode::Connection,
                "Cluster SQL context is closed",
            ));
        }
        let result = bounded(
            &CancellationToken::new(),
            Duration::from_secs(5),
            self.wait_ready(),
        )
        .await;
        if result.is_err() {
            self.destroy_best_effort().await;
        }
        result
    }
    async fn close(mut self: Box<Self>) -> DriverResult<()> {
        self.pending = None;
        if self.open {
            self.destroy().await
        } else {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::fixture;

    #[tokio::test]
    async fn cancellation_after_submission_confirms_remote_stop_and_keeps_the_session() {
        let cancel = CancellationToken::new();
        let signal = cancel.clone();
        let (mut client, task) = super::super::tests::fixture_with_hook(
            vec![
                (200, json!({"id":"context"})),
                (200, json!({"status":"Running"})),
                (200, json!({"id":"command"})),
                (200, json!({"status":"Running"})),
                (200, json!({})),
                (200, json!({"status":"Cancelled"})),
                (200, json!({})),
            ],
            move |count| {
                if count == 4 {
                    signal.cancel();
                }
            },
        )
        .await;
        client.config = DatabricksConfig::parse(
            &json!({"profile":"fixture","compute":"cluster","clusterId":"classic"}),
        )
        .unwrap();
        let mut session = open(client, &cancel).await.unwrap();
        assert_eq!(
            session
                .execute(&query("SELECT slow", 1), &mut Rows::default(), cancel)
                .await
                .unwrap_err()
                .code,
            ErrorCode::Cancelled
        );
        assert!(session.is_open());
        session.close().await.unwrap();
        let requests = task.await.unwrap();
        assert!(requests[4].starts_with("POST /api/1.2/commands/cancel"));
        assert!(requests[5].starts_with("GET /api/1.2/commands/status"));
    }

    #[tokio::test]
    async fn cancellation_during_context_creation_still_destroys_the_context() {
        let cancel = CancellationToken::new();
        let signal = cancel.clone();
        let (mut client, task) = super::super::tests::fixture_with_hook(
            vec![(200, json!({"id":"context"})), (200, json!({}))],
            move |count| {
                if count == 1 {
                    signal.cancel();
                }
            },
        )
        .await;
        client.config = DatabricksConfig::parse(
            &json!({"profile":"fixture","compute":"cluster","clusterId":"classic"}),
        )
        .unwrap();
        assert_eq!(
            open(client, &cancel).await.unwrap_err().code,
            ErrorCode::Cancelled
        );
        assert!(task.await.unwrap()[1].starts_with("POST /api/1.2/contexts/destroy"));
    }
    use super::*;

    #[derive(Default)]
    struct Rows(Vec<Row>);
    impl RowSink for Rows {
        fn columns(&mut self, _: Vec<ColumnInfo>) -> SinkControl {
            SinkControl::Continue
        }
        fn row(&mut self, row: Row) -> SinkControl {
            self.0.push(row);
            SinkControl::Continue
        }
    }
    fn query(sql: &str, max_rows: u32) -> QueryRequest {
        QueryRequest {
            parameters: Vec::new(),
            sql: sql.into(),
            options: ExecutionOptions {
                max_rows,
                ..ExecutionOptions::default()
            },
        }
    }
    fn finished() -> Value {
        json!({"status":"Finished","results":{"resultType":"text","data":""}})
    }
    async fn client(
        responses: Vec<(u16, Value)>,
    ) -> (Client, tokio::task::JoinHandle<Vec<String>>) {
        let (mut client, task) = fixture(responses).await;
        client.config = DatabricksConfig::parse(
            &json!({"profile":"fixture","compute":"cluster","clusterId":"classic-123"}),
        )
        .unwrap();
        (client, task)
    }
    #[test]
    fn cluster_json_preserves_decimal_lexemes_and_structured_values() {
        let raw = br#"{"status":"Finished","results":{"resultType":"table","data":[[9007199254740993.0123456789,12345678901234567890123456789012345678,null,"NULL",true,{"n":9007199254740993.1},[1,"two"],"quote\" and \\ slash",1e-10]],"schema":[]}}"#;
        let value = decode_response(raw).unwrap();
        let data = &value["results"]["data"][0];
        assert_eq!(data[0], "9007199254740993.0123456789");
        assert_eq!(data[1], "12345678901234567890123456789012345678");
        assert!(data[2].is_null());
        assert_eq!(data[3], "NULL");
        assert_eq!(data[4], true);
        assert_eq!(data[5], r#"{"n":9007199254740993.1}"#);
        assert_eq!(data[6], r#"[1,"two"]"#);
        assert_eq!(data[7], "quote\" and \\ slash");
        assert_eq!(data[8], "1e-10");
        assert!(decode_response(br#"{"results":{"resultType":"table","data":[[01]]}}"#).is_err());
        assert!(decode_response(br#"{"results":{"resultType":"table","data":{}}}"#).is_err());
        assert_eq!(
            decode_response(br#"{"results":{"resultType":"error","cause":"bad"}}"#).unwrap()["results"]
                ["cause"],
            "bad"
        );
    }
    #[tokio::test]
    async fn classic_context_is_reused_pages_are_local_and_close_destroys_context() {
        let (client, task) = client(vec![
            (200,json!({"id":"context-1"})), (200,json!({"status":"Running"})),
            (200,json!({"id":"command-1"})),
            (200,json!({"status":"Finished","results":{"resultType":"table","schema":[{"name":"amount","type":"decimal(38,10)"}],"data":[["9007199254740993.01"],[null],["NULL"]],"truncated":true}})),
            (200,json!({"id":"command-2"})), (200,finished()), (200,json!({})),
        ]).await;
        let mut session = open(client, &CancellationToken::new()).await.unwrap();
        let mut rows = Rows::default();
        let request = query("SELECT amount FROM main.sales", 1);
        let outcome = session
            .execute(&request, &mut rows, CancellationToken::new())
            .await
            .unwrap();
        assert!(outcome.has_more && !outcome.truncated);
        assert!(
            session
                .fetch(&request.options, &mut rows, CancellationToken::new())
                .await
                .unwrap()
                .has_more
        );
        let last = session
            .fetch(&request.options, &mut rows, CancellationToken::new())
            .await
            .unwrap();
        assert!(!last.has_more && last.truncated);
        assert_eq!(
            rows.0,
            vec![
                vec![Cell::Text("9007199254740993.01".into())],
                vec![Cell::Null],
                vec![Cell::Text("NULL".into())]
            ]
        );
        session
            .execute(
                &query("USE CATALOG main", 10),
                &mut rows,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        session.close().await.unwrap();
        let requests = task.await.unwrap();
        assert_eq!(
            requests
                .iter()
                .filter(|r| r.starts_with("POST /api/1.2/contexts/create"))
                .count(),
            1
        );
        assert_eq!(
            requests
                .iter()
                .filter(|r| r.starts_with("POST /api/1.2/commands/execute"))
                .count(),
            2
        );
        assert!(requests[2].contains("SELECT amount FROM main.sales"));
        assert!(requests[2].contains("\"contextId\":\"context-1\""));
        assert!(
            requests
                .last()
                .unwrap()
                .starts_with("POST /api/1.2/contexts/destroy")
        );
        assert!(requests.iter().all(|r| !r.contains("/sql/statements")));
    }
    #[tokio::test]
    async fn database_error_and_confirmed_timeout_keep_context_and_never_replay_sql() {
        let (client,task) = client(vec![
            (200,json!({"id":"context"})),(200,json!({"status":"Running"})),
            (200,json!({"id":"bad"})),(200,json!({"status":"Finished","results":{"resultType":"error","cause":"syntax error SQLSTATE: 42601"}})),
            (200,json!({"id":"slow"})),(200,json!({"status":"Running"})),
            (200,json!({})),(200,json!({"status":"Cancelled"})),
            (200,json!({"id":"next"})),(200,finished()),(200,json!({})),
        ]).await;
        let mut session = open(client, &CancellationToken::new()).await.unwrap();
        let mut rows = Rows::default();
        let failure = session
            .execute(
                &query("invalid SQL", 10),
                &mut rows,
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert_eq!(failure.code, ErrorCode::Database);
        assert_eq!(failure.server.unwrap().sqlstate.as_deref(), Some("42601"));
        assert!(session.is_open());
        let mut slow = query("SELECT slow", 10);
        slow.options.timeout = Some(Duration::from_millis(50));
        assert_eq!(
            session
                .execute(&slow, &mut rows, CancellationToken::new())
                .await
                .unwrap_err()
                .code,
            ErrorCode::Timeout
        );
        assert!(session.is_open());
        session
            .execute(
                &query("SELECT next", 10),
                &mut rows,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        session.close().await.unwrap();
        let requests = task.await.unwrap();
        assert_eq!(
            requests
                .iter()
                .filter(|r| r.contains("\"command\":\"SELECT slow\""))
                .count(),
            1
        );
        assert!(requests[6].starts_with("POST /api/1.2/commands/cancel"));
        assert!(requests[7].starts_with("GET /api/1.2/commands/status"));
    }
    #[tokio::test]
    async fn failed_recovery_closes_the_context() {
        let (client, task) = client(vec![
            (200, json!({"id":"context"})),
            (200, json!({"status":"Running"})),
            (200, json!({"id":"command"})),
            (200, json!({"status":"unexpected"})),
            (500, json!({})),
            (200, json!({})),
        ])
        .await;
        let mut session = open(client, &CancellationToken::new()).await.unwrap();
        let failure = session
            .execute(
                &query("SELECT 1", 1),
                &mut Rows::default(),
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert_eq!(failure.code, ErrorCode::Protocol);
        assert!(!session.is_open());
        assert!(
            task.await
                .unwrap()
                .last()
                .unwrap()
                .starts_with("POST /api/1.2/contexts/destroy")
        );
    }
    #[tokio::test]
    async fn defaults_are_quoted_and_applied_once_in_the_cluster_context() {
        let (mut client, task) = client(vec![
            (200, json!({"id":"context"})),
            (200, json!({"status":"Running"})),
            (200, json!({"id":"catalog"})),
            (200, finished()),
            (200, json!({"id":"schema"})),
            (200, finished()),
            (200, json!({"id":"sql"})),
            (200, finished()),
            (200, json!({})),
        ])
        .await;
        client.config.default_catalog = Some("a`b".into());
        client.config.default_schema = Some("s.x".into());
        let mut session = open(client, &CancellationToken::new()).await.unwrap();
        let mut request = query("SELECT 1", 10);
        request.options.context = SessionContext {
            catalog: Some("a`b".into()),
            schema: Some("s.x".into()),
        };
        session
            .execute(&request, &mut Rows::default(), CancellationToken::new())
            .await
            .unwrap();
        session.close().await.unwrap();
        let requests = task.await.unwrap();
        assert!(requests[2].contains("USE CATALOG `a``b`"));
        assert!(requests[4].contains("USE SCHEMA `s.x`"));
        assert_eq!(
            requests
                .iter()
                .filter(|r| r.starts_with("POST /api/1.2/commands/execute"))
                .count(),
            3
        );
    }
}
