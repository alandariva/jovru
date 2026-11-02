//! Application connection service: validation, SQLite migration and vault policy.
use crate::tr;
use crate::{
    backend::{self, MetadataSnapshot},
    drivers::*,
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

pub async fn load(dir: &Path, secrets: &dyn SecretStore) -> DriverResult<Vec<LoadedConnection>> {
    let repository = ConfigRepository::open(dir).await?;
    let result = load_from(dir, secrets, &repository).await;
    repository.close().await;
    result
}

async fn load_from(
    dir: &Path,
    secrets: &dyn SecretStore,
    repository: &ConfigRepository,
) -> DriverResult<Vec<LoadedConnection>> {
    let legacy = dir.join("connections.json");
    let bytes = match tokio::fs::read(&legacy).await {
        Ok(bytes) => Some(bytes),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(_) => {
            return Err(DriverError::new(
                ErrorCode::Internal,
                tr!("Could not read the legacy connections.json."),
            ));
        }
    };
    if !repository.legacy_imported().await? {
        let imported = bytes
            .as_ref()
            .map(|bytes| legacy_profiles(bytes))
            .transpose()?
            .unwrap_or_default();
        repository.import_legacy(&imported).await?;
    }
    let profiles = repository.list().await?;
    let mut migration_warning = None;
    if let Some(bytes) = bytes {
        // A previous crash may have committed SQLite before unlinking the JSON.
        // Delete only if every legacy profile is already represented identically.
        let old = legacy_profiles(&bytes)?;
        if !old.iter().all(|old| {
            profiles
                .iter()
                .any(|new| new.name == old.name && new.config == old.config)
        }) {
            return Err(DriverError::validation(tr!(
                "The legacy JSON changed after the migration. It was preserved to avoid losing connections."
            )));
        }
        if tokio::fs::remove_file(legacy).await.is_err() {
            migration_warning = Some(
                tr!("Migration completed; could not delete the legacy connections.json.")
                    .to_owned(),
            );
        }
    }
    let registry = builtin_registry()?;
    let mut loaded = Vec::new();
    for profile in profiles {
        registry
            .get(profile.config.driver)?
            .validate_connection(&profile.config)?;
        let mut warning = migration_warning.clone();
        let credentials =
            if profile.remember_password && profile.config.driver != DriverKind::Databricks {
                match secrets.get(profile.id).await {
                    Ok(value) => value,
                    Err(error) => {
                        warning = Some(error.to_string());
                        Credentials::default()
                    }
                }
            } else {
                Credentials::default()
            };
        let cache = match prepare_cache(dir, &profile, true).await {
            Ok(cache) => Some(cache),
            Err(error) => {
                warning = Some(error.to_string());
                None
            }
        };
        let input = ResolvedConnection {
            config: profile.config.clone(),
            credentials,
        };
        loaded.push(LoadedConnection {
            profile,
            input,
            cache,
            warning,
        });
    }
    Ok(loaded)
}

pub async fn save(
    dir: &Path,
    profile: ConnectionProfile,
    credentials: Credentials,
    secrets: &dyn SecretStore,
) -> DriverResult<LoadedConnection> {
    builtin_registry()?
        .get(profile.config.driver)?
        .validate_connection(&profile.config)?;
    let repository = ConfigRepository::open(dir).await?;
    let result = save_to(dir, profile, credentials, secrets, &repository).await;
    repository.close().await;
    result
}
async fn save_to(
    dir: &Path,
    profile: ConnectionProfile,
    credentials: Credentials,
    secrets: &dyn SecretStore,
    repository: &ConfigRepository,
) -> DriverResult<LoadedConnection> {
    let previous = repository
        .list()
        .await?
        .into_iter()
        .find(|value| value.id == profile.id);
    let profile = repository.save(&profile).await?;
    let mut warning = None;
    if profile.config.driver != DriverKind::Databricks && profile.remember_password {
        if let Some(password) = credentials.password.clone() {
            if let Err(error) = secrets.set(profile.id, password).await {
                warning = Some(tr!(
                    "Connection saved. Could not store the password in the vault: {0}",
                    error
                ));
            }
        } else {
            warning = Some(
                tr!("Connection saved without an available password. Enter it before querying the server.")
                    .into(),
            );
        }
    } else if previous
        .as_ref()
        .is_some_and(|profile| profile.remember_password)
        && secrets.delete(profile.id).await.is_err()
    {
        warning = Some(tr!("Remember password was turned off, but the previous password could not be removed from the vault. It will not be used when reopening.").into());
    }
    let cache = match prepare_cache(dir, &profile, false).await {
        Ok(value) => Some(value),
        Err(error) => {
            warning = Some(tr!("Connection saved. Cache unavailable: {0}", error));
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
        cache,
        warning,
    })
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
