//! Application connection service: validation, SQLite migration and vault policy.
use crate::tr;
use crate::{
    backend::{self, MetadataSnapshot},
    drivers::*,
    metadata_scope::MetadataScope,
    result::{DEFAULT_ROW_LIMIT, MAX_ROW_LIMIT},
};
use jykre_secrets::SecretStore;
use jykre_storage::{ConfigRepository, MetadataRepository};
use serde_json::Value;
use std::{
    ffi::OsString,
    path::{Path, PathBuf},
};

/// Folder name inside the platform data directory, following each OS convention.
const APP_DIR: &str = if cfg!(any(windows, target_os = "macos")) {
    "JykreStudio"
} else {
    "jykre-studio"
};

/// `JYKRE_CONFIG_DIR`, else the per-user local data directory:
/// `%LOCALAPPDATA%` on Windows, `~/Library/Application Support` on macOS and
/// `$XDG_DATA_HOME` (default `~/.local/share`) on Linux.
pub fn data_dir() -> PathBuf {
    resolve_data_dir(std::env::var_os("JYKRE_CONFIG_DIR"), dirs::data_local_dir())
}

fn resolve_data_dir(configured: Option<OsString>, platform: Option<PathBuf>) -> PathBuf {
    configured
        .filter(|dir| !dir.is_empty())
        .map(PathBuf::from)
        .or_else(|| platform.map(|base| base.join(APP_DIR)))
        // Only without a home directory; the app then keeps data next to it.
        .unwrap_or_else(|| PathBuf::from(".jykre"))
}
pub fn default_path() -> PathBuf {
    data_dir().join("config.db")
}

pub struct LoadedConnection {
    pub profile: ConnectionProfile,
    pub input: ResolvedConnection,
    /// Rows kept per statement, between 1 and [`MAX_ROW_LIMIT`].
    pub row_limit: u32,
    /// What the user chose to introspect; pending when nothing was chosen.
    pub scope: MetadataScope,
    pub cache: Option<MetadataSnapshot>,
    pub warning: Option<String>,
}

fn legacy_profiles(bytes: &[u8]) -> DriverResult<Vec<ConnectionProfile>> {
    let values: Value = serde_json::from_slice(bytes).map_err(|_| {
        DriverError::validation(tr!(
            "The legacy connections.json is invalid; no data was removed."
        ))
    })?;
    let values = values
        .as_array()
        .ok_or_else(|| DriverError::validation(tr!("Invalid legacy connections format.")))?;
    let registry = builtin_registry()?;
    values
        .iter()
        .map(|value| {
            let name = value["name"]
                .as_str()
                .filter(|name| !name.trim().is_empty() && !name.chars().any(char::is_control))
                .ok_or_else(|| DriverError::validation(tr!("Invalid legacy connection name.")))?
                .to_owned();
            let config: ConnectionConfig = serde_json::from_value(value["config"].clone())
                .map_err(|_| DriverError::validation(tr!("Invalid legacy configuration.")))?;
            registry.get(config.driver)?.validate_connection(&config)?;
            Ok(ConnectionProfile {
                id: ConnectionId::new(),
                name,
                config,
                remember_password: false,
                revision: 0,
            })
        })
        .collect()
}

/// The saved connections that could be opened, and what was left out.
#[derive(Default)]
pub struct LoadedConnections {
    pub connections: Vec<LoadedConnection>,
    /// One message per connection (or migration step) that could not be
    /// loaded; the others are loaded regardless.
    pub problems: Vec<String>,
}

/// Loads every saved connection. A profile that cannot be read or is no
/// longer valid is left out with a problem, so it never hides the others.
pub async fn load(dir: &Path, secrets: &dyn SecretStore) -> DriverResult<LoadedConnections> {
    let repository = ConfigRepository::open(dir).await?;
    let result = load_from(dir, secrets, &repository).await;
    repository.close().await;
    result
}

