//! Versioned `migrate` creates and evolves a native enum type (ADR 0046/0053
//! follow-up) against a real PostgreSQL. The autodetector now emits `CREATE TYPE`
//! (before the table that uses it) and `ALTER TYPE … ADD VALUE`; this proves both
//! actually apply inside `migrate`'s transaction.
//!
//! ```text
//! DELONIX_ORM_TEST_DSN='host=/var/run/postgresql port=5432 user=walter dbname=ormtest' \
//!   cargo test -p dorm-migrations --test migrate_enum_live -- --ignored --nocapture
//! ```

use dorm_core::db;
use dorm_core::meta::{FieldMeta, ModelMeta};
use dorm_core::value::{FieldType, Value};
use dorm_migrations::ddl::Backend;
use dorm_migrations::{autodetect, Migration, Snapshot};
use tokio::runtime::Runtime;

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
            .unwrap()
    })
}

// A model with an enum column; its variants live in the EnumMeta slice.
dorm_core::inventory::submit! {
    dorm_core::meta::EnumMeta { type_name: "mig_status", variants: &["open", "closed", "archived"] }
}

const fn field(name: &'static str, ty: FieldType, pk: bool) -> FieldMeta {
    FieldMeta {
        name,
        column: name,
        ty,
        nullable: false,
        primary_key: pk,
        unique: false,
        index: false,
        max_length: None,
        default: None,
        auto_now_add: false,
        auto_now: false,
        relation: None,
    }
}

const FIELDS: &[FieldMeta] = &[
    field("id", FieldType::I64, true),
    field("status", FieldType::Enum("mig_status"), false),
];
const ORDER_META: ModelMeta = ModelMeta {
    name: "MigOrder",
    table: "mig_orders",
    fields: FIELDS,
    ordering: &[],
    tenant_scoped: false,
    unique_together: &[],
    checks: &[],
};

#[test]
#[ignore = "needs a Postgres (DELONIX_ORM_TEST_DSN)"]
fn a_versioned_migration_creates_then_grows_a_native_enum() {
    let Some(dsn) = dsn() else {
        eprintln!("skipped: DELONIX_ORM_TEST_DSN is not set");
        return;
    };
    std::env::set_var(db::DSN_ENV, &dsn);

    runtime().block_on(async {
        // Clean slate, including this test's rows in the shared ledger.
        for stmt in [
            "DELETE FROM orm_schema_migrations WHERE version IN (4601, 4602)",
            "DROP TABLE IF EXISTS mig_orders",
            "DROP TYPE IF EXISTS mig_status",
        ] {
            db::execute(stmt, &[]).await.ok();
        }

        // m1: the autodetector turns "empty → model with an enum" into a
        // CREATE TYPE (before the table) + CREATE TABLE.
        let s0 = Snapshot::empty();
        let m1 = Migration {
            version: 4601,
            name: "create_mig_orders".into(),
            statements: autodetect(&s0, &[&ORDER_META], Backend::Postgres).unwrap(),
            down: vec![],
        };
        assert!(
            m1.statements
                .iter()
                .any(|s| s.contains("CREATE TYPE \"mig_status\" AS ENUM")),
            "m1 creates the type: {:?}",
            m1.statements
        );

        // m2: a hand-authored ADD VALUE — proving `ALTER TYPE … ADD VALUE` applies
        // inside migrate's transaction and the label is usable after it commits.
        let m2 = Migration {
            version: 4602,
            name: "grow_mig_status".into(),
            statements: vec!["ALTER TYPE mig_status ADD VALUE IF NOT EXISTS 'escalated'".into()],
            down: vec![],
        };

        let applied = dorm_migrations::migrate(&[m1, m2])
            .await
            .expect("the enum migration chain applies");
        assert_eq!(applied, vec![4601, 4602], "both migrations ran in order");

        // The table and type are live: a row with an original label inserts, an
        // out-of-set label is refused, and the label m2 added now works.
        db::execute(
            "INSERT INTO mig_orders (id, status) VALUES (1, 'archived')",
            &[],
        )
        .await
        .expect("an original enum label inserts");
        let bad = db::execute(
            "INSERT INTO mig_orders (id, status) VALUES (2, 'bogus')",
            &[],
        )
        .await;
        assert!(
            bad.is_err(),
            "an out-of-set label is refused by the enum type"
        );
        db::execute(
            "INSERT INTO mig_orders (id, status) VALUES (3, $1::mig_status)",
            &[&Value::Text("escalated".into())],
        )
        .await
        .expect("the label ALTER TYPE ADD VALUE added is usable");

        // Re-running the chain is a no-op (ledger).
        let again = dorm_migrations::migrate(&[
            Migration {
                version: 4601,
                name: "create_mig_orders".into(),
                statements: vec![],
                down: vec![],
            },
            Migration {
                version: 4602,
                name: "grow_mig_status".into(),
                statements: vec![],
                down: vec![],
            },
        ])
        .await
        .expect("re-deploy");
        assert!(again.is_empty(), "a re-deploy applies nothing: {again:?}");

        eprintln!("ok: versioned migrate created the enum type + table and grew it with ADD VALUE");
    });
}
