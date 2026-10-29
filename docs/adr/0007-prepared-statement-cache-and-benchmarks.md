# ADR 0007 — A per-connection prepared-statement cache, and the benchmark that proves it

## Status

Accepted. Library brick: `db::query` and `db::execute` prepare through
deadpool's per-connection `StatementCache` (`prepare_cached`) instead of parsing
an unnamed statement on every call. Proven by `crates/dorm-core/benches/prepared_cache.rs`
(timing) and `crates/dorm-core/tests/prepared_cache_live.rs` (a deterministic,
CI-gated assertion).

## Context

Every typed `QuerySet` terminal and every raw statement funnels through two
functions in `db.rs`: `query` (rows back) and `execute` (row count back). Both
ended in `conn.query(sql, params)` / `conn.execute(sql, params)` — the
`tokio_postgres::Client` convenience methods.

Those methods are a trap for a hot path. Each one Parses an **unnamed** prepared
statement, Binds, Executes, and throws the parse away. The next identical call
pays Parse again. An ORM's access pattern is the opposite of that waste: the same
handful of query shapes, with the same `$n` placeholders, run over and over — the
compiler emits byte-identical SQL for a given `QuerySet`. We were asking the
server to re-plan a statement it had just planned, thousands of times a second.

The fix is the oldest one in the book: prepare each distinct statement once per
connection, keep the handle, and Bind/Execute against it thereafter.

## Decision

`query` and `execute` now call `conn.prepare_cached(sql)` and run against the
returned `Statement`. `prepare_cached` is deadpool-postgres' wrapper over a
per-connection `StatementCache`: first sight of a SQL string on a connection
Parses and names it; every later sight returns the cached handle with no round
trip. The cache lives on the pooled connection, so it survives across checkouts
(the pool recycles with `RecyclingMethod::Fast`, which does not clear it).

This holds on both code paths already in `db.rs`:

- **Outside a transaction** — the acquired pooled connection caches, and the
  cache persists because the connection returns to the pool.
- **Inside `atomic(…)`** — the task-local connection the transaction already
  holds caches too, so a loop of writes in one transaction prepares once.

Nothing else changed: the transaction routing, the read-only guard, the
statement timeout and TLS (ADR 0004), and the backend seam (ADR 0003) all sit
above this and are untouched. SQLite is unaffected — it has its own path and
`rusqlite`'s own statement handling.

## The proof

### A deterministic test (runs in CI, gated on a DSN)

`pg_prepared_statements` lists a connection's **named** prepared statements and
is silent about unnamed ones. That difference is the whole test:
`prepared_cache_live.rs` pins the pool to one connection, runs one SQL string
five times, and asserts the catalog holds exactly **one** entry for it. The old
unnamed path would have left **zero**. One row ≠ zero rows is a clean, binary
witness that the statement is now cached rather than re-parsed per call.

### A benchmark (criterion, gated on a DSN)

`benches/prepared_cache.rs` measures two things against a real PostgreSQL, each
iteration a genuine round trip driven on a shared tokio runtime:

1. **The cache in isolation** — the same SELECT/INSERT on raw `tokio-postgres`,
   ad-hoc (`client.query(sql, …)`, the old shape) vs a statement prepared once.
2. **The typed ORM on top** — `QuerySet`, which now rides the cache, next to the
   equivalent hand-written call.

Measured on a 13th Gen Intel Core i9-13900H (20 threads), PostgreSQL 16.15 over
the local Unix socket, rustc 1.96.0, criterion's median of 100 samples:

| group        | arm          | median  | vs ad-hoc |
|--------------|--------------|---------|-----------|
| **select**   | raw-adhoc    | 189 µs  | —         |
|              | raw-cached   | 87 µs   | **2.2× faster** |
|              | dorm-typed   | 104 µs  | 1.8× faster |
| **insert**   | raw-adhoc    | 469 µs  | —         |
|              | raw-cached   | 347 µs  | **1.35× faster** |
|              | dorm-typed   | 503 µs  | ~1.0× |

Reading the numbers:

- **SELECT is where the cache shines.** Removing the per-call Parse round trip
  roughly halves the wire time (189 → 87 µs). The typed `QuerySet` lands at
  104 µs — it bets its SQL-compilation and row-decoding overhead against the
  Parse it no longer pays, and comes out *well below* the old ad-hoc path. Before
  this ADR the typed path was effectively `raw-adhoc` plus ORM overhead (~200 µs);
  it is now 104 µs — close to 2× on the full ORM call, not just the wire.
- **INSERT gains less in relative terms** because the write itself — heap, WAL,
  `RETURNING` — dominates and is unaffected by the cache. The Parse saving is the
  same absolute ~120 µs (469 → 347 µs), simply a smaller share of a pricier
  statement. The typed insert sits near raw-adhoc: the ORM overhead it adds is
  about what the cache gives back.

Both arms confirm the same thing from two directions: the Parse round trip was
real, measurable cost, and the cache removes it.

### Benchmark method notes (why the INSERT group resets)

The three INSERT arms run in sequence and each appends thousands of rows. Left
alone, a growing index on `name` — every arm inserts the same value, piling
duplicate keys onto one b-tree page — makes each later arm slower than the one
before, for reasons that have nothing to do with statement caching. (The first
honest-looking run showed `raw-cached` *slower* than `raw-adhoc` purely from this
drift.) So `reset_for_insert` drops the index and truncates before the group,
leaving each INSERT a constant-time heap append — isolating the one variable
under test. The SELECT group keeps the index and reads a fixed 100-row `seed`
set, so its timing does not drift as the INSERT arms append.

## What this is not

- **Not a query-plan cache across schema changes.** A named prepared statement
  binds to the schema it was planned against. DDL that changes a cached
  statement's result columns can make the server reject the stale plan
  (`cached plan must not change result type`) on the connection that holds it.
  This is the standard prepared-statement trade-off; in practice DDL and the hot
  query path do not interleave on the same long-lived connection during normal
  operation, and the migrator runs DDL through `batch_execute` (`simple`), which
  is not cached. A caller that mixes online DDL with cached reads on one
  connection owns that interaction — named here rather than hidden.
- **Not a cross-connection cache.** The cache is per connection by design, which
  is also why the pool's `Fast` recycling is safe: a recycled connection keeps
  *its own* valid statements, and shares none with another.
- **Not a change to any public signature.** `query`/`execute` take and return
  exactly what they did; callers see only the lower latency.

## Consequences

- The ORM's hot path pays Parse once per statement per connection instead of once
  per call — a measured 1.8× on a typed SELECT end to end.
- A first-ever call on a fresh connection is unchanged (it must Parse regardless);
  the win accrues to repetition, which is the access pattern that matters.
- A new CI-gated regression test (`prepared_cache_live`) fails if the caching is
  ever lost, and a reproducible benchmark (`cargo bench -p dorm-core --bench
  prepared_cache`) quantifies it on demand.

## Reproducing

```bash
createdb dorm_bench
DELONIX_ORM_DSN="host=/var/run/postgresql user=$USER dbname=dorm_bench" \
  cargo bench -p dorm-core --bench prepared_cache
DELONIX_ORM_DSN="host=/var/run/postgresql user=$USER dbname=dorm_bench" \
  cargo test -p dorm-core --test prepared_cache_live -- --nocapture
dropdb dorm_bench
```

Both no-op cleanly when `DELONIX_ORM_DSN` (or `PAAS_DATABASE_URL`) is unset, so a
machine with no database skips rather than fails.
