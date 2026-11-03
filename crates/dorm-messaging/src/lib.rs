//! Messaging for DORM: a CloudEvents envelope and a transactional bus (ADR 0031).
//!
//! The envelope is [CloudEvents 1.0](https://cloudevents.io): every message
//! carries `id`, `source`, `type`, `specversion`, an optional `time` and a
//! W3C `traceparent`, plus a JSON `data` body. The default transport is the
//! generic outbox ([`dorm-outbox`](dorm_outbox)): [`OutboxBus::publish`] writes
//! the event in the **same transaction** as the business change, and
//! [`OutboxBus::drain`] consumes it with **at-least-once** delivery and
//! **exactly-once effect** — the handler's database work and the inbox record
//! commit together, so a redelivered event is a no-op.
//!
//! Broker backends implement the same [`Publisher`] trait behind Cargo features,
//! and [`OutboxBus::forward`] bridges the outbox to any of them, at-least-once.
//! **NATS** ([`nats`], feature `nats`, ADR 0051) and **AMQP/RabbitMQ** ([`amqp`],
//! feature `amqp`, ADR 0052) are implemented and proven against live brokers;
//! Kafka is the same shape, behind its feature, not yet wired.

use std::future::Future;

use dorm_core::db;
use dorm_core::error::{Error, Result};

/// A CloudEvents 1.0 envelope.
#[derive(Debug, Clone, PartialEq)]
pub struct CloudEvent {
    /// Unique per `(source, id)` — the idempotency key the inbox dedupes on.
    pub id: String,
    pub source: String,
    /// The event type, e.g. `com.ngolacloud.app.deployed`. (`type` is reserved in
    /// Rust, hence `ty`.)
    pub ty: String,
    /// RFC3339, optional.
    pub time: Option<String>,
    /// W3C trace context, carried end to end for distributed tracing.
    pub traceparent: Option<String>,
    /// The event body.
    pub data: serde_json::Value,
}

impl CloudEvent {
    /// A new event with the required attributes; `time`/`traceparent` unset.
    pub fn new(
        id: impl Into<String>,
        source: impl Into<String>,
        ty: impl Into<String>,
        data: serde_json::Value,
    ) -> Self {
        CloudEvent {
            id: id.into(),
            source: source.into(),
            ty: ty.into(),
            time: None,
            traceparent: None,
            data,
        }
    }

    pub fn with_traceparent(mut self, traceparent: impl Into<String>) -> Self {
        self.traceparent = Some(traceparent.into());
        self
    }

    pub fn with_time(mut self, time: impl Into<String>) -> Self {
        self.time = Some(time.into());
        self
    }

    /// The structured-mode JSON of the envelope (CloudEvents JSON format).
    pub fn to_json(&self) -> serde_json::Value {
        let mut map = serde_json::Map::new();
        map.insert("specversion".into(), "1.0".into());
        map.insert("id".into(), self.id.clone().into());
        map.insert("source".into(), self.source.clone().into());
        map.insert("type".into(), self.ty.clone().into());
        if let Some(t) = &self.time {
            map.insert("time".into(), t.clone().into());
        }
        if let Some(tp) = &self.traceparent {
            map.insert("traceparent".into(), tp.clone().into());
        }
        map.insert("data".into(), self.data.clone());
        serde_json::Value::Object(map)
    }

    /// Parse an envelope from its JSON, checking the required attributes.
    pub fn from_json(v: &serde_json::Value) -> Result<Self> {
        let s = |k: &str| {
            v.get(k)
                .and_then(|x| x.as_str())
                .map(str::to_string)
                .ok_or_else(|| Error::Config(format!("CloudEvent is missing `{k}`")))
        };
        Ok(CloudEvent {
            id: s("id")?,
            source: s("source")?,
            ty: s("type")?,
            time: v.get("time").and_then(|x| x.as_str()).map(str::to_string),
            traceparent: v
                .get("traceparent")
                .and_then(|x| x.as_str())
                .map(str::to_string),
            data: v.get("data").cloned().unwrap_or(serde_json::Value::Null),
        })
    }
}

/// Publish a [`CloudEvent`] to a topic. The outbox-backed implementation requires
/// a transaction; a broker implementation does not.
pub trait Publisher {
    fn publish(&self, topic: &str, event: &CloudEvent) -> impl Future<Output = Result<()>>;
}

