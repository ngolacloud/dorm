//! The NATS broker backend + the outbox→broker bridge (ADR 0031/0051) against a
//! live NATS server and a live PostgreSQL.
//!
//! ```text
//! DORM_NATS_URL='nats://127.0.0.1:4232' \
//! DELONIX_ORM_TEST_DSN='host=/var/run/postgresql port=5432 user=walter dbname=ormtest' \
//!   cargo test -p dorm-messaging --features nats --test nats_live -- --ignored --nocapture
//! ```
//!
//! The durable path: business writes land in the outbox transactionally
//! ([`OutboxBus::publish`]); [`OutboxBus::forward`] relays them to NATS with
//! at-least-once delivery, advancing its cursor only after a successful publish.

#![cfg(feature = "nats")]

use dorm_core::db;
use dorm_messaging::nats::NatsPublisher;
use dorm_messaging::{CloudEvent, OutboxBus, Publisher};
use futures::StreamExt;

fn var(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|s| !s.trim().is_empty())
}

async fn setup_outbox() {
    for stmt in dorm_outbox::migrations() {
        db::execute(&stmt, &[]).await.expect("outbox schema");
    }
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a live NATS (DORM_NATS_URL) and Postgres (DELONIX_ORM_TEST_DSN)"]
async fn outbox_events_are_forwarded_to_nats_at_least_once() {
    let (Some(nats_url), Some(dsn)) = (var("DORM_NATS_URL"), var("DELONIX_ORM_TEST_DSN")) else {
        eprintln!("skipped: set DORM_NATS_URL and DELONIX_ORM_TEST_DSN");
        return;
    };
    std::env::set_var(db::DSN_ENV, &dsn);
    setup_outbox().await;

    let subject = "l7.orders";
    let publisher = NatsPublisher::connect(&nats_url)
        .await
        .expect("connect to NATS");

    // Subscribe before forwarding — core NATS delivers to live subscribers.
    let mut sub = publisher
        .client()
        .subscribe(subject.to_string())
        .await
        .expect("subscribe");
    publisher.client().flush().await.expect("flush sub");

    // Business writes: three CloudEvents committed to the outbox in one tx.
    let events: Vec<CloudEvent> = (0..3)
        .map(|i| {
            CloudEvent::new(
                format!("evt-{i}"),
                "/ngolacloud/compute",
                "app.deployed",
                serde_json::json!({ "n": i }),
            )
        })
        .collect();
    let to_publish = events.clone();
    db::atomic(move |_tx| async move {
        for ev in &to_publish {
            OutboxBus.publish("l7.orders", ev).await?;
        }
        Ok(())
    })
    .await
    .expect("publish to the outbox in a transaction");

    // The bridge relays the outbox to NATS; the cursor advances only on success.
    let forwarded = OutboxBus
        .forward(
            "nats-bridge",
            "l7.orders",
            dorm_outbox::RelayConfig::default(),
            &publisher,
        )
        .await
        .expect("forward to NATS");
    assert_eq!(forwarded, 3, "all three events forwarded");

    // The subscriber receives the three envelopes, intact.
    let mut got = Vec::new();
    for _ in 0..3 {
        let msg = tokio::time::timeout(std::time::Duration::from_secs(5), sub.next())
            .await
            .expect("a message arrived before the timeout")
            .expect("the subscription is open");
        let json: serde_json::Value =
            serde_json::from_slice(&msg.payload).expect("the body is CloudEvents JSON");
        got.push(CloudEvent::from_json(&json).expect("a valid CloudEvent"));
    }
    got.sort_by(|a, b| a.id.cmp(&b.id));
    assert_eq!(got, events, "every forwarded envelope arrived intact");

    // A second pass forwards nothing: the cursor already covers these events
    // (at-least-once, not at-least-twice on a healthy run).
    let again = OutboxBus
        .forward(
            "nats-bridge",
            "l7.orders",
            dorm_outbox::RelayConfig::default(),
            &publisher,
        )
        .await
        .expect("second forward");
    assert_eq!(again, 0, "a second pass forwards nothing new");

    eprintln!("ok: 3 outbox events forwarded to NATS and received intact; cursor is idempotent");
}
