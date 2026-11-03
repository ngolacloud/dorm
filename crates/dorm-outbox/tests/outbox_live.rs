//! Generic outbox against a real PostgreSQL (ADR 0026).
//!
//! ```text
//! DELONIX_ORM_TEST_DSN='host=127.0.0.1 port=5432 user=orm dbname=ormtest' \
//!   cargo test -p dorm-outbox --test outbox_live -- --ignored --nocapture
//! ```

use std::sync::{Arc, Mutex};

use dorm_core::db;
use dorm_core::error::Error;
use dorm_outbox::{migrations, publish, record_once, relay, RelayConfig};
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

async fn setup() {
    for stmt in migrations() {
        db::execute(&stmt, &[]).await.expect("outbox schema");
    }
    // This test owns the outbox tables in the throwaway database.
    for t in [
        "dorm_outbox",
        "dorm_outbox_cursor",
        "dorm_inbox",
        "dorm_outbox_dead",
    ] {
        db::execute(&format!("TRUNCATE {t}"), &[])
            .await
            .expect("truncate");
    }
}

#[test]
#[ignore = "needs a Postgres (DELONIX_ORM_TEST_DSN)"]
fn consumers_fan_out_dead_letter_and_the_inbox_dedupes() {
    let Some(dsn) = dsn() else {
        eprintln!("skipped: DELONIX_ORM_TEST_DSN is not set");
        return;
    };
    std::env::set_var(dorm_core::db::DSN_ENV, &dsn);

    runtime().block_on(async {
        setup().await;

        // Publish five events for the business topic, all in one transaction.
        let ids: Vec<i64> = db::atomic(|_tx| async {
            let mut ids = Vec::new();
            for i in 0..5 {
                ids.push(publish("orders", &serde_json::json!({ "n": i })).await?);
            }
            Ok(ids)
        })
        .await
        .expect("publish in a transaction");
        assert_eq!(ids.len(), 5);
        assert!(ids.windows(2).all(|w| w[0] < w[1]), "ids are monotonic");

        // Consumer A drains all five, in order.
        let seen_a = Arc::new(Mutex::new(Vec::<i64>::new()));
        let a = seen_a.clone();
        let stats = relay(
            "projection-a",
            "orders",
            RelayConfig::default(),
            move |ev| {
                let a = a.clone();
                async move {
                    a.lock().unwrap().push(ev.payload["n"].as_i64().unwrap());
                    Ok(())
                }
            },
        )
        .await
        .expect("relay A");
        assert_eq!(stats.processed, 5);
        assert_eq!(
            *seen_a.lock().unwrap(),
            vec![0, 1, 2, 3, 4],
            "A saw all, in order"
        );

        // Consumer B is independent — its own cursor, so it also sees all five.
        let seen_b = Arc::new(Mutex::new(Vec::<i64>::new()));
        let b = seen_b.clone();
        let stats = relay(
            "projection-b",
            "orders",
            RelayConfig::default(),
            move |ev| {
                let b = b.clone();
                async move {
                    b.lock().unwrap().push(ev.payload["n"].as_i64().unwrap());
                    Ok(())
                }
            },
        )
        .await
        .expect("relay B");
        assert_eq!(stats.processed, 5, "B drains the same events (fan-out)");

        // Re-draining A does nothing: its cursor is already at the end.
        let stats = relay(
            "projection-a",
            "orders",
            RelayConfig::default(),
            move |_ev| async {
                panic!("A should have nothing left to do");
                #[allow(unreachable_code)]
                Ok(())
            },
        )
        .await
        .expect("relay A again");
        assert_eq!(stats.processed, 0);

        // A handler that always fails dead-letters after max_attempts, and the
        // cursor advances so the consumer does not stall.
        let cfg = RelayConfig {
            max_attempts: 2,
            backoff_base_ms: 1,
            ..Default::default()
        };
        let stats = relay("flaky", "orders", cfg, move |_ev| async {
            Err::<(), Error>(Error::Db("boom".into()))
        })
        .await
        .expect("relay flaky");
        assert_eq!(stats.processed, 0);
        assert_eq!(stats.dead_lettered, 5, "all five dead-lettered");

        let dead = db::sql(
            "SELECT COUNT(*)::bigint FROM dorm_outbox_dead WHERE consumer = $1",
            &[dorm_core::value::Value::Text("flaky".into())],
        )
        .await
        .expect("dead count");
        assert_eq!(dead[0].try_get::<i64>(0).unwrap(), 5);

        // The inbox dedupes: the first record wins, the second is a no-op.
        assert!(record_once("c", "msg-1").await.expect("first"));
        assert!(
            !record_once("c", "msg-1").await.expect("second"),
            "duplicate is refused"
        );
        assert!(record_once("c", "msg-2").await.expect("other id"));
    });
}
