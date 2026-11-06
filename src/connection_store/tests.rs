use super::*;
use jykre_secrets::SecretStore;
use std::{collections::HashMap, sync::Mutex};

fn preferences(row_limit: u32) -> Preferences {
    Preferences {
        row_limit,
        scope: MetadataScope::default(),
        revalidate_hours: 0,
        appearance: Appearance::default(),
    }
}

#[derive(Default)]
struct MemoryVault {
    passwords: Mutex<HashMap<ConnectionId, SecretString>>,
    unavailable: bool,
    /// Removing an entry fails, as a locked vault does.
    refuses_delete: bool,
}
#[async_trait]
impl SecretStore for MemoryVault {
    async fn get(&self, id: ConnectionId) -> DriverResult<Credentials> {
        if self.unavailable {
            return Err(DriverError::new(
                ErrorCode::Authentication,
                "Fixture vault unavailable",
            ));
        }
        Ok(Credentials {
            password: self.passwords.lock().unwrap().get(&id).cloned(),
        })
    }
    async fn set(&self, id: ConnectionId, password: SecretString) -> DriverResult<()> {
        if self.unavailable {
            return Err(DriverError::new(
                ErrorCode::Authentication,
                "Fixture vault unavailable",
            ));
        }
        self.passwords.lock().unwrap().insert(id, password);
        Ok(())
    }
    async fn delete(&self, id: ConnectionId) -> DriverResult<()> {
        if self.refuses_delete {
            return Err(DriverError::new(
                ErrorCode::Authentication,
                "Fixture vault refused",
            ));
        }
        self.passwords.lock().unwrap().remove(&id);
        Ok(())
    }
}
fn fixture() -> (PathBuf, ConnectionProfile) {
    (
        std::env::temp_dir().join(format!("jykre-service-{}", ConnectionId::new())),
        ConnectionProfile {
            id: ConnectionId::new(),
            name: "Synthetic fixture".into(),
            config: ConnectionConfig {
                driver: DriverKind::Postgres,
                options: serde_json::json!({"host":"localhost","user":"fixture","database":"fixture","sslMode":"disable"}),
            },
            remember_password: false,
            revision: 0,
        },
    )
}

