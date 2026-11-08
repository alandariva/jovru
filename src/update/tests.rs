//! Update tests: preferences and schedule, signatures made with a test key,
//! and the release list and the whole install against an HTTP server on
//! this computer. No request leaves the machine.
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::watch;

use super::install::{Job, Reports, install_job};
use super::release::asset_name;
use super::*;
use crate::drivers::{CancellationToken, ErrorCode};

/// A folder in the system's temporary folder, removed when dropped.
pub(crate) struct TempDir(PathBuf);

impl TempDir {
    pub(crate) fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("jykre-update-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    pub(crate) fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Every file under `dir`, by relative path with `/`, with its content.
pub(crate) fn tree(dir: &Path) -> Vec<(String, Vec<u8>)> {
    fn walk(base: &Path, dir: &Path, out: &mut Vec<(String, Vec<u8>)>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(base, &path, out);
            } else {
                let relative = path.strip_prefix(base).unwrap();
                let name = relative
                    .components()
                    .map(|part| part.as_os_str().to_string_lossy().into_owned())
                    .collect::<Vec<_>>()
                    .join("/");
                out.push((name, std::fs::read(&path).unwrap()));
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, dir, &mut out);
    out.sort();
    out
}

// --- Preferences and schedule ------------------------------------------

#[test]
fn checks_are_on_unless_turned_off() {
    assert!(UpdatePreferences::from_value(None).check);
    assert!(UpdatePreferences::from_value(Some(&serde_json::json!("odd"))).check);
    let off = UpdatePreferences { check: false };
    assert_eq!(UpdatePreferences::from_value(Some(&off.to_value())), off);
    let record = UpdateRecord {
        last_checked_ms: Some(1_700_000_000_000),
    };
    assert_eq!(UpdateRecord::from_value(Some(&record.to_value())), record);
    assert_eq!(UpdateRecord::from_value(None), UpdateRecord::default());
}

#[test]
fn the_next_check_waits_a_day_and_never_runs_at_start() {
    let on = UpdatePreferences::default();
    let now = 10 * CHECK_INTERVAL.as_millis() as u64;
    let never = UpdateRecord::default();
    assert_eq!(
        next_check_in(now, &never, &on, true),
        Some(FIRST_CHECK_DELAY)
    );
    assert_eq!(
        next_check_in(now, &never, &UpdatePreferences { check: false }, true),
        None
    );
    assert_eq!(next_check_in(now, &never, &on, false), None);
    let hour = 60 * 60 * 1000;
    let recent = UpdateRecord {
        last_checked_ms: Some(now - hour),
    };
    assert_eq!(
        next_check_in(now, &recent, &on, true),
        Some(CHECK_INTERVAL - Duration::from_millis(hour))
    );
    let old = UpdateRecord {
        last_checked_ms: Some(now - 2 * CHECK_INTERVAL.as_millis() as u64),
    };
    assert_eq!(next_check_in(now, &old, &on, true), Some(FIRST_CHECK_DELAY));
    let future = UpdateRecord {
        last_checked_ms: Some(now + hour),
    };
    assert_eq!(
        next_check_in(now, &future, &on, true),
        Some(FIRST_CHECK_DELAY)
    );
}

#[tokio::test]
async fn preferences_and_record_survive_a_restart() {
    let temp = TempDir::new();
    let dir = temp.path();
    assert_eq!(
        load(dir).await.unwrap(),
        (UpdatePreferences::default(), UpdateRecord::default())
    );
    save_preferences(
        dir,
        UpdatePreferences { check: false },
        PREFERENCE_WRITES.ask(),
    )
    .await
    .unwrap();
    let record = UpdateRecord {
        last_checked_ms: Some(42),
    };
    save_record(dir, record, RECORD_WRITES.ask()).await.unwrap();
    assert_eq!(
        load(dir).await.unwrap(),
        (UpdatePreferences { check: false }, record)
    );
}

// --- Signed archives -----------------------------------------------------

const VERSION: &str = "0.9.0";

fn signing_key(seed: u8) -> self_update::zipsign_api::SigningKey {
    self_update::zipsign_api::SigningKey::from_bytes(&[seed; 32])
}

fn verifying_keys(seed: u8) -> &'static [self_update::VerifyingKey] {
    let key = signing_key(seed).verifying_key().to_bytes();
    Box::leak(vec![key].into_boxed_slice())
}

/// The files of a release folder: what the archive holds under its folder.
fn release_files(platform: Platform) -> Vec<(String, Vec<u8>)> {
    vec![
        (platform.executable().to_owned(), b"new binary".to_vec()),
        ("LICENSE".to_owned(), b"new license".to_vec()),
        (
            "catalog/postgresql/manifest.json".to_owned(),
            b"new catalog".to_vec(),
        ),
    ]
}

/// The archive of `platform`, signed by `seed` (unsigned with `None`) for
/// the file name `context`.
fn archive(platform: Platform, seed: Option<u8>, context: &str) -> Vec<u8> {
    let folder = asset_name(VERSION, platform)
        .trim_end_matches(".tar.gz")
        .trim_end_matches(".zip")
        .to_owned();
    let files = release_files(platform);
    let plain = match platform {
        Platform::LinuxX86_64 => {
            let encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
            let mut builder = tar::Builder::new(encoder);
            for (name, content) in &files {
                let mut header = tar::Header::new_gnu();
                header.set_size(content.len() as u64);
                header.set_mode(0o644);
                header.set_cksum();
                builder
                    .append_data(&mut header, format!("{folder}/{name}"), content.as_slice())
                    .unwrap();
            }
            builder.into_inner().unwrap().finish().unwrap()
        }
        Platform::WindowsX86_64 => {
            let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
            let options = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);
            for (name, content) in &files {
                writer
                    .start_file(format!("{folder}/{name}"), options)
                    .unwrap();
                writer.write_all(content).unwrap();
            }
            writer.finish().unwrap().into_inner()
        }
    };
    let Some(seed) = seed else {
        return plain;
    };
    let mut input = std::io::Cursor::new(plain);
    let mut output = std::io::Cursor::new(Vec::new());
    let keys = [signing_key(seed)];
    let context = Some(context.as_bytes());
    match platform {
        Platform::LinuxX86_64 => {
            zipsign_api::sign::copy_and_sign_tar(&mut input, &mut output, &keys, context).unwrap();
        }
        Platform::WindowsX86_64 => {
            zipsign_api::sign::copy_and_sign_zip(&mut input, &mut output, &keys, context).unwrap();
        }
    }
    output.into_inner()
}

