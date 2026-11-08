//! The listener and one task per connection (sections 9.2.1–9.2.3).
//!
//! A connection reads and writes independently: the reader always drains
//! the socket, and responses (and `cancel`) wait in a queue the writer
//! empties as the program reads. A program that reads nothing therefore
//! never stops the host from reading it, and asking it to end never waits.
use super::frame::{Framer, Piece};
use super::message::{self, Incoming};
use super::{
    Claim, FAIL_ROOM, HostShared, JjpEvent, Link, MAX_MESSAGE_BYTES, ProtocolProblem, Terminal,
    error_code,
};
use jykre_core::CancellationToken;
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinSet;
use tokio::time::Instant;

/// Bytes read from the socket at a time.
const READ_CHUNK: usize = 64 * 1024;

/// Accepts connections until `stop`; dropping the set at the end aborts
/// the connections still open, so no task outlives the listener.
pub(super) async fn accept(listener: TcpListener, host: Arc<HostShared>, stop: CancellationToken) {
    let mut connections = JoinSet::new();
    loop {
        tokio::select! {
            biased;
            () = stop.cancelled() => break,
            Some(_) = connections.join_next(), if !connections.is_empty() => {}
            accepted = listener.accept() => match accepted {
                Ok((stream, peer)) => {
                    // Bound to 127.0.0.1, so a peer is always local; the
                    // limit keeps idle connections from piling up.
                    if peer.ip().is_loopback() && connections.len() < host.config.max_connections {
                        connections.spawn(serve(stream, host.clone()));
                    }
                }
                Err(error) => {
                    tracing::debug!(error_kind = ?error.kind(), "jjp/1 accept failed");
                    // Out of descriptors, for example: wait instead of spinning.
                    tokio::select! {
                        () = stop.cancelled() => break,
                        () = tokio::time::sleep(Duration::from_millis(50)) => {}
                    }
                }
            }
        }
    }
}

/// Responses waiting for the writer, bounded by what the program left
/// unread.
struct Outbox {
    sender: mpsc::UnboundedSender<Vec<u8>>,
    pending: Arc<AtomicUsize>,
    limit: usize,
}

impl Outbox {
    /// Queues a response; `false` when the program has left more than the
    /// limit unread.
    fn send(&self, bytes: Vec<u8>) -> bool {
        let size = bytes.len();
        let before = self.pending.fetch_add(size, Ordering::AcqRel);
        if before.saturating_add(size) > self.limit {
            self.pending.fetch_sub(size, Ordering::AcqRel);
            return false;
        }
        self.push(bytes);
        true
    }

    /// Queues past the limit: `cancel`, and the last error before closing.
    fn force(&self, bytes: Vec<u8>) {
        self.pending.fetch_add(bytes.len(), Ordering::AcqRel);
        self.push(bytes);
    }

    fn push(&self, bytes: Vec<u8>) {
        // The writer is gone only once the socket failed: nothing more can
        // be delivered, and the reader finds out on its next read.
        let _ = self.sender.send(bytes);
    }
}

async fn serve(stream: TcpStream, host: Arc<HostShared>) {
    // Responses are small and awaited one by one; without this they could
    // wait for Nagle's timer. Failing to set it only delays them.
    let _ = stream.set_nodelay(true);
    let (reader, writer) = stream.into_split();
    let (sender, queue) = mpsc::unbounded_channel();
    let pending = Arc::new(AtomicUsize::new(0));
    let reader_done = CancellationToken::new();
    let hard_stop = CancellationToken::new();
    let flush = host.config.flush;
    let connection = Connection {
        outbox: Outbox {
            sender,
            pending: pending.clone(),
            limit: host.config.max_pending_output,
        },
        host,
        hard_stop: hard_stop.clone(),
        link: None,
        cancel: None,
        progress_last: None,
        progress_pending: None,
        traffic: 0,
    };
    let read = async {
        connection.run(reader).await;
        reader_done.cancel();
    };
    tokio::join!(
        read,
        write(
            writer,
            queue,
            pending,
            reader_done.clone(),
            hard_stop,
            flush
        )
    );
}

