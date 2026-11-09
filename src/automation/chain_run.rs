//! The owner of one chain run (specification, section 9.3.5; ADR-0036): it
//! admits each step as an ordinary run, waits for its end, maps its outputs
//! to the next step's inputs, and stops at the first step that does not
//! succeed. The chain holds no admission slot of its own; a step releases
//! its slot before the next one asks for one, so a chain progresses even
//! with a global limit of 1.
use super::chain::{InputProblem, step_inputs};
use super::definition::{ChainStep, JobBody};
use super::engine::{
    ChainRequest, ConnectionInput, Ended, Engine, PlanStep, RunSpec, SqlWork, StepRun, Work,
    chain_cause,
};
use super::executor;
use super::outputs::Output;
use super::snapshot::{self, PlannedStep};
use super::{
    Cause, ChainOutcome, ChainRunId, ChainStatus, Phase, Progress, Refusal, RunId, RunOutcome,
    RunState, RunStatus, SqlJob, UtcMillis,
};
use jykre_core::CancellationToken;
use jykre_core::automation::{RunOrigin, RunSchedule};
use jykre_storage::{ChainFinish, NewChainRun};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};
use tokio::sync::watch;

/// How the chain stopped, when it did not succeed.
struct Stopped {
    state: RunState,
    cause: Option<Cause>,
    step: Option<String>,
    input: Option<InputProblem>,
}

impl Stopped {
    fn failed(cause: Cause, step: &str) -> Self {
        Self {
            state: RunState::Failed,
            cause: Some(cause),
            step: Some(step.to_owned()),
            input: None,
        }
    }

    fn cancelled(step: Option<&str>) -> Self {
        Self {
            state: RunState::Cancelled,
            cause: None,
            step: step.map(str::to_owned),
            input: None,
        }
    }
}

/// A chain run the engine admitted to start.
pub(super) struct Started {
    pub id: ChainRunId,
    pub request: ChainRequest,
    /// The chain's form, checked.
    pub values: Vec<(String, Option<String>)>,
    /// The schedule that started it (ADR-0041); its steps carry it too.
    pub scheduled: Option<RunSchedule>,
}

