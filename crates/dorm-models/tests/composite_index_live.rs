//! Composite (`@@index([a, b])`) indexes against a real PostgreSQL (ADR 0044).
//!
//! ```text
//! DELONIX_ORM_TEST_DSN='host=/var/run/postgresql port=5432 user=walter dbname=ormtest' \
//!   cargo test -p dorm-models --test composite_index_live -- --ignored --nocapture
//! ```
//!
//! The composite index rides in the `IndexMeta` inventory slice — `ModelMeta`
//! gains no field. `ddl::indexes`/`indexes_concurrent` append it after the
//! per-column ones, so `sync` builds it `CONCURRENTLY` and idempotently.

use dorm_core::model::Model;
use dorm_derive::Model;
use tokio::runtime::Runtime;

/// An events table with a two-column index on `(tenant, created_at)` — the common
/// "latest rows for a tenant" access path — plus a single-column one on `status`.
#[derive(Model, Debug, Clone)]
#[model(
    table = "l5_events",
    ordering = "id",
    index_together = "tenant, created_at",
    index_together = "status"
)]
struct Event {
    #[field(primary_key)]
    id: i64,
    #[field(max_length = 40)]
    tenant: String,
    #[field(max_length = 20)]
    status: String,
    created_at: chrono::DateTime<chrono::Utc>,
}

fn dsn() -> Option<String> {
    std::env::var("DELONIX_ORM_TEST_DSN")
        .ok()
        .filter(|s| !s.trim().is_empty())
}

fn runtime() -> &'static Runtime {
    static RT: std::sync::OnceLock<Runtime> = std::sync::OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("a tokio runtime")
    })
}

/// Pure shape: the composite index resolves through the slice and `ddl::indexes`
/// emits one `CREATE INDEX` spanning both columns. Runs without a database.
#[test]
fn the_composite_index_resolves_and_emits_one_create_index_over_both_columns() {
    // The inventory slice has no guaranteed iteration order (it is linker-built),
    // so compare the index groups as a SET — two independent composite indexes on
    // one model have no inherent order between them.
    let mut groups: Vec<Vec<&str>> = Event::meta()
        .composite_indexes()
        .map(|g| g.to_vec())
        .collect();
    groups.sort();
    let mut expected = vec![vec!["status"], vec!["tenant", "created_at"]];
    expected.sort();
    assert_eq!(groups, expected);

    let idx = dorm_migrations::ddl::indexes(Event::meta());
    assert!(
        idx.iter()
            .any(|s| s.contains("l5_events_tenant_created_at_idx")
                && s.contains("(\"tenant\", \"created_at\")")),
        "a composite CREATE INDEX over both columns: {idx:?}"
    );
}

#[test]
#[ignore = "needs a Postgres (DELONIX_ORM_TEST_DSN)"]
fn sync_builds_the_composite_index_on_the_real_table() {
    let Some(dsn) = dsn() else {
        eprintln!("skipped: DELONIX_ORM_TEST_DSN is not set");
        return;
    };
    std::env::set_var(dorm_core::db::DSN_ENV, &dsn);

    runtime().block_on(async {
        dorm_core::db::execute("DROP TABLE IF EXISTS l5_events", &[])
            .await
            .expect("clean");

        // `sync` creates the missing table and then builds every declared index
        // CONCURRENTLY, outside the transaction — including the composite one.
        dorm_migrations::sync(&[Event::meta()])
            .await
            .expect("sync creates the table and its indexes");

        // The composite index exists and spans exactly (tenant, created_at).
        let rows = dorm_core::db::query(
            "SELECT indexdef FROM pg_indexes \
             WHERE tablename = 'l5_events' AND indexname = 'l5_events_tenant_created_at_idx'",
            &[],
        )
        .await
        .expect("pg_indexes lookup");
        let def = rows
            .first()
            .expect("the composite index was created")
            .try_get::<usize, String>(0)
            .expect("indexdef is text");
        assert!(
            def.contains("(tenant, created_at)"),
            "the index spans both columns in order: {def}"
        );

        // It is a valid index (a failed concurrent build would leave it INVALID).
        let valid = dorm_core::db::query(
            "SELECT i.indisvalid FROM pg_index i \
             JOIN pg_class c ON c.oid = i.indexrelid \
             WHERE c.relname = 'l5_events_tenant_created_at_idx'",
            &[],
        )
        .await
        .expect("pg_index lookup");
        assert!(
            valid
                .first()
                .and_then(|r| r.try_get::<usize, bool>(0).ok())
                .unwrap_or(false),
            "the concurrent build left a valid index"
        );

        // A second sync is a no-op for the index (IF NOT EXISTS) — idempotent.
        dorm_migrations::sync(&[Event::meta()])
            .await
            .expect("second sync is idempotent");

        eprintln!("ok: sync built the composite index (tenant, created_at) and it is valid");
    });
}
