//! What the Automation views decide without a window: where a job's extra
//! statements start, which confirmations a run needs, what one plan dialog
//! lists, which sources a chain step's input may take, what a step run
//! again on its own is prefilled with, and how a chain run's steps read in
//! its history. Tested here; the views only call these.
use super::words;
use jykre::automation::JobId;
use jykre::automation::definition::{
    ChainBody, ChainStep, JobBody, JobDefinition, Protocol, Source,
};
use jykre::automation::{Cause, JobKind, RunState};
use jykre::sql::SqlDialect;
use jykre::tr;
use jykre_core::automation::{ChainRunRecord, RunOutput, RunRecord};
use std::collections::HashMap;

/// The line (from 1) where each statement after the first starts: a job
/// runs exactly one, so these are marked before saving (section 4.3).
pub fn extra_statements(sql: &str, dialect: Option<SqlDialect>) -> Vec<usize> {
    jykre::sql::statements(sql, dialect)
        .into_iter()
        .filter_map(|range| {
            let text = sql.get(range.clone())?;
            let skipped = text.len() - text.trim_start().len();
            (!text.trim().is_empty()).then(|| range.start + skipped)
        })
        .skip(1)
        .map(|start| sql[..start].matches('\n').count() + 1)
        .collect()
}

/// What the editor says about the statements, or `None` for exactly one.
pub fn statements_note(lines: &[usize]) -> Option<String> {
    if lines.is_empty() {
        return None;
    }
    if let [line] = lines {
        return Some(tr!(
            "This job holds 2 statements, and a job runs exactly one. The extra statement starts on line {0}; remove it before running.",
            line
        ));
    }
    let lines: Vec<String> = lines.iter().map(ToString::to_string).collect();
    Some(tr!(
        "This job holds {0} statements, and a job runs exactly one. The extra statements start on lines {1}; remove them before running.",
        lines.len() + 1,
        lines.join(", ")
    ))
}

/// The confirmation a SQL run on a production connection needs before it
/// starts: its risky statement (`guard::risk`, described as a tab does)
/// and, when the job stores its outputs, that they will be stored
/// (ADR-0035). `None` when nothing needs confirming.
pub fn production_confirmation(
    connection: &str,
    production: bool,
    risk: Option<String>,
    stores_outputs: bool,
) -> Option<(String, String)> {
    if !production || (risk.is_none() && !stores_outputs) {
        return None;
    }
    let mut parts = Vec::new();
    if let Some(risk) = risk {
        parts.push(risk);
    }
    if stores_outputs {
        parts.push(words::production_outputs(connection));
    }
    Some((tr!("Run on {0}?", connection), parts.join("\n\n")))
}

/// One step of a chain's plan, as the one confirmation lists it
/// (section 9.3.4).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlanLine {
    pub id: String,
    pub job: String,
    pub version: i64,
    pub kind: JobKind,
    /// Where a SQL step runs.
    pub connection: Option<String>,
    pub production: bool,
    /// The step's risky statement on production, described.
    pub risk: Option<String>,
    /// A program whose version has not run before: the trusted-code
    /// warning (Q-10).
    pub new_program: bool,
    /// The job's draft differs from the version the step runs.
    pub changed_since: bool,
}

/// The description of the one dialog that confirms a whole chain: every
/// step, its connection, production risks and each new external program.
pub fn plan_description(lines: &[PlanLine]) -> String {
    let mut parts: Vec<String> = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        let mut text = tr!(
            "{0}. {1} · {2} version {3} ({4})",
            index + 1,
            line.id,
            line.job,
            line.version,
            words::kind_name(line.kind)
        );
        if let Some(connection) = &line.connection {
            text.push_str(" · ");
            text.push_str(&if line.production {
                tr!("on {0} (production)", connection)
            } else {
                tr!("on {0}", connection)
            });
        }
        if line.changed_since {
            text.push_str(" · ");
            text.push_str(tr!("the job changed since this version"));
        }
        if let Some(risk) = &line.risk {
            text.push_str("\n   ");
            text.push_str(risk);
        }
        if line.new_program {
            text.push_str("\n   ");
            text.push_str(&tr!(
                "First run of this program version. {0}",
                words::trusted_note()
            ));
        }
        parts.push(text);
    }
    let mut description = parts.join("\n");
    if lines.iter().any(|line| line.new_program) {
        description.push_str("\n\n");
        description.push_str(words::logs_note());
    }
    description.push_str("\n\n");
    description.push_str(tr!(
        "Exactly this plan runs. A step whose connection's settings change before it starts does not start. Steps after one that fails do not run."
    ));
    description
}

