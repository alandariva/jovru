//! Local persistence isolated from drivers and the GUI.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]
mod automation;
mod config;
mod metadata;
pub use automation::{
    AUTOMATION_FILE, AUTOMATION_LOCK_FILE, AutomationLock, AutomationRepository, ChainFinish,
    ChainRef, Fingerprinter, HistoryEntry, HistoryFilter, InUse, NewChainRun, NewRun,
    OUTPUT_QUOTA_BYTES, OUTPUT_VALUE_RETENTION, Opened, Orphan, OutputValue, Pruned, RUN_RETENTION,
    Recovered, RunFinish, StoredJob, StoredVersion, fingerprint,
};
pub use config::{
    CONFIG_BACKUPS_KEPT, ConfigRepository, UnreadableProfile, config_backup_time, config_backups,
    metadata_scope_key, restore_config_backup, row_limit_key,
};
pub use metadata::{
    ChildPage, MetadataRepository, PageCursor, PageRequest, PendingRefresh, RefreshState,
    RefreshStatus, SearchCursor, SearchPage,
};

use jykre_core::*;
use sqlx::{ConnectOptions, SqlitePool, sqlite::*};
use std::{path::Path, time::Duration};

pub(crate) fn error(message: &str) -> DriverError {
    DriverError::new(ErrorCode::Internal, message)
}
pub(crate) fn sql_error(_: sqlx::Error) -> DriverError {
    error("Could not access local storage.")
}
pub(crate) fn json_error(_: serde_json::Error) -> DriverError {
    error("Invalid local data.")
}

pub(crate) async fn open_pool(path: &Path, durable: bool) -> DriverResult<SqlitePool> {
    let pool = SqlitePoolOptions::new()
        .max_connections(4)
        .connect_with(
            SqliteConnectOptions::new()
                .filename(path)
                .create_if_missing(true)
                .journal_mode(SqliteJournalMode::Wal)
                .synchronous(if durable {
                    SqliteSynchronous::Full
                } else {
                    SqliteSynchronous::Normal
                })
                .foreign_keys(true)
                .busy_timeout(Duration::from_secs(5))
                // SQLx logs SQL by default; local SQL never reaches the logs.
                .disable_statement_logging(),
        )
        .await
        .map_err(sql_error)?;
    let checked = sqlx::query_scalar::<_, String>("PRAGMA quick_check")
        .fetch_one(&pool)
        .await;
    if !matches!(checked, Ok(ref value) if value == "ok") {
        pool.close().await;
        return Err(error("Invalid or corrupted SQLite file."));
    }
    Ok(pool)
}

pub(crate) async fn schema(pool: &SqlitePool, sql: &'static str) -> DriverResult<()> {
    let version: i64 = sqlx::query_scalar("PRAGMA user_version")
        .fetch_one(pool)
        .await
        .map_err(sql_error)?;
    if version > 1 {
        return Err(error(
            "This file was created by a newer version of the application.",
        ));
    }
    if version == 0 {
        let mut tx = pool.begin().await.map_err(sql_error)?;
        sqlx::raw_sql(sql)
            .execute(&mut *tx)
            .await
            .map_err(sql_error)?;
        sqlx::query("PRAGMA user_version=1")
            .execute(&mut *tx)
            .await
            .map_err(sql_error)?;
        tx.commit().await.map_err(sql_error)?;
    }
    Ok(())
}
