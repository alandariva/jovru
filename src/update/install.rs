//! Installs a release: download, verify the signature, unpack, check, swap.
//! Nothing is unpacked before the signature verifies, and nothing in the
//! folder changes before the swap, which is all or nothing.
use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::sync::watch;
use tracing::Instrument;

use crate::drivers::{CancellationToken, DriverError, DriverResult, ErrorCode};

use super::download::{self, DOWNLOAD_TIMEOUT, MAX_ARCHIVE_BYTES, Progress};
use super::error;
use super::layout::InstallDir;
use super::release::{Candidate, Platform, asset_name};
use super::state::Phase;
use super::swap::{self, WORK_DIR};
use super::{Endpoints, keys};

/// Verifying and unpacking the archive.
pub const UNPACK_TIMEOUT: Duration = Duration::from_secs(120);

/// A release installed in the folder; it runs after a restart.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Installed {
    pub version: String,
    /// The executable to start, recorded before the swap: on Linux the
    /// running process's own path follows the replaced file.
    pub restart_path: PathBuf,
}

/// What to install, and where.
#[derive(Clone, Copy, Debug)]
pub struct Job<'a> {
    pub endpoints: &'a Endpoints,
    pub candidate: &'a Candidate,
    pub dir: &'a InstallDir,
    pub platform: Platform,
    /// Any one of these may have signed the archive ([`keys::RELEASE_KEYS`]).
    pub keys: &'static [self_update::VerifyingKey],
}

/// How the work is going, for the view.
#[derive(Clone, Debug)]
pub struct Reports {
    pub progress: watch::Sender<Progress>,
    pub phase: watch::Sender<Phase>,
}

/// Installs `candidate` into `dir` with the release keys. Cancelling stops
/// the work before the swap, never during it.
pub async fn install(
    endpoints: &Endpoints,
    candidate: &Candidate,
    dir: &InstallDir,
    platform: Platform,
    reports: Reports,
    cancel: CancellationToken,
) -> DriverResult<Installed> {
    let job = Job {
        endpoints,
        candidate,
        dir,
        platform,
        keys: keys::RELEASE_KEYS,
    };
    install_job(job, &reports, &cancel).await
}

/// [`install`] of a [`Job`], with the keys it names.
pub(crate) async fn install_job(
    job: Job<'_>,
    reports: &Reports,
    cancel: &CancellationToken,
) -> DriverResult<Installed> {
    let span = crate::client_span!(
        "update_install",
        http.request.method = "GET",
        url.template = "/repos/{owner}/{repo}/releases/assets/{id}",
        jykre.bytes = tracing::field::Empty,
        jykre.files = tracing::field::Empty,
    );
    let id = uuid::Uuid::new_v4().simple().to_string();
    let staging = job.dir.dir.join(WORK_DIR).join(format!("staging-{id}"));
    let result = run(job, reports, cancel, &staging, &id, &span)
        .instrument(span.clone())
        .await;
    crate::telemetry::attributes::record_outcome(&span, &result);
    // Whatever happened, the staging folder goes (the swap emptied it), and
    // the work folder with it when nothing else is in it.
    let removal = staging.clone();
    let _ = tokio::task::spawn_blocking(move || {
        let _ = std::fs::remove_dir_all(&removal);
        if let Some(work) = removal.parent() {
            let _ = std::fs::remove_dir(work);
        }
    })
    .await;
    match &result {
        Ok(_) => tracing::info!("update installed"),
        Err(error) => tracing::warn!(error_code = ?error.code, "update not installed"),
    }
    result
}

