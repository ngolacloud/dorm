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
//! Broker backends (NATS JetStream, Kafka, AMQP) implement the same [`Publisher`]
//! trait behind Cargo features; they need a running broker and are not exercised
//! here yet.

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
