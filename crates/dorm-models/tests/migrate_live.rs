//! Live: apply the generated schema (the generic example models) to a real
//! PostgreSQL, and prove it **creates** the schema and is **idempotent** (a
//! second run changes nothing — the "adopt an existing database" path, since
//! every statement is `IF NOT EXISTS`).
//!
//! Gated on `DELONIX_ORM_DSN`: without a throwaway database it is skipped. Run:
//!
//! ```bash
//! DELONIX_ORM_DSN=postgresql://user:pw@localhost/throwaway \
//!   cargo test -p dorm-models --test migrate_live -- --nocapture
//! ```

use dorm_core::{db, model::Model};
use dorm_migrations as mig;
use dorm_models::{Application, Deployment, Plan, Product, Tenant};

#[test]
fn migrate_creates_the_schema_then_is_idempotent() {
    let Ok(dsn) = std::env::var("DELONIX_ORM_DSN") else {
        eprintln!("skipped: set DELONIX_ORM_DSN to a throwaway Postgres to run this");
        return;
    };
    std::env::set_var(db::DSN_ENV, &dsn);

    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    rt.block_on(async {
        let models = [
            <Tenant as Model>::meta(),
            <Plan as Model>::meta(),
            <Application as Model>::meta(),
            <Deployment as Model>::meta(),
            <Product as Model>::meta(),
        ];
        let initial = mig::initial(&models).expect("build the initial migration");

        let applied = mig::migrate(std::slice::from_ref(&initial))
            .await
            .expect("first migrate");
        assert_eq!(applied, vec![1], "the first run applies version 1");

        let again = mig::migrate(std::slice::from_ref(&initial))
            .await
            .expect("second migrate");
        assert!(
            again.is_empty(),
            "the second run is a no-op (idempotent/adopt)"
        );

        for t in [
            "tenants",
            "plans",
            "applications",
            "deployments",
            "products",
            "orm_schema_migrations",
        ] {
            let rows = db::query(
                "SELECT 1 FROM information_schema.tables \
                 WHERE table_schema = 'public' AND table_name = $1",
                &[&t],
            )
            .await
            .expect("query information_schema");
            assert_eq!(rows.len(), 1, "table `{t}` exists after migrate");
            eprintln!("ok: table `{t}` present");
        }
    });
}
