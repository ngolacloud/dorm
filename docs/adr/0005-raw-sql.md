# ADR 0005 — The raw SQL escape hatch

## Status

Accepted. Library brick (this ADR): `Manager::raw`, `db::sql`, `db::sql_execute`.
The `dorm db execute` CLI command and the dbshell `:raw` verb are later bricks
that consume this API (ADR 0006 for the shell).

## Context

DORM's compiler is deliberately the only place SQL is built, which is what makes
injection impossible by construction. But a compiler is never complete: CTEs,
window functions, `DISTINCT ON`, planner hints, `LATERAL` joins, engine-specific
functions — a mature project meets all of these. Without a sanctioned escape
hatch, the author of such a query is pushed *outside* DORM entirely, to a second
connection with none of DORM's rules. That is the worst outcome: the one query
that most needs care is the one that leaves the safe path.

So the escape hatch is a feature, not a concession — provided it keeps the one
invariant that actually matters: **no value is ever formatted into SQL**.

## Decision

Three entry points, each parameterized, none of which interpolates a value:

- **`Manager::<T>::raw(sql, params) -> Vec<T>`** — Django's `Model.objects.raw`.
  Runs hand-written SQL and maps each row back to the model with `T::from_row`,
  so the result is the same typed struct the rest of the API returns. The SQL
  must `SELECT` the model's columns, in the model's field order (`from_row` reads
  by position) — documented, the caller's contract.
- **`db::sql(sql, params) -> Vec<Row>`** — an untyped read, for a shape no model
  describes (an aggregate, an ad-hoc projection). Returns backend-agnostic rows.
- **`db::sql_execute(sql, params) -> u64`** — a raw write (DML), returning rows
  affected. It calls `guard_write` first, so it is **refused under
  `set_read_only(true)`** exactly like every compiled write, with a named
  `Error::ReadOnly`.

In all three, `params` is a `&[Value]`, and every element travels to the server
as a bound `$n` (PostgreSQL) / `?n` (SQLite). The text between the placeholders
is the caller's: DORM does not parse it, does not quote into it, and makes no
claim about what it does beyond running it. That is the honest division — "you
own the SQL; DORM owns the parameter binding and the connection."

Raw statements route through the same `db` layer as everything else, so they
observe the transaction in scope (`atomic`), the connection pool, the statement
timeout and TLS from ADR 0004, and the read-only guard — nothing about taking
the escape hatch steps outside those guarantees.

## What this is not

- **Not a second way to name identifiers unsafely.** The compiler still refuses
  a caller-supplied table or column name everywhere it builds SQL. `raw` does not
  *resolve* names — it runs text the developer wrote in their own source, the
  same trust boundary as the Rust code around it. A string that reaches `raw`
  from an end user is the caller's injection to own, and the docs say so.
- **Not typed at compile time.** The SQL is opaque to the compiler; a column that
  does not match the model surfaces at `from_row`, at runtime. That is the price
  of the escape hatch, named rather than hidden.

## Consequences

- A query the compiler cannot express stays inside DORM's pool, transactions,
  timeout, TLS and read-only rules instead of forcing a side channel.
- The read-only guard now covers raw writes too, so a read-only session is
  genuinely read-only — not "read-only unless you drop to raw."
- Next bricks: `dorm db execute` (the CLI surface, already `planned` in
  cli-contract.md) and the shell `:raw` verb (ADR 0006), both thin layers over
  these three functions.
