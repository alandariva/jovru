//! Benchmarks for the hot paths listed in ADR-0010: building the grid view,
//! converting driver rows to result data, splitting large scripts and paging
//! metadata children from a large local cache.
//!
//! ```text
//! cargo bench --bench hot_paths -- --save-baseline main   # before a change
//! cargo bench --bench hot_paths -- --baseline main        # after it
//! ```
//!
//! All data is synthetic and built in memory or in a temporary folder.
// A development tool, like the tests: a setup that fails must stop the run.
#![allow(clippy::expect_used)]
use criterion::{BatchSize, Criterion, criterion_group, criterion_main};
use jykre::drivers::{
    CancellationToken, Cell, ColumnInfo, ConnectionConfig, ConnectionId, DriverKind,
    IntrospectionReport, MetadataObject, ObjectKind, ObjectPath, RowSink,
};
use jykre::grid::{GridKey, GridView};
use jykre::result::{
    DEFAULT_MEMORY_CEILING, MAX_ROW_LIMIT, ResultBuilder, ResultColumn, ResultData,
};
use jykre_storage::MetadataRepository;
use std::hint::black_box;

/// Shaped like the `sales.event_log` seed: 100,000 rows of five columns.
fn event_log(rows: usize) -> ResultData {
    let columns = [
        ("id", "int8"),
        ("level", "text"),
        ("message", "text"),
        ("occurred_at", "timestamptz"),
        ("payload", "jsonb"),
    ]
    .into_iter()
    .map(|(name, kind)| ResultColumn::new(name, kind))
    .collect();
    let mut data = ResultData::new(columns);
    for row in 0..rows {
        let cells = [
            row.to_string(),
            ["debug", "info", "warn", "error"][row % 4].to_owned(),
            format!("Synthetic event {row}"),
            format!("2026-01-01 00:{:02}:00+00", row % 60),
            format!("{{\"seq\": {row}}}"),
        ];
        data.push_row(cells.iter().map(|cell| Some(cell.as_str())));
    }
    data.shrink_to_fit();
    data
}

fn grid_view(c: &mut Criterion) {
    let result = event_log(100_000);
    let mut group = c.benchmark_group("grid view, 100,000 rows");
    group.sample_size(10);
    group.bench_function("unfiltered", |b| {
        b.iter(|| GridView::new(black_box(&result), GridKey::default()))
    });
    group.bench_function("filtered and reversed", |b| {
        let key = GridKey {
            filter: "error".into(),
            ..GridKey::default()
        };
        b.iter(|| GridView::new(black_box(&result), key.clone()))
    });
    group.finish();
}

/// 100,000 rows of 30 mixed driver cells, as a wide result arrives.
fn driver_rows(rows: usize) -> (Vec<ColumnInfo>, Vec<Vec<Cell>>) {
    let columns = (0..30)
        .map(|index| ColumnInfo {
            name: format!("column_{index}"),
            data_type: ["int8", "text", "numeric", "timestamptz", "bool"][index % 5].into(),
            nullable: Some(true),
        })
        .collect();
    let rows = (0..rows)
        .map(|row| {
            (0..30)
                .map(|column| match column % 6 {
                    0 => Cell::Integer(row as i64),
                    1 => Cell::Text(format!("text {row} {column}")),
                    2 => Cell::Decimal(format!("{row}.{column:02}")),
                    3 => Cell::Temporal("2026-11-02 10:00:00+00".into()),
                    4 => Cell::Boolean(row % 2 == 0),
                    _ => Cell::Null,
                })
                .collect()
        })
        .collect();
    (columns, rows)
}

fn result_conversion(c: &mut Criterion) {
    let (columns, rows) = driver_rows(100_000);
    let mut group = c.benchmark_group("result conversion");
    group.sample_size(10);
    group.bench_function("100,000 rows x 30 columns", |b| {
        b.iter_batched(
            || (columns.clone(), rows.clone()),
            |(columns, rows)| {
                let mut builder = ResultBuilder::new(MAX_ROW_LIMIT, DEFAULT_MEMORY_CEILING);
                builder.columns(columns);
                for row in rows {
                    builder.row(row);
                }
                builder.finish()
            },
            BatchSize::LargeInput,
        )
    });
    group.finish();
}