async fn load_from(
    dir: &Path,
    secrets: &dyn SecretStore,
    repository: &ConfigRepository,
) -> DriverResult<LoadedConnections> {
    let mut problems = Vec::new();
    let legacy = dir.join("connections.json");
    let bytes = match tokio::fs::read(&legacy).await {
        Ok(bytes) => Some(bytes),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(_) => {
            problems.push(tr!("Could not read the legacy connections.json.").to_owned());
            None
        }
    };
    // A legacy file that cannot be read is kept and not imported, so the
    // next start tries again; the connections already in SQLite still load.
    let old = bytes.as_ref().map(|bytes| legacy_profiles(bytes));
    if let Some(Err(error)) = &old {
        problems.push(error.to_string());
    }
    if !repository.legacy_imported().await? {
        match &old {
            Some(Err(_)) => {}
            Some(Ok(imported)) => repository.import_legacy(imported).await?,
            None => repository.import_legacy(&[]).await?,
        }
    }
    let mut profiles = Vec::new();
    for profile in repository.list_each().await? {
        match profile {
            Ok(profile) => profiles.push(profile),
            Err(unreadable) => problems.push(match unreadable.name {
                Some(name) => tr!(
                    "The connection {0} could not be read and is not shown: {1}",
                    name,
                    unreadable.error
                ),
                None => tr!(
                    "A saved connection could not be read and is not shown: {0}",
                    unreadable.error
                ),
            }),
        }
    }
    let mut migration_warning = None;
    if let Some(Ok(old)) = old {
        // A previous crash may have committed SQLite before unlinking the JSON.
        // Delete only if every legacy profile is already represented identically.
        if !old.iter().all(|old| {
            profiles
                .iter()
                .any(|new| new.name == old.name && new.config == old.config)
        }) {
            problems.push(
                tr!(
                    "The legacy JSON changed after the migration. It was preserved to avoid losing connections."
                )
                .to_owned(),
            );
        } else if tokio::fs::remove_file(legacy).await.is_err() {
            migration_warning = Some(
                tr!("Migration completed; could not delete the legacy connections.json.")
                    .to_owned(),
            );
        }
    }
    let registry = builtin_registry()?;
    let mut loaded = Vec::new();
    for profile in profiles {
        if let Err(error) = registry
            .get(profile.config.driver)
            .and_then(|driver| driver.validate_connection(&profile.config))
        {
            problems.push(tr!(
                "The connection {0} is not valid and is not shown: {1}",
                profile.name,
                error
            ));
            continue;
        }
        let mut warning = migration_warning.clone();
        let credentials = if profile.remember_password
            && profile.config.driver != DriverKind::Databricks
        {
            match secrets.get(profile.id).await {
                Ok(value) => {
                    if value.password.is_none() {
                        add_warning(
                            &mut warning,
                            tr!(
                                "Remember password is on, but the vault has no password for this connection. Enter it again to query the server."
                            ),
                        );
                    }
                    value
                }
                Err(error) => {
                    tracing::warn!(
                        connection_id = %profile.id,
                        error_code = ?error.code,
                        "password unavailable from the vault"
                    );
                    add_warning(&mut warning, error.to_string());
                    Credentials::default()
                }
            }
        } else {
            Credentials::default()
        };
        let cache = match prepare_cache(dir, &profile, true).await {
            Ok(cache) => Some(cache),
            Err(error) => {
                add_warning(&mut warning, error.to_string());
                None
            }
        };
        let input = ResolvedConnection {
            config: profile.config.clone(),
            credentials,
        };
        let row_limit = match stored_row_limit(repository, profile.id).await {
            Ok(limit) => limit,
            Err(error) => {
                add_warning(
                    &mut warning,
                    tr!(
                        "The saved row limit could not be read; the default is used: {0}",
                        error
                    ),
                );
                DEFAULT_ROW_LIMIT
            }
        };
        let scope = match stored_scope(dir, repository, &profile).await {
            Ok((scope, derived)) => {
                if derived {
                    add_warning(
                        &mut warning,
                        tr!(
                            "What to introspect was taken from the metadata already saved. Review it in the connection settings."
                        ),
                    );
                }
                scope
            }
            Err(error) => {
                add_warning(
                    &mut warning,
                    tr!(
                        "What to introspect could not be read; choose it again in the connection settings: {0}",
                        error
                    ),
                );
                MetadataScope::default()
            }
        };
        loaded.push(LoadedConnection {
            profile,
            input,
            row_limit,
            scope,
            cache,
            warning,
        });
    }
    Ok(LoadedConnections {
        connections: loaded,
        problems,
    })
}

