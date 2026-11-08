//! Phase 3 and the program side of phase 4 (specification, section 11):
//! the tree ends on Stop and at the deadline, ignored signals and inherited
//! pipes cannot keep a run open, the denylist holds, output spam is bounded,
//! exits map per the outcome table, and `jjp/1` programs end as 9.1.5 says.
//! The child program is this test binary ([`test_child`]); Unix-only tests
//! also use `sh`.
use super::prepare::{candidate_names, environment_with};
use super::test_child::{self, EXIT, MODE, OUTPUTS, PEER};
use super::*;
use crate::automation::definition::{EnvironmentSpec, OutputSpec, PinnedFile, ProcessBody};
use crate::automation::jjp::{JjpConfig, ProtocolProblem};
use crate::automation::logs::{self, LOG_LIMIT, Stream};
use jykre_core::ParameterKind;
use std::ffi::OsString;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use tokio::io::AsyncReadExt;
use tokio::net::TcpListener;
use tokio::sync::watch;

fn data_dir(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("jykre-program-{label}-{}", RunId::new()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn os(pairs: &[(&str, &str)]) -> Vec<(OsString, OsString)> {
    pairs
        .iter()
        .map(|(name, value)| (OsString::from(name), OsString::from(value)))
        .collect()
}

/// Short waits, so a test that must wait for the grace takes a fraction of
/// a second.
fn fast() -> Timings {
    Timings {
        grace: Duration::from_millis(400),
        drain: Duration::from_millis(500),
        reap: Duration::from_secs(10),
    }
}

/// One run of a body, with the knobs the tests turn.
struct Trial {
    body: ProcessBody,
    base: Vec<(OsString, OsString)>,
    inputs: NamedValues,
    host: JjpHost,
    timeout: Duration,
    stop: CancellationToken,
    timings: Timings,
    progress: Option<ProgressSink>,
}

/// A small file to stand for the entrypoint, so preparing a trial does not
/// hash this whole test binary.
fn small_entrypoint() -> String {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("Cargo.toml")
        .to_string_lossy()
        .into_owned()
}

/// The outputs of an accepted `complete`.
fn outputs_of(report: &ProgramReport) -> &NamedValues {
    match &report.terminal {
        Some(Terminal::Complete { outputs }) => Some(outputs),
        _ => None,
    }
    .expect("complete was accepted")
}

impl Trial {
    fn new(mut body: ProcessBody) -> Self {
        if body.entrypoint.is_none() && body.program == test_child::program() {
            body.entrypoint = Some(small_entrypoint());
        }
        Self {
            body,
            base: std::env::vars_os().collect(),
            inputs: NamedValues::default(),
            host: JjpHost::new(),
            timeout: Duration::from_secs(60),
            stop: CancellationToken::new(),
            timings: fast(),
            progress: None,
        }
    }

    async fn run(self, data: &Path) -> ProgramReport {
        let Trial {
            body,
            base,
            inputs,
            host,
            timeout,
            stop,
            timings,
            progress,
        } = self;
        let prepared = tokio::task::spawn_blocking(move || prepare(&body, base))
            .await
            .unwrap()
            .unwrap();
        run(ProgramRequest {
            run: RunId::new(),
            prepared,
            inputs,
            host,
            data_dir: data.to_path_buf(),
            deadline: Instant::now() + timeout,
            stop,
            progress,
            started: None,
            timings,
        })
        .await
    }
}

fn log_text(data: &Path, report: &ProgramReport, stream: Stream) -> String {
    String::from_utf8_lossy(&std::fs::read(logs::log_path(data, report.run, stream)).unwrap())
        .into_owned()
}

/// A listener the child's `hold` connects to; it counts connections and
/// tells when each one ends.
struct Peer {
    address: SocketAddr,
    connected: watch::Receiver<usize>,
    closed: watch::Receiver<usize>,
}

impl Peer {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (connected_sender, connected) = watch::channel(0);
        let (closed_sender, closed) = watch::channel(0);
        let closed_sender = std::sync::Arc::new(closed_sender);
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                connected_sender.send_modify(|count| *count += 1);
                let closed = closed_sender.clone();
                tokio::spawn(async move {
                    let mut byte = [0_u8; 1];
                    // EOF or a reset: the holder died.
                    let _ = stream.read(&mut byte).await;
                    closed.send_modify(|count| *count += 1);
                });
            }
        });
        Self {
            address,
            connected,
            closed,
        }
    }

    async fn wait_connected(&mut self, count: usize) {
        tokio::time::timeout(
            Duration::from_secs(30),
            self.connected.wait_for(|seen| *seen >= count),
        )
        .await
        .expect("the child and its grandchild connect")
        .unwrap();
    }

    async fn wait_closed(&mut self, count: usize) {
        tokio::time::timeout(
            Duration::from_secs(10),
            self.closed.wait_for(|seen| *seen >= count),
        )
        .await
        .expect("every holder of the peer died")
        .unwrap();
    }
}

// ---------------------------------------------------------------------------
// Before the run.

