//! The typed API against a real PostgreSQL.
//!
//! Skipped unless a DSN is in the environment, and marked `#[ignore]` besides —
//! the convention already used by `crates/delonix-db/tests/pg_engine_e2e.rs:15`
//! and `crates/delonix-api/tests/migracoes_e2e.rs:38`. No `testcontainers`, no
//! compose file, because this repo has neither.
//!
//! ```text
//! DELONIX_ORM_TEST_DSN='host=127.0.0.1 port=5432 user=orm dbname=ormtest' \
//!   cargo test -p dorm-models --test typed_e2e -- --ignored --nocapture
//! ```
//!
//! Every test picks its own tenant slug and filters by it, so the suite is safe
//! to run in parallel and safe to re-run against a database that already has
//! rows in it. Nothing here truncates: a test that truncates a shared table is
//! a test that fails when a colleague runs the suite at the same time.

use dorm_core::prelude::*;
use dorm_core::queryset::QuerySet;
use dorm_models::*;
use rust_decimal::Decimal;
use tokio::runtime::Runtime;

/// The DSN, or `None` to skip.
fn dsn() -> Option<String> {
    std::env::var("DELONIX_ORM_TEST_DSN")
        .ok()
        .filter(|s| !s.trim().is_empty())
}

/// The one runtime every test shares.
///
/// It has to be shared, and that is a property of the library rather than a
/// convenience here: `tokio-postgres` drives each connection from a background
/// task, so a pooled connection belongs to the runtime that created it. Give
/// each test its own runtime and the first one to finish takes its connections
/// down with it — the next test to borrow one from the global pool gets
/// `connection closed`. Which is exactly what happened before this was shared.
///
/// Multi-threaded, because `block_on` is called from several test threads at
/// once and a current-thread runtime cannot serve that.
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

