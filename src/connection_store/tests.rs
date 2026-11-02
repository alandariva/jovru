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
    assert!(load(&dir, &vault).await.unwrap().is_empty());
    let saved = save(&dir, profile.clone(), Credentials::default(), &vault)
        .await
        .unwrap();
    profile = saved.profile;
    profile.name = "Renamed fixture".into();
    let saved = save(&dir, profile.clone(), Credentials::default(), &vault)
        .await
        .unwrap();
    let loaded = load(&dir, &vault).await.unwrap();
    assert_eq!(loaded.len(), 1);
    assert_eq!(loaded[0].profile, saved.profile);
    profile = saved.profile;
    profile.config.options["password"] = serde_json::json!("synthetic-secret");
    assert!(
        save(&dir, profile, Credentials::default(), &vault)
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
    let first = load(&dir, &vault).await.unwrap();
    assert_eq!(first.len(), 1);
    assert!(!legacy.exists());
    let second = load(&dir, &vault).await.unwrap();
    assert_eq!(first[0].profile.id, second[0].profile.id);
    std::fs::remove_dir_all(&dir).unwrap();
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(&legacy, b"invalid json").unwrap();
    assert!(load(&dir, &vault).await.is_err());
    assert_eq!(std::fs::read(&legacy).unwrap(), b"invalid json");
    let repository = ConfigRepository::open(&dir).await.unwrap();
    assert!(repository.list().await.unwrap().is_empty());
    repository.close().await;
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn remembering_is_opt_in_and_vault_failure_keeps_only_session_credentials() {
    let (dir, mut profile) = fixture();
    let vault = MemoryVault::default();
    let credentials = Credentials {
        password: Some(SecretString::from("synthetic-vault-password".to_owned())),
    };
    let saved = save(&dir, profile.clone(), credentials.clone(), &vault)
        .await
        .unwrap();
    assert!(vault.passwords.lock().unwrap().is_empty());
    assert!(
        load(&dir, &vault).await.unwrap()[0]
            .input
            .credentials
            .password
            .is_none()
    );
    profile = saved.profile;
    profile.remember_password = true;
    let saved = save(&dir, profile, credentials.clone(), &vault)
        .await
        .unwrap();
    assert!(
        load(&dir, &vault).await.unwrap()[0]
            .input
            .credentials
            .password
            .is_some()
    );
    let mut profile = saved.profile;
    profile.remember_password = false;
    save(&dir, profile, credentials.clone(), &vault)
        .await
        .unwrap();
    assert!(vault.passwords.lock().unwrap().is_empty());
    let unavailable = MemoryVault {
        unavailable: true,
        ..Default::default()
    };
    let mut profile = load(&dir, &vault).await.unwrap().remove(0).profile;
    profile.remember_password = true;
    let saved = save(&dir, profile, credentials, &unavailable)
        .await
        .unwrap();
    assert!(saved.input.credentials.password.is_some());
    assert!(saved.warning.is_some());
    assert!(
        load(&dir, &unavailable).await.unwrap()[0]
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