#[test]
fn the_environment_drops_the_denylist_and_unset_then_sets_and_adds_the_address() {
    let base = os(&[
        ("PATH", "/bin"),
        ("JYKRE_LOG", "debug"),
        ("JYKRE_CONFIG_DIR", "/data"),
        ("OTEL_EXPORTER_OTLP_HEADERS", "authorization=secret"),
        ("NODE_OPTIONS", "--require x"),
        ("PYTHONPATH", "p"),
        ("PYTHONSTARTUP", "s"),
        ("PYTHONHOME", "h"),
        ("AWS_PROFILE", "prod"),
        ("LOG_FORMAT", "json"),
        ("jykre_lower", "kept where names have case"),
    ]);
    let spec = EnvironmentSpec {
        unset: vec!["AWS_PROFILE".into()],
        set: vec![
            ("LOG_FORMAT".into(), "plain".into()),
            ("JYKRE_JJP_ADDRESS".into(), "the job's own".into()),
        ],
    };
    let address: SocketAddr = "127.0.0.1:4000".parse().unwrap();
    let unix = environment_with(base.clone(), &spec, Some(address), false);
    assert_eq!(
        unix,
        os(&[
            ("PATH", "/bin"),
            ("jykre_lower", "kept where names have case"),
            ("LOG_FORMAT", "plain"),
            ("JYKRE_JJP_ADDRESS", "127.0.0.1:4000"),
        ])
    );
    // Windows ignores case: `jykre_lower` is denied, `Path` is replaced.
    let mut spec_windows = spec.clone();
    spec_windows.set.push(("Path".into(), "C:\\tools".into()));
    let windows = environment_with(base.clone(), &spec_windows, None, true);
    assert_eq!(
        windows,
        os(&[
            ("LOG_FORMAT", "plain"),
            ("JYKRE_JJP_ADDRESS", "the job's own"),
            ("Path", "C:\\tools"),
        ])
    );
    // Without `jjp/1`, the job's own value stays; the host's comes last.
    assert!(
        environment_with(base.clone(), &spec, None, false).contains(&(
            OsString::from("JYKRE_JJP_ADDRESS"),
            OsString::from("the job's own")
        ))
    );
    let removed = removed_variables(base, &spec);
    assert!(removed.contains(&OsString::from("AWS_PROFILE")));
    assert!(removed.contains(&OsString::from("OTEL_EXPORTER_OTLP_HEADERS")));
    assert!(!removed.contains(&OsString::from("PATH")));
    assert!(is_denied(OsString::from("JYKRE_ANYTHING").as_os_str()));
    assert!(!is_denied(OsString::from("NODE_PATH").as_os_str()));
    assert_eq!(ENV_DENYLIST.len(), 6);
}

#[test]
fn windows_names_try_exe_and_com_in_pathext_order_and_never_scripts() {
    let pathext = OsString::from(".COM;.EXE;.BAT;.CMD;.VBS;.JS");
    assert_eq!(
        candidate_names("node", Some(&pathext), true),
        vec!["node.com", "node.exe"]
    );
    let reordered = OsString::from(".EXE;.CMD");
    assert_eq!(
        candidate_names("node", Some(&reordered), true),
        vec!["node.exe"]
    );
    assert_eq!(
        candidate_names("node", None, true),
        vec!["node.com", "node.exe"]
    );
    assert_eq!(
        candidate_names("Tool.EXE", Some(&pathext), true),
        vec!["Tool.EXE"]
    );
    assert_eq!(candidate_names("node", Some(&pathext), false), vec!["node"]);
}

