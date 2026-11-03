use crate::DatabricksConfig;
use jykre_core::{DriverError, DriverResult, ErrorCode, SecretString};
use serde_json::Value;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{Arc, Mutex, OnceLock},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{io::AsyncReadExt, process::Command, sync::Mutex as AsyncMutex, time::Instant};

const REUSE: Duration = Duration::from_secs(300);

/// Non-sensitive metadata from `databricks auth profiles --skip-validate`.
/// Listing does not acquire tokens or validate every profile against a server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CliProfile {
    pub name: String,
    pub host: Option<String>,
    /// CLI preference, independent of the profile's literal name.
    pub is_default: bool,
}

impl CliProfile {
    /// Initial choice for a new connection. Explicit/saved selections belong
    /// to the caller and must not be overwritten by this preference.
    pub fn preferred(profiles: &[Self]) -> Option<&Self> {
        profiles
            .iter()
            .find(|profile| profile.is_default)
            .or_else(|| profiles.iter().find(|profile| profile.name == "DEFAULT"))
            .or(match profiles {
                [profile] => Some(profile),
                _ => None,
            })
    }
}

pub(super) fn config_path() -> DriverResult<PathBuf> {
    std::env::var_os("DATABRICKS_CONFIG_FILE")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("USERPROFILE")
                .or_else(|| std::env::var_os("HOME"))
                .map(|home| PathBuf::from(home).join(".databrickscfg"))
        })
        .ok_or_else(|| DriverError::validation("Cannot locate Databricks CLI configuration"))
}

fn validate_executable(path: &Path) -> DriverResult<()> {
    if path.as_os_str().is_empty()
        || path
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("bat") || ext.eq_ignore_ascii_case("cmd"))
    {
        return Err(DriverError::validation(
            "Use the native Databricks executable",
        ));
    }
    Ok(())
}

async fn cli_output(
    executable: &Path,
    config_file: &Path,
    args: &[&str],
    max_bytes: u64,
) -> DriverResult<Vec<u8>> {
    validate_executable(executable)?;
    let mut command = Command::new(executable);
    command
        .args(args)
        .env("DATABRICKS_CONFIG_FILE", config_file)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    #[cfg(windows)]
    command.creation_flags(0x08000000);
    let mut child = command.spawn().map_err(|_| {
        DriverError::new(
            ErrorCode::Authentication,
            "Cannot launch Databricks CLI; check the CLI path and installation",
        )
    })?;
    let mut bytes = Vec::new();
    child
        .stdout
        .take()
        .ok_or_else(super::runtime::protocol)?
        .take(max_bytes + 1)
        .read_to_end(&mut bytes)
        .await
        .map_err(|_| super::runtime::protocol())?;
    if bytes.len() as u64 > max_bytes {
        return Err(super::runtime::protocol());
    }
    if !child
        .wait()
        .await
        .map_err(|_| super::runtime::protocol())?
        .success()
    {
        return Err(DriverError::new(
            ErrorCode::Authentication,
            "Databricks CLI command failed; check the CLI installation and authenticate the selected profile",
        ));
    }
    Ok(bytes)
}

/// Discovers workspace profile names with the configured native CLI. Output is
/// bounded to 1 MiB; execution is cancellable, with a 60-second timeout. Account
/// console hosts are excluded; account_id alone does not imply an account-only
/// profile. Authentication is checked later for the selected one.
pub async fn list_profiles(
    cli_path: Option<&Path>,
    cancel: jykre_core::CancellationToken,
) -> DriverResult<Vec<CliProfile>> {
    let executable = cli_path.unwrap_or_else(|| Path::new("databricks"));
    validate_executable(executable)?;
    if cancel.is_cancelled() {
        return Err(DriverError::cancelled());
    }
    let config = config_path()?;
    let work = profiles_in_file(executable, &config);
    tokio::select! { biased;
        _ = cancel.cancelled() => Err(DriverError::cancelled()),
        result = tokio::time::timeout(Duration::from_secs(60),work) => result.map_err(|_| DriverError::new(ErrorCode::Timeout,"Databricks CLI profile discovery timed out"))?,
    }
}

