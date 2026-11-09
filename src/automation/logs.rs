//! External programs' logs on disk
//! ([ADR-0037](../../docs/adr/0037-program-logs-on-disk.md)): up to three
//! files per run in `automation/logs/<run-id>/` under the data folder —
//! `stdout.log`, `stderr.log` and, under `jjp/1`, `jjp.log` — each at most
//! [`LOG_LIMIT`] bytes, marker included.
//!
//! What a program prints is not under Studio's control and may hold values,
//! rows or tokens. It is written as received and read back only when the
//! user opens a run's log: it never reaches `tracing`, the application's log
//! files, telemetry, error reports or `automation.db`. Only sizes leave this
//! module, and no type here prints content in its `Debug` output.
use jykre_core::automation::RunId;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufWriter};

/// The most a log file holds, its final marker line included (ADR-0037).
pub const LOG_LIMIT: u64 = 1024 * 1024;

/// Room kept at the end of every file for the marker line, so the file never
/// exceeds [`LOG_LIMIT`]: a newline, `[jykre] `, a 20-digit count and the
/// longest wording take 68 bytes.
const MARKER_ROOM: u64 = 96;

/// The folder under the data folder that holds every run's logs.
const LOGS_FOLDER: [&str; 2] = ["automation", "logs"];

/// One of a run's log files.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Stream {
    Stdout,
    Stderr,
    /// `jjp/1` messages: the program's `log` notifications, its `fail`
    /// message and the host's notes about the protocol.
    Jjp,
}

impl Stream {
    pub const ALL: [Stream; 3] = [Stream::Stdout, Stream::Stderr, Stream::Jjp];

    /// The file's name inside the run's folder.
    pub fn file_name(self) -> &'static str {
        match self {
            Stream::Stdout => "stdout.log",
            Stream::Stderr => "stderr.log",
            Stream::Jjp => "jjp.log",
        }
    }
}

/// `automation/logs` under `data_dir`: one folder per run.
pub fn logs_root(data_dir: &Path) -> PathBuf {
    LOGS_FOLDER
        .iter()
        .fold(data_dir.to_path_buf(), |path, part| path.join(part))
}

/// The folder of one run's logs. A [`RunId`] is a UUID, so the name never
/// leaves [`logs_root`].
pub fn log_dir(data_dir: &Path, run: RunId) -> PathBuf {
    logs_root(data_dir).join(run.to_string())
}

/// The file of one stream of a run.
pub fn log_path(data_dir: &Path, run: RunId, stream: Stream) -> PathBuf {
    log_dir(data_dir, run).join(stream.file_name())
}

