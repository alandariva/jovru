use super::*;

fn dir() -> PathBuf {
    std::env::temp_dir().join(format!("jykre-metadata-{}", ConnectionId::new()))
}
fn config() -> ConnectionConfig {
    ConnectionConfig {
        driver: DriverKind::MySql,
        options: serde_json::json!({"host":"localhost","user":"fixture","database":"fixture","sslMode":"disabled"}),
    }
}
fn objects() -> Vec<MetadataObject> {
    let db = MetadataObject::new(ObjectKind::Database, &ObjectPath::default(), "db.a/%_");
    let table = MetadataObject::new(ObjectKind::Table, &db.path, "target");
    let col = MetadataObject::new(ObjectKind::Column, &table.path, "old");
    let sibling = MetadataObject::new(ObjectKind::Table, &db.path, "target.more");
    vec![db, table, col, sibling]
}
async fn seed(repository: &MetadataRepository) {
    let mut run = repository.begin(&ObjectPath::default()).await.unwrap();
    run.batch(objects()).await.unwrap();
    run.activate(&IntrospectionReport::default(), &CancellationToken::new())
        .await
        .unwrap();
}
#[tokio::test]
async fn partial_publication_is_atomic_does_not_rewrite_siblings_and_persists() {
    let dir = dir();
    let id = ConnectionId::new();
    let config = config();
    let repository = MetadataRepository::open(&dir, id, &config).await.unwrap();
    seed(&repository).await;
    let original = repository.all().await.unwrap();
    let root_status = repository.statuses().await.unwrap()[0]
        .last_success_at
        .clone();
    sqlx::raw_sql("CREATE TRIGGER protect_sibling_delete BEFORE DELETE ON objects WHEN OLD.name='target.more' BEGIN SELECT RAISE(ABORT,'sibling changed'); END; CREATE TRIGGER protect_sibling_update BEFORE UPDATE ON objects WHEN OLD.name='target.more' BEGIN SELECT RAISE(ABORT,'sibling changed'); END;").execute(&repository.pool).await.unwrap();
    let table = objects()[1].clone();
    let col = MetadataObject::new(ObjectKind::Column, &table.path, "new");
    let mut run = repository.begin(&table.path).await.unwrap();
    run.batch(vec![table.clone(), col.clone()]).await.unwrap();
    assert_eq!(repository.all().await.unwrap(), original);
    run.activate(&IntrospectionReport::default(), &CancellationToken::new())
        .await
        .unwrap();
    drop(run);
    let all = repository.all().await.unwrap();
    assert!(all.contains(&objects()[3]));
    assert!(!all.contains(&objects()[2]));
    assert!(all.contains(&col));
    let statuses = repository.statuses().await.unwrap();
    assert_eq!(
        statuses
            .iter()
            .find(|status| status.scope.0.is_empty())
            .unwrap()
            .last_success_at,
        root_status
    );
    assert_eq!(
        statuses
            .iter()
            .find(|status| status.scope == table.path)
            .unwrap()
            .state,
        RefreshState::Success
    );
    // The enum changes the type, not the text existing caches hold.
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT state FROM scope_status WHERE scope_key=?")
            .bind(key(&table.path).unwrap())
            .fetch_one(&repository.pool)
            .await
            .unwrap(),
        "success"
    );
    repository.close().await;
    let reopened = MetadataRepository::open(&dir, id, &config).await.unwrap();
    assert_eq!(reopened.all().await.unwrap(), all);
    assert_eq!(reopened.version().await.unwrap(), 2);
    let mut run = reopened.begin(&table.path).await.unwrap();
    run.activate(&IntrospectionReport::default(), &CancellationToken::new())
        .await
        .unwrap();
    drop(run);
    assert!(
        !reopened
            .all()
            .await
            .unwrap()
            .iter()
            .any(|object| object.path.is_within(&table.path))
    );
    assert!(reopened.all().await.unwrap().contains(&objects()[3]));
    reopened.close().await;
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn refresh_states_read_back_the_text_they_store() {
    for state in [
        RefreshState::Running,
        RefreshState::Success,
        RefreshState::Partial,
        RefreshState::Warning,
        RefreshState::Failed,
        RefreshState::Cancelled,
        RefreshState::Interrupted,
    ] {
        assert_eq!(RefreshState::parse(state.as_str()), state);
    }
    assert_eq!(
        RefreshState::parse("written by a newer version"),
        RefreshState::Unknown
    );
}

