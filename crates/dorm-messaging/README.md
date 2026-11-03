# dorm-messaging

Optional messaging for [DORM](../../README.md) (ADR 0031): a **CloudEvents 1.0**
envelope and `Publisher`/`Subscriber` traits.

The default transport is the generic outbox ([dorm-outbox](../dorm-outbox)):

- `OutboxBus::publish(topic, &event)` — writes the event in the **same
  transaction** as the business change.
- `OutboxBus::drain(consumer, topic, cfg, handler)` — **at-least-once** delivery,
  **exactly-once effect**: the handler's database work and the inbox record commit
  together, so a redelivered event is a no-op.

Every message carries `id`, `source`, `type`, `specversion`, optional `time`, and
a W3C `traceparent` for distributed tracing, plus a JSON `data` body.

Broker backends (NATS JetStream, Kafka, AMQP) implement the same `Publisher` trait
behind Cargo features; they need a running broker.

```text
DELONIX_ORM_TEST_DSN='host=127.0.0.1 port=5432 user=orm dbname=ormtest' \
  cargo test -p dorm-messaging --test messaging_live -- --ignored --nocapture
```