#[tokio::test]
async fn profiles_round_trip_and_reject_embedded_credentials() {
    let (dir, mut profile) = fixture();
    let vault = MemoryVault::default();
    assert!(load(&dir, &vault).await.unwrap().connections.is_empty());
    let saved = save(
        &dir,
        profile.clone(),
        Credentials::default(),
        preferences(1_000),
        &vault,
    )
    .await
    .unwrap();
    profile = saved.profile;
    profile.name = "Renamed fixture".into();
    let saved = save(
        &dir,
        profile.clone(),
        Credentials::default(),
        preferences(1_000),
        &vault,
    )
    .await
    .unwrap();
    let loaded = load(&dir, &vault).await.unwrap().connections;
    assert_eq!(loaded.len(), 1);
    assert_eq!(loaded[0].profile, saved.profile);
    profile = saved.profile;
    profile.config.options["password"] = serde_json::json!("synthetic-secret");
    assert!(
        save(
            &dir,
            profile,
            Credentials::default(),
            preferences(1_000),
            &vault
        )
        .await
        .is_err()
    );
    assert!(
        !String::from_utf8_lossy(&std::fs::read(dir.join("config.db")).unwrap())
            .contains("synthetic-secret")
    );
    backend::release_caches(&dir).await;
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn legacy_migration_is_complete_idempotent_and_deletes_json_only_after_success() {
    let (dir, profile) = fixture();
    std::fs::create_dir_all(&dir).unwrap();
    let legacy = dir.join("connections.json");
    std::fs::write(
        &legacy,
        serde_json::to_vec(&serde_json::json!([{"name":profile.name,"config":profile.config}]))
            .unwrap(),
    )
    .unwrap();
    let vault = MemoryVault::default();
    let first = load(&dir, &vault).await.unwrap().connections;
    assert_eq!(first.len(), 1);
    assert!(!legacy.exists());
    let second = load(&dir, &vault).await.unwrap().connections;
    assert_eq!(first[0].profile.id, second[0].profile.id);
    backend::release_caches(&dir).await;
    std::fs::remove_dir_all(&dir).unwrap();
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(&legacy, b"invalid json").unwrap();
    // An invalid legacy file is reported and kept, nothing is imported, and
    // the start goes on with what SQLite has.
    let loaded = load(&dir, &vault).await.unwrap();
    assert!(loaded.connections.is_empty());
    assert_eq!(loaded.problems.len(), 1);
    assert_eq!(std::fs::read(&legacy).unwrap(), b"invalid json");
    let repository = ConfigRepository::open(&dir).await.unwrap();
    assert!(repository.list().await.unwrap().is_empty());
    assert!(!repository.legacy_imported().await.unwrap());
    repository.close().await;
    // Once fixed, the next start imports it.
    std::fs::write(
        &legacy,
        serde_json::to_vec(&serde_json::json!([{"name":profile.name,"config":profile.config}]))
            .unwrap(),
    )
    .unwrap();
    let loaded = load(&dir, &vault).await.unwrap();
    assert_eq!(loaded.connections.len(), 1);
    assert!(loaded.problems.is_empty());
    backend::release_caches(&dir).await;
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn remembering_is_opt_in_and_vault_failure_keeps_only_session_credentials() {
    let (dir, mut profile) = fixture();
    let vault = MemoryVault::default();
    let credentials = Credentials {
        password: Some(SecretString::from("synthetic-vault-password".to_owned())),
    };
    let saved = save(
        &dir,
        profile.clone(),
        credentials.clone(),
        preferences(1_000),
        &vault,
    )
    .await
    .unwrap();
    assert!(vault.passwords.lock().unwrap().is_empty());
    assert!(
        load(&dir, &vault).await.unwrap().connections[0]
            .input
            .credentials
            .password
            .is_none()
    );
    profile = saved.profile;
    profile.remember_password = true;
    let saved = save(
        &dir,
        profile,
        credentials.clone(),
        preferences(1_000),
        &vault,
    )
    .await
    .unwrap();
    assert!(
        load(&dir, &vault).await.unwrap().connections[0]
            .input
            .credentials
            .password
            .is_some()
    );
    let mut profile = saved.profile;
    profile.remember_password = false;
    save(
        &dir,
        profile,
        credentials.clone(),
        preferences(1_000),
        &vault,
    )
    .await
    .unwrap();
    assert!(vault.passwords.lock().unwrap().is_empty());
    let unavailable = MemoryVault {
        unavailable: true,
        ..Default::default()
    };
    let mut profile = load(&dir, &vault)
        .await
        .unwrap()
        .connections
        .remove(0)
        .profile;
    profile.remember_password = true;
    let saved = save(&dir, profile, credentials, preferences(1_000), &unavailable)
        .await
        .unwrap();
    assert!(saved.input.credentials.password.is_some());
    assert!(saved.warning.is_some());
    // The preference follows what the vault holds: nothing was stored.
    assert!(!saved.profile.remember_password);
    assert!(
        !load(&dir, &vault).await.unwrap().connections[0]
            .profile
            .remember_password
    );
    assert!(
        load(&dir, &unavailable).await.unwrap().connections[0]
            .input
            .credentials
            .password
            .is_none()
    );
    assert!(
        !String::from_utf8_lossy(&std::fs::read(dir.join("config.db")).unwrap())
            .contains("synthetic-vault-password")
    );
    backend::release_caches(&dir).await;
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn data_dir_prefers_override_then_platform_folder() {
    let platform = Some(PathBuf::from("base"));
    assert_eq!(
        resolve_data_dir(Some("custom".into()), platform.clone()),
        PathBuf::from("custom")
    );
    assert_eq!(
        resolve_data_dir(Some("".into()), platform.clone()),
        PathBuf::from("base").join(APP_DIR)
    );
    assert_eq!(
        resolve_data_dir(None, platform),
        PathBuf::from("base").join(APP_DIR)
    );
    assert_eq!(resolve_data_dir(None, None), PathBuf::from(".jykre"));
}

#[tokio::test]
async fn row_limits_are_saved_per_connection_and_validated() {
    let (dir, profile) = fixture();
    let vault = MemoryVault::default();
    for invalid in [0, MAX_ROW_LIMIT + 1] {
        assert!(
            save(
                &dir,
                profile.clone(),
                Credentials::default(),
                preferences(invalid),
                &vault
            )
            .await
            .is_err()
        );
    }
    let saved = save(
        &dir,
        profile,
        Credentials::default(),
        preferences(25_000),
        &vault,
    )
    .await
    .unwrap();
    assert_eq!(saved.row_limit, 25_000);
    assert_eq!(
        load(&dir, &vault).await.unwrap().connections[0].row_limit,
        25_000
    );
    backend::release_caches(&dir).await;
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn expanded_explorer_nodes_survive_a_restart() {
    let (dir, profile) = fixture();
    let vault = MemoryVault::default();
    let saved = save(
        &dir,
        profile,
        Credentials::default(),
        preferences(DEFAULT_ROW_LIMIT),
        &vault,
    )
    .await
    .unwrap();
    let id = saved.profile.id;
    assert!(
        load(&dir, &vault).await.unwrap().connections[0]
            .expanded
            .is_empty()
    );
    let schema = ObjectPath::default()
        .child(ObjectKind::Database, "db/a")
        .child(ObjectKind::Schema, "sales");
    let expanded = [schema.parent().unwrap(), schema.clone()];
    save_expanded(&dir, id, &expanded).await.unwrap();
    assert_eq!(
        load(&dir, &vault).await.unwrap().connections[0].expanded,
        expanded
    );
    // An unreadable list starts collapsed instead of failing the load.
    let repository = ConfigRepository::open(&dir).await.unwrap();
    repository
        .set_explorer_state(id, &serde_json::json!({"expanded": "not a list"}))
        .await
        .unwrap();
    repository.close().await;
    let loaded = load(&dir, &vault).await.unwrap().connections;
    assert!(loaded[0].expanded.is_empty());
    assert!(loaded[0].warning.is_none());
    backend::release_caches(&dir).await;
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn connections_without_a_saved_row_limit_use_the_default() {
    let (dir, profile) = fixture();
    let repository = ConfigRepository::open(&dir).await.unwrap();
    repository.save(&profile).await.unwrap();
    // Written by a newer version with a higher maximum.
    repository
        .set_row_limit(profile.id, MAX_ROW_LIMIT + 1)
        .await
        .unwrap();
    repository.close().await;
    let loaded = load(&dir, &MemoryVault::default())
        .await
        .unwrap()
        .connections;
    assert_eq!(loaded[0].row_limit, DEFAULT_ROW_LIMIT);
    backend::release_caches(&dir).await;
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn a_password_is_kept_only_for_the_same_driver_server_and_user() {
    let config = |driver, options: serde_json::Value| ConnectionConfig { driver, options };
    let base = config(
        DriverKind::Postgres,
        serde_json::json!({"host":"db.local","port":5432,"user":"app","database":"a"}),
    );
    let same = config(
        DriverKind::Postgres,
        serde_json::json!({"host":" DB.local ","port":5432,"user":"app","database":"b"}),
    );
    assert!(keeps_password(&base, &same));
    for other in [
        config(
            DriverKind::MySql,
            serde_json::json!({"host":"db.local","port":5432,"user":"app"}),
        ),
        config(
            DriverKind::Postgres,
            serde_json::json!({"host":"other.local","port":5432,"user":"app"}),
        ),
        config(
            DriverKind::Postgres,
            serde_json::json!({"host":"db.local","port":5433,"user":"app"}),
        ),
        config(
            DriverKind::Postgres,
            serde_json::json!({"host":"db.local","port":5432,"user":"admin"}),
        ),
    ] {
        assert!(!keeps_password(&base, &other), "{other:?}");
    }
}

#[tokio::test]
async fn a_remembered_password_is_not_carried_to_another_driver() {
    let (dir, mut profile) = fixture();
    let vault = MemoryVault::default();
    profile.remember_password = true;
    let credentials = Credentials {
        password: Some(SecretString::from("synthetic-postgres-password".to_owned())),
    };
    let saved = save(&dir, profile, credentials, preferences(1_000), &vault)
        .await
        .unwrap();
    assert_eq!(vault.passwords.lock().unwrap().len(), 1);
    // The same server keeps it when the field is left empty.
    let mut profile = saved.profile;
    profile.config.options["database"] = serde_json::json!("other");
    let saved = save(
        &dir,
        profile,
        Credentials::default(),
        preferences(1_000),
        &vault,
    )
    .await
    .unwrap();
    assert_eq!(vault.passwords.lock().unwrap().len(), 1);
    // Another driver does not: the vault entry goes.
    let mut profile = saved.profile;
    profile.config = ConnectionConfig {
        driver: DriverKind::MySql,
        options: serde_json::json!({"host":"localhost","user":"fixture","sslMode":"disabled"}),
    };
    save(
        &dir,
        profile,
        Credentials::default(),
        preferences(1_000),
        &vault,
    )
    .await
    .unwrap();
    assert!(vault.passwords.lock().unwrap().is_empty());
    assert!(
        load(&dir, &vault).await.unwrap().connections[0]
            .input
            .credentials
            .password
            .is_none()
    );
    backend::release_caches(&dir).await;
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn a_missing_vault_entry_is_reported_and_warnings_add_up() {
    let (dir, mut profile) = fixture();
    let vault = MemoryVault::default();
    profile.remember_password = true;
    let credentials = Credentials {
        password: Some(SecretString::from("synthetic-password".to_owned())),
    };
    let saved = save(&dir, profile, credentials, preferences(1_000), &vault)
        .await
        .unwrap();
    assert!(saved.profile.remember_password);
    assert_eq!(saved.warning, None);
    assert_eq!(
        load(&dir, &vault).await.unwrap().connections[0].warning,
        None
    );
    // The entry disappears outside the application (another tool, a reset
    // keyring): the preference says remember, so reopening says so.
    vault.passwords.lock().unwrap().clear();
    let loaded = load(&dir, &vault).await.unwrap().connections.remove(0);
    assert!(loaded.input.credentials.password.is_none());
    assert!(
        loaded
            .warning
            .as_deref()
            .is_some_and(|warning| warning.contains("vault has no password")),
        "{:?}",
        loaded.warning
    );
    // Saving with nothing to store turns the preference off instead of
    // claiming a password the vault does not have.
    let saved = save(
        &dir,
        loaded.profile,
        Credentials::default(),
        preferences(1_000),
        &vault,
    )
    .await
    .unwrap();
    assert!(!saved.profile.remember_password);
    assert!(saved.warning.is_some());
    assert!(
        !load(&dir, &vault).await.unwrap().connections[0]
            .profile
            .remember_password
    );
    backend::release_caches(&dir).await;
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn warnings_are_collected_in_order() {
    let mut warning = None;
    add_warning(&mut warning, "First.");
    add_warning(&mut warning, "Second.");
    assert_eq!(warning.as_deref(), Some("First. Second."));
}

#[tokio::test]
async fn an_invalid_profile_is_left_out_without_hiding_the_others() {
    let (dir, good) = fixture();
    let repository = ConfigRepository::open(&dir).await.unwrap();
    repository.save(&good).await.unwrap();
    // Saved by another version, with options this one rejects.
    let mut bad = fixture().1;
    bad.name = "Invalid fixture".into();
    bad.config.options["sslMode"] = serde_json::json!("not-a-mode");
    repository.save(&bad).await.unwrap();
    repository.close().await;
    let loaded = load(&dir, &MemoryVault::default()).await.unwrap();
    assert_eq!(loaded.connections.len(), 1);
    assert_eq!(loaded.connections[0].profile.name, good.name);
    assert_eq!(loaded.problems.len(), 1);
    assert!(
        loaded.problems[0].contains("Invalid fixture"),
        "{:?}",
        loaded.problems
    );
    // Saving the valid one still works next to the invalid one.
    let profile = loaded.connections[0].profile.clone();
    assert!(
        save(
            &dir,
            profile,
            Credentials::default(),
            preferences(500),
            &MemoryVault::default()
        )
        .await
        .is_ok()
    );
    backend::release_caches(&dir).await;
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn the_metadata_choice_is_saved_and_an_old_cache_becomes_the_choice_once() {
    let (dir, profile) = fixture();
    let vault = MemoryVault::default();
    let sales = ObjectPath::default()
        .child(ObjectKind::Database, "fixture")
        .child(ObjectKind::Schema, "sales");
    let saved = save(
        &dir,
        profile.clone(),
        Credentials::default(),
        Preferences {
            row_limit: 1_000,
            scope: MetadataScope {
                all: false,
                include: vec![sales.clone()],
            },
            revalidate_hours: 0,
            appearance: Appearance {
                color: Some(crate::connection_appearance::ConnectionColor::Red),
                environment: Some(crate::connection_appearance::Environment::Production),
            },
        },
        &vault,
    )
    .await
    .unwrap();
    assert_eq!(saved.scope.include, std::slice::from_ref(&sales));
    let loaded = load(&dir, &vault).await.unwrap().connections.remove(0);
    assert_eq!(loaded.scope.include, [sales]);
    // The colour and environment are saved with the other preferences.
    assert_eq!(loaded.appearance, saved.appearance);
    assert!(loaded.appearance.is_production());
    assert!(loaded.warning.is_none());
    backend::release_caches(&dir).await;
    std::fs::remove_dir_all(&dir).unwrap();

    // A cache written before choosing existed, with no saved choice.
    let (dir, profile) = fixture();
    let repository = ConfigRepository::open(&dir).await.unwrap();
    repository.save(&profile).await.unwrap();
    repository.close().await;
    let empty = load(&dir, &vault).await.unwrap().connections.remove(0);
    assert!(empty.scope.is_pending());
    let cache = MetadataRepository::open(
        &dir,
        profile.id,
        &backend::cache_config(&profile.config).unwrap(),
    )
    .await
    .unwrap();
    let db = MetadataObject::new(ObjectKind::Database, &ObjectPath::default(), "fixture");
    let schemas =
        ["public", "sales"].map(|name| MetadataObject::new(ObjectKind::Schema, &db.path, name));
    let mut run = cache.begin(&ObjectPath::default()).await.unwrap();
    run.batch([vec![db], schemas.to_vec()].concat())
        .await
        .unwrap();
    run.activate(&IntrospectionReport::default(), &CancellationToken::new())
        .await
        .unwrap();
    drop(run);
    cache.close().await;
    let derived = load(&dir, &vault).await.unwrap().connections.remove(0);
    assert_eq!(
        derived.scope.include,
        schemas
            .iter()
            .map(|schema| schema.path.clone())
            .collect::<Vec<_>>()
    );
    assert!(
        derived
            .warning
            .is_some_and(|warning| warning.contains("Review it"))
    );
    let again = load(&dir, &vault).await.unwrap().connections.remove(0);
    assert_eq!(again.scope, derived.scope);
    assert!(again.warning.is_none());
    backend::release_caches(&dir).await;
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn a_session_without_a_remembered_password_asks_for_one() {
    let input = |driver, password: Option<&str>| ResolvedConnection {
        config: ConnectionConfig {
            driver,
            options: serde_json::json!({}),
        },
        credentials: Credentials {
            password: password.map(SecretString::from),
        },
    };
    assert!(needs_password(&input(DriverKind::Postgres, None)));
    assert!(needs_password(&input(DriverKind::MySql, None)));
    // An empty password typed in the prompt is an answer too.
    assert!(!needs_password(&input(DriverKind::Postgres, Some(""))));
    assert!(!needs_password(&input(DriverKind::MySql, Some("secret"))));
    // Databricks signs in through the CLI; a SQLite file needs no password.
    assert!(!needs_password(&input(DriverKind::Databricks, None)));
    assert!(!needs_password(&input(DriverKind::Sqlite, None)));
}

#[tokio::test]
async fn a_vault_that_does_not_answer_counts_as_unavailable() {
    let limit = std::time::Duration::from_millis(20);
    let started = std::time::Instant::now();
    let stuck = vault_within(limit, std::future::pending::<DriverResult<()>>()).await;
    assert_eq!(stuck.map_err(|error| error.code), Err(ErrorCode::Timeout));
    assert!(started.elapsed() < std::time::Duration::from_secs(5));
    // An answer within the limit passes through, success or failure.
    assert!(vault_within(limit, async { Ok(()) }).await.is_ok());
    let refused = vault_within(limit, async {
        Err::<(), _>(DriverError::new(ErrorCode::Authentication, "refused"))
    })
    .await;
    assert_eq!(
        refused.map_err(|error| error.code),
        Err(ErrorCode::Authentication)
    );
}

#[tokio::test]
async fn a_superseded_preference_write_never_lands_after_the_newer_one() {
    let writes = LatestWrite::new();
    let older = writes.ask();
    let newer = writes.ask();
    // The newer write runs; the older one, even if it runs later, is skipped.
    assert_eq!(
        writes.run(newer, async { Ok(18) }).await.map(Result::ok),
        Some(Some(18))
    );
    assert_eq!(
        writes.run(older, async { Ok(16) }).await.map(Result::ok),
        None
    );
    // A write in progress finishes before the next one starts.
    let order = std::sync::Mutex::new(Vec::new());
    let first = writes.ask();
    let slow = writes.run(first, async {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        order.lock().unwrap().push(first);
        Ok(())
    });
    let (done, _) = tokio::join!(slow, async {
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        writes.ask()
    });
    // A write asked for while one runs does not cut it short.
    assert!(done.is_some_and(|done| done.is_ok()));
    assert_eq!(order.lock().unwrap().as_slice(), [first]);
}

#[tokio::test]
async fn the_font_size_is_written_by_its_latest_ticket() {
    let dir = std::env::temp_dir().join(format!("jykre-font-{}", ConnectionId::new()));
    let older = FONT_SIZE_WRITES.ask();
    let newer = FONT_SIZE_WRITES.ask();
    save_font_size(&dir, 18.0, newer).await.unwrap();
    save_font_size(&dir, 16.0, older).await.unwrap();
    let repository = ConfigRepository::open(&dir).await.unwrap();
    assert_eq!(
        repository.setting("editor.font_size").await.unwrap(),
        Some(serde_json::json!(18.0))
    );
    repository.close().await;
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn deleting_a_connection_removes_its_profile_password_and_cache() {
    let (dir, mut profile) = fixture();
    profile.remember_password = true;
    let vault = MemoryVault::default();
    let credentials = Credentials {
        password: Some(SecretString::from("synthetic-vault-password".to_owned())),
    };
    let saved = save(&dir, profile, credentials, preferences(500), &vault)
        .await
        .unwrap()
        .profile;
    assert_eq!(vault.passwords.lock().unwrap().len(), 1);
    let cache = dir.join("metadata").join(format!("{}.db", saved.id));
    assert!(cache.exists());

    // A profile changed since it was read is not deleted, and nothing else
    // goes with it.
    let mut stale = saved.clone();
    stale.revision -= 1;
    assert!(delete(&dir, &stale, &vault).await.is_err());
    assert_eq!(load(&dir, &vault).await.unwrap().connections.len(), 1);
    assert_eq!(vault.passwords.lock().unwrap().len(), 1);

    assert_eq!(delete(&dir, &saved, &vault).await.unwrap(), None);
    assert!(load(&dir, &vault).await.unwrap().connections.is_empty());
    assert!(vault.passwords.lock().unwrap().is_empty());
    assert!(!cache.exists());
    backend::release_caches(&dir).await;
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn a_vault_that_keeps_the_password_is_reported_after_the_delete() {
    let (dir, profile) = fixture();
    let vault = MemoryVault {
        refuses_delete: true,
        ..MemoryVault::default()
    };
    let saved = save(
        &dir,
        profile,
        Credentials::default(),
        preferences(500),
        &vault,
    )
    .await
    .unwrap()
    .profile;
    let warning = delete(&dir, &saved, &vault).await.unwrap();
    assert!(warning.is_some_and(|warning| warning.contains("vault")));
    assert!(load(&dir, &vault).await.unwrap().connections.is_empty());
    backend::release_caches(&dir).await;
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn each_start_keeps_a_copy_of_the_configuration_to_restore() {
    let (dir, profile) = fixture();
    let vault = MemoryVault::default();
    assert!(latest_backup(&dir).await.is_none());
    let saved = save(
        &dir,
        profile,
        Credentials::default(),
        preferences(500),
        &vault,
    )
    .await
    .unwrap()
    .profile;
    load(&dir, &vault).await.unwrap();
    let (backup, taken) = latest_backup(&dir).await.unwrap();
    assert!(taken <= std::time::SystemTime::now());

    std::fs::write(dir.join("config.db"), b"synthetic corrupt configuration").unwrap();
    assert!(load(&dir, &vault).await.is_err());
    let aside = restore_backup(&dir, &backup).await.unwrap();
    assert!(aside.exists());
    let loaded = load(&dir, &vault).await.unwrap();
    assert_eq!(loaded.connections.len(), 1);
    assert_eq!(loaded.connections[0].profile.id, saved.id);
    backend::release_caches(&dir).await;
    std::fs::remove_dir_all(dir).unwrap();
}