/// What a saved connection keeps besides its profile and password.
pub struct Preferences {
    /// Rows kept per statement, between 1 and [`MAX_ROW_LIMIT`].
    pub row_limit: u32,
    /// What to introspect; pending (nothing chosen) is allowed, for example
    /// when the server could not be reached to list its namespaces.
    pub scope: MetadataScope,
}

/// Saves a profile, its preferences and, when asked to remember it, its
/// password.
pub async fn save(
    dir: &Path,
    profile: ConnectionProfile,
    credentials: Credentials,
    preferences: Preferences,
    secrets: &dyn SecretStore,
) -> DriverResult<LoadedConnection> {
    builtin_registry()?
        .get(profile.config.driver)?
        .validate_connection(&profile.config)?;
    validate_row_limit(preferences.row_limit)?;
    let repository = ConfigRepository::open(dir).await?;
    let result = save_to(dir, profile, credentials, preferences, secrets, &repository).await;
    repository.close().await;
    result
}
async fn save_to(
    dir: &Path,
    mut profile: ConnectionProfile,
    credentials: Credentials,
    preferences: Preferences,
    secrets: &dyn SecretStore,
    repository: &ConfigRepository,
) -> DriverResult<LoadedConnection> {
    let Preferences { row_limit, scope } = preferences;
    // Another profile that cannot be read does not block this one.
    let previous = repository
        .list_each()
        .await?
        .into_iter()
        .flatten()
        .find(|value| value.id == profile.id);
    let mut warning = None;
    // The vault is written before the profile, so `remember_password` is
    // saved only when the vault really holds the password.
    let remembered_before = previous.as_ref().is_some_and(|previous| {
        previous.remember_password && keeps_password(&previous.config, &profile.config)
    });
    let mut stored = false;
    if profile.config.driver != DriverKind::Databricks && profile.remember_password {
        match credentials.password.clone() {
            Some(password) => match secrets.set(profile.id, password).await {
                Ok(()) => stored = true,
                Err(error) => {
                    profile.remember_password = false;
                    add_warning(
                        &mut warning,
                        tr!(
                            "Connection saved. Could not store the password in the vault, so Remember password was turned off: {0}",
                            error
                        ),
                    );
                }
            },
            // Nothing new to store: the vault may still hold the password,
            // only for the same server and user.
            None if remembered_before
                && secrets
                    .get(profile.id)
                    .await
                    .is_ok_and(|stored| stored.password.is_some()) => {}
            None => {
                profile.remember_password = false;
                add_warning(
                    &mut warning,
                    tr!(
                        "Connection saved without an available password, so Remember password was turned off. Enter the password before querying the server."
                    ),
                );
            }
        }
    } else {
        profile.remember_password = false;
    }
    let profile = match repository.save(&profile).await {
        Ok(profile) => profile,
        Err(error) => {
            // A new connection that was not saved must not leave its
            // password behind.
            if stored && previous.is_none() {
                let _ = secrets.delete(profile.id).await;
            }
            return Err(error);
        }
    };
    repository.set_row_limit(profile.id, row_limit).await?;
    repository
        .set_metadata_scope(profile.id, &scope.to_value())
        .await?;
    let had_entry = previous
        .as_ref()
        .is_some_and(|previous| previous.remember_password);
    // Remember turned off (by the user or above): the old entry goes, so a
    // password for another server or user is never read back.
    if had_entry && !profile.remember_password && secrets.delete(profile.id).await.is_err() {
        add_warning(
            &mut warning,
            tr!(
                "Remember password was turned off, but the previous password could not be removed from the vault. It will not be used when reopening."
            ),
        );
    }
    let cache = match prepare_cache(dir, &profile, false).await {
        Ok(value) => Some(value),
        Err(error) => {
            add_warning(
                &mut warning,
                tr!("Connection saved. Cache unavailable: {0}", error),
            );
            None
        }
    };
    let input = ResolvedConnection {
        config: profile.config.clone(),
        credentials,
    };
    Ok(LoadedConnection {
        profile,
        input,
        row_limit,
        scope,
        cache,
        warning,
    })
}

