# ADR 0008 — Production observability: per-statement tracing, a slow-query log, and pool metrics

## Status

Accepted. Library brick: `db::query` and `db::execute` emit one `tracing` event
per statement; a configurable threshold (`DELONIX_ORM_SLOW_QUERY_MS`) adds a
`warn` for slow ones; `db::pool_stats()` returns a serializable snapshot of the
connection pool. Proven by `crates/dorm-core/tests/observability_live.rs` (a
capturing subscriber witnesses the slow-query `warn` and inspects the live pool)
and the offline unit tests in `db.rs` (`summarize_sql`, `PoolStats` serde shape).

## Context

A library that owns the connection pool and every round trip is the one place
that can see what the database is actually doing — which statements run, how long
they take, how saturated the pool is. Without that, the operator of a service
built on DORM is blind between "the app is slow" and `pg_stat_activity`, with no
bridge in between. ADR 0007 made the hot path fast; this ADR makes it *visible*,
which is the other half of running it in production.

Three things are worth seeing, and only the library can see them cheaply:

1. **Each statement** — its SQL shape, its latency, its row count, and whether it
   ran inside a transaction. This is the raw material of every "why is this
   endpoint slow" investigation.
2. **The slow ones, loudly.** A statement over a threshold is a signal that
   should reach a default log without anyone first turning the per-statement
   stream all the way up.
3. **The pool.** `max_size`, how many connections exist, how many are idle, and
   how many callers are queued waiting — the gauge that says whether the pool is
   the bottleneck.

The constraint that shapes all of it: **this must cost nothing when no one is
listening, and must never log a secret.**

## Decision

### `tracing` as an always-on direct dependency, not a feature

`tracing` is a facade. With no subscriber installed, every `tracing::debug!` /
`warn!` compiles down to a check of a global max-level (a relaxed atomic load)
and an early return — no formatting, no allocation, no event. It already arrives
as a transitive dependency of `tokio-postgres`, so naming it directly adds
nothing to the build graph; it just lets `db.rs` emit its own spans.

So there is no feature gate. ADR 0004 gated TLS behind a feature because TLS
pulls a real, heavy dependency tree (`openssl`/`rustls`) that a user who does not
want TLS should not pay to compile. Observability is the opposite case: the
dependency is already present and its disabled cost is a branch. A
`feature = "tracing"` here would buy nothing and would force every downstream and
every binary to remember to turn it on — the likely outcome being that in
production, where it matters most, someone forgot. Always on, free when silent,
is the correct default for a facade.

### One event per statement, at `debug`

`query` and `execute` now time the whole call — prepare-or-cache-hit plus
Bind/Execute, the latency the caller actually pays — and hand the result to a
single `observe(...)` helper. It emits on target `dorm::db`:

- `op` — `"query"` or `"execute"`,
- `sql` — the statement text, summarised (see below),
- `duration_ms` — wall-clock milliseconds,
- `rows` — rows returned (query) or affected (execute), or `None` on failure,
- `in_tx` — whether a transaction was in scope.

At `debug`, because one line per statement is detail an operator opts into for an
investigation, not noise a default `info` subscriber should carry.

### The slow-query log, opt-in by threshold

`observe` also reads a threshold once from `DELONIX_ORM_SLOW_QUERY_MS` and, when
a statement's elapsed time meets or exceeds it, emits a **separate `warn!`** —
separate so a slow query surfaces on a default subscriber without anyone enabling
the `debug` per-statement stream. The `warn` carries the same fields plus
`threshold_ms`.

**The default is off.** An absent or zero `DELONIX_ORM_SLOW_QUERY_MS` disables
the slow-query log entirely. Production opts *into* this signal with a number
that means something for its workload; it is never surprised by warnings it did
not ask for, and a machine with no configuration behaves exactly as before. The
threshold is resolved once and cached (a `OnceLock`), matching how the pool
sizing is read: it is launch configuration, not a per-call knob, and keeping the
env lookup off the hot path preserves ADR 0007's win.

### Pool metrics via an owned, serializable snapshot

`db::pool_stats() -> Result<PoolStats>` returns deadpool's `Status`
(`max_size`, `size`, `available`, `waiting`) copied into `PoolStats`, a struct
DORM owns that derives `serde::{Serialize, Deserialize}`. It is a plain snapshot:
the numbers are already stale by the time the caller reads them, which is all a
pool gauge ever is, and it is cheap enough to call per metrics scrape or health
check. It errors only when no pool can be built — i.e. no DSN is configured.

Returning our own type rather than re-exporting `deadpool_postgres::Status` keeps
the pool implementation out of the public API: a caller can serialize the
snapshot into a `/metrics` endpoint without taking a deadpool dependency, and we
can change pools underneath without breaking them (ADR 0003's seam, extended to
observability).

