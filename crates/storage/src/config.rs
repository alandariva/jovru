use crate::*;
use sqlx::Row;
use std::path::{Path, PathBuf};

const SCHEMA: &str = r#"
CREATE TABLE connections (
    id TEXT PRIMARY KEY, name TEXT NOT NULL UNIQUE, config_json TEXT NOT NULL,
    remember_password INTEGER NOT NULL DEFAULT 0, revision INTEGER NOT NULL,
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
);
CREATE TABLE settings (key TEXT PRIMARY KEY, value_json TEXT NOT NULL);
"#;

/// A stored profile that could not be read, with its name when that much
/// is readable.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnreadableProfile {
    pub name: Option<String>,
    pub error: DriverError,
}

#[derive(Clone)]
pub struct ConfigRepository {
    pool: SqlitePool,
}

impl ConfigRepository {
    pub async fn open(dir: &Path) -> DriverResult<Self> {
        // At the first start several parts of the application open the new
        // file at once. Creating it, turning on WAL and the first schema
        // can answer SQLITE_BUSY at once instead of waiting for the busy
        // timeout, so opens within this process take turns.
        static OPENING: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
        let _opening = OPENING.lock().await;
        tokio::fs::create_dir_all(dir)
            .await
            .map_err(|_| error("Could not create the data folder."))?;
        let path = dir.join("config.db");
        let existed = path.exists();
        let pool = open_pool(&path, true).await?;
        let version: i64 = sqlx::query_scalar("PRAGMA user_version")
            .fetch_one(&pool)
            .await
            .map_err(sql_error)?;
        // A file without tables is new, possibly created a moment ago by
        // another part of the application opening it at the same time:
        // only a file written before versioning has something to back up.
        let tables: i64 = sqlx::query_scalar("SELECT count(*) FROM sqlite_master")
            .fetch_one(&pool)
            .await
            .map_err(sql_error)?;
        if existed && version == 0 && tables > 0 {
            let backup_dir = dir.join("backups");
            tokio::fs::create_dir_all(&backup_dir)
                .await
                .map_err(|_| error("Could not prepare the configuration backup."))?;
            let backup = backup_dir.join(format!("config-before-v1-{}.db", ConnectionId::new()));
            sqlx::query("VACUUM INTO ?")
                .bind(backup.to_string_lossy().as_ref())
                .execute(&pool)
                .await
                .map_err(sql_error)?;
        }
        schema(&pool, SCHEMA).await?;
        Ok(Self { pool })
    }
    pub async fn close(&self) {
        self.pool.close().await;
    }

    /// Every profile; fails when any one cannot be read. See
    /// [`ConfigRepository::list_each`] to keep the readable ones.
    pub async fn list(&self) -> DriverResult<Vec<ConnectionProfile>> {
        self.list_each()
            .await?
            .into_iter()
            .map(|profile| profile.map_err(|unreadable| unreadable.error))
            .collect()
    }