fn written(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, bytes).unwrap();
    path
}

#[test]
fn a_signature_binds_the_key_and_the_file_name() {
    for platform in [Platform::LinuxX86_64, Platform::WindowsX86_64] {
        let temp = TempDir::new();
        let name = asset_name(VERSION, platform);
        let signed = archive(platform, Some(1), &name);
        let path = written(temp.path(), &name, &signed);
        assert!(self_update::verify_signature(&path, verifying_keys(1)).is_ok());
        assert!(
            self_update::verify_signature(&path, verifying_keys(2)).is_err(),
            "another key"
        );

        let older = asset_name("0.8.0", platform);
        let renamed = written(temp.path(), &older, &signed);
        assert!(
            self_update::verify_signature(&renamed, verifying_keys(1)).is_err(),
            "the same archive under another version's name"
        );

        let mut changed = signed.clone();
        let middle = changed.len() / 2;
        changed[middle] ^= 0xff;
        let changed = written(temp.path(), &name, &changed);
        assert!(self_update::verify_signature(&changed, verifying_keys(1)).is_err());

        let unsigned = written(temp.path(), &name, &archive(platform, None, &name));
        assert!(self_update::verify_signature(&unsigned, verifying_keys(1)).is_err());
    }
}

// --- An HTTP server on this computer ---------------------------------------

/// What the server answers to a path.
#[derive(Clone)]
enum Reply {
    Full {
        status: u16,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    },
    /// Headers announcing a body that never comes.
    Stall,
}

impl Reply {
    fn ok(body: impl Into<Vec<u8>>) -> Self {
        Self::Full {
            status: 200,
            headers: Vec::new(),
            body: body.into(),
        }
    }