#[tokio::test]
async fn errors_cancellation_and_crash_never_publish_staged_data() {
    let dir = dir();
    let id = ConnectionId::new();
    let config = config();
    let repository = MetadataRepository::open(&dir, id, &config).await.unwrap();
    seed(&repository).await;
    let all = repository.all().await.unwrap();
    let mut run = repository.begin(&objects()[1].path).await.unwrap();
    run.batch(vec![objects()[1].clone()]).await.unwrap();
    let cancel = CancellationToken::new();
    cancel.cancel();
    let error = run
        .activate(&IntrospectionReport::default(), &cancel)
        .await
        .unwrap_err();
    run.fail(&error).await.unwrap();
    drop(run);
    assert_eq!(repository.all().await.unwrap(), all);
    assert_eq!(repository.version().await.unwrap(), 1);
    let mut run = repository.begin(&objects()[1].path).await.unwrap();
    run.batch(vec![objects()[1].clone()]).await.unwrap();
    assert!(
        repository
            .begin_within(&ObjectPath::default(), std::time::Duration::ZERO)
            .await
            .is_err()
    );
    // A second reader must not recover a refresh that still owns its OS lock.
    repository.recover_interrupted().await.unwrap();
    assert!(
        repository
            .statuses()
            .await
            .unwrap()
            .iter()
            .any(|status| status.state == RefreshState::Running)
    );
    drop(run);
    repository.close().await;
    let reopened = MetadataRepository::open(&dir, id, &config).await.unwrap();
    reopened.recover_interrupted().await.unwrap();
    assert_eq!(reopened.all().await.unwrap(), all);
    assert!(
        reopened
            .statuses()
            .await
            .unwrap()
            .iter()
            .any(|status| status.state == RefreshState::Interrupted)
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM staging_objects")
            .fetch_one(&reopened.pool)
            .await
            .unwrap(),
        0
    );
    reopened.close().await;
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn a_refresh_holds_the_writer_lock_only_to_publish() {
    let dir = dir();
    let id = ConnectionId::new();
    let config = config();
    let repository = MetadataRepository::open(&dir, id, &config).await.unwrap();
    seed(&repository).await;
    let table = objects()[1].clone();
    let mut run = repository.begin(&table.path).await.unwrap();
    run.batch(vec![table.clone()]).await.unwrap();
    // While the refresh reads, another writer gets the lock at once.
    let held = writer_lock(&repository.lock_path).unwrap();
    // Publishing waits for the writer to finish instead of failing.
    let release = tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        drop(held);
    });
    run.activate(&IntrospectionReport::default(), &CancellationToken::new())
        .await
        .unwrap();
    release.await.unwrap();
    drop(run);
    assert!(!repository.all().await.unwrap().contains(&objects()[2]));
    // A new refresh waits for one that is finishing.
    let finishing = repository.begin(&table.path).await.unwrap();
    let release = tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        drop(finishing);
    });
    let next = repository.begin(&ObjectPath::default()).await;
    release.await.unwrap();
    assert!(next.is_ok());
    drop(next);
    repository.close().await;
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn saving_another_configuration_during_a_read_makes_the_read_stale() {
    let dir = dir();
    let id = ConnectionId::new();
    let config = config();
    let repository = MetadataRepository::open(&dir, id, &config).await.unwrap();
    seed(&repository).await;
    let mut run = repository.begin(&objects()[1].path).await.unwrap();
    let mut changed = config.clone();
    changed.options["user"] = serde_json::json!("another-fixture-user");
    // The read in progress does not keep the new configuration from being
    // accepted, and what it read is never published.
    let current = MetadataRepository::prepare(&dir, id, &changed)
        .await
        .unwrap();
    assert!(
        run.activate(&IntrospectionReport::default(), &CancellationToken::new())
            .await
            .is_err()
    );
    drop(run);
    assert!(current.all().await.unwrap().is_empty());
    repository.close().await;
    current.close().await;
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn permission_warnings_preserve_unreadable_subtrees_and_mark_them() {
    let dir = dir();
    let repository = MetadataRepository::open(&dir, ConnectionId::new(), &config())
        .await
        .unwrap();
    seed(&repository).await;
    let mut run = repository.begin(&ObjectPath::default()).await.unwrap();
    run.batch(vec![objects()[0].clone()]).await.unwrap();
    let report = IntrospectionReport {
        warnings: vec![IntrospectionWarning {
            path: objects()[1].path.clone(),
            message: "fixture denied".into(),
        }],
    };
    run.activate(&report, &CancellationToken::new())
        .await
        .unwrap();
    drop(run);
    let all = repository.all().await.unwrap();
    assert!(all.contains(&objects()[1]));
    assert!(all.contains(&objects()[2]));
    assert!(!all.contains(&objects()[3]));
    assert!(
        repository
            .statuses()
            .await
            .unwrap()
            .iter()
            .any(
                |status| status.state == RefreshState::Warning && status.scope == objects()[1].path
            )
    );
    repository.close().await;
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn malformed_batch_and_failed_commit_preserve_active_cache() {
    let dir = dir();
    let repository = MetadataRepository::open(&dir, ConnectionId::new(), &config())
        .await
        .unwrap();
    seed(&repository).await;
    let original = repository.all().await.unwrap();
    let mut run = repository.begin(&objects()[1].path).await.unwrap();
    assert!(run.batch(vec![objects()[3].clone()]).await.is_err());
    let orphan = MetadataObject::new(ObjectKind::Column, &objects()[1].path, "orphan");
    run.batch(vec![orphan]).await.unwrap();
    let error = run
        .activate(&IntrospectionReport::default(), &CancellationToken::new())
        .await
        .unwrap_err();
    run.fail(&error).await.unwrap();
    drop(run);
    assert_eq!(repository.all().await.unwrap(), original);
    repository.close().await;
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn config_change_invalidates_cache_and_corruption_is_preserved() {
    let dir = dir();
    let id = ConnectionId::new();
    let mut config = config();
    let repository = MetadataRepository::open(&dir, id, &config).await.unwrap();
    seed(&repository).await;
    repository.close().await;
    config.options["user"] = serde_json::json!("another-test-user");
    let repository = MetadataRepository::prepare(&dir, id, &config)
        .await
        .unwrap();
    assert!(repository.all().await.unwrap().is_empty());
    repository.close().await;
    let path = dir.join("metadata").join(format!("{id}.db"));
    std::fs::write(&path, b"synthetic corrupt fixture").unwrap();
    let repository = MetadataRepository::open(&dir, id, &config).await.unwrap();
    assert!(repository.recovered);
    assert!(repository.all().await.unwrap().is_empty());
    assert!(
        std::fs::read_dir(dir.join("metadata"))
            .unwrap()
            .any(|entry| entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains(".corrupt-"))
    );
    repository.close().await;
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn obsolete_readers_and_refresh_handles_cannot_revert_cache_identity() {
    let dir = dir();
    let id = ConnectionId::new();
    let old_config = config();
    let old_reader = MetadataRepository::open(&dir, id, &old_config)
        .await
        .unwrap();
    seed(&old_reader).await;
    let mut current_config = old_config.clone();
    current_config.options["user"] = serde_json::json!("new-fixture-user");
    let current = MetadataRepository::prepare(&dir, id, &current_config)
        .await
        .unwrap();
    seed(&current).await;
    let expected = current.all().await.unwrap();
    let version = current.version().await.unwrap();

    assert!(
        MetadataRepository::open(&dir, id, &old_config)
            .await
            .is_err()
    );
    assert!(old_reader.begin(&ObjectPath::default()).await.is_err());
    assert_eq!(current.version().await.unwrap(), version);
    assert_eq!(current.all().await.unwrap(), expected);

    old_reader.close().await;
    current.close().await;
    let reopened = MetadataRepository::open(&dir, id, &current_config)
        .await
        .unwrap();
    assert_eq!(reopened.version().await.unwrap(), version);
    assert_eq!(reopened.all().await.unwrap(), expected);
    reopened.close().await;
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn a_scoped_refresh_into_an_empty_cache_publishes_its_namespace_ancestors() {
    let dir = dir();
    let repository = MetadataRepository::open(&dir, ConnectionId::new(), &config())
        .await
        .unwrap();
    let db = ObjectPath::default().child(ObjectKind::Database, "db.a/%_");
    let schema = db.child(ObjectKind::Schema, "sales");
    let table = MetadataObject::new(ObjectKind::Table, &schema, "orders");
    let mut run = repository.begin(&schema).await.unwrap();
    run.batch(vec![
        MetadataObject::new(ObjectKind::Schema, &db, "sales"),
        table.clone(),
    ])
    .await
    .unwrap();
    run.activate(&IntrospectionReport::default(), &CancellationToken::new())
        .await
        .unwrap();
    drop(run);
    let root = repository
        .children(&ObjectPath::default(), None, 10)
        .await
        .unwrap()
        .objects;
    assert_eq!(root.len(), 1);
    assert_eq!(root[0].path, db);
    assert_eq!(
        repository
            .children(&schema, None, 10)
            .await
            .unwrap()
            .objects,
        [table]
    );
    // A scope the server no longer has publishes nothing, not even ancestors.
    let other = ObjectPath::default()
        .child(ObjectKind::Database, "gone")
        .child(ObjectKind::Schema, "x");
    let mut run = repository.begin(&other).await.unwrap();
    run.activate(&IntrospectionReport::default(), &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(
        repository
            .children(&ObjectPath::default(), None, 10)
            .await
            .unwrap()
            .objects
            .len(),
        1
    );
    repository.close().await;
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn pages_follow_kind_order_and_continue_from_their_cursor() {
    let dir = dir();
    let repository = MetadataRepository::open(&dir, ConnectionId::new(), &config())
        .await
        .unwrap();
    let db = MetadataObject::new(ObjectKind::Database, &ObjectPath::default(), "db");
    let mut objects = vec![db.clone()];
    // Stored text would put materialized views first and views last.
    for (kind, prefix, count) in [
        (ObjectKind::View, "v", 3),
        (ObjectKind::MaterializedView, "m", 2),
        (ObjectKind::Table, "t", 5),
    ] {
        for index in 0..count {
            objects.push(MetadataObject::new(
                kind,
                &db.path,
                format!("{prefix}{index}"),
            ));
        }
    }
    let table = objects.last().unwrap().path.clone();
    for ordinal in [3, 1, 2] {
        let mut column = MetadataObject::new(ObjectKind::Column, &table, format!("c{ordinal}"));
        column.ordinal = Some(ordinal);
        objects.push(column);
    }
    let mut key = MetadataObject::new(ObjectKind::ForeignKey, &table, "fk");
    key.ordinal = None;
    objects.push(key);
    let mut run = repository.begin(&ObjectPath::default()).await.unwrap();
    run.batch(objects).await.unwrap();
    run.activate(&IntrospectionReport::default(), &CancellationToken::new())
        .await
        .unwrap();
    drop(run);
    // Pages of three cross from one kind to the next without repeating or
    // skipping a child.
    let mut names = Vec::new();
    let mut after = None;
    loop {
        let page = repository
            .children(&db.path, after.as_ref(), 3)
            .await
            .unwrap();
        names.extend(page.objects.into_iter().map(|object| object.name));
        match page.next {
            Some(next) => after = Some(next),
            None => break,
        }
    }
    assert_eq!(
        names,
        ["t0", "t1", "t2", "t3", "t4", "v0", "v1", "v2", "m0", "m1"]
    );
    // Columns follow their ordinal, then keys.
    let (version, pages) = repository
        .pages(&[
            PageRequest {
                parent: table.clone(),
                after: None,
                limit: 2,
            },
            PageRequest {
                parent: ObjectPath::default(),
                after: None,
                limit: 10,
            },
        ])
        .await
        .unwrap();
    assert_eq!(version, repository.version().await.unwrap());
    let columns: Vec<_> = pages[0].objects.iter().map(|o| o.name.as_str()).collect();
    assert_eq!(columns, ["c1", "c2"]);
    let rest = repository
        .children(&table, pages[0].next.as_ref(), 10)
        .await
        .unwrap();
    let rest: Vec<_> = rest.objects.iter().map(|o| o.name.as_str()).collect();
    assert_eq!(rest, ["c3", "fk"]);
    assert_eq!(pages[1].objects, [db]);
    assert!(pages[1].next.is_none());
    repository.close().await;
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn retain_keeps_the_selection_its_ancestors_and_named_nodes_only() {
    let dir = dir();
    let repository = MetadataRepository::open(&dir, ConnectionId::new(), &config())
        .await
        .unwrap();
    let db = MetadataObject::new(ObjectKind::Database, &ObjectPath::default(), "db.a/%_");
    let kept = MetadataObject::new(ObjectKind::Schema, &db.path, "kept");
    // A name that is a prefix of the kept one must not survive with it.
    let dropped = MetadataObject::new(ObjectKind::Schema, &db.path, "kept.more");
    let kept_table = MetadataObject::new(ObjectKind::Table, &kept.path, "orders");
    let dropped_table = MetadataObject::new(ObjectKind::Table, &dropped.path, "orders");
    let mut run = repository.begin(&ObjectPath::default()).await.unwrap();
    run.batch(vec![
        db.clone(),
        kept.clone(),
        dropped.clone(),
        kept_table.clone(),
        dropped_table,
    ])
    .await
    .unwrap();
    run.activate(&IntrospectionReport::default(), &CancellationToken::new())
        .await
        .unwrap();
    drop(run);
    for scope in [&kept.path, &dropped.path] {
        let mut run = repository.begin(scope).await.unwrap();
        run.fail(&DriverError::cancelled()).await.unwrap();
    }
    let before = repository.version().await.unwrap();
    // Keeping the root keeps everything.
    assert_eq!(
        repository
            .retain(&[ObjectPath::default()], &[])
            .await
            .unwrap(),
        before
    );
    assert_eq!(repository.all().await.unwrap().len(), 5);
    let catalog = ObjectPath::default().child(ObjectKind::Database, "empty");
    let version = repository
        .retain(
            std::slice::from_ref(&kept.path),
            std::slice::from_ref(&catalog),
        )
        .await
        .unwrap();
    assert_eq!(version, before + 1);
    assert_eq!(repository.version().await.unwrap(), version);
    let paths: Vec<_> = repository
        .all()
        .await
        .unwrap()
        .into_iter()
        .map(|object| object.path)
        .collect();
    assert_eq!(paths.len(), 4);
    for path in [&db.path, &kept.path, &kept_table.path, &catalog] {
        assert!(paths.contains(path), "{path:?} was removed");
    }
    let scopes: Vec<_> = repository
        .statuses()
        .await
        .unwrap()
        .into_iter()
        .map(|status| status.scope)
        .collect();
    assert!(scopes.contains(&kept.path));
    assert!(!scopes.contains(&dropped.path));
    assert_eq!(
        repository.paths(ObjectKind::Schema).await.unwrap(),
        std::slice::from_ref(&kept.path)
    );
    // Nothing selected leaves an empty cache.
    repository.retain(&[], &[]).await.unwrap();
    assert!(repository.all().await.unwrap().is_empty());
    repository.close().await;
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn a_version_1_cache_migrates_with_unicode_search_names() {
    let dir = dir();
    let id = ConnectionId::new();
    let path = dir.join("metadata");
    std::fs::create_dir_all(&path).unwrap();
    // A file as version 1 left it: its schema and one object, no search name.
    let pool = open_pool(&path.join(format!("{id}.db")), false)
        .await
        .unwrap();
    sqlx::raw_sql(SCHEMA).execute(&pool).await.unwrap();
    sqlx::raw_sql("PRAGMA user_version=1")
        .execute(&pool)
        .await
        .unwrap();
    let table = MetadataObject::new(ObjectKind::Table, &ObjectPath::default(), "AÇÃO");
    sqlx::query("INSERT INTO objects(path_key,parent_key,path_json,kind,name,ordinal,object_json) VALUES(?,NULL,?,?,?,NULL,?)")
        .bind(key(&table.path).unwrap())
        .bind(serde_json::to_string(&table.path).unwrap())
        .bind(serde_json::to_string(&table.kind).unwrap())
        .bind(&table.name)
        .bind(serde_json::to_string(&table).unwrap())
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO scope_status(scope_key,scope_json,state,last_run,last_attempt_at,last_success_at,version) VALUES('','[]','success',1,'2026-01-01T00:00:00.000Z','2026-01-01T00:00:00.000Z',1)")
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;
    let repository = MetadataRepository::open(&dir, id, &config()).await.unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, i64>("PRAGMA user_version")
            .fetch_one(&repository.pool)
            .await
            .unwrap(),
        2
    );
    // The object written before the step is found by its Unicode name.
    assert_eq!(repository.search("ação", None, 10).await.unwrap(), [table]);
    let status = &repository.statuses().await.unwrap()[0];
    assert_eq!(
        status.last_published_at.as_deref(),
        Some("2026-01-01T00:00:00.000Z")
    );
    // Opening again runs no step twice.
    repository.close().await;
    let repository = MetadataRepository::open(&dir, id, &config()).await.unwrap();
    repository.close().await;
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn the_search_folds_unicode_and_filters_kinds_before_the_limit() {
    let dir = dir();
    let repository = MetadataRepository::open(&dir, ConnectionId::new(), &config())
        .await
        .unwrap();
    let db = MetadataObject::new(ObjectKind::Database, &ObjectPath::default(), "db");
    let mut objects = vec![db.clone()];
    let accented = MetadataObject::new(ObjectKind::Table, &db.path, "AÇÃO");
    objects.push(accented.clone());
    // More columns called id than the limit, then the table called orders_id.
    for index in 0..210 {
        let table = MetadataObject::new(ObjectKind::Table, &db.path, format!("t{index:03}"));
        objects.push(MetadataObject::new(ObjectKind::Column, &table.path, "id"));
        objects.push(table);
    }
    let orders = MetadataObject::new(ObjectKind::Table, &db.path, "orders_id");
    objects.push(orders.clone());
    let mut run = repository.begin(&ObjectPath::default()).await.unwrap();
    run.batch(objects).await.unwrap();
    run.activate(&IntrospectionReport::default(), &CancellationToken::new())
        .await
        .unwrap();
    drop(run);
    let found = repository.search("ação", None, 200).await.unwrap();
    assert!(found.contains(&accented));
    // The database comes along as the match's ancestor.
    assert!(found.contains(&db));
    let all = repository.search("id", None, 200).await.unwrap();
    assert!(!all.contains(&orders));
    let tables = repository
        .search("id", Some(&[ObjectKind::Table]), 200)
        .await
        .unwrap();
    assert!(tables.contains(&orders));
    repository.close().await;
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn a_partial_refresh_records_when_it_published() {
    let dir = dir();
    let repository = MetadataRepository::open(&dir, ConnectionId::new(), &config())
        .await
        .unwrap();
    let mut run = repository.begin(&ObjectPath::default()).await.unwrap();
    run.batch(objects()).await.unwrap();
    let report = IntrospectionReport {
        warnings: vec![IntrospectionWarning {
            path: objects()[1].path.clone(),
            message: "fixture denied".into(),
        }],
    };
    run.activate(&report, &CancellationToken::new())
        .await
        .unwrap();
    drop(run);
    let root = repository
        .statuses()
        .await
        .unwrap()
        .into_iter()
        .find(|status| status.scope.0.is_empty())
        .unwrap();
    assert_eq!(root.state, RefreshState::Partial);
    assert!(root.last_success_at.is_none());
    assert!(root.last_published_at.is_some());
    repository.close().await;
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn a_cache_moves_to_a_new_identity_without_losing_its_objects() {
    let dir = dir();
    let id = ConnectionId::new();
    let full = config();
    let repository = MetadataRepository::open(&dir, id, &full).await.unwrap();
    seed(&repository).await;
    repository.close().await;
    let narrower = ConnectionConfig {
        driver: full.driver,
        options: serde_json::json!({"host":"localhost","user":"fixture"}),
    };
    MetadataRepository::adopt(&dir, id, &full, &narrower)
        .await
        .unwrap();
    let repository = MetadataRepository::open(&dir, id, &narrower).await.unwrap();
    assert_eq!(repository.all().await.unwrap().len(), objects().len());
    repository.close().await;
    // A cache of another identity is left alone.
    let other = ConnectionConfig {
        driver: full.driver,
        options: serde_json::json!({"host":"elsewhere","user":"fixture"}),
    };
    MetadataRepository::adopt(&dir, id, &full, &other)
        .await
        .unwrap();
    assert!(MetadataRepository::open(&dir, id, &other).await.is_err());
    // No cache, nothing created.
    let missing = ConnectionId::new();
    MetadataRepository::adopt(&dir, missing, &full, &narrower)
        .await
        .unwrap();
    assert!(!dir.join("metadata").join(format!("{missing}.db")).exists());
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn a_scope_denied_as_a_whole_keeps_its_read_and_says_no_access() {
    let dir = dir();
    let repository = MetadataRepository::open(&dir, ConnectionId::new(), &config())
        .await
        .unwrap();
    seed(&repository).await;
    let schema = objects()[0].path.clone();
    let run = repository.begin(&schema).await.unwrap();
    let report = IntrospectionReport {
        warnings: vec![IntrospectionWarning {
            path: schema.clone(),
            message: "fixture denied".into(),
        }],
    };
    let mut run = run;
    run.activate(&report, &CancellationToken::new())
        .await
        .unwrap();
    drop(run);
    assert!(repository.all().await.unwrap().contains(&objects()[3]));
    let status = repository
        .statuses()
        .await
        .unwrap()
        .into_iter()
        .find(|status| status.scope == schema)
        .unwrap();
    assert_eq!(status.state, RefreshState::Warning);
    repository.close().await;
    std::fs::remove_dir_all(dir).unwrap();
}
