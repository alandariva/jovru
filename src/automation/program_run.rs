//! The owner of one external program's run (specification, sections 9.1.4,
//! 9.1.5 and 9.2): the version's file checked, durable admission, the
//! process group recorded as soon as it starts, the executor's verdict, the
//! declared outputs, and the terminal write before anything is published.
use super::definition::JobBody;
use super::engine::{Ended, Engine, RunSpec, Work};
use super::jjp::{NamedValues, Terminal};
use super::outputs;
use super::process::{self, Prepared, ProgramPhase, ProgramProgress, ProgramRequest};
use super::{
    Cause, Phase, ProgramOutcome, Progress, Refusal, RunOutcome, RunState, RunStatus, UtcMillis,
    snapshot,
};
use jykre_core::CancellationToken;
use jykre_core::automation::{JobKind, RunOrigin};
use jykre_storage::{AutomationRepository, NewRun, OutputValue, RunFinish};
use std::sync::Arc;
use std::time::{Instant, SystemTime};
use tokio::sync::{mpsc, watch};

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
        job,
        revision,
        version,
        chain,
        work,
    } = spec;
    let Work::Program { values } = work else {
        return Ended::Refused(Refusal::Missing);
    };
    let JobBody::Process(body) = &definition.body else {
        return Ended::Refused(Refusal::Missing);
    };
    status.send_replace(RunStatus::Active(Progress::new(Phase::Starting)));
    // The files as they are now; a published version's changed file is
    // refused before admission (Q-16).
    let checked = {
        let body = body.clone();
        tokio::task::spawn_blocking(move || process::prepare(&body, std::env::vars_os())).await
    };
    let prepared = match checked {
        Ok(Ok(prepared)) => prepared,
        Ok(Err(refused)) => return Ended::Refused(Refusal::Program(refused)),
        Err(_) => return Ended::Refused(Refusal::Missing),
    };
    let launch = match &prepared {
        Prepared::Ready(launch) => Some(launch),
        Prepared::CannotStart { .. } => None,
    };
    let started_at = SystemTime::now();
    let admitted = repository
        .admit_run(&NewRun {
            id,
            kind: JobKind::Process,
            job_id: job,
            job_revision: revision,
            job_version: version,
            chain_run_id: chain.map(|(chain, _)| chain),
            step_index: chain.and_then(|(_, step)| i64::try_from(step).ok()),
            origin: RunOrigin::Manual,
            reads_only: false,
            snapshot_json: snapshot::program(&definition, launch),
            connection_id: None,
            connection_revision: None,
            program_fingerprint: launch.and_then(|launch| launch.fingerprint.clone()),
            started_at: UtcMillis::from(started_at),
        })
        .await;
    if let Err(error) = admitted {
        tracing::warn!(run = %id, error_code = ?error.code, "automation run not admitted: history unavailable");
        return Ended::Refused(Refusal::History);
    }
    let program = launch.map(|launch| launch.program.clone());
    let started = Instant::now();
    let deadline = started + definition.timeout();

    let progress_status = status.clone();
    let progress: process::ProgressSink = Arc::new(move |progress: &ProgramProgress| {
        progress_status.send_replace(RunStatus::Active(Progress {
            phase: match progress.phase {
                ProgramPhase::Starting => Phase::Starting,
                ProgramPhase::Running => Phase::Running,
                ProgramPhase::Stopping => Phase::Stopping,
            },
            rows: 0,
            percent: progress
                .percent
                .map(|percent| percent.clamp(0.0, 100.0).round() as u8),
            stage: progress.stage.clone(),
        }));
    });
    // The group is recorded while the program runs, so recovery finds it
    // after a crash (section 9.1.6).
    let (started_sender, mut started_receiver) = mpsc::channel(1);
    let on_started: process::StartedSink = Arc::new(move |group, leader_started| {
        drop(started_sender.try_send((group, leader_started)));
    });
    let execution = process::run(ProgramRequest {
        run: id,
        prepared,
        inputs: NamedValues::new(values),
        host: engine.inner.jjp.clone(),
        data_dir: engine.inner.data_dir.clone(),
        deadline,
        stop: stop.clone(),
        progress: Some(progress),
        started: Some(on_started),
        timings: engine.inner.timings,
    });
    tokio::pin!(execution);
    let report = loop {
        tokio::select! {
            report = &mut execution => break report,
            Some((group, leader_started)) = started_receiver.recv() => {
                if group.is_some()
                    && let Err(error) = repository
                        .set_process(id, group, leader_started.as_deref())
                        .await
                {
                    tracing::warn!(run = %id, error_code = ?error.code, "program's group not recorded");
                }
            }
        }
    };

    let verdict = report.verdict();
    let (mut state, mut cause, mut effect_unknown) =
        (verdict.state, verdict.cause, verdict.effect_unknown);
    let produced = match (&report.terminal, state) {
        (Some(Terminal::Complete { outputs: received }), RunState::Succeeded)
            if !body.outputs.is_empty() =>
        {
            let taken = outputs::from_message(&body.outputs, received.clone().into_inner());
            if taken.is_err() {
                state = RunState::Failed;
                cause = Some(Cause::Output);
                effect_unknown = true;
            }
            Some(taken)
        }
        _ => None,
    };
    let persists = definition.persists_outputs();
    let stored_outputs: Option<Vec<OutputValue>> = match (&produced, persists) {
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
    let quota_fits = stored_outputs.as_ref().map(|values| {
        values
            .iter()
            .map(|value| value.value.as_deref().map_or(0, str::len))
            .sum::<usize>()
            <= jykre_storage::OUTPUT_QUOTA_BYTES
    });
    let (exit_code, exit_signal) = match report.exit {
        Some(process::Exit::Code(code)) => (Some(i64::from(code)), None),
        Some(process::Exit::Signal(signal)) => (None, Some(i64::from(signal))),
        Some(process::Exit::Unknown) | None => (None, None),
    };
    let as_i64 = |bytes: Option<u64>| bytes.and_then(|bytes| i64::try_from(bytes).ok());
    let written = repository
        .finish_run(
            id,
            &RunFinish {
                state,
                cause,
                effect_unknown,
                exit_code,
                exit_signal,
                failure_code: report.failure_code().map(str::to_owned),
                log_bytes_stdout: as_i64(report.logs.stdout),
                log_bytes_stderr: as_i64(report.logs.stderr),
                log_bytes_jjp: as_i64(report.logs.jjp),
                outputs: stored_outputs,
                finished_at: UtcMillis::now(),
                ..RunFinish::default()
            },
        )
        .await;
    let history_failed = match written {
        Ok(()) => false,
        Err(error) => {
            // The program is never started again; recovery marks the run
            // interrupted.
            tracing::error!(run = %id, error_code = ?error.code, "automation run's end not recorded");
            true
        }
    };
    tracing::info!(
        run = %id,
        state = %state,
        cause = ?cause.map(|cause| cause.as_str()),
        effect_unknown,
        decided_by = ?report.decided_by,
        elapsed_ms = started.elapsed().as_millis() as u64,
        "automation program run finished"
    );
    Ended::Finished(Arc::new(RunOutcome {
        id,
        kind: JobKind::Process,
        state,
        cause,
        error: None,
        effect_unknown,
        sql: None,
        program: Some(ProgramOutcome {
            exit_code,
            exit_signal,
            failure_code: report.failure_code().map(str::to_owned),
            stop: report.stop,
            program,
            program_fingerprint: report.program_fingerprint.clone(),
            log_bytes: report.logs,
            protocol: report.protocol_problem,
            start_error: match report.decided_by {
                process::Decider::NotStarted(error) => Some(error),
                _ => None,
            },
        }),
        outputs: produced,
        outputs_stored: if persists { quota_fits } else { None },
        history_failed,
        started_at,
        elapsed: started.elapsed(),
    }))
}
