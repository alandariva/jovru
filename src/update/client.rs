//! The requests to GitHub: the release list, and the HTTP clients the list
//! and the downloads use.
//!
//! Both clients use rustls with ring and the system's trust store, HTTPS
//! only, and send no `Referer`, cookie or token. The list follows no
//! redirect. A download follows at most [`MAX_REDIRECTS`], only to GitHub's
//! download hosts ([`Endpoints::redirect_allowed`]).
use std::time::Duration;

use reqwest_genai::Url;
use reqwest_genai::redirect::{Attempt, Policy};
use tracing::Instrument;

use crate::drivers::{CancellationToken, DriverError, DriverResult, ErrorCode};
use crate::tr;

use super::error;
use super::release::{Candidate, OWNER, Platform, REPOSITORY, ReleaseEntry, select_update};
use super::{CHECK_INTERVAL, RETRY_AFTER_FAILURE, user_agent};

/// Connecting to GitHub.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// Reading the whole release list.
pub const CHECK_TIMEOUT: Duration = Duration::from_secs(15);
/// Silence while a download's body arrives.
pub const READ_TIMEOUT: Duration = Duration::from_secs(30);
/// Redirects a download may follow.
pub const MAX_REDIRECTS: usize = 3;

/// The hosts GitHub serves release files from, after redirecting the API's
/// asset address.
pub const DOWNLOAD_HOSTS: &[&str] = &[
    "objects.githubusercontent.com",
    "release-assets.githubusercontent.com",
];

/// Where the requests go. [`Endpoints::github`] in the application; tests
/// point it at a server on this computer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Endpoints {
    /// The API's base address, without a trailing `/`.
    pub api_base: String,
    /// Hosts a download may be redirected to.
    pub download_hosts: Vec<String>,
    /// Plain HTTP on this computer (tests only).
    loopback: bool,
}

impl Endpoints {
    pub fn github() -> Self {
        Self {
            api_base: "https://api.github.com".to_owned(),
            download_hosts: DOWNLOAD_HOSTS
                .iter()
                .map(|host| (*host).to_owned())
                .collect(),
            loopback: false,
        }
    }

    /// A server on this computer at `base` (`http://127.0.0.1:port`).
    #[cfg(test)]
    pub(crate) fn loopback(base: &str) -> Self {
        Self {
            api_base: base.to_owned(),
            download_hosts: vec!["127.0.0.1".to_owned()],
            loopback: true,
        }
    }

    /// Whether `url` is the API address of one of the repository's release
    /// files (`…/repos/<owner>/<repo>/releases/assets/<number>`).
    pub fn asset_url_allowed(&self, url: &str) -> bool {
        let prefix = format!(
            "{}/repos/{OWNER}/{REPOSITORY}/releases/assets/",
            self.api_base
        );
        url.strip_prefix(&prefix)
            .is_some_and(|id| !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit()))
    }

    /// Whether a download may follow a redirect to `url`, after `hops`
    /// earlier addresses: HTTPS on the standard port to a download host,
    /// without a user or password, at most [`MAX_REDIRECTS`] times.
    pub fn redirect_allowed(&self, url: &Url, hops: usize) -> bool {
        let scheme_ok = url.scheme() == "https" && url.port().is_none()
            || self.loopback && url.scheme() == "http";
        hops <= MAX_REDIRECTS
            && scheme_ok
            && url.username().is_empty()
            && url.password().is_none()
            && url
                .host_str()
                .is_some_and(|host| self.download_hosts.iter().any(|allowed| allowed == host))
    }

    fn builder(&self) -> reqwest_genai::ClientBuilder {
        // The HTTP client panics when no provider is installed; another part
        // of the process may have installed ring first.
        let _ = rustls::crypto::ring::default_provider().install_default();
        reqwest_genai::Client::builder()
            .https_only(!self.loopback)
            .referer(false)
            .user_agent(user_agent())
            .connect_timeout(CONNECT_TIMEOUT)
    }

    /// The client of the release list: no redirect.
    pub fn api_client(&self) -> DriverResult<reqwest_genai::Client> {
        self.builder()
            .timeout(CHECK_TIMEOUT)
            .redirect(Policy::none())
            .build()
            .map_err(|_| client_failure())
    }

    /// The client of a download: redirects to the download hosts only, and
    /// a bounded silence; the whole download's limit is the caller's.
    pub fn download_client(&self) -> DriverResult<reqwest_genai::Client> {
        let endpoints = self.clone();
        self.builder()
            .read_timeout(READ_TIMEOUT)
            .redirect(Policy::custom(move |attempt: Attempt<'_>| {
                if endpoints.redirect_allowed(attempt.url(), attempt.previous().len()) {
                    attempt.follow()
                } else {
                    // The redirect itself comes back, and is refused by status.
                    attempt.stop()
                }
            }))
            .build()
            .map_err(|_| client_failure())
    }
}

fn client_failure() -> DriverError {
    DriverError::new(
        ErrorCode::Internal,
        tr!("Could not prepare the HTTP client for updates"),
    )
}