pub(super) async fn run(
    engine: &Engine,
    started: Started,
    stop: &CancellationToken,
    status: &watch::Sender<ChainStatus>,
) -> ChainStatus {
    let Started {
        id,
        request,
        values,
        scheduled,
    } = started;
    let Some(repository) = engine.repository() else {
        return ChainStatus::Refused(Refusal::Unavailable);
    };
    let ChainRequest {
        plan,
        values: _,
        connections,
    } = request;
    let JobBody::Chain(body) = &plan.chain.body else {
        return ChainStatus::Refused(Refusal::Missing);
    };
    let planned: Vec<PlannedStep<'_>> = plan
        .steps
        .iter()
        .map(|step| PlannedStep {
            id: &step.id,
            definition: &step.definition,
            version: step.version,
            connection: match &step.definition.body {
                JobBody::Sql(sql) => sql.connection.and_then(|connection| {
                    connections
                        .get(&connection)
                        .map(|input| (connection, input.revision))
                }),
                _ => None,
            },
        })
        .collect();
    let started_at = SystemTime::now();
    let started = Instant::now();
    let admitted = repository
        .admit_chain_run(&NewChainRun {
            id,
            job_id: plan.stored.map(|(job, _)| job),
            job_revision: plan.stored.map(|(_, revision)| revision),
            origin: if scheduled.is_some() {
                RunOrigin::Schedule
            } else {
                RunOrigin::Manual
            },
            snapshot_json: snapshot::chain(&plan.chain, &planned),
            scheduled,
            started_at: UtcMillis::from(started_at),
        })
        .await;
    if let Err(error) = admitted {
        tracing::warn!(chain_run = %id, error_code = ?error.code, "automation chain not admitted: history unavailable");
        return ChainStatus::Refused(Refusal::History);
    }
    let deadline = started + Duration::from_millis(body.timeout_ms.max(1));
    let mut steps: Vec<Arc<RunOutcome>> = Vec::new();
    let mut earlier: HashMap<String, Vec<Output>> = HashMap::new();
    // A schedule opens no dialog: a connection whose password is not
    // remembered fails the chain before its first step (section 9.5.7).
    let mut stopped = scheduled.and_then(|_| {
        plan.steps
            .iter()
            .find_map(|step| match &step.definition.body {
                JobBody::Sql(sql) => sql
                    .connection
                    .and_then(|connection| connections.get(&connection))
                    .filter(|input| crate::connection_store::needs_password(&input.resolved))
                    .map(|_| Stopped::failed(Cause::Authentication, &step.id)),
                _ => None,
            })
    });
    for (index, (step, planned)) in body.steps.iter().zip(&plan.steps).enumerate() {
        if stopped.is_some() {
            break;
        }
        status.send_replace(ChainStatus::Running {
            step: index,
            run: None,
        });
        if stop.is_cancelled() {
            stopped = Some(Stopped::cancelled(None));
            break;
        }
        if Instant::now() >= deadline {
            stopped = Some(Stopped::failed(Cause::Timeout, &step.id));
            break;
        }
        let context = Context {
            engine,
            chain: id,
            scheduled,
            chain_values: &values,
            connections: &connections,
            stop,
            deadline,
            status,
        };
        match run_step(&context, index, step, planned, &earlier).await {
            Err(halt) => {
                stopped = Some(halt);
                break;
            }
            Ok(outcome) => {
                steps.push(outcome.clone());
                if outcome.state != RunState::Succeeded {
                    stopped = Some(if outcome.state == RunState::Cancelled {
                        // Stop came first unless the chain's own deadline
                        // stopped the step.
                        if Instant::now() >= deadline && !stop.is_cancelled() {
                            Stopped::failed(Cause::Timeout, &step.id)
                        } else {
                            Stopped::cancelled(Some(&step.id))
                        }
                    } else {
                        Stopped::failed(outcome.cause.unwrap_or(Cause::Internal), &step.id)
                    });
                    break;
                }
                earlier.insert(
                    step.id.clone(),
                    outcome
                        .output_values()
                        .map(<[Output]>::to_vec)
                        .unwrap_or_default(),
                );
            }
        }
    }
    let effect_unknown = steps.iter().any(|step| step.effect_unknown);
    let (state, cause, failed_step, input_problem) = match stopped {
        None => (RunState::Succeeded, None, None, None),
        Some(stopped) => (stopped.state, stopped.cause, stopped.step, stopped.input),
    };
    let written = repository
        .finish_chain_run(
            id,
            &ChainFinish {
                state,
                cause,
                failed_step: failed_step.clone(),
                effect_unknown,
                finished_at: UtcMillis::now(),
            },
        )
        .await;
    let history_failed = match written {
        Ok(()) => false,
        Err(error) => {
            tracing::error!(chain_run = %id, error_code = ?error.code, "automation chain's end not recorded");
            true
        }
    };
    tracing::info!(
        chain_run = %id,
        state = %state,
        cause = ?cause.map(|cause| cause.as_str()),
        steps = steps.len(),
        effect_unknown,
        elapsed_ms = started.elapsed().as_millis() as u64,
        "automation chain finished"
    );
    ChainStatus::Finished(Arc::new(ChainOutcome {
        id,
        state,
        cause,
        failed_step,
        effect_unknown,
        steps,
        step_ids: body.steps.iter().map(|step| step.id.clone()).collect(),
        input_problem,
        history_failed,
        started_at,
        elapsed: started.elapsed(),
    }))
}

/// What every step of one chain run shares.
struct Context<'a> {
    engine: &'a Engine,
    chain: ChainRunId,
    scheduled: Option<RunSchedule>,
    chain_values: &'a [(String, Option<String>)],
    connections: &'a HashMap<jykre_core::ConnectionId, ConnectionInput>,
    stop: &'a CancellationToken,
    deadline: Instant,
    status: &'a watch::Sender<ChainStatus>,
}

