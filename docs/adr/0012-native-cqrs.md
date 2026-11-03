# ADR 0012 — Native CQRS: commands write, queries read, enforced

## Status

Accepted and implemented. Builds directly on the statement-kind classification
and primary/replica routing of ADR 0010 and the per-task read-only scope of B-12.

## Context

CQRS — Command Query Responsibility Segregation — splits an application's writes
(commands) from its reads (queries), so the two can scale, be reasoned about, and
be routed independently. Most frameworks leave it as a *convention*: nothing
stops a "query" from issuing an `UPDATE`, and nothing routes a command to the
primary and a query to a replica without the application wiring it by hand.

DORM already had the two halves of the machinery, from ADR 0010:

- every compiled statement is classified `Read` or `Write` by the compiler, and
  reads route to a replica while writes stay on the primary;
- a session can be made read-only per task (B-12), where any write is refused
  with a named `Error::ReadOnly` before it reaches SQL.

So CQRS in DORM need not be a new subsystem — it is the *application-level shape*
of routing primitives that already exist, made first-class and, crucially,
**enforced**.

## Decision

A small `cqrs` module with two traits and four entry points.

```rust
pub trait Command { type Output; async fn execute(self) -> Result<Self::Output>; }
pub trait Query   { type Output; async fn fetch(self)   -> Result<Self::Output>; }

pub async fn dispatch<C: Command>(cmd: C) -> Result<C::Output>; // typed command
pub async fn ask<Q: Query>(q: Q)           -> Result<Q::Output>; // typed query
pub async fn command<T>(f: impl FnOnce() -> impl Future<Output = Result<T>>) -> Result<T>;
pub async fn query<T>(f:   impl FnOnce() -> impl Future<Output = Result<T>>) -> Result<T>;
```

- **A command runs on the primary, inside a transaction.** `dispatch`/`command`
  wrap the work in `db::atomic`, so a multi-statement command is all-or-nothing
  and reads its own writes; being a transaction, it is pinned to the primary.
- **A query runs read-only and replica-eligible.** `ask`/`query` wrap the work in
  `db::with_read_only(true, …)`, so the handler **physically cannot write** — a
  stray `INSERT`, even through the raw `sql_execute` escape hatch, is refused with
  `Error::ReadOnly` — and the reads DORM compiles route to a replica when the
  active `Database` has one.

The guarantee is the point: the segregation is enforced by the engine, not
promised by discipline. The closure forms (`command`/`query`) exist for the
common case where defining a `Command`/`Query` type is more ceremony than the
operation deserves.

### Why read-only, not just "prefer a replica"

A replica is already read-only at the server, but a command-free process can run
without a replica and still want the guarantee. `with_read_only(true)` gives it
unconditionally: the query side cannot mutate *any* backend, replica or not. The
replica routing is an optimisation on top; the read-only refusal is the contract.

### Multi-store CQRS falls out for free

Because routing follows the `Database` in scope (ADR 0010), a process can run a
command against a write store and a query against a separate read store by
wrapping each in its own `with_session` — no new API. The command/query split and
the database split compose.

## Consequences

- Application code reads as CQRS — `cqrs::dispatch(TransferMoney{..})`,
  `cqrs::ask(AccountStatement{..})` — with the write/read guarantees carried by
  the framework, not re-implemented per handler.
- A query that tries to write is a *caught bug*, not a production incident: it
  fails in tests the first time it runs, by name.
- Commands are atomic by default. A command that must not run in a transaction
  (e.g. certain DDL on backends where DDL auto-commits — see the capability
  system in ADR 0011) is a future opt-out, not today's default.
- The model API is unchanged; `cqrs` is an additive layer over `db`.

## Proof

- `crates/dorm-core/tests/cqrs_routing_live.rs` —
  `commands_hit_the_primary_and_queries_hit_the_replica`, live against
  PostgreSQL with two databases standing in for a primary/replica pair: a query's
  read is served by the replica, a query that tries to write is refused with
  `Error::ReadOnly`, and a command's write lands on the primary (and only the
  primary).
- The read-only guarantee and command atomicity are backend-agnostic; the same
  properties hold on SQLite (feature-gated lane).