    fn status(status: u16, headers: &[(&str, &str)]) -> Self {
        Self::Full {
            status,
            headers: headers
                .iter()
                .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
                .collect(),
            body: Vec::new(),
        }
    }
}

type Routes = Arc<dyn Fn(&str, &str) -> Reply + Send + Sync>;

/// A server on `127.0.0.1` that answers each request with `routes(path,
/// base)` and keeps the heads it received.
struct Stub {
    base: String,
    heads: Arc<Mutex<Vec<String>>>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Stub {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn stub(routes: impl Fn(&str, &str) -> Reply + Send + Sync + 'static) -> Stub {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let heads = Arc::new(Mutex::new(Vec::new()));
    let routes: Routes = Arc::new(routes);
    let task = tokio::spawn({
        let base = base.clone();
        let heads = heads.clone();
        async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let base = base.clone();
                let heads = heads.clone();
                let routes = routes.clone();
                tokio::spawn(async move {
                    let mut head = Vec::new();
                    let mut byte = [0u8; 1];
                    while !head.ends_with(b"\r\n\r\n") {
                        if socket.read(&mut byte).await.unwrap_or(0) == 0 {
                            return;
                        }
                        head.push(byte[0]);
                    }
                    let head = String::from_utf8_lossy(&head).into_owned();
                    let path = head.split_whitespace().nth(1).unwrap_or("/").to_owned();
                    heads.lock().unwrap().push(head);
                    match routes(&path, &base) {
                        Reply::Full {
                            status,
                            headers,
                            body,
                        } => {
                            let mut out = format!(
                                "HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n",
                                body.len()
                            );
                            for (name, value) in headers {
                                out.push_str(&format!("{name}: {value}\r\n"));
                            }
                            out.push_str("\r\n");
                            let _ = socket.write_all(out.as_bytes()).await;
                            let _ = socket.write_all(&body).await;
                            let _ = socket.shutdown().await;
                        }
                        Reply::Stall => {
                            let _ = socket
                                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 1000\r\n\r\nx")
                                .await;
                            tokio::time::sleep(Duration::from_secs(3600)).await;
                        }
                    }
                });
            }
        }
    });
    Stub { base, heads, task }
}

const LIST: &str = "/repos/alandariva/jykre-studio/releases?per_page=100";
const ASSET: &str = "/repos/alandariva/jykre-studio/releases/assets/7";
const FILE: &str = "/download/file?token=secret";

/// The release list with `versions`, each with both platforms' archives.
fn release_list(base: &str, versions: &[&str]) -> String {
    let releases: Vec<_> = versions
        .iter()
        .map(|version| {
            let asset = |platform| {
                serde_json::json!({
                    "name": asset_name(version, platform),
                    "url": format!("{base}{ASSET}"),
                })
            };
            serde_json::json!({
                "tag_name": format!("v{version}"),
                "created_at": "2026-11-08T00:00:00Z",
                "name": format!("Jykre Studio {version}"),
                "body": "notes",
                "html_url": release::release_page(version),
                "assets": [asset(Platform::LinuxX86_64), asset(Platform::WindowsX86_64)],
            })
        })
        .collect();
    serde_json::Value::Array(releases).to_string()
}

