use jykre_core::*;
use jykre_storage::ConfigRepository;

#[tokio::test]
async fn configuration_settings_and_identity_survive_restart_and_reject_stale_writes() {
    let dir = std::env::temp_dir().join(format!("jykre-config-{}", ConnectionId::new()));
    let repository = ConfigRepository::open(&dir).await.unwrap();
    let profile = ConnectionProfile {
        id: ConnectionId::new(),
        name: "Synthetic connection".into(),
        config: ConnectionConfig {
            driver: DriverKind::Postgres,
            options: serde_json::json!({"host":"localhost","database":"fixture","user":"fixture","sslMode":"disable"}),
        },
        remember_password: false,
        revision: 0,
    };
    let saved = repository.save(&profile).await.unwrap();
    assert_eq!(saved.revision, 1);
    repository
        .set_setting("editor.font_size", &serde_json::json!(16))
        .await
        .unwrap();
    repository.close().await;
    let reopened = ConfigRepository::open(&dir).await.unwrap();
    assert_eq!(reopened.list().await.unwrap(), vec![saved.clone()]);
    assert_eq!(
        reopened.setting("editor.font_size").await.unwrap(),
        Some(serde_json::json!(16))
    );
    let mut edited = saved.clone();
    edited.name = "Renamed fixture".into();
    let edited = reopened.save(&edited).await.unwrap();
    assert_eq!(edited.id, saved.id);
    assert!(reopened.save(&saved).await.is_err());
    assert_eq!(reopened.list().await.unwrap(), vec![edited]);
    reopened.close().await;
    std::fs::write(dir.join("config.db"), b"synthetic corrupt configuration").unwrap();
    assert!(ConfigRepository::open(&dir).await.is_err());
    assert_eq!(
        std::fs::read(dir.join("config.db")).unwrap(),
        b"synthetic corrupt configuration"
    );
    std::fs::remove_dir_all(dir).unwrap();
}