/// The default bus: events ride the generic outbox (ADR 0026), so publishing is
/// transactional with the business write and consumption is at-least-once with
/// exactly-once effect.
#[derive(Debug, Clone, Copy, Default)]
pub struct OutboxBus;

impl Publisher for OutboxBus {
    /// Write the event to the outbox — **inside `db::atomic`**, so it commits with
    /// the business change (the outbox refuses a publish outside a transaction).
    async fn publish(&self, topic: &str, event: &CloudEvent) -> Result<()> {
        dorm_outbox::publish(topic, &event.to_json())
            .await
            .map(|_| ())
    }
}

impl OutboxBus {
    /// Drain the events `consumer` has not seen for `topic`, calling `handler` for
    /// each **exactly once in effect**: the handler's database work and the inbox
    /// record commit in one transaction, so a redelivered event (at-least-once) is
    /// a no-op. Returns how many events were processed this pass.
    pub async fn drain<H, Fut>(
        &self,
        consumer: &str,
        topic: &str,
        cfg: dorm_outbox::RelayConfig,
        handler: H,
    ) -> Result<u64>
    where
        H: Fn(CloudEvent) -> Fut,
        Fut: Future<Output = Result<()>>,
    {
        let handler = &handler;
        let consumer = consumer.to_string();
        let stats = dorm_outbox::relay(&consumer.clone(), topic, cfg, move |ev| {
            let consumer = consumer.clone();
            async move {
                let event = CloudEvent::from_json(&ev.payload)?;
                // The inbox guard and the handler's effect share one transaction:
                // record-once returns false on a redelivery, and then the handler
                // is skipped — exactly-once effect under at-least-once delivery.
                db::atomic(move |_tx| async move {
                    if dorm_outbox::record_once(&consumer, &event.id).await? {
                        handler(event).await
                    } else {
                        Ok(())
                    }
                })
                .await
            }
        })
        .await?;
        Ok(stats.processed)
    }

    /// Forward outbox events to an external broker through `publisher` (ADR 0031):
    /// drain what `consumer` has not seen for `topic` and publish each, advancing
    /// the cursor **only after** a successful publish. This is the
    /// transactional-outbox → broker bridge — the business write committed the
    /// event with its own transaction ([`OutboxBus::publish`]); this relay carries
    /// it to the broker with **at-least-once** delivery (a publish that fails
    /// leaves the cursor where it was, so the event is retried, and after
    /// `max_attempts` it dead-letters, exactly as [`dorm_outbox::relay`] defines).
    ///
    /// Works with any [`Publisher`]: the [`nats::NatsPublisher`] behind the `nats`
    /// feature, or a test double. Returns how many events it forwarded this pass.
    pub async fn forward<P: Publisher>(
        &self,
        consumer: &str,
        topic: &str,
        cfg: dorm_outbox::RelayConfig,
        publisher: &P,
    ) -> Result<u64> {
        let stats = dorm_outbox::relay(consumer, topic, cfg, move |ev| async move {
            let event = CloudEvent::from_json(&ev.payload)?;
            publisher.publish(topic, &event).await
        })
        .await?;
        Ok(stats.processed)
    }
}

/// The NATS broker backend (ADR 0031/0051), behind the `nats` feature.
///
/// A [`NatsPublisher`] carries [`CloudEvent`]s to a NATS subject in
/// CloudEvents **structured** JSON (the whole envelope is the message body).
/// Pair it with [`OutboxBus::forward`] for the durable, at-least-once path:
/// business writes land in the outbox transactionally, and the relay forwards
/// them here.
#[cfg(feature = "nats")]
pub mod nats {
    use super::{CloudEvent, Error, Publisher, Result};

    /// A [`Publisher`] that publishes to a NATS server.
    pub struct NatsPublisher {
        client: async_nats::Client,
    }

    impl NatsPublisher {
        /// Connect to a NATS server (`nats://host:port`).
        pub async fn connect(url: &str) -> Result<Self> {
            let client = async_nats::connect(url)
                .await
                .map_err(|e| Error::Db(format!("NATS connect failed: {e}")))?;
            Ok(NatsPublisher { client })
        }

        /// The underlying client, for a subscription or JetStream context the
        /// caller wants to drive directly.
        pub fn client(&self) -> &async_nats::Client {
            &self.client
        }
    }

