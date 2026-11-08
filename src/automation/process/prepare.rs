//! Before a program runs (specification, sections 9.1.2–9.1.4): the
//! program, the entrypoint and its hash, the working directory and the
//! environment. Every function here touches only the file system (no
//! process starts) and blocks, so async callers run them in
//! `spawn_blocking`.
use crate::automation::definition::{
    EnvironmentSpec, OutputSpec, ProcessBody, Protocol, is_shell_script,
};
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::io::{self, Read};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

/// Names never inherited from Studio's environment (section 9.1.3); a
/// trailing `*` matches any name with that prefix. Matched without regard
/// to case on Windows, whose names ignore it. The program editor lists them.
pub const ENV_DENYLIST: &[&str] = &[
    "JYKRE_*",
    "OTEL_*",
    "NODE_OPTIONS",
    "PYTHONPATH",
    "PYTHONSTARTUP",
    "PYTHONHOME",
];

/// Where a `jjp/1` program finds the host (`127.0.0.1:<port>`); added after
/// the denylist, the job's `unset` and its `set`.
pub const JJP_ADDRESS_VARIABLE: &str = "JYKRE_JJP_ADDRESS";

/// Why a run is refused before admission: nothing runs and no history is
/// written (section 4.3). Paths name the user's own files, for the message
/// in the form; they are never logged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refused {
    /// No program.
    NoProgram,
    /// A `.bat` or `.cmd` file, which Windows runs through `cmd.exe`.
    ShellScript,
    /// The program is neither an absolute path nor a bare name to look up on
    /// `PATH` (`bin/tool`, `./tool`).
    ProgramPath,
    /// The entrypoint is relative and there is no absolute working directory
    /// to read it from.
    EntrypointPath { path: PathBuf },
    /// The working directory is relative, does not exist or is not a folder.
    WorkingDirectory { path: PathBuf },
    /// A published version's file is missing or its SHA-256 changed (Q-16);
    /// the interface offers **Publish**.
    ProgramChanged { path: PathBuf },
    /// A published version's file could not be read to check it.
    EntrypointUnreadable { path: PathBuf },
}

/// Why a program could not start. The run fails without `effect_unknown`,
/// since nothing ran.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartError {
    /// The program, or the entrypoint, does not exist.
    NotFound,
    /// The program is not executable, or the system refused to start it.
    PermissionDenied,
    /// The run's log files could not be created.
    Logs,
    /// The `jjp/1` host could not take the run: its `jjp.log`, a random
    /// token or the listening port failed.
    Listener,
    /// The system refused to start it for another reason.
    Other,
}

impl StartError {
    pub(crate) fn of(error: &io::Error) -> Self {
        match error.kind() {
            io::ErrorKind::NotFound => StartError::NotFound,
            io::ErrorKind::PermissionDenied => StartError::PermissionDenied,
            _ => StartError::Other,
        }
    }
}

/// A program ready to start: resolved, checked and hashed when prepared.
/// `Debug` shows the environment's names, never its values.
#[derive(Clone, PartialEq)]
pub struct Launch {
    /// Absolute.
    pub program: PathBuf,
    /// Passed as a vector, never through a shell.
    pub arguments: Vec<String>,
    pub working_directory: PathBuf,
    /// The file a version pins: the script an interpreter runs, or the
    /// program itself.
    pub entrypoint: PathBuf,
    /// `sha256:` and the hex of the entrypoint when it was prepared; `None`
    /// when it could not be read (an unpublished job still runs).
    pub fingerprint: Option<String>,
    /// Studio's environment minus the denylist and `unset`, plus `set`;
    /// [`super::run`] adds [`JJP_ADDRESS_VARIABLE`] under `jjp/1`.
    pub environment: Vec<(OsString, OsString)>,
    pub protocol: Protocol,
    /// The outputs `complete` must carry, under `jjp/1`.
    pub outputs: Vec<OutputSpec>,
}

impl fmt::Debug for Launch {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let names: Vec<_> = self.environment.iter().map(|(name, _)| name).collect();
        formatter
            .debug_struct("Launch")
            .field("program", &self.program)
            .field("arguments", &self.arguments.len())
            .field("working_directory", &self.working_directory)
            .field("entrypoint", &self.entrypoint)
            .field("fingerprint", &self.fingerprint)
            .field("environment", &names)
            .field("protocol", &self.protocol)
            .field("outputs", &self.outputs)
            .finish()
    }
}

