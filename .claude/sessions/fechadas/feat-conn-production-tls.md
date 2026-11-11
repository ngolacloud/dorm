---
session: ee6749e2-68a3-4c0a-b28a-7bad1821fceb
opened: 2026-10-28
branch: feat/conn-production-tls
status: closed
closed: 2026-10-28T21:30:16+01:00
---

# Objective
Make DORM's PostgreSQL connection production-grade — TLS (feature-gated rustls default, openssl optional) and a per-query statement timeout — so a DORM client can talk to a database over an untrusted network safely (ADR 0004).

# Done when
- [x] `cargo test` is green, including new tests for the TLS connector selection (`sslmode` parsing) and the statement-timeout path. (65 unit tests in dorm-core + full workspace `--all-targets`; `make check` gate green: fmt + clippy `--all-features -D warnings` + tests + doc links.)
- [x] `dorm-core` builds with `--no-default-features` (NoTls), with default features (rustls), and with `--features tls-openssl`; the chosen TLS backend is driven by the DSN's `sslmode`. (All three compile; verify-full/require supported, verify-ca an explicit error.)
- [x] A query that exceeds `DELONIX_ORM_STATEMENT_TIMEOUT_MS` is aborted and surfaces a named `Error::Timeout`. (Unit test on the SQLSTATE 57014 mapping + `classify_db`; **live e2e** `statement_timeout_live.rs` proved `pg_sleep(2)` under a 200ms ceiling is cancelled, against local PostgreSQL 16.)

# Out of scope
- Retry/reconnect with backoff and pool health-check round-trip (ADR 0004 follow-up).
- Expanding `Value` types (f64, bytea, date, arrays) — separate session.
- The raw SQL escape hatch (ADR 0005), dbshell DML (ADR 0006), benchmarks/prepared-statement cache (ADR 0007).
- Observability (tracing/slow-query/pool metrics) — transversal, lands after 0005.

# Handoff

ADR 0004 (first two bricks) landed on `feat/conn-production-tls`. Commits not yet
made — owner takes the PR.

**What changed**
- `dorm-core/src/db.rs`: `extract_tls_params` (strips `sslmode`/`sslrootcert`
  the driver can't parse), `ssl_requirement`, `merge_statement_timeout`,
  `build_manager` (TLS connector selection), and the `tls_rustls` / `tls_openssl`
  modules. `pool()` now applies the timeout option and the chosen TLS backend.
- `dorm-core/src/error.rs`: `Error::Timeout`, `Error::Tls`, `classify_db`
  (SQLSTATE 57014 → Timeout), `QUERY_CANCELED`.
- `dorm-core/Cargo.toml` + workspace `Cargo.toml`: `tls-rustls` (default) and
  `tls-openssl` features and their deps.
- Docs: `docs/adr/0004-production-connections.md`, TLS + timeout section in
  `docs/connection-and-transactions.md`.
- Tests: pure-helper unit tests (sslmode, extraction, timeout merge, classify);
  `tests/statement_timeout_live.rs` (ignored; live-proven here).

**Known gaps (follow-ups under ADR 0004, deliberately not this session)**
- `sslmode=verify-ca` returns an explicit error (hostname-skipping verifier TBD).
- No retry/reconnect with backoff; recycle is still `Fast` (no health round-trip).
- `Value` not yet widened (`f64`, `bytea`, `date`/`time`, arrays).
- A live TLS handshake was not exercised (local PG is socket-only); the rustls
  verify-full path is build- and unit-proven, not yet live.

**Next phases** (separate sessions/branches): ADR 0005 raw SQL escape hatch +
`dorm db execute`; ADR 0006 dbshell DML; ADR 0007 benchmarks + prepared-statement
cache; observability (tracing/slow-log/pool metrics) transversal after 0005.
