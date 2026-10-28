//! Live: the additive schema `sync` (the `prisma db push` shape) against a real
//! PostgreSQL. Proves the four properties that matter: it creates a missing
//! schema, it is idempotent, it adds a column a model gained, and it recreates a
//! table that went missing — never dropping anything itself.
//!
//! Gated on `DELONIX_ORM_DSN`, like `migrate_live`. Run:
//!
//! ```bash
//! DELONIX_ORM_DSN=postgresql://user:pw@localhost/throwaway \
//!   cargo test -p dorm-models --test sync_live -- --nocapture
//! ```

use dorm_core::{db, model::Model};
use dorm_migrations as mig;
use dorm_models::{Application, Deployment, Plan, Product, Tenant};

async fn table_exists(table: &str) -> bool {
    !db::query(
        "SELECT 1 FROM information_schema.tables \
         WHERE table_schema='public' AND table_name=$1",
        &[&table],
    )
    .await
    .expect("information_schema query")
    .is_empty()
}

async fn column_exists(table: &str, column: &str) -> bool {
    !db::query(
        "SELECT 1 FROM information_schema.columns \
         WHERE table_schema='public' AND table_name=$1 AND column_name=$2",
        &[&table, &column],
    )
    .await
    .expect("information_schema query")
    .is_empty()
}

#[test]
fn sync_creates_is_idempotent_adds_columns_and_recreates_tables() {
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

        // 1. Against whatever state the database is in, sync makes it match.
        mig::sync(&models).await.expect("first sync");
        assert!(table_exists("tenants").await, "tenants created");
        assert!(table_exists("deployments").await, "deployments created");
        eprintln!("ok: sync created the schema");

        // 2. A second sync changes nothing.
        let again = mig::sync(&models).await.expect("second sync");
        assert!(
            again.is_empty(),
            "second sync should be a no-op, got {again:?}"
        );
        eprintln!("ok: sync is idempotent");

        // 3. A column that goes missing is re-added. `created_at` has a `now()`
        //    default, so the ADD COLUMN is safe whether or not the table has rows.
        db::execute("ALTER TABLE tenants DROP COLUMN IF EXISTS created_at", &[])
            .await
            .expect("drop column");
        assert!(!column_exists("tenants", "created_at").await);
        let added = mig::sync(&models).await.expect("sync adds the column");
        assert!(
            added
                .iter()
                .any(|s| s.contains("ADD COLUMN") && s.contains("created_at")),
            "expected an ADD COLUMN for created_at, got {added:?}"
        );
        assert!(
            column_exists("tenants", "created_at").await,
            "created_at re-added"
        );
        eprintln!("ok: sync added a missing column");

        // 4. A table that goes missing is recreated.
        db::execute("DROP TABLE IF EXISTS deployments CASCADE", &[])
            .await
            .expect("drop table");
        assert!(!table_exists("deployments").await);
        let recreated = mig::sync(&models).await.expect("sync recreates the table");
        assert!(
            recreated
                .iter()
                .any(|s| s.contains("CREATE TABLE") && s.contains("deployments")),
            "expected a CREATE TABLE for deployments, got {recreated:?}"
        );
        assert!(table_exists("deployments").await, "deployments recreated");
        eprintln!("ok: sync recreated a missing table");
    });
}
