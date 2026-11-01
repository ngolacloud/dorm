//! `prefetch` against a real PostgreSQL (ADR 0013): loading a parent's children
//! for a whole batch costs **one** extra query, not one per parent.
//!
//! ```text
//! DELONIX_ORM_TEST_DSN='host=127.0.0.1 port=55432 user=dorm password=dorm dbname=dorm_<fresh>' \
//!   cargo test -p dorm-models --test prefetch_live -- --ignored --nocapture
//! ```

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use dorm_core::prelude::*;
use dorm_models::*;
use rust_decimal::Decimal;
use tokio::runtime::Runtime;
use tracing_subscriber::layer::{Context, SubscriberExt};
use tracing_subscriber::Layer;

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
    std::env::set_var(dorm_core::db::DSN_ENV, dsn);
    runtime().block_on(async {
        for statement in schema_sql().split(";\n") {
            let s = statement.trim();
            if !s.is_empty() {
                dorm_core::db::execute(s, &[]).await.expect("schema");
            }
        }
    });
}

/// Counts the statements DORM runs, by watching the `dorm::db` tracing target —
/// one event per statement (ADR 0008). The witness for "one query, not N".
#[derive(Clone, Default)]
struct StatementCounter(Arc<AtomicUsize>);

impl<S: tracing::Subscriber> Layer<S> for StatementCounter {
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
        if event.metadata().target() == "dorm::db" {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
}

#[test]
#[ignore = "needs a Postgres (DELONIX_ORM_TEST_DSN)"]
fn prefetch_loads_all_children_in_one_query() {
    let Some(dsn) = dsn() else {
        eprintln!("skipped: DELONIX_ORM_TEST_DSN is not set");
        return;
    };
    setup(&dsn);

    let counter = StatementCounter::default();
    let subscriber = tracing_subscriber::registry().with(counter.clone());
    let _guard = tracing::subscriber::set_default(subscriber);

    runtime().block_on(async {
        // Three tenants, a plan, and a known number of applications each. Slugs are
        // unique to this test so it is parallel- and rerun-safe.
        let plan = Plan::objects()
            .get_or_create(
                plan::code.eq("prefetch-plan"),
                NewPlan {
                    code: "prefetch-plan".into(),
                    name: "Prefetch".into(),
                    monthly_price: Decimal::new(0, 0),
                },
            )
            .await
            .expect("plan")
            .0;

        let counts = [("prefetch-a", 3usize), ("prefetch-b", 1), ("prefetch-c", 0)];
        let mut tenants = Vec::new();
        for (slug, n) in counts {
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
            // Start clean for this tenant.
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
            tenants.push(tenant);
        }

        // Count only the prefetch's own statements.
        counter.0.store(0, Ordering::SeqCst);
        let apps = prefetch::<Tenant, Application>(&tenants, "apps")
            .await
            .expect("prefetch runs");
        let queries = counter.0.load(Ordering::SeqCst);

        // One query for the whole batch — this is the N+1 kill. Three parents, not
        // three queries.
        assert_eq!(
            queries, 1,
            "prefetch must issue exactly one query, got {queries}"
        );

        // And the grouping is correct: each tenant gets exactly its applications.
        assert_eq!(apps.of(&tenants[0]).len(), 3, "tenant a has 3 apps");
        assert_eq!(apps.of(&tenants[1]).len(), 1, "tenant b has 1 app");
        assert_eq!(apps.of(&tenants[2]).len(), 0, "tenant c has none");
        assert_eq!(apps.len(), 4, "four applications in total");

        // Every child really belongs to the tenant it was filed under.
        for app in apps.of(&tenants[0]) {
            assert_eq!(app.tenant.id(), &tenants[0].pk());
        }
    });
}