/// What [`prepare`] found: a program to start, or one that cannot start,
/// whose run fails with `NotFound` or `PermissionDenied` (section 9.1.5).
#[derive(Debug, Clone, PartialEq)]
pub enum Prepared {
    Ready(Launch),
    CannotStart {
        error: StartError,
        protocol: Protocol,
    },
}

/// Checks a program job against the files as they are now, before its run
/// is admitted. `base` is Studio's environment (`std::env::vars_os()`).
///
/// Refuses ([`Refused`]) a definition that cannot run as written: no
/// program, a `.bat`/`.cmd`, a relative program path, a working directory
/// that does not exist, or — for a published version ([`ProcessBody::pinned`])
/// — an entrypoint that is missing or whose SHA-256 changed. A program not
/// found on `PATH`, or not executable, is not refused: it gives
/// [`Prepared::CannotStart`]. Blocks on the file system.
pub fn prepare(
    body: &ProcessBody,
    base: impl IntoIterator<Item = (OsString, OsString)>,
) -> Result<Prepared, Refused> {
    let program = body.program.trim();
    if program.is_empty() {
        return Err(Refused::NoProgram);
    }
    if is_shell_script(program) {
        return Err(Refused::ShellScript);
    }
    let environment = program_environment(base, &body.environment, None);
    let lookup = |name: &str| {
        environment
            .iter()
            .find(|(existing, _)| same_name(existing, name, cfg!(windows)))
            .map(|(_, value)| value.as_os_str())
    };
    let resolved = if Path::new(program).is_absolute() {
        Ok(PathBuf::from(program))
    } else if program.contains(['/', '\\']) {
        return Err(Refused::ProgramPath);
    } else {
        resolve_program(program, lookup("PATH"), lookup("PATHEXT"))
    };
    if let Ok(path) = &resolved
        && is_shell_script(&path.to_string_lossy())
    {
        return Err(Refused::ShellScript);
    }

    let explicit_directory = body.working_directory.as_deref().map(str::trim);
    if let Some(directory) = explicit_directory
        && !Path::new(directory).is_absolute()
    {
        return Err(Refused::WorkingDirectory {
            path: PathBuf::from(directory),
        });
    }
    let entrypoint = match body.entrypoint.as_deref().map(str::trim) {
        Some(entry) if Path::new(entry).is_absolute() => Some(PathBuf::from(entry)),
        Some(entry) => match explicit_directory {
            Some(directory) => Some(Path::new(directory).join(entry)),
            None => {
                return Err(Refused::EntrypointPath {
                    path: PathBuf::from(entry),
                });
            }
        },
        None => resolved.as_ref().ok().cloned(),
    };

    // A folder the job names is checked first: it costs nothing, while
    // hashing a large program does.
    if let Some(directory) = explicit_directory
        && !Path::new(directory).is_dir()
    {
        return Err(Refused::WorkingDirectory {
            path: PathBuf::from(directory),
        });
    }
    let cannot_start = |error| {
        Ok(Prepared::CannotStart {
            error,
            protocol: body.protocol,
        })
    };
    let entrypoint = match (entrypoint, &body.pinned) {
        (Some(entry), _) if entry.is_file() => entry,
        // A published version's file must be there; the interface offers
        // to publish again.
        (Some(entry), Some(_)) => return Err(Refused::ProgramChanged { path: entry }),
        (None, Some(pinned)) => {
            return Err(Refused::ProgramChanged {
                path: PathBuf::from(&pinned.path),
            });
        }
        // The program itself was not found: its own error says why.
        (None, None) => {
            return match resolved {
                Err(error) => cannot_start(error),
                Ok(_) => cannot_start(StartError::NotFound),
            };
        }
        (Some(_), None) => return cannot_start(StartError::NotFound),
    };
    let working_directory = match explicit_directory {
        Some(directory) => PathBuf::from(directory),
        None => match entrypoint.parent() {
            Some(parent) if parent.is_dir() => parent.to_path_buf(),
            _ => {
                return Err(Refused::WorkingDirectory {
                    path: entrypoint
                        .parent()
                        .map(Path::to_path_buf)
                        .unwrap_or_default(),
                });
            }
        },
    };
    let fingerprint = match (&body.pinned, fingerprint_file(&entrypoint)) {
        (Some(pinned), Ok(found)) if found == pinned.sha256 => Some(found),
        (Some(_), Ok(_)) => return Err(Refused::ProgramChanged { path: entrypoint }),
        (Some(_), Err(error)) if error.kind() == io::ErrorKind::NotFound => {
            return Err(Refused::ProgramChanged { path: entrypoint });
        }
        (Some(_), Err(_)) => return Err(Refused::EntrypointUnreadable { path: entrypoint }),
        (None, Ok(found)) => Some(found),
        // An unpublished job hashes its file only to record it: one it
        // cannot read still runs, and the program reports the problem.
        (None, Err(_)) => None,
    };
    let program = match resolved {
        Ok(program) => program,
        Err(error) => return cannot_start(error),
    };
    Ok(Prepared::Ready(Launch {
        program,
        arguments: body.arguments.clone(),
        working_directory,
        entrypoint,
        fingerprint,
        environment,
        protocol: body.protocol,
        outputs: if body.protocol == Protocol::Jjp1 {
            body.outputs.clone()
        } else {
            Vec::new()
        },
    }))
}

