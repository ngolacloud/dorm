//! The AMQP broker backend + the outbox→broker bridge (ADR 0052) against a live
//! RabbitMQ and a live PostgreSQL.
//!
//! ```text
//! DORM_AMQP_URL='amqp://guest:guest@127.0.0.1:5672/%2f' \
//! DELONIX_ORM_TEST_DSN='host=/var/run/postgresql port=5432 user=walter dbname=ormtest' \
//!   cargo test -p dorm-messaging --features amqp --test amqp_live -- --ignored --nocapture
//! ```
//!
//! The same durable path as NATS (ADR 0051), a different broker: business writes
//! land in the outbox transactionally, and [`OutboxBus::forward`] relays them to
//! RabbitMQ with at-least-once delivery, advancing its cursor only after the
//! broker confirms each publish.

#![cfg(feature = "amqp")]

use dorm_core::db;
use dorm_messaging::amqp::AmqpPublisher;
use dorm_messaging::{CloudEvent, OutboxBus, Publisher};
use futures::StreamExt;
use lapin::options::{
    BasicAckOptions, BasicConsumeOptions, QueueDeclareOptions, QueuePurgeOptions,
};
use lapin::types::FieldTable;

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
#[ignore = "needs a live RabbitMQ (DORM_AMQP_URL) and Postgres (DELONIX_ORM_TEST_DSN)"]
async fn outbox_events_are_forwarded_to_amqp_at_least_once() {
    let (Some(amqp_url), Some(dsn)) = (var("DORM_AMQP_URL"), var("DELONIX_ORM_TEST_DSN")) else {
        eprintln!("skipped: set DORM_AMQP_URL and DELONIX_ORM_TEST_DSN");
        return;
    };
    std::env::set_var(db::DSN_ENV, &dsn);
    setup_outbox().await;

    let queue = "l7.orders.amqp";
    let publisher = AmqpPublisher::connect(&amqp_url)
        .await
        .expect("connect to RabbitMQ");

    // Declare the queue before forwarding — the default exchange routes a message
    // to the queue whose name equals the routing key, so it must exist first; and
    // purge it so the assertion counts only this run's events.
    let ch = publisher.channel();
    ch.queue_declare(queue, QueueDeclareOptions::default(), FieldTable::default())
        .await
        .expect("declare queue");
    ch.queue_purge(queue, QueuePurgeOptions::default())
        .await
        .expect("purge queue");

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
            OutboxBus.publish("l7.orders.amqp", ev).await?;
        }
        Ok(())
    })
    .await
    .expect("publish to the outbox in a transaction");

    // The bridge relays the outbox to RabbitMQ; the cursor advances only after the
    // broker confirms each publish.
    let forwarded = OutboxBus
        .forward(
            "amqp-bridge",
            "l7.orders.amqp",
            dorm_outbox::RelayConfig::default(),
            &publisher,
        )
        .await
        .expect("forward to AMQP");
    assert_eq!(forwarded, 3, "all three events forwarded");

    // A consumer drains the queue and gets the three envelopes, intact.
    let mut consumer = ch
        .basic_consume(
            queue,
            "amqp-live-test",
            BasicConsumeOptions::default(),
            FieldTable::default(),
        )
        .await
        .expect("consume");
    let mut got = Vec::new();
    for _ in 0..3 {
        let delivery = tokio::time::timeout(std::time::Duration::from_secs(5), consumer.next())
            .await
            .expect("a message arrived before the timeout")
            .expect("the consumer is open")
            .expect("a delivery");
        let json: serde_json::Value =
            serde_json::from_slice(&delivery.data).expect("the body is CloudEvents JSON");
        got.push(CloudEvent::from_json(&json).expect("a valid CloudEvent"));
        delivery.ack(BasicAckOptions::default()).await.expect("ack");
    }
    got.sort_by(|a, b| a.id.cmp(&b.id));
    assert_eq!(got, events, "every forwarded envelope arrived intact");

    // A second pass forwards nothing: the cursor already covers these events.
    let again = OutboxBus
        .forward(
            "amqp-bridge",
            "l7.orders.amqp",
            dorm_outbox::RelayConfig::default(),
            &publisher,
        )
        .await
        .expect("second forward");
    assert_eq!(again, 0, "a second pass forwards nothing new");

    eprintln!(
        "ok: 3 outbox events forwarded to RabbitMQ and received intact; cursor is idempotent"
    );
}
