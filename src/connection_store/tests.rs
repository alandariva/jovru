use super::*;
use jykre_secrets::SecretStore;
use std::{collections::HashMap, sync::Mutex};

#[derive(Default)]
struct MemoryVault {
    passwords: Mutex<HashMap<ConnectionId, SecretString>>,
    unavailable: bool,
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
    let saved = save(&dir, profile.clone(), Credentials::default(), 1_000, &vault)
        .await
        .unwrap();
    profile = saved.profile;
    profile.name = "Renamed fixture".into();
    let saved = save(&dir, profile.clone(), Credentials::default(), 1_000, &vault)
        .await
        .unwrap();
    let loaded = load(&dir, &vault).await.unwrap().connections;
    assert_eq!(loaded.len(), 1);
    assert_eq!(loaded[0].profile, saved.profile);
    profile = saved.profile;
    profile.config.options["password"] = serde_json::json!("synthetic-secret");
    assert!(
        save(&dir, profile, Credentials::default(), 1_000, &vault)
            .await
            .is_err()
    );
    assert!(
        !String::from_utf8_lossy(&std::fs::read(dir.join("config.db")).unwrap())
            .contains("synthetic-secret")
    );
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
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn remembering_is_opt_in_and_vault_failure_keeps_only_session_credentials() {
    let (dir, mut profile) = fixture();
    let vault = MemoryVault::default();
    let credentials = Credentials {
        password: Some(SecretString::from("synthetic-vault-password".to_owned())),
    };
    let saved = save(&dir, profile.clone(), credentials.clone(), 1_000, &vault)
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
    let saved = save(&dir, profile, credentials.clone(), 1_000, &vault)
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
    save(&dir, profile, credentials.clone(), 1_000, &vault)
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
    let saved = save(&dir, profile, credentials, 1_000, &unavailable)
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
                invalid,
                &vault
            )
            .await
            .is_err()
        );
    }
    let saved = save(&dir, profile, Credentials::default(), 25_000, &vault)
        .await
        .unwrap();
    assert_eq!(saved.row_limit, 25_000);
    assert_eq!(
        load(&dir, &vault).await.unwrap().connections[0].row_limit,
        25_000
    );
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
    let saved = save(&dir, profile, credentials, 1_000, &vault)
        .await
        .unwrap();
    assert_eq!(vault.passwords.lock().unwrap().len(), 1);
    // The same server keeps it when the field is left empty.
    let mut profile = saved.profile;
    profile.config.options["database"] = serde_json::json!("other");
    let saved = save(&dir, profile, Credentials::default(), 1_000, &vault)
        .await
        .unwrap();
    assert_eq!(vault.passwords.lock().unwrap().len(), 1);
    // Another driver does not: the vault entry goes.
    let mut profile = saved.profile;
    profile.config = ConnectionConfig {
        driver: DriverKind::MySql,
        options: serde_json::json!({"host":"localhost","user":"fixture","sslMode":"disabled"}),
    };
    save(&dir, profile, Credentials::default(), 1_000, &vault)
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
    let saved = save(&dir, profile, credentials, 1_000, &vault)
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
    let saved = save(&dir, loaded.profile, Credentials::default(), 1_000, &vault)
        .await
        .unwrap();
    assert!(!saved.profile.remember_password);
    assert!(saved.warning.is_some());
    assert!(
        !load(&dir, &vault).await.unwrap().connections[0]
            .profile
            .remember_password
    );
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
            500,
            &MemoryVault::default()
        )
        .await
        .is_ok()
    );
    std::fs::remove_dir_all(dir).unwrap();
}