/// Finds a bare program name on `path` (the program's own `PATH`). On
/// Windows it tries the name with each of `.exe` and `.com`, in the order
/// `pathext` gives them (`.COM;.EXE` when unset); `.bat` and `.cmd` are
/// never tried. Empty and relative entries of `path` are skipped, so the
/// current folder is never searched. On Unix only an executable file
/// matches; a match without the execute permission gives
/// [`StartError::PermissionDenied`].
pub fn resolve_program(
    name: &str,
    path: Option<&OsStr>,
    pathext: Option<&OsStr>,
) -> Result<PathBuf, StartError> {
    let candidates = candidate_names(name, pathext, cfg!(windows));
    let mut not_executable = false;
    for folder in path.map(std::env::split_paths).into_iter().flatten() {
        if folder.as_os_str().is_empty() || !folder.is_absolute() {
            continue;
        }
        for candidate in &candidates {
            let full = folder.join(candidate);
            let Ok(metadata) = std::fs::metadata(&full) else {
                continue;
            };
            if !metadata.is_file() {
                continue;
            }
            if is_executable(&metadata) {
                return Ok(full);
            }
            not_executable = true;
        }
    }
    Err(if not_executable {
        StartError::PermissionDenied
    } else {
        StartError::NotFound
    })
}

/// The file names a bare `name` may have on disk.
pub(super) fn candidate_names(name: &str, pathext: Option<&OsStr>, windows: bool) -> Vec<String> {
    if !windows {
        return vec![name.to_owned()];
    }
    let lower = name.to_ascii_lowercase();
    if lower.ends_with(".exe") || lower.ends_with(".com") {
        return vec![name.to_owned()];
    }
    let mut extensions: Vec<String> = Vec::new();
    for extension in pathext
        .map(|value| value.to_string_lossy().into_owned())
        .unwrap_or_default()
        .split(';')
        .map(|extension| extension.trim().to_ascii_lowercase())
    {
        if (extension == ".exe" || extension == ".com") && !extensions.contains(&extension) {
            extensions.push(extension);
        }
    }
    if extensions.is_empty() {
        extensions = vec![".com".to_owned(), ".exe".to_owned()];
    }
    extensions
        .into_iter()
        .map(|extension| format!("{name}{extension}"))
        .collect()
}

