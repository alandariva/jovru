//! The owner of one SQL run (specification, sections 4.6, 4.8, 5.5 and
//! 6.3): durable admission, the statement on its own session, arbitration
//! between completion, Stop and the deadline, the declared outputs, and the
//! terminal write before anything is published.
use super::definition::JobBody;
use super::engine::{Ended, Engine, RunSpec, SETTLE, SqlWork, Work};
use super::executor::{self, Execution};
use super::outputs::{self, Output};
use super::{
    Cause, Phase, Progress, Refusal, RunOutcome, RunState, RunStatus, SqlOutcome, UtcMillis,
    snapshot,
};
use jykre_core::CancellationToken;
use jykre_core::automation::{JobKind, RunOrigin};
use jykre_storage::{AutomationRepository, NewRun, OutputValue, RunFinish};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Instant, SystemTime};
use tokio::sync::watch;

/// What came first, in the owner's eyes.
enum Decision {
    Stopped,
    Deadline,
}

pub(super) async fn run(
    engine: &Engine,
    repository: &AutomationRepository,
    spec: RunSpec,
    stop: &CancellationToken,
    status: &watch::Sender<RunStatus>,
) -> Ended {
    let RunSpec {
        id,
        definition,
        job: job_id,
        revision,
        version,
        chain,
        work,
    } = spec;
    let Work::Sql(work) = work else {
        return Ended::Refused(Refusal::Missing);
    };
    let SqlWork {
        job,
        prepared,
        connection,
    } = *work;
    let JobBody::Sql(body) = &definition.body else {
        return Ended::Refused(Refusal::Missing);
    };
    let started_at = SystemTime::now();
    let snapshot = snapshot::sql(
        &definition,
        connection.id,
        connection.revision,
        u64::try_from(job.timeout.as_millis()).unwrap_or(u64::MAX),
        job.max_rows,
        prepared.reads_only(),
    );
    // Durable admission, before any network I/O (section 6.3).
    let admitted = repository
        .admit_run(&NewRun {
            id,
            kind: JobKind::Sql,
            job_id,
            job_revision: revision,
            job_version: version,
            chain_run_id: chain.map(|(chain, _)| chain),
            step_index: chain.and_then(|(_, step)| i64::try_from(step).ok()),
            origin: RunOrigin::Manual,
            reads_only: prepared.reads_only(),
            snapshot_json: snapshot,
            connection_id: Some(connection.id),
            connection_revision: Some(connection.revision),
            program_fingerprint: None,
            started_at: UtcMillis::from(started_at),
        })
        .await;
    if let Err(error) = admitted {
        tracing::warn!(run = %id, error_code = ?error.code, "automation run not admitted: history unavailable");
        return Ended::Refused(Refusal::History);
    }
    if stop.is_cancelled() {
        // Stopped before anything was sent: nothing can have taken effect.
        return finish(
            repository,
            id,
            Ending {
                outcome: Outcome::new(RunState::Cancelled, None, false),
                done: Execution::default(),
                reads_only: prepared.reads_only(),
                produced: None,
                persists: definition.persists_outputs(),
                started_at,
                started: Instant::now(),
            },
        )
        .await;
    }
    let started = Instant::now();
    let deadline = started + job.timeout;
    let work = CancellationToken::new();
    let confirmed = AtomicBool::new(false);
    let execution = executor::execute(
        connection.driver.clone(),
        connection.resolved.clone(),
        &prepared,
        engine.inner.memory_ceiling,
        work.clone(),
        deadline,
        status,
        &confirmed,
    );
    tokio::pin!(execution);
    // The first event observed decides; the others are ignored. A completion
    // already confirmed wins over a Stop or a deadline that became ready in
    // the same instant, since it is what happened.
    let decided = tokio::select! { biased;
        done = &mut execution => Err(done),
        () = stop.cancelled() => Ok(Decision::Stopped),
        () = tokio::time::sleep_until(deadline.into()) => Ok(Decision::Deadline),
    };
    let reads_only = prepared.reads_only();
    let completed = |done: Execution| {
        let cause = done.error.as_ref().map(|error| Cause::of(error.code));
        let state = if done.confirmed {
            RunState::Succeeded
        } else {
            RunState::Failed
        };
        let effect_unknown = state == RunState::Failed
            && done.sent
            && !reads_only
            && cause.is_none_or(Cause::leaves_effect_unknown);
        let cause = (state == RunState::Failed).then(|| cause.unwrap_or(Cause::Internal));
        (Outcome::new(state, cause, effect_unknown), done)
    };
    // The statement's end was confirmed before Stop or the deadline was
    // observed, while the session was closing: completion came first.
    let decided = match decided {
        Ok(_) if confirmed.load(Ordering::SeqCst) => {
            match tokio::time::timeout(SETTLE, &mut execution).await {
                Ok(done) => Err(done),
                Err(_) => decided,
            }
        }
        other => other,
    };
    let (outcome, done) = match decided {
        Err(done) => completed(done),
        Ok(decision) => {
            let sent_before = matches!(
                &*status.borrow(),
                RunStatus::Active(Progress {
                    phase: Phase::Executing | Phase::Completing,
                    ..
                })
            );
            status.send_replace(RunStatus::Active(Progress::new(Phase::Stopping)));
            work.cancel();
            let settled = tokio::time::timeout(SETTLE, &mut execution).await;
            if settled.is_err() {
                tracing::warn!(run = %id, "automation run did not settle in time; its session was dropped");
            }
            let done = settled.ok();
            // Once sent, a write's effect is unknown: the run never claims
            // it finished after Stop or the deadline.
            let sent = done.as_ref().map_or(sent_before, |done| done.sent);
            let (state, cause) = match decision {
                Decision::Stopped => (RunState::Cancelled, None),
                Decision::Deadline => (RunState::Failed, Some(Cause::Timeout)),
            };
            let mut done = done.unwrap_or_default();
            // The driver's error only says it was interrupted, which the
            // state already says.
            done.error = None;
            (Outcome::new(state, cause, sent && !reads_only), done)
        }
    };
    // Declared outputs, from what the result kept (section 9.3.2).
    let mut outcome = outcome;
    let produced = if outcome.state == RunState::Succeeded && !body.outputs.is_empty() {
        let seen = done.data.row_count() as u64
            + done.rows_discarded
            + u64::from(done.truncated && reads_only);
        let taken = outputs::from_result(&body.outputs, body.row, &done.data, seen);
        if taken.is_err() {
            outcome.state = RunState::Failed;
            outcome.cause = Some(Cause::Output);
        }
        Some(taken)
    } else {
        None
    };
    finish(
        repository,
        id,
        Ending {
            outcome,
            done,
            reads_only,
            produced,
            persists: definition.persists_outputs(),
            started_at,
            started,
        },
    )
    .await
}

