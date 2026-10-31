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
| JSON | `--json` on every read command emits a stable object. | done (doctor, db *, migrate status/resolve/diff, validate) |
| Exit codes | `0` ok · `1` error · `2` usage (clap) · `3` checks found problems. | done |
| Credential redaction | A DSN is never printed with its password, in output or errors. | done |
| Completions | `dorm completions <bash\|zsh\|fish\|powershell\|elvish>`. | done |
| Non-interactive | `--yes` (`-y`) / `--non-interactive` proceed without prompting on a destructive command. Guards (today): `dorm db seed` (arbitrary writes from a file) and `dorm db execute --write` when the statement is a bare `DROP`/`TRUNCATE` or a `DELETE` with no `WHERE`. On a terminal these prompt `… Continue? [y/N]`; `--yes`/`--non-interactive` skip the prompt and proceed; a piped/scripted run with neither flag refuses without blocking. No prompt on any read command or targeted write. | partial |
| Diagnostics | Errors name the cause; `doctor` names cause and next step. | partial |

## Commands

| Command | Responsibility | Exit | Status | Notes |
|---|---|---|---|---|
| `dorm init [name]` | Scaffold a project (lib + models + binaries). | 0/1 | **done** | Writes a runnable project depending on the published crates. |
| `dorm new model <Name>` | Add a model (struct + registration). | 0/1 | **done** | Not a Prisma command; the DORM equivalent of editing `schema.prisma`. |
| `dorm validate` | Check the models for mistakes a query/migration would trip over. | 0/3/1 | **done** | Cargo lane; `--json`. Checks: primary key, relation targets, duplicate tables, reverse-name collisions. |
| `dorm doctor` | Diagnose config, connectivity, server version, read-only, ledger. | 0/3/1 | **done** | Native (no project needed); `--json`. |
| `dorm db push` / `dorm push` | Sync schema to the models, additively. | 0/1 | **done** | Never drops or retypes — that **is** the data-loss protection. Not for production history. |
| `dorm migrate deploy` (= `dorm migrate`) | Apply the reviewed migration chain in `migrations/` (or the initial schema when there is none). | 0/1 | **done** | Cargo lane; applies every unrecorded version in order under one transaction and advisory lock. Idempotent. Authoring is `migrate make`/`migrate dev`. |
| `dorm migrate status` | Applied-versions ledger. | 0/1 | **done** | Native; `--json`; readable UTC timestamps. |
| `dorm migrate diff` | Show the additive changes `push` would apply, without touching the DB. | 0/1 | **done** | Cargo lane; read-only; `--json`. |
| `dorm shell` | Open the project REPL. | 0/1 | **done** | Query + `:models` + **DML** `insert`/`update`/`delete` through the compiler, `:sql` preview, `:raw`, and **interactive transactions** `:begin`/`:commit`/`:rollback` (ADR 0006), read-only unless `--write`. Admin ops are [roadmap](roadmap.md). |
| `dorm studio [--write]` | Browse and edit the live database in a browser — DORM's Studio. | 0/1 | **done** | Native (reads `information_schema`); loopback only; read-only unless `--write`. Inline edit, relation editing, foreign-key navigation, global search, bulk delete, CSV export. See [DORM Studio](studio.md). |
| `dorm studio generate [--out]` | Write `src/bin/studio.rs` — a **typed** Studio over your models. | 0/1 | **done** | Over the `dorm-studio` crate; run with `cargo run --bin studio`. Adds relation names and reverse relations. |
| `dorm db tables\|show\|count\|rows` | Browse the live database, read-only. | 0/1 | **done** | Native; `--json`; `rows --where col=value` (bound param, column validated). |
| `dorm completions <shell>` | Print a completion script. | 0 | **done** | |
| `dorm generate` | Compile `dorm.schema` → `src/models.rs`. | 0/1 | **done** | Native; the declarative schema compiled to `#[derive(Model)]` — see [schema.md](schema.md), [ADR 0001](adr/0001-declarative-schema.md). |
| `dorm format [--schema] [--check]` | Pretty-print `dorm.schema` into its canonical form, in place. | 0/3 | **done** | Native; **lossless** — formats the text, not the AST, so comments, every field and block-level attribute (`@@index`, `@db.Text`, …) and the `datasource` block all survive; only layout changes (two-space indent, one blank line between blocks, aligned field columns, aligned `=`). Idempotent (`format∘format` is a no-op). `--check` writes nothing and exits 3 if the file is not already formatted. Whitespace *inside* a string or an attribute's `(...)` is kept verbatim. |
| `dorm db pull [--out]` | Introspect a live PostgreSQL schema into `dorm.schema`. | 0/1 | **done** | Native; recovers tables, columns, types, serial/PK, nullability, foreign keys (with `@@map`). Approximate — not unique/index/length/check/ordering or relation `onDelete`; the file says so. Round-trips through `dorm generate`. |
| `dorm api generate` / `dorm api serve` | Generate (`src/bin/api.rs`) and serve an Axum CRUD API + OpenAPI over the models. | 0/1 | **done** | Full CRUD: `GET`/`POST /<table>`, `GET`/`PATCH`/`DELETE /<table>/{id}`, `/openapi.json`, `/health`. Handlers go through the `QuerySet`; `DORM_API_ADDR` sets host:port. Generated code — see ADR 0001. |
| `dorm db seed [--file]` | Run a SQL seed file against the database (default `seeds.sql`). | 0/1 | **done** | Native; applies the file as one batch (multiple statements), on the write path — a read-only session refuses it. Confirms before running (`--yes`/`-y` or `--non-interactive` to skip; a non-tty run with neither refuses — see Non-interactive). |
| `dorm db execute "<SQL>" [-p VALUE]... [--write]` | Run explicit SQL with bound parameters. | 0/1 | **done** | Native; `--json`. The CLI surface over the ADR 0005 valve (`db::sql` / `db::sql_execute`). Read-only by default — a write needs `--write` and still passes the read-only guard. A `--write` that is a bare `DROP`/`TRUNCATE` or a `DELETE` without `WHERE` confirms first (`--yes` to skip); targeted writes do not prompt. Every `-p` value is bound as `$n`, never spliced. Renders the closed set of types the ORM knows; project others as `::text`. |
| `dorm migrate dev [name]` | Author **and** apply the next migration, in one step. | 0/1 | **done** | Cargo lane; the dev loop = `migrate make <name>` then `migrate deploy`. Stops if `make` fails rather than deploying a broken chain. Like `prisma migrate dev`. |
| `dorm migrate resolve <version> [--name]` | Mark a version applied in the ledger **without running its SQL** — recovery after a manual fix. | 0/1 | **done** | Native; `--json`. Refuses unless the operator names the exact version, and refuses a version already recorded. Name is read from `migrations/` or given by `--name`; the version is a bound parameter, never spliced. Like `prisma migrate resolve --applied`. |
| `dorm db shell` | Open the native DB client (`psql`) on the configured database. | 0/1 | **done** | Native; hands you to `psql` with the DSN, then everything is `psql`. (Was `dorm dbshell` — now a `db` subcommand.) |
| `dorm createsuperuser` / `changepassword` / `user …` | Application-account admin. | — | **planned** | Via an accounts adapter; never writes incompatible hashes — see roadmap §accounts. |

