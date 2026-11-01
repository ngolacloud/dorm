//! B-24 end to end on SQLite: a hinted column rename preserves the data, where
//! the old drop+add would have lost it. Zero infrastructure — `sqlite::memory:`
//! and the `sqlite` feature, the same shape as `migrate_deploy_sqlite`.
//!
//! ```text
//! cargo test -p dorm-migrations --features sqlite --test rename_preserves_data_sqlite
//! ```
#![cfg(feature = "sqlite")]

use dorm_core::db;
use dorm_core::meta::{FieldMeta, ModelMeta};
use dorm_core::value::{FieldType, Value};
use dorm_migrations::ddl::Backend;
use dorm_migrations::{autodetect, autodetect_with_renames, ColumnRename, Migration, Snapshot};
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
const ID: FieldMeta = FieldMeta {
    primary_key: true,
    ..col("id", FieldType::I64)
};
const TITLE: FieldMeta = col("title", FieldType::Text);
const HEADLINE: FieldMeta = col("headline", FieldType::Text);

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
const V2_FIELDS: &[FieldMeta] = &[ID, HEADLINE];

#[test]
fn a_hinted_rename_keeps_the_rows_data() {
    std::env::set_var("DELONIX_ORM_DSN", "sqlite::memory:");

    let v1: &'static ModelMeta = Box::leak(Box::new(model(V1_FIELDS)));
    let v2: &'static ModelMeta = Box::leak(Box::new(model(V2_FIELDS)));

    // v1: create the table with `title`.
    let m1 = Migration {
        version: 1,
        name: "initial".into(),
        statements: autodetect(&Snapshot::empty(), &[v1], Backend::Sqlite).unwrap(),
    };
    // v2: rename `title` -> `headline`, declared by the author.
    let m2 = Migration {
        version: 2,
        name: "rename_title_to_headline".into(),
        statements: autodetect_with_renames(
            &Snapshot::from_models(&[v1]),
            &[v2],
            Backend::Sqlite,
            &[ColumnRename {
                table: "notes".into(),
                from: "title".into(),
                to: "headline".into(),
            }],
        )
        .unwrap(),
    };
    // The rename migration is exactly a RENAME COLUMN — no drop, no add.
    assert_eq!(
        m2.statements,
        vec!["ALTER TABLE notes RENAME COLUMN title TO headline"],
        "a pure rename is one RENAME COLUMN"
    );

    runtime().block_on(async {
        dorm_migrations::migrate(&[m1]).await.expect("deploy v1");

        // A row written under the OLD name.
        db::run(
            "INSERT INTO notes (title) VALUES (?1)",
            &[Value::Text("hello".into())],
        )
        .await
        .expect("insert under the old name");

        dorm_migrations::migrate(&[m2])
            .await
            .expect("deploy the rename");

        // The data is readable under the NEW name — it was carried across, not
        // dropped. A drop+add would return NULL (or no column) here.
        let value: String = db::fetch("SELECT headline FROM notes WHERE id = 1", &[])
            .await
            .expect("read under the new name")
            .first()
            .expect("the row is still there")
            .try_get::<String>(0)
            .expect("the renamed column holds a value");
        assert_eq!(value, "hello", "the rename preserved the row's data");
    });
}
