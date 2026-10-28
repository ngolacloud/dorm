# ADR 0003 — a backend-generic `dorm-core::db`

Status: accepted (design); implementation pending a dedicated effort · 2026-10-28

## Context

Brick 2 of [ADR 0002](0002-multi-backend.md) made the **compiler** dialect-aware
and shipped `dorm-sqlite`, which executes SQLite-dialect `Sql` and decodes rows
— full CRUD proven. What remains is **execution through the typed `QuerySet`'s
async terminals**: today `Tenant::objects().all().await` and friends call
`dorm_core::db::{query, execute}`, which are hardwired to a `tokio-postgres`
pool, and `Model::from_row` decodes a `tokio_postgres::Row`. SQLite cannot
produce that row type, and SQLite's driver (`rusqlite`) is synchronous.

This ADR fixes the design so the implementation — which touches the *published*
async engine — can be done deliberately, in its own effort, without
destabilising the proven PostgreSQL path.

## Decision

Introduce a **driver seam** in `dorm-core::db`, chosen by the DSN, with
PostgreSQL as the default and SQLite behind a cargo feature.

1. **Backend selection from the DSN.** `postgres(ql)://…` / libpq keyword form →
   Postgres; `sqlite:…` / `file:…` / `:memory:` → SQLite. A process talks to one
   backend, resolved once (mirroring today's single global pool).
2. **A `Row` abstraction.** `Model::from_row` and `value::read_column` take an
   abstract row (an enum `Row { Pg(tokio_postgres::Row), Sqlite(…) }`, or a
   trait) so decoding is backend-agnostic. This is the central change and the
   reason the work is its own effort — it touches every decode site.
3. **`query`/`execute` dispatch.** Keep the `async` signature. Postgres runs on
   `tokio-postgres` as today. SQLite runs `rusqlite` on `spawn_blocking` over a
   small connection pool, reusing `dorm-sqlite`'s bind/decode (lifted into core
   behind `#[cfg(feature = "sqlite")]`, since `dorm-sqlite` depends on core and
   cannot be depended on back — no cycle).
4. **Dialect flows from the backend.** The `QuerySet` terminals compile with
   `compile_*_on(plan, db::dialect())` instead of the Postgres-only `compile_*`.
5. **Defaults and features.** `default = ["postgres"]`. SQLite is opt-in
   (`features = ["sqlite"]`) so a Postgres-only user pulls neither `rusqlite` nor
   the bundled SQLite. PostgreSQL behaviour and every snapshot stay identical.

## Why not now

It rewrites the decode path of the published query engine and adds a sync driver
under async — a change whose blast radius is the whole of `dorm-core`. Rushed at
the end of a long session it would risk the five published crates. The honest
first step is this committed design; the implementation is the next focused
effort, gated on all existing PostgreSQL tests staying green and the SQLite path
proven through `QuerySet::{all, rows, get, count, update, delete}`.

## Consequences

- After implementation, `Tenant::objects().all().await` works on SQLite with no
  code change — the backend is the DSN's.
- `dorm-sqlite` stays useful as the low-level, no-feature executor; core's SQLite
  path reuses its logic.
- The per-version capability matrix (ADR 0002, brick 4) is published once the
  async path is proven per backend — not before.
