---
session: ee6749e2-68a3-4c0a-b28a-7bad1821fceb
opened: 2026-10-28
branch: feat/dbshell-dml
status: closed
closed: 2026-10-28T21:30:16+01:00
---

# Objective
Give the `dorm` shell write DML through the same compiler the reads use (ADR 0006, brick 1) — `insert` / `update` / `delete` from string-keyed input, `:sql` to preview a command's SQL, and `:raw` for the ADR 0005 escape hatch — all gated read-only by default with a `--write` opt-in.

# Done when
- [x] `insert <Model> col=val…`, `update <Model> [filter…] set col=val…`, and `delete <Model> filter…` run through the compiler against a live database, each printing what it changed. (Live `dml_live::the_shell_inserts_updates_and_deletes_…` drives the `CARGO_BIN_EXE_dorm` binary; manual run confirmed: inserted/updated/deleted 1 row, read-back shows the rename.)
- [x] Read-only unless `--write`: a write without it is refused by name (`this session is read-only; 'insert' would write`), `delete` with no filter refused unless `all`; `:sql` prints SQL + bound params without running, `:raw` runs via `db::sql`. (Proven in `dml_live` + manual.)
- [x] `make check` green (fmt + clippy `--all-features -D warnings` + `test --all-targets` + doc links); model-erased write path in `dorm-core` (`insert_value_rows`/`update_value_rows`/`delete_value_rows`); `docs/adr/0006-dbshell-dml.md` written.

# Out of scope
- Interactive multi-line transactions (`:begin`/`:commit`/`:rollback`) — brick 2; needs the buffer-vs-dedicated-connection decision (the current `atomic()` is closure-scoped and a task-local does not survive separate `block_on` lines).
- `dorm db execute` CLI command (separate branch, reuses the same helpers).
- Autocomplete, history, multi-row display paging, and the accounts shell (ADR roadmap Phases 4–5).
- Benchmarks (ADR 0007) and observability.

# Handoff
Delivered: ADR 0006 brick 1 on `feat/dbshell-dml` @ 05d1e0a (child of feat/sql-raw; committed, NOT pushed — owner takes the PR after 0005). The `dorm` shell runs `insert`/`update`/`delete` through the same compiler the reads use (model-erased helpers `insert_value_rows`/`update_value_rows`/`delete_value_rows` in dorm-core next to `fetch_value_rows`), plus `:sql` preview and `:raw` (ADR 0005). Read-only by default; `--write` opts in; unfiltered `delete` refused unless `all`. Proven live driving the real binary: `cargo test -p dorm-shell --test dml_live -- --ignored` → 2 passed (full insert→read→update→read→delete→read cycle + read-only refusal); manual run showed `(inserted 1 row)`, `name=NovoNome` after update, `(deleted 1 row)`, and `error: this session is read-only; 'insert' would write`. `make check` green (fmt + clippy --all-features -D warnings + test --all-targets + doc -D warnings).
Not delivered: nothing in this contract's scope left open. Out of scope and still pending in the campaign: ADR 0006 brick 2 (interactive transactions), `dorm db execute` CLI, ADR 0007 (benchmarks + prepared-statement cache), observability.
Learned: `tokio_postgres::Config` REJECTS `sslmode=verify-full`/`sslrootcert` (ADR 0004 lesson, carried). The REPL's `CURRENT_TX` task-local does NOT survive the per-line `block_on`, so interactive `:begin/:commit` needs a design decision (buffer-in-one-`atomic()` vs a dedicated connection held across lines). Shell DML is safe for free because the compiler already validates every column name against model metadata and quotes only static strings — the dynamic front end inherits the typed one's safety. Live proofs use local PostgreSQL 16 at `/var/run/postgresql` (user walter), throwaway DB created+dropped per run. The three campaign branches are siblings/child off `feat/backend-generic-db`, all committed, none pushed.
Next session: ADR 0007 — add criterion benchmarks (DORM vs raw tokio-postgres) and a prepared-statement cache, to answer whether DORM performs at native-query level (today unmeasured).