/// Point the ORM at the test database and make sure the schema is there.
///
/// Serialised twice over, because `CREATE TABLE IF NOT EXISTS` is **not**
/// race-safe: two sessions that pass the existence check together both try to
/// create the table's sequence, and one gets a duplicate-key error on
/// `pg_class`. The tests run in parallel, so this is not a theoretical race —
/// it failed ten of eleven tests before the locking went in.
///
/// * `Once` serialises the threads of this process and makes the work happen
///   exactly once.
/// * `pg_advisory_xact_lock` serialises against any *other* process running the
///   same suite. It is transaction-scoped, so it is released on commit with no
///   unlock to forget.
fn setup(dsn: &str) {
    static ONCE: std::sync::Once = std::sync::Once::new();
    // A number, not a name: advisory locks are keyed by integer, and this one is
    // arbitrary but fixed so every process picks the same lock.
    const SCHEMA_LOCK: i64 = 0x0121_0ec0;

    ONCE.call_once(|| {
        std::env::set_var(dorm_core::db::DSN_ENV, dsn);
        let rt = runtime();
        rt.block_on(async {
            db::atomic(|_tx| async {
                dorm_core::db::execute("SELECT pg_advisory_xact_lock($1)", &[&SCHEMA_LOCK])
                    .await?;
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

/// A tenant and a plan to hang applications off, both unique to the caller.
async fn seed(slug: &str) -> (Tenant, Plan) {
    let tenant = Tenant::objects()
        .get_or_create(
            tenant::slug.eq(slug),
            NewTenant {
                slug: slug.to_string(),
                name: format!("Tenant {slug}"),
            },
        )
        .await
        .expect("the tenant is created or found")
        .0;

    // Start from a known state for *this tenant only*.
    //
    // Idempotence has to be built in, not hoped for: a previous run that failed
    // part-way leaves rows behind, and the next run then asserts a count that
    // includes them. Scoping the delete to the tenant is what makes this safe to
    // do while another test — or a colleague — is using the same database. A
    // `TRUNCATE` would not be.
    //
    // Deployments go with it: the foreign key is `ON DELETE CASCADE`.
    Application::objects()
        .filter(application::tenant.eq(tenant.pk()))
        .delete()
        .await
        .expect("the tenant's applications are cleared");

    let code = format!("basic-{slug}");
    let plan = Plan::objects()
        .get_or_create(
            plan::code.eq(code.clone()),
            NewPlan {
                code,
                name: "Basic".into(),
                monthly_price: Decimal::new(1999, 2),
            },
        )
        .await
        .expect("the plan is created or found")
        .0;

    (tenant, plan)
}

macro_rules! e2e {
    ($name:ident, $slug:expr, |$t:ident, $p:ident| $body:block) => {
        #[test]
        #[ignore = "needs a Postgres (DELONIX_ORM_TEST_DSN)"]
        fn $name() {
            let Some(dsn) = dsn() else {
                eprintln!("skipped: DELONIX_ORM_TEST_DSN is not set");
                return;
            };
            setup(&dsn);
            let rt = runtime();
            rt.block_on(async {
                let ($t, $p) = seed($slug).await;
                $body
            });
        }
    };
}

// ------------------------------------------------------- the acceptance script

e2e!(
    the_acceptance_criterion_from_the_request,
    "acme-accept",
    |t, p| {
        // `Application::objects().create(..)`
        let app = Application::objects()
            .create(NewApplication {
                tenant: t.as_fk(),
                plan: p.as_fk(),
                name: "api".into(),
            })
            .await
            .expect("the application is created");
        assert!(
            !app.pk().is_null(),
            "the serial primary key must come back from the INSERT"
        );

        // The exact query from the request's acceptance criteria.
        let n = Application::objects()
            .filter(application::tenant.slug().eq("acme-accept"))
            .filter(application::name.icontains("ap"))
            .count()
            .await
            .expect("the count runs");
        assert_eq!(n, 1, "one application matches slug=acme-accept and name~ap");

        // A name that does not match must not be counted.
        let none = Application::objects()
            .filter(application::tenant.slug().eq("acme-accept"))
            .filter(application::name.icontains("zz"))
            .count()
            .await
            .expect("the count runs");
        assert_eq!(none, 0);

        app.delete().await.expect("cleanup");
    }
);

e2e!(
    an_f_expression_increments_atomically,
    "acme-fexpr",
    |t, p| {
        let app = Application::objects()
            .create(NewApplication {
                tenant: t.as_fk(),
                plan: p.as_fk(),
                name: "worker".into(),
            })
            .await
            .expect("the application is created");

        let d = Deployment::objects()
            .create(NewDeployment {
                app: app.as_fk(),
                replicas: 1,
                status: "running".into(),
                config: serde_json::json!({"strategy": "rolling"}),
            })
            .await
            .expect("the deployment is created");

        // `update(#{ replicas: F("replicas") + 1 })`
        let touched = Deployment::objects()
            .filter(deployment::app.eq(app.pk()))
            .update(vec![deployment::replicas.set(F::col("replicas") + 1i64)])
            .await
            .expect("the update runs");
        assert_eq!(touched, 1);

        let mut d = d;
        d.refresh().await.expect("re-read");
        assert_eq!(d.replicas, 2, "the increment happened in the database");

        // `auto_now` is refreshed even though the update never mentioned it.
        assert!(
            d.updated_at >= d.created_at,
            "updated_at should have been bumped by the write"
        );

        d.delete().await.expect("cleanup");
        app.delete().await.expect("cleanup");
    }
);

e2e!(
    annotate_counts_a_reverse_relation_and_filters_it,
    "acme-annot",
    |t, p| {
        for name in ["one", "two"] {
            Application::objects()
                .create(NewApplication {
                    tenant: t.as_fk(),
                    plan: p.as_fk(),
                    name: name.into(),
                })
                .await
                .expect("the application is created");
        }

        // `Tenant.objects.annotate(#{n: Count("apps")}).filter(#{n__gte: 1}).values(["slug","n"])`
        let rows = Tenant::objects()
            .annotate(Aggregate::new("n", AggKind::Count, "apps"))
            .filter(tenant::slug.eq("acme-annot"))
            .filter(Q::new("n__gte", 1i64))
            .values(&["slug", "n"])
            .rows()
            .await
            .expect("the annotated query runs");

        assert_eq!(rows.len(), 1, "one tenant matches");
        assert_eq!(rows[0]["slug"], Value::Text("acme-annot".into()));
        assert_eq!(
            rows[0]["n"],
            Value::I64(2),
            "the tenant has exactly the two applications just created"
        );

        // A threshold above the real count must exclude it — proving the
        // condition really is a HAVING and not a no-op.
        let empty = Tenant::objects()
            .annotate(Aggregate::new("n", AggKind::Count, "apps"))
            .filter(tenant::slug.eq("acme-annot"))
            .filter(Q::new("n__gte", 3i64))
            .values(&["slug", "n"])
            .rows()
            .await
            .expect("the annotated query runs");
        assert!(empty.is_empty(), "n__gte 3 must exclude a tenant with 2");

        Application::objects()
            .filter(application::tenant.eq(t.pk()))
            .delete()
            .await
            .expect("cleanup");
    }
);

// ------------------------------------------------------------- transactions

e2e!(
    an_atomic_block_that_fails_leaves_no_trace,
    "acme-rollback",
    |t, p| {
        let app = Application::objects()
            .create(NewApplication {
                tenant: t.as_fk(),
                plan: p.as_fk(),
                name: "api".into(),
            })
            .await
            .expect("the application is created");

        // The REPL's `atomic(|| { app.delete(); throw "rollback"; })`.
        let outcome: Result<()> = db::atomic(|_tx| async {
            Application::objects()
                .filter(application::name.eq("api"))
                .filter(application::tenant.eq(t.pk()))
                .delete()
                .await?;
            Err(Error::RolledBack("rollback".into()))
        })
        .await;
        assert!(outcome.is_err(), "the block must report the failure");

        // `Application.objects.filter(#{name: "api"}).exists()` → true
        let still_there = Application::objects()
            .filter(application::name.eq("api"))
            .filter(application::tenant.eq(t.pk()))
            .exists()
            .await
            .expect("the existence check runs");
        assert!(
            still_there,
            "the delete inside the failed block must have been rolled back"
        );

        app.delete().await.expect("cleanup");
    }
);

e2e!(
    a_savepoint_rolls_back_without_losing_the_outer_work,
    "acme-savepoint",
    |t, p| {
        // Taken before the closures: the `async move` blocks would otherwise
        // consume the models, and the assertions afterwards still need the key.
        let tenant_pk = t.pk();
        let (tenant_fk, plan_fk) = (t.as_fk(), p.as_fk());

        let outcome: Result<()> = db::atomic(|outer| async move {
            assert_eq!(outer.depth(), 1);
            assert!(!outer.is_savepoint());

            // Committed with the outer transaction.
            Application::objects()
                .create(NewApplication {
                    tenant: tenant_fk.clone(),
                    plan: plan_fk.clone(),
                    name: "kept".into(),
                })
                .await?;

            // Undone by its own savepoint, leaving the outer work intact.
            let inner: Result<()> = db::atomic(|sp| async move {
                assert_eq!(sp.depth(), 2);
                assert!(sp.is_savepoint());
                Application::objects()
                    .create(NewApplication {
                        tenant: tenant_fk,
                        plan: plan_fk,
                        name: "discarded".into(),
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

        let names: Vec<String> = Application::objects()
            .filter(application::tenant.eq(tenant_pk.clone()))
            .all()
            .await
            .expect("the read runs")
            .into_iter()
            .map(|a| a.name)
            .collect();
        assert!(
            names.contains(&"kept".to_string()),
            "the outer insert committed: {names:?}"
        );
        assert!(
            !names.contains(&"discarded".to_string()),
            "the savepoint's insert was undone: {names:?}"
        );

        Application::objects()
            .filter(application::tenant.eq(tenant_pk))
            .delete()
            .await
            .expect("cleanup");
    }
);

// ------------------------------------------------------------- the rest of it

e2e!(get_distinguishes_none_from_several, "acme-get", |t, p| {
    // None.
    let err = Application::objects()
        .filter(application::tenant.eq(t.pk()))
        .get()
        .await
        .expect_err("there is no application yet");
    assert!(
        matches!(err, Error::DoesNotExist { .. }),
        "expected DoesNotExist, got {err}"
    );

    for name in ["a", "b"] {
        Application::objects()
            .create(NewApplication {
                tenant: t.as_fk(),
                plan: p.as_fk(),
                name: name.into(),
            })
            .await
            .expect("the application is created");
    }

    // Several — and the count is reported, because 2 and 4000 call for
    // different reactions.
    let err = Application::objects()
        .filter(application::tenant.eq(t.pk()))
        .get()
        .await
        .expect_err("there are two");
    match err {
        Error::MultipleObjectsReturned { count, .. } => assert_eq!(count, 2),
        other => panic!("expected MultipleObjectsReturned, got {other}"),
    }

    // Exactly one.
    let one = Application::objects()
        .filter(application::tenant.eq(t.pk()))
        .filter(application::name.eq("a"))
        .get()
        .await
        .expect("exactly one matches");
    assert_eq!(one.name, "a");

    Application::objects()
        .filter(application::tenant.eq(t.pk()))
        .delete()
        .await
        .expect("cleanup");
});

e2e!(
    select_related_loads_the_parent_in_one_query,
    "acme-related",
    |t, p| {
        Application::objects()
            .create(NewApplication {
                tenant: t.as_fk(),
                plan: p.as_fk(),
                name: "api".into(),
            })
            .await
            .expect("the application is created");

        let apps = Application::objects()
            .filter(application::tenant.eq(t.pk()))
            .select_related("tenant")
            .select_related("plan")
            .all()
            .await
            .expect("the join runs");

        assert_eq!(apps.len(), 1);
        let app = &apps[0];
        let loaded_tenant = app
            .tenant
            .get()
            .expect("select_related attached the tenant row");
        assert_eq!(loaded_tenant.slug, "acme-related");
        let loaded_plan = app.plan.get().expect("and the plan row");
        assert_eq!(loaded_plan.monthly_price, Decimal::new(1999, 2));

        Application::objects()
            .filter(application::tenant.eq(t.pk()))
            .delete()
            .await
            .expect("cleanup");
    }
);

e2e!(ordering_first_and_last_agree, "acme-order", |t, p| {
    for name in ["alpha", "beta", "gamma"] {
        Application::objects()
            .create(NewApplication {
                tenant: t.as_fk(),
                plan: p.as_fk(),
                name: name.into(),
            })
            .await
            .expect("the application is created");
    }

    let base = || {
        QuerySet::<Application>::new()
            .filter(application::tenant.eq(t.pk()))
            .order_by_str(&["name"])
    };

    let first = base().first().await.expect("first runs").expect("a row");
    let last = base().last().await.expect("last runs").expect("a row");
    assert_eq!(first.name, "alpha");
    assert_eq!(
        last.name, "gamma",
        "last must reverse the ordering, not the page"
    );

    // `limit`/`offset` page through the same ordering.
    let middle = base()
        .offset(1)
        .limit(1)
        .all()
        .await
        .expect("the page runs");
    assert_eq!(middle.len(), 1);
    assert_eq!(middle[0].name, "beta");

    Application::objects()
        .filter(application::tenant.eq(t.pk()))
        .delete()
        .await
        .expect("cleanup");
});

e2e!(
    a_decimal_survives_the_round_trip,
    "acme-decimal",
    |_t, p| {
        // The reason `Decimal` is in the closed set instead of `f64`: 19.99 must
        // come back as 19.99, not 19.989999999999998.
        let mut p = p;
        p.refresh().await.expect("re-read the plan");
        assert_eq!(p.monthly_price, Decimal::new(1999, 2));
        assert_eq!(p.monthly_price.to_string(), "19.99");
    }
);

e2e!(bulk_create_inserts_in_one_statement, "acme-bulk", |t, p| {
    let rows: Vec<NewApplication> = (0..5)
        .map(|i| NewApplication {
            tenant: t.as_fk(),
            plan: p.as_fk(),
            name: format!("svc-{i}"),
        })
        .collect();

    let created = Application::objects()
        .bulk_create(rows)
        .await
        .expect("the bulk insert runs");
    assert_eq!(created.len(), 5);
    assert!(
        created.iter().all(|a| !a.pk().is_null()),
        "every row's generated key comes back"
    );

    let n = Application::objects()
        .filter(application::tenant.eq(t.pk()))
        .count()
        .await
        .expect("the count runs");
    assert_eq!(n, 5);

    Application::objects()
        .filter(application::tenant.eq(t.pk()))
        .delete()
        .await
        .expect("cleanup");
});

e2e!(
    a_read_only_session_refuses_a_write_before_it_reaches_sql,
    "acme-readonly",
    |t, p| {
        db::set_read_only(true);
        let err = Application::objects()
            .create(NewApplication {
                tenant: t.as_fk(),
                plan: p.as_fk(),
                name: "nope".into(),
            })
            .await
            .expect_err("a read-only session must refuse this");
        // The refusal names the operation rather than surfacing a PostgreSQL
        // error about a read-only transaction.
        assert!(
            matches!(err, Error::ReadOnly("create")),
            "expected ReadOnly(\"create\"), got {err}"
        );
        db::set_read_only(false);

        // And the session still works once the flag is cleared.
        let app = Application::objects()
            .create(NewApplication {
                tenant: t.as_fk(),
                plan: p.as_fk(),
                name: "yes".into(),
            })
            .await
            .expect("the write is allowed again");
        app.delete().await.expect("cleanup");
    }
);
