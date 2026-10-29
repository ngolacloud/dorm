//! Live proof of the production observability bricks (ADR 0008): that a slow
//! statement emits a `tracing::warn!` on the `dorm::db` target carrying the SQL
//! (and no parameter value), and that `db::pool_stats()` reports the live pool.
//!
//! The witness is a capturing `tracing` subscriber installed only for this test:
//! a `Layer` that records every event's level, target, message and fields into a
//! shared `Vec`. We make PostgreSQL take a known, long time with
//! `SELECT pg_sleep(0.2)` under `DELONIX_ORM_SLOW_QUERY_MS=50`, then assert a
//! `warn` for it exists — and that its `sql` field holds the statement text, not
//! the `0.2` literal, which is bound as `$1` and must never reach the log.
//!
//! One test owns the whole process so the one-shot config reads (the pool, and
//! the once-resolved slow-query threshold) see the values this test sets, and so
//! nothing races on the shared process environment.
//!
//! Gated on `DELONIX_ORM_DSN`, like the other live suites:
//!
//! ```bash
//! DELONIX_ORM_DSN="host=/var/run/postgresql user=$USER dbname=dorm_obs" \
//!   cargo test -p dorm-core --test observability_live -- --nocapture
//! ```

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use dorm_core::db;
use tracing::field::{Field, Visit};
use tracing::Level;
use tracing_subscriber::layer::{Context, SubscriberExt};
use tracing_subscriber::Layer;

/// One captured `tracing` event, flattened to the pieces the assertions read.
#[derive(Clone, Debug)]
struct Captured {
    level: Level,
    target: String,
    message: String,
    fields: BTreeMap<String, String>,
}

/// A `Layer` that pushes every event into a shared buffer. Test-only: a library
/// must never install a subscriber, but a test may, to observe what it emits.
#[derive(Clone, Default)]
struct CaptureLayer {
    events: Arc<Mutex<Vec<Captured>>>,
}

/// Collects an event's fields into strings. The `message` is the special field
/// `tracing` uses for the macro's format string; everything else is a named
/// field such as `sql`, `op` or `duration_ms`.
#[derive(Default)]
struct FieldGrab {
    message: String,
    fields: BTreeMap<String, String>,
}

impl Visit for FieldGrab {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        let rendered = format!("{value:?}");
        if field.name() == "message" {
            self.message = rendered;
        } else {
            self.fields.insert(field.name().to_string(), rendered);
        }
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        // `%sql` arrives here as a plain string — keep it unquoted so a substring
        // check reads naturally.
        if field.name() == "message" {
            self.message = value.to_string();
        } else {
            self.fields
                .insert(field.name().to_string(), value.to_string());
        }
    }
}

impl<S: tracing::Subscriber> Layer<S> for CaptureLayer {
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
        let mut grab = FieldGrab::default();
        event.record(&mut grab);
        let meta = event.metadata();
        self.events.lock().unwrap().push(Captured {
            level: *meta.level(),
            target: meta.target().to_string(),
            message: grab.message,
            fields: grab.fields,
        });
    }
}

#[test]
fn a_slow_statement_warns_with_sql_but_no_parameter_value_and_pool_stats_reports() {
    let Ok(dsn) = std::env::var("DELONIX_ORM_DSN").or_else(|_| std::env::var("PAAS_DATABASE_URL"))
    else {
        eprintln!("skipped: set DELONIX_ORM_DSN to a throwaway Postgres to run this");
        return;
    };
    // Set before the first `db::*` call: the pool and the slow-query threshold
    // each read the environment once, on first use.
    std::env::set_var(db::DSN_ENV, &dsn);
    std::env::set_var(db::SLOW_QUERY_MS_ENV, "50");

    let capture = CaptureLayer::default();
    let subscriber = tracing_subscriber::registry().with(capture.clone());
    let _guard = tracing::subscriber::set_default(subscriber);

    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    rt.block_on(async {
        // 200 ms against a 50 ms threshold: comfortably slow, and the sleep is a
        // bound parameter ($1) so the value must NOT appear in the log.
        db::query("SELECT pg_sleep($1)", &[&0.2f64])
            .await
            .expect("pg_sleep query");

        // pool_stats must report a live pool after a real checkout.
        let stats = db::pool_stats().expect("pool stats");
        assert!(
            stats.max_size > 0,
            "pool has a configured ceiling: {stats:?}"
        );
        assert!(
            stats.size >= 1,
            "at least one connection exists after a query: {stats:?}"
        );
    });

    let events = capture.events.lock().unwrap().clone();

    // The per-statement debug event fired on `dorm::db`.
    assert!(
        events
            .iter()
            .any(|e| e.target == "dorm::db" && e.message == "statement finished"),
        "expected a per-statement debug event; got {events:#?}"
    );

    // The slow-query warning fired, at WARN, on `dorm::db`, naming the SQL.
    let slow = events
        .iter()
        .find(|e| e.target == "dorm::db" && e.message == "slow query")
        .unwrap_or_else(|| panic!("expected a slow-query warn; got {events:#?}"));
    assert_eq!(slow.level, Level::WARN, "slow query must be a warning");
    let sql = slow.fields.get("sql").expect("slow event carries sql");
    assert!(sql.contains("pg_sleep"), "sql text is logged: {sql}");
    assert!(
        sql.contains("$1"),
        "the placeholder is logged, not the value: {sql}"
    );

    // THE security assertion: the bound value (0.2) must never reach any log
    // line. We bound it as a float, so neither "0.2" nor "0.200" may appear in
    // any captured field or message.
    for e in &events {
        assert!(
            !e.message.contains("0.2"),
            "a parameter value leaked into a log message: {:?}",
            e
        );
        for (name, value) in &e.fields {
            assert!(
                !value.contains("0.2"),
                "a parameter value leaked into field `{name}`: {value}"
            );
        }
    }
}