    impl Publisher for NatsPublisher {
        /// Publish the CloudEvents-JSON envelope to `topic` (the NATS subject) and
        /// flush, so a failure surfaces here rather than being buffered — the
        /// relay must not advance its cursor on an unacknowledged publish.
        async fn publish(&self, topic: &str, event: &CloudEvent) -> Result<()> {
            let bytes = serde_json::to_vec(&event.to_json())
                .map_err(|e| Error::Db(format!("CloudEvent serialize failed: {e}")))?;
            self.client
                .publish(topic.to_string(), bytes.into())
                .await
                .map_err(|e| Error::Db(format!("NATS publish failed: {e}")))?;
            self.client
                .flush()
                .await
                .map_err(|e| Error::Db(format!("NATS flush failed: {e}")))?;
            Ok(())
        }
    }
}

/// The AMQP broker backend (ADR 0052), behind the `amqp` feature.
///
/// An [`AmqpPublisher`] carries [`CloudEvent`]s to an AMQP 0-9-1 broker (RabbitMQ)
/// as CloudEvents **structured** JSON, on the default exchange with the topic as
/// the routing key (so a queue of the topic's name receives it). Publisher
/// **confirms** are on, so [`Publisher::publish`] returns only once the broker has
/// acked — the relay must not advance its cursor on an unconfirmed publish. Pair
/// it with [`OutboxBus::forward`] for the durable, at-least-once path.
#[cfg(feature = "amqp")]
pub mod amqp {
    use super::{CloudEvent, Error, Publisher, Result};
    use lapin::options::BasicPublishOptions;
    use lapin::{BasicProperties, Channel, Connection, ConnectionProperties};

    /// A [`Publisher`] that publishes to an AMQP 0-9-1 broker. Holds the connection
    /// so the channel stays open for the publisher's lifetime.
    pub struct AmqpPublisher {
        _connection: Connection,
        channel: Channel,
    }

    impl AmqpPublisher {
        /// Connect to an AMQP broker (`amqp://user:pass@host:port/vhost`) and put
        /// the channel into publisher-confirm mode.
        pub async fn connect(uri: &str) -> Result<Self> {
            let connection = Connection::connect(uri, ConnectionProperties::default())
                .await
                .map_err(|e| Error::Db(format!("AMQP connect failed: {e}")))?;
            let channel = connection
                .create_channel()
                .await
                .map_err(|e| Error::Db(format!("AMQP channel failed: {e}")))?;
            channel
                .confirm_select(lapin::options::ConfirmSelectOptions::default())
                .await
                .map_err(|e| Error::Db(format!("AMQP confirm_select failed: {e}")))?;
            Ok(AmqpPublisher {
                _connection: connection,
                channel,
            })
        }

        /// The underlying channel, for a caller that wants to declare a queue, set
        /// up a consumer, or publish to a named exchange directly.
        pub fn channel(&self) -> &Channel {
            &self.channel
        }
    }

    impl Publisher for AmqpPublisher {
        /// Publish the CloudEvents-JSON envelope with `topic` as the routing key on
        /// the default exchange, and **await the broker's confirm**, so a failure
        /// surfaces here rather than being buffered.
        async fn publish(&self, topic: &str, event: &CloudEvent) -> Result<()> {
            let bytes = serde_json::to_vec(&event.to_json())
                .map_err(|e| Error::Db(format!("CloudEvent serialize failed: {e}")))?;
            self.channel
                .basic_publish(
                    "",
                    topic,
                    BasicPublishOptions::default(),
                    &bytes,
                    BasicProperties::default(),
                )
                .await
                .map_err(|e| Error::Db(format!("AMQP publish failed: {e}")))?
                .await
                .map_err(|e| Error::Db(format!("AMQP publish not confirmed: {e}")))?;
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_envelope_round_trips_through_cloudevents_json() {
        let ce = CloudEvent::new(
            "evt-1",
            "/ngolacloud/compute",
            "app.deployed",
            serde_json::json!({ "app": "api" }),
        )
        .with_traceparent("00-abc-def-01");
        let json = ce.to_json();
        assert_eq!(json["specversion"], "1.0");
        assert_eq!(json["type"], "app.deployed");
        assert_eq!(json["traceparent"], "00-abc-def-01");
        assert_eq!(CloudEvent::from_json(&json).unwrap(), ce);
    }

    #[test]
    fn a_missing_required_attribute_is_an_error() {
        let bad = serde_json::json!({ "id": "x" }); // no source/type
        assert!(CloudEvent::from_json(&bad).is_err());
    }
}
