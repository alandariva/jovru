//! A program every test platform can run: this test binary, started again
//! on [`automation_test_child`] alone, told what to do by
//! [`MODE`] in its environment. The variable travels through the job's
//! `environment.set`, as a user's constant would; a `JYKRE_*` name would
//! read as if it escaped the denylist.
//!
//! Modes (`name` or `name:argument`):
//!
//! - `exit:<code>`: exits with that code.
//! - `print:<text>`: prints the text to stdout, exits 0.
//! - `spam:<MiB>`: writes that many MiB without a newline to stdout and to
//!   stderr, exits 0.
//! - `env:<A,B,…>`: prints `A=present` or `A=absent` for each name.
//! - `sleep`: sleeps 60 s.
//! - `hold`: connects to [`PEER`] and waits there up to 60 s; the peer sees
//!   the connection end when this process dies.
//! - `tree`: starts a `hold` grandchild, then holds too.
//! - `orphan`: starts a `hold` grandchild that inherits stdout and stderr,
//!   then exits 0 at once.
//! - `jjp-complete`: `hello`, a `log`, a `progress`, then `complete` with
//!   the JSON object in [`OUTPUTS`], then exits with [`EXIT`] (default 0).
//! - `jjp-fail`: `hello`, then `fail` with code `E_TEST`, exits 0.
//! - `jjp-silent`: `hello`, then exits 0 without a terminal message.
//! - `jjp-no-hello`: never connects; sleeps 60 s.
//! - `jjp-cancel`: `hello`, then waits for `cancel` and answers with
//!   `fail` code `CANCELLED`, exits 0.
//! - `jjp-flood`: `hello`, then 100,000 requests for an unknown method
//!   without reading a single answer, then sleeps 60 s.
//! - `jjp-echo`: `hello`, then `complete` with its inputs as outputs.
use crate::automation::definition::{DEFAULT_TIMEOUT_MS, EnvironmentSpec, ProcessBody, Protocol};
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::time::Duration;

/// What the child does.
pub(super) const MODE: &str = "AUTOMATION_TEST_CHILD";
/// `127.0.0.1:<port>` of the test's listener, for `hold`.
pub(super) const PEER: &str = "AUTOMATION_TEST_PEER";
/// `complete`'s outputs, as a JSON object.
pub(super) const OUTPUTS: &str = "AUTOMATION_TEST_OUTPUTS";
/// The exit code after `complete`.
pub(super) const EXIT: &str = "AUTOMATION_TEST_EXIT";

/// The libtest name of [`automation_test_child`].
const NAME: &str = "automation::process::test_child::automation_test_child";

