//! `migrate` (the `migrate deploy` shape) end to end on SQLite — the proof that
//! a whole autodetected chain applies in order, records its ledger, and is
//! idempotent, all with zero infrastructure.
//!
//! The migrator was PostgreSQL-only until it was made backend-agnostic; this is
//! the test that pins the SQLite half. Gated behind the `sqlite` feature, which
//! turns on the ORM core's SQLite backend:
//!
//! ```text
//! cargo test -p dorm-migrations --features sqlite --test migrate_deploy_sqlite
//! ```
//!
//! One `#[test]`: the ORM core's SQLite connection is a single process-global,
//! and `sqlite::memory:` is one database, so the phases run in order within it.
#![cfg(feature = "sqlite")]

use dorm_core::db;
use dorm_core::meta::{FieldMeta, ModelMeta};
use dorm_core::value::{FieldType, Value};
use dorm_migrations::ddl::Backend;
use dorm_migrations::{autodetect, Migration, Snapshot};
use tokio::runtime::Runtime;

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

// Hand-built, relation-free metadata — no derive, no registry — the same
// approach the ddl/autodetect unit tests take.
const fn col(name: &'static str, ty: FieldType) -> FieldMeta {
    FieldMeta {
        name,
        column: name,
        ty,
        nullable: false,
        primary_key: false,
        unique: false,
        index: false,
        max_length: None,
        default: None,
        auto_now_add: false,
        auto_now: false,
        relation: None,
    }
}
const ID: FieldMeta = FieldMeta {
    primary_key: true,
    ..col("id", FieldType::I64)
};
const TITLE: FieldMeta = col("title", FieldType::Text);
const NULLABLE_BODY: FieldMeta = FieldMeta {
    nullable: true,
    ..col("body", FieldType::Text)
};
const NULLABLE_NOTE: FieldMeta = FieldMeta {
    nullable: true,
    ..col("note", FieldType::Text)
};

fn model(fields: &'static [FieldMeta]) -> ModelMeta {
    ModelMeta {
        name: "Note",
        table: "notes",
        fields,
        ordering: &[],
        tenant_scoped: false,
        unique_together: &[],
        checks: &[],
    }
}

const V1_FIELDS: &[FieldMeta] = &[ID, TITLE];
const V2_FIELDS: &[FieldMeta] = &[ID, TITLE, NULLABLE_BODY];
const V3_FIELDS: &[FieldMeta] = &[ID, TITLE, NULLABLE_BODY, NULLABLE_NOTE];

#[test]
fn a_whole_autodetected_chain_deploys_in_order_and_is_idempotent() {
    std::env::set_var("DELONIX_ORM_DSN", "sqlite::memory:");

    // Leak the per-version models to `&'static` (the autodetector takes
    // `&'static ModelMeta`); a test process ending reclaims it.
    let v1: &'static ModelMeta = Box::leak(Box::new(model(V1_FIELDS)));
    let v2: &'static ModelMeta = Box::leak(Box::new(model(V2_FIELDS)));
    let v3: &'static ModelMeta = Box::leak(Box::new(model(V3_FIELDS)));

    // Build the chain the way a project would: each migration is the diff from
    // the previous snapshot, and carries the snapshot it produces forward.
    let s0 = Snapshot::empty();
    let m1 = Migration {
        version: 1,
        name: "initial".into(),
        statements: autodetect(&s0, &[v1], Backend::Sqlite).unwrap(),
    };
    let s1 = Snapshot::from_models(&[v1]);
    let m2 = Migration {
        version: 2,
        name: "add_body".into(),
        statements: autodetect(&s1, &[v2], Backend::Sqlite).unwrap(),
    };
    let s2 = Snapshot::from_models(&[v2]);
    let m3 = Migration {
        version: 3,
        name: "add_note".into(),
        statements: autodetect(&s2, &[v3], Backend::Sqlite).unwrap(),
    };
    let chain = vec![m1, m2, m3];

    runtime().block_on(async {
        // First deploy: all three versions apply, in order.
        let applied = dorm_migrations::migrate(&chain)
            .await
            .expect("first deploy");
        assert_eq!(applied, vec![1, 2, 3], "the whole chain applied in order");

        // The ledger records exactly those three.
        let versions = ledger_versions().await;
        assert_eq!(versions, vec![1, 2, 3], "the ledger has the chain");

        // The schema really evolved: a row using the v3 columns is accepted.
        db::run(
            "INSERT INTO notes (title, body, note) VALUES (?1, ?2, ?3)",
            &[
                Value::Text("t".into()),
                Value::Text("b".into()),
                Value::Text("n".into()),
            ],
        )
        .await
        .expect("insert using the columns the chain added");

        // Second deploy of the same chain is a no-op: everything is recorded.
        let again = dorm_migrations::migrate(&chain).await.expect("re-deploy");
        assert!(again.is_empty(), "a re-deploy applies nothing: {again:?}");
        assert_eq!(
            ledger_versions().await,
            vec![1, 2, 3],
            "no duplicate ledger rows"
        );
    });
}

/// Read the applied versions out of the ledger, sorted.
async fn ledger_versions() -> Vec<i64> {
    let mut v: Vec<i64> = db::fetch("SELECT version FROM orm_schema_migrations", &[])
        .await
        .expect("read the ledger")
        .iter()
        .map(|r| r.try_get::<i64>(0).expect("a version"))
        .collect();
    v.sort_unstable();
    v
}