#[cfg(unix)]
#[test]
fn programs_are_found_on_path_only_when_executable() {
    use std::os::unix::fs::PermissionsExt;
    let dir = data_dir("path");
    let tool = dir.join("tool");
    std::fs::write(&tool, b"#!/bin/sh\n").unwrap();
    std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();
    let plain = dir.join("plain");
    std::fs::write(&plain, b"data").unwrap();
    std::fs::set_permissions(&plain, std::fs::Permissions::from_mode(0o644)).unwrap();
    let path = std::env::join_paths([Path::new("relative"), dir.as_path()]).unwrap();
    assert_eq!(resolve_program("tool", Some(&path), None), Ok(tool));
    assert_eq!(
        resolve_program("plain", Some(&path), None),
        Err(StartError::PermissionDenied)
    );
    assert_eq!(
        resolve_program("missing", Some(&path), None),
        Err(StartError::NotFound)
    );
    assert_eq!(
        resolve_program("tool", None, None),
        Err(StartError::NotFound)
    );
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn definitions_that_cannot_run_are_refused_before_anything_starts() {
    let base = || std::env::vars_os().collect::<Vec<_>>();
    let with = |change: &dyn Fn(&mut ProcessBody)| {
        let mut body = test_child::body("exit:0", Protocol::None, &[]);
        change(&mut body);
        prepare(&body, base())
    };
    assert_eq!(
        with(&|body| body.program = " ".into()),
        Err(Refused::NoProgram)
    );
    assert_eq!(
        with(&|body| body.program = "C:\\jobs\\build.bat".into()),
        Err(Refused::ShellScript)
    );
    assert_eq!(
        with(&|body| body.program = "deploy.CMD".into()),
        Err(Refused::ShellScript)
    );
    assert_eq!(
        with(&|body| body.program = "bin/tool".into()),
        Err(Refused::ProgramPath)
    );
    assert_eq!(
        with(&|body| body.working_directory = Some("relative/dir".into())),
        Err(Refused::WorkingDirectory {
            path: "relative/dir".into()
        })
    );
    let missing = std::env::temp_dir().join(format!("jykre-missing-{}", RunId::new()));
    assert_eq!(
        with(&|body| body.working_directory = Some(missing.to_string_lossy().into_owned())),
        Err(Refused::WorkingDirectory {
            path: missing.clone()
        })
    );
    assert_eq!(
        with(&|body| body.entrypoint = Some("export.js".into())),
        Err(Refused::EntrypointPath {
            path: "export.js".into()
        })
    );
}

#[test]
fn a_changed_or_missing_pinned_file_refuses_the_run() {
    let dir = data_dir("pinned");
    let script = dir.join("export.js");
    std::fs::write(&script, b"console.log('version 1')\n").unwrap();
    let fingerprint = fingerprint_file(&script).unwrap();
    assert!(fingerprint.starts_with("sha256:") && fingerprint.len() == 71);
    let mut body = test_child::body("exit:0", Protocol::None, &[]);
    body.entrypoint = Some(script.to_string_lossy().into_owned());
    body.pinned = Some(PinnedFile {
        path: script.to_string_lossy().into_owned(),
        sha256: fingerprint.clone(),
    });
    let base = || std::env::vars_os().collect::<Vec<_>>();
    let launch = match prepare(&body, base()) {
        Ok(Prepared::Ready(launch)) => Some(launch),
        _ => None,
    }
    .expect("the pinned file matches");
    assert_eq!(launch.fingerprint.as_deref(), Some(fingerprint.as_str()));
    assert_eq!(launch.working_directory, dir);
    assert_eq!(launch.entrypoint, script);

    std::fs::write(&script, b"console.log('version 2')\n").unwrap();
    assert_eq!(
        prepare(&body, base()),
        Err(Refused::ProgramChanged {
            path: script.clone()
        })
    );
    std::fs::remove_file(&script).unwrap();
    assert_eq!(
        prepare(&body, base()),
        Err(Refused::ProgramChanged {
            path: script.clone()
        })
    );
    // Unpublished, a missing entrypoint cannot start; nothing is refused.
    body.pinned = None;
    assert_eq!(
        prepare(&body, base()),
        Ok(Prepared::CannotStart {
            error: StartError::NotFound,
            protocol: Protocol::None
        })
    );
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn a_missing_program_fails_with_not_found_and_keeps_no_logs() {
    let missing = std::env::temp_dir().join(format!("jykre-no-program-{}", RunId::new()));
    for program in [
        missing.to_string_lossy().into_owned(),
        format!(
            "jykre-no-such-program-{}",
            RunId::new().to_string().replace('-', "")
        ),
    ] {
        let mut body = test_child::body("exit:0", Protocol::Jjp1, &[]);
        body.program = program;
        let prepared = prepare(&body, std::env::vars_os()).unwrap();
        assert_eq!(
            prepared,
            Prepared::CannotStart {
                error: StartError::NotFound,
                protocol: Protocol::Jjp1
            }
        );
        let data = data_dir("missing");
        let host = JjpHost::new();
        let report = run(ProgramRequest {
            run: RunId::new(),
            prepared,
            inputs: NamedValues::default(),
            host: host.clone(),
            data_dir: data.clone(),
            deadline: Instant::now() + Duration::from_secs(5),
            stop: CancellationToken::new(),
            progress: None,
            started: None,
            timings: fast(),
        })
        .await;
        assert_eq!(report.decided_by, Decider::NotStarted(StartError::NotFound));
        assert_eq!(
            report.verdict(),
            Verdict {
                state: RunState::Failed,
                cause: Some(Cause::NotFound),
                effect_unknown: false
            }
        );
        assert!(!logs::log_dir(&data, report.run).exists());
        assert_eq!(host.address(), None, "nothing listened for it");
        std::fs::remove_dir_all(data).unwrap();
    }
}

#[cfg(unix)]
#[tokio::test]
async fn a_file_without_the_execute_permission_fails_with_permission_denied() {
    use std::os::unix::fs::PermissionsExt;
    let data = data_dir("not-executable");
    let program = data.join("tool");
    std::fs::write(&program, b"#!/bin/sh\nexit 0\n").unwrap();
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o644)).unwrap();
    let mut body = test_child::body("exit:0", Protocol::Jjp1, &[]);
    body.program = program.to_string_lossy().into_owned();
    body.arguments = Vec::new();
    let report = Trial::new(body).run(&data).await;
    assert_eq!(
        report.decided_by,
        Decider::NotStarted(StartError::PermissionDenied)
    );
    assert_eq!(report.verdict().cause, Some(Cause::PermissionDenied));
    assert!(!report.verdict().effect_unknown);
    assert!(!logs::log_dir(&data, report.run).exists());
    std::fs::remove_dir_all(data).unwrap();
}

// ---------------------------------------------------------------------------
// The outcome table.