/// A check that did not reach an answer, and when to try again.
#[derive(Clone, Debug)]
pub struct CheckFailure {
    pub error: DriverError,
    /// Never sooner than [`RETRY_AFTER_FAILURE`]; when GitHub limits
    /// requests, after the wait it asks for (at most a day).
    pub retry_in: Duration,
}

/// Reads the release list and returns the newest version above `current`,
/// if any. Runs on the database runtime.
pub async fn check(
    endpoints: &Endpoints,
    current: &str,
    platform: Option<Platform>,
    cancel: &CancellationToken,
) -> Result<Option<Candidate>, CheckFailure> {
    let span = crate::client_span!(
        "update_check",
        http.request.method = "GET",
        url.template = "/repos/{owner}/{repo}/releases",
    );
    let result = async {
        tokio::select! {
            listed = tokio::time::timeout(CHECK_TIMEOUT, list(endpoints)) => match listed {
                Ok(listed) => listed,
                Err(_) => Err(failure(error::timeout(), None)),
            },
            () = cancel.cancelled() => Err(failure(DriverError::cancelled(), None)),
        }
    }
    .instrument(span.clone())
    .await;
    let outcome = result
        .as_ref()
        .map(|_| ())
        .map_err(|failed| failed.error.clone());
    crate::telemetry::attributes::record_outcome(&span, &outcome);
    match &result {
        Ok(listed) => tracing::info!(releases = listed.len(), "update check finished"),
        Err(failed) => tracing::info!(error_code = ?failed.error.code, "update check failed"),
    }
    result.map(|releases| select_update(current, &releases, platform))
}

async fn list(endpoints: &Endpoints) -> Result<Vec<ReleaseEntry>, CheckFailure> {
    let client = endpoints
        .api_client()
        .map_err(|error| failure(error, None))?;
    let releases = self_update::backends::github::ReleaseList::configure()
        .repo_owner(OWNER)
        .repo_name(REPOSITORY)
        .api_base_url(endpoints.api_base.as_str())
        .reqwest_async_client(client)
        .timeout(CHECK_TIMEOUT)
        .request_header(self_update::http::header::USER_AGENT, user_agent())
        .request_header(
            self_update::http::header::ACCEPT,
            "application/vnd.github+json",
        )
        .request_header("X-GitHub-Api-Version", "2022-11-28")
        .build()
        .map_err(|error| failure(error::from_update_error(&error), None))?
        .fetch_async()
        .await
        .map_err(|error| {
            let wait = error.rate_limit_delay();
            failure(error::from_update_error(&error), wait)
        })?;
    Ok(releases
        .all()
        .iter()
        .map(ReleaseEntry::from_release)
        .collect())
}

fn failure(error: DriverError, wait: Option<Duration>) -> CheckFailure {
    CheckFailure {
        error,
        retry_in: wait
            .unwrap_or(RETRY_AFTER_FAILURE)
            .clamp(RETRY_AFTER_FAILURE, CHECK_INTERVAL),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(text: &str) -> Url {
        Url::parse(text).unwrap()
    }

    #[test]
    fn only_the_repository_release_files_are_downloaded() {
        let github = Endpoints::github();
        assert!(github.asset_url_allowed(
            "https://api.github.com/repos/alandariva/jykre-studio/releases/assets/123456"
        ));
        for refused in [
            "https://api.github.com/repos/other/jykre-studio/releases/assets/1",
            "https://api.github.com/repos/alandariva/jykre-studio/releases/assets/",
            "https://api.github.com/repos/alandariva/jykre-studio/releases/assets/1/../2",
            "http://api.github.com/repos/alandariva/jykre-studio/releases/assets/1",
            "https://evil.example/repos/alandariva/jykre-studio/releases/assets/1",
        ] {
            assert!(!github.asset_url_allowed(refused), "{refused}");
        }
    }

    #[test]
    fn downloads_follow_redirects_to_github_download_hosts_only() {
        let github = Endpoints::github();
        let cdn = url("https://release-assets.githubusercontent.com/a/b?sig=x");
        assert!(github.redirect_allowed(&cdn, 1));
        assert!(github.redirect_allowed(&url("https://objects.githubusercontent.com/x"), 3));
        assert!(!github.redirect_allowed(&cdn, MAX_REDIRECTS + 1));
        for refused in [
            "http://release-assets.githubusercontent.com/a",
            "https://release-assets.githubusercontent.com:8443/a",
            "https://user:pass@release-assets.githubusercontent.com/a",
            "https://api.github.com/repos/x",
            "https://githubusercontent.com.evil.example/a",
        ] {
            assert!(!github.redirect_allowed(&url(refused), 1), "{refused}");
        }
    }

    #[test]
    fn a_failed_check_waits_at_least_an_hour_and_at_most_a_day() {
        let error = || DriverError::new(ErrorCode::Connection, "x");
        assert_eq!(failure(error(), None).retry_in, RETRY_AFTER_FAILURE);
        assert_eq!(
            failure(error(), Some(Duration::from_secs(5))).retry_in,
            RETRY_AFTER_FAILURE
        );
        assert_eq!(
            failure(error(), Some(Duration::from_secs(90_000))).retry_in,
            CHECK_INTERVAL
        );
    }
}
