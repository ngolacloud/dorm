---
session: 2026-10-28-bench-prepared-cache
opened: 2026-10-28
branch: feat/bench-prepared-cache
status: closed
closed: 2026-10-29T10:40:47+01:00
---

# Objective
Prove DORM's performance (ADR 0007): criterion benchmarks (DORM vs native tokio-postgres) plus a per-connection prepared-statement cache that removes the per-call `prepare`. Expanded mid-session, at the owner's request, into ADR 0008 (observability) and the `dorm db execute` CLI, then the integration and merge of the whole production-hardening campaign (ADR 0004–0008 + CLI) onto `main`.

# Done when
- [x] `cargo bench` (new, criterion) compares SELECT/INSERT typed-DORM vs raw tokio-postgres and prints the numbers; results captured in `docs/adr/0007-*.md`. (Live PG16: SELECT 189µs ad-hoc → 87µs cached → 104µs typed; INSERT 469→347µs. ADR 0007 written with the table.)
- [x] A per-connection prepared-statement cache removes the per-call `prepare`; proven by a test or by the benchmark difference. (Both: `prepare_cached` in `db::query`/`execute`; `tests/prepared_cache_live.rs` asserts exactly one named statement in `pg_prepared_statements`; benchmark shows 2.2× on SELECT.)
- [x] (Added) ADR 0008 observability + `dorm db execute` CLI delivered, then the whole campaign integrated and merged to `main`. (PR #4 MERGED, `main`==`9ad73fb`; CI check/docs/integration all green.)

# Out of scope
- ADR 0006 brick 2: interactive transactions in the dbshell (`:begin`/`:commit`).
- ADR 0004 follow-ups: verify-ca verifier, retry/reconnect with backoff, pool health-check on recycle, widening `Value` (f64/bytea/date/arrays).

# Handoff
Delivered: The full DORM production-hardening campaign is on `main` (`9ad73fb`), merged via PR #4 (merge commit, each ADR preserved). ADR 0007 — `prepare_cached` per connection in `crates/dorm-core/src/db.rs`, first criterion benchmarks (`benches/prepared_cache.rs`), live proof (`tests/prepared_cache_live.rs`), ADR with measured numbers. ADR 0008 — `tracing` per statement (target `dorm::db`), slow-query log (`DELONIX_ORM_SLOW_QUERY_MS`), `db::pool_stats()`; never logs bound values. CLI `dorm db execute` over the ADR 0005 raw valve (read-only by default, `--write`, `-p` bound as `$n`). ADR 0004 (TLS + statement timeout) also carried in (it was a sibling branch off `b6bf054`, not yet on the old `main`). Validated on the integrated result: clippy `--all-features -D warnings`, `fmt --check`, `cargo doc -D warnings`, offline suite, and 6 live suites run isolated — all green; CI on PR #4 green.
Not delivered: nothing from this session's criteria; the two out-of-scope items remain for later sessions.
Learned: (1) The published `main` was far behind — all evolution (m2m, migrations, ADR 0003/0004) lived on branches ahead; the campaign PR fast-forwarded `main` across all of it. (2) The docs CI job (`cargo doc --no-deps --workspace --document-private-items`, `RUSTDOCFLAGS=-D warnings`) catches broken intra-doc links that clippy does not — `[`StatementCache`]` (an external deadpool type) failed it; run the docs gate before opening a PR. (3) `cargo test --workspace` WITH a DSN fails spuriously — multiple live suites rewrite schema `public` concurrently on one database; run live suites individually (`make test-live` does). (4) tokio-postgres needs an explicit host in the DSN (`host=/var/run/postgresql …`); a URL `postgresql:///db` fails "both host and hostaddr are missing". (All four written to auto-memory `dorm-production-hardening-campaign`.)
Next session: ADR 0006 brick 2 — interactive transactions in the dbshell (`:begin`/`:commit`/`:rollback`), deciding buffer-vs-dedicated-connection since the `CURRENT_TX` task-local does not survive the per-line `block_on`.

# The next-session prompt
See below.