#[test]
fn verdicts_follow_the_outcome_table() {
    let complete = Terminal::Complete {
        outputs: NamedValues::default(),
    };
    let fail = Terminal::Fail { code: "E".into() };
    let ok = |state, cause, effect_unknown| Verdict {
        state,
        cause,
        effect_unknown,
    };
    let cases = [
        (
            Protocol::None,
            Decider::Exit,
            Some(Exit::Code(0)),
            None,
            ok(RunState::Succeeded, None, false),
        ),
        (
            Protocol::None,
            Decider::Exit,
            Some(Exit::Code(3)),
            None,
            ok(RunState::Failed, Some(Cause::Exit), true),
        ),
        (
            Protocol::None,
            Decider::Exit,
            Some(Exit::Signal(9)),
            None,
            ok(RunState::Failed, Some(Cause::Exit), true),
        ),
        (
            Protocol::Jjp1,
            Decider::Exit,
            Some(Exit::Code(0)),
            Some(&complete),
            ok(RunState::Succeeded, None, false),
        ),
        (
            Protocol::Jjp1,
            Decider::Exit,
            Some(Exit::Code(2)),
            Some(&complete),
            ok(RunState::Failed, Some(Cause::Exit), true),
        ),
        (
            Protocol::Jjp1,
            Decider::Exit,
            Some(Exit::Signal(15)),
            Some(&complete),
            ok(RunState::Failed, Some(Cause::Exit), true),
        ),
        (
            Protocol::Jjp1,
            Decider::Exit,
            Some(Exit::Code(0)),
            Some(&fail),
            ok(RunState::Failed, Some(Cause::Program), true),
        ),
        (
            Protocol::Jjp1,
            Decider::Exit,
            Some(Exit::Code(7)),
            Some(&fail),
            ok(RunState::Failed, Some(Cause::Program), true),
        ),
        (
            Protocol::Jjp1,
            Decider::Exit,
            Some(Exit::Code(0)),
            None,
            ok(RunState::Failed, Some(Cause::Protocol), true),
        ),
        (
            Protocol::Jjp1,
            Decider::Exit,
            Some(Exit::Code(1)),
            None,
            ok(RunState::Failed, Some(Cause::Protocol), true),
        ),
        (
            Protocol::None,
            Decider::Exit,
            Some(Exit::Unknown),
            None,
            ok(RunState::Failed, Some(Cause::Internal), true),
        ),
        (
            Protocol::Jjp1,
            Decider::Stop,
            Some(Exit::Code(0)),
            Some(&complete),
            ok(RunState::Cancelled, None, true),
        ),
        (
            Protocol::None,
            Decider::Stop,
            Some(Exit::Signal(9)),
            None,
            ok(RunState::Cancelled, None, true),
        ),
        (
            Protocol::None,
            Decider::Deadline,
            Some(Exit::Signal(9)),
            None,
            ok(RunState::Failed, Some(Cause::Timeout), true),
        ),
        (
            Protocol::Jjp1,
            Decider::Deadline,
            Some(Exit::Code(0)),
            Some(&complete),
            ok(RunState::Failed, Some(Cause::Timeout), true),
        ),
        (
            Protocol::Jjp1,
            Decider::Protocol,
            Some(Exit::Signal(9)),
            None,
            ok(RunState::Failed, Some(Cause::Protocol), true),
        ),
        (
            Protocol::None,
            Decider::NotStarted(StartError::NotFound),
            None,
            None,
            ok(RunState::Failed, Some(Cause::NotFound), false),
        ),
        (
            Protocol::Jjp1,
            Decider::NotStarted(StartError::PermissionDenied),
            None,
            None,
            ok(RunState::Failed, Some(Cause::PermissionDenied), false),
        ),
        (
            Protocol::None,
            Decider::NotStarted(StartError::Other),
            None,
            None,
            ok(RunState::Failed, Some(Cause::Internal), false),
        ),
        (
            Protocol::None,
            Decider::NotStarted(StartError::Logs),
            None,
            None,
            ok(RunState::Failed, Some(Cause::Internal), false),
        ),
    ];
    for (protocol, decided_by, exit, terminal, expected) in cases {
        assert_eq!(
            verdict(protocol, decided_by, exit, terminal),
            expected,
            "{protocol:?} {decided_by:?} {exit:?} {terminal:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// Running.

#[tokio::test]
async fn exit_codes_decide_programs_without_a_protocol() {
    let data = data_dir("exit");
    let ok = Trial::new(test_child::body("exit:0", Protocol::None, &[]))
        .run(&data)
        .await;
    assert_eq!(ok.decided_by, Decider::Exit);
    assert_eq!(ok.exit, Some(Exit::Code(0)));
    assert_eq!(ok.verdict().state, RunState::Succeeded);
    assert!(!ok.verdict().effect_unknown);
    assert_eq!(ok.stop, None);
    assert_eq!(
        ok.program_fingerprint,
        Some(fingerprint_file(Path::new(&small_entrypoint())).unwrap())
    );
    if cfg!(unix) {
        assert!(ok.process_group.is_some_and(|group| group > 1));
    }
    if cfg!(target_os = "linux") {
        // A fast child may end before its start time is read.
        if let Some(started) = &ok.process_started {
            assert!(started.parse::<u64>().is_ok());
        }
    }
    assert_eq!(ok.logs.jjp, None);
    assert!(ok.logs.stdout.is_some() && ok.logs.stderr.is_some());

    let three = Trial::new(test_child::body("exit:3", Protocol::None, &[]))
        .run(&data)
        .await;
    assert_eq!(three.exit, Some(Exit::Code(3)));
    assert_eq!(
        three.verdict(),
        Verdict {
            state: RunState::Failed,
            cause: Some(Cause::Exit),
            effect_unknown: true
        }
    );
    std::fs::remove_dir_all(data).unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn a_signal_is_recorded_and_fails_with_exit() {
    let data = data_dir("signal");
    let mut body = test_child::body("", Protocol::None, &[]);
    body.program = "/bin/sh".into();
    body.arguments = vec!["-c".into(), "kill -9 $$".into()];
    let report = Trial::new(body).run(&data).await;
    assert_eq!(report.exit, Some(Exit::Signal(9)));
    assert_eq!(report.verdict().cause, Some(Cause::Exit));
    std::fs::remove_dir_all(data).unwrap();
}

#[tokio::test]
async fn stop_ends_the_whole_tree() {
    let data = data_dir("stop-tree");
    let mut peer = Peer::start().await;
    let address = peer.address.to_string();
    let mut trial = Trial::new(test_child::body(
        "tree",
        Protocol::None,
        &[(PEER, &address)],
    ));
    let stop = trial.stop.clone();
    let phases = std::sync::Arc::new(Mutex::new(Vec::new()));
    let seen = phases.clone();
    trial.progress = Some(std::sync::Arc::new(move |progress: &ProgramProgress| {
        seen.lock().unwrap().push(progress.phase);
    }));
    let folder = data.clone();
    let running = tokio::spawn(async move { trial.run(&folder).await });
    peer.wait_connected(2).await;
    let stopped_at = Instant::now();
    stop.cancel();
    let report = running.await.unwrap();
    peer.wait_closed(2).await;
    assert_eq!(report.decided_by, Decider::Stop);
    assert_eq!(report.verdict().state, RunState::Cancelled);
    assert!(report.verdict().effect_unknown);
    // On Unix `SIGTERM` reached the whole group, which ended on its own
    // (Linux watches the rest of the group end; other Unix systems kill
    // what is left at once). Windows cannot ask, so the job was killed
    // after the grace.
    if cfg!(target_os = "linux") {
        assert_eq!(report.stop, Some(StopResult::EndedOnItsOwn));
    } else if cfg!(unix) {
        assert!(report.stop.is_some());
    } else {
        assert_eq!(report.stop, Some(StopResult::Killed));
        assert!(stopped_at.elapsed() >= fast().grace);
    }
    assert_eq!(
        *phases.lock().unwrap(),
        vec![
            ProgramPhase::Starting,
            ProgramPhase::Running,
            ProgramPhase::Stopping
        ]
    );
    std::fs::remove_dir_all(data).unwrap();
}

#[tokio::test]
async fn the_deadline_ends_the_whole_tree() {
    let data = data_dir("deadline-tree");
    let mut peer = Peer::start().await;
    let address = peer.address.to_string();
    let mut trial = Trial::new(test_child::body(
        "tree",
        Protocol::None,
        &[(PEER, &address)],
    ));
    trial.timeout = Duration::from_secs(4);
    let folder = data.clone();
    let running = tokio::spawn(async move { trial.run(&folder).await });
    peer.wait_connected(2).await;
    let report = running.await.unwrap();
    peer.wait_closed(2).await;
    assert_eq!(report.decided_by, Decider::Deadline);
    assert_eq!(
        report.verdict(),
        Verdict {
            state: RunState::Failed,
            cause: Some(Cause::Timeout),
            effect_unknown: true
        }
    );
    assert!(report.stop.is_some());
    std::fs::remove_dir_all(data).unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn a_tree_that_ignores_sigterm_is_killed_after_the_grace() {
    let data = data_dir("ignore-term");
    let mut peer = Peer::start().await;
    let address = peer.address.to_string();
    // The shell ignores SIGTERM and execs the child, which keeps ignoring
    // it, as does the grandchild it starts.
    let mut body = test_child::body("tree", Protocol::None, &[(PEER, &address)]);
    let mut arguments = vec![
        "-c".to_owned(),
        "trap '' TERM; exec \"$0\" \"$@\"".to_owned(),
        body.program.clone(),
    ];
    arguments.extend(body.arguments.clone());
    body.program = "/bin/sh".into();
    body.arguments = arguments;
    let trial = Trial::new(body);
    let stop = trial.stop.clone();
    let folder = data.clone();
    let running = tokio::spawn(async move { trial.run(&folder).await });
    peer.wait_connected(2).await;
    let stopped_at = Instant::now();
    stop.cancel();
    let report = running.await.unwrap();
    let waited = stopped_at.elapsed();
    peer.wait_closed(2).await;
    assert_eq!(report.decided_by, Decider::Stop);
    assert_eq!(report.stop, Some(StopResult::Killed));
    assert_eq!(report.exit, Some(Exit::Signal(9)));
    assert!(waited >= fast().grace, "killed after {waited:?}");
    assert_eq!(Timings::default().grace, Duration::from_secs(5));
    std::fs::remove_dir_all(data).unwrap();
}

#[tokio::test]
async fn a_grandchild_holding_the_pipes_cannot_keep_the_run_open() {
    let data = data_dir("orphan");
    let mut peer = Peer::start().await;
    let address = peer.address.to_string();
    let trial = Trial::new(test_child::body(
        "orphan",
        Protocol::None,
        &[(PEER, &address)],
    ));
    let started = Instant::now();
    let report = trial.run(&data).await;
    // The grandchild would hold stdout for 60 s; it was killed with the
    // group or job when the leader exited.
    assert!(started.elapsed() < Duration::from_secs(20));
    assert_eq!(report.decided_by, Decider::Exit);
    assert_eq!(report.verdict().state, RunState::Succeeded);
    assert!(log_text(&data, &report, Stream::Stdout).contains("leader done"));
    if *peer.connected.borrow() > 0 {
        peer.wait_closed(1).await;
    }
    std::fs::remove_dir_all(data).unwrap();
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn a_descendant_that_left_the_group_is_abandoned_after_the_drain_grace() {
    let data = data_dir("setsid");
    let mut body = test_child::body("", Protocol::None, &[]);
    body.program = "/bin/sh".into();
    body.arguments = vec!["-c".into(), "setsid sleep 5 & echo started".into()];
    let started = Instant::now();
    let report = Trial::new(body).run(&data).await;
    let elapsed = started.elapsed();
    assert_eq!(report.verdict().state, RunState::Succeeded);
    assert!(elapsed < Duration::from_secs(4), "took {elapsed:?}");
    assert!(log_text(&data, &report, Stream::Stdout).contains("started"));
    std::fs::remove_dir_all(data).unwrap();
}

#[tokio::test]
async fn newline_free_spam_is_bounded_on_disk_and_ends_with_the_marker() {
    let data = data_dir("spam");
    let report = Trial::new(test_child::body("spam:8", Protocol::None, &[]))
        .run(&data)
        .await;
    assert_eq!(report.verdict().state, RunState::Succeeded);
    for stream in [Stream::Stdout, Stream::Stderr] {
        let path = logs::log_path(&data, report.run, stream);
        let size = std::fs::metadata(&path).unwrap().len();
        assert!(size <= LOG_LIMIT, "{stream:?} holds {size} bytes");
        let expected = match stream {
            Stream::Stdout => report.logs.stdout,
            _ => report.logs.stderr,
        };
        assert_eq!(expected, Some(size));
        let text = std::fs::read_to_string(&path).unwrap();
        let marker = text.lines().last().unwrap();
        assert!(
            marker.starts_with("[jykre] ")
                && marker.ends_with(" bytes dropped past the 1 MiB limit")
        );
        let dropped: u64 = marker
            .trim_start_matches("[jykre] ")
            .split(' ')
            .next()
            .unwrap()
            .parse()
            .unwrap();
        // The harness's own lines share stdout; the spam alone is 8 MiB.
        assert!(dropped >= 7 * 1024 * 1024, "{dropped} bytes dropped");
    }
    std::fs::remove_dir_all(data).unwrap();
}

#[tokio::test]
async fn denylisted_variables_are_not_inherited_and_nothing_is_logged() {
    let (subscriber, captured) = crate::logging::capture::subscriber(tracing::Level::TRACE);
    let _default = tracing::subscriber::set_default(subscriber);
    let data = data_dir("sentinel");
    let names = [
        "JYKRE_SENTINEL",
        "OTEL_EXPORTER_OTLP_HEADERS",
        "NODE_OPTIONS",
        "PYTHONPATH",
        "PYTHONSTARTUP",
        "PYTHONHOME",
        "UNSET_SENTINEL",
        "KEPT_SENTINEL",
        "SET_SENTINEL",
    ];
    let mode = format!("env:{}", names.join(","));
    let mut body = test_child::body(
        &mode,
        Protocol::None,
        &[("SET_SENTINEL", "set-value-73914")],
    );
    body.environment.unset = vec!["UNSET_SENTINEL".into()];
    let mut trial = Trial::new(body);
    for name in &names[..7] {
        trial.base.push((
            OsString::from(name),
            OsString::from("sentinel-secret-73914"),
        ));
    }
    trial.base.push((
        OsString::from("KEPT_SENTINEL"),
        OsString::from("kept-73914"),
    ));
    let report = trial.run(&data).await;
    assert_eq!(report.verdict().state, RunState::Succeeded);
    let stdout = log_text(&data, &report, Stream::Stdout);
    for name in &names[..7] {
        assert!(
            stdout.contains(&format!("{name}=absent")),
            "{name} reached the program"
        );
    }
    assert!(stdout.contains("KEPT_SENTINEL=present"));
    assert!(stdout.contains("SET_SENTINEL=present"));
    let logged = captured.text();
    assert!(
        logged.contains("program run ended"),
        "the run logged its end"
    );
    for secret in [
        "sentinel-secret-73914",
        "kept-73914",
        "set-value-73914",
        MODE,
    ] {
        assert!(!logged.contains(secret), "{secret} reached the log");
    }
    assert!(
        !logged.contains(&test_child::program()),
        "the path reached the log"
    );
    std::fs::remove_dir_all(data).unwrap();
}

// ---------------------------------------------------------------------------
// `jjp/1` programs.

fn declared() -> Vec<OutputSpec> {
    vec![
        OutputSpec {
            name: "rows".into(),
            kind: ParameterKind::Number,
        },
        OutputSpec {
            name: "note".into(),
            kind: ParameterKind::Text,
        },
    ]
}

fn jjp_trial(mode: &str, set: &[(&str, &str)]) -> Trial {
    let mut body = test_child::body(mode, Protocol::Jjp1, set);
    body.outputs = declared();
    Trial::new(body)
}

#[tokio::test]
async fn a_jjp_program_completes_with_its_outputs() {
    let data = data_dir("jjp-complete");
    let trial = jjp_trial(
        "jjp-complete",
        &[(OUTPUTS, r#"{"rows":"12.50","note":null}"#)],
    );
    let host = trial.host.clone();
    let report = trial.run(&data).await;
    assert_eq!(report.verdict().state, RunState::Succeeded, "{report:?}");
    assert!(report.hello);
    let outputs = outputs_of(&report);
    assert_eq!(outputs.get("rows"), Some(Some("12.50")));
    assert_eq!(outputs.get("note"), Some(None));
    assert!(!format!("{report:?}").contains("12.50"));
    assert_eq!(report.protocol_problem, None);
    let jjp = log_text(&data, &report, Stream::Jjp);
    assert!(jjp.contains("[info] working\n"));
    assert_eq!(report.logs.jjp, Some(jjp.len() as u64));
    assert!(log_text(&data, &report, Stream::Stdout).contains("\"result\":{}"));
    assert_eq!(host.address(), None, "the listener closed with the run");
    std::fs::remove_dir_all(data).unwrap();
}

#[tokio::test]
async fn a_jjp_complete_followed_by_a_nonzero_exit_fails_with_exit() {
    let data = data_dir("jjp-exit");
    let report = jjp_trial(
        "jjp-complete",
        &[(OUTPUTS, r#"{"rows":"1","note":"x"}"#), (EXIT, "4")],
    )
    .run(&data)
    .await;
    assert!(matches!(report.terminal, Some(Terminal::Complete { .. })));
    assert_eq!(report.exit, Some(Exit::Code(4)));
    assert_eq!(report.verdict().cause, Some(Cause::Exit));
    std::fs::remove_dir_all(data).unwrap();
}

#[tokio::test]
async fn a_jjp_program_that_fails_records_its_code() {
    let data = data_dir("jjp-fail");
    let report = jjp_trial("jjp-fail", &[]).run(&data).await;
    assert_eq!(report.failure_code(), Some("E_TEST"));
    assert_eq!(report.verdict().cause, Some(Cause::Program));
    assert!(log_text(&data, &report, Stream::Jjp).contains("[fail E_TEST] synthetic failure\n"));
    std::fs::remove_dir_all(data).unwrap();
}

#[tokio::test]
async fn a_jjp_program_that_ends_without_a_terminal_fails_with_protocol() {
    let data = data_dir("jjp-silent");
    let report = jjp_trial("jjp-silent", &[]).run(&data).await;
    assert_eq!(report.verdict().cause, Some(Cause::Protocol));
    assert!(report.hello);
    assert!(matches!(
        report.protocol_problem,
        Some(ProtocolProblem::Disconnected | ProtocolProblem::NoTerminal)
    ));
    std::fs::remove_dir_all(data).unwrap();
}

#[tokio::test]
async fn a_jjp_program_that_never_says_hello_is_killed() {
    let data = data_dir("jjp-no-hello");
    let mut trial = jjp_trial("jjp-no-hello", &[]);
    trial.host = JjpHost::with_config(JjpConfig {
        hello_timeout: Duration::from_millis(500),
        ..JjpConfig::default()
    });
    let started = Instant::now();
    let report = trial.run(&data).await;
    assert!(started.elapsed() < Duration::from_secs(20));
    assert_eq!(report.decided_by, Decider::Protocol);
    assert_eq!(report.protocol_problem, Some(ProtocolProblem::HelloTimeout));
    assert_eq!(report.stop, Some(StopResult::Killed));
    assert!(!report.hello);
    assert!(log_text(&data, &report, Stream::Jjp).contains("did not say hello in time"));
    std::fs::remove_dir_all(data).unwrap();
}

/// A trial whose progress says when the program said `hello`.
fn with_hello_signal(trial: &mut Trial) -> watch::Receiver<bool> {
    let (sender, receiver) = watch::channel(false);
    trial.progress = Some(std::sync::Arc::new(move |progress: &ProgramProgress| {
        if progress.hello {
            sender.send_replace(true);
        }
    }));
    receiver
}

#[tokio::test]
async fn stop_sends_cancel_and_a_program_that_obeys_ends_on_its_own() {
    let data = data_dir("jjp-cancel");
    let mut trial = jjp_trial("jjp-cancel", &[]);
    trial.timings.grace = Duration::from_secs(10);
    let mut hello = with_hello_signal(&mut trial);
    let stop = trial.stop.clone();
    let folder = data.clone();
    let running = tokio::spawn(async move { trial.run(&folder).await });
    tokio::time::timeout(Duration::from_secs(30), hello.wait_for(|said| *said))
        .await
        .unwrap()
        .unwrap();
    let stopped_at = Instant::now();
    stop.cancel();
    let report = running.await.unwrap();
    assert!(stopped_at.elapsed() < Duration::from_secs(10));
    assert_eq!(report.decided_by, Decider::Stop);
    assert_eq!(report.stop, Some(StopResult::EndedOnItsOwn));
    assert_eq!(report.verdict().state, RunState::Cancelled);
    // The answer to `cancel` is recorded, but Stop decided first.
    assert_eq!(report.failure_code(), Some("CANCELLED"));
    std::fs::remove_dir_all(data).unwrap();
}

#[tokio::test]
async fn cancellation_ends_a_program_that_never_reads() {
    let data = data_dir("jjp-flood");
    let mut trial = jjp_trial("jjp-flood", &[]);
    // Enough room for every answer, so only Stop decides: the program
    // floods and reads nothing, and its socket stays full.
    trial.host = JjpHost::with_config(JjpConfig {
        max_pending_output: 64 * 1024 * 1024,
        ..JjpConfig::default()
    });
    let mut hello = with_hello_signal(&mut trial);
    let stop = trial.stop.clone();
    let folder = data.clone();
    let running = tokio::spawn(async move { trial.run(&folder).await });
    tokio::time::timeout(Duration::from_secs(30), hello.wait_for(|said| *said))
        .await
        .unwrap()
        .unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    let stopped_at = Instant::now();
    stop.cancel();
    let report = running.await.unwrap();
    let waited = stopped_at.elapsed();
    assert_eq!(report.decided_by, Decider::Stop);
    assert_eq!(report.stop, Some(StopResult::Killed));
    assert!(waited < Duration::from_secs(10), "stopping took {waited:?}");
    std::fs::remove_dir_all(data).unwrap();
}

#[tokio::test]
async fn inputs_reach_the_program_and_come_back_as_outputs() {
    let data = data_dir("jjp-echo");
    let mut trial = jjp_trial("jjp-echo", &[]);
    trial.inputs = NamedValues::new(vec![
        ("rows".into(), Some("3".into())),
        ("note".into(), Some("ação".into())),
    ]);
    let report = trial.run(&data).await;
    assert_eq!(report.verdict().state, RunState::Succeeded, "{report:?}");
    assert_eq!(outputs_of(&report).get("note"), Some(Some("ação")));
    std::fs::remove_dir_all(data).unwrap();
}

/// The Node.js example in `examples/jjp/`, end to end. Ignored by default:
/// CI runners are not required to have Node.js. Run it with
/// `cargo test --lib automation::process::tests::the_node_example -- --ignored`.
#[tokio::test]
#[ignore = "needs node on PATH"]
async fn the_node_example_exports_and_completes() {
    let node = resolve_program(
        "node",
        std::env::var_os("PATH").as_deref(),
        std::env::var_os("PATHEXT").as_deref(),
    )
    .expect("not run: node is not on PATH (this test needs Node.js)");
    let data = data_dir("node");
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/jjp/export.js");
    let target = data.join("rows.csv");
    let mut body = test_child::body("", Protocol::Jjp1, &[]);
    body.program = node.to_string_lossy().into_owned();
    body.arguments = vec![script.to_string_lossy().into_owned()];
    body.entrypoint = Some(script.to_string_lossy().into_owned());
    body.outputs = vec![
        OutputSpec {
            name: "rows".into(),
            kind: ParameterKind::Number,
        },
        OutputSpec {
            name: "file".into(),
            kind: ParameterKind::Text,
        },
    ];
    let mut trial = Trial::new(body);
    trial.inputs = NamedValues::new(vec![
        ("rows".into(), Some("5".into())),
        ("file".into(), Some(target.to_string_lossy().into_owned())),
    ]);
    let report = trial.run(&data).await;
    assert_eq!(report.verdict().state, RunState::Succeeded, "{report:?}");
    assert_eq!(outputs_of(&report).get("rows"), Some(Some("5")));
    assert_eq!(std::fs::read_to_string(&target).unwrap().lines().count(), 6);
    std::fs::remove_dir_all(data).unwrap();
}

// ---------------------------------------------------------------------------
// Recovery (Linux).

#[cfg(target_os = "linux")]
#[tokio::test]
async fn a_live_group_is_found_by_its_start_time_and_ended() {
    use process_wrap::tokio::{CommandWrap, ProcessGroup};
    let mut command = tokio::process::Command::new("/bin/sh");
    command
        .args(["-c", "sleep 30 & wait"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    let mut wrapped = CommandWrap::from(command);
    wrapped.wrap(ProcessGroup::leader());
    let mut child = wrapped.spawn().unwrap();
    let group = i64::from(child.id().unwrap());
    let started = recovery::start_time(child.id().unwrap()).await.unwrap();
    assert!(orphan_alive(group, &started));
    // Another start time is another process that reused the ID.
    assert!(!orphan_alive(group, "1"));
    assert!(!end_group(group, "1").unwrap());
    assert!(orphan_alive(group, &started), "nothing was sent");

    assert!(end_group(group, &started).unwrap());
    let status = tokio::time::timeout(Duration::from_secs(10), child.inner_mut().wait())
        .await
        .unwrap()
        .unwrap();
    use std::os::unix::process::ExitStatusExt;
    assert_eq!(status.signal(), Some(9));
    assert!(!orphan_alive(group, &started));
    assert!(!end_group(group, &started).unwrap());
}