/// Writes queued responses in order. Ends when the reader ended and the
/// queue is empty, `flush` after the reader ended, or at once on
/// `hard_stop`; a write the program never reads is simply abandoned then.
async fn write(
    mut writer: OwnedWriteHalf,
    mut queue: mpsc::UnboundedReceiver<Vec<u8>>,
    pending: Arc<AtomicUsize>,
    reader_done: CancellationToken,
    hard_stop: CancellationToken,
    flush: Duration,
) {
    let give_up = async {
        reader_done.cancelled().await;
        tokio::time::sleep(flush).await;
    };
    tokio::pin!(give_up);
    loop {
        let bytes = tokio::select! {
            biased;
            () = hard_stop.cancelled() => return,
            () = &mut give_up => return,
            bytes = queue.recv() => match bytes {
                Some(bytes) => bytes,
                None => return,
            },
        };
        let written = tokio::select! {
            biased;
            () = hard_stop.cancelled() => return,
            () = &mut give_up => return,
            written = writer.write_all(&bytes) => written,
        };
        pending.fetch_sub(bytes.len(), Ordering::AcqRel);
        if written.is_err() {
            return;
        }
    }
}

/// Whether the connection goes on after a message.
#[derive(Debug, PartialEq, Eq)]
enum Flow {
    Continue,
    Close,
}

/// The reading side of one connection: unauthenticated until its `hello`,
/// then bound to one run.
struct Connection {
    host: Arc<HostShared>,
    outbox: Outbox,
    hard_stop: CancellationToken,
    /// The run, once `hello` presented its token.
    link: Option<Link>,
    /// Whether the run's owner asked the program to end; `None` once
    /// `cancel` was queued.
    cancel: Option<watch::Receiver<bool>>,
    progress_last: Option<Instant>,
    progress_pending: Option<super::JjpProgress>,
    traffic: u64,
}

impl Connection {
    async fn run(mut self, mut reader: OwnedReadHalf) {
        let config = self.host.config;
        let hello_deadline = Instant::now() + config.hello_timeout;
        let mut framer = Framer::new(MAX_MESSAGE_BYTES);
        let mut buffer = vec![0_u8; READ_CHUNK];
        let mut pieces = Vec::new();
        loop {
            let stop = self.link.as_ref().map(|link| link.run.stop.clone());
            let progress_due = match (&self.progress_pending, self.progress_last) {
                (Some(_), Some(last)) => Some(last + config.progress_interval),
                _ => None,
            };
            let bound = self.link.is_some();
            let read = tokio::select! {
                biased;
                () = until_cancelled(stop) => {
                    // The session ended: close at once.
                    self.hard_stop.cancel();
                    return;
                }
                () = tokio::time::sleep_until(hello_deadline), if !bound => return,
                () = until_cancel_requested(&mut self.cancel) => {
                    self.cancel = None;
                    self.outbox.force(message::cancel());
                    continue;
                }
                () = tokio::time::sleep_until(progress_due.unwrap_or(hello_deadline)), if progress_due.is_some() => {
                    if let (Some(progress), Some(link)) = (self.progress_pending.take(), &self.link) {
                        link.run.progress.send_replace(Some(progress));
                        self.progress_last = Some(Instant::now());
                    }
                    continue;
                }
                read = reader.read(&mut buffer) => read,
            };
            // The end of the stream, or a broken one: a bound run's session
            // sees its link dropped and decides.
            let read = match read {
                Ok(0) | Err(_) => return,
                Ok(read) => read,
            };
            self.traffic = self
                .traffic
                .saturating_add(u64::try_from(read).unwrap_or(u64::MAX));
            if self.link.is_some() && self.traffic > config.traffic_quota {
                self.outbox.force(message::error(
                    &Value::Null,
                    error_code::QUOTA,
                    "Quota exceeded",
                    None,
                ));
                self.fail(ProtocolProblem::Quota).await;
                return;
            }
            let chunk = buffer.get(..read).unwrap_or_default();
            pieces.clear();
            let framed = framer.split(chunk, &mut pieces);
            for piece in pieces.drain(..) {
                let line = match &piece {
                    Piece::InChunk(range) => chunk.get(range.clone()).unwrap_or_default(),
                    Piece::Joined(line) => line.as_slice(),
                };
                if self.line(line).await == Flow::Close {
                    return;
                }
            }
            if framed.is_err() {
                self.fail(ProtocolProblem::LineTooLong).await;
                return;
            }
        }
    }

