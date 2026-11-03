//! `revert` end to end on SQLite (ADR 0014): an additive migration rolls back
//! cleanly, and an irreversible one (a dropped column) stops the revert by name
//! rather than losing data. Zero infrastructure — `sqlite::memory:`.
//!
//! ```text
//! cargo test -p dorm-migrations --features sqlite --test revert_sqlite
//! ```
//!
//! One `#[test]`: the ORM core's SQLite connection is a single process-global and
//! `sqlite::memory:` is one database, so the phases run in order within it.
#![cfg(feature = "sqlite")]

use dorm_core::db;
use dorm_core::meta::{FieldMeta, ModelMeta};
use dorm_migrations::ddl::Backend;
use dorm_migrations::{make_migration, revert, Snapshot};
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
use dorm_core::value::FieldType;
const ID: FieldMeta = FieldMeta {
    primary_key: true,
    ..col("id", FieldType::I64)
};
const TITLE: FieldMeta = col("title", FieldType::Text);
const NULLABLE_BODY: FieldMeta = FieldMeta {
    nullable: true,
    ..col("body", FieldType::Text)
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

const V1: &[FieldMeta] = &[ID, TITLE];
const V2: &[FieldMeta] = &[ID, TITLE, NULLABLE_BODY];

/// Whether `notes.body` exists, probed by selecting it (an absent column errors).
/// Uses the backend-agnostic `db::sql` — `db::query` is the PostgreSQL-native path.
async fn has_body() -> bool {
    db::sql("SELECT body FROM notes LIMIT 0", &[]).await.is_ok()
}

/// The versions the ledger records as applied.
async fn applied() -> Vec<i64> {
    let mut v: Vec<i64> = db::fetch("SELECT version FROM orm_schema_migrations", &[])
        .await
        .unwrap()
        .iter()
        .map(|r| r.try_get::<i64>(0).unwrap())
        .collect();
    v.sort_unstable();
    v
}

#[test]
fn an_additive_migration_reverts_and_an_irreversible_one_is_refused() {
    std::env::set_var("DELONIX_ORM_DSN", "sqlite::memory:");
    let v1: &'static ModelMeta = Box::leak(Box::new(model(V1)));
    let v2: &'static ModelMeta = Box::leak(Box::new(model(V2)));

    runtime().block_on(async {
        // The chain: v1 (create the table), then v2 (add the nullable `body`).
        let (m1, s1) = make_migration(&Snapshot::empty(), &[v1], 1, "initial", Backend::Sqlite)
            .unwrap()
            .expect("initial has changes");
        let (m2, s2) = make_migration(&s1, &[v2], 2, "add_body", Backend::Sqlite)
            .unwrap()
            .expect("add_body has changes");

        // An additive migration records a real `down`.
        assert!(!m2.down.is_empty(), "add_body must be reversible");

        // Apply both.
        dorm_migrations::migrate(std::slice::from_ref(&m1))
            .await
            .unwrap();
        dorm_migrations::migrate(std::slice::from_ref(&m2))
            .await
            .unwrap();
        assert!(has_body().await, "body exists after applying add_body");
        assert_eq!(applied().await, vec![1, 2]);

        // Revert to version 1: `body` is dropped and the ledger forgets v2.
        let undone = revert(&[m1.clone(), m2.clone()], 1).await.unwrap();
        assert_eq!(undone, vec![2], "only v2 is undone");
        assert!(!has_body().await, "body is gone after reverting to v1");
        assert_eq!(applied().await, vec![1]);

        // Re-apply: the column comes back — the migration is genuinely repeatable.
        dorm_migrations::migrate(std::slice::from_ref(&m2))
            .await
            .unwrap();
        assert!(has_body().await, "body is back after re-applying");
        assert_eq!(applied().await, vec![1, 2]);

        // Now an irreversible migration: v3 drops `body`. Its `down` is empty.
        let (m3, _s3) = make_migration(&s2, &[v1], 3, "drop_body", Backend::Sqlite)
            .unwrap()
            .expect("drop_body has changes");
        assert!(
            m3.down.is_empty(),
            "dropping a column cannot be auto-reversed"
        );
        dorm_migrations::migrate(std::slice::from_ref(&m3))
            .await
            .unwrap();
        assert!(!has_body().await, "body dropped by v3");
        assert_eq!(applied().await, vec![1, 2, 3]);

        // Reverting across the irreversible v3 is refused — and changes nothing.
        let err = revert(&[m1, m2, m3], 2)
            .await
            .expect_err("revert must refuse an irreversible migration");
        assert!(
            err.to_string().contains("not reversible"),
            "the refusal must name it: {err}"
        );
        assert_eq!(applied().await, vec![1, 2, 3], "the ledger is untouched");
    });
}