/// A source a chain step's input may take, as its list offers it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceChoice {
    pub label: String,
    /// `None`: no source; the input takes its default, or NULL.
    pub source: Option<SourceKind>,
}

/// What a choice binds; a constant takes its text from a field.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SourceKind {
    Input(String),
    Step { step: String, output: String },
    Constant,
}

impl SourceKind {
    pub fn of(source: &Source) -> Self {
        match source {
            Source::Input(name) => SourceKind::Input(name.clone()),
            Source::Step { step, output } => SourceKind::Step {
                step: step.clone(),
                output: output.clone(),
            },
            Source::Value(_) => SourceKind::Constant,
        }
    }
}

/// The sources the input of step `index` may take: no source, each input of
/// the chain, each declared output of an earlier step, or a constant. An
/// earlier `none` program declares no outputs, so it offers none
/// (section 9.1.2). `versions[i]` is the definition of step `i`'s version,
/// once read.
pub fn source_choices(
    chain: &ChainBody,
    index: usize,
    versions: &[Option<JobDefinition>],
) -> Vec<SourceChoice> {
    let mut choices = vec![SourceChoice {
        label: tr!("Its default, or NULL").to_owned(),
        source: None,
    }];
    for input in &chain.inputs {
        choices.push(SourceChoice {
            label: tr!("Chain input {0}", input.name),
            source: Some(SourceKind::Input(input.name.clone())),
        });
    }
    for (earlier, step) in chain.steps.iter().enumerate().take(index) {
        let Some(Some(definition)) = versions.get(earlier) else {
            continue;
        };
        for output in definition.outputs() {
            choices.push(SourceChoice {
                label: tr!("Output {0} of step {1}", output.name, step.id),
                source: Some(SourceKind::Step {
                    step: step.id.clone(),
                    output: output.name.clone(),
                }),
            });
        }
    }
    choices.push(SourceChoice {
        label: tr!("A constant").to_owned(),
        source: Some(SourceKind::Constant),
    });
    choices
}

/// Whether a step's version is a program without a protocol, whose outputs
/// a later step cannot read.
pub fn unreadable_step(definition: &JobDefinition) -> bool {
    matches!(&definition.body, JobBody::Process(process) if process.protocol == Protocol::None)
}

/// A new step's ID, unique in the chain: `step_1`, `step_2`, …
pub fn new_step_id(chain: &ChainBody) -> String {
    (1..)
        .map(|n| format!("step_{n}"))
        .find(|id| chain.steps.iter().all(|step| &step.id != id))
        .unwrap_or_else(|| "step".to_owned())
}

/// What a step run again on its own is prefilled with (specification,
/// section 9.3.5; ADR-0035): a constant of the chain, or an output an
/// earlier step stored whose value has not expired. Chain inputs are never
/// stored, so those fields stay empty, as do outputs not stored. Returns
/// the values and the inputs left to fill in.
pub fn prefill(
    step: &ChainStep,
    target: &JobDefinition,
    stored: &HashMap<String, Vec<RunOutput>>,
) -> (HashMap<String, String>, Vec<String>) {
    let mut values = HashMap::new();
    let mut missing = Vec::new();
    for spec in target.inputs() {
        let value = match step.source(&spec.name) {
            Some(Source::Value(constant)) => Some(constant.clone()),
            Some(Source::Step { step: from, output }) => stored.get(from).and_then(|outputs| {
                let found = outputs.iter().find(|candidate| &candidate.name == output)?;
                if found.expired {
                    None
                } else if found.is_null {
                    // NULL is an empty field, as in a form.
                    Some(String::new())
                } else {
                    found.value.clone()
                }
            }),
            Some(Source::Input(_)) | None => None,
        };
        match value {
            Some(value) => {
                values.insert(spec.name.clone(), value);
            }
            None => missing.push(spec.name.clone()),
        }
    }
    (values, missing)
}

/// How one step of a chain run reads in its history.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StepLine {
    pub id: String,
    pub job: Option<JobId>,
    /// The step's run, when it started.
    pub run: Option<RunRecord>,
    pub text: String,
}

