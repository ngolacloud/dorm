//! The whole `dorm migrate make` → `dorm migrate deploy` loop, end to end on
//! SQLite with zero infrastructure: author two migrations to an on-disk history,
//! then deploy the chain it holds and watch the schema appear.
//!
//! Gated behind the `sqlite` feature (it turns on the ORM core's SQLite backend):
//!
//! ```text
//! cargo test -p dorm-migrations --features sqlite --test history_deploy_sqlite
//! ```
#![cfg(feature = "sqlite")]

use std::path::PathBuf;

use dorm_core::db;
use dorm_core::meta::{FieldMeta, ModelMeta};
use dorm_core::value::{FieldType, Value};
use dorm_migrations::ddl::Backend;
use dorm_migrations::History;
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

struct TempDir(PathBuf);
impl TempDir {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("dorm-hist-e2e-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        TempDir(dir)
    }
}
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
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
const ID: FieldMeta = FieldMeta {
    primary_key: true,
    ..col("id", FieldType::I64)
};
const BODY: FieldMeta = FieldMeta {
    nullable: true,
    ..col("body", FieldType::Text)
};
const V1: &[FieldMeta] = &[ID, col("title", FieldType::Text)];
const V2: &[FieldMeta] = &[ID, col("title", FieldType::Text), BODY];

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

#[test]
fn make_writes_the_history_and_deploy_applies_it() {
    std::env::set_var("DELONIX_ORM_DSN", "sqlite::memory:");
    let tmp = TempDir::new();
    let h = History::open(&tmp.0);
    let v1: &'static ModelMeta = Box::leak(Box::new(model(V1)));
    let v2: &'static ModelMeta = Box::leak(Box::new(model(V2)));

    // `dorm migrate make` twice: an initial, then an add-column diff.
    h.make(&[v1], "initial", Backend::Sqlite).unwrap().unwrap();
    h.make(&[v2], "add body", Backend::Sqlite).unwrap().unwrap();

    runtime().block_on(async {
        // `dorm migrate deploy`: apply the whole chain the history holds.
        let applied = dorm_migrations::migrate(&h.to_migrations().unwrap())
            .await
            .expect("deploy the chain");
        assert_eq!(applied, vec![1, 2], "both migrations applied in order");

        // The schema the files described is really there.
        db::run(
            "INSERT INTO notes (title, body) VALUES (?1, ?2)",
            &[Value::Text("t".into()), Value::Text("b".into())],
        )
        .await
        .expect("insert using the column the chain added");

        // Deploying again is a no-op — the ledger already has the chain.
        let again = dorm_migrations::migrate(&h.to_migrations().unwrap())
            .await
            .expect("re-deploy");
        assert!(
            again.is_empty(),
            "a second deploy applies nothing: {again:?}"
        );
    });
}