### The security invariant: SQL text yes, parameter values never

The one rule the whole design keeps: **a log line carries the SQL text and
metadata, never a bound parameter value.** The SQL already holds `$n`
placeholders, not literals — the compiler puts every value behind a bind
parameter (ADR 0005), so the text is safe to log by construction. The values are
exactly what is most likely to be sensitive: a password being hashed, an email, a
token. `observe` is handed the SQL and a row count and is *never* handed
`params`, so there is no path by which a value can reach a log, by accident or
otherwise. The live test asserts this directly: it binds `0.2` as `$1` and then
checks that `"0.2"` appears in no captured message or field.

Long SQL is truncated to 300 bytes on a UTF-8 boundary, tagged with the original
byte length, so a 50-line generated statement does not flood the log while a
short one is shown whole.

## The proof

`observability_live.rs` installs a capturing `tracing` subscriber — a small
`Layer` that records every event's level, target, message and fields into a
shared `Vec` — then, under `DELONIX_ORM_SLOW_QUERY_MS=50`, runs
`SELECT pg_sleep($1)` with `$1 = 0.2` (200 ms against a 50 ms threshold). It
asserts:

- a per-statement `debug` event fired on `dorm::db` (`"statement finished"`);
- a `warn` fired on `dorm::db` (`"slow query"`), at level `WARN`, whose `sql`
  field contains `pg_sleep` **and** the `$1` placeholder;
- **no** captured message or field anywhere contains `"0.2"` — the bound value
  never leaked;
- `db::pool_stats()` reports `max_size > 0` and `size >= 1` after a real
  checkout.

Offline, unit tests pin `summarize_sql` (short text passes through un-allocated;
over-long text truncates on a char boundary without panicking on a multi-byte
split, and is tagged with the byte length) and the `PoolStats` serde shape
(stable, metrics-friendly field names, round-trips).

## What this is not

- **Not SQLite instrumentation.** The events sit at `db::query`/`db::execute`,
  which are the PostgreSQL-native entry points (ADR 0003). SQLite runs its own
  `rusqlite` path under `spawn_blocking` and is not observed here. Named rather
  than hidden; a later brick can add it symmetrically if SQLite graduates from
  development backend to a production one.
- **Not a metrics exporter.** `pool_stats()` is a snapshot a caller can serialize
  into whatever it already runs (Prometheus, a health endpoint, a log line). DORM
  does not open a port, register a global registry, or depend on any metrics
  crate — that is the embedding application's choice, not a library's to make.
- **Not a subscriber.** The library emits events; it never installs or configures
  a `tracing` subscriber. Choosing the subscriber, the format, the levels and the
  sinks belongs to the binary at the top of the process, not to a crate in its
  dependency tree. The test installs one because a test is an application.
- **Not parameter-level logging, ever.** There is deliberately no option to log
  bound values. A caller who needs to see a specific value can log it themselves,
  in their own code, where they own the decision and its trust boundary.

## Consequences

- An operator of a service on DORM can turn up `dorm::db` to `debug` and see every
  statement with its timing, or leave it at `warn` and catch only the slow ones —
  without patching the library or adding a second connection outside it.
- The slow-query threshold is one environment variable, consistent with the pool
  knobs (`DELONIX_ORM_POOL_MAX`, `DELONIX_ORM_POOL_ACQUIRE_TIMEOUT_MS`), and off
  by default so nothing changes for a process that does not set it.
- `pool_stats()` makes pool saturation observable before it becomes a timeout,
  with no new dependency forced on the caller.
- A process that installs no subscriber pays the facade's disabled-path cost —
  effectively nothing — so ADR 0007's hot-path win stands.
- The security posture is explicit and tested: SQL text and metadata are logged;
  parameter values are not, and a test fails if that ever regresses.

## Reproducing

```bash
createdb dorm_obs
# The slow-query warn and the pool snapshot, witnessed by a capturing subscriber:
DELONIX_ORM_DSN="host=/var/run/postgresql user=$USER dbname=dorm_obs" \
  cargo test -p dorm-core --test observability_live -- --nocapture
# The offline units (summarize_sql, PoolStats serde):
cargo test -p dorm-core --lib db::tests
dropdb dorm_obs
```

The live test no-ops cleanly when `DELONIX_ORM_DSN` (or `PAAS_DATABASE_URL`) is
unset, so a machine with no database skips rather than fails.

To see the events against a real workload, install any subscriber and raise the
target:

```rust
tracing_subscriber::fmt()
    .with_env_filter("dorm::db=debug")
    .init();
```

```bash
RUST_LOG=dorm::db=debug DELONIX_ORM_SLOW_QUERY_MS=100 ./your-app
```
