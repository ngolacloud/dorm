//! The Kafka broker backend + the outbox→broker bridge (ADR 0053) against a live
//! Kafka (KRaft) and a live PostgreSQL.
//!
//! ```text
//! DORM_KAFKA_BROKERS='127.0.0.1:9095' \
//! DELONIX_ORM_TEST_DSN='host=/var/run/postgresql port=5432 user=walter dbname=ormtest' \
//!   cargo test -p dorm-messaging --features kafka --test kafka_live -- --ignored --nocapture
//! ```
//!
//! The same durable path as NATS/AMQP (ADR 0051/0052), a third broker: business
//! writes land in the outbox transactionally, and [`OutboxBus::forward`] relays
//! them to Kafka with at-least-once delivery, advancing its cursor only after each
//! produce is acked.

#![cfg(feature = "kafka")]

use dorm_core::db;
use dorm_messaging::kafka::KafkaPublisher;
use dorm_messaging::{CloudEvent, OutboxBus, Publisher};
use rskafka::client::partition::{OffsetAt, UnknownTopicHandling};
use std::collections::BTreeSet;

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
#[ignore = "needs a live Kafka (DORM_KAFKA_BROKERS) and Postgres (DELONIX_ORM_TEST_DSN)"]
async fn outbox_events_are_forwarded_to_kafka_at_least_once() {
    let (Some(brokers), Some(dsn)) = (var("DORM_KAFKA_BROKERS"), var("DELONIX_ORM_TEST_DSN"))
    else {
        eprintln!("skipped: set DORM_KAFKA_BROKERS and DELONIX_ORM_TEST_DSN");
        return;
    };
    std::env::set_var(db::DSN_ENV, &dsn);
    setup_outbox().await;

    let topic = "l7-orders-kafka";
    let publisher = KafkaPublisher::connect(&brokers)
        .await
        .expect("connect to Kafka");
    publisher
        .ensure_topic(topic, 1, 1)
        .await
        .expect("ensure topic");

    // The topic persists across runs, so read only from the current end: capture
    // the latest offset before publishing.
    let consumer = publisher
        .client()
        .partition_client(topic.to_string(), 0, UnknownTopicHandling::Retry)
        .await
        .expect("partition client");
    let start = consumer
        .get_offset(OffsetAt::Latest)
        .await
        .expect("latest offset");

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
            OutboxBus.publish("l7-orders-kafka", ev).await?;
        }
        Ok(())
    })
    .await
    .expect("publish to the outbox in a transaction");

    // The bridge relays the outbox to Kafka; the cursor advances only after each
    // produce is acked.
    let forwarded = OutboxBus
        .forward(
            "kafka-bridge",
            "l7-orders-kafka",
            dorm_outbox::RelayConfig::default(),
            &publisher,
        )
        .await
        .expect("forward to Kafka");
    assert_eq!(forwarded, 3, "all three events forwarded");

    // Fetch the three records back from where the log ended before this run.
    let mut got = Vec::new();
    let mut offset = start;
    for _ in 0..10 {
        if got.len() >= 3 {
            break;
        }
        let (records, _high_watermark) = consumer
            .fetch_records(offset, 1..1_000_000, 1_000)
            .await
            .expect("fetch");
        for rao in records {
            let value = rao.record.value.expect("a record value");
            let json: serde_json::Value =
                serde_json::from_slice(&value).expect("the body is CloudEvents JSON");
            got.push(CloudEvent::from_json(&json).expect("a valid CloudEvent"));
            offset = rao.offset + 1;
        }
    }
    got.sort_by(|a, b| a.id.cmp(&b.id));
    assert_eq!(got, events, "every forwarded envelope arrived intact");

    // A second pass forwards nothing: the cursor already covers these events.
    let again = OutboxBus
        .forward(
            "kafka-bridge",
            "l7-orders-kafka",
            dorm_outbox::RelayConfig::default(),
            &publisher,
        )
        .await
        .expect("second forward");
    assert_eq!(again, 0, "a second pass forwards nothing new");

    eprintln!(
        "ok: 3 outbox events forwarded to Kafka and fetched back intact; cursor is idempotent"
    );
}

/// Key-hashed partitioning (ADR 0053 follow-up): events with distinct keys spread
/// across a multi-partition topic, instead of all landing on partition 0.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a live Kafka (DORM_KAFKA_BROKERS)"]
async fn distinct_keys_spread_across_partitions() {
    let Some(brokers) = var("DORM_KAFKA_BROKERS") else {
        eprintln!("skipped: DORM_KAFKA_BROKERS is not set");
        return;
    };
    let topic = "l7-kafka-partitioned";
    let publisher = KafkaPublisher::connect(&brokers).await.expect("connect");
    publisher
        .ensure_topic(topic, 3, 1)
        .await
        .expect("ensure 3-partition topic");

    // Record each partition's end before publishing, so we count only this run.
    let mut starts = Vec::new();
    for p in 0..3i32 {
        let pc = publisher
            .client()
            .partition_client(topic.to_string(), p, UnknownTopicHandling::Retry)
            .await
            .expect("partition client");
        starts.push(pc.get_offset(OffsetAt::Latest).await.expect("offset"));
    }

    // Publish 30 events with distinct ids directly through the publisher.
    for i in 0..30i64 {
        let ev = CloudEvent::new(
            format!("key-{i}"),
            "/ngolacloud/compute",
            "app.deployed",
            serde_json::json!({ "n": i }),
        );
        publisher.publish(topic, &ev).await.expect("publish");
    }

    // Count the partitions that received records this run.
    let mut used = BTreeSet::new();
    let mut total = 0;
    for p in 0..3i32 {
        let pc = publisher
            .client()
            .partition_client(topic.to_string(), p, UnknownTopicHandling::Retry)
            .await
            .expect("partition client");
        let (recs, _hw) = pc
            .fetch_records(starts[p as usize], 1..1_000_000, 1_000)
            .await
            .expect("fetch");
        if !recs.is_empty() {
            used.insert(p);
            total += recs.len();
        }
    }
    assert_eq!(total, 30, "every event landed somewhere");
    assert!(
        used.len() >= 2,
        "distinct keys must spread across more than one partition, used {used:?}"
    );

    eprintln!(
        "ok: 30 distinct keys spread across {} of 3 partitions",
        used.len()
    );
}
