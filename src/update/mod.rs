//! Updates from GitHub releases ([ADR-0033](../../docs/adr/0033-update-check-and-signed-self-install.md)).
//!
//! Once a day (on by default, off in Preferences) the release list is read
//! anonymously; a newer version, pre-releases included, is offered in the
//! status bar and in Preferences › About. Nothing is downloaded or replaced
//! without the user's click. An archive is installed only when it verifies
//! against the release key compiled in ([`keys`]), and only in the folder
//! the release workflow ships ([`layout`]); entries are swapped with a
//! rollback ([`swap`]). The application then restarts through GPUI.
//!
//! Kept in `config.db`'s `settings`: `update.preferences`
//! (`{"check": true}`) and `update.state` (`{"last_checked_ms": …}`).
use std::path::Path;
use std::time::Duration;

use serde_json::{Value, json};

use crate::connection_store::LatestWrite;
use crate::drivers::DriverResult;

pub mod client;
pub mod download;
pub mod error;
pub mod install;
pub mod keys;
pub mod layout;
pub mod release;
pub mod state;
pub mod swap;

pub use client::{CheckFailure, Endpoints, check};
pub use download::Progress;
pub use install::{Installed, Reports, install};
pub use layout::{InstallDir, Installability, ManualReason};
pub use release::{Candidate, Platform};
pub use state::{Actions, Phase, UpdateState};

/// The setting that holds [`UpdatePreferences`].
pub const PREFERENCES_KEY: &str = "update.preferences";
/// The setting that holds [`UpdateRecord`].
pub const RECORD_KEY: &str = "update.state";

/// How long after the window opens the first automatic check may run.
pub const FIRST_CHECK_DELAY: Duration = Duration::from_secs(30);
/// Automatic checks run at most this often.
pub const CHECK_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);
/// After a check that failed (no network, GitHub limiting requests), the
/// next one waits at least this long.
pub const RETRY_AFTER_FAILURE: Duration = Duration::from_secs(60 * 60);

/// Whether the application checks for updates by itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UpdatePreferences {
    pub check: bool,
}

impl Default for UpdatePreferences {
    fn default() -> Self {
        Self { check: true }
    }
}

impl UpdatePreferences {
    /// Reads the stored setting; a missing or unreadable value checks.
    pub fn from_value(value: Option<&Value>) -> Self {
        Self {
            check: value
                .and_then(|value| value.get("check"))
                .and_then(Value::as_bool)
                .unwrap_or(true),
        }
    }

    pub fn to_value(self) -> Value {
        json!({ "check": self.check })
    }
}

/// When the last check that reached GitHub ended.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct UpdateRecord {
    /// Milliseconds since the Unix epoch.
    pub last_checked_ms: Option<u64>,
}

impl UpdateRecord {
    pub fn from_value(value: Option<&Value>) -> Self {
        Self {
            last_checked_ms: value
                .and_then(|value| value.get("last_checked_ms"))
                .and_then(Value::as_u64),
        }
    }

    pub fn to_value(self) -> Value {
        json!({ "last_checked_ms": self.last_checked_ms })
    }
}

/// How long until the next automatic check, or `None` when none runs:
/// turned off, or not `automatic` (a development build). Never sooner than
/// [`FIRST_CHECK_DELAY`]; a last check in the future (the clock went back)
/// counts as due.
pub fn next_check_in(
    now_ms: u64,
    record: &UpdateRecord,
    preferences: &UpdatePreferences,
    automatic: bool,
) -> Option<Duration> {
    if !preferences.check || !automatic {
        return None;
    }
    let wait = match record.last_checked_ms {
        Some(last) if last <= now_ms => {
            let since = Duration::from_millis(now_ms - last);
            CHECK_INTERVAL.saturating_sub(since)
        }
        _ => Duration::ZERO,
    };
    Some(wait.max(FIRST_CHECK_DELAY))
}

/// Whether this build checks by itself: release builds only.
pub fn automatic_checks() -> bool {
    !cfg!(debug_assertions)
}

/// Milliseconds since the Unix epoch, now.
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

/// The `User-Agent` of every request: the application and its version,
/// nothing else.
pub fn user_agent() -> String {
    format!("JykreStudio/{}", env!("CARGO_PKG_VERSION"))
}

/// Writes of [`PREFERENCES_KEY`]; the last one asked for wins.
pub static PREFERENCE_WRITES: LatestWrite = LatestWrite::new();
/// Writes of [`RECORD_KEY`]; the last one asked for wins.
pub static RECORD_WRITES: LatestWrite = LatestWrite::new();

/// Reads the preferences and the record kept in the data folder `dir`.
pub async fn load(dir: &Path) -> DriverResult<(UpdatePreferences, UpdateRecord)> {
    let repository = jykre_storage::ConfigRepository::open(dir).await?;
    let preferences = repository.setting(PREFERENCES_KEY).await;
    let record = repository.setting(RECORD_KEY).await;
    repository.close().await;
    Ok((
        UpdatePreferences::from_value(preferences?.as_ref()),
        UpdateRecord::from_value(record?.as_ref()),
    ))
}

/// Saves the preferences for the write `ticket` of [`PREFERENCE_WRITES`].
pub async fn save_preferences(
    dir: &Path,
    preferences: UpdatePreferences,
    ticket: u64,
) -> DriverResult<()> {
    PREFERENCE_WRITES
        .run(ticket, write(dir, PREFERENCES_KEY, preferences.to_value()))
        .await
        .unwrap_or(Ok(()))
}

/// Saves the record for the write `ticket` of [`RECORD_WRITES`].
pub async fn save_record(dir: &Path, record: UpdateRecord, ticket: u64) -> DriverResult<()> {
    RECORD_WRITES
        .run(ticket, write(dir, RECORD_KEY, record.to_value()))
        .await
        .unwrap_or(Ok(()))
}

async fn write(dir: &Path, key: &str, value: Value) -> DriverResult<()> {
    let repository = jykre_storage::ConfigRepository::open(dir).await?;
    let result = repository.set_setting(key, &value).await;
    repository.close().await;
    result
}

#[cfg(test)]
mod tests;
