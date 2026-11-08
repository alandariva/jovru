//! Saved queries as plain `.sql` files (0.2.0 M7,
//! [ADR-0031](../../docs/adr/0031-saved-queries-as-sql-files.md)).
//!
//! The files hold only SQL: a query's title is its file name, and its tags
//! and connection live in the local [`library`] state. Everything here is
//! file work for the runtime's blocking pool: it knows nothing of GPUI and
//! names no session, driver or query, so opening, discovering, migrating or
//! recovering files cannot run SQL (`cargo xtask check` enforces it).
use crate::tr;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

pub mod library;
pub mod migration;
pub mod recovery;
pub mod scan;
pub mod tabs;

/// The largest file (or recovery file) opened into a tab.
pub const FILE_LIMIT: u64 = 4 * 1024 * 1024;

/// Why a file could not be read or written. Messages never name a path or
/// hold the file's text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FileError {
    /// The file is not there (moved, renamed or deleted).
    Missing,
    /// Larger than [`FILE_LIMIT`].
    TooLarge { size: u64 },
    /// Not UTF-8 text (a UTF-16 or UTF-32 byte order mark, or invalid
    /// bytes).
    NotUtf8,
    /// The destination already exists and was not to be replaced.
    Exists,
    /// The operating system refused; its message.
    Io(String),
    /// The work was cancelled.
    Cancelled,
}

impl FileError {
    fn io(error: &std::io::Error) -> Self {
        match error.kind() {
            std::io::ErrorKind::NotFound => Self::Missing,
            std::io::ErrorKind::AlreadyExists => Self::Exists,
            _ => Self::Io(error.to_string()),
        }
    }

    /// The error in words, for a notice.
    pub fn message(&self) -> String {
        match self {
            Self::Missing => tr!("The file is not there: it was moved, renamed or deleted.").into(),
            Self::TooLarge { size } => tr!(
                "The file has {0}; files above {1} are not opened, and nothing is cut to fit.",
                crate::cell_format::size(*size),
                crate::cell_format::size(FILE_LIMIT)
            ),
            Self::NotUtf8 => tr!(
                "The file is not UTF-8 text. Save it as UTF-8 in another editor to open it here."
            )
            .into(),
            Self::Exists => tr!("A file with that name already exists.").into(),
            Self::Io(message) => tr!("The system refused: {0}", message),
            Self::Cancelled => tr!("Cancelled.").into(),
        }
    }
}

/// How a file's text is written back: the line endings and byte order mark
/// it was read with.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TextFormat {
    /// Lines end with `\r\n` (the style of the file's first line break).
    pub crlf: bool,
    /// The file starts with a UTF-8 byte order mark.
    pub bom: bool,
}

const UTF8_BOM: &[u8] = &[0xEF, 0xBB, 0xBF];

/// A file's bytes as the editor's text (`\n` line endings only) and how to
/// write them back. Other byte order marks and invalid UTF-8 are refused.
pub fn decode(bytes: &[u8]) -> Result<(String, TextFormat), FileError> {
    let (bom, body) = match bytes.strip_prefix(UTF8_BOM) {
        Some(body) => (true, body),
        None => (false, bytes),
    };
    if !bom
        && (bytes.starts_with(&[0xFF, 0xFE])
            || bytes.starts_with(&[0xFE, 0xFF])
            || bytes.starts_with(&[0x00, 0x00, 0xFE, 0xFF]))
    {
        return Err(FileError::NotUtf8);
    }
    let text = std::str::from_utf8(body).map_err(|_| FileError::NotUtf8)?;
    let crlf = text
        .find('\n')
        .is_some_and(|at| at > 0 && text.as_bytes()[at - 1] == b'\r');
    Ok((text.replace("\r\n", "\n"), TextFormat { crlf, bom }))
}

/// The editor's text as the file's bytes, in `format`.
pub fn encode(text: &str, format: TextFormat) -> Vec<u8> {
    let text = text.replace("\r\n", "\n");
    let text = if format.crlf {
        text.replace('\n', "\r\n")
    } else {
        text
    };
    let mut bytes = Vec::with_capacity(text.len() + 3);
    if format.bom {
        bytes.extend_from_slice(UTF8_BOM);
    }
    bytes.extend_from_slice(text.as_bytes());
    bytes
}

/// What a file was loaded or last saved from, to notice a change on disk
/// before writing over it: its size, modification time and a 64-bit FNV-1a
/// digest of its bytes (a change detector, not a security measure).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Fingerprint {
    pub size: u64,
    /// Nanoseconds since the Unix epoch, when the system tells.
    pub modified: Option<u128>,
    pub digest: u64,
}

impl Fingerprint {
    fn of(bytes: &[u8], metadata: &fs::Metadata) -> Self {
        Self {
            size: bytes.len() as u64,
            modified: modified(metadata),
            digest: digest(bytes),
        }
    }