async fn profiles_in_file(executable: &Path, config_file: &Path) -> DriverResult<Vec<CliProfile>> {
    let bytes = cli_output(
        executable,
        config_file,
        &["auth", "profiles", "--skip-validate", "--output", "json"],
        1024 * 1024,
    )
    .await?;
    parse_profiles(&bytes)
}

/// Use the CLI's own INI semantics for both discovery and connection setup.
/// Reading a profile's host ourselves can disagree on BOMs, comments and other
/// supported configuration syntax. Tokens are not acquired by this command.
pub(super) async fn workspace_origin(
    config: &DatabricksConfig,
    config_file: &Path,
) -> DriverResult<reqwest::Url> {
    let executable = config
        .cli_path
        .as_deref()
        .unwrap_or_else(|| Path::new("databricks"));
    let profiles = tokio::time::timeout(
        Duration::from_secs(60),
        profiles_in_file(executable, config_file),
    )
    .await
    .map_err(|_| {
        DriverError::new(
            ErrorCode::Timeout,
            "Databricks CLI profile discovery timed out",
        )
    })??;
    origin_for_profile(&profiles, &config.profile)
}

fn origin_for_profile(profiles: &[CliProfile], selected: &str) -> DriverResult<reqwest::Url> {
    let profile = profiles.iter().find(|profile| profile.name == selected).ok_or_else(|| {
        DriverError::new(ErrorCode::NotFound, "Selected Databricks CLI workspace profile was not found; reload CLI profiles and select it again")
    })?;
    let host = profile.host.as_deref().ok_or_else(|| DriverError::validation(
        "Selected Databricks CLI profile has no HTTPS workspace URL; configure its host and reload CLI profiles",
    ))?;
    reqwest::Url::parse(host).map_err(|_| super::runtime::protocol())
}

