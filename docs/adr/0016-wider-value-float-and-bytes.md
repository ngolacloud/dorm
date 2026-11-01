# ADR 0016 — Widening `Value`: float and bytes

## Status

Accepted and implemented. A Phase-1 item from ADR 0011 ("wider `Value`"),
delivering the two most commonly-missing scalar primitives. Dates/times, arrays,
and composite keys remain follow-ups on the same seam.

## Context

`Value` — the one type every bound parameter and every decoded column passes
through — covered `bool`, `i32`, `i64`, `text`, `decimal`, `timestamp`, `uuid`
and `json`. Two everyday primitives were missing:

- **A float.** `Decimal` is deliberately the type for money (a float loses
  cents — ADR's note, kept), but plenty of columns are genuinely floating point:
  a coordinate, a ratio, a measurement, a score. There was no way to store one.
- **Bytes.** A hash, a key, a signature, a small binary blob — `BYTEA`/`BLOB` —
  had no representation, forcing a hex-in-text workaround that the database
  could not index or size correctly.

`Value` is a **closed** set by design (ADR 0002/the type's own note): an open one
would leave the migration autodetector unable to name a column's type and let the
REPL ask for a column the compiler cannot describe. So widening it means adding
variants, not opening it.

## Decision

Two new `FieldType`s and `Value` variants, wired through every seam the closed set
touches:

| Kind | `FieldType` | `Value` | PostgreSQL | SQLite | Rust |
|---|---|---|---|---|---|
| Float | `F64` | `F64(f64)` | `DOUBLE PRECISION` | `REAL` | `f64` |
| Bytes | `Bytes` | `Bytes(Vec<u8>)` | `BYTEA` | `BLOB` | `Vec<u8>` |

The seams updated, each of which the compiler's exhaustiveness check pinned:

- **Binding** (`ToSql`) and **PG decode** (`read_column`, `Decode`).
- **SQLite** encode (`value_to_sqlite`) and decode (`cell_to_value`, `Decode`),
  mirrored in the `dorm-sqlite` crate — a float travels as `REAL`, a blob as
  `BLOB`, so nothing round-trips through text.
- **DDL** type mapping (PostgreSQL via `sql_type`, SQLite via its storage class).
- **Migrations** (`type_tag`), so a snapshot records the new types.
- **`ColumnType`** for `f64`/`Vec<u8>`, so the derive fills a struct field's type
  in with no attribute.
- **Coercion**: an integer or a numeric string widens into a float (a REPL `1` or
  `19.5` into a `DOUBLE` column). The reverse is never implicit — a float into an
  integer or a decimal could lose information, so it stays an error, the same rule
  the integer coercions already keep.
- **Display**: a blob renders as `\x`-prefixed hex, PostgreSQL's own rendering, so
  a REPL cell and a CSV/JSON export are unambiguous and copy-pasteable.

### Why not a float for money, still

`Decimal` remains the money type; `F64` is for quantities where binary rounding is
acceptable. The coercion rules enforce the boundary: nothing silently turns a
decimal into a float or back, so a money column cannot acquire float error by
accident.

## Consequences

- A model can now hold a `f64` or a `Vec<u8>` field directly — `pub score: f64`,
  `pub digest: Vec<u8>` — and the derive, the migrator, and both backends handle
  it end to end with no attribute.
- The closed-set invariant holds: the autodetector names the new types, the REPL
  can ask for them, and nothing opened to arbitrary `ToSql`.
- Dates/times, arrays (still deliberately not an `in`-lookup `Value`), and
  composite keys are unaffected and remain the next widenings.

## Proof

- `crates/dorm-core/tests/value_roundtrip_live.rs` — a `DOUBLE PRECISION`/`REAL`
  and a `BYTEA`/`BLOB` column take a bound `Value::F64` and `Value::Bytes` and read
  back `1.5` and the exact bytes `[0, 1, 2, 250, 255]`, through both the typed
  `try_get` and the model-erased `read_column`, on **SQLite** (no server) and on
  **PostgreSQL** (live).
- Unit tests pin the pure halves: the widening coercions (and that the reverse is
  refused), the SQL/Rust type mapping, and the `\x`-hex blob rendering.