#[tokio::test]
async fn the_check_reads_the_list_anonymously_and_offers_the_newest() {
    let server = stub(|path, base| match path {
        LIST => Reply::ok(release_list(base, &["0.1.0", "0.2.0-rc.1", "0.1.9"])),
        _ => Reply::status(404, &[]),
    })
    .await;
    let endpoints = Endpoints::loopback(&server.base);
    let candidate = check(
        &endpoints,
        "0.1.0",
        Some(Platform::LinuxX86_64),
        &CancellationToken::new(),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(candidate.version, "0.2.0-rc.1");
    assert_eq!(
        candidate.asset.map(|asset| asset.name).as_deref(),
        Some("jykre-studio-v0.2.0-rc.1-linux-x86_64.tar.gz")
    );
    let heads = server.heads.lock().unwrap().join("\n").to_lowercase();
    assert!(heads.contains(&format!("user-agent: {}", user_agent().to_lowercase())));
    assert!(!heads.contains("authorization"), "no token is sent");
    assert!(!heads.contains("cookie"));
}

#[tokio::test]
async fn a_failed_check_is_classified_and_waits_before_the_next() {
    let cases: [(Reply, ErrorCode); 4] = [
        (Reply::status(404, &[]), ErrorCode::NotFound),
        (
            Reply::status(
                403,
                &[("x-ratelimit-remaining", "0"), ("retry-after", "7200")],
            ),
            ErrorCode::Connection,
        ),
        (Reply::status(500, &[]), ErrorCode::Connection),
        (Reply::ok("not json"), ErrorCode::Protocol),
    ];
    for (reply, code) in cases {
        let rate_limited = matches!(&reply, Reply::Full { status: 403, .. });
        let server = stub(move |_, _| reply.clone()).await;
        let failure = check(
            &Endpoints::loopback(&server.base),
            "0.1.0",
            None,
            &CancellationToken::new(),
        )
        .await
        .unwrap_err();
        assert_eq!(failure.error.code, code);
        assert!(!failure.error.message.contains("127.0.0.1"), "no address");
        let expected = if rate_limited {
            Duration::from_secs(7200)
        } else {
            RETRY_AFTER_FAILURE
        };
        assert_eq!(failure.retry_in, expected);
    }
}

#[tokio::test]
async fn a_check_can_be_cancelled() {
    let server = stub(|_, _| Reply::Stall).await;
    let cancel = CancellationToken::new();
    cancel.cancel();
    let failure = check(&Endpoints::loopback(&server.base), "0.1.0", None, &cancel)
        .await
        .unwrap_err();
    assert_eq!(failure.error.code, ErrorCode::Cancelled);
}

// --- The whole install -------------------------------------------------

/// An installed folder of the previous version, with a file of the user's.
fn installed(temp: &TempDir, platform: Platform) -> InstallDir {
    let dir = temp.path().join("Jykre Studio");
    std::fs::create_dir_all(dir.join("catalog/postgresql")).unwrap();
    std::fs::write(dir.join(platform.executable()), b"old binary").unwrap();
    std::fs::write(dir.join("LICENSE"), b"old license").unwrap();
    std::fs::write(dir.join("catalog/postgresql/manifest.json"), b"old catalog").unwrap();
    std::fs::write(dir.join("notes.txt"), b"the user's").unwrap();
    InstallDir {
        exe: dir.join(platform.executable()),
        dir,
    }
}

fn candidate(base: &str, platform: Platform) -> Candidate {
    Candidate {
        version: VERSION.to_owned(),
        page: release::release_page(VERSION),
        asset: Some(release::AssetRef {
            name: asset_name(VERSION, platform),
            url: format!("{base}{ASSET}"),
        }),
    }
}

/// A server that redirects the asset to its file, as GitHub does.
async fn serving(file: Reply) -> Stub {
    stub(move |path, base| match path {
        ASSET => Reply::status(302, &[("Location", &format!("{base}{FILE}"))]),
        FILE => file.clone(),
        _ => Reply::status(404, &[]),
    })
    .await
}

async fn run_install(
    server: &Stub,
    dir: &InstallDir,
    platform: Platform,
    keys: &'static [self_update::VerifyingKey],
    cancel: &CancellationToken,
) -> crate::drivers::DriverResult<Installed> {
    let endpoints = Endpoints::loopback(&server.base);
    let candidate = candidate(&server.base, platform);
    let reports = Reports {
        progress: watch::channel(Progress::default()).0,
        phase: watch::channel(Phase::Download).0,
    };
    let job = Job {
        endpoints: &endpoints,
        candidate: &candidate,
        dir,
        platform,
        keys,
    };
    install_job(job, &reports, cancel).await
}

#[tokio::test]
async fn a_signed_release_replaces_the_folder_and_keeps_the_user_files() {
    for platform in [Platform::LinuxX86_64, Platform::WindowsX86_64] {
        let temp = TempDir::new();
        let dir = installed(&temp, platform);
        let file = archive(platform, Some(1), &asset_name(VERSION, platform));
        let server = serving(Reply::ok(file)).await;
        let done = run_install(
            &server,
            &dir,
            platform,
            verifying_keys(1),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(
            done,
            Installed {
                version: VERSION.to_owned(),
                restart_path: dir.exe.clone(),
            }
        );
        let mut expected = release_files(platform);
        expected.push(("notes.txt".to_owned(), b"the user's".to_vec()));
        expected.sort();
        let files: Vec<_> = tree(&dir.dir)
            .into_iter()
            .filter(|(path, _)| !path.starts_with(".jykre-update/done-"))
            .collect();
        assert_eq!(files, expected, "{platform:?}");
        let heads = server.heads.lock().unwrap().join("\n").to_lowercase();
        assert!(heads.contains("accept: application/octet-stream"));
    }
}

#[tokio::test]
async fn an_archive_that_does_not_verify_changes_nothing() {
    let platform = Platform::LinuxX86_64;
    let name = asset_name(VERSION, platform);
    let cases = [
        archive(platform, Some(2), &name),
        archive(platform, None, &name),
        archive(platform, Some(1), &asset_name("0.8.0", platform)),
    ];
    for file in cases {
        let temp = TempDir::new();
        let dir = installed(&temp, platform);
        let before = tree(&dir.dir);
        let server = serving(Reply::ok(file)).await;
        let error = run_install(
            &server,
            &dir,
            platform,
            verifying_keys(1),
            &CancellationToken::new(),
        )
        .await
        .unwrap_err();
        assert_eq!(error.code, ErrorCode::Protocol);
        assert!(error.message.contains("nothing was installed"));
        assert_eq!(tree(&dir.dir), before);
    }
}

#[tokio::test]
async fn a_download_outside_github_is_refused() {
    let platform = Platform::LinuxX86_64;
    let temp = TempDir::new();
    let dir = installed(&temp, platform);
    let before = tree(&dir.dir);
    let server = stub(|path, _| match path {
        ASSET => Reply::status(302, &[("Location", "http://example.invalid/file")]),
        _ => Reply::status(404, &[]),
    })
    .await;
    let error = run_install(
        &server,
        &dir,
        platform,
        verifying_keys(1),
        &CancellationToken::new(),
    )
    .await
    .unwrap_err();
    assert_eq!(error.code, ErrorCode::Protocol);
    assert!(!error.message.contains("example.invalid"));
    assert_eq!(tree(&dir.dir), before);
}

#[tokio::test]
async fn an_archive_announced_too_large_is_not_read() {
    let platform = Platform::LinuxX86_64;
    let temp = TempDir::new();
    let dir = installed(&temp, platform);
    let before = tree(&dir.dir);
    // A server that announces more than the cap and sends nothing.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let raw = tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let mut buffer = [0u8; 4096];
            let _ = socket.read(&mut buffer).await;
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n",
                download::MAX_ARCHIVE_BYTES + 1
            );
            let _ = socket.write_all(head.as_bytes()).await;
        }
    });
    let server = Stub {
        base,
        heads: Arc::new(Mutex::new(Vec::new())),
        task: raw,
    };
    let error = run_install(
        &server,
        &dir,
        platform,
        verifying_keys(1),
        &CancellationToken::new(),
    )
    .await
    .unwrap_err();
    assert_eq!(error.code, ErrorCode::Protocol);
    assert!(error.message.contains("larger than expected"));
    assert_eq!(tree(&dir.dir), before);
}

#[tokio::test]
async fn a_cancelled_download_changes_nothing() {
    let platform = Platform::LinuxX86_64;
    let temp = TempDir::new();
    let dir = installed(&temp, platform);
    let before = tree(&dir.dir);
    let server = serving(Reply::Stall).await;
    let cancel = CancellationToken::new();
    let stop = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(200)).await;
        stop.cancel();
    });
    let error = run_install(&server, &dir, platform, verifying_keys(1), &cancel)
        .await
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::Cancelled);
    assert_eq!(tree(&dir.dir), before, "the staging folder is removed too");
    assert!(
        !dir.dir.join(swap::WORK_DIR).exists(),
        "and the empty work folder"
    );
}
