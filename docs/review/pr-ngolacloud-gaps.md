# Close the NgolaCloud data-layer gaps (L1–L16)

## Why

DORM has to be NgolaCloud's multi-tenant data layer for ~10 years and hundreds of
thousands of users. This branch closes the L1–L16 gap measurement, each gap
**measured against the prior state, decided in an ADR, then implemented with a
live proof** against real infrastructure (PostgreSQL 16, PgBouncer, a streaming
replica fleet, NATS, RabbitMQ, Kafka). It holds the independence rule (no business
names in the crates) and the invariants: the query compiler stays pure, no
gratuitous `ModelMeta`/`FieldMeta`/`Value` fields (new per-model metadata rides
`inventory` slices), heavy/optional things sit behind a feature or an optional
crate, and `make check` is green at every commit. Nothing is pushed until this
merges.

## What's in it (by area, ADR + live proof)

**L1/L3/L4 — tenancy, concurrency, pagination** — tenant scope seam, optimistic
versioning, keyset pagination (ADR 0021/0023/0024).

**L2 — online migrations** — resumable backfill, expand/contract, `migrate lint`,
per-migration `lock_timeout` (ADR 0022/0038).

**L5 — richer column types (proven live on PG 16)**
- `@@id`/`@@unique` emitted from `dorm.schema` (0043); composite `@@index` via an
  `IndexMeta` inventory slice — no `ModelMeta` field (0044).
- Composite-key instance `save`/`delete`; this fixed a **latent correctness bug**
  where `UPDATE/DELETE … WHERE pk IN (SELECT pk …)` used only the first key column
  and could sweep sibling rows — now a row-value tuple `(a,b)` (0045).
- Native `ENUM` (0046) + a top-level `enum` block in `dorm.schema` (0047) +
  `ALTER TYPE … ADD VALUE` to evolve one; `CIDR`/`MACADDR` native types (0048).
- arrays + JSONB path lookups; `Value::ToSql` hardening via `to_sql_checked`
  (0037/0040/0041).

**L6/L9/L10/L11/L12 — outbox, jobs, partitions, schema handles, event sourcing**
— optional crates `dorm-outbox`/`dorm-jobs`/`dorm-eventstore`, monthly RANGE
partitions, `Database::connect_schema` (0026–0029/0033/0035/0039).

**L7 — broker backends over a transactional outbox (all proven live)** — one
generic bridge `OutboxBus::forward<P: Publisher>` (at-least-once; cursor advances
only after an acked publish) with **NATS** (0051), **AMQP/RabbitMQ** (0052) and
**Kafka** (0053, pure-Rust `rskafka`, key-hashed partitioning, TLS/SASL seam). An
outbox-only build pulls no broker client.

**L8 — multi-replica reads (proven live on a primary + two standbys)** — a replica
fleet with round-robin balancing, reactive ejection on a failed connection,
proactive **lag-based** ejection (`eject_lagging_replicas`, LSN-aware so an idle
primary is no false positive), and fallback to the primary; transactions/writes
stay on the primary (0050).

**L15 — PgBouncer transaction pooling (proven live)** — detect `pool_mode` from the
admin console, auto-switch to unprepared, and self-heal on both `26000` (missing)
and `42P05` (colliding) prepared-statement faults; the authoritative fix
(`max_prepared_statements`) is documented and shown to work under concurrency
(0049).

**L16 — scale benchmark + CI gate** — reproducible harness (`DORM_BENCH_ROWS`):
keyset vs deep OFFSET and composite-index seek vs scan, measured at **1M and 10M**
(gaps widen with scale: O(n) vs O(log n) — keyset ~305x/~2271x, index ~72x/~490x).
`scripts/bench_gate.sh` / `make bench` enforce a host-independent speedup floor and
exit non-zero on regression; baseline in `docs/review/bench-baseline.md` (0036).

## Follow-ups also landed (this round)

- `Inet`/`Cidr`/`MacAddr` as declarative `dorm.schema` types (0040/0048).
- `ALTER TYPE … ADD VALUE` on `sync` **and** in the **versioned** migrate: the
  autodetector's `Snapshot` now carries enum types, emits `CREATE TYPE` before the
  tables and `ALTER TYPE ADD VALUE` for grown ones, and `down_for` drops a new type
  / marks a grown enum irreversible (0046).
- Proactive lag-based replica ejection (0050).
- Key-hashed Kafka partitioning + a `from_client` TLS/SASL seam (0053).
- The scale CI gate and the 10M baseline (0036).

## Honestly deferred (in the ADRs)

- Scheduling the 10M / dedicated-host CI run (the gate and numbers exist).
- Kafka auth/TLS wiring beyond the `from_client` seam; `MACADDR8`; enum
  reorder/remove (not expressible in PostgreSQL).

## Testing

`make check` is green on every commit (fmt + clippy `-D warnings` + offline tests).
Live tests are `#[ignore]` and gated on env DSNs/brokers; each module doc has the
exact invocation. All were run green this session against locally stood-up infra
(PG 16, PgBouncer 1.22, a streaming primary+2 standbys, NATS 2.10, RabbitMQ 3.12,
Kafka 4.2 KRaft).

## Review notes

- Two commits were authored by isolated-worktree subagents and integrated by
  cherry-pick (composite-PK save/delete; the `dorm.schema` enum block); the
  `dorm-schema`/compiler overlaps were reconciled by hand.
- ADR numbering continues at 0043+ (the `review/dorm-e2e` branch's *Proposed*
  0010–0017 are a separate, colliding track — untouched here).
