use crate::*;
use sqlx::Row;
use std::path::Path;

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
        if existed && version == 0 {
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
        if profile.name.trim().is_empty() || profile.name.chars().any(char::is_control) {
            return Err(DriverError::validation("Invalid connection name."));
        }
        let json = serde_json::to_string(&profile.config).map_err(json_error)?;
        let result = if profile.revision == 0 {
            sqlx::query("INSERT INTO connections(id,name,config_json,remember_password,revision) VALUES(?,?,?,?,1)")
                .bind(profile.id.to_string()).bind(&profile.name).bind(json).bind(profile.remember_password).execute(&self.pool).await
        } else {
            sqlx::query("UPDATE connections SET name=?,config_json=?,remember_password=?,revision=revision+1,updated_at=strftime('%Y-%m-%dT%H:%M:%fZ','now') WHERE id=? AND revision=?")
                .bind(&profile.name).bind(json).bind(profile.remember_password).bind(profile.id.to_string()).bind(profile.revision).execute(&self.pool).await
        }.map_err(|error| if error.as_database_error().is_some_and(|db| db.is_unique_violation()) { DriverError::validation("A connection with this name already exists.") } else { sql_error(error) })?;
        if result.rows_affected() != 1 {
            return Err(DriverError::validation(
                "The connection was changed in another window. Reopen the configuration.",
            ));
        }
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

fn row_limit_key(id: ConnectionId) -> String {
    format!("connection.{id}.row_limit")
}

fn metadata_scope_key(id: ConnectionId) -> String {
    format!("connection.{id}.metadata_scope")
}

fn explorer_key(id: ConnectionId) -> String {
    format!("connection.{id}.explorer")
}