fn parse_profiles(bytes: &[u8]) -> DriverResult<Vec<CliProfile>> {
    #[derive(serde::Deserialize)]
    struct Profile {
        name: String,
        host: Option<String>,
        #[serde(default, rename = "default")]
        is_default: bool,
    }
    #[derive(serde::Deserialize)]
    struct Profiles {
        profiles: Option<Vec<Profile>>,
    }
    let value: Profiles = serde_json::from_slice(bytes).map_err(|_| super::runtime::protocol())?;
    let mut profiles = Vec::new();
    for profile in value.profiles.unwrap_or_default() {
        if profile.name.trim().is_empty() || profile.name.chars().any(char::is_control) {
            return Err(super::runtime::protocol());
        }
        let host = profile
            .host
            .and_then(|host| reqwest::Url::parse(&host).ok())
            .filter(|host| {
                host.scheme() == "https"
                    && host.host_str().is_some()
                    && host.username().is_empty()
                    && host.password().is_none()
                    && host.query().is_none()
                    && host.fragment().is_none()
                    && host.path() == "/"
            });
        // Like the CLI SDK's account-host fallback, classify the destination,
        // not account_id: normal workspace profiles can carry both IDs.
        if host
            .as_ref()
            .and_then(reqwest::Url::host_str)
            .is_some_and(|host| host.starts_with("accounts.") || host.starts_with("accounts-dod."))
        {
            continue;
        }
        profiles.push(CliProfile {
            name: profile.name,
            host: host.map(|host| host.origin().ascii_serialization()),
            is_default: profile.is_default,
        });
    }
    profiles.sort_by(|a, b| a.name.cmp(&b.name));
    profiles.dedup_by(|a, b| a.name == b.name);
    Ok(profiles)
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct Key {
    profile: String,
    executable: PathBuf,
    config_file: PathBuf,
    origin: String,
}

#[derive(Debug)]
struct CachedToken {
    secret: SecretString,
    until: Instant,
}

#[derive(Debug, Default)]
struct State {
    token: Option<CachedToken>,
    generation: u64,
}

impl State {
    // The async guard serializes refreshes, including simultaneous 401s for the
    // same generation. A failed/cancelled refresh leaves no rejected token.
    async fn get_with<F, Fut>(
        &mut self,
        rejected: Option<u64>,
        resolve: F,
    ) -> DriverResult<(SecretString, u64)>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = DriverResult<CachedToken>>,
    {
        if let Some(token) = &self.token
            && token.until > Instant::now()
            && rejected != Some(self.generation)
        {
            return Ok((token.secret.clone(), self.generation));
        }
        self.token = None;
        let token = resolve().await?;
        self.generation = self.generation.wrapping_add(1);
        let result = (token.secret.clone(), self.generation);
        self.token = Some(token);
        Ok(result)
    }
}

/// Shared by profile/configuration/workspace, never by compute or tab.
#[derive(Debug)]
pub(super) struct Auth {
    key: Key,
    state: AsyncMutex<State>,
    pub(super) http: reqwest::Client,
}

impl Auth {
    pub(super) fn shared(
        config: &DatabricksConfig,
        config_file: PathBuf,
        origin: &reqwest::Url,
    ) -> DriverResult<Arc<Self>> {
        static CACHE: OnceLock<Mutex<HashMap<Key, Arc<Auth>>>> = OnceLock::new();
        let key = Key {
            profile: config.profile.clone(),
            executable: config
                .cli_path
                .clone()
                .unwrap_or_else(|| "databricks".into()),
            config_file,
            origin: origin.as_str().into(),
        };
        let mut cache = CACHE.get_or_init(Mutex::default).lock().map_err(|_| {
            DriverError::new(ErrorCode::Connection, "Authentication cache is unavailable")
        })?;
        // Bound idle entries; live sessions retain their own Arc. Expired idle
        // credentials are dropped whenever the cache is used.
        cache.retain(|_, auth| {
            Arc::strong_count(auth) > 1
                || auth.state.try_lock().is_ok_and(|state| {
                    state
                        .token
                        .as_ref()
                        .is_some_and(|token| token.until > Instant::now())
                })
        });
        if let Some(auth) = cache.get(&key) {
            return Ok(auth.clone());
        }
        if cache.len() >= 64 {
            cache.retain(|_, auth| Arc::strong_count(auth) > 1);
        }
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|_| super::runtime::protocol())?;
        let auth = Arc::new(Self {
            key: key.clone(),
            state: AsyncMutex::new(State::default()),
            http,
        });
        cache.insert(key, auth.clone());
        Ok(auth)
    }

    pub(super) async fn token(&self, rejected: Option<u64>) -> DriverResult<(SecretString, u64)> {
        self.state
            .lock()
            .await
            .get_with(rejected, || async {
                tokio::time::timeout(Duration::from_secs(60), self.resolve(rejected.is_some()))
                    .await
                    .map_err(|_| {
                        DriverError::new(ErrorCode::Timeout, "CLI authentication timed out")
                    })?
            })
            .await
    }

    pub(super) async fn invalidate(&self, generation: u64) {
        let mut state = self.state.lock().await;
        if state.generation == generation {
            state.token = None;
        }
    }

    async fn resolve(&self, force_refresh: bool) -> DriverResult<CachedToken> {
        let mut args = vec![
            "auth",
            "token",
            "--profile",
            &self.key.profile,
            "--output",
            "json",
        ];
        if force_refresh {
            args.push("--force-refresh");
        }
        let bytes = cli_output(&self.key.executable, &self.key.config_file, &args, 65536).await?;
        let mut value: Value =
            serde_json::from_slice(&bytes).map_err(|_| super::runtime::protocol())?;
        let lifetime = token_lifetime(&value, SystemTime::now())?;
        let secret = value["access_token"].take();
        let secret = secret
            .as_str()
            .filter(|v| !v.is_empty())
            .ok_or_else(super::runtime::protocol)?;
        Ok(CachedToken {
            secret: SecretString::from(secret.to_owned()),
            until: Instant::now() + lifetime,
        })
    }
}