async fn run(
    job: Job<'_>,
    reports: &Reports,
    cancel: &CancellationToken,
    staging: &Path,
    id: &str,
    span: &tracing::Span,
) -> DriverResult<Installed> {
    let Job {
        endpoints,
        candidate,
        dir,
        platform,
        keys,
    } = job;
    let asset = candidate.asset.as_ref().ok_or_else(|| {
        DriverError::new(
            ErrorCode::NotFound,
            crate::tr!("This release has no archive for this system."),
        )
    })?;
    if !endpoints.asset_url_allowed(&asset.url) {
        return Err(error::from_status(302));
    }
    // The name is ours, from the version: it is what the signature binds.
    let archive = staging.join(asset_name(&candidate.version, platform));
    tokio::fs::create_dir_all(staging)
        .await
        .map_err(|e| error::from_io(&e))?;

    reports.phase.send_replace(Phase::Download);
    let client = endpoints.download_client()?;
    let bytes = tokio::select! {
        done = tokio::time::timeout(
            DOWNLOAD_TIMEOUT,
            download::download(&client, &asset.url, &archive, MAX_ARCHIVE_BYTES, &reports.progress),
        ) => done.unwrap_or_else(|_| Err(error::timeout()))?,
        () = cancel.cancelled() => return Err(DriverError::cancelled()),
    };
    span.record("jykre.bytes", crate::telemetry::attributes::count(bytes));

    reports.phase.send_replace(Phase::Verify);
    let unpacked = staging.join("files");
    let unpack = {
        let archive = archive.clone();
        let unpacked = unpacked.clone();
        tokio::task::spawn_blocking(move || verify_and_unpack(&archive, &unpacked, keys, platform))
    };
    let staged = tokio::select! {
        done = tokio::time::timeout(UNPACK_TIMEOUT, unpack) => match done {
            Ok(Ok(staged)) => staged?,
            Ok(Err(_)) => return Err(error::internal()),
            Err(_) => return Err(DriverError::new(
                ErrorCode::Timeout,
                crate::tr!("Unpacking the update took too long; nothing was installed."),
            )),
        },
        () = cancel.cancelled() => return Err(DriverError::cancelled()),
    };
    if cancel.is_cancelled() {
        return Err(DriverError::cancelled());
    }

    // From here on nothing is cancelled: the swap is renames only.
    reports.phase.send_replace(Phase::Install);
    let work = dir.dir.join(WORK_DIR);
    let folder = dir.dir.clone();
    let id = id.to_owned();
    let swapped = tokio::task::spawn_blocking(move || {
        let trash = work.join(format!("trash-{id}"));
        let count = swap::apply(&folder, &staged, &trash)?;
        let done = work.join(format!("done-{id}"));
        if std::fs::rename(&trash, &done).is_ok() {
            // On Windows the running executable stays until the next start.
            let _ = std::fs::remove_dir_all(&done);
        }
        Ok::<_, DriverError>(count)
    })
    .await
    .map_err(|_| error::internal())??;
    span.record("jykre.files", crate::telemetry::attributes::count(swapped));
    Ok(Installed {
        version: candidate.version.clone(),
        restart_path: dir.exe.clone(),
    })
}

/// Verifies `archive` against `keys`, then unpacks and checks it.
fn verify_and_unpack(
    archive: &Path,
    unpacked: &Path,
    keys: &[self_update::VerifyingKey],
    platform: Platform,
) -> DriverResult<swap::StagedRelease> {
    if keys.is_empty() {
        // `verify_signature` would accept anything.
        return Err(error::unsigned());
    }
    self_update::verify_signature(archive, keys).map_err(|e| error::from_update_error(&e))?;
    let kind = match platform {
        Platform::LinuxX86_64 => self_update::ArchiveKind::Tar(Some(self_update::Compression::Gz)),
        Platform::WindowsX86_64 => self_update::ArchiveKind::Zip,
    };
    self_update::Extract::from_source(archive)
        .archive(kind)
        .extract_into(unpacked)
        .map_err(|e| match e {
            self_update::Error::Io(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::StorageFull
                ) =>
            {
                error::from_io(&error)
            }
            _ => error::unreadable_archive(),
        })?;
    swap::validate_staged(unpacked, platform)
}