/// Every step of a chain run: those that ran with their run, the others
/// **Not run**, naming the step and cause that stopped the chain
/// (section 9.3.5).
pub fn chain_steps(
    chain: &ChainRunRecord,
    steps: &[(String, Option<JobId>)],
    runs: &[RunRecord],
) -> Vec<StepLine> {
    let stopped_by = chain.failed_step.clone().or_else(|| {
        runs.iter()
            .filter(|run| run.state != RunState::Succeeded)
            .filter_map(|run| run.step_index)
            .filter_map(|index| usize::try_from(index).ok())
            .filter_map(|index| steps.get(index).map(|(id, _)| id.clone()))
            .next()
    });
    let cause = match (chain.state, chain.cause) {
        (RunState::Failed, Some(cause)) => words::cause_sentence(cause).to_owned(),
        (RunState::Cancelled, _) => tr!("the chain was stopped.").to_owned(),
        (RunState::Interrupted, _) => tr!("Jykre Studio closed while it ran.").to_owned(),
        _ => String::new(),
    };
    steps
        .iter()
        .enumerate()
        .map(|(index, (id, job))| {
            let run = runs
                .iter()
                .find(|run| run.step_index == i64::try_from(index).ok())
                .cloned();
            let text = match &run {
                Some(run) => {
                    let elapsed = run.finished_at.map(|end| end.since(run.started_at));
                    words::state_label(run.state, run.cause, elapsed)
                }
                None => match &stopped_by {
                    Some(step) => tr!("Not run: the chain stopped at step {0}. {1}", step, cause),
                    None => tr!("Not run.").to_owned(),
                },
            };
            StepLine {
                id: id.clone(),
                job: *job,
                run,
                text,
            }
        })
        .collect()
}

/// A chain run whose cause is `PlanChanged` names the connection whose
/// settings changed: that of the step that stopped it.
pub fn plan_changed(
    chain: &ChainRunRecord,
    connection_of_step: impl Fn(&str) -> Option<String>,
) -> Option<String> {
    if chain.cause != Some(Cause::PlanChanged) {
        return None;
    }
    let step = chain.failed_step.as_deref()?;
    let connection = connection_of_step(step)?;
    Some(plan_changed_text(&connection, step))
}

/// Which connection changed and which step did not start (`PlanChanged`).
pub fn plan_changed_text(connection: &str, step: &str) -> String {
    tr!(
        "The settings of {0} changed after the plan was confirmed, so step {1} did not start.",
        connection,
        step
    )
}

/// A run's or chain run's frozen definition, from its snapshot.
pub fn snapshot_definition(snapshot: &str) -> Option<JobDefinition> {
    let value: serde_json::Value = serde_json::from_str(snapshot).ok()?;
    JobDefinition::from_value(value.get("definition")?).ok()
}

/// One step of a chain run's snapshot: its ID, version and definition, and
/// the connection it ran on.
#[derive(Clone, Debug)]
pub struct SnapshotStep {
    pub id: String,
    pub version: i64,
    pub definition: Option<JobDefinition>,
}

/// The chain and its steps, from a chain run's snapshot.
pub fn chain_snapshot(snapshot: &str) -> Option<(JobDefinition, Vec<SnapshotStep>)> {
    let value: serde_json::Value = serde_json::from_str(snapshot).ok()?;
    let chain = JobDefinition::from_value(value.get("definition")?).ok()?;
    let steps = value
        .get("steps")?
        .as_array()?
        .iter()
        .filter_map(|step| {
            Some(SnapshotStep {
                id: step.get("id")?.as_str()?.to_owned(),
                version: step.get("version")?.as_i64()?,
                definition: step
                    .get("definition")
                    .and_then(|definition| JobDefinition::from_value(definition).ok()),
            })
        })
        .collect();
    Some((chain, steps))
}

#[cfg(test)]
mod tests {
    use super::*;
    use jykre::automation::definition::{OutputSpec, SqlOutput};
    use jykre::automation::{ChainRunId, RunId, UtcMillis};
    use jykre::reports::ParameterSpec;
    use jykre_core::automation::RunOrigin;
    use jykre_core::{ConnectionId, ParameterKind};

    #[test]
    fn the_extra_statements_are_found_by_their_first_line() {
        let sql = "SELECT 1;\n\n  -- next\n  UPDATE t SET a = 1;\nDELETE FROM t";
        assert_eq!(
            extra_statements(sql, Some(SqlDialect::Postgres)),
            vec![4, 5]
        );
        assert_eq!(extra_statements("SELECT 1;", None), Vec::<usize>::new());
        assert_eq!(extra_statements("  ", None), Vec::<usize>::new());
        assert_eq!(
            statements_note(&[4, 5]).as_deref(),
            Some(
                "This job holds 3 statements, and a job runs exactly one. The extra statements start on lines 4, 5; remove them before running."
            )
        );
        assert_eq!(
            statements_note(&[2]).as_deref(),
            Some(
                "This job holds 2 statements, and a job runs exactly one. The extra statement starts on line 2; remove it before running."
            )
        );
        assert_eq!(statements_note(&[]), None);
    }

