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

- **Tier 1**: PostgreSQL (shipped), **SQLite** — bricks 1 and 2 shipped:
  - *Brick 1* — the DDL dialect
    (`dorm_migrations::ddl::schema_for_on(models, Backend::Sqlite)`) generates
    SQLite schema from the same `ModelMeta`.
  - *Brick 2 (compiler + executor)* — the compiler is dialect-aware across
    `compile_{select,insert,update,delete,count}_on` with `Dialect::Sqlite`
    (`?n` placeholders — renumbered correctly through the `UPDATE`/`DELETE`
    subqueries — `LIKE`, `CURRENT_TIMESTAMP`), and `dorm-sqlite` executes that
    compiled SQL against `rusqlite`, decoding rows to `Value`s. **Full CRUD +
    count proven** over a real SQLite database (insert `RETURNING`, select,
    update, delete). **Remaining**: wiring SQLite into `QuerySet`'s async
    terminals (`.all().await`) via one backend-generic `dorm-core::db` so the
    backend is chosen there, and aggregate/exists dialect. PostgreSQL stays the
    default throughout ([ADR 0002](adr/0002-multi-backend.md)).
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

**Shipped (ADR 0006, brick 1)**: write DML through the compiler — `insert`,
`update`, `delete` from string-keyed input — plus `:sql` to preview a command's
SQL and bound params, and `:raw` for the ADR 0005 escape hatch; read-only by
default, `--write` to opt in, unfiltered `delete` refused unless `all`. Next:
interactive transactions (`:begin`/`:commit`), autocomplete and history.

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

- **Shipped (ADR 0004)**: TLS on PostgreSQL — `sslmode=require|verify-full`,
  feature-gated rustls (default) / openssl backends, roots from `sslrootcert` or
  the bundled Mozilla set, never a silent downgrade; and a server-side
  `statement_timeout` surfaced as `Error::Timeout`. Follow-ups: `verify-ca`,
  retry/reconnect, a recycle health check, and a wider `Value` set.
- DSN formats per backend (PostgreSQL/MySQL URL forms; SQLite file/memory;
  SQL Server/Oracle per driver), covering percent-encoding, IPv6, TLS and
  certificate validation (never silently disabled), local sockets, timeouts,
  schemas, driver-specific parameters, relative/absolute paths, and the
  DSN/config/env precedence.
- A per-backend behaviour note for identifiers/quoting, case/collation, nulls,
  booleans/decimals/timestamps, generated IDs, upsert, pagination,
  locks/isolation, transactional DDL, constraints, and cancellation/timeouts.

## Phase 8 — AI: RAG and a native MCP server (optional crates)

Two **optional** crates on top of the core, so a project that does not use them
gains no new dependency and an unchanged `dorm-core` graph — the same discipline
as the accounts adapter.

- **`dorm-rag`** ([ADR 0019](adr/0019-rag.md)). A model marked `#[derive(Rag)]`
  (or `@@rag(..)` in `dorm.schema`) is indexed in pgvector and gains semantic and
  hybrid search with the **same filters and the same tenant isolation** as any
  query. Change capture is a database **trigger** writing an outbox, so every
  write path — typed, `:raw`, `dorm db execute`, Studio, `db seed`, `psql` — is
  indexed and the core write path is untouched. A worker drains the outbox
  (`SKIP LOCKED`), re-embedding only changed chunks. Search pre-filters through
  the one compiler — the vector is a bound `$n::vector`, the `Value` enum gains no
  variant. Embedders are feature-gated: `embed-local` (ONNX, default, data stays
  on the host), `embed-ollama`, `embed-openai-compat`. SQLite is an explicit
  capability error, never a silent non-vector fallback. Bricks: **(1) declaration
  — `RagMeta`, `#[derive(Rag)]`, `@@rag`, `dorm validate` ✅**, **(2) migrations
  (extension, shadow tables, outbox, HNSW + GIN, enqueue triggers) ✅ — generated
  by `dorm_rag::migrations`, snapshot-tested and proven live on pgvector:pg16;
  flows through `migrate deploy` by appending to a `Migration`'s statements**,
  **(3) worker + `dorm rag status|reindex|worker` ✅ — outbox drain with
  `SKIP LOCKED`, SAVEPOINT-isolated per record, hash/embedder staleness, retries +
  dead-letter; proven live on pgvector + Ollama `all-minilm` (semantic search
  returns the right record)**, **(4) the `Embedder` trait ✅ + backends —
  `embed-local` ✅ (ONNX on the host via `fastembed`, `all-MiniLM-L6-v2`, proven
  live — download-once then offline, semantically sane), `embed-ollama` ✅,
  `embed-openai-compat` ✅ (any `/v1/embeddings`, proven live against Ollama's
  OpenAI shim)**,
  **(5) typed `RagQuerySetExt` (`semantic/hybrid/min_score/search`) + dynamic
  `semantic_search` ✅ — compiler-reused pre-filtering (vector bound as
  `$n::text::vector`, subquery params first so no renumbering), RRF hybrid, SQL
  snapshot tests, and a live tenant-isolation proof on pgvector + Ollama**,
  **(6) the REPL `search` verb and `:rag` ✅ (in `dorm-shell`; the `dorm rag` CLI
  landed in brick 3).**

- **`dorm-mcp`** ([ADR 0020](adr/0020-mcp-server.md)). A native MCP server that
  exposes the registry as tools and resources, generated into the project with
  `dorm mcp generate` (so it links the models), over the official `rmcp` SDK. Read
  tools (`list_models`, `describe_model`, `query`, `get`, `count`, `aggregate`,
  `semantic_search`) are generated from `FieldMeta`; write tools (`insert`,
  `update`, `delete`) appear only with `--write`, each with `dry_run`, a row cap,
  and a refusal of unfiltered mutations. **No raw SQL is exposed.** Security is the
  design: read-only by default behind the core's `guard_write`, the tenant from
  the credential (never a tool argument), loopback-only by default, allow/deny
  lists, limits reusing `DELONIX_ORM_STATEMENT_TIMEOUT_MS`, a `tracing` audit event
  per call, and record/RAG text delimited as untrusted on the way out. Bricks:
  **(1) the read-only server + `dorm mcp generate` ✅ — JSON-RPC 2.0 over stdio
  (no `rmcp`; see ADR 0020), the seven read tools generated from `FieldMeta`,
  resources, prompts; tenant forced from config; proven live over the protocol on
  pgvector + Ollama (semantic_search → query, tenant isolation)**, **(2) writes
  ✅ — `insert`/`update`/`delete` only with `--write`, each with `dry_run`
  (SQL+params+affected preview), a row cap, and a refused unfiltered mutation;
  HTTP transport ✅ with bearer auth (tenant+scope from the token), loopback
  default and a rate limit; a `dorm::mcp` audit event per call (no secrets).
  Proven live (write tools, tenant forced on insert, cap, HTTP 401/200). The
  full OAuth 2.1 authorization server and MCP SSE framing remain a follow-up.**

## Phase 7 — Guides and the proof journey

- A **Prisma → DORM** and a **Django → DORM** guide, each as
  *known concept → DORM equivalent → behavioural difference → runnable example*.
- A full journey on PostgreSQL **and** SQLite: `init` → schema → generate →
  migration → queries → shell → administrative account create/update.
- The published feature matrix reflects only what is demonstrated.
