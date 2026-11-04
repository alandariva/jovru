#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]
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

#[tokio::test]
async fn a_profile_and_its_settings_are_saved_together_or_not_at_all() {
    let dir = std::env::temp_dir().join(format!("jykre-config-atomic-{}", ConnectionId::new()));
    let repository = ConfigRepository::open(&dir).await.unwrap();
    let profile = ConnectionProfile {
        id: ConnectionId::new(),
        name: "Synthetic atomic".into(),
        config: ConnectionConfig {
            driver: DriverKind::Postgres,
            options: serde_json::json!({"host":"localhost","database":"fixture","user":"fixture","sslMode":"disable"}),
        },
        remember_password: false,
        revision: 0,
    };
    let limit = jykre_storage::row_limit_key(profile.id);
    let scope = jykre_storage::metadata_scope_key(profile.id);
    let saved = repository
        .save_with_settings(
            &profile,
            &[
                (limit.clone(), serde_json::json!(500)),
                (scope.clone(), serde_json::json!({"all": true})),
            ],
        )
        .await
        .unwrap();
    assert_eq!(saved.revision, 1);
    assert_eq!(repository.row_limit(profile.id).await.unwrap(), Some(500));
    assert_eq!(
        repository.metadata_scope(profile.id).await.unwrap(),
        Some(serde_json::json!({"all": true}))
    );
    // A stale save writes neither the profile nor its settings.
    let mut stale = profile.clone();
    stale.revision = 7;
    stale.name = "Stale".into();
    assert!(
        repository
            .save_with_settings(&stale, &[(limit.clone(), serde_json::json!(9))])
            .await
            .is_err()
    );
    assert_eq!(repository.row_limit(profile.id).await.unwrap(), Some(500));
    assert_eq!(repository.list().await.unwrap(), vec![saved.clone()]);
    // A name taken by another connection fails the same way.
    let other = ConnectionProfile {
        id: ConnectionId::new(),
        name: "Synthetic atomic".into(),
        ..profile.clone()
    };
    let other_limit = jykre_storage::row_limit_key(other.id);
    assert!(
        repository
            .save_with_settings(&other, &[(other_limit, serde_json::json!(9))])
            .await
            .is_err()
    );
    assert_eq!(repository.row_limit(other.id).await.unwrap(), None);
    // The retry of the first profile still has the right revision.
    let mut edited = saved.clone();
    edited.name = "Synthetic edited".into();
    let edited = repository
        .save_with_settings(&edited, &[(limit, serde_json::json!(750))])
        .await
        .unwrap();
    assert_eq!(edited.revision, 2);
    assert_eq!(repository.row_limit(profile.id).await.unwrap(), Some(750));
    repository.close().await;
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn row_limits_are_kept_per_connection_and_survive_restart() {
    let dir = std::env::temp_dir().join(format!("jykre-row-limit-{}", ConnectionId::new()));
    let (first, second) = (ConnectionId::new(), ConnectionId::new());
    let repository = ConfigRepository::open(&dir).await.unwrap();
    assert_eq!(repository.row_limit(first).await.unwrap(), None);
    repository.set_row_limit(first, 50_000).await.unwrap();
    assert!(repository.set_row_limit(second, 0).await.is_err());
    repository.close().await;
    let reopened = ConfigRepository::open(&dir).await.unwrap();
    assert_eq!(reopened.row_limit(first).await.unwrap(), Some(50_000));
    assert_eq!(reopened.row_limit(second).await.unwrap(), None);
    // A value that is not a row count reads as unset.
    reopened
        .set_setting(
            &format!("connection.{second}.row_limit"),
            &serde_json::json!("many"),
        )
        .await
        .unwrap();
    assert_eq!(reopened.row_limit(second).await.unwrap(), None);
    reopened.close().await;
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn an_unreadable_profile_does_not_hide_the_others() {
    let dir = std::env::temp_dir().join(format!("jykre-config-each-{}", ConnectionId::new()));
    let repository = ConfigRepository::open(&dir).await.unwrap();
    let profile = |name: &str| ConnectionProfile {
        id: ConnectionId::new(),
        name: name.into(),
        config: ConnectionConfig {
            driver: DriverKind::Postgres,
            options: serde_json::json!({"host":"localhost","database":"fixture","user":"fixture"}),
        },
        remember_password: false,
        revision: 0,
    };
    let good = repository.save(&profile("Good fixture")).await.unwrap();
    repository.save(&profile("Broken fixture")).await.unwrap();
    repository.close().await;
    // Another version or a hand edit leaves a row that is not valid JSON.
    let pool = sqlx::SqlitePool::connect(&format!("sqlite://{}", dir.join("config.db").display()))
        .await
        .unwrap();
    sqlx::query("UPDATE connections SET config_json='{' WHERE name='Broken fixture'")
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;
    let reopened = ConfigRepository::open(&dir).await.unwrap();
    let each = reopened.list_each().await.unwrap();
    assert_eq!(each.len(), 2);
    assert_eq!(each[0], Ok(good));
    assert_eq!(
        each[1].as_ref().unwrap_err().name.as_deref(),
        Some("Broken fixture")
    );
    assert!(reopened.list().await.is_err());
    reopened.close().await;
    std::fs::remove_dir_all(dir).unwrap();
}
