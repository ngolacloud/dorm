//! Tenant-scope enforcement against a real PostgreSQL (ADR 0021).
//!
//! The acceptance proof for L1: with a tenant in scope, no surface — typed or
//! dynamic — returns another tenant's rows, even when the caller writes no tenant
//! filter at all; with no tenant in scope a `tenant_scoped` query is refused
//! fail-closed; and `with_system_scope` is the one audited way across tenants.
//!
//! ```text
//! DELONIX_ORM_TEST_DSN='host=127.0.0.1 port=5432 user=orm dbname=ormtest' \
//!   cargo test -p dorm-models --test tenant_scope_live -- --ignored --nocapture
//! ```

use dorm_core::compiler::Plan as QueryPlan;
use dorm_core::model::Model;
use dorm_core::prelude::*;
use dorm_core::queryset::fetch_value_rows;
use dorm_models::*;
use rust_decimal::Decimal;
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
            .expect("a tokio runtime")
    })
}

fn setup(dsn: &str) {
    static ONCE: std::sync::Once = std::sync::Once::new();
    const SCHEMA_LOCK: i64 = 0x0021_5c0e;
    ONCE.call_once(|| {
        std::env::set_var(dorm_core::db::DSN_ENV, dsn);
        runtime().block_on(async {
            db::atomic(|_tx| async {
                dorm_core::db::execute("SELECT pg_advisory_xact_lock($1)", &[&SCHEMA_LOCK]).await?;
                for statement in schema_sql().split(";\n") {
                    let s = statement.trim();
                    if s.is_empty() {
                        continue;
                    }
                    dorm_core::db::execute(s, &[]).await.map_err(|e| {
                        Error::Config(format!("creating the schema failed on `{s}`: {e}"))
                    })?;
                }
                Ok(())
            })
            .await
            .expect("the schema is created");
        });
    });
}

/// Create a tenant and `n` applications for it, clearing any left over. The
/// writes run under that tenant's scope — the injection path under test.
async fn seed_tenant(slug: &str, plan: &Plan, n: usize) -> Tenant {
    let tenant = Tenant::objects()
        .get_or_create(
            tenant::slug.eq(slug),
            NewTenant {
                slug: slug.into(),
                name: format!("Tenant {slug}"),
            },
        )
        .await
        .expect("tenant")
        .0;
    db::with_tenant(tenant.pk(), async {
        Application::objects()
            .filter(application::tenant.eq(tenant.pk()))
            .delete()
            .await
            .expect("clear apps");
        for i in 0..n {
            Application::objects()
                .create(NewApplication {
                    tenant: tenant.as_fk(),
                    plan: plan.as_fk(),
                    name: format!("{slug}-app-{i}"),
                })
                .await
                .expect("app");
        }
    })
    .await;
    tenant
}

#[test]
#[ignore = "needs a Postgres (DELONIX_ORM_TEST_DSN)"]
fn no_surface_crosses_tenants_and_a_missing_scope_fails_closed() {
    let Some(dsn) = dsn() else {
        eprintln!("skipped: DELONIX_ORM_TEST_DSN is not set");
        return;
    };
    setup(&dsn);
    runtime().block_on(async {
        let plan = Plan::objects()
            .get_or_create(
                plan::code.eq("scope-plan"),
                NewPlan {
                    code: "scope-plan".into(),
                    name: "Scope".into(),
                    monthly_price: Decimal::new(0, 0),
                },
            )
            .await
            .expect("plan")
            .0;

        // Two tenants: three apps for A, two for B.
        let a = seed_tenant("scope-a", &plan, 3).await;
        let b = seed_tenant("scope-b", &plan, 2).await;

        // 1) Typed surface, no explicit tenant filter: A sees only its three.
        let a_apps = db::with_tenant(a.pk(), Application::objects().all().all())
            .await
            .expect("A lists its apps");
        assert_eq!(a_apps.len(), 3, "A must see exactly its own apps");
        assert!(
            a_apps.iter().all(|app| app.tenant.id() == &a.pk()),
            "every row A sees belongs to A"
        );

        // B, likewise, sees only its two — the same query text, a different scope.
        let b_apps = db::with_tenant(b.pk(), Application::objects().all().all())
            .await
            .expect("B lists its apps");
        assert_eq!(b_apps.len(), 2, "B must see exactly its own apps");
        assert!(b_apps.iter().all(|app| app.tenant.id() == &b.pk()));

        // 2) Dynamic surface (the REPL/MCP path): same isolation, no `T`.
        let dynamic = db::with_tenant(a.pk(), async {
            fetch_value_rows(&QueryPlan::new(Application::meta())).await
        })
        .await
        .expect("dynamic rows for A");
        assert_eq!(dynamic.len(), 3, "the dynamic path is scoped too");

        // 3) Fail-closed: a tenant_scoped query with no scope is refused.
        match Application::objects().all().all().await {
            Err(Error::TenantScopeMissing(name)) => assert_eq!(name, "Application"),
            other => panic!("expected TenantScopeMissing, got {other:?}"),
        }

        // 4) The audited escape sees across tenants.
        let all = db::with_system_scope(async {
            Application::objects()
                .filter(
                    application::tenant
                        .eq(a.pk())
                        .or(application::tenant.eq(b.pk())),
                )
                .all()
                .await
        })
        .await
        .expect("system scope lists both");
        assert_eq!(all.len(), 5, "system scope sees A's and B's apps together");

        // 5) An insert under A's scope lands under A even when it names B.
        let crossed = db::with_tenant(a.pk(), async {
            Application::objects()
                .create(NewApplication {
                    tenant: b.as_fk(), // tries to write under B…
                    plan: plan.as_fk(),
                    name: "scope-a-forced".into(),
                })
                .await
        })
        .await
        .expect("the insert succeeds");
        assert_eq!(
            crossed.tenant.id(),
            &a.pk(),
            "the active tenant is forced onto the insert, overriding the row"
        );

        // Clean the extra row so the counts stay stable on re-run.
        db::with_tenant(a.pk(), async {
            Application::objects()
                .filter(application::name.eq("scope-a-forced"))
                .delete()
                .await
                .expect("cleanup");
        })
        .await;
    });
}
