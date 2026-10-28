//! The typed `QuerySet` against SQLite — the proof of [ADR 0003].
//!
//! The same `Tenant::objects()…` API the PostgreSQL e2e suite exercises, run
//! here against an in-memory SQLite chosen purely by the DSN (`sqlite::memory:`).
//! Nothing in the query code is backend-specific: the dialect, the placeholders,
//! the row decoding and the connection all follow from the DSN, which is the
//! whole point of the backend-generic `db`.
//!
//! Gated behind the `sqlite` feature so the default build — the one `make check`
//! tests — stays PostgreSQL-only and pulls neither `rusqlite` nor a bundled
//! SQLite:
//!
//! ```text
//! cargo test -p dorm-models --features sqlite --test sqlite_e2e
//! ```
//!
//! One shared runtime and a serial run (`CONN` is a single process-global
//! connection, and `sqlite::memory:` is one database), so the four phases below
//! build on each other in order within a single `#[test]`.
#![cfg(feature = "sqlite")]

use dorm_core::prelude::*;
use dorm_migrations::ddl::{schema_for_on, Backend};
use dorm_models::{tenant, NewTenant, Tenant};
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

#[test]
fn the_typed_queryset_runs_end_to_end_on_sqlite() {
    // The backend is the DSN's: no code path here names SQLite.
    std::env::set_var("DELONIX_ORM_DSN", "sqlite::memory:");

    runtime().block_on(async {
        // Stand the schema up through the backend-generic DDL path.
        let ddl = schema_for_on(&[<Tenant as Model>::meta()], Backend::Sqlite)
            .expect("the SQLite schema compiles");
        db::run_ddl(&ddl).await.expect("create the schema");

        // create() — compiled for SQLite, INSERT … RETURNING * decoded back into
        // the struct, serial key and auto_now_add stamp filled in server-side.
        let acme = Tenant::objects()
            .create(NewTenant {
                slug: "acme".into(),
                name: "ACME Lda".into(),
            })
            .await
            .expect("insert acme");
        assert_eq!(acme.id, 1, "the serial primary key came back");
        assert_eq!(acme.slug, "acme");

        let globex = Tenant::objects()
            .create(NewTenant {
                slug: "globex".into(),
                name: "Globex".into(),
            })
            .await
            .expect("insert globex");
        assert_eq!(globex.id, 2);

        // all() — the headline terminal. Two rows, decoded into typed structs,
        // honouring the model's declared `-created_at` ordering.
        let all = Tenant::objects().all().all().await.expect("select all");
        assert_eq!(all.len(), 2, "both rows came back");
        let slugs: Vec<&str> = all.iter().map(|t| t.slug.as_str()).collect();
        assert!(
            slugs.contains(&"acme") && slugs.contains(&"globex"),
            "{slugs:?}"
        );

        // count()
        assert_eq!(
            Tenant::objects().all().count().await.expect("count"),
            2,
            "count sees both rows"
        );

        // get() by a typed filter.
        let found = Tenant::objects()
            .get(tenant::slug.eq("acme"))
            .await
            .expect("get acme");
        assert_eq!(found.id, acme.id);

        // update() — a filtered write, returning the number of rows touched.
        let touched = Tenant::objects()
            .filter(tenant::slug.eq("globex"))
            .update(vec![Assignment::new(
                "name",
                F::lit(Value::Text("Globex Corp".into())),
            )])
            .await
            .expect("update globex");
        assert_eq!(touched, 1, "one row updated");
        let renamed = Tenant::objects()
            .get(tenant::slug.eq("globex"))
            .await
            .expect("re-get globex");
        assert_eq!(renamed.name, "Globex Corp", "the update stuck");

        // delete() — a filtered delete, then a count to prove it.
        let deleted = Tenant::objects()
            .filter(tenant::slug.eq("acme"))
            .delete()
            .await
            .expect("delete acme");
        assert_eq!(deleted, 1, "one row deleted");
        assert_eq!(
            Tenant::objects()
                .all()
                .count()
                .await
                .expect("count after delete"),
            1,
            "only globex remains"
        );

        // atomic() — a failing block rolls its write back, exactly as on
        // PostgreSQL. The create inside the block hits the connection, then the
        // Err unwinds it: counting the slug afterwards must find nothing.
        let outcome: Result<()> = db::atomic(|_tx| async {
            Tenant::objects()
                .create(NewTenant {
                    slug: "rollback-me".into(),
                    name: "Doomed".into(),
                })
                .await?;
            Err(Error::RolledBack("rollback".into()))
        })
        .await;
        assert!(outcome.is_err(), "the block must report the failure");
        assert_eq!(
            Tenant::objects()
                .filter(tenant::slug.eq("rollback-me"))
                .count()
                .await
                .expect("count the rolled-back slug"),
            0,
            "the insert inside the failed atomic must have been rolled back"
        );

        // atomic() — a block that succeeds commits its write.
        db::atomic(|_tx| async {
            Tenant::objects()
                .create(NewTenant {
                    slug: "committed".into(),
                    name: "Kept".into(),
                })
                .await?;
            Ok::<(), Error>(())
        })
        .await
        .expect("the atomic commits");
        assert_eq!(
            Tenant::objects()
                .filter(tenant::slug.eq("committed"))
                .count()
                .await
                .expect("count the committed slug"),
            1,
            "the insert inside the ok atomic must have committed"
        );

        // A savepoint rolls back without losing the outer transaction's work,
        // the nested-atomic guarantee the REPL relies on.
        let outcome: Result<()> = db::atomic(|outer| async move {
            assert_eq!(outer.depth(), 1);
            assert!(!outer.is_savepoint());
            Tenant::objects()
                .create(NewTenant {
                    slug: "outer-kept".into(),
                    name: "Outer".into(),
                })
                .await?;

            let inner: Result<()> = db::atomic(|sp| async move {
                assert_eq!(sp.depth(), 2);
                assert!(sp.is_savepoint());
                Tenant::objects()
                    .create(NewTenant {
                        slug: "inner-discarded".into(),
                        name: "Inner".into(),
                    })
                    .await?;
                Err(Error::RolledBack("inner".into()))
            })
            .await;
            assert!(inner.is_err(), "the savepoint must report its failure");
            Ok(())
        })
        .await;
        assert!(outcome.is_ok(), "the outer transaction must still commit");
        assert_eq!(
            Tenant::objects()
                .filter(tenant::slug.eq("outer-kept"))
                .count()
                .await
                .expect("count the outer slug"),
            1,
            "the outer insert committed"
        );
        assert_eq!(
            Tenant::objects()
                .filter(tenant::slug.eq("inner-discarded"))
                .count()
                .await
                .expect("count the savepoint slug"),
            0,
            "the savepoint's insert was undone"
        );
    });
}
