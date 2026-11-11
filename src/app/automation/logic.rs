//! What the Automation views decide without a window: where a job's extra
//! statements start, which confirmations a run needs, what one plan dialog
//! lists, which sources a chain step's input may take, what a step run
//! again on its own is prefilled with, and how a chain run's steps read in
//! its history. Tested here; the views only call these.
use super::words;
use crate::app::ui::{Feedback, Tone};
use jykre::automation::JobId;
use jykre::automation::definition::{
    ChainBody, ChainStep, JobBody, JobDefinition, Protocol, Source,
};
use jykre::automation::{Availability, Cause, JobKind, RunState};
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

/// What the SQL Job's connection control shows, and what is wrong with the
/// choice, said under the control before a run refuses it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConnectionChoice {
    pub label: String,
    pub problem: Option<String>,
}

/// The connection control of a SQL Job. `chosen` is the job's connection
/// when it still exists (its name, and whether it can run now), `named`
/// whether the job names one at all, `none_saved` whether there is no
/// connection to choose.
pub fn connection_choice(
    chosen: Option<(&str, bool)>,
    named: bool,
    none_saved: bool,
) -> ConnectionChoice {
    match chosen {
        Some((name, runs)) => ConnectionChoice {
            label: name.to_owned(),
            problem: (!runs).then(|| tr!("The connection {0} cannot run now.", name)),
        },
        None if named => ConnectionChoice {
            label: tr!("A connection that no longer exists").to_owned(),
            problem: Some(
                tr!("The job's connection no longer exists. Choose another one here.").to_owned(),
            ),
        },
        None => ConnectionChoice {
            label: tr!("Choose a connection").to_owned(),
            problem: Some(if none_saved {
                tr!("There is no connection yet. Add one in the explorer, then choose it here.")
                    .to_owned()
            } else {
                tr!("Choose the connection the job runs on.").to_owned()
            }),
        },
    }
}

/// The item of `items` when it is the only one: a new SQL Job takes the
/// only saved connection, and none when there are several to choose from.
pub fn only<T>(items: &mut impl Iterator<Item = T>) -> Option<T> {
    let first = items.next()?;
    items.next().is_none().then_some(first)
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
        label: source_label(None),
        source: None,
    }];
    let mut offer = |kind: SourceKind| {
        choices.push(SourceChoice {
            label: source_label(Some(&kind)),
            source: Some(kind),
        });
    };
    for input in &chain.inputs {
        offer(SourceKind::Input(input.name.clone()));
    }
    for (earlier, step) in chain.steps.iter().enumerate().take(index) {
        let Some(Some(definition)) = versions.get(earlier) else {
            continue;
        };
        for output in definition.outputs() {
            offer(SourceKind::Step {
                step: step.id.clone(),
                output: output.name.clone(),
            });
        }
    }
    offer(SourceKind::Constant);
    choices
}

/// How a source reads in a step input's list; `None` is no source.
fn source_label(source: Option<&SourceKind>) -> String {
    match source {
        None => tr!("Its default, or NULL").to_owned(),
        Some(SourceKind::Input(name)) => tr!("Chain input {0}", name),
        Some(SourceKind::Step { step, output }) => tr!("Output {0} of {1}", output, step),
        Some(SourceKind::Constant) => tr!("A constant").to_owned(),
    }
}