/// Prepares, admits and runs one step. `Err` when the step stopped the
/// chain without a run of its own (its inputs, a changed plan, the queue).
async fn run_step(
    context: &Context<'_>,
    index: usize,
    step: &ChainStep,
    planned: &PlanStep,
    earlier: &HashMap<String, Vec<Output>>,
) -> Result<Arc<RunOutcome>, Stopped> {
    let Context {
        engine,
        chain,
        scheduled,
        chain_values,
        connections,
        stop,
        deadline,
        status,
    } = *context;
    let definition = &planned.definition;
    let inputs =
        step_inputs(step, definition, chain_values, earlier).map_err(|problem| Stopped {
            state: RunState::Failed,
            cause: Some(Cause::Input),
            step: Some(step.id.clone()),
            input: Some(problem),
        })?;
    let (work, connection_id) = match &definition.body {
        JobBody::Sql(sql) => {
            let connection_id = sql
                .connection
                .ok_or_else(|| Stopped::failed(Cause::Connection, &step.id))?;
            let connection = connections
                .get(&connection_id)
                .ok_or_else(|| Stopped::failed(Cause::Connection, &step.id))?;
            // Exactly the confirmed plan runs (section 9.3.4).
            if engine.connection_revision(connection_id) != Some(connection.revision) {
                return Err(Stopped::failed(Cause::PlanChanged, &step.id));
            }
            let job = SqlJob::from_body(sql, connection_id);
            let prepared =
                executor::prepare(&job, connection.driver.descriptor().dialect, &inputs.typed)
                    .map_err(|_| Stopped::failed(Cause::Input, &step.id))?;
            (
                Work::Sql(Box::new(SqlWork {
                    job,
                    prepared,
                    connection: connection.clone(),
                })),
                Some(connection_id),
            )
        }
        JobBody::Process(_) => (
            Work::Program {
                values: inputs.values.clone(),
            },
            None,
        ),
        JobBody::Chain(_) => return Err(Stopped::failed(Cause::Validation, &step.id)),
    };
    let run = RunId::new();
    let step_stop = stop.child_token();
    let (sender, _keep) = watch::channel(RunStatus::Active(Progress::new(Phase::Admitted)));
    let ticket = engine
        .submit_step(StepRun {
            id: run,
            name: planned.job_name.clone(),
            kind: definition.kind(),
            job: Some(planned.job),
            chain: (chain, index),
            connection: connection_id,
            stop: step_stop.clone(),
            sender: sender.clone(),
        })
        .map_err(|refusal| halted(&refusal, false, stop, &step.id))?;
    status.send_replace(ChainStatus::Running {
        step: index,
        run: Some(run),
    });
    let mut timed_out = false;
    let waiting = engine.wait(ticket, &step_stop, &sender);
    tokio::pin!(waiting);
    let slot = tokio::select! { biased;
        slot = &mut waiting => slot,
        () = tokio::time::sleep_until(deadline.into()) => {
            timed_out = !stop.is_cancelled();
            step_stop.cancel();
            waiting.await
        }
    };
    let slot = match slot {
        Ok(slot) => slot,
        Err(refusal) => {
            sender.send_replace(RunStatus::Refused(refusal.clone()));
            engine.forget_step(run);
            return Err(halted(&refusal, timed_out, stop, &step.id));
        }
    };
    // Exactly the confirmed plan runs: a connection changed while the step
    // waited does not start it (section 9.3.4).
    if let (Some(connection), Work::Sql(work)) = (connection_id, &work)
        && engine.connection_revision(connection) != Some(work.connection.revision)
    {
        drop(slot);
        sender.send_replace(RunStatus::Refused(Refusal::Missing));
        engine.forget_step(run);
        return Err(Stopped::failed(Cause::PlanChanged, &step.id));
    }
    let spec = RunSpec {
        id: run,
        definition: definition.clone(),
        job: Some(planned.job),
        revision: None,
        version: Some(planned.version),
        chain: Some((chain, index)),
        scheduled,
        work,
    };
    let execution = engine.execute(spec, &step_stop, &sender);
    tokio::pin!(execution);
    let ended = tokio::select! { biased;
        ended = &mut execution => ended,
        () = tokio::time::sleep_until(deadline.into()), if !timed_out => {
            timed_out = !stop.is_cancelled();
            step_stop.cancel();
            execution.await
        }
    };
    // The step's session or program is closed and its slot given back
    // before the next step asks for one.
    drop(slot);
    engine.forget_step(run);
    match ended {
        Ended::Finished(outcome) => {
            sender.send_replace(RunStatus::Finished(outcome.clone()));
            if timed_out && outcome.state == RunState::Cancelled {
                return Err(Stopped::failed(Cause::Timeout, &step.id));
            }
            Ok(outcome)
        }
        Ended::Refused(refusal) => {
            sender.send_replace(RunStatus::Refused(refusal.clone()));
            Err(halted(&refusal, timed_out, stop, &step.id))
        }
    }
}

/// How a step that never ran stops its chain: Stop or quitting cancel it,
/// the chain's deadline fails it with `Timeout`, anything else with the
/// refusal's cause.
fn halted(refusal: &Refusal, timed_out: bool, stop: &CancellationToken, step: &str) -> Stopped {
    if timed_out {
        Stopped::failed(Cause::Timeout, step)
    } else if stop.is_cancelled() || matches!(refusal, Refusal::Withdrawn | Refusal::ShuttingDown) {
        Stopped::cancelled(Some(step))
    } else {
        Stopped::failed(chain_cause(refusal), step)
    }
}
