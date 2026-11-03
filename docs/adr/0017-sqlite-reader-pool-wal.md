# ADR 0017 — A SQLite reader pool under WAL

## Status

Accepted and implemented. The last Phase-1 item from ADR 0011 ("SQLite pool").

## Context

The SQLite backend used a single shared connection, with a logical lock
(`TX_LOCK`) serialising every statement so a transaction could never be observed
mid-flight. Correct, but every read waited behind every other statement — there
was no read concurrency at all. For an embedded or test backend that is often
fine; for one standing in for a small production database it is a throughput
ceiling the engine itself does not impose.

SQLite's own answer is **WAL** (write-ahead logging): with WAL, readers see the
last committed snapshot and do **not** block the writer, and the writer does not
block readers. The missing piece was more than one connection to take advantage
of it.

## Decision

A **reader pool** for file-backed databases, alongside the existing single
connection kept as the **writer**.

- **WAL on open.** A file database's writer connection is put in WAL journal mode
  (and given a `busy_timeout`). WAL is a persistent property of the file, so the
  writer setting it is enough — readers that open the same file are already in WAL.
- **A bounded reader pool.** Standalone reads (a `fetch` with no transaction in
  scope) check out a read-only connection from a pool — sized by the same
  `DELONIX_ORM_POOL_MAX` as PostgreSQL — run on it under `spawn_blocking`, and
  return it. Up to N reads run genuinely concurrently, no longer serialised on
  `TX_LOCK` or the one connection.
- **Writes and transactions are unchanged.** `run`, DDL, `atomic`, and the
  interactive transaction still use the single writer connection under `TX_LOCK`,
  exactly as before. A read **inside** a transaction also stays on that
  connection — it must see the transaction's own uncommitted writes, which a
  separate reader could not.
- **`:memory:` is unchanged.** An in-memory database has no file to share between
  connections and cannot use WAL, so it keeps the single-connection behaviour. The
  pool exists only when the DSN names a file.

This is the same primary/replica shape ADR 0010 gave PostgreSQL, one layer down:
the writer is the primary, the reader pool is the local replica, and a
transaction pins to the writer for read-your-writes.

## Consequences

- A file-backed SQLite database serves many reads at once while a write is in
  flight, instead of one statement at a time — a real throughput change for a
  read-heavy embedded workload.
- Correctness is preserved where it matters: write serialisation, transaction
  isolation, and read-your-writes inside a transaction are all exactly as before,
  because writes and in-transaction reads never left the single writer connection.
- `:memory:` — which the whole test suite uses — is byte-for-byte unaffected.

## Proof

`crates/dorm-core/tests/sqlite_pool.rs` —
`file_backed_sqlite_pools_readers_under_wal`, on a temp file (no server): it
confirms the database is in WAL mode, runs sixteen concurrent reads that each see
all the rows, checks read-your-writes across an autocommit write, and checks that
a read inside a transaction sees its own uncommitted insert and that the rollback
undoes it. The existing SQLite suites (migrations, typed, CQRS, value round-trip)
continue to pass on `:memory:`, confirming the single-connection path is
unchanged.