fn statement_splitting(c: &mut Criterion) {
    let script: String = (0..10_000)
        .map(|index| {
            format!(
                "-- statement {index}\nSELECT id, 'a;b' AS text, $$x;y$$ FROM t{index} /* ; */ WHERE id > {index};\n"
            )
        })
        .collect();
    c.bench_function("split a 10,000-statement script", |b| {
        b.iter(|| jykre::sql::statements(black_box(&script)))
    });
}

fn large_cache_config() -> ConnectionConfig {
    ConnectionConfig {
        driver: DriverKind::Postgres,
        options: serde_json::json!({"host":"localhost","user":"fixture","database":"fixture","sslMode":"disable"}),
    }
}

/// A cache of 500,101 objects: one database, 100 schemas of 5,000 tables.
async fn large_cache(dir: &std::path::Path, id: ConnectionId) -> (MetadataRepository, ObjectPath) {
    let repository = MetadataRepository::open(dir, id, &large_cache_config())
        .await
        .expect("a cache in a temporary folder");
    let database = MetadataObject::new(ObjectKind::Database, &ObjectPath::default(), "fixture");
    let mut pending = repository
        .begin(&ObjectPath::default())
        .await
        .expect("a refresh");
    pending
        .batch(vec![database.clone()])
        .await
        .expect("a batch");
    let mut middle = ObjectPath::default();
    for schema in 0..100 {
        let schema = MetadataObject::new(
            ObjectKind::Schema,
            &database.path,
            format!("schema_{schema:03}"),
        );
        let tables = (0..5_000)
            .map(|table| {
                MetadataObject::new(ObjectKind::Table, &schema.path, format!("table_{table:05}"))
            })
            .collect::<Vec<_>>();
        if schema.name == "schema_050" {
            middle = schema.path.clone();
        }
        pending.batch(vec![schema]).await.expect("a batch");
        for chunk in tables.chunks(5_000) {
            pending.batch(chunk.to_vec()).await.expect("a batch");
        }
    }
    pending
        .activate(&IntrospectionReport::default(), &CancellationToken::new())
        .await
        .expect("published");
    drop(pending);
    (repository, middle)
}

fn metadata_paging(c: &mut Criterion) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a runtime");
    let dir = std::env::temp_dir().join(format!("jykre-bench-{}", ConnectionId::new()));
    let id = ConnectionId::new();
    let (repository, schema) = runtime.block_on(large_cache(&dir, id));
    let mut group = c.benchmark_group("metadata paging, 500,000 objects");
    group.sample_size(20);
    // What expanding a node cost when every read opened the cache.
    group.bench_function("open the cache and read a first page", |b| {
        let config = large_cache_config();
        b.iter(|| {
            runtime.block_on(async {
                let reader = MetadataRepository::open(&dir, id, &config).await?;
                let page = reader.children(black_box(&schema), None, 200).await;
                reader.close().await;
                page
            })
        })
    });
    // Expanding now reads through the cache kept open.
    group.bench_function("first page of 200 children", |b| {
        b.iter(|| runtime.block_on(repository.children(black_box(&schema), None, 200)))
    });
    // The cursor where the 25th page starts, as "Load more" keeps it.
    let after = runtime.block_on(async {
        let mut after = None;
        for _ in 0..24 {
            after = repository
                .children(&schema, after.as_ref(), 200)
                .await
                .expect("a page")
                .next;
        }
        after
    });
    group.bench_function("page of 200 children at offset 4,800", |b| {
        b.iter(|| runtime.block_on(repository.children(black_box(&schema), after.as_ref(), 200)))
    });
    // The palette's search among 500,000 objects (budget 50 ms): a name of
    // three characters or more goes through the trigram index; a shorter
    // one scans the name index.
    group.bench_function("search a name among 500,000 objects", |b| {
        b.iter(|| runtime.block_on(repository.search(black_box("table_04999"), None, 200)))
    });
    group.bench_function("search two characters among 500,000 objects", |b| {
        b.iter(|| runtime.block_on(repository.search(black_box("zq"), None, 200)))
    });
    group.finish();
    runtime.block_on(repository.close());
    // Best effort: the folder is temporary.
    let _ = std::fs::remove_dir_all(dir);
}

criterion_group!(
    benches,
    grid_view,
    result_conversion,
    statement_splitting,
    metadata_paging
);
criterion_main!(benches);