    /// One complete line.
    async fn line(&mut self, line: &[u8]) -> Flow {
        // A blank line carries no message.
        if line.iter().all(u8::is_ascii_whitespace) {
            return Flow::Continue;
        }
        let Ok(text) = std::str::from_utf8(line) else {
            return self.fail(ProtocolProblem::InvalidUtf8).await;
        };
        let Ok(value) = serde_json::from_str::<Value>(text) else {
            self.outbox.force(message::error(
                &Value::Null,
                error_code::PARSE_ERROR,
                "Parse error",
                None,
            ));
            return self.fail(ProtocolProblem::InvalidJson).await;
        };
        match message::classify(value) {
            Ok(incoming) if self.link.is_none() => self.unbound(incoming),
            Ok(incoming) => self.bound(incoming).await,
            Err(id) => {
                let sent = self.reply(message::error(
                    &id,
                    error_code::INVALID_REQUEST,
                    "Invalid request",
                    None,
                ));
                match sent {
                    Flow::Continue if self.link.is_none() => Flow::Close,
                    sent => self.after_reply(sent).await,
                }
            }
        }
    }

    /// Before `hello`: only `hello` is accepted, and anything else closes the
    /// connection. Nothing here touches a run, so nothing fails.
    fn unbound(&mut self, incoming: Incoming) -> Flow {
        let Some(id) = incoming.id else {
            return Flow::Close;
        };
        if incoming.method != "hello" {
            self.reply(message::error(
                &id,
                error_code::HELLO,
                "hello is required first",
                None,
            ));
            return Flow::Close;
        }
        let token = match message::hello_token(&incoming.params) {
            Ok(token) => token,
            Err(text) => {
                self.reply(message::error(&id, error_code::INVALID_PARAMS, text, None));
                return Flow::Close;
            }
        };
        match self.host.claim(token) {
            Claim::Bound(link) => {
                link.run
                    .hello
                    .store(true, std::sync::atomic::Ordering::Release);
                let result = message::hello_result(&link.run, std::time::Instant::now());
                self.outbox.force(message::result(&id, result));
                // The channel holds every control event of a run.
                let _ = link.events.try_send(JjpEvent::Hello);
                tracing::debug!(run = %link.run.run, "jjp/1 hello accepted");
                self.cancel = Some(link.run.cancel.subscribe());
                self.link = Some(link);
                Flow::Continue
            }
            Claim::Refused => {
                self.reply(message::error(
                    &id,
                    error_code::TOKEN,
                    "Invalid or already used token",
                    None,
                ));
                Flow::Close
            }
        }
    }

    /// After `hello`: the run's messages.
    async fn bound(&mut self, incoming: Incoming) -> Flow {
        let Some(link) = &self.link else {
            return Flow::Close;
        };
        let run = link.run.clone();
        let Incoming { id, method, params } = incoming;
        if run.terminal_accepted() {
            // After `complete` or `fail` nothing else is accepted.
            return match id {
                Some(id) => {
                    let sent = self.reply(message::error(
                        &id,
                        error_code::TERMINAL,
                        "A terminal message was already accepted",
                        None,
                    ));
                    self.after_reply(sent).await
                }
                None => Flow::Continue,
            };
        }
        let sent = match (method.as_str(), id) {
            ("log", None) => {
                if let Some((level, text)) = message::log_params(&params) {
                    let mut line = Vec::with_capacity(text.len() + 9);
                    line.extend_from_slice(format!("[{level}] ").as_bytes());
                    line.extend_from_slice(text.as_bytes());
                    line.push(b'\n');
                    run.write_log(&line, FAIL_ROOM).await;
                }
                Flow::Continue
            }
            ("progress", None) => {
                if let Some(progress) = message::progress_params(&params) {
                    self.progress(progress);
                }
                Flow::Continue
            }
            ("complete", Some(id)) => match message::check_outputs(&run.outputs, &params) {
                Ok(outputs) => self.terminal(&id, Terminal::Complete { outputs }),
                Err(problem) => self.reply(message::error(
                    &id,
                    error_code::INVALID_PARAMS,
                    &problem.message(&run.outputs),
                    Some(problem.data()),
                )),
            },
            ("fail", Some(id)) => match message::fail_params(&params) {
                Ok(failure) => {
                    // The message may hold values: `jjp.log` only.
                    let mut line = format!("[fail {}] ", failure.code).into_bytes();
                    line.extend_from_slice(failure.message.as_bytes());
                    if failure.cut {
                        line.extend_from_slice(" [cut at 4 KiB]".as_bytes());
                    }
                    line.push(b'\n');
                    run.write_log(&line, 0).await;
                    self.terminal(&id, Terminal::Fail { code: failure.code })
                }
                Err(text) => {
                    self.reply(message::error(&id, error_code::INVALID_PARAMS, text, None))
                }
            },
            ("hello", Some(id)) => self.reply(message::error(
                &id,
                error_code::HELLO,
                "hello was already sent",
                None,
            )),
            ("log" | "progress", Some(id)) => self.reply(message::error(
                &id,
                error_code::INVALID_REQUEST,
                "log and progress are notifications",
                None,
            )),
            ("hello" | "complete" | "fail", None) => {
                run.note(&format!("{method} sent without an id was ignored"))
                    .await;
                Flow::Continue
            }
            (_, Some(id)) => self.reply(message::error(
                &id,
                error_code::METHOD_NOT_FOUND,
                "Method not found",
                None,
            )),
            // An unknown notification gets no answer (JSON-RPC 2.0).
            (_, None) => Flow::Continue,
        };
        self.after_reply(sent).await
    }

