# DORM roadmap

The ambition is an ORM fit for mature, complex projects, familiar to Prisma and
Django users, with Rust-native ergonomics — proven by contracts, docs, tests and
real use. This roadmap is the authoritative plan for getting there **without**
shipping empty commands or claiming support that has not been demonstrated.

Done so far: the typed query layer and `QuerySet`, the derive, additive schema
`sync` (`push`) and the versioned initial migration, the CLI foundation
(`validate`, `doctor`, `migrate diff`, `db *`, `--json`, exit codes, redaction,
completions), and the **declarative `dorm.schema` → Rust** pipeline (`dorm
generate`, Prisma-faithful syntax, [ADR 0001](adr/0001-declarative-schema.md),
[schema.md](schema.md)). See [cli-contract.md](cli-contract.md) for exact status.

`dorm api generate`/`serve` ships **full CRUD**: `GET`/`POST /<table>`,
`GET`/`PATCH`/`DELETE /<table>/{id}` and `/openapi.json`, generated code over the
`QuerySet` (ADR 0001's corollary), proven serving live JSON (create → 201,
update, delete → 204, 404 on a missing id).

Next on this front: typed request/response schemas (today the write body is a
dynamic JSON object), pagination metadata and filtering on list endpoints, then
`db pull` (introspect → schema) and `format` (pretty-print the schema), which the
schema file now makes tractable.

## Guiding rules

- A command is not shipped until it does something real and tested.
- Compatibility of protocol is not equivalence of behaviour. Each backend is
  proven per version, and differences are documented, not hidden.
- Destructive operations show their impact and require an explicit opt-in in
  automation.
- The API may be common across backends; the execution plan and the guarantees
  reflect the real backend.

## Phase 1 — CLI ergonomics (in progress)

- **Shipped**: `validate`, `doctor`, `migrate diff`, `--json`, exit codes,
  credential redaction, completions, `db` browser.
- **Next**: `--yes`/`--non-interactive` (arrives with the first destructive
  command), `db execute` (parameterised SQL), `db seed`.

## Phase 2 — Migrations beyond the initial

- `migrate dev`: author a new versioned migration from a model change (needs a
  stored snapshot of the previous schema to diff against — the autodetector).
- `migrate resolve`: reconcile the ledger after manual recovery, **never**
  marking a version applied without evidence.
- `migrate deploy`: apply a reviewed chain (today it applies version 1 only).
- Destructive changes surfaced and gated; `push` stays additive and
  dev-oriented.

## Phase 3 — Multi-SGBD by tiers

DORM core is currently coupled to `tokio-postgres` (see `dorm-core::db`,
`model`, `queryset`, `value`). Multi-backend starts with a **driver/backend
trait** that separates the portable core from each engine's dialect; the
compiler emits backend-aware SQL; unsupported capabilities return explicit
errors rather than wrong results.

- **Tier 1**: PostgreSQL (shipped), **SQLite** (file + in-memory, in the test
  matrix from day one — see below).
- **Tier 2**: MySQL; MariaDB with its own tests and documented differences.
- **Enterprise**: Microsoft SQL Server; Oracle.
- Other engines evaluated separately.

Each backend ships with a **per-version matrix**: Types · CRUD · Relations ·
Transactions · Introspection · Migrations · Native features · Limitations ·
Tests. "Supported" means that matrix is green, not that a connection opens.

### SQLite as a first-class backend (Tier 1)

Not a stand-in for the Postgres tests — its own semantics. Must validate: file
and in-memory DBs, persistence/reopen, foreign-key enforcement, reader/writer
concurrency, `busy_timeout`, WAL, in-memory pooling semantics,
table-rebuild migrations, index/constraint/trigger preservation, and
date/decimal/JSON typing differences. Document whether the SQLite library is
system-provided or bundled.

## Phase 4 — Shell inspired by Django

`dorm shell` loads the project's models and config and offers: discovery of
models/fields/relations, CRUD, explicit transactions, SQL inspection,
connection/context inspection (no secrets), reproducible scripts, autocomplete
and help, and a clear display of the active environment and datasource. A
read-only profile by default in production, enforced also by the connection's
permissions. The shell is interpreted — it does not promise Rust's compile-time
guarantees for dynamically evaluated expressions; it shares the query
representation and rules with the Rust client wherever possible.

## Phase 5 — Application-account administration

An **optional** adapter crate (e.g. `dorm-accounts`) — the core ORM imposes no
`User` table, no fixed fields, no universal auth. The project defines the user
model, the login identifier, required fields, password policy, hashing service,
active state, roles/privileges, auditing, and session-revocation rules. The CLI
(`createsuperuser`, `changepassword`, `user show|update`) and the shell use the
**application's** auth logic — never writing incompatible hashes directly. If no
adapter is configured, the command explains how to configure one. Identity
managed by an external provider (e.g. Keycloak) is routed through the authorised
adapter or declared out of scope — never by editing the provider's own store.

Security: no-echo password entry, environment/account named, DB-enforced
uniqueness, transactional create/update, explicit privilege changes, auditing
without credentials, defined behaviour for inactive accounts and the last
administrator, and non-interactive operation over secure channels.

## Phase 6 — DSNs and compatibility, documented

- DSN formats per backend (PostgreSQL/MySQL URL forms; SQLite file/memory;
  SQL Server/Oracle per driver), covering percent-encoding, IPv6, TLS and
  certificate validation (never silently disabled), local sockets, timeouts,
  schemas, driver-specific parameters, relative/absolute paths, and the
  DSN/config/env precedence.
- A per-backend behaviour note for identifiers/quoting, case/collation, nulls,
  booleans/decimals/timestamps, generated IDs, upsert, pagination,
  locks/isolation, transactional DDL, constraints, and cancellation/timeouts.

## Phase 7 — Guides and the proof journey

- A **Prisma → DORM** and a **Django → DORM** guide, each as
  *known concept → DORM equivalent → behavioural difference → runnable example*.
- A full journey on PostgreSQL **and** SQLite: `init` → schema → generate →
  migration → queries → shell → administrative account create/update.
- The published feature matrix reflects only what is demonstrated.
