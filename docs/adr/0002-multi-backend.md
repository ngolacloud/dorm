# ADR 0002 — multi-backend by dialect and driver

Status: accepted · 2026-10-28 · supersedes nothing; extends the roadmap's
"multi-SGBD by tiers".

## Context

DORM today speaks one backend: PostgreSQL, hardwired through `dorm-core`
(`db` uses `tokio-postgres`; `compiler` emits `$n` placeholders, `ILIKE`,
`BIGSERIAL`, `TIMESTAMPTZ`; `dorm-cli doctor`/`db` read `information_schema`).
The ambition is a tiered multi-backend ORM — PostgreSQL and **SQLite** first,
then MySQL/MariaDB, then SQL Server/Oracle — *without* pretending that protocol
compatibility is behavioural equivalence.

## Decision

Separate the **portable core** from each backend's **dialect** and **driver**,
in three seams, and advance one seam at a time so the proven PostgreSQL path is
never destabilised:

1. **Dialect — SQL generation.** DDL and query SQL become parameterised by a
   `Backend`. Placeholders (`$n` vs `?`), identifier quoting, type names
   (`BIGSERIAL`/`TIMESTAMPTZ`/`JSONB` vs `INTEGER … AUTOINCREMENT`/`TEXT`),
   pattern matching (`ILIKE` vs `LIKE`), and `RETURNING` support differ here.
2. **Driver — connection and execution.** `dorm-core::db` abstracts over a
   connection/pool trait; `tokio-postgres` is one impl, a SQLite driver
   (`rusqlite` on `spawn_blocking`, or `sqlx`) another. A `Value` maps to each
   driver's parameter and result types.
3. **Introspection — catalogue reads.** `doctor`/`db`/`sync` read the live
   schema per backend (`information_schema` vs `sqlite_master`/`pragma`).

The API stays common; the **execution plan and the guarantees reflect the real
backend**. Unsupported capabilities return an explicit error, never a wrong
result.

## Bricks (the order of work)

- **Brick 1 — SQLite DDL dialect (this ADR's first increment, shipped).**
  `dorm-migrations` gains `Backend { Postgres, Sqlite }` and
  `schema_for_on(models, backend)` / `create_table_on(…)`, generating
  SQLite-flavoured DDL from the same `&ModelMeta`. Proven by creating a real
  SQLite database with `rusqlite`, applying the schema, and round-tripping an
  insert/select. **Additive** — the existing `schema_for`/`create_table` stay
  PostgreSQL, so nothing already published changes behaviour.
- **Brick 2 — the driver trait in `dorm-core::db`.** Abstract the pool and
  `query`/`execute` over a backend; add a SQLite driver; make the compiler's
  placeholders and `ILIKE`/`LIKE` dialect-aware. This is where SQLite becomes a
  *queryable* backend through the typed API and the REPL.
- **Brick 3 — introspection + CLI.** `doctor`/`db`/`sync` per backend; a
  `datasource { provider = "sqlite" url = "file:app.db" }` drives the dialect.
- **Brick 4 — the per-version matrix** (Types · CRUD · Relations · Transactions ·
  Introspection · Migrations · Native features · Limitations · Tests) published
  before SQLite is called "supported". Protocol compatibility is not equivalence.

## SQLite type and DDL mapping (Brick 1)

| `FieldType` | PostgreSQL | SQLite |
|---|---|---|
| I32 / I64 | INTEGER / BIGINT | INTEGER |
| serial PK (i64, no default) | BIGSERIAL | INTEGER PRIMARY KEY AUTOINCREMENT |
| Text | TEXT | TEXT |
| Bool | BOOLEAN | INTEGER |
| Decimal | NUMERIC | NUMERIC |
| Timestamp | TIMESTAMPTZ | TEXT |
| Uuid | UUID | TEXT |
| Json | JSONB | TEXT |
| `now()` default | `now()` | `CURRENT_TIMESTAMP` |

`UNIQUE`, `CHECK`, `REFERENCES … ON DELETE`, and `CREATE INDEX IF NOT EXISTS`
carry over; SQLite enforces foreign keys only with `PRAGMA foreign_keys = ON`,
which the driver (Brick 2) will set per connection.

## Consequences

- SQLite users can already generate a correct schema from their models (Brick 1).
- Querying SQLite through the ORM waits for Brick 2 — the honest status in the
  [roadmap](../roadmap.md) and the feature matrix.
- The PostgreSQL path is untouched: every change is additive until Brick 2, and
  Brick 2 keeps PostgreSQL as the default `Backend`.
