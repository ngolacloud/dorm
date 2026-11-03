# dorm-jobs

An optional, durable **PostgreSQL job queue** for [DORM](../../README.md)
(ADR 0027) — what a control plane's reconcilers need.

- `enqueue(queue, kind, payload, opts)` — durable work, with **priority**,
  future **scheduling** (`delay_ms`), `max_attempts`, and a **`unique_key`** so a
  resource has at most one active job (a duplicate enqueue is a no-op).
- `dequeue(queue, worker)` / `run_once(...)` — claim the next due job with
  `FOR UPDATE SKIP LOCKED`, so two workers never take the same one.
- `complete` / `fail` — `fail` retries with backoff, then **dead-letters**.
- `acquire_lease(name, holder, ttl_ms)` — expiring **leases** for leader
  election: one active reconciler per partition, pool-friendly (no session
  advisory lock pinned to the wrong connection).
- `depth(queue)` / `oldest_age_secs(queue)` — queue metrics.

One-way: `dorm-core` never depends on this crate.

```text
DELONIX_ORM_TEST_DSN='host=127.0.0.1 port=5432 user=orm dbname=ormtest' \
  cargo test -p dorm-jobs --test jobs_live -- --ignored --nocapture
```
