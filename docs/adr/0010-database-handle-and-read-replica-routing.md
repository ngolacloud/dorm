# ADR 0010 — The `Database` handle, read-replica routing, and the multi-tenant seam

## Status

Accepted. Implements the handle, the primary/replica split, and the session
scope that carries a database down to the `db::*` calls. Full multi-tenancy
(per-tenant pools built from a registry, schema/search-path switching) is a
follow-up that *builds on* this seam rather than changing it — see
"Multi-tenancy" below. This ADR supersedes the implicit single-process pool that
ADRs 0003/0004/0007 assumed.

## Context

Until now `dorm-core::db` owned exactly one database: a `static POOL:
OnceLock<Pool>` built from the process environment, plus a `static READ_ONLY`
flag. Every `db::query`/`db::execute`/`db::atomic` meant *that* pool, implicitly.
That is the right shape for the embedded single-database case — the CLI, the
shell, a service that talks to one database — and it must stay effortless there.

But it blocked two things a high-criticality deployment needs:

1. **Read replicas.** Reads vastly outnumber writes in the ORM's access pattern.
   A deployment wants to send them to one or more replicas and keep the primary
   for writes — impossible when there is one global pool and no notion of a
   statement's direction.
2. **Multi-tenancy.** One process serving many tenants wants a database (or at
   least a pool/schema) *per tenant*, chosen per request. A process-global pool
   makes "which database" a launch-time constant, not a per-request decision.

Both reduce to the same root: **the database was ambient global state, not a
value.** This is the last global the correctness campaign had left standing
(DESIGN-01 / H1).

## Decision

### A database is a value: `Database`

```rust
pub struct Database {
    primary: Pool,
    replica: Option<Pool>,
    dsn: String,
    read_only: bool,
}
```

A process may hold as many as it needs. `Database::connect(primary, replica,
read_only)` builds one from explicit DSNs and needs **no** process environment —
which is what frees the module from *requiring* a global pool. The former body
of `pool()` is lifted into `build_pool`, so a primary and a replica are built
the same way (same TLS, statement timeout, and sizing from ADR 0004/0007).

### The database reaches `db::*` through a task-local session

Threading a `&Database` through every `QuerySet` method is impossible for the
same reason the current transaction cannot be a parameter (ADR 0003, the module
note in `db.rs`): `app.delete()` was compiled with no handle to pass. So the
database rides the **same task-local shape** the transaction scope and the
read-only scope already use:

```rust
pub async fn with_session<F, T>(db: Arc<Database>, fut: F) -> T
```

Every statement consults the session in scope and falls back to a lazily-built
process-default (`DEFAULT_DB`, from the environment) when there is none. The
model API does not change at all: a server wraps a unit of work in
`with_session`; everything inside routes to that database. A spawned task starts
outside the scope and falls back to the default — a spawned task is a new
session, exactly as it is for transactions and read-only.

### Direction is classified by the compiler, not guessed from SQL

Routing a read to a replica needs to know a statement only reads. The naïve
signal — "`fetch` returns rows, so it is a read" — is **wrong**: `INSERT …
RETURNING` returns rows and is a write, and routing it to a read-only replica
would fail. Parsing the SQL text for a leading `SELECT` is fragile.

So the **compiler** — the one place that knows for certain — stamps every
compiled statement with a `StatementKind`:

```rust
pub enum StatementKind { Read, Write }
```

`compile_select`/`count`/`exists`/`aggregate` are `Read`; `insert`/`update`/
`delete` are `Write`. The typed read funnel (`model::run_query`) routes on it:
a `Read` may go to the replica (`db::fetch_read`), a `Write`-that-returns-rows
stays on the primary (`db::fetch`).

### What routes where

| Statement | Route |
|---|---|
| Compiled `Read` (SELECT/COUNT/EXISTS/aggregate), outside a transaction | **Replica** when the active `Database` has one, else primary |
| Compiled `Write` (INSERT/UPDATE/DELETE, incl. `… RETURNING`) | **Primary** |
| Anything inside `atomic`/an interactive transaction | **Primary** (the transaction holds one connection and must read its own uncommitted writes) |
| Raw `db::sql` / `db::query` / DDL | **Primary** (the escape hatch is predictable: DORM routes only the reads it generated and understands) |
| m2m lookups (`exists`/`count`/`keys`) | **Replica**-eligible (they are reads) |

A `Route::ReadReplica` resolves to the replica only when one is configured **and
no transaction is open** — enforced in one place, `Database::pool_for`.

### Read-only precedence

Read-only now has three sources, most specific winning: the per-task scope
(`with_read_only`, B-12) overrides the session's `Database.read_only`, which
overrides the process-wide `set_read_only` flag.

## Multi-tenancy

This ADR deliberately ships the *seam*, not a tenant registry. A `Database` is
already the unit a tenant needs; a server builds one per tenant (or looks one up
in a cache) and runs the request under `with_session(tenant_db, …)`. The pieces
a fuller multi-tenancy story (the proposed ADR for tenant isolation) will add —
a per-tenant pool cache, `search_path`/schema selection, a tenant key on the
handle — hang off `Database` and `with_session` without disturbing the `db::*`
calls or the model API. Building those is out of scope here; not boxing them out
is the point.

## Consequences

- **The last process global is gone as a *requirement*.** `DEFAULT_DB` remains
  as a convenience for the embedded single-database path; a server can ignore it
  entirely and run purely through explicit handles. `backend()`/`dialect()` now
  consult the active session's DSN, so a pure-session process needs no env DSN.
- **Reads scale horizontally** behind `DELONIX_ORM_REPLICA_DSN` (for the
  default) or an explicit replica DSN (per handle), with no change to model code.
- **Replica staleness is the caller's opt-in.** Configuring a replica accepts
  that a read outside a transaction may be served slightly stale — the standard
  read-replica contract. Read-your-writes within a transaction is preserved
  because transactions pin to the primary.
- **`pool()` is now the *default's* primary pool**, kept for the pool-stats
  gauge; statements no longer go through it, they go through `acquire(route)`,
  which honours the session.

## Proof

`crates/dorm-core/tests/read_replica_routing_live.rs` —
`read_queries_route_to_replica`. Two separate databases stand in for a
primary/replica pair, each seeded with its own sentinel (there is no real
streaming replica, which makes routing *visible*: a read that returns the
replica's sentinel could only have run on the replica). It proves, against live
PostgreSQL, that a read is served by the replica, a plain query and every write
by the primary, and a read inside a transaction by the primary. The existing
`dorm-models` typed end-to-end suite (15/15) continues to pass unchanged, with
no replica configured — the fallback path.