    /// Accepts the run's terminal message: recorded before it is answered,
    /// so the run's owner never misses it.
    fn terminal(&mut self, id: &Value, terminal: Terminal) -> Flow {
        let Some(link) = &self.link else {
            return Flow::Close;
        };
        if !link.run.accept_terminal(terminal.clone()) {
            return self.reply(message::error(
                id,
                error_code::TERMINAL,
                "A terminal message was already accepted",
                None,
            ));
        }
        // The channel holds every control event of a run.
        let _ = link.events.try_send(JjpEvent::Terminal(terminal));
        self.outbox.force(message::result(id, json!({})));
        Flow::Continue
    }

    /// Coalesces `progress` to one per interval: the latest replaces the one
    /// waiting.
    fn progress(&mut self, progress: super::JjpProgress) {
        let Some(link) = &self.link else {
            return;
        };
        let now = Instant::now();
        let ready = self
            .progress_last
            .is_none_or(|last| now >= last + self.host.config.progress_interval);
        if ready {
            link.run.progress.send_replace(Some(progress));
            self.progress_last = Some(now);
            self.progress_pending = None;
        } else {
            self.progress_pending = Some(progress);
        }
    }

    /// Queues a response; [`Flow::Close`] when the program left too much
    /// unread, which [`Connection::after_reply`] turns into a failure.
    fn reply(&mut self, bytes: Vec<u8>) -> Flow {
        if self.outbox.send(bytes) {
            Flow::Continue
        } else {
            Flow::Close
        }
    }

    async fn after_reply(&mut self, sent: Flow) -> Flow {
        match sent {
            Flow::Continue => Flow::Continue,
            Flow::Close => {
                self.outbox.force(message::error(
                    &Value::Null,
                    error_code::QUOTA,
                    "Quota exceeded",
                    None,
                ));
                self.fail(ProtocolProblem::Quota).await
            }
        }
    }

    /// Closes the connection. Before a terminal message the run fails with
    /// cause `Protocol`; after one, the run still ends with the program's
    /// exit and the problem is only noted.
    async fn fail(&mut self, problem: ProtocolProblem) -> Flow {
        if let Some(link) = &self.link {
            if link.run.record_problem(problem) {
                // The channel holds every control event of a run.
                let _ = link.events.try_send(JjpEvent::Failed(problem));
            }
            link.run.note(problem.note()).await;
            tracing::debug!(run = %link.run.run, problem = problem.as_str(), "jjp/1 connection closed");
        }
        Flow::Close
    }
}

async fn until_cancelled(token: Option<CancellationToken>) {
    match token {
        Some(token) => token.cancelled_owned().await,
        None => std::future::pending().await,
    }
}

/// Resolves when the run's owner asked the program to end.
async fn until_cancel_requested(cancel: &mut Option<watch::Receiver<bool>>) {
    match cancel {
        Some(receiver) => {
            if receiver.wait_for(|requested| *requested).await.is_err() {
                // The run is gone; it can no longer ask.
                std::future::pending::<()>().await;
            }
        }
        None => std::future::pending().await,
    }
}
