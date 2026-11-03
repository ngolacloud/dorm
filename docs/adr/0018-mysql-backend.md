# ADR 0018 — The MySQL backend (plan)

## Status

**Draft / proposed.** This is the implementation plan for Phase 2.2 of ADR 0011,
synthesised from three independent design passes (runtime architecture, compiler
semantics, verification harness). It is the contract the implementation will prove
against; each "not supported" below is **declared**, in the honest-capability
spirit of ADR 0011 — never hidden.

## Context

DORM supports PostgreSQL (and the PG-wire CockroachDB) and SQLite. MySQL is the
first **non-PG-wire** backend: a different driver, different parameter binding
(positional `?`), no `RETURNING`, non-transactional DDL, and `TINYINT(1)` for
booleans. The Phase-0 foundation is already in place — the centralised `Dialect`
(ADR 0011), the `trait Driver` seam, the `Capability` matrix — and a `Dialect::MySql`
*profile* was committed ahead of the driver (`cc37185`).

## Decisions

| Question | Decision | Rationale |
|---|---|---|
| **Where does the pool live?** | **Option B** — a module-global `static MYSQL_POOL` in `mod mysql`; MySQL ignores the `Database` handle, per-session DSN and replica, exactly as SQLite does today | Purely additive, **zero risk to the PostgreSQL path**; the `trait Driver` + `fetch_routed` seam was designed for this. Option A (`Database` owns an `Arc<dyn Driver>` that holds the pool) is the Phase-0 end-state and its own session — conflating it with "add MySQL" multiplies the blast radius and risks the PG path. Recorded as explicit debt: MySQL, like SQLite, does not honour `with_session`/replica until the A refactor. |
| **Driver crate** | **`mysql_async`** | Async-tokio native (`Conn: Send`, no second runtime, no `spawn_blocking`), its own pool (no deadpool), statement cache. `sqlx-mysql`'s compile-time query checking is redundant with DORM's own compiler; the sync `mysql` crate would force `spawn_blocking`. |
| **Identifier quoting** | **`SET SESSION sql_mode='…,ANSI_QUOTES,…'`** per connection | Leaves `quote()` (double quotes) untouched, zero compiler churn, snapshots identical across dialects. Making `quote()` dialect-aware (backticks) would thread `Dialect` through ~20 call sites. DORM never emits double-quoted string literals, so ANSI_QUOTES is safe. |
| **Timezone / charset** | `SET time_zone='+00:00'` + `utf8mb4` per connection | UTC round-trip aligned with `chrono::DateTime<Utc>`. |

### Correction to the committed foundation

The committed `Dialect::MySql.placeholder(n)` returns a bare `"?"`. This is
**unsafe**: with bare `?`, the parameter vector must be in textual left-to-right
order, but a `SELECT` with `HAVING` binds the HAVING parameter *before* the WHERE
one (the number disambiguates today) — bare `?` would swap the values, a silently
wrong query. **Fix:** MySQL emits numbered `?n` through the whole compiler (like
SQLite), and a final pure pass `positionalize(text, params)` rewrites each `?k`→`?`
and reorders the parameter vector by textual appearance. Correct for any
reordering (WHERE/HAVING, subquery shift). Run once at the outermost level (split
`compile_*_core` numbered / `compile_*_on` = core + positionalize); a no-op for
PG/SQLite, so their snapshots are unchanged.

## Implementation sequence (landable increments)

- **Inc. 0 — Detection foundation.** `Backend::MySql`; `backend_of` recognises
  `mysql://`/`mariadb://`; `dialect()`→`MySql`; `ddl::Backend::MySql`. Revert
  `placeholder` to `?n`, add `positionalize` + `positional_unnumbered()`, rewrite
  the profile test, snapshot `positionalize` (incl. out-of-order WHERE/HAVING).
- **Inc. 1 — Runtime/driver** (feature `mysql`, `mysql_async`). `mod mysql`
  (`static MYSQL_POOL`, builder: ANSI_QUOTES + time_zone + utf8mb4 + sizing + TTL +
  acquire retry reusing `backoff_delay`), `Row::MySql`, `Decode::from_mysql` (the
  `impl_decode!` macro goes 3-way), `mysql_*` helpers, `value_to_mysql`,
  `MySqlDriver: Driver`, `Backend::MySql` dispatch arms, `mysql_disabled()`.
  Transactions: a task-local `MYSQL_TX` holding an owned `Conn` with raw
  `BEGIN`/`SAVEPOINT` + a `PanicGuard`, mirroring `atomic_pg`.
