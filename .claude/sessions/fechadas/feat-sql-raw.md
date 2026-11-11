---
session: ee6749e2-68a3-4c0a-b28a-7bad1821fceb
opened: 2026-10-28
branch: feat/sql-raw
status: closed
closed: 2026-10-28T21:30:16+01:00
---

# Objective
Give DORM a safe, parameterized raw-SQL escape hatch at the library level (ADR 0005) — a Django-shaped `QuerySet::raw` that maps hand-written SQL back to typed models, plus untyped `db::sql` / guarded `db::sql_execute` — so a query the compiler cannot express (CTEs, window functions, hints) stays inside DORM's safety rules instead of forcing a side channel.

# Done when
- [x] `Manager::<T>::raw(sql, params)` runs hand-written SQL and returns `Vec<T>` via `T::from_row`; every value travels as a bound `$n`. (Live-proven: `typed_e2e::raw_sql_maps_rows_back_to_the_model` maps a `tenants` SELECT to `Tenant` and shows `' OR '1'='1` binds inert.)
- [x] `db::sql` (untyped read → `Vec<Row>`) and `db::sql_execute` (write → rows affected) are public; `sql_execute` refuses under `set_read_only(true)` with `Error::ReadOnly`. (Offline test `db::tests::raw_execute_is_refused_in_a_read_only_session`.)
- [x] `make check` green (fmt + clippy `--all-features -D warnings` + `test --all-targets` + doc links), and `docs/adr/0005-raw-sql.md` records the contract and guardrails.

# Out of scope
- `dorm db execute` CLI command (next brick of ADR 0005 — contract already in cli-contract.md as planned).
- dbshell `:raw` and DML (ADR 0006) — consumes this API.
- TLS/timeout follow-ups (ADR 0004), benchmarks (ADR 0007), observability.
- A typed-parameter-schema or compile-time check of the raw SQL text: raw is "you own the SQL", values stay bound.

# Handoff

ADR 0005 library brick landed on `feat/sql-raw` (sibling of `feat/conn-production-tls`,
both off `feat/backend-generic-db`). Not pushed — owner takes the PR.

**What changed**
- `dorm-core/src/queryset.rs`: `Manager::<T>::raw(sql, &[Value]) -> Vec<T>` — runs
  hand-written SQL, maps each row with `from_row` (columns must be the model's, in
  field order).
- `dorm-core/src/db.rs`: `db::sql` (untyped read) and `db::sql_execute` (write,
  read-only-guarded), over the existing backend seam — so raw observes the pool,
  the transaction in scope, the ADR 0004 timeout/TLS, and the read-only guard.
- Docs: `docs/adr/0005-raw-sql.md`; `cli-contract.md` `db execute` row notes the
  shipped library half.
- Tests: `db::tests::raw_execute_is_refused_in_a_read_only_session` (offline);
  `typed_e2e::raw_sql_maps_rows_back_to_the_model` (live, mapping + injection-inert).

**Next bricks (deliberately out of this session)**
- `dorm db execute` CLI command over `db::sql`/`db::sql_execute` (its own branch).
- ADR 0006 dbshell: DML through the compiler + `:raw` consuming this API.
- Then ADR 0007 (benchmarks + prepared-statement cache) and observability.