/// What a step input's button shows: the source the chain's definition
/// holds, also when the list no longer offers it (an earlier step renamed,
/// removed or moved after it, or its version no longer declaring that
/// output). The step still reads that source, and its problem says so, so
/// the button never reads as the default.
pub fn shown_source(current: Option<&SourceKind>, choices: &[SourceChoice]) -> String {
    match current {
        Some(kind)
            if !choices
                .iter()
                .any(|choice| choice.source.as_ref() == Some(kind)) =>
        {
            tr!("{0} (not available)", source_label(Some(kind)))
        }
        current => source_label(current),
    }
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
    let not_run = |step: &str| match (chain.state, chain.cause) {
        (RunState::Failed, Some(cause)) => tr!(
            "Not run: the chain stopped at {0}. {1}",
            step,
            words::cause_sentence(cause, Some(JobKind::Chain))
        ),
        (RunState::Cancelled, _) => tr!("Not run: the chain was stopped at {0}.", step),
        (RunState::Interrupted, _) => {
            tr!("Not run: Jykre Studio closed while {0} ran.", step)
        }
        _ => tr!("Not run: the chain stopped at {0}.", step),
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
                    Some(step) => not_run(step),
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
        "The settings of {0} changed after the plan was confirmed, so {1} did not start.",
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

/// Whether a job's **Run and result** shows the result grid: only a finished run's
/// result. A run that is queued or running has none yet, and the previous
/// run's would read as its own; the status above says **Queued** or what
/// it is doing (section 8.1.1).
pub fn shows_result(has_result: bool, running: bool) -> bool {
    has_result && !running
}

/// A section of a job's editor that folds to its title and one line saying
/// what it holds, so the form opens on what a job needs first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Section {
    Limits,
    Outputs,
    Versions,
    /// A program's entrypoint and working folder.
    Files,
    Environment,
    Protocol,
    /// A program's logs, its stdin and how Stop ends it.
    Running,
}

/// The sections a job's editor opens unfolded: those that hold something
/// the job sets, so nothing chosen is out of sight.
pub fn open_sections(definition: &JobDefinition) -> Vec<Section> {
    let mut open = Vec::new();
    if !definition.outputs().is_empty() || definition.persists_outputs() {
        open.push(Section::Outputs);
    }
    if let JobBody::Process(process) = &definition.body {
        if process.entrypoint.is_some() || process.working_directory.is_some() {
            open.push(Section::Files);
        }
        if !process.environment.unset.is_empty() || !process.environment.set.is_empty() {
            open.push(Section::Environment);
        }
        if process.protocol == Protocol::Jjp1 {
            open.push(Section::Protocol);
        }
    }
    open
}

/// What the folded **Limits** says: the timeout and, for a SQL Job, the
/// rows a run keeps.
pub fn limits_summary(timeout_ms: u64, rows: Option<u32>) -> String {
    let timeout = jykre::cell_format::duration(timeout_ms);
    match rows {
        Some(1) => tr!("Timeout {0} · at most 1 row", timeout),
        Some(rows) => tr!("Timeout {0} · at most {1} rows", timeout, rows),
        None => tr!("Timeout {0}", timeout),
    }
}

/// What the folded **Declared outputs** says.
pub fn outputs_summary(count: usize, stored: bool) -> String {
    match (count, stored) {
        (0, _) => tr!("None").to_owned(),
        (1, false) => tr!("1 output").to_owned(),
        (1, true) => tr!("1 output, stored").to_owned(),
        (count, false) => tr!("{0} outputs", count),
        (count, true) => tr!("{0} outputs, stored", count),
    }
}

/// What the folded **Published versions** says.
pub fn versions_summary(latest: Option<i64>) -> String {
    match latest {
        Some(version) => tr!("Latest: version {0}", version),
        None => tr!("Not published").to_owned(),
    }
}

/// What the folded entrypoint and working folder say.
pub fn files_summary(entrypoint: &str, folder: &str) -> String {
    match (entrypoint.trim(), folder.trim()) {
        ("", "") => tr!("The program itself, in its own folder").to_owned(),
        (entrypoint, "") => entrypoint.to_owned(),
        ("", folder) => tr!("In {0}", folder),
        (entrypoint, folder) => tr!("{0}, in {1}", entrypoint, folder),
    }
}

/// What the folded **Environment** says.
pub fn environment_summary(unset: usize, set: usize) -> String {
    if unset == 0 && set == 0 {
        tr!("Jykre Studio's, minus its own variables").to_owned()
    } else {
        tr!("{0} removed, {1} set", unset, set)
    }
}

/// What the folded **Protocol** says.
pub fn protocol_summary(protocol: Protocol) -> &'static str {
    match protocol {
        Protocol::None => tr!("None: fixed arguments, no inputs or outputs"),
        Protocol::Jjp1 => tr!("jjp/1: inputs and outputs"),
    }
}

/// The stream of a program's output shown first: what it printed to
/// stderr when the run did not succeed, or when stdout is empty; otherwise
/// stdout.
pub fn first_stream(failed: bool, stdout: bool, stderr: bool) -> super::program_bits::Stream {
    use super::program_bits::Stream;
    if stderr && (failed || !stdout) {
        Stream::Stderr
    } else {
        Stream::Stdout
    }
}

/// The notice **Try again** leaves while another window holds the lock.
pub fn still_held() -> Feedback {
    Feedback::new(Tone::Info, tr!("Another window still holds automation."))
}

