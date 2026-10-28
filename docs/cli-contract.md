# `dorm` CLI — contract

The target command surface, mapped onto the Prisma CLI (reference: Prisma CLI
6.x) where the semantics match, with Rust-native ergonomics. This is a **living
contract**: each row states what the command does, its arguments, its exit
codes, and — honestly — whether it exists yet.

Status legend: **done** (shipped and tested) · **partial** (works, with named
limits) · **planned** (contract agreed, not built — see
[roadmap](roadmap.md)). No command is shipped empty to simulate parity.

## Global behaviour

| Concern | Contract | Status |
|---|---|---|
| Datasource | `--dsn <DSN>` overrides `DELONIX_ORM_DSN` (→ `PAAS_DATABASE_URL`) for the run. Precedence: `--dsn` > `DELONIX_ORM_DSN` > `PAAS_DATABASE_URL`. | done |
| JSON | `--json` on every read command emits a stable object. | done (doctor, db *, migrate status/diff, validate) |
| Exit codes | `0` ok · `1` error · `2` usage (clap) · `3` checks found problems. | done |
| Credential redaction | A DSN is never printed with its password, in output or errors. | done |
| Completions | `dorm completions <bash\|zsh\|fish\|powershell\|elvish>`. | done |
| Non-interactive | No command prompts yet; `--yes`/`--non-interactive` lands with the first interactive or destructive command (createsuperuser, destructive migrate). | planned |
| Diagnostics | Errors name the cause; `doctor` names cause and next step. | partial |

## Commands

| Command | Responsibility | Exit | Status | Notes |
|---|---|---|---|---|
| `dorm init [name]` | Scaffold a project (lib + models + binaries). | 0/1 | **done** | Writes a runnable project depending on the published crates. |
| `dorm new model <Name>` | Add a model (struct + registration). | 0/1 | **done** | Not a Prisma command; the DORM equivalent of editing `schema.prisma`. |
| `dorm validate` | Check the models for mistakes a query/migration would trip over. | 0/3/1 | **done** | Cargo lane; `--json`. Checks: primary key, relation targets, duplicate tables, reverse-name collisions. |
| `dorm doctor` | Diagnose config, connectivity, server version, read-only, ledger. | 0/3/1 | **done** | Native (no project needed); `--json`. |
| `dorm db push` / `dorm push` | Sync schema to the models, additively. | 0/1 | **done** | Never drops or retypes — that **is** the data-loss protection. Not for production history. |
| `dorm migrate deploy` (= `dorm migrate`) | Apply reviewed migrations (the initial migration). | 0/1 | **partial** | Applies version 1; multi-version authoring is `migrate dev` (planned). |
| `dorm migrate status` | Applied-versions ledger. | 0/1 | **done** | Native; `--json`; readable UTC timestamps. |
| `dorm migrate diff` | Show the additive changes `push` would apply, without touching the DB. | 0/1 | **done** | Cargo lane; read-only; `--json`. |
| `dorm shell` | Open the project REPL. | 0/1 | **partial** | Query + `:models` + **DML** `insert`/`update`/`delete` through the compiler, `:sql` preview and `:raw` (ADR 0006), read-only unless `--write`. Interactive transactions and admin ops are [roadmap](roadmap.md). |
| `dorm db tables\|show\|count\|rows` | Browse the live database, read-only. | 0/1 | **done** | Native; `--json`; `rows --where col=value` (bound param, column validated). |
| `dorm completions <shell>` | Print a completion script. | 0 | **done** | |
| `dorm generate` | Compile `dorm.schema` → `src/models.rs`. | 0/1 | **done** | Native; the declarative schema compiled to `#[derive(Model)]` — see [schema.md](schema.md), [ADR 0001](adr/0001-declarative-schema.md). |
| `dorm format` | Pretty-print `dorm.schema`. | — | **planned** | Now that the schema exists, this is a formatter over it. |
| `dorm db pull [--out]` | Introspect a live PostgreSQL schema into `dorm.schema`. | 0/1 | **done** | Native; recovers tables, columns, types, serial/PK, nullability, foreign keys (with `@@map`). Approximate — not unique/index/length/check/ordering or relation `onDelete`; the file says so. Round-trips through `dorm generate`. |
| `dorm api generate` / `dorm api serve` | Generate (`src/bin/api.rs`) and serve an Axum CRUD API + OpenAPI over the models. | 0/1 | **done** | Full CRUD: `GET`/`POST /<table>`, `GET`/`PATCH`/`DELETE /<table>/{id}`, `/openapi.json`, `/health`. Handlers go through the `QuerySet`; `DORM_API_ADDR` sets host:port. Generated code — see ADR 0001. |
| `dorm db seed` | Run configured seeds. | — | **planned** | |
| `dorm db execute` | Run explicit SQL with parameters. | — | **planned** | The library half shipped (ADR 0005): `db::sql`, `db::sql_execute` (read-only-guarded), and `Manager::raw` (typed). This command is the CLI surface over them. |
| `dorm migrate dev` | Author + apply a dev migration. | — | **planned** | Needs migration authoring beyond the initial. |
| `dorm migrate resolve` | Reconcile ledger after manual recovery. | — | **planned** | Must never mark applied without evidence. |
| `dorm dbshell` | Open the native DB client (`psql`, …). | — | **planned** | Per-backend. |
| `dorm createsuperuser` / `changepassword` / `user …` | Application-account admin. | — | **planned** | Via an accounts adapter; never writes incompatible hashes — see roadmap §accounts. |

## JSON shapes (stable)

- `doctor`: `{ "ok": bool, "checks": [{ "name", "status": "ok|warn|error", "detail" }] }`
- `db tables`: `{ "tables": ["…"] }`
- `db show`: `{ "table", "columns": [{ "name", "type", "nullable": bool }] }`
- `db count`: `{ "table", "count": N }`
- `db rows`: `{ "table", "columns": ["…"], "rows": [{col: "val"}], "count": N }`
- `migrate status`: `{ "applied": [{ "version", "name", "applied_unix", "applied_at" }] }`
- `migrate diff`: `{ "changes": ["<ddl>"] }`
- `validate`: `{ "ok": bool, "issues": [{ "severity", "model", "message" }] }`

## Lanes

Two kinds of command, because DORM is a compile-time ORM:

- **Native** — run inside the installed binary over a DSN: `doctor`, `db *`,
  `migrate status`, `completions`, `init`, `new model`. No project build.
- **Cargo lane** — build and run one of the project's own binaries, because they
  need the compiled models: `validate`, `migrate deploy`, `migrate diff`,
  `push`, `shell`. `--json` is forwarded to the binary.
