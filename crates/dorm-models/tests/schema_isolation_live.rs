//! Multi-schema isolation against a real PostgreSQL (ADR 0028 / L11).
//!
//! Two handles bound to two schemas run the **same** model and each sees only its
//! own schema's rows — a module cannot reach another's schema through the ORM.
//!
//! ```text
//! DELONIX_ORM_TEST_DSN='host=127.0.0.1 port=5432 user=orm dbname=ormtest' \
//!   cargo test -p dorm-models --test schema_isolation_live -- --ignored --nocapture
//! ```

use std::sync::Arc;

use dorm_core::db::Database;
use dorm_core::prelude::*;
use dorm_derive::Model;
use tokio::runtime::Runtime;

/// One model, resolved to whichever schema the active handle's search_path names.
#[derive(Model, Debug, Clone)]
#[model(table = "l11_widget", ordering = "id")]
struct Widget {
    #[field(primary_key)]
    id: i64,
    #[field(max_length = 40)]
    label: String,
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

#[test]
#[ignore = "needs a Postgres (DELONIX_ORM_TEST_DSN)"]
fn two_schema_bound_handles_cannot_see_each_others_tables() {
    let Some(dsn) = dsn() else {
        eprintln!("skipped: DELONIX_ORM_TEST_DSN is not set");
        return;
    };
    std::env::set_var(dorm_core::db::DSN_ENV, &dsn);

    runtime().block_on(async {
        // Build the two schemas and a same-named table in each, seeded differently.
        for schema in ["mod_a", "mod_b"] {
            dorm_core::db::execute(&format!("CREATE SCHEMA IF NOT EXISTS {schema}"), &[])
                .await
                .expect("schema");
            dorm_core::db::execute(
                &format!(
                    "CREATE TABLE IF NOT EXISTS {schema}.l11_widget \
                     (id BIGINT PRIMARY KEY, label TEXT NOT NULL)"
                ),
                &[],
            )
            .await
            .expect("table");
            dorm_core::db::execute(&format!("TRUNCATE {schema}.l11_widget"), &[])
                .await
                .expect("truncate");
        }
        dorm_core::db::execute(
            "INSERT INTO mod_a.l11_widget (id, label) VALUES (1, 'a-one'), (2, 'a-two')",
            &[],
        )
        .await
        .expect("seed a");
        dorm_core::db::execute(
            "INSERT INTO mod_b.l11_widget (id, label) VALUES (10, 'b-ten')",
            &[],
        )
        .await
        .expect("seed b");

        // One handle per module, each bound to its schema.
        let db_a = Arc::new(Database::connect_schema(&dsn, None, false, "mod_a").unwrap());
        let db_b = Arc::new(Database::connect_schema(&dsn, None, false, "mod_b").unwrap());

        // The same model, run under each handle, resolves to that handle's schema.
        let a = db::with_session(db_a.clone(), Widget::objects().all().all())
            .await
            .expect("list under mod_a");
        assert_eq!(a.len(), 2, "mod_a sees only its two widgets");
        assert!(
            a.iter().all(|w| w.label.starts_with("a-")),
            "every row under mod_a is mod_a's: {a:?}"
        );

        let b = db::with_session(db_b.clone(), Widget::objects().all().all())
            .await
            .expect("list under mod_b");
        assert_eq!(b.len(), 1, "mod_b sees only its one widget");
        assert_eq!(b[0].label, "b-ten");

        // The decisive isolation check: mod_a's id 10 does not exist for mod_a,
        // even though mod_b has a widget 10 — the schemas do not leak.
        let crossed = db::with_session(
            db_a.clone(),
            Widget::objects().filter(widget::id.eq(10)).all(),
        )
        .await
        .expect("query under mod_a");
        assert!(crossed.is_empty(), "mod_a cannot see mod_b's row 10");

        // An invalid schema name is refused before a connection is made.
        assert!(Database::connect_schema(&dsn, None, false, "evil; DROP SCHEMA mod_a").is_err());
    });
}