/// The automation notice once automation is `state`: **Try again** that
/// found the lock still held (`retried`) says so, and that notice goes as
/// soon as the lock is taken or automation is off. Any other notice (what
/// recovery found) stays until it is closed.
pub fn notice_after(
    notice: Option<Feedback>,
    state: &Availability,
    retried: bool,
) -> Option<Feedback> {
    match state {
        Availability::ReadOnly if retried => Some(still_held()),
        Availability::ReadOnly => notice,
        _ => notice.filter(|notice| *notice != still_held()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jykre::automation::OffReason;
    use jykre::automation::definition::{OutputSpec, SqlOutput};
    use jykre::automation::{ChainRunId, RunId, UtcMillis};
    use jykre::reports::ParameterSpec;
    use jykre_core::automation::RunOrigin;
    use jykre_core::{ConnectionId, ParameterKind};

    #[test]
    fn the_extra_statements_are_found_by_their_first_line() {
        crate::app::source_text();
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
    fn the_connection_control_says_what_is_chosen_and_what_is_missing() {
        crate::app::source_text();
        let chosen = connection_choice(Some(("Sales", true)), true, false);
        assert_eq!(chosen.label, "Sales");
        assert_eq!(chosen.problem, None);
        let demo = connection_choice(Some(("Demo", false)), true, false);
        assert_eq!(
            demo.problem.as_deref(),
            Some("The connection Demo cannot run now.")
        );
        let deleted = connection_choice(None, true, false);
        assert_eq!(deleted.label, "A connection that no longer exists");
        assert!(
            deleted
                .problem
                .is_some_and(|text| text.contains("Choose another"))
        );
        let unset = connection_choice(None, false, false);
        assert_eq!(unset.label, "Choose a connection");
        assert_eq!(
            unset.problem.as_deref(),
            Some("Choose the connection the job runs on.")
        );
        let none = connection_choice(None, false, true);
        assert!(none.problem.is_some_and(|text| text.contains("explorer")));
    }

    #[test]
    fn a_new_job_takes_a_connection_only_when_there_is_one() {
        assert_eq!(only(&mut [7].into_iter()), Some(7));
        assert_eq!(only(&mut [7, 8].into_iter()), None);
        assert_eq!(only(&mut std::iter::empty::<u8>()), None);
    }

    #[test]
    fn production_asks_for_a_risk_or_stored_outputs_only() {
        crate::app::source_text();
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
        crate::app::source_text();
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
        crate::app::source_text();
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
                "Output address of lookup",
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
        crate::app::source_text();
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
            schedule_id: None,
            due_at: None,
            catch_up: false,
            started_at: UtcMillis(0),
            finished_at: Some(UtcMillis(10)),
        }
    }

    #[test]
    fn steps_after_a_failure_read_not_run_with_the_step_and_cause() {
        crate::app::source_text();
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
            schedule_id: None,
            due_at: None,
            catch_up: false,
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
            "Not run: the chain stopped at lookup. Its outputs did not match what the job declares."
        );
        assert!(lines[2].run.is_none());
        let mut changed = chain.clone();
        changed.cause = Some(Cause::PlanChanged);
        assert_eq!(
            plan_changed(&changed, |step| (step == "lookup").then(|| "Billing".to_owned())),
            Some(
                "The settings of Billing changed after the plan was confirmed, so lookup did not start."
                    .to_owned()
            )
        );
        assert_eq!(plan_changed(&chain, |_| None), None);
        // A stopped or interrupted chain names the step once, in a sentence.
        let steps = vec![("step_1".to_owned(), None), ("step_2".to_owned(), None)];
        let runs = vec![step_run(0, RunState::Cancelled, None)];
        let mut stopped = chain.clone();
        stopped.state = RunState::Cancelled;
        stopped.cause = None;
        stopped.failed_step = Some("step_1".into());
        assert_eq!(
            chain_steps(&stopped, &steps, &runs)[1].text,
            "Not run: the chain was stopped at step_1."
        );
        stopped.state = RunState::Interrupted;
        assert_eq!(
            chain_steps(&stopped, &steps, &runs)[1].text,
            "Not run: Jykre Studio closed while step_1 ran."
        );
    }

    #[test]
    fn a_step_input_shows_the_source_its_definition_holds() {
        crate::app::source_text();
        let choices = vec![
            SourceChoice {
                label: "Its default, or NULL".into(),
                source: None,
            },
            SourceChoice {
                label: "Output address of lookup".into(),
                source: Some(SourceKind::Step {
                    step: "lookup".into(),
                    output: "address".into(),
                }),
            },
        ];
        assert_eq!(shown_source(None, &choices), "Its default, or NULL");
        let offered = SourceKind::Step {
            step: "lookup".into(),
            output: "address".into(),
        };
        assert_eq!(
            shown_source(Some(&offered), &choices),
            "Output address of lookup"
        );
        // An output the version no longer declares is still what the step
        // reads: never shown as the default.
        let stale = SourceKind::Step {
            step: "lookup".into(),
            output: "zip".into(),
        };
        assert_eq!(
            shown_source(Some(&stale), &choices),
            "Output zip of lookup (not available)"
        );
        assert_eq!(
            shown_source(Some(&SourceKind::Input("gone".into())), &choices),
            "Chain input gone (not available)"
        );
    }

    #[test]
    fn snapshots_give_back_their_definitions() {
        crate::app::source_text();
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

    #[test]
    fn a_job_opens_unfolded_only_what_it_sets() {
        crate::app::source_text();
        let plain = program_version(Protocol::None);
        assert!(open_sections(&plain).is_empty());
        let mut set = program_version(Protocol::Jjp1);
        if let JobBody::Process(process) = &mut set.body {
            process.entrypoint = Some("/jobs/export.js".into());
            process.environment.unset.push("AWS_PROFILE".into());
        }
        // The jjp/1 version declares an output, so its outputs open too.
        assert_eq!(
            open_sections(&set),
            [
                Section::Outputs,
                Section::Files,
                Section::Environment,
                Section::Protocol
            ]
        );
        assert_eq!(open_sections(&sql_version(&["rows"])), [Section::Outputs]);
        assert!(open_sections(&sql_version(&[])).is_empty());
    }

    #[test]
    fn a_folded_section_says_what_it_holds() {
        crate::app::source_text();
        assert_eq!(
            limits_summary(300_000, Some(1000)),
            "Timeout 5 min 0 s · at most 1000 rows"
        );
        assert_eq!(limits_summary(60_000, None), "Timeout 1 min 0 s");
        assert_eq!(outputs_summary(0, false), "None");
        assert_eq!(outputs_summary(1, true), "1 output, stored");
        assert_eq!(outputs_summary(3, false), "3 outputs");
        assert_eq!(versions_summary(None), "Not published");
        assert_eq!(versions_summary(Some(4)), "Latest: version 4");
        assert_eq!(
            files_summary("", " "),
            "The program itself, in its own folder"
        );
        assert_eq!(files_summary("/jobs/a.js", ""), "/jobs/a.js");
        assert_eq!(files_summary("a.js", "/jobs"), "a.js, in /jobs");
        assert_eq!(
            environment_summary(0, 0),
            "Jykre Studio's, minus its own variables"
        );
        assert_eq!(environment_summary(1, 2), "1 removed, 2 set");
        assert!(protocol_summary(Protocol::None).starts_with("None"));
    }

    #[test]
    fn a_failed_program_shows_what_it_wrote_to_stderr_first() {
        use crate::app::automation::program_bits::Stream;
        assert_eq!(first_stream(false, true, true), Stream::Stdout);
        assert_eq!(first_stream(true, true, true), Stream::Stderr);
        assert_eq!(first_stream(false, false, true), Stream::Stderr);
        assert_eq!(first_stream(true, true, false), Stream::Stdout);
        assert_eq!(first_stream(false, false, false), Stream::Stdout);
    }

    #[test]
    fn a_waiting_run_never_shows_the_last_run_s_result() {
        crate::app::source_text();
        // Queued or running: the status says so; no result is shown.
        assert!(!shows_result(true, true));
        assert!(!shows_result(false, true));
        // Finished with a result, or without one.
        assert!(shows_result(true, false));
        assert!(!shows_result(false, false));
    }

    #[test]
    fn the_still_held_notice_goes_once_the_lock_is_taken() {
        crate::app::source_text();
        // Try again while another window holds the lock says so.
        let notice = notice_after(None, &Availability::ReadOnly, true);
        assert_eq!(notice, Some(still_held()));
        assert_eq!(still_held().text, "Another window still holds automation.");
        // Read-only again without Try again keeps it.
        let notice = notice_after(notice, &Availability::ReadOnly, false);
        assert_eq!(notice, Some(still_held()));
        // Taken: the notice goes, from the retry's answer or the watch.
        assert_eq!(
            notice_after(notice.clone(), &Availability::Ready, true),
            None
        );
        assert_eq!(
            notice_after(notice.clone(), &Availability::Ready, false),
            None
        );
        assert_eq!(
            notice_after(notice, &Availability::Off(OffReason::Unreadable), false),
            None
        );
        // What recovery found stays until it is closed.
        let recovery = Feedback::new(Tone::Info, "1 run was active".to_owned());
        assert_eq!(
            notice_after(Some(recovery.clone()), &Availability::Ready, false),
            Some(recovery.clone())
        );
        assert_eq!(
            notice_after(Some(recovery.clone()), &Availability::ReadOnly, false),
            Some(recovery)
        );
    }
}