    pub fn to_value(&self) -> serde_json::Value {
        serde_json::json!({
            "size": self.size,
            // As text: JSON numbers do not hold 128 bits.
            "modified": self.modified.map(|nanos| nanos.to_string()),
            "digest": format!("{:016x}", self.digest),
        })
    }

    pub fn from_value(value: &serde_json::Value) -> Option<Self> {
        Some(Self {
            size: value["size"].as_u64()?,
            modified: value["modified"]
                .as_str()
                .and_then(|text| text.parse().ok()),
            digest: u64::from_str_radix(value["digest"].as_str()?, 16).ok()?,
        })
    }
}

fn modified(metadata: &fs::Metadata) -> Option<u128> {
    metadata
        .modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|elapsed| elapsed.as_nanos())
}

/// 64-bit FNV-1a.
pub fn digest(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x0100_0000_01b3)
    })
}

/// A file read into a tab.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Loaded {
    pub text: String,
    pub format: TextFormat,
    pub base: Fingerprint,
}

/// Reads `path` within [`FILE_LIMIT`], as UTF-8. Never writes.
pub fn read(path: &Path) -> Result<Loaded, FileError> {
    let metadata = fs::metadata(path).map_err(|error| FileError::io(&error))?;
    if metadata.len() > FILE_LIMIT {
        return Err(FileError::TooLarge {
            size: metadata.len(),
        });
    }
    let bytes = fs::read(path).map_err(|error| FileError::io(&error))?;
    if bytes.len() as u64 > FILE_LIMIT {
        return Err(FileError::TooLarge {
            size: bytes.len() as u64,
        });
    }
    let (text, format) = decode(&bytes)?;
    Ok(Loaded {
        text,
        format,
        base: Fingerprint::of(&bytes, &metadata),
    })
}

/// How a file on disk compares with what a tab loaded or saved.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OnDisk {
    Same,
    /// Someone else wrote it meanwhile.
    Changed,
    Missing,
}

/// Compares `path` with `base`: the same size and time is the same file;
/// otherwise its bytes are read and compared by digest, so a file saved
/// again with the same text is not a change.
pub fn check(path: &Path, base: &Fingerprint) -> Result<OnDisk, FileError> {
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(OnDisk::Missing),
        Err(error) => return Err(FileError::io(&error)),
    };
    if metadata.len() == base.size && modified(&metadata) == base.modified {
        return Ok(OnDisk::Same);
    }
    if metadata.len() != base.size {
        return Ok(OnDisk::Changed);
    }
    let bytes = fs::read(path).map_err(|error| FileError::io(&error))?;
    Ok(if digest(&bytes) == base.digest {
        OnDisk::Same
    } else {
        OnDisk::Changed
    })
}

/// A temporary file next to `target`, created with `create_new` so it never
/// reuses a file that happens to have the name.
fn temporary(target: &Path, bytes: &[u8]) -> Result<PathBuf, FileError> {
    let folder = target.parent().unwrap_or(Path::new("."));
    let stem = target
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let temp = folder.join(format!(".{stem}.{}.tmp", uuid::Uuid::new_v4().simple()));
    let written = (|| {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)?;
        file.write_all(bytes)?;
        file.flush()?;
        file.sync_all()
    })();
    match written {
        Ok(()) => Ok(temp),
        Err(error) => {
            // Best effort: a partial temporary file holds no complete text.
            let _ = fs::remove_file(&temp);
            Err(FileError::io(&error))
        }
    }
}

/// Writes `text` to `path` in `format`: into a temporary file next to it,
/// flushed and synced, then renamed over it, so a failure leaves an existing
/// file as it was. With `replace` false an existing `path` is refused
/// ([`FileError::Exists`]). Returns what the file now is.
pub fn write(
    path: &Path,
    text: &str,
    format: TextFormat,
    replace: bool,
) -> Result<Fingerprint, FileError> {
    let bytes = encode(text, format);
    if !replace && fs::symlink_metadata(path).is_ok() {
        return Err(FileError::Exists);
    }
    let temp = temporary(path, &bytes)?;
    if !replace && fs::symlink_metadata(path).is_ok() {
        let _ = fs::remove_file(&temp);
        return Err(FileError::Exists);
    }
    if let Err(error) = fs::rename(&temp, path) {
        let _ = fs::remove_file(&temp);
        return Err(FileError::io(&error));
    }
    let metadata = fs::metadata(path).map_err(|error| FileError::io(&error))?;
    Ok(Fingerprint::of(&bytes, &metadata))
}

