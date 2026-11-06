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
/// The key versions 1 and 2 wrote: each segment's JSON in hex.
fn hex_key(path: &ObjectPath) -> String {
    path.0
        .iter()
        .map(|part| {
            serde_json::to_vec(part)
                .unwrap()
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("/")
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
        .bind(hex_key(&table.path))
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
        3
    );
    // The object written before the steps is found by its Unicode name.
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

#[test]
fn keys_round_trip_and_a_node_s_range_holds_its_descendants_only() {
    let root = ObjectPath::default();
    let database = MetadataObject::new(ObjectKind::Database, &root, "a/b%2F%25");
    let schema = MetadataObject::new(ObjectKind::Schema, &database.path, "ação");
    let table = MetadataObject::new(ObjectKind::Table, &schema.path, "t");
    // Names that are the database's name with the separator, the escape or
    // the character after the separator appended.
    let others: Vec<ObjectPath> = ["a/b%2F%25/x", "a/b%2F%250", "a/b%2F%25%", "a", "a/b"]
        .into_iter()
        .map(|name| MetadataObject::new(ObjectKind::Database, &root, name).path)
        .collect();
    for path in [&root, &database.path, &schema.path, &table.path]
        .into_iter()
        .chain(&others)
    {
        assert_eq!(&path_of(&key(path).unwrap()).unwrap(), path);
    }
    // The raw separator only ever separates segments.
    assert_eq!(key(&schema.path).unwrap().matches('/').count(), 1);
    let within = |inner: &ObjectPath, outer: &ObjectPath| {
        let (inner, outer) = (key(inner).unwrap(), key(outer).unwrap());
        inner >= format!("{outer}/") && inner < format!("{outer}0")
    };
    assert!(within(&schema.path, &database.path));
    assert!(within(&table.path, &database.path));
    assert!(within(&table.path, &schema.path));
    assert!(!within(&database.path, &database.path));
    for other in &others {
        assert!(!within(other, &database.path), "{other:?}");
        assert!(!within(&database.path, other), "{other:?}");
    }
    // A parent's key sorts before its children's.
    assert!(key(&database.path).unwrap() < key(&schema.path).unwrap());
}

#[tokio::test]
async fn a_version_2_cache_compacts_its_keys_and_reads_back_the_same() {
    let dir = dir();
    let id = ConnectionId::new();
    let config = config();
    let path = dir.join("metadata");
    std::fs::create_dir_all(&path).unwrap();
    // A file as version 2 left it: hex keys, paths in `path_json` and in
    // every object, a status per scope and a refresh left running.
    let pool = open_pool(&path.join(format!("{id}.db")), false)
        .await
        .unwrap();
    for step in [SCHEMA, STEP_2, STEP_2_INDEX, "PRAGMA user_version=2"] {
        sqlx::raw_sql(step).execute(&pool).await.unwrap();
    }
    sqlx::query("INSERT INTO metadata_info(singleton,identity,version) VALUES(1,?,1)")
        .bind(identity(&config).unwrap())
        .execute(&pool)
        .await
        .unwrap();
    let mut written = objects();
    let schema = MetadataObject::new(ObjectKind::Schema, &written[0].path, "sales/eu");
    let column = {
        let mut column = MetadataObject::new(ObjectKind::Column, &written[1].path, "new");
        column.ordinal = Some(2);
        column.data_type = Some("integer".into());
        column
    };
    written.extend([schema.clone(), column]);
    for object in &written {
        sqlx::query("INSERT INTO objects(path_key,parent_key,path_json,kind,name,ordinal,object_json,search_name) VALUES(?,?,?,?,?,?,?,?)")
            .bind(hex_key(&object.path))
            .bind(object.path.parent().filter(|parent| !parent.0.is_empty()).map(|parent| hex_key(&parent)))
            .bind(serde_json::to_string(&object.path).unwrap())
            .bind(serde_json::to_string(&object.kind).unwrap())
            .bind(&object.name)
            .bind(object.ordinal)
            .bind(serde_json::to_string(object).unwrap())
            .bind(search_name(&object.name))
            .execute(&pool)
            .await
            .unwrap();
    }
    for (scope, state) in [
        (&ObjectPath::default(), "success"),
        (&schema.path, "running"),
    ] {
        sqlx::query("INSERT INTO scope_status(scope_key,scope_json,state,last_run,last_attempt_at,last_success_at,last_published_at,version) VALUES(?,?,?,1,'2026-01-01T00:00:00.000Z','2026-01-01T00:00:00.000Z','2026-01-01T00:00:00.000Z',1)")
            .bind(hex_key(scope))
            .bind(serde_json::to_string(scope).unwrap())
            .bind(state)
            .execute(&pool)
            .await
            .unwrap();
    }
    sqlx::query(
        "INSERT INTO refresh_runs(scope_key,scope_json,base_version,state) VALUES(?,?,1,'running')",
    )
    .bind(hex_key(&schema.path))
    .bind(serde_json::to_string(&schema.path).unwrap())
    .execute(&pool)
    .await
    .unwrap();
    pool.close().await;

    let repository = MetadataRepository::open(&dir, id, &config).await.unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, i64>("PRAGMA user_version")
            .fetch_one(&repository.pool)
            .await
            .unwrap(),
        3
    );
    let mut expected = written.clone();
    expected.sort_by_key(|object| key(&object.path).unwrap());
    assert_eq!(repository.all().await.unwrap(), expected);
    // One copy of each path: the key. The vacuum gave the space back.
    let columns: Vec<String> = sqlx::query_scalar("SELECT name FROM pragma_table_info('objects')")
        .fetch_all(&repository.pool)
        .await
        .unwrap();
    assert!(!columns.iter().any(|column| column == "path_json"));
    let indexes: Vec<String> = sqlx::query_scalar(
        "SELECT name FROM sqlite_schema WHERE type='index' AND tbl_name='objects' AND sql IS NOT NULL ORDER BY name",
    )
    .fetch_all(&repository.pool)
    .await
    .unwrap();
    assert_eq!(indexes, ["objects_parent", "objects_search"]);
    let (json, free): (String, i64) = sqlx::query_as(
        "SELECT object_json,(SELECT freelist_count FROM pragma_freelist_count) FROM objects WHERE name='new'",
    )
    .fetch_one(&repository.pool)
    .await
    .unwrap();
    assert!(!json.contains("target"), "{json}");
    assert_eq!(free, 0);
    // Pages, search, relations and statuses read what version 2 held.
    let table = &written[1];
    let page = repository.children(&table.path, None, 10).await.unwrap();
    assert_eq!(page.objects, [written[2].clone(), written[5].clone()]);
    assert_eq!(
        repository.search("eu", None, 10).await.unwrap(),
        [written[0].clone(), schema.clone()]
    );
    let (_, relations) = repository.relations().await.unwrap();
    assert_eq!(
        relations,
        [
            (written[1].path.clone(), ObjectKind::Table),
            (written[3].path.clone(), ObjectKind::Table)
        ]
    );
    let statuses = repository.statuses().await.unwrap();
    assert_eq!(statuses.len(), 2);
    let left = statuses
        .iter()
        .find(|status| status.scope == schema.path)
        .unwrap();
    assert_eq!(left.state, RefreshState::Interrupted);
    // A refresh and a retain after the step find their ranges by the new keys.
    let mut run = repository.begin(&written[1].path).await.unwrap();
    run.batch(vec![table.clone()]).await.unwrap();
    run.activate(&IntrospectionReport::default(), &CancellationToken::new())
        .await
        .unwrap();
    drop(run);
    let after = repository.all().await.unwrap();
    assert!(
        !after
            .iter()
            .any(|object| object.name == "old" || object.name == "new")
    );
    assert!(after.contains(&written[3]));
    repository
        .retain(std::slice::from_ref(&schema.path), &[])
        .await
        .unwrap();
    assert_eq!(
        repository.all().await.unwrap(),
        [written[0].clone(), schema.clone()]
    );
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

#[tokio::test]
async fn a_step_that_breaks_a_reference_leaves_the_file_as_it_was() {
    let dir = dir();
    let id = ConnectionId::new();
    let path = dir.join("metadata");
    std::fs::create_dir_all(&path).unwrap();
    let file = path.join(format!("{id}.db"));
    let pool = open_pool(&file, false).await.unwrap();
    for step in [SCHEMA, STEP_2, STEP_2_INDEX, "PRAGMA user_version=2"] {
        sqlx::raw_sql(step).execute(&pool).await.unwrap();
    }
    // A column whose table is missing: a reference step 3 would carry over.
    let table = MetadataObject::new(ObjectKind::Table, &ObjectPath::default(), "gone");
    let column = MetadataObject::new(ObjectKind::Column, &table.path, "orphan");
    let mut connection = pool.acquire().await.unwrap();
    sqlx::raw_sql("PRAGMA foreign_keys=OFF")
        .execute(&mut *connection)
        .await
        .unwrap();
    sqlx::query("INSERT INTO objects(path_key,parent_key,path_json,kind,name,ordinal,object_json,search_name) VALUES(?,?,?,?,?,NULL,?,?)")
        .bind(hex_key(&column.path))
        .bind(hex_key(&table.path))
        .bind(serde_json::to_string(&column.path).unwrap())
        .bind(serde_json::to_string(&column.kind).unwrap())
        .bind(&column.name)
        .bind(serde_json::to_string(&column).unwrap())
        .bind(search_name(&column.name))
        .execute(&mut *connection)
        .await
        .unwrap();
    connection.detach().close().await.unwrap();
    pool.close().await;
    let refused = MetadataRepository::open(&dir, id, &config())
        .await
        .err()
        .unwrap();
    assert!(refused.message.contains("broken reference"), "{refused:?}");
    let pool = open_pool(&file, false).await.unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, i64>("PRAGMA user_version")
            .fetch_one(&pool)
            .await
            .unwrap(),
        2
    );
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT path_key FROM objects")
            .fetch_one(&pool)
            .await
            .unwrap(),
        hex_key(&column.path)
    );
    pool.close().await;
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn removing_a_deleted_connections_cache_leaves_the_others() {
    let dir = dir();
    let (gone, kept) = (ConnectionId::new(), ConnectionId::new());
    for id in [gone, kept] {
        let repository = MetadataRepository::open(&dir, id, &config()).await.unwrap();
        seed(&repository).await;
        repository.close().await;
    }
    MetadataRepository::remove(&dir, gone).await.unwrap();
    let names: Vec<String> = std::fs::read_dir(dir.join("metadata"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        names
            .iter()
            .all(|name| !name.starts_with(&gone.to_string()))
    );
    assert!(names.contains(&format!("{kept}.db")));
    // Nothing left to remove is fine.
    MetadataRepository::remove(&dir, gone).await.unwrap();
    let reopened = MetadataRepository::open(&dir, kept, &config())
        .await
        .unwrap();
    assert!(!reopened.all().await.unwrap().is_empty());
    reopened.close().await;
    std::fs::remove_dir_all(dir).unwrap();
}

/// Publishes `objects` as one full refresh.
async fn publish(repository: &MetadataRepository, objects: Vec<MetadataObject>) {
    let mut run = repository.begin(&ObjectPath::default()).await.unwrap();
    for chunk in objects.chunks(10_000) {
        run.batch(chunk.to_vec()).await.unwrap();
    }
    run.activate(&IntrospectionReport::default(), &CancellationToken::new())
        .await
        .unwrap();
}

/// The names of every match of `pattern`, read page by page.
async fn pattern_names(
    repository: &MetadataRepository,
    pattern: &str,
    kinds: Option<&[ObjectKind]>,
    namespace: Option<&ObjectPath>,
) -> Vec<String> {
    let mut names = Vec::new();
    let mut after = None;
    loop {
        let page = repository
            .search_pattern(pattern, kinds, namespace, after.as_ref(), 200)
            .await
            .unwrap();
        names.extend(page.objects.into_iter().map(|object| object.name));
        match page.next {
            Some(next) => after = Some(next),
            None => break,
        }
    }
    names.sort();
    names
}

#[tokio::test]
async fn a_pattern_search_reads_wildcards_by_unicode_lower_case_and_filters() {
    let dir = dir();
    let repository = MetadataRepository::open(&dir, ConnectionId::new(), &config())
        .await
        .unwrap();
    let db = MetadataObject::new(ObjectKind::Database, &ObjectPath::default(), "db");
    let sales = MetadataObject::new(ObjectKind::Schema, &db.path, "sales");
    let hr = MetadataObject::new(ObjectKind::Schema, &db.path, "hr");
    let mut objects = vec![db.clone(), sales.clone(), hr.clone()];
    for name in [
        "cliente",
        "CLIENTES_ATIVOS",
        "dim_cliente",
        "CLIÊNTE_VIP",
        "cliante",
        "pedido",
    ] {
        objects.push(MetadataObject::new(ObjectKind::Table, &sales.path, name));
    }
    let cliente = MetadataObject::new(ObjectKind::Table, &sales.path, "cliente");
    for name in ["cliente_id", "id", "nome"] {
        objects.push(MetadataObject::new(ObjectKind::Column, &cliente.path, name));
    }
    for name in ["cliente_rh", "a[b]c", "abc", "x]y", "a*b", "aXb"] {
        objects.push(MetadataObject::new(ObjectKind::Table, &hr.path, name));
    }
    publish(&repository, objects).await;
    let all = |pattern| pattern_names(&repository, pattern, None, None);
    assert_eq!(
        all("*cliente*").await,
        [
            "CLIENTES_ATIVOS",
            "cliente",
            "cliente_id",
            "cliente_rh",
            "dim_cliente"
        ]
    );
    assert_eq!(
        all("cliente*").await,
        ["CLIENTES_ATIVOS", "cliente", "cliente_id", "cliente_rh"]
    );
    assert_eq!(all("*_id").await, ["cliente_id"]);
    assert_eq!(all("cli?nte").await, ["cliante", "cliente"]);
    // Upper case and accents fold by Unicode rules; `ê` is not `e`.
    assert_eq!(all("*CLIÊNTE*").await, ["CLIÊNTE_VIP"]);
    assert_eq!(all("cli?nte_vip").await, ["CLIÊNTE_VIP"]);
    // Brackets are literal, with or without wildcards: `[b]` is not a class.
    assert_eq!(all("a[b]c").await, ["a[b]c"]);
    assert_eq!(all("*[b]*").await, ["a[b]c"]);
    assert_eq!(all("*]*").await, ["a[b]c", "x]y"]);
    assert_eq!(all("x]?").await, ["x]y"]);
    assert_eq!(all("a?b").await, ["a*b", "aXb"]);
    assert_eq!(all("*").await.len(), 18);
    // Without a wildcard, a pattern finds what the search finds.
    for query in ["cliente", "CLIENTE", "li", "e", "_id", "ênte"] {
        let mut expected: Vec<_> = repository
            .search(query, None, 200)
            .await
            .unwrap()
            .into_iter()
            .filter(|object| search_name(&object.name).contains(&search_name(query)))
            .map(|object| object.name)
            .collect();
        expected.sort();
        assert_eq!(all(query).await, expected, "{query}");
    }
    // Kinds and the namespace apply before the limit.
    assert_eq!(
        pattern_names(&repository, "*cliente*", Some(&[ObjectKind::Table]), None).await,
        ["CLIENTES_ATIVOS", "cliente", "cliente_rh", "dim_cliente"]
    );
    assert_eq!(
        pattern_names(&repository, "*cliente*", None, Some(&hr.path)).await,
        ["cliente_rh"]
    );
    assert_eq!(
        pattern_names(
            &repository,
            "cli*",
            Some(&[ObjectKind::Column]),
            Some(&sales.path)
        )
        .await,
        ["cliente_id"]
    );
    // Inside a namespace means below it: the node itself is not a match.
    assert!(
        pattern_names(&repository, "sales", None, Some(&sales.path))
            .await
            .is_empty()
    );
    assert_eq!(
        pattern_names(&repository, "sales", None, Some(&ObjectPath::default())).await,
        ["sales"]
    );
    // Matches carry their full path.
    let page = repository
        .search_pattern("dim_*", None, None, None, 10)
        .await
        .unwrap();
    assert_eq!(
        page.objects,
        [MetadataObject::new(
            ObjectKind::Table,
            &sales.path,
            "dim_cliente"
        )]
    );
    assert!(page.next.is_none());
    for limit in [0, 201] {
        assert!(
            repository
                .search_pattern("*", None, None, None, limit)
                .await
                .is_err()
        );
    }
    repository.close().await;
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn pattern_pages_return_every_match_exactly_once_in_order() {
    let dir = dir();
    let repository = MetadataRepository::open(&dir, ConnectionId::new(), &config())
        .await
        .unwrap();
    let db = MetadataObject::new(ObjectKind::Database, &ObjectPath::default(), "db");
    let mut objects = vec![db.clone()];
    // Many matches share a name, so pages break between equal names.
    for index in 0..150 {
        let table = MetadataObject::new(ObjectKind::Table, &db.path, format!("T{index:03}_id"));
        objects.push(MetadataObject::new(ObjectKind::Column, &table.path, "id"));
        objects.push(MetadataObject::new(
            ObjectKind::Column,
            &table.path,
            "ID_old",
        ));
        objects.push(table);
    }
    publish(&repository, objects).await;
    // `*id*` walks the name index; `*_id` goes through the trigram index.
    for (pattern, expected) in [("*id*", 450), ("*_id", 150), ("id", 450), ("*_old", 150)] {
        for limit in [1, 7, 200] {
            let mut seen = Vec::new();
            let mut after = None;
            loop {
                let page = repository
                    .search_pattern(pattern, None, None, after.as_ref(), limit)
                    .await
                    .unwrap();
                assert!(page.objects.len() <= limit as usize);
                seen.extend(page.objects);
                match page.next {
                    Some(next) => after = Some(next),
                    None => break,
                }
            }
            assert_eq!(seen.len(), expected, "{pattern} by {limit}");
            let keys: Vec<_> = seen
                .iter()
                .map(|object| (search_name(&object.name), key(&object.path).unwrap()))
                .collect();
            let mut sorted = keys.clone();
            sorted.sort();
            sorted.dedup();
            assert_eq!(sorted.len(), expected, "{pattern} by {limit}: repeated");
            assert!(
                keys.windows(2).all(|pair| pair[0].0 <= pair[1].0),
                "{pattern} by {limit}: out of order"
            );
        }
    }
    repository.close().await;
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn a_pattern_search_over_100_000_objects_stays_within_budget() {
    let dir = dir();
    let repository = MetadataRepository::open(&dir, ConnectionId::new(), &config())
        .await
        .unwrap();
    let db = MetadataObject::new(ObjectKind::Database, &ObjectPath::default(), "db");
    let mut objects = vec![db.clone()];
    let columns = [
        "id",
        "nome",
        "criado_em",
        "valor",
        "status",
        "cliente_id",
        "obs",
        "tipo",
    ];
    let mut schema_3 = None;
    for schema in 0..10 {
        let schema = MetadataObject::new(ObjectKind::Schema, &db.path, format!("schema_{schema}"));
        for table in 0..1_100 {
            let name = if table % 100 == 0 {
                format!("cliente_{table:04}")
            } else {
                format!("tabela_{table:04}")
            };
            let table = MetadataObject::new(ObjectKind::Table, &schema.path, name);
            for column in columns {
                objects.push(MetadataObject::new(ObjectKind::Column, &table.path, column));
            }
            objects.push(table);
        }
        if schema.name == "schema_3" {
            schema_3 = Some(schema.path.clone());
        }
        objects.push(schema);
    }
    assert!(objects.len() > 99_000);
    publish(&repository, objects).await;
    let schema_3 = schema_3.unwrap();
    for (pattern, kinds, namespace) in [
        ("*cliente*", None, None),
        ("*CLIENTE*", Some(&[ObjectKind::Table][..]), None),
        ("cli?nte_*", None, Some(&schema_3)),
        ("*_id", None, None),
        ("?d", None, None),
        ("q?", None, None),
        // No literal prefix and no trigram: the whole name index, no match.
        ("?q", None, None),
        ("*", Some(&[ObjectKind::Schema][..]), None),
    ] {
        let started = std::time::Instant::now();
        let page = repository
            .search_pattern(pattern, kinds, namespace, None, 200)
            .await
            .unwrap();
        let elapsed = started.elapsed();
        // About 80 ms at worst in a debug build (2026-11-06): the budget is
        // generous, for a loaded gate, and still fails a complexity regression.
        assert!(
            elapsed < std::time::Duration::from_secs(2),
            "{pattern}: {elapsed:?}"
        );
        if !pattern.contains('q') {
            assert!(!page.objects.is_empty(), "{pattern}");
        }
    }
    repository.close().await;
    std::fs::remove_dir_all(dir).unwrap();
}