struct Outcome {
    state: RunState,
    cause: Option<Cause>,
    effect_unknown: bool,
}

impl Outcome {
    fn new(state: RunState, cause: Option<Cause>, effect_unknown: bool) -> Self {
        Self {
            state,
            cause,
            effect_unknown,
        }
    }
}

/// How the run ended, before it is recorded.
struct Ending {
    outcome: Outcome,
    done: Execution,
    reads_only: bool,
    produced: Option<Result<Vec<Output>, outputs::OutputProblem>>,
    persists: bool,
    started_at: SystemTime,
    started: Instant,
}

/// Writes the terminal state, then builds what is published.
async fn finish(repository: &AutomationRepository, id: super::RunId, ending: Ending) -> Ended {
    let Ending {
        outcome,
        done,
        reads_only,
        produced,
        persists,
        started_at,
        started,
    } = ending;
    let server_code = done
        .error
        .as_ref()
        .and_then(|error| error.server.as_ref())
        .and_then(|server| server.sqlstate.clone().or(server.vendor_code.clone()));
    let stored_outputs = match (&produced, persists) {
        (Some(Ok(values)), true) => Some(
            values
                .iter()
                .map(|output| OutputValue {
                    name: output.name.clone(),
                    kind: output.kind,
                    value: output.value.clone(),
                })
                .collect(),
        ),
        _ => None,
    };
    let quota_fits = stored_outputs.as_ref().map(|values: &Vec<OutputValue>| {
        values
            .iter()
            .map(|value| value.value.as_deref().map_or(0, str::len))
            .sum::<usize>()
            <= jykre_storage::OUTPUT_QUOTA_BYTES
    });
    let finished_at = UtcMillis::now();
    let written = repository
        .finish_run(
            id,
            &RunFinish {
                state: outcome.state,
                cause: outcome.cause,
                effect_unknown: outcome.effect_unknown,
                rows_delivered: i64::try_from(done.data.row_count()).ok(),
                rows_discarded: i64::try_from(done.rows_discarded).ok(),
                rows_affected: done.rows_affected.and_then(|rows| i64::try_from(rows).ok()),
                truncated: done.truncated,
                server_code: server_code.clone(),
                outputs: stored_outputs,
                finished_at,
                ..RunFinish::default()
            },
        )
        .await;
    let history_failed = match written {
        Ok(()) => false,
        Err(error) => {
            // The run is never sent again; recovery marks it interrupted.
            tracing::error!(run = %id, error_code = ?error.code, "automation run's end not recorded");
            true
        }
    };
    tracing::info!(
        run = %id,
        state = %outcome.state,
        cause = ?outcome.cause.map(|cause| cause.as_str()),
        effect_unknown = outcome.effect_unknown,
        truncated = done.truncated,
        rows = done.data.row_count(),
        discarded = done.rows_discarded,
        elapsed_ms = started.elapsed().as_millis() as u64,
        "automation run finished"
    );
    Ended::Finished(Arc::new(RunOutcome {
        id,
        kind: JobKind::Sql,
        state: outcome.state,
        cause: outcome.cause,
        error: done.error,
        effect_unknown: outcome.effect_unknown,
        sql: Some(SqlOutcome {
            reads_only,
            truncated: done.truncated,
            rows_discarded: done.rows_discarded,
            rows_affected: done.rows_affected,
            data: done.data,
            transaction_rolled_back: done.transaction_rolled_back,
            completion: done.completion,
            server_code,
        }),
        program: None,
        outputs: produced,
        outputs_stored: if persists { quota_fits } else { None },
        history_failed,
        started_at,
        elapsed: started.elapsed(),
    }))
}