    /// Every profile in order, each read on its own: a row that cannot be
    /// read (invalid JSON, a bad identity) does not hide the others.
    pub async fn list_each(
        &self,
    ) -> DriverResult<Vec<Result<ConnectionProfile, UnreadableProfile>>> {
        let rows = sqlx::query(
            "SELECT id,name,config_json,remember_password,revision FROM connections ORDER BY rowid",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(sql_error)?;
        Ok(rows
            .into_iter()
            .map(|row| {
                let read = || -> DriverResult<ConnectionProfile> {
                    Ok(ConnectionProfile {
                        id: row
                            .try_get::<String, _>("id")
                            .map_err(sql_error)?
                            .parse()
                            .map_err(|_| error("Invalid connection identity."))?,
                        name: row.try_get("name").map_err(sql_error)?,
                        config: serde_json::from_str(
                            &row.try_get::<String, _>("config_json").map_err(sql_error)?,
                        )
                        .map_err(json_error)?,
                        remember_password: row.try_get("remember_password").map_err(sql_error)?,
                        revision: row.try_get("revision").map_err(sql_error)?,
                    })
                };
                read().map_err(|error| UnreadableProfile {
                    name: row.try_get("name").ok(),
                    error,
                })
            })
            .collect())
    }

    /// Compare-and-swap prevents an obsolete editor from overwriting a newer profile.
    pub async fn save(&self, profile: &ConnectionProfile) -> DriverResult<ConnectionProfile> {
        self.save_with_settings(profile, &[]).await
    }

    /// Saves a profile and settings that belong to it (its row limit, what
    /// to introspect) in one transaction: either all of them are written,
    /// or none is and the profile keeps its revision, so a retry is not
    /// refused as a change made in another window.
    pub async fn save_with_settings(
        &self,
        profile: &ConnectionProfile,
        settings: &[(String, serde_json::Value)],
    ) -> DriverResult<ConnectionProfile> {
        if profile.name.trim().is_empty() || profile.name.chars().any(char::is_control) {
            return Err(DriverError::validation("Invalid connection name."));
        }
        let json = serde_json::to_string(&profile.config).map_err(json_error)?;
        let values = settings
            .iter()
            .map(|(key, value)| Ok((key, serde_json::to_string(value).map_err(json_error)?)))
            .collect::<DriverResult<Vec<_>>>()?;
        let mut tx = self.pool.begin().await.map_err(sql_error)?;
        let result = if profile.revision == 0 {
            sqlx::query("INSERT INTO connections(id,name,config_json,remember_password,revision) VALUES(?,?,?,?,1)")
                .bind(profile.id.to_string()).bind(&profile.name).bind(json).bind(profile.remember_password).execute(&mut *tx).await
        } else {
            sqlx::query("UPDATE connections SET name=?,config_json=?,remember_password=?,revision=revision+1,updated_at=strftime('%Y-%m-%dT%H:%M:%fZ','now') WHERE id=? AND revision=?")
                .bind(&profile.name).bind(json).bind(profile.remember_password).bind(profile.id.to_string()).bind(profile.revision).execute(&mut *tx).await
        }.map_err(|error| if error.as_database_error().is_some_and(|db| db.is_unique_violation()) { DriverError::validation("A connection with this name already exists.") } else { sql_error(error) })?;
        if result.rows_affected() != 1 {
            return Err(DriverError::validation(
                "The connection was changed in another window. Reopen the configuration.",
            ));
        }
        for (key, value) in values {
            sqlx::query("INSERT INTO settings(key,value_json) VALUES(?,?) ON CONFLICT(key) DO UPDATE SET value_json=excluded.value_json")
                .bind(key).bind(value).execute(&mut *tx).await.map_err(sql_error)?;
        }
        tx.commit().await.map_err(sql_error)?;
        let mut saved = profile.clone();
        saved.revision += 1;
        Ok(saved)
    }

    /// Import and marker are committed together. The caller can then remove the JSON.
    pub async fn import_legacy(&self, profiles: &[ConnectionProfile]) -> DriverResult<()> {
        let mut tx = self.pool.begin().await.map_err(sql_error)?;
        let imported: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM settings WHERE key='legacy_connections_imported')",
        )
        .fetch_one(&mut *tx)
        .await
        .map_err(sql_error)?;
        if !imported {
            for profile in profiles {
                sqlx::query("INSERT INTO connections(id,name,config_json,remember_password,revision) VALUES(?,?,?,0,1)")
                    .bind(profile.id.to_string()).bind(&profile.name).bind(serde_json::to_string(&profile.config).map_err(json_error)?)
                    .execute(&mut *tx).await.map_err(sql_error)?;
            }
            sqlx::query(
                "INSERT INTO settings(key,value_json) VALUES('legacy_connections_imported','true')",
            )
            .execute(&mut *tx)
            .await
            .map_err(sql_error)?;
        }
        tx.commit().await.map_err(sql_error)
    }
    pub async fn legacy_imported(&self) -> DriverResult<bool> {
        sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM settings WHERE key='legacy_connections_imported')",
        )
        .fetch_one(&self.pool)
        .await
        .map_err(sql_error)
    }
    /// The row limit chosen for a connection, if one was saved. Stored as
    /// the setting `connection.<id>.row_limit`; a value that is not a
    /// positive 32-bit number reads as unset.
    pub async fn row_limit(&self, id: ConnectionId) -> DriverResult<Option<u32>> {
        Ok(self
            .setting(&row_limit_key(id))
            .await?
            .and_then(|value| value.as_u64())
            .and_then(|value| u32::try_from(value).ok())
            .filter(|value| *value > 0))
    }

    /// Saves a connection's row limit. Callers validate its range.
    pub async fn set_row_limit(&self, id: ConnectionId, limit: u32) -> DriverResult<()> {
        if limit == 0 {
            return Err(DriverError::validation(
                "Row limit must be greater than zero",
            ));
        }
        self.set_setting(&row_limit_key(id), &serde_json::json!(limit))
            .await
    }

    /// What the user chose to introspect for a connection, stored as the
    /// setting `connection.<id>.metadata_scope`. The application owns its
    /// shape; `None` means nothing was chosen yet.
    pub async fn metadata_scope(
        &self,
        id: ConnectionId,
    ) -> DriverResult<Option<serde_json::Value>> {
        self.setting(&metadata_scope_key(id)).await
    }

    pub async fn set_metadata_scope(
        &self,
        id: ConnectionId,
        scope: &serde_json::Value,
    ) -> DriverResult<()> {
        self.set_setting(&metadata_scope_key(id), scope).await
    }

    /// What the explorer showed expanded for a connection, stored as the
    /// setting `connection.<id>.explorer`. The application owns its shape.
    pub async fn explorer_state(
        &self,
        id: ConnectionId,
    ) -> DriverResult<Option<serde_json::Value>> {
        self.setting(&explorer_key(id)).await
    }

    pub async fn set_explorer_state(
        &self,
        id: ConnectionId,
        state: &serde_json::Value,
    ) -> DriverResult<()> {
        self.set_setting(&explorer_key(id), state).await
    }

    /// Deletes a profile and every setting kept under its identity (row
    /// limit, what to introspect, appearance, explorer state) in one
    /// transaction. Refused when the profile changed since `revision` was
    /// read, as a save is, so a connection edited in another window is not
    /// deleted unseen.
    pub async fn delete(&self, id: ConnectionId, revision: i64) -> DriverResult<()> {
        let mut tx = self.pool.begin().await.map_err(sql_error)?;
        let deleted = sqlx::query("DELETE FROM connections WHERE id=? AND revision=?")
            .bind(id.to_string())
            .bind(revision)
            .execute(&mut *tx)
            .await
            .map_err(sql_error)?;
        if deleted.rows_affected() != 1 {
            return Err(DriverError::validation(
                "The connection was changed in another window. Reopen the configuration.",
            ));
        }
        // Compared as a prefix, not with LIKE, so no character of the key
        // is read as a wildcard.
        sqlx::query("DELETE FROM settings WHERE substr(key, 1, length(?1)) = ?1")
            .bind(format!("connection.{id}."))
            .execute(&mut *tx)
            .await
            .map_err(sql_error)?;
        tx.commit().await.map_err(sql_error)
    }

    /// Copies `config.db` to `backups/config-v<version>-<millis>.db` in the
    /// data folder `dir`, as a consistent snapshot (`VACUUM INTO`), and keeps
    /// the [`CONFIG_BACKUPS_KEPT`] most recent copies. A copy identical to
    /// the newest one is not kept twice. Returns the newest copy.
    pub async fn back_up(&self, dir: &Path) -> DriverResult<PathBuf> {
        let folder = dir.join(BACKUP_FOLDER);
        tokio::fs::create_dir_all(&folder)
            .await
            .map_err(|_| error("Could not prepare the configuration backup."))?;
        let version: i64 = sqlx::query_scalar("PRAGMA user_version")
            .fetch_one(&self.pool)
            .await
            .map_err(sql_error)?;
        let mut stamp = now_millis();
        let mut path = folder.join(format!("config-v{version}-{stamp}.db"));
        // Two starts within the same millisecond never overwrite a copy.
        while tokio::fs::try_exists(&path).await.unwrap_or(false) {
            stamp += 1;
            path = folder.join(format!("config-v{version}-{stamp}.db"));
        }
        let newest = config_backups(dir).await.into_iter().next();
        sqlx::query("VACUUM INTO ?")
            .bind(path.to_string_lossy().as_ref())
            .execute(&self.pool)
            .await
            .map_err(|_| error("Could not write the configuration backup."))?;
        if let Some(newest) = newest
            && same_contents(&newest, &path).await
        {
            let _ = tokio::fs::remove_file(&path).await;
            return Ok(newest);
        }
        for old in config_backups(dir)
            .await
            .into_iter()
            .skip(CONFIG_BACKUPS_KEPT)
        {
            let _ = tokio::fs::remove_file(old).await;
        }
        Ok(path)
    }

    pub async fn setting(&self, key: &str) -> DriverResult<Option<serde_json::Value>> {
        let value: Option<String> =
            sqlx::query_scalar("SELECT value_json FROM settings WHERE key=?")
                .bind(key)
                .fetch_optional(&self.pool)
                .await
                .map_err(sql_error)?;
        value
            .map(|value| serde_json::from_str(&value).map_err(json_error))
            .transpose()
    }
    pub async fn set_setting(&self, key: &str, value: &serde_json::Value) -> DriverResult<()> {
        sqlx::query("INSERT INTO settings(key,value_json) VALUES(?,?) ON CONFLICT(key) DO UPDATE SET value_json=excluded.value_json")
            .bind(key).bind(serde_json::to_string(value).map_err(json_error)?).execute(&self.pool).await.map_err(sql_error)?;
        Ok(())
    }
}