#[cfg(unix)]
fn is_executable(metadata: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    metadata.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn is_executable(_metadata: &std::fs::Metadata) -> bool {
    true
}

/// Whether Studio's variable `name` is on the [`ENV_DENYLIST`].
pub fn is_denied(name: &OsStr) -> bool {
    denied(name, cfg!(windows))
}

fn denied(name: &OsStr, fold: bool) -> bool {
    ENV_DENYLIST
        .iter()
        .any(|entry| match entry.strip_suffix('*') {
            Some(prefix) => starts_with(name, prefix, fold),
            None => same_name(name, entry, fold),
        })
}

fn same_name(name: &OsStr, other: &str, fold: bool) -> bool {
    let name = name.to_string_lossy();
    if fold {
        name.eq_ignore_ascii_case(other)
    } else {
        name == other
    }
}

fn starts_with(name: &OsStr, prefix: &str, fold: bool) -> bool {
    let name = name.to_string_lossy();
    match name.get(..prefix.len()) {
        Some(head) if fold => head.eq_ignore_ascii_case(prefix),
        Some(head) => head == prefix,
        None => false,
    }
}

/// The program's environment (section 9.1.3): `base` (Studio's) without the
/// [`ENV_DENYLIST`] and the job's `unset` names, then the job's `set`
/// constants, then, for `jjp/1`, [`JJP_ADDRESS_VARIABLE`]. Names compare
/// without regard to case on Windows. Pure: the caller passes `base`.
pub fn program_environment(
    base: impl IntoIterator<Item = (OsString, OsString)>,
    spec: &EnvironmentSpec,
    jjp_address: Option<SocketAddr>,
) -> Vec<(OsString, OsString)> {
    environment_with(base, spec, jjp_address, cfg!(windows))
}

pub(super) fn environment_with(
    base: impl IntoIterator<Item = (OsString, OsString)>,
    spec: &EnvironmentSpec,
    jjp_address: Option<SocketAddr>,
    fold: bool,
) -> Vec<(OsString, OsString)> {
    let mut environment: Vec<(OsString, OsString)> = base
        .into_iter()
        .filter(|(name, _)| {
            !denied(name, fold) && !spec.unset.iter().any(|unset| same_name(name, unset, fold))
        })
        .collect();
    for (name, value) in &spec.set {
        environment.retain(|(existing, _)| !same_name(existing, name, fold));
        environment.push((OsString::from(name), OsString::from(value)));
    }
    if let Some(address) = jjp_address {
        add_jjp_address(&mut environment, address, fold);
    }
    environment
}

/// Sets [`JJP_ADDRESS_VARIABLE`] in an environment already filtered, last,
/// replacing a value the job may have set.
pub(super) fn add_jjp_address(
    environment: &mut Vec<(OsString, OsString)>,
    address: SocketAddr,
    fold: bool,
) {
    environment.retain(|(existing, _)| !same_name(existing, JJP_ADDRESS_VARIABLE, fold));
    environment.push((
        OsString::from(JJP_ADDRESS_VARIABLE),
        OsString::from(address.to_string()),
    ));
}

/// The names in `base` the program will not receive (the denylist and the
/// job's `unset`, unless the job sets them again), for the program editor.
pub fn removed_variables(
    base: impl IntoIterator<Item = (OsString, OsString)>,
    spec: &EnvironmentSpec,
) -> Vec<OsString> {
    let fold = cfg!(windows);
    base.into_iter()
        .map(|(name, _)| name)
        .filter(|name| {
            (denied(name, fold) || spec.unset.iter().any(|unset| same_name(name, unset, fold)))
                && !spec.set.iter().any(|(set, _)| same_name(name, set, fold))
        })
        .collect()
}

/// `sha256:` followed by the lowercase hex of the file's SHA-256, read in
/// pieces so a large program never sits in memory. Blocks on the file.
pub fn fingerprint_file(path: &Path) -> io::Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Fingerprinter::new();
    let mut buffer = vec![0_u8; 64 * 1024];
    loop {
        match file.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => hasher.update(buffer.get(..read).unwrap_or_default()),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(hasher.finish())
}

// TODO(merge): use jykre_storage::Fingerprinter (being added in parallel with
// the same `new`/`update`/`finish` API) and delete this stand-in; the jykre
// crate must not depend on `sha2` itself.
use standin::Fingerprinter;

/// A private SHA-256 (FIPS 180-4) with the API of
/// `jykre_storage::Fingerprinter`, until that type is merged.
mod standin {
    const ROUND: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    const INITIAL: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];

    pub(super) struct Fingerprinter {
        state: [u32; 8],
        block: [u8; 64],
        filled: usize,
        length: u64,
    }

    impl Fingerprinter {
        pub(super) fn new() -> Self {
            Self {
                state: INITIAL,
                block: [0; 64],
                filled: 0,
                length: 0,
            }
        }

        pub(super) fn update(&mut self, mut bytes: &[u8]) {
            self.length = self.length.wrapping_add(
                u64::try_from(bytes.len())
                    .unwrap_or(u64::MAX)
                    .wrapping_mul(8),
            );
            while !bytes.is_empty() {
                let take = (64 - self.filled).min(bytes.len());
                let (head, rest) = bytes.split_at(take);
                if let Some(target) = self.block.get_mut(self.filled..self.filled + take) {
                    target.copy_from_slice(head);
                }
                self.filled += take;
                bytes = rest;
                if self.filled == 64 {
                    let block = self.block;
                    self.compress(&block);
                    self.filled = 0;
                }
            }
        }

        pub(super) fn finish(mut self) -> String {
            let length = self.length;
            let mut padding = vec![0x80_u8];
            let used = (self.filled + 1) % 64;
            let zeros = if used <= 56 { 56 - used } else { 120 - used };
            padding.extend(std::iter::repeat_n(0_u8, zeros));
            // `update` would count the padding into the length.
            for byte in padding.iter().chain(length.to_be_bytes().iter()) {
                if let Some(slot) = self.block.get_mut(self.filled) {
                    *slot = *byte;
                }
                self.filled += 1;
                if self.filled == 64 {
                    let block = self.block;
                    self.compress(&block);
                    self.filled = 0;
                }
            }
            let mut text = String::with_capacity(7 + 64);
            text.push_str("sha256:");
            for word in self.state {
                text.push_str(&format!("{word:08x}"));
            }
            text
        }

        fn compress(&mut self, block: &[u8; 64]) {
            let mut schedule = [0_u32; 64];
            for (word, bytes) in schedule.iter_mut().zip(block.as_chunks::<4>().0) {
                *word = u32::from_be_bytes(*bytes);
            }
            // `index` runs from 16 to 63, so every index below is in bounds.
            for index in 16..64 {
                let early = schedule[index - 15];
                let late = schedule[index - 2];
                let s0 = early.rotate_right(7) ^ early.rotate_right(18) ^ (early >> 3);
                let s1 = late.rotate_right(17) ^ late.rotate_right(19) ^ (late >> 10);
                schedule[index] = schedule[index - 16]
                    .wrapping_add(s0)
                    .wrapping_add(schedule[index - 7])
                    .wrapping_add(s1);
            }
            let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = self.state;
            for (round, word) in ROUND.iter().zip(schedule.iter()) {
                let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
                let choice = (e & f) ^ (!e & g);
                let first = h
                    .wrapping_add(s1)
                    .wrapping_add(choice)
                    .wrapping_add(*round)
                    .wrapping_add(*word);
                let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
                let majority = (a & b) ^ (a & c) ^ (b & c);
                let second = s0.wrapping_add(majority);
                h = g;
                g = f;
                f = e;
                e = d.wrapping_add(first);
                d = c;
                c = b;
                b = a;
                a = first.wrapping_add(second);
            }
            for (state, value) in self.state.iter_mut().zip([a, b, c, d, e, f, g, h]) {
                *state = state.wrapping_add(value);
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::Fingerprinter;

        fn hash(pieces: &[&[u8]]) -> String {
            let mut hasher = Fingerprinter::new();
            for piece in pieces {
                hasher.update(piece);
            }
            hasher.finish()
        }

        #[test]
        fn known_vectors_match_in_any_split() {
            assert_eq!(
                hash(&[]),
                "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
            );
            assert_eq!(
                hash(&[b"abc"]),
                "sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
            );
            let two_blocks: &[u8] = b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq";
            let expected =
                "sha256:248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1";
            assert_eq!(hash(&[two_blocks]), expected);
            for split in 0..two_blocks.len() {
                let (head, tail) = two_blocks.split_at(split);
                assert_eq!(hash(&[head, tail]), expected);
            }
            let million = vec![b'a'; 1_000_000];
            assert_eq!(
                hash(&[&million[..333_333], &million[333_333..]]),
                "sha256:cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0"
            );
        }
    }
}