    #[test]
    fn production_asks_for_a_risk_or_stored_outputs_only() {
        assert_eq!(
            production_confirmation("Prod", false, Some("x".into()), true),
            None
        );
        assert_eq!(production_confirmation("Prod", true, None, false), None);
        let (title, description) =
            production_confirmation("Prod", true, Some("DELETE without WHERE.".into()), true)
                .unwrap();
        assert_eq!(title, "Run on Prod?");
        assert!(description.starts_with("DELETE without WHERE."));
        assert!(description.contains("store its outputs' values on this computer for 7 days"));
    }

    #[test]
    fn the_plan_lists_every_step_connection_risk_and_new_program() {
        let lines = vec![
            PlanLine {
                id: "lookup".into(),
                job: "Lookup".into(),
                version: 3,
                kind: JobKind::Sql,
                connection: Some("Billing".into()),
                production: true,
                risk: Some("UPDATE without WHERE changes every row of the table.".into()),
                new_program: false,
                changed_since: true,
            },
            PlanLine {
                id: "export".into(),
                job: "Export".into(),
                version: 1,
                kind: JobKind::Process,
                connection: None,
                production: false,
                risk: None,
                new_program: true,
                changed_since: false,
            },
        ];
        let text = plan_description(&lines);
        assert!(text.starts_with(
            "1. lookup · Lookup version 3 (SQL Job) · on Billing (production) · the job changed since this version\n   UPDATE without WHERE"
        ));
        assert!(text.contains(
            "2. export · Export version 1 (Program)\n   First run of this program version."
        ));
        assert!(text.contains("not sandboxed"));
        assert!(text.contains("7 days"));
        assert!(text.ends_with("Steps after one that fails do not run."));
    }

    fn sql_version(outputs: &[&str]) -> JobDefinition {
        let mut job = JobDefinition::new_sql("Lookup", Some(ConnectionId::new()));
        if let JobBody::Sql(sql) = &mut job.body {
            sql.sql = "SELECT a FROM t WHERE k = :key".into();
            sql.parameters = vec![ParameterSpec::new("key")];
            sql.outputs = outputs
                .iter()
                .map(|name| SqlOutput {
                    name: (*name).into(),
                    kind: ParameterKind::Text,
                    column: 1,
                })
                .collect();
        }
        job
    }

    fn program_version(protocol: Protocol) -> JobDefinition {
        let mut job = JobDefinition::new_process("Export");
        if let JobBody::Process(process) = &mut job.body {
            process.program = "node".into();
            process.protocol = protocol;
            if protocol == Protocol::Jjp1 {
                process.inputs = vec![ParameterSpec::new("address"), {
                    let mut format = ParameterSpec::new("format");
                    format.required = false;
                    format
                }];
                process.outputs = vec![OutputSpec {
                    name: "file".into(),
                    kind: ParameterKind::Text,
                }];
            }
        }
        job
    }

    #[test]
    fn a_step_input_offers_chain_inputs_earlier_outputs_and_a_constant() {
        let lookup = sql_version(&["address"]);
        let quiet = program_version(Protocol::None);
        let export = program_version(Protocol::Jjp1);
        let chain = ChainBody {
            inputs: vec![ParameterSpec::new("external_key")],
            steps: vec![
                ChainStep::new("lookup", lookup.id, 1),
                ChainStep::new("quiet", quiet.id, 1),
                ChainStep::new("export", export.id, 1),
            ],
            timeout_ms: 3_600_000,
        };
        let versions = vec![Some(lookup), Some(quiet.clone()), Some(export)];
        let labels: Vec<String> = source_choices(&chain, 2, &versions)
            .into_iter()
            .map(|choice| choice.label)
            .collect();
        assert_eq!(
            labels,
            vec![
                "Its default, or NULL",
                "Chain input external_key",
                "Output address of step lookup",
                "A constant"
            ]
        );
        // The first step reads no earlier step.
        assert_eq!(source_choices(&chain, 0, &versions).len(), 3);
        assert!(unreadable_step(&quiet));
        assert_eq!(new_step_id(&chain), "step_1");
    }

    fn output(name: &str, value: Option<&str>, expired: bool) -> RunOutput {
        RunOutput {
            position: 0,
            name: name.into(),
            kind: ParameterKind::Text,
            value: value.map(str::to_owned),
            is_null: value.is_none() && !expired,
            bytes: 0,
            value_expires_at: UtcMillis(0),
            expired,
        }
    }