/// How many copies of `config.db` [`ConfigRepository::back_up`] keeps.
pub const CONFIG_BACKUPS_KEPT: usize = 5;

const BACKUP_FOLDER: &str = "backups";

fn now_millis() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis())
}

/// The copies of `config.db` in `backups/`, newest first. Only the copies
/// [`ConfigRepository::back_up`] writes are listed.
pub async fn config_backups(dir: &Path) -> Vec<PathBuf> {
    let Ok(mut entries) = tokio::fs::read_dir(dir.join(BACKUP_FOLDER)).await else {
        return Vec::new();
    };
    let mut backups = Vec::new();
    while let Ok(Some(entry)) = entries.next_entry().await {
        let path = entry.path();
        if is_backup(&path) {
            backups.push((backup_stamp(&path), path));
        }
    }
    backups.sort_by_key(|backup| std::cmp::Reverse(backup.0));
    backups.into_iter().map(|(_, path)| path).collect()
}

/// When a copy listed by [`config_backups`] was taken, read from its name.
pub fn config_backup_time(path: &Path) -> std::time::SystemTime {
    let millis = u64::try_from(backup_stamp(path)).unwrap_or(u64::MAX);
    std::time::UNIX_EPOCH + std::time::Duration::from_millis(millis)
}

/// Puts `backup` in place of `config.db`, which is renamed (with its `-wal`
/// and `-shm` files) to `config.db.broken-<millis>` and kept for diagnosis.
/// The copy is checked before anything moves. Returns where the previous
/// file went.
pub async fn restore_config_backup(dir: &Path, backup: &Path) -> DriverResult<PathBuf> {
    if !is_backup(backup) || backup.parent() != Some(dir.join(BACKUP_FOLDER).as_path()) {
        return Err(DriverError::validation("Not a configuration backup."));
    }
    // A copy that does not open is never put in place.
    open_pool(backup, false).await?.close().await;
    let stamp = now_millis();
    let current = dir.join("config.db");
    let aside = dir.join(format!("config.db.broken-{stamp}"));
    if tokio::fs::try_exists(&current).await.unwrap_or(false) {
        tokio::fs::rename(&current, &aside)
            .await
            .map_err(|_| error("Could not set the damaged configuration aside."))?;
    }
    for suffix in ["-wal", "-shm"] {
        let companion = dir.join(format!("config.db{suffix}"));
        if tokio::fs::try_exists(&companion).await.unwrap_or(false) {
            let _ = tokio::fs::rename(
                &companion,
                dir.join(format!("config.db.broken-{stamp}{suffix}")),
            )
            .await;
        }
    }
    tokio::fs::copy(backup, &current)
        .await
        .map_err(|_| error("Could not restore the configuration backup."))?;
    Ok(aside)
}

/// A name [`ConfigRepository::back_up`] writes: `config-v<n>-<millis>.db`.
fn is_backup(path: &Path) -> bool {
    let digits = |text: &str| !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit());
    path.file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_prefix("config-v")?.strip_suffix(".db"))
        .and_then(|rest| rest.split_once('-'))
        .is_some_and(|(version, millis)| digits(version) && digits(millis))
}

fn backup_stamp(path: &Path) -> u128 {
    path.file_stem()
        .and_then(|stem| stem.to_str())
        .and_then(|stem| stem.rsplit_once('-'))
        .and_then(|(_, millis)| millis.parse().ok())
        .unwrap_or(0)
}

async fn same_contents(a: &Path, b: &Path) -> bool {
    match (tokio::fs::read(a).await, tokio::fs::read(b).await) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// The settings key of a connection's row limit.
pub fn row_limit_key(id: ConnectionId) -> String {
    format!("connection.{id}.row_limit")
}

/// The settings key of what a connection introspects.
pub fn metadata_scope_key(id: ConnectionId) -> String {
    format!("connection.{id}.metadata_scope")
}

fn explorer_key(id: ConnectionId) -> String {
    format!("connection.{id}.explorer")
}