## JSON shapes (stable)

- `doctor`: `{ "ok": bool, "checks": [{ "name", "status": "ok|warn|error", "detail" }] }`
- `db tables`: `{ "tables": ["…"] }`
- `db show`: `{ "table", "columns": [{ "name", "type", "nullable": bool }] }`
- `db count`: `{ "table", "count": N }`
- `db rows`: `{ "table", "columns": ["…"], "rows": [{col: "val"}], "count": N }`
- `migrate status`: `{ "applied": [{ "version", "name", "applied_unix", "applied_at" }] }`
- `migrate resolve`: `{ "resolved": { "version", "name", "applied_unix", "applied_at" } }`
- `migrate diff`: `{ "changes": ["<ddl>"] }`
- `validate`: `{ "ok": bool, "issues": [{ "severity", "model", "message" }] }`

## Lanes

Two kinds of command, because DORM is a compile-time ORM:

- **Native** — run inside the installed binary over a DSN: `doctor`, `db *`,
  `migrate status`, `migrate resolve`, `completions`, `init`, `new model`. No
  project build.
- **Cargo lane** — build and run one of the project's own binaries, because they
  need the compiled models: `validate`, `migrate dev`, `migrate deploy`,
  `migrate diff`, `push`, `shell`. `--json` is forwarded to the binary.