- **Inc. 2 — INSERT without RETURNING** (the central blocker, ~13 tests).
  `compile_insert_on` gated on `supports(Returning)`. A new Driver verb
  `insert(sql, params) -> (affected, last_insert_id)`. A single funnel
  `model::insert_returning`: with RETURNING → as today; without → `atomic { INSERT;
  SELECT … WHERE pk IN (ids) ORDER BY pk }` (ids = the contiguous auto-increment
  block from `last_insert_id`, or the input PKs). `bulk_create`'s single-chunk path
  now wraps in `atomic`. `is_unique_violation` learns MySQL error **1062**.
- **Inc. 3 — DDL / types** (`ddl.rs`). `Backend::MySql` everywhere. Map: `BIGINT
  AUTO_INCREMENT PRIMARY KEY` (serial), **`DECIMAL(38,9)`** (bare `DECIMAL` is
  `(10,0)` and truncates cents), `DATETIME(6)`, `CHAR(36)` (uuid), `JSON`,
  `LONGBLOB` (bytes), `TINYINT(1)` (bool), `DOUBLE` (f64), **`VARCHAR(n)` for TEXT
  when indexed/unique/PK** (MySQL rejects an index/UNIQUE on `TEXT`), **FK as a
  table-level constraint** (an inline column FK is ignored by InnoDB), default
  `CURRENT_TIMESTAMP(6)`, `IF [NOT] EXISTS` guards → `""` (unsupported).
  `mysql_column_alters` (`MODIFY COLUMN`) + a MySQL arm in `diff_table`.
- **Inc. 4 — Migrator** (`supports(DdlInTransaction)` gate). MySQL: a pinned
  connection for the run, **`GET_LOCK`/`RELEASE_LOCK`** (not an advisory lock), the
  ledger row **committed per version** (DDL auto-commits, so the outer `atomic` is
  inert), a 3-way execution placeholder for the ledger insert. Same for `revert`.
  Documented degradation: a mid-migration failure is not atomic → recommend
  single-statement migrations on MySQL.
- **Inc. 5 — Harness + conformance.** Generalise `typed_e2e` (its two PG couplings:
  `pg_advisory_xact_lock`→`GET_LOCK` per dialect; `schema_sql`→`schema_for_on`; the
  raw-SQL `$1`→`?` per backend). `make compat-mysql` (`mysql:8`, **double** readiness:
  `mysqladmin ping` **and** `SELECT 1`). A new `mysql_live.rs` (bool TINYINT,
  last_insert_id, consecutive ids, decimal, blob, json, savepoint, **utf8mb4
  case-insensitivity of `exact`**, migration+revert with the auto-commit caveat).

## Gotchas (the ones that bite)

- **ANSI_QUOTES is correctness.** Without it `WHERE "name" = ?` compares the string
  literal `'name'`. And `COM_RESET_CONNECTION` clears it — confirm `mysql_async`
  re-applies the `init` statements after a reset, or disable `reset_connection`.
- **Three silent DDL bugs:** bare `DECIMAL` truncates cents; `TEXT` is not indexable
  (use `VARCHAR(n)`); an inline FK is ignored (use a table-level constraint).
- **`last_insert_id` is per-connection** → the `create`/`bulk_create` reload must run
  on the same pinned connection (`atomic`).
- **DDL auto-commits** → migrations are not atomic; `GET_LOCK` + ledger-per-version.
- **Default collation `utf8mb4_0900_ai_ci`** makes `name.eq("api")` case-insensitive —
  decide: `COLLATE utf8mb4_bin` on columns needing binary equality, or declare the
  divergence (the `exact`-case-insensitivity test pins the choice).
- **`route` ignored / no replica / no `with_session`** on MySQL — parity with SQLite
  until the Option-A refactor.

## Definition of done

`make compat-mysql` green end to end (the 15 `typed_e2e` tests + `mysql_live`);
`make check` still green; every unsupported capability (RETURNING, transactional
DDL, native boolean, CONCURRENTLY) declared here, not hidden.

Honest follow-ups: MariaDB (has RETURNING — a per-server, not per-dialect,
capability); the `bulk_create` parameter ceiling per dialect; the Option-A refactor
(`Database` owns the driver) to give MySQL replica/session support.