    #[test]
    fn a_step_run_again_is_prefilled_only_from_stored_values() {
        let export = program_version(Protocol::Jjp1);
        let mut step = ChainStep::new("export", export.id, 1);
        step.inputs = vec![
            (
                "address".into(),
                Source::Step {
                    step: "lookup".into(),
                    output: "address".into(),
                },
            ),
            ("format".into(), Source::Value("csv".into())),
        ];
        let mut stored = HashMap::new();
        stored.insert(
            "lookup".to_owned(),
            vec![output("address", Some("Rua 1"), false)],
        );
        let (values, missing) = prefill(&step, &export, &stored);
        assert_eq!(values.get("address").map(String::as_str), Some("Rua 1"));
        assert_eq!(values.get("format").map(String::as_str), Some("csv"));
        assert!(missing.is_empty());
        // Expired or not stored: left to fill in.
        stored.insert("lookup".to_owned(), vec![output("address", None, true)]);
        let (values, missing) = prefill(&step, &export, &stored);
        assert!(!values.contains_key("address"));
        assert_eq!(missing, vec!["address".to_owned()]);
        let (_, missing) = prefill(&step, &export, &HashMap::new());
        assert_eq!(missing, vec!["address".to_owned()]);
    }

    fn step_run(index: i64, state: RunState, cause: Option<Cause>) -> RunRecord {
        RunRecord {
            id: RunId::new(),
            kind: JobKind::Sql,
            job_id: None,
            job_revision: None,
            job_version: Some(1),
            chain_run_id: None,
            step_index: Some(index),
            origin: RunOrigin::Manual,
            state,
            cause,
            effect_unknown: false,
            reads_only: true,
            fingerprint: String::new(),
            connection_id: None,
            connection_revision: None,
            rows_delivered: None,
            rows_discarded: None,
            rows_affected: None,
            truncated: false,
            server_code: None,
            exit_code: None,
            exit_signal: None,
            process_group: None,
            process_started: None,
            program_fingerprint: None,
            failure_code: None,
            log_bytes_stdout: None,
            log_bytes_stderr: None,
            log_bytes_jjp: None,
            outputs_stored: None,
            started_at: UtcMillis(0),
            finished_at: Some(UtcMillis(10)),
        }
    }

    #[test]
    fn steps_after_a_failure_read_not_run_with_the_step_and_cause() {
        let chain = ChainRunRecord {
            id: ChainRunId::new(),
            job_id: None,
            job_revision: None,
            origin: RunOrigin::Manual,
            state: RunState::Failed,
            cause: Some(Cause::Output),
            failed_step: Some("lookup".into()),
            effect_unknown: false,
            fingerprint: String::new(),
            started_at: UtcMillis(0),
            finished_at: Some(UtcMillis(20)),
        };
        let steps = vec![
            ("lookup".to_owned(), None),
            ("export".to_owned(), None),
            ("notify".to_owned(), None),
        ];
        let runs = vec![step_run(0, RunState::Failed, Some(Cause::Output))];
        let lines = chain_steps(&chain, &steps, &runs);
        assert_eq!(lines[0].text, "Failed");
        assert!(lines[0].run.is_some());
        assert_eq!(
            lines[1].text,
            "Not run: the chain stopped at step lookup. Its outputs did not match what the job declares."
        );
        assert!(lines[2].run.is_none());
        let mut changed = chain.clone();
        changed.cause = Some(Cause::PlanChanged);
        assert_eq!(
            plan_changed(&changed, |step| (step == "lookup").then(|| "Billing".to_owned())),
            Some(
                "The settings of Billing changed after the plan was confirmed, so step lookup did not start."
                    .to_owned()
            )
        );
        assert_eq!(plan_changed(&chain, |_| None), None);
    }

    #[test]
    fn snapshots_give_back_their_definitions() {
        let job = sql_version(&[]);
        let snapshot = serde_json::json!({"kind": "sql", "definition": job.to_value()}).to_string();
        assert_eq!(snapshot_definition(&snapshot), Some(job.clone()));
        let chain = JobDefinition::new_chain("Nightly");
        let snapshot = serde_json::json!({
            "kind": "chain",
            "definition": chain.to_value(),
            "steps": [{"id": "lookup", "version": 2, "definition": job.to_value()}],
        })
        .to_string();
        let (read, steps) = chain_snapshot(&snapshot).unwrap();
        assert_eq!(read, chain);
        assert_eq!(steps[0].id, "lookup");
        assert_eq!(steps[0].version, 2);
        assert_eq!(steps[0].definition, Some(job));
        assert!(snapshot_definition("not json").is_none());
    }
}
