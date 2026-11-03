# ADR 0011 — Multi-backend expansion and production-hardening roadmap

## Status

Accepted as the roadmap. Each phase lands under its own ADR and branch with live
proof; this ADR is the map and the sequencing contract, not the implementation.

## Context

DORM today is production-strong on a narrow target — PostgreSQL (and SQLite for
tests/embedded), single-tenant, with the schema under the team's control. An
honest comparison against Django's ORM and Prisma puts us ahead on
safety-by-construction, native read-replica routing (ADR 0010), built-in
observability (ADR 0008), and transaction guarantees — and behind on three
production blockers and on backend breadth.

The owner's goal: close the production gaps and add **MySQL, SQL Server,
CockroachDB, Oracle, and MongoDB**, validating each against a database stood up
locally with **delonix** (the workspace's container runtime; Docker/Podman are
absent here).

The branching surface that makes this tractable is small and concentrated:
`compiler.rs` branches on a `Dialect` enum in ~4 real places (ILIKE/LIKE, the
`$n`/`?n` placeholder, `now()`, the placeholder marker); `db.rs` has 6
`match backend()` seams; rows are an `enum Row { Pg, Sqlite }`. The exception is
MongoDB, which is not SQL and breaks the `Plan → Sql{text,params}` model
outright.

## Decision

### Ordering principle

Foundation before fan-out. Without a backend abstraction, each new backend means
editing ~30 scattered `match` arms. So: (0) abstraction + test harness, (1)
backend-agnostic production blockers, then (2) the SQL backends cheapest-first,
then (3) MongoDB behind a decision ADR.

### Phase 0 — Backend abstraction + delonix compatibility harness

- **`Dialect` becomes behaviour, not scattered matches.** The dialect-specific
  choices move onto the type (placeholder style `$n`/`?`/`?n`/`:n`/`@pN`,
  identifier quoting `"x"`/`` `x` ``/`[x]`, `ILIKE`, `now()`, LIMIT/OFFSET vs
  `OFFSET…FETCH`/`TOP`/`ROWNUM`, RETURNING vs `OUTPUT` vs `last_insert_id`,
  upsert `ON CONFLICT`/`ON DUPLICATE KEY`/`MERGE`). Enum-dispatch over a closed
  set keeps it `Copy` and offline-testable; adding a backend becomes adding arms
  in **one** place.
- **`Driver` abstracts the runtime seam** (`fetch`/`fetch_read`/`run`/`run_ddl`/
  `begin`) that `db.rs` does with `match backend()`. `Database` holds the driver.
- **Capability flags.** Not every backend has RETURNING, savepoints,
  DDL-in-transaction, `CREATE INDEX CONCURRENTLY`, or a native boolean. Each
  backend declares what it supports; the core **degrades or refuses with a named
  error** — it never emits silently-wrong SQL. (Same capability-matrix
  discipline the delonix runtime uses.)
- **delonix harness.** `make compat-<backend>` stands the database up with
  `delonix container run` / `delonix compose`, waits for ready, and runs the
  `#[ignore]` live suite gated on `DORM_COMPAT_<BACKEND>_DSN`.

Done when PG and SQLite are re-expressed through the abstraction with **zero
behaviour change** (`make check` green), the capability matrix prints, and
`make compat-postgres` is green through delonix.

### Phase 1 — Production blockers (backend-agnostic, parallel with Phase 0)

| # | Theme | Done when | Severity |
|---|---|---|---|
| 1.1 | Reversible migrations | `migrate down`/`--to`, each autodetect op emits its inverse, squash, data-migration hook | High |
| 1.2 | `prefetch_related` | reverse-FK/M2M loaded in one extra query per relation (no N+1); query-count test | High |
| 1.3 | Connection resilience | retry+backoff on transient failure, health-check on recycle, live kill-and-recover test | Medium |
| 1.4 | Wider `Value` | JSON/JSONB, arrays, UUID, enums, decimal, bytes, composite keys; per-backend round-trip | Medium |
| 1.5 | SQLite pool | reader pool + single writer (WAL) instead of one shared connection | Low |

### Phase 2 — SQL backends, cheapest-first

| Order | Backend | delonix image | Difficulty | Key differences |
|---|---|---|---|---|
| 2.1 | **CockroachDB** | `cockroachdb/cockroach` | Low | PostgreSQL wire-compatible → reuses the PG dialect almost verbatim; **proves the abstraction cheaply**. Savepoint semantics, `unique_rowid()` serials, `AS OF SYSTEM TIME` |
| 2.2 | MySQL/MariaDB | `mysql:8` | Medium | `?` placeholders, backtick quoting, **no RETURNING** (`last_insert_id`), `ON DUPLICATE KEY UPDATE`, `TINYINT(1)` bool, non-transactional DDL |
| 2.3 | SQL Server | `mcr.microsoft.com/mssql/server:2022` | Medium-High | `@pN` binds, `[bracket]` quoting, `OUTPUT`, `OFFSET…FETCH`, `TOP`, `BIT` bool, `MERGE` upsert |
| 2.4 | Oracle | `container-registry.oracle.com/database/free` (23ai) | High | `:n` binds, no native bool (`NUMBER(1)`), sequences/`IDENTITY`, `FETCH FIRST`/`ROWNUM`, `MERGE`, 30-char identifier history, DATE/TIMESTAMP quirks, image licensing |

Per backend, done when the same conformance matrix passes live against the
delonix-spun container: typed CRUD, `filter`/`exclude`, aggregates,
`select_related`+`prefetch_related`, transactions+savepoints (or a declared
degradation), `bulk_create`, forward+down migrations, and the `Value` types.

### Phase 3 — MongoDB, behind a decision ADR

MongoDB is **not SQL**: the `Plan → Sql{text,params}` model does not apply; it
needs a parallel terminal `Plan → {BSON filter | aggregation pipeline | update
document}`, and JOIN/`select_related`/DDL do not map 1:1. So Phase 3 opens with
a **spike + decision ADR**: should a SQL ORM include a document store, and with
what subset — (a) first-class backend with its own BSON compiler, (b) a
restricted CRUD+simple-filter subset, or (c) not at all, documented. Only on
"yes" does a `DocumentBackend` trait and a `mongo:7` harness follow.

## Sequencing

```
Phase 0 (abstraction + delonix harness) ┐
Phase 1 (blockers, in parallel) ────────┴─▶ 2.1 Cockroach ─▶ 2.2 MySQL ─▶ 2.3 MSSQL ─▶ 2.4 Oracle ─▶ 3 Mongo (spike+ADR)
```

Each box is one or more dedicated sessions on its own `feat/dorm-*` branch, with
an ADR and live proof; integration/merge is the owner's.

## Consequences

- Adding a SQL backend becomes "implement the dialect arms + driver + type
  mapping + capability declaration", localised, not a scatter-edit.
- The capability system makes partial support **honest**: a backend without
  RETURNING degrades to a documented second round trip or refuses by name — the
  matrix never claims more than is true.
- MongoDB is explicitly *not* committed here; the spike decides it, so the SQL
  work is never held hostage to the document-store paradigm.
