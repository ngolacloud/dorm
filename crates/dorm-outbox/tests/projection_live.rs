//! A read model projected from the outbox event log (ADR 0035 / L12).
//!
//! The CQRS read side built on the generic outbox (L6/ADR 0026): a projector
//! drains domain events and maintains a denormalised read-model table — no second
//! change-capture mechanism. The property that matters is **rebuildability**: the
//! read model is a function of the log, so truncating it and replaying from the
//! start reconstructs it exactly.
//!
//! ```text
//! DELONIX_ORM_TEST_DSN='host=127.0.0.1 port=5432 user=orm dbname=ormtest' \
//!   cargo test -p dorm-outbox --test projection_live -- --ignored --nocapture
//! ```

use dorm_core::db;
use dorm_core::error::Result;
use dorm_core::value::Value;
use dorm_outbox::{publish, relay, Event, RelayConfig};
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

/// The projection handler: a 1:1, idempotent upsert of an order into the read
/// model, keyed by `order_id` — so replaying the log is safe.
async fn project(ev: Event) -> Result<()> {
    let order_id = ev.payload["order_id"].as_i64().unwrap();
    let customer = ev.payload["customer"].as_str().unwrap_or("").to_string();
    let status = ev.payload["status"].as_str().unwrap_or("").to_string();
    db::run(
        "INSERT INTO rm_order (order_id, customer, status) VALUES ($1, $2, $3) \
         ON CONFLICT (order_id) DO UPDATE SET customer = EXCLUDED.customer, status = EXCLUDED.status",
        &[Value::I64(order_id), Value::Text(customer), Value::Text(status)],
    )
    .await?;
    Ok(())
}

async fn status_of(order_id: i64) -> Option<String> {
    let rows = db::sql(
        "SELECT status FROM rm_order WHERE order_id = $1",
        &[Value::I64(order_id)],
    )
    .await
    .unwrap();
    rows.first().map(|r| r.try_get::<String>(0).unwrap())
}

async fn count() -> i64 {
    db::sql("SELECT COUNT(*)::bigint FROM rm_order", &[])
        .await
        .unwrap()[0]
        .try_get::<i64>(0)
        .unwrap()
}

#[test]
#[ignore = "needs a Postgres (DELONIX_ORM_TEST_DSN)"]
fn a_read_model_is_projected_from_events_and_rebuilds_from_the_log() {
    let Some(dsn) = dsn() else {
        eprintln!("skipped: DELONIX_ORM_TEST_DSN is not set");
        return;
    };
    std::env::set_var(dorm_core::db::DSN_ENV, &dsn);

    runtime().block_on(async {
        for stmt in dorm_outbox::migrations() {
            db::execute(&stmt, &[]).await.expect("outbox schema");
        }
        db::execute(
            "CREATE TABLE IF NOT EXISTS rm_order (order_id BIGINT PRIMARY KEY, customer TEXT, status TEXT)",
            &[],
        )
        .await
        .expect("read model table");
        for t in ["dorm_outbox", "dorm_outbox_cursor", "dorm_inbox", "rm_order"] {
            db::execute(&format!("TRUNCATE {t}"), &[]).await.expect("truncate");
        }

        // The event log: three orders placed, then one of them changes status.
        db::atomic(|_tx| async {
            publish("orders", &serde_json::json!({"order_id": 1, "customer": "acme", "status": "placed"})).await?;
            publish("orders", &serde_json::json!({"order_id": 2, "customer": "acme", "status": "placed"})).await?;
            publish("orders", &serde_json::json!({"order_id": 3, "customer": "globex", "status": "placed"})).await?;
            publish("orders", &serde_json::json!({"order_id": 1, "customer": "acme", "status": "shipped"})).await
        })
        .await
        .expect("append to the log");

        // Project the log into the read model.
        relay("rm-orders", "orders", RelayConfig::default(), project)
            .await
            .expect("project");
        assert_eq!(count().await, 3, "three distinct orders in the read model");
        assert_eq!(status_of(1).await.as_deref(), Some("shipped"), "order 1's latest state");
        assert_eq!(status_of(3).await.as_deref(), Some("placed"));

        // Rebuild: drop the read model and the projector's cursor, then replay the
        // whole log — the read model is a pure function of the log, so it comes
        // back identical.
        db::execute("TRUNCATE rm_order", &[]).await.expect("drop read model");
        db::execute(
            "DELETE FROM dorm_outbox_cursor WHERE consumer = 'rm-orders'",
            &[],
        )
        .await
        .expect("reset cursor");
        assert_eq!(count().await, 0, "read model emptied");

        relay("rm-orders", "orders", RelayConfig::default(), project)
            .await
            .expect("replay");
        assert_eq!(count().await, 3, "rebuilt from the log");
        assert_eq!(
            status_of(1).await.as_deref(),
            Some("shipped"),
            "the rebuild reproduced order 1's final state"
        );
    });
}
