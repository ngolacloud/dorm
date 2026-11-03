# dorm-outbox

An optional, generic **transactional outbox/inbox** for [DORM](../../README.md)
(ADR 0026).

Publish an event in the **same transaction** as the business write it announces,
then let independent consumers drain it at their own pace — each tracking its own
cursor, so two consumers see the same events (fan-out) and neither destroys what
the other has not read.

- `outbox::publish(topic, payload)` — append an event; **errors outside a
  transaction**, which is what makes it an outbox.
- `outbox::relay(consumer, topic, cfg, handler)` — drain the events this consumer
  has not seen, advancing its cursor; retries with backoff and **dead-letters**
  what keeps failing.
- `outbox::record_once(consumer, message_id)` — the idempotency **inbox**, for
  exactly-once *effect* under at-least-once delivery.

Delivery is at-least-once; a non-idempotent handler guards on the inbox. Optional
and one-way: `dorm-core` never depends on this crate, so a project that does not
use it has an unchanged dependency graph.

```text
DELONIX_ORM_TEST_DSN='host=127.0.0.1 port=5432 user=orm dbname=ormtest' \
  cargo test -p dorm-outbox --test outbox_live -- --ignored --nocapture
```