// The CLI serializes oauth2.Token's RFC3339 expiry. UTC offsets are supported
// without adding a date/time dependency just for an authentication deadline.
fn expiry_seconds(text: &str) -> Option<u64> {
    let (date, time) = text.split_once('T')?;
    let mut date = date.split('-');
    let year: u64 = date.next()?.parse().ok()?;
    let month: usize = date.next()?.parse().ok()?;
    let day: u64 = date.next()?.parse().ok()?;
    if date.next().is_some() || !(1970..=9999).contains(&year) || !(1..=12).contains(&month) {
        return None;
    }
    let leap = |y: u64| y.is_multiple_of(4) && (!y.is_multiple_of(100) || y.is_multiple_of(400));
    let months = [
        31,
        if leap(year) { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    if day == 0 || day > *months.get(month - 1)? {
        return None;
    }
    let (clock, offset): (&str, i64) = if let Some(clock) = time.strip_suffix('Z') {
        (clock, 0)
    } else {
        let pos = time.find(['+', '-'])?;
        let zone = time.get(pos + 1..)?;
        let (hours, minutes) = zone.split_once(':')?;
        let hours: i64 = hours.parse().ok()?;
        let minutes: i64 = minutes.parse().ok()?;
        if hours > 23 || minutes > 59 || hours < 0 || minutes < 0 {
            return None;
        }
        (
            time.get(..pos)?,
            (hours * 3600 + minutes * 60)
                * if time.as_bytes().get(pos) == Some(&b'+') {
                    1
                } else {
                    -1
                },
        )
    };
    let mut parts = clock.split(':');
    let hours: u64 = parts.next()?.parse().ok()?;
    let minutes: u64 = parts.next()?.parse().ok()?;
    let seconds = parts.next()?;
    let seconds: u64 = seconds.split('.').next()?.parse().ok()?;
    if parts.next().is_some() || hours > 23 || minutes > 59 || seconds > 59 {
        return None;
    }
    let days: u64 = (1970..year)
        .map(|y| if leap(y) { 366 } else { 365 })
        .sum::<u64>()
        + months.get(..month - 1)?.iter().sum::<u64>()
        + day
        - 1;
    u64::try_from((days * 86400 + hours * 3600 + minutes * 60 + seconds) as i64 - offset).ok()
}

fn token_lifetime(value: &Value, now: SystemTime) -> DriverResult<Duration> {
    let remaining =
        if let Some(expiry) = value["expiry"].as_str().filter(|v| !v.starts_with("0001-")) {
            let expiry = expiry_seconds(expiry).ok_or_else(super::runtime::protocol)?;
            (UNIX_EPOCH + Duration::from_secs(expiry))
                .duration_since(now)
                .unwrap_or_default()
        } else if let Some(seconds) = value["expires_in"].as_u64() {
            Duration::from_secs(seconds)
        } else {
            return Ok(REUSE);
        };
    if remaining <= Duration::from_secs(30) {
        return Err(DriverError::new(
            ErrorCode::Authentication,
            "Databricks CLI returned an expired token; authenticate the profile again",
        ));
    }
    Ok(REUSE.min(remaining - Duration::from_secs(30)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn workspace_profiles_with_account_and_workspace_ids_are_kept_even_when_unvalidated() {
        let profiles = parse_profiles(
            br#"{"profiles":[{
            "name":"free", "host":"https://workspace.invalid", "account_id":"synthetic-account",
            "workspace_id":"100", "cloud":"aws", "auth_type":"databricks-cli",
            "valid":false, "default":true
        }]}"#,
        )
        .unwrap();
        assert_eq!(profiles.len(), 1);
        assert_eq!(profiles[0].name, "free");
        assert!(profiles[0].is_default);
        assert_eq!(
            origin_for_profile(&profiles, "free").unwrap().as_str(),
            "https://workspace.invalid/"
        );
        assert_eq!(CliProfile::preferred(&profiles).unwrap().name, "free");
    }

    #[test]
    fn initial_profile_uses_cli_default_before_the_literal_default_name() {
        let mut profiles = parse_profiles(
            br#"{"profiles":[
            {"name":"DEFAULT","host":"https://legacy.invalid"},
            {"name":"free","host":"https://workspace.invalid","default":true}
        ]}"#,
        )
        .unwrap();
        assert_eq!(CliProfile::preferred(&profiles).unwrap().name, "free");
        for profile in &mut profiles {
            profile.is_default = false;
        }
        assert_eq!(CliProfile::preferred(&profiles).unwrap().name, "DEFAULT");
        profiles.retain(|profile| profile.name != "DEFAULT");
        assert_eq!(CliProfile::preferred(&profiles).unwrap().name, "free");
        assert!(CliProfile::preferred(&[]).is_none());
        let profiles = parse_profiles(
            br#"{"profiles":[
            {"name":"first","host":"https://first.invalid"},
            {"name":"second","host":"https://second.invalid"}
        ]}"#,
        )
        .unwrap();
        assert!(CliProfile::preferred(&profiles).is_none());
    }

    #[test]
    fn account_console_hosts_are_excluded_even_with_workspace_metadata() {
        for host in [
            "https://accounts.cloud.databricks.com",
            "https://accounts.azuredatabricks.net",
            "https://accounts.gcp.databricks.com",
            "https://accounts-dod.cloud.databricks.us",
        ] {
            let profiles = parse_profiles(&serde_json::to_vec(&json!({"profiles":[{
                "name":"account","host":host,"account_id":"synthetic","workspace_id":"100","default":true
            }]})).unwrap()).unwrap();
            assert!(profiles.is_empty());
        }
    }

    #[test]
    fn connection_origin_uses_cli_metadata_and_never_substitutes_another_profile() {
        let profiles = parse_profiles(
            br#"{"profiles":[
            {"name":"DEFAULT","host":"https://default.invalid"},
            {"name":"selected","host":"https://selected.invalid:443/"},
            {"name":"missing-host"},
            {"name":"account","host":"https://accounts.invalid","account_id":"123"}
        ]}"#,
        )
        .unwrap();
        assert_eq!(
            origin_for_profile(&profiles, "selected").unwrap().as_str(),
            "https://selected.invalid/"
        );
        assert_eq!(
            origin_for_profile(&profiles, "absent-private-sentinel")
                .unwrap_err()
                .code,
            ErrorCode::NotFound
        );
        assert_eq!(
            origin_for_profile(&profiles, "missing-host")
                .unwrap_err()
                .code,
            ErrorCode::Validation
        );
        assert_eq!(
            origin_for_profile(&profiles, "account").unwrap_err().code,
            ErrorCode::NotFound
        );
        assert!(
            !format!(
                "{:?}",
                origin_for_profile(&profiles, "absent-private-sentinel").unwrap_err()
            )
            .contains("private-sentinel")
        );
    }

    #[test]
    fn connection_origin_rejects_unsafe_hosts_returned_by_the_cli() {
        for host in [
            "http://workspace.invalid",
            "https://user:private-sentinel@workspace.invalid",
            "https://workspace.invalid/api",
            "https://workspace.invalid/?token=private-sentinel",
            "https://workspace.invalid/#fragment",
        ] {
            let profiles = parse_profiles(
                &serde_json::to_vec(&json!({"profiles":[{"name":"fixture","host":host}]})).unwrap(),
            )
            .unwrap();
            let failure = origin_for_profile(&profiles, "fixture").unwrap_err();
            assert_eq!(failure.code, ErrorCode::Validation);
            assert!(!format!("{failure:?}").contains("private-sentinel"));
        }
    }

    #[test]
    fn cli_profile_listing_keeps_only_safe_workspace_metadata() {
        let profiles = parse_profiles(
            br#"{"profiles":[
            {"name":"ZETA","host":"https://zeta.invalid","valid":false},
            {"name":"ACCOUNT","host":"https://accounts.invalid","account_id":"123"},
            {"name":"ALPHA","host":"https://alpha.invalid","token":"private-sentinel"},
            {"name":"ALPHA","host":"https://alpha.invalid"},
            {"name":"HIDDEN_HOST","host":"https://user:private-sentinel@host.invalid"}
        ]}"#,
        )
        .unwrap();
        assert_eq!(
            profiles.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(),
            vec!["ALPHA", "HIDDEN_HOST", "ZETA"]
        );
        assert_eq!(profiles[0].host.as_deref(), Some("https://alpha.invalid"));
        assert_eq!(profiles[1].host, None);
        assert!(!format!("{profiles:?}").contains("private-sentinel"));
        assert!(parse_profiles(br#"{"profiles":[{"name":"bad\nprofile"}]}"#).is_err());
        assert!(parse_profiles(br#"{"profiles":null}"#).unwrap().is_empty());
    }

    #[tokio::test]
    async fn profile_discovery_honors_prior_cancellation_and_rejects_shell_scripts() {
        let cancel = jykre_core::CancellationToken::new();
        cancel.cancel();
        assert_eq!(
            list_profiles(Some(Path::new("missing-fixture.exe")), cancel)
                .await
                .unwrap_err()
                .code,
            ErrorCode::Cancelled
        );
        assert_eq!(
            list_profiles(
                Some(Path::new("fixture.cmd")),
                jykre_core::CancellationToken::new()
            )
            .await
            .unwrap_err()
            .code,
            ErrorCode::Validation
        );
    }
    #[tokio::test]
    async fn cache_identity_includes_profile_origin_config_file_and_cli() {
        let config = DatabricksConfig::parse(&json!({"profile":"cache-fixture"})).unwrap();
        let origin = reqwest::Url::parse("https://cache-fixture.invalid/").unwrap();
        let auth = Auth::shared(&config, "cache-fixture.cfg".into(), &origin).unwrap();
        assert!(Arc::ptr_eq(
            &auth,
            &Auth::shared(&config, "cache-fixture.cfg".into(), &origin).unwrap()
        ));
        assert!(!Arc::ptr_eq(
            &auth,
            &Auth::shared(&config, "other.cfg".into(), &origin).unwrap()
        ));
        let other = DatabricksConfig::parse(&json!({"profile":"other"})).unwrap();
        assert!(!Arc::ptr_eq(
            &auth,
            &Auth::shared(&other, "cache-fixture.cfg".into(), &origin).unwrap()
        ));
        let other =
            DatabricksConfig::parse(&json!({"profile":"cache-fixture","cliPath":"other.exe"}))
                .unwrap();
        assert!(!Arc::ptr_eq(
            &auth,
            &Auth::shared(&other, "cache-fixture.cfg".into(), &origin).unwrap()
        ));
        let other = reqwest::Url::parse("https://other.invalid/").unwrap();
        assert!(!Arc::ptr_eq(
            &auth,
            &Auth::shared(&config, "cache-fixture.cfg".into(), &other).unwrap()
        ));
        let mut state = auth.state.lock().await;
        state
            .get_with(None, || async {
                Ok(CachedToken {
                    secret: SecretString::from("private-sentinel".to_owned()),
                    until: Instant::now() + REUSE,
                })
            })
            .await
            .unwrap();
        assert!(!format!("{state:?}").contains("private-sentinel"));
        assert!(
            state
                .get_with(Some(1), || async {
                    Err(DriverError::new(ErrorCode::Authentication, "test failure"))
                })
                .await
                .is_err()
        );
        assert!(state.token.is_none());
    }
    #[test]
    fn expiry_bounds_reuse_and_handles_offsets_and_leap_days() {
        let now = UNIX_EPOCH + Duration::from_secs(expiry_seconds("2024-02-29T12:00:00Z").unwrap());
        assert_eq!(
            token_lifetime(&json!({"expiry":"2024-02-29T09:01:00-03:00"}), now).unwrap(),
            Duration::from_secs(30)
        );
        assert_eq!(
            token_lifetime(&json!({"expires_in":3600}), now).unwrap(),
            REUSE
        );
        assert!(token_lifetime(&json!({"expiry":"2024-02-29T12:00:00Z"}), now).is_err());
        assert!(expiry_seconds("2023-02-29T12:00:00Z").is_none());
        assert_eq!(token_lifetime(&json!({}), now).unwrap(), REUSE);
    }
    #[tokio::test]
    async fn concurrent_refreshes_coalesce_and_rejected_generations_do_not_replace_fresh_tokens() {
        let state = AsyncMutex::new(State::default());
        let calls = std::sync::atomic::AtomicUsize::new(0);
        let load = || async {
            calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            tokio::task::yield_now().await;
            Ok(CachedToken {
                secret: SecretString::from("fixture".to_owned()),
                until: Instant::now() + REUSE,
            })
        };
        let state = &state;
        let get =
            |rejected| async move { state.lock().await.get_with(rejected, load).await.unwrap().1 };
        let (a, b) = tokio::join!(get(None), get(None));
        assert_eq!(a, b);
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        let (a, b) = tokio::join!(get(Some(a)), get(Some(b)));
        assert_eq!(a, b);
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);
        state.lock().await.token.as_mut().unwrap().until = Instant::now();
        get(None).await;
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 3);
    }
}