/// What starts this binary on the helper alone.
pub(super) fn arguments() -> Vec<String> {
    [
        NAME,
        "--exact",
        "--nocapture",
        "--quiet",
        "--test-threads=1",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect()
}

/// This test binary, as an absolute path.
pub(super) fn program() -> String {
    std::env::current_exe()
        .expect("the test binary has a path")
        .to_string_lossy()
        .into_owned()
}

/// A job that runs this binary in `mode`, with more constants in its
/// environment.
pub(super) fn body(mode: &str, protocol: Protocol, set: &[(&str, &str)]) -> ProcessBody {
    ProcessBody {
        program: program(),
        arguments: arguments(),
        entrypoint: None,
        working_directory: None,
        environment: EnvironmentSpec {
            unset: Vec::new(),
            set: std::iter::once((MODE, mode))
                .chain(set.iter().copied())
                .map(|(name, value)| (name.to_owned(), value.to_owned()))
                .collect(),
        },
        protocol,
        inputs: Vec::new(),
        outputs: Vec::new(),
        timeout_ms: DEFAULT_TIMEOUT_MS,
        persist_outputs: false,
        pinned: None,
    }
}

#[test]
fn automation_test_child() {
    let Ok(mode) = std::env::var(MODE) else {
        // Run as a test of its own: there is nothing to do.
        return;
    };
    let code = act(&mode);
    let _ = std::io::stdout().flush();
    std::process::exit(code);
}

fn act(mode: &str) -> i32 {
    let (name, argument) = mode.split_once(':').unwrap_or((mode, ""));
    match name {
        "exit" => argument.parse().unwrap_or(1),
        "print" => {
            write_out(argument.as_bytes());
            0
        }
        "spam" => {
            let megabytes: usize = argument.parse().unwrap_or(1);
            let chunk = vec![b'x'; 1024 * 1024];
            for _ in 0..megabytes {
                let _ = std::io::stdout().lock().write_all(&chunk);
                let _ = std::io::stderr().lock().write_all(&chunk);
            }
            0
        }
        "env" => {
            for variable in argument.split(',') {
                let state = if std::env::var_os(variable).is_some() {
                    "present"
                } else {
                    "absent"
                };
                write_out(format!("{variable}={state}\n").as_bytes());
            }
            0
        }
        "sleep" => {
            std::thread::sleep(Duration::from_secs(60));
            0
        }
        "hold" => {
            hold();
            0
        }
        "tree" => {
            let _grandchild = grandchild(false);
            hold();
            0
        }
        "orphan" => {
            let _grandchild = grandchild(true);
            write_out(b"leader done\n");
            0
        }
        "jjp-complete" => {
            let mut client = Client::hello();
            client.notify("log", json!({"level": "info", "message": "working"}));
            client.notify("progress", json!({"percent": 50, "stage": "halfway"}));
            let outputs: Value = std::env::var(OUTPUTS)
                .ok()
                .and_then(|text| serde_json::from_str(&text).ok())
                .unwrap_or_else(|| json!({}));
            let answer = client.request("complete", json!({"outputs": outputs}));
            write_out(format!("complete answer: {answer}\n").as_bytes());
            std::env::var(EXIT)
                .ok()
                .and_then(|code| code.parse().ok())
                .unwrap_or(0)
        }
        "jjp-fail" => {
            let mut client = Client::hello();
            client.request(
                "fail",
                json!({"code": "E_TEST", "message": "synthetic failure"}),
            );
            0
        }
        "jjp-silent" => {
            let _client = Client::hello();
            0
        }
        "jjp-no-hello" => {
            std::thread::sleep(Duration::from_secs(60));
            0
        }
        "jjp-cancel" => {
            let mut client = Client::hello();
            loop {
                let Some(message) = client.read() else {
                    return 2;
                };
                if message["method"] == "cancel" {
                    client.request("fail", json!({"code": "CANCELLED"}));
                    return 0;
                }
            }
        }
        "jjp-flood" => {
            let mut client = Client::hello();
            let mut lines = Vec::new();
            for id in 0..100_000 {
                lines.extend_from_slice(
                    format!("{{\"jsonrpc\":\"2.0\",\"id\":{id},\"method\":\"nope\"}}\n").as_bytes(),
                );
            }
            let _ = client.stream.write_all(&lines);
            std::thread::sleep(Duration::from_secs(60));
            0
        }
        "jjp-echo" => {
            let mut client = Client::hello();
            let inputs = client.hello["result"]["inputs"].clone();
            client.request("complete", json!({"outputs": inputs}));
            0
        }
        _ => 100,
    }
}

fn write_out(bytes: &[u8]) {
    let mut stdout = std::io::stdout().lock();
    let _ = stdout.write_all(bytes);
    let _ = stdout.flush();
}

/// Connects to the test's peer and waits until the process dies (60 s at
/// most, so a leak cleans itself up).
fn hold() {
    let Some(peer) = std::env::var(PEER).ok() else {
        return;
    };
    if let Ok(mut stream) = TcpStream::connect(peer) {
        let _ = stream.set_read_timeout(Some(Duration::from_secs(60)));
        let mut byte = [0_u8; 1];
        let _ = stream.read(&mut byte);
    }
}

/// Starts this binary again in `hold` mode; with `inherit`, it keeps this
/// process's stdout and stderr.
fn grandchild(inherit: bool) -> Option<std::process::Child> {
    let mut command = std::process::Command::new(std::env::current_exe().ok()?);
    command.args(arguments()).env(MODE, "hold");
    if !inherit {
        command
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
    }
    command.stdin(std::process::Stdio::null()).spawn().ok()
}

/// A blocking `jjp/1` client, as small as a program's would be.
struct Client {
    stream: TcpStream,
    reader: BufReader<TcpStream>,
    hello: Value,
    next_id: u64,
}

impl Client {
    /// Reads the token from stdin, connects and says `hello`.
    fn hello() -> Self {
        let mut line = String::new();
        let _ = std::io::stdin().lock().read_line(&mut line);
        let token: Value = serde_json::from_str(&line).unwrap_or(Value::Null);
        let address = std::env::var("JYKRE_JJP_ADDRESS").unwrap_or_default();
        let stream = TcpStream::connect(address).expect("the host listens");
        let reader = BufReader::new(stream.try_clone().expect("a socket clones"));
        let mut client = Self {
            stream,
            reader,
            hello: Value::Null,
            next_id: 1,
        };
        client.hello = client.request(
            "hello",
            json!({"token": token["token"], "protocol": "jjp/1"}),
        );
        client
    }

    fn notify(&mut self, method: &str, params: Value) {
        let line = json!({"jsonrpc": "2.0", "method": method, "params": params});
        let _ = writeln!(self.stream, "{line}");
    }

    /// Sends a request and returns its answer, skipping notifications.
    fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        let line = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        let _ = writeln!(self.stream, "{line}");
        while let Some(message) = self.read() {
            if message["id"] == id {
                return message;
            }
        }
        Value::Null
    }

    fn read(&mut self) -> Option<Value> {
        let mut line = String::new();
        match self.reader.read_line(&mut line) {
            Ok(0) | Err(_) => None,
            Ok(_) => serde_json::from_str(&line).ok(),
        }
    }
}