/// Adds `text` to the warnings already collected, so a later one does not
/// hide an earlier one.
fn add_warning(warning: &mut Option<String>, text: impl Into<String>) {
    let text = text.into();
    *warning = Some(match warning.take() {
        Some(previous) => format!("{previous} {text}"),
        None => text,
    });
}

/// Whether a password kept for `previous` may be sent to `next`: the same
/// driver, host, port and user. Changing any of them points the credential
/// at another identity, so an empty password field must not carry it over.
pub fn keeps_password(previous: &ConnectionConfig, next: &ConnectionConfig) -> bool {
    let text = |config: &ConnectionConfig, key: &str| -> String {
        match &config.options[key] {
            Value::String(value) => value.trim().to_owned(),
            Value::Null => String::new(),
            other => other.to_string(),
        }
    };
    previous.driver == next.driver
        && text(previous, "host").eq_ignore_ascii_case(&text(next, "host"))
        && ["port", "user"]
            .iter()
            .all(|key| text(previous, key) == text(next, key))
}

/// Checks a row limit typed by the user.
pub fn validate_row_limit(row_limit: u32) -> DriverResult<()> {
    if (1..=MAX_ROW_LIMIT).contains(&row_limit) {
        Ok(())
    } else {
        Err(DriverError::validation(tr!(
            "Enter a row limit between 1 and {0}.",
            crate::cell_format::count(MAX_ROW_LIMIT as usize)
        )))
    }
}

/// The saved row limit, or the default when none was saved or it is out of
/// range (for example, written by a newer version with a higher maximum).
async fn stored_row_limit(repository: &ConfigRepository, id: ConnectionId) -> DriverResult<u32> {
    Ok(repository
        .row_limit(id)
        .await?
        .filter(|limit| *limit <= MAX_ROW_LIMIT)
        .unwrap_or(DEFAULT_ROW_LIMIT))
}

/// The saved choice of what to introspect, and whether it was just taken
/// from a cache written before choosing existed: such a cache holds what the
/// user had read, so its last-level namespaces become the choice, once. A
/// connection with neither is pending.
async fn stored_scope(
    dir: &Path,
    repository: &ConfigRepository,
    profile: &ConnectionProfile,
) -> DriverResult<(MetadataScope, bool)> {
    if let Some(value) = repository.metadata_scope(profile.id).await? {
        return MetadataScope::from_value(&value)
            .map(|scope| (scope, false))
            .ok_or_else(|| DriverError::validation(tr!("Invalid saved metadata choice.")));
    }
    let levels = builtin_registry()?
        .get(profile.config.driver)?
        .descriptor()
        .capabilities
        .namespace_levels();
    let Some(last) = levels.last() else {
        return Ok((MetadataScope::default(), false));
    };
    let cache = MetadataRepository::open(dir, profile.id, &profile.config).await?;
    let paths = cache.paths(*last).await;
    cache.close().await;
    let scope = MetadataScope {
        all: false,
        include: paths?,
    }
    .normalized();
    if scope.is_pending() {
        return Ok((scope, false));
    }
    repository
        .set_metadata_scope(profile.id, &scope.to_value())
        .await?;
    Ok((scope, true))
}

async fn prepare_cache(
    dir: &Path,
    profile: &ConnectionProfile,
    recover: bool,
) -> DriverResult<MetadataSnapshot> {
    let repository = MetadataRepository::prepare(dir, profile.id, &profile.config).await?;
    let recovered = repository.recovered;
    repository.close().await;
    let mut snapshot = backend::cached_metadata(dir, profile.id, &profile.config, recover).await?;
    snapshot.recovered |= recovered;
    Ok(snapshot)
}

#[cfg(test)]
mod tests;