/// Whether `a` and `b` name the same existing file (a change of case on a
/// system that ignores it).
fn same_file(a: &Path, b: &Path) -> bool {
    match (fs::canonicalize(a), fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// Renames `from` to `to`, never over another file: `to` is checked right
/// before the rename, which is refused when taken, unless both name the same
/// file (a change of case alone).
pub fn rename(from: &Path, to: &Path) -> Result<(), FileError> {
    if fs::symlink_metadata(from).is_err() {
        return Err(FileError::Missing);
    }
    if fs::symlink_metadata(to).is_ok() && !same_file(from, to) {
        return Err(FileError::Exists);
    }
    fs::rename(from, to).map_err(|error| FileError::io(&error))
}

/// Names for query files.
pub mod names {
    use std::path::{Path, PathBuf};

    /// `title` made safe as a file name (without extension): characters
    /// Windows forbids become `_`, a reserved device name gets a `_` in
    /// front, trailing dots and spaces go, and it is cut at 120 characters;
    /// blank, it is `fallback`.
    pub fn safe_stem(title: &str, fallback: &str) -> String {
        let cleaned: String = title
            .chars()
            .map(|ch| {
                if ch.is_control()
                    || matches!(ch, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*')
                {
                    '_'
                } else {
                    ch
                }
            })
            .collect();
        let trimmed = cleaned.trim().trim_end_matches(['.', ' ']);
        let mut stem: String = if trimmed.is_empty() {
            fallback.to_owned()
        } else {
            trimmed.chars().take(120).collect()
        };
        let upper = stem.to_ascii_uppercase();
        let device = upper.split('.').next().unwrap_or_default();
        let reserved = matches!(device, "CON" | "PRN" | "AUX" | "NUL")
            || (device.len() == 4
                && (device.starts_with("COM") || device.starts_with("LPT"))
                && device.ends_with(|ch: char| ch.is_ascii_digit() && ch != '0'));
        if reserved {
            stem.insert(0, '_');
        }
        stem
    }

    /// A query's file name for `title`.
    pub fn file_name(title: &str) -> String {
        format!("{}.sql", safe_stem(title, "Query"))
    }

    /// A query's title: its file name without the extension.
    pub fn title_of(path: &Path) -> String {
        path.file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
            .unwrap_or_default()
    }

    /// Whether `path` ends in `.sql`, in any case.
    pub fn is_sql(path: &Path) -> bool {
        path.extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("sql"))
    }

    /// `path` with `.sql` added when it has no extension, and whether it
    /// was added.
    pub fn with_sql_extension(path: &Path) -> (PathBuf, bool) {
        if path.extension().is_some() {
            (path.to_path_buf(), false)
        } else {
            let mut name = path.as_os_str().to_owned();
            name.push(".sql");
            (PathBuf::from(name), true)
        }
    }

    /// `<title>.sql` in `folder`, or `<title> (2).sql`, `(3)`… when `taken`
    /// says the name is used.
    pub fn free(folder: &Path, title: &str, taken: &dyn Fn(&Path) -> bool) -> PathBuf {
        let stem = safe_stem(title, "Query");
        let first = folder.join(format!("{stem}.sql"));
        if !taken(&first) {
            return first;
        }
        (2..)
            .map(|n| folder.join(format!("{stem} ({n}).sql")))
            .find(|path| !taken(path))
            .unwrap_or(first)
    }

    /// Whether two paths name the same file as the system compares names
    /// (ignoring case on Windows).
    pub fn same_path(a: &Path, b: &Path) -> bool {
        // Component by component, so `/` and `\` separate alike.
        let parts = |path: &Path| -> Vec<String> {
            path.components()
                .map(|part| {
                    let part = part.as_os_str().to_string_lossy();
                    if cfg!(windows) {
                        part.to_lowercase()
                    } else {
                        part.into_owned()
                    }
                })
                .collect()
        };
        parts(a) == parts(b)
    }
}

#[cfg(test)]
pub(crate) mod testing {
    use std::path::{Path, PathBuf};

    /// A folder of its own under the system's temporary folder, removed
    /// when dropped.
    pub struct Folder(pub PathBuf);

    impl Folder {
        pub fn new() -> Self {
            let path =
                std::env::temp_dir().join(format!("jykre-query-files-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        pub fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for Folder {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::Folder;
    use super::*;

    #[test]
    fn text_keeps_its_line_endings_and_byte_order_mark() {
        let (text, format) = decode(b"\xEF\xBB\xBFSELECT 1;\r\nSELECT 2;\r\n").unwrap();
        assert_eq!(text, "SELECT 1;\nSELECT 2;\n");
        assert_eq!(
            format,
            TextFormat {
                crlf: true,
                bom: true
            }
        );
        assert_eq!(
            encode(&text, format),
            b"\xEF\xBB\xBFSELECT 1;\r\nSELECT 2;\r\n"
        );
        let (text, format) = decode("SELECT 'é';\n".as_bytes()).unwrap();
        assert_eq!(format, TextFormat::default());
        assert_eq!(encode(&text, format), "SELECT 'é';\n".as_bytes());
        // Mixed endings are written in the style of the first break.
        let (text, format) = decode(b"a\nb\r\nc").unwrap();
        assert_eq!(encode(&text, format), b"a\nb\nc");
        // Other encodings replace nothing: they are refused.
        assert_eq!(decode(b"\xFF\xFEa\x00").unwrap_err(), FileError::NotUtf8);
        assert_eq!(decode(b"\xFE\xFF\x00a").unwrap_err(), FileError::NotUtf8);
        assert_eq!(decode(b"caf\xE9").unwrap_err(), FileError::NotUtf8);
    }

    #[test]
    fn writing_is_atomic_and_notices_changes_on_disk() {
        let folder = Folder::new();
        let path = folder.path().join("orders.sql");
        let base = write(&path, "SELECT 1;\n", TextFormat::default(), false).unwrap();
        assert_eq!(check(&path, &base).unwrap(), OnDisk::Same);
        // A new file never replaces one that exists.
        assert_eq!(
            write(&path, "SELECT 2;\n", TextFormat::default(), false).unwrap_err(),
            FileError::Exists
        );
        assert_eq!(read(&path).unwrap().text, "SELECT 1;\n");
        // Someone else writes it: the change is seen before writing over it.
        std::thread::sleep(std::time::Duration::from_millis(20));
        fs::write(&path, "SELECT 3;\n").unwrap();
        assert_eq!(check(&path, &base).unwrap(), OnDisk::Changed);
        let loaded = read(&path).unwrap();
        assert_eq!(loaded.text, "SELECT 3;\n");
        let saved = write(&path, "SELECT 4;\n", loaded.format, true).unwrap();
        assert_eq!(check(&path, &saved).unwrap(), OnDisk::Same);
        // No temporary file is left behind.
        assert_eq!(fs::read_dir(folder.path()).unwrap().count(), 1);
        fs::remove_file(&path).unwrap();
        assert_eq!(check(&path, &saved).unwrap(), OnDisk::Missing);
        assert_eq!(read(&path).unwrap_err(), FileError::Missing);
        // A write into a folder that does not exist fails and creates nothing.
        let nowhere = folder.path().join("missing").join("a.sql");
        assert!(write(&nowhere, "x", TextFormat::default(), true).is_err());
        let round = Fingerprint::from_value(&saved.to_value()).unwrap();
        assert_eq!(round, saved);
    }

    #[test]
    fn files_over_the_limit_are_refused_whole() {
        let folder = Folder::new();
        let path = folder.path().join("big.sql");
        fs::write(&path, vec![b'x'; FILE_LIMIT as usize + 1]).unwrap();
        assert_eq!(
            read(&path).unwrap_err(),
            FileError::TooLarge {
                size: FILE_LIMIT + 1
            }
        );
    }

    #[test]
    fn renaming_never_replaces_another_file() {
        let folder = Folder::new();
        let a = folder.path().join("a.sql");
        let b = folder.path().join("b.sql");
        fs::write(&a, "A").unwrap();
        fs::write(&b, "B").unwrap();
        assert_eq!(rename(&a, &b).unwrap_err(), FileError::Exists);
        assert_eq!(fs::read_to_string(&b).unwrap(), "B");
        let c = folder.path().join("c.sql");
        rename(&a, &c).unwrap();
        assert_eq!(fs::read_to_string(&c).unwrap(), "A");
        assert_eq!(rename(&a, &c).unwrap_err(), FileError::Missing);
        // A change of case alone is allowed.
        let upper = folder.path().join("C.sql");
        rename(&c, &upper).unwrap();
        assert!(upper.exists());
    }

    #[test]
    fn names_are_safe_and_never_collide() {
        use names::*;
        assert_eq!(file_name("Sales: Q1/Q2?"), "Sales_ Q1_Q2_.sql");
        assert_eq!(file_name("  "), "Query.sql");
        assert_eq!(file_name("con"), "_con.sql");
        assert_eq!(file_name("report. "), "report.sql");
        assert_eq!(title_of(Path::new("x/Monthly sales.sql")), "Monthly sales");
        assert!(is_sql(Path::new("a.SQL")) && !is_sql(Path::new("a.txt")));
        assert_eq!(
            with_sql_extension(Path::new("dir/orders")),
            (PathBuf::from("dir/orders.sql"), true)
        );
        assert_eq!(
            with_sql_extension(Path::new("dir/orders.txt")),
            (PathBuf::from("dir/orders.txt"), false)
        );
        let taken = [
            PathBuf::from("q/orders.sql"),
            PathBuf::from("q/orders (2).sql"),
        ];
        let free_name = free(Path::new("q"), "orders", &|path| {
            taken.iter().any(|taken| same_path(taken, path))
        });
        assert_eq!(free_name, PathBuf::from("q/orders (3).sql"));
    }
}