/// Removes a run's log folder (**Delete logs**, pruning). A folder that does
/// not exist is already deleted, so that is not an error.
pub async fn delete_logs(data_dir: &Path, run: RunId) -> io::Result<()> {
    match tokio::fs::remove_dir_all(log_dir(data_dir, run)).await {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

/// The runs that have a log folder, for pruning folders whose run no longer
/// exists. Entries whose name is not a run ID in its canonical form, and
/// anything that is not a folder, are ignored and never deleted by mistake.
/// No folder at all gives an empty list.
pub async fn log_runs(data_dir: &Path) -> io::Result<Vec<RunId>> {
    let mut entries = match tokio::fs::read_dir(logs_root(data_dir)).await {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let mut runs = Vec::new();
    while let Some(entry) = entries.next_entry().await? {
        if !entry.file_type().await?.is_dir() {
            continue;
        }
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        // UUIDs also parse from braced, URN and hyphen-less forms: only the
        // form `log_dir` writes names a folder this module owns.
        if let Ok(run) = name.parse::<RunId>()
            && run.to_string() == name
        {
            runs.push(run);
        }
    }
    Ok(runs)
}

/// A log read back for the user: the bytes decoded as text, never
/// interpreted ([`decode`]). Its `Debug` output shows sizes only.
#[derive(Clone, PartialEq, Eq)]
pub struct LogText {
    pub text: String,
    /// The file's size.
    pub bytes: u64,
    /// Whether `text` holds the whole file. A file written by [`LogWriter`]
    /// always fits; a larger one (changed by hand) is cut at [`LOG_LIMIT`].
    pub complete: bool,
}

impl fmt::Debug for LogText {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LogText")
            .field("bytes", &self.bytes)
            .field("complete", &self.complete)
            .finish_non_exhaustive()
    }
}

/// Reads one stream of a run, at most [`LOG_LIMIT`] bytes. `None` when the
/// run has no such file (it never started, its protocol has no `jjp.log`,
/// or its logs were deleted).
pub async fn read_log(data_dir: &Path, run: RunId, stream: Stream) -> io::Result<Option<LogText>> {
    let file = match tokio::fs::File::open(log_path(data_dir, run, stream)).await {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let bytes = file.metadata().await?.len();
    let mut content = Vec::with_capacity(usize::try_from(bytes.min(LOG_LIMIT)).unwrap_or(0));
    file.take(LOG_LIMIT).read_to_end(&mut content).await?;
    let complete = u64::try_from(content.len()).is_ok_and(|read| read >= bytes);
    Ok(Some(LogText {
        text: decode(&content, stream, legacy_code_page),
        bytes,
        complete,
    }))
}

/// Decodes a log for display (ADR-0037, amended 2026-11-09). Bytes on disk
/// are never changed. `jjp.log` is Studio's own UTF-8. A program's stdout or
/// stderr is read as UTF-8 when it is valid UTF-8 (a character cut by the
/// 1 MiB limit at the very end included); otherwise `legacy` decodes it in
/// the console's code page (on Windows, the OEM code page: CP437, CP850…),
/// and UTF-8 with replacement is the last resort.
fn decode(bytes: &[u8], stream: Stream, legacy: fn(&[u8]) -> Option<String>) -> String {
    let lossy = || String::from_utf8_lossy(bytes).into_owned();
    match std::str::from_utf8(bytes) {
        Ok(text) => text.to_owned(),
        // Only the last character is incomplete: the file was cut there.
        Err(error) if error.error_len().is_none() => lossy(),
        Err(_) if stream == Stream::Jjp => lossy(),
        Err(_) => legacy(bytes).unwrap_or_else(lossy),
    }
}

/// The console's code page decodes a program's output that is not UTF-8. On
/// Windows that is the OEM code page the registry names; elsewhere there is
/// none, and UTF-8 with replacement stays.
#[cfg(windows)]
fn legacy_code_page(bytes: &[u8]) -> Option<String> {
    static OEM: std::sync::OnceLock<Option<u16>> = std::sync::OnceLock::new();
    let page = (*OEM.get_or_init(oem_code_page))?;
    oem_cp::code_table::DECODING_TABLE_CP_MAP
        .get(&page)
        .map(|table| table.decode_string_lossy(bytes))
}

#[cfg(not(windows))]
fn legacy_code_page(_: &[u8]) -> Option<String> {
    None
}

/// The OEM code page the console uses, as Windows records it.
#[cfg(windows)]
fn oem_code_page() -> Option<u16> {
    windows_registry::LOCAL_MACHINE
        .open(r"SYSTEM\CurrentControlSet\Control\Nls\CodePage")
        .and_then(|key| key.get_string("OEMCP"))
        .ok()
        .and_then(|page| page.trim().parse().ok())
}

/// What a finished [`LogWriter`] left on disk.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LogSize {
    /// The file's size, marker included; at most [`LOG_LIMIT`].
    pub bytes: u64,
    /// Bytes read and discarded past the limit (stdout and stderr).
    pub dropped_bytes: u64,
    /// Whole messages discarded past the limit (`jjp.log`).
    pub dropped_messages: u64,
    /// Writing failed (a full disk, a removed folder): what came after the
    /// failure was discarded and counted as dropped.
    pub failed: bool,
}

/// Writes one log file within [`LOG_LIMIT`], however much arrives.
///
/// Writing never fails for the caller: past the limit, or after the disk
/// refused a write, bytes are counted and discarded, so a stream is always
/// drained ([`LogWriter::write`]). [`LogWriter::finish`] ends the file with
/// one marker line giving what was dropped.
pub struct LogWriter {
    file: Option<BufWriter<tokio::fs::File>>,
    run: RunId,
    stream: Stream,
    written: u64,
    dropped_bytes: u64,
    dropped_messages: u64,
    ends_with_newline: bool,
    failed: bool,
}

impl fmt::Debug for LogWriter {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LogWriter")
            .field("run", &self.run)
            .field("stream", &self.stream)
            .field("written", &self.written)
            .field("dropped_bytes", &self.dropped_bytes)
            .field("dropped_messages", &self.dropped_messages)
            .finish_non_exhaustive()
    }
}

impl LogWriter {
    /// Creates (or empties) the stream's file, and the run's folder. On
    /// Unix the folder and the file are readable by their owner only.
    pub async fn create(data_dir: &Path, run: RunId, stream: Stream) -> io::Result<Self> {
        let folder = log_dir(data_dir, run);
        let mut builder = tokio::fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        builder.mode(0o700);
        builder.create(&folder).await?;
        let mut options = tokio::fs::OpenOptions::new();
        options.create(true).write(true).truncate(true);
        #[cfg(unix)]
        options.mode(0o600);
        let file = options.open(folder.join(stream.file_name())).await?;
        Ok(Self {
            file: Some(BufWriter::new(file)),
            run,
            stream,
            written: 0,
            dropped_bytes: 0,
            dropped_messages: 0,
            ends_with_newline: true,
            failed: false,
        })
    }

    /// Bytes the file holds so far, without the marker.
    pub fn written(&self) -> u64 {
        self.written
    }

    /// What the content may still take before the marker's room.
    fn room(&self) -> u64 {
        (LOG_LIMIT - MARKER_ROOM).saturating_sub(self.written)
    }

    /// Writes `bytes` as received, up to the limit; the rest is counted and
    /// discarded. Used for stdout and stderr, where a line may be cut.
    pub async fn write(&mut self, bytes: &[u8]) {
        let kept = usize::try_from(self.room())
            .unwrap_or(usize::MAX)
            .min(bytes.len());
        let (head, rest) = bytes.split_at(kept);
        let dropped = u64::try_from(rest.len()).unwrap_or(u64::MAX);
        if !self.put(head).await {
            self.dropped_bytes = self
                .dropped_bytes
                .saturating_add(u64::try_from(head.len()).unwrap_or(u64::MAX));
        }
        self.dropped_bytes = self.dropped_bytes.saturating_add(dropped);
    }

    /// Writes one whole message, or none of it: a message that does not fit
    /// while leaving `keep_free` bytes of room is counted and discarded.
    /// Returns whether it was written. Used for `jjp.log`, where the room
    /// kept free guarantees that a `fail` message always fits.
    pub async fn write_message(&mut self, message: &[u8], keep_free: u64) -> bool {
        let size = u64::try_from(message.len()).unwrap_or(u64::MAX);
        if size.saturating_add(keep_free) > self.room() || !self.put(message).await {
            self.dropped_messages = self.dropped_messages.saturating_add(1);
            return false;
        }
        true
    }

    /// Writes within the room already checked; `false` when the disk refused
    /// it now or earlier.
    async fn put(&mut self, bytes: &[u8]) -> bool {
        if bytes.is_empty() {
            return !self.failed;
        }
        let Some(file) = self.file.as_mut() else {
            return false;
        };
        match file.write_all(bytes).await {
            Ok(()) => {
                self.written += u64::try_from(bytes.len()).unwrap_or(u64::MAX);
                self.ends_with_newline = bytes.last() == Some(&b'\n');
                true
            }
            Err(error) => {
                // The content is never logged, only that writing stopped.
                tracing::warn!(
                    run = %self.run,
                    stream = self.stream.file_name(),
                    error_kind = ?error.kind(),
                    "automation log write failed; the rest of the stream is discarded"
                );
                self.file = None;
                self.failed = true;
                false
            }
        }
    }

    /// Ends the file with the marker line when anything was dropped, and
    /// flushes it.
    pub async fn finish(mut self) -> LogSize {
        let marker = if self.dropped_messages > 0 {
            Some(format!(
                "[jykre] {} messages dropped past the 1 MiB limit\n",
                self.dropped_messages
            ))
        } else if self.dropped_bytes > 0 {
            Some(format!(
                "[jykre] {} bytes dropped past the 1 MiB limit\n",
                self.dropped_bytes
            ))
        } else {
            None
        };
        if let Some(file) = self.file.as_mut() {
            let mut ending = Vec::new();
            if let Some(marker) = marker {
                if !self.ends_with_newline {
                    ending.push(b'\n');
                }
                ending.extend_from_slice(marker.as_bytes());
            }
            let flushed = async {
                file.write_all(&ending).await?;
                file.flush().await
            }
            .await;
            match flushed {
                Ok(()) => self.written += u64::try_from(ending.len()).unwrap_or(0),
                Err(error) => {
                    tracing::warn!(
                        run = %self.run,
                        stream = self.stream.file_name(),
                        error_kind = ?error.kind(),
                        "automation log could not be finished"
                    );
                    self.failed = true;
                }
            }
        }
        LogSize {
            bytes: self.written,
            dropped_bytes: self.dropped_bytes,
            dropped_messages: self.dropped_messages,
            failed: self.failed,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_legacy(_: &[u8]) -> Option<String> {
        None
    }

    fn latin1(bytes: &[u8]) -> Option<String> {
        Some(bytes.iter().map(|&byte| char::from(byte)).collect())
    }

    #[test]
    fn valid_utf8_is_shown_as_written_and_never_through_the_code_page() {
        let text = "ação → 100 %".as_bytes();
        assert_eq!(decode(text, Stream::Stdout, latin1), "ação → 100 %");
        // Cut by the 1 MiB limit inside its last character: still UTF-8.
        let cut = "ação →".as_bytes();
        let cut = &cut[..cut.len() - 1];
        assert_eq!(decode(cut, Stream::Stderr, latin1), "ação \u{fffd}");
    }

    #[test]
    fn output_that_is_not_utf8_is_read_in_the_console_code_page() {
        // "Média" in a single-byte code page: 0xE9 alone is not UTF-8.
        let bytes = b"M\xe9dia";
        assert_eq!(decode(bytes, Stream::Stdout, latin1), "Média");
        // Without a code page, UTF-8 with replacement stays.
        assert_eq!(decode(bytes, Stream::Stdout, no_legacy), "M\u{fffd}dia");
        // Studio's own jjp.log is always UTF-8.
        assert_eq!(decode(bytes, Stream::Jjp, latin1), "M\u{fffd}dia");
    }

    #[cfg(windows)]
    #[test]
    fn windows_names_its_oem_code_page_and_decodes_it() {
        let page = oem_code_page().expect("Windows records an OEM code page");
        let decoded = legacy_code_page(b"M\x82dia");
        assert!(decoded.is_some(), "no decoding table for CP{page}");
        if matches!(page, 437 | 850 | 858 | 860 | 863 | 865) {
            // 0x82 is "é" in these pages, as `ping` writes it on pt-BR.
            assert_eq!(decoded.as_deref(), Some("Média"));
        }
    }

    pub(crate) fn temp_data_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("jykre-automation-{label}-{}", RunId::new()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[tokio::test]
    async fn a_stream_past_the_limit_is_cut_and_ends_with_the_bytes_dropped() {
        let data = temp_data_dir("logs-cut");
        let run = RunId::new();
        let mut writer = LogWriter::create(&data, run, Stream::Stdout).await.unwrap();
        let chunk = vec![b'x'; 64 * 1024];
        let total: u64 = 40 * 64 * 1024;
        for _ in 0..40 {
            writer.write(&chunk).await;
        }
        let size = writer.finish().await;
        let on_disk = std::fs::read(log_path(&data, run, Stream::Stdout)).unwrap();
        assert_eq!(size.bytes, on_disk.len() as u64);
        assert!(size.bytes <= LOG_LIMIT);
        let kept = LOG_LIMIT - MARKER_ROOM;
        assert_eq!(size.dropped_bytes, total - kept);
        let text = String::from_utf8(on_disk).unwrap();
        let marker = format!(
            "\n[jykre] {} bytes dropped past the 1 MiB limit\n",
            total - kept
        );
        assert!(text.ends_with(&marker), "marker missing");
        assert_eq!(text.len() as u64, kept + marker.len() as u64);
        std::fs::remove_dir_all(&data).unwrap();
    }

    #[tokio::test]
    async fn a_stream_within_the_limit_is_kept_as_received_without_a_marker() {
        let data = temp_data_dir("logs-small");
        let run = RunId::new();
        let mut writer = LogWriter::create(&data, run, Stream::Stderr).await.unwrap();
        // Invalid UTF-8 and a missing final newline are kept byte for byte.
        writer.write(b"line one\n\xff\xfe partial").await;
        let size = writer.finish().await;
        assert_eq!(size.dropped_bytes, 0);
        assert_eq!(
            std::fs::read(log_path(&data, run, Stream::Stderr)).unwrap(),
            b"line one\n\xff\xfe partial"
        );
        let read = read_log(&data, run, Stream::Stderr).await.unwrap().unwrap();
        // Not UTF-8, so shown in the console's code page where there is one
        // (Windows), with replacement otherwise; the file is unchanged.
        let expected = legacy_code_page(b"line one\n\xff\xfe partial")
            .unwrap_or_else(|| "line one\n\u{fffd}\u{fffd} partial".to_owned());
        assert_eq!(read.text, expected);
        #[cfg(not(windows))]
        assert_eq!(read.text, "line one\n\u{fffd}\u{fffd} partial");
        assert!(read.complete);
        assert!(!format!("{read:?}").contains("line one"));
        assert_eq!(read_log(&data, run, Stream::Jjp).await.unwrap(), None);
        std::fs::remove_dir_all(&data).unwrap();
    }

    #[tokio::test]
    async fn messages_are_whole_or_dropped_and_counted_with_room_kept_free() {
        let data = temp_data_dir("logs-messages");
        let run = RunId::new();
        let mut writer = LogWriter::create(&data, run, Stream::Jjp).await.unwrap();
        let message = vec![b'm'; 1000];
        let keep_free = 4096;
        let mut written = 0;
        while writer.write_message(&message, keep_free).await {
            written += 1;
        }
        assert!(!writer.write_message(&message, keep_free).await);
        // The room kept free still takes a message that needs it.
        assert!(writer.write_message(b"[fail x] last words\n", 0).await);
        let size = writer.finish().await;
        assert_eq!(size.dropped_messages, 2);
        assert!(size.bytes <= LOG_LIMIT);
        let text = std::fs::read_to_string(log_path(&data, run, Stream::Jjp)).unwrap();
        let (kept, rest) = text.split_at(written * 1000);
        assert!(kept.bytes().all(|byte| byte == b'm'));
        assert_eq!(
            rest,
            "[fail x] last words\n[jykre] 2 messages dropped past the 1 MiB limit\n"
        );
        std::fs::remove_dir_all(&data).unwrap();
    }

    #[tokio::test]
    async fn runs_are_listed_from_their_folders_and_deleted() {
        let data = temp_data_dir("logs-list");
        assert_eq!(log_runs(&data).await.unwrap(), Vec::new());
        let first = RunId::new();
        let second = RunId::new();
        for run in [first, second] {
            LogWriter::create(&data, run, Stream::Stdout)
                .await
                .unwrap()
                .finish()
                .await;
        }
        // Not run IDs, or not in canonical form, or not folders: ignored.
        let root = logs_root(&data);
        std::fs::create_dir_all(root.join("not-a-run")).unwrap();
        std::fs::create_dir_all(root.join(first.to_string().replace('-', ""))).unwrap();
        std::fs::write(root.join(RunId::new().to_string()), b"file").unwrap();
        let mut listed = log_runs(&data).await.unwrap();
        listed.sort();
        let mut expected = vec![first, second];
        expected.sort();
        assert_eq!(listed, expected);

        delete_logs(&data, first).await.unwrap();
        assert!(!log_dir(&data, first).exists());
        delete_logs(&data, first).await.unwrap();
        assert_eq!(log_runs(&data).await.unwrap(), vec![second]);
        std::fs::remove_dir_all(&data).unwrap();
    }
}
