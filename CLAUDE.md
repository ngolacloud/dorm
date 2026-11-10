# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What DORM is

DORM (Delonix ORM) is a **Django-shaped, compile-time-typed ORM for PostgreSQL**
built on `tokio-postgres`. It is an **independent product** (ADR 0073): the crates
here carry no NgolaCloud business entity and no platform code — an external Rust
program can depend on `dorm-core` alone. The platform's own tables live *outside*
this repo, layered over these crates. The dependency is strictly one-way:
frameworks depend on DORM, never the reverse (ADR 0054).

Read `README.md` and `ARCHITECTURE.md` first; design decisions live in
`docs/adr/` (ADRs are referenced by number throughout the code and are the source
of truth for *why* something is the way it is).

## Commands

`make help` lists everything. The common ones:

```sh
make check        # fmt --check + clippy -D warnings + cargo test --all-targets — EXACTLY what CI runs
make test         # offline test suite (no database)
make fmt          # cargo fmt --all
make build        # release build of the whole workspace
make doc          # API docs (uses --document-private-items)
make shell        # the dorm-shell REPL
```

**`make check` is the whole gate.** Do not claim validation without running it.
It is what CI runs on the fast lane; clippy is `-D warnings`, so any warning fails.

### Running a single test / a single crate

```sh
cargo test -p dorm-core                         # one crate
cargo test -p dorm-core --test sql_snapshots     # one test binary
cargo test -p dorm-core compiles_the_same        # by name substring
cargo insta review                               # review snapshot diffs (insta)
```

### Live and compatibility tests (need a throwaway PostgreSQL)

The offline suite needs no database. Live suites are `--ignored` and read a DSN
from an env var — **not** `DELONIX_ORM_DSN` for all of them, note the different
names:

```sh
make test-live DSN='host=127.0.0.1 user=me dbname=throwaway'
#   migrate_live  reads DELONIX_ORM_DSN
#   typed_e2e     reads DELONIX_ORM_TEST_DSN  (and is --ignored)

make bench DSN=...            # scale benchmark + regression gate (ADR 0036)
make compat-cockroach         # spins a container, runs the typed conformance suite
make compat-mysql             # MySQL 8 driver round-trip (ADR 0018)
```

Containers are spun with **`delonix`** (the workspace's rootless container engine);
**Docker and Podman are absent on this host**. Override with `CRE=docker` only if
you know it's installed. `DELONIX_ORM_DSN` is read **only by the CLI and the live
tests** — the library itself reads no environment variables (configuration is
programmatic).

## Architecture: two front ends, one compiler

The entire design exists to make one compiler the *only* place SQL is built, so
safety is a property of construction, not discipline.

```text
#[derive(Model)] ──submits──▶ registry (linker-built inventory slice)
      typed handles / model metadata
                 ▼
   Q / F ──▶ Plan ──▶ compiler (PURE) ──▶ Sql { text, params } ──▶ db::query/execute
              ▲
   the dorm-shell REPL builds the SAME Plan
```

- **`dorm-derive`** — `#[derive(Model)]`. At compile time emits `&'static ModelMeta`,
  submits it into an `inventory` slice, and generates typed field handles
  (`tenant::slug`). Adding a model is a recompile, not an edit to a registration list.
- **`dorm-core::registry`** — the **only** place a caller's string becomes an
  identifier. `resolve` walks a filter path (`apps__name__icontains`) against the
  metadata and returns a `&'static FieldMeta` + the joins, or an error.
- **`dorm-core::compiler`** — **pure**: `Plan` in, parameterised `Sql` out, no I/O.
  Values become `$n`; identifiers are the static strings the registry handed back.
  Purity is what lets snapshot tests pin generated text *and* parameter order.
- **`dorm-core::queryset`** — the typed, lazy, immutable `QuerySet`/`Manager`.
- **`dorm-core::db`** — the pool and transactions. **One rule dominates: the pool
  belongs to the runtime that built it**, so a long-lived caller (the REPL) keeps a
  single Tokio runtime for its whole session.
- **`dorm-migrations`** — reads the *same* `ModelMeta` and generates reviewable DDL
  plus a versioned migrator.

### The two front ends converge — and that's tested

The typed API (`tenant::slug.eq("acme")`) and the string-keyed API
(`.filter_str("slug", "acme")`, what the REPL is built on) build the **same**
`Q`/`Plan`. Their agreement is a snapshot test
(`filter_by_string_key_compiles_the_same_as_the_typed_form`), not a convention.
The dynamic path is safe at runtime because it cannot name a table or column the
derive macro did not put in the binary.

### Invariants you must not break

| Invariant | Enforced by |
|---|---|
| No identifier comes from a caller | `registry::resolve` → `&'static FieldMeta`; compiler quotes only static strings |
| No value is formatted into SQL | every `Value` is bound as `$n` via `ToSql` |
| The compiler does no I/O | `dorm-core::compiler` has no DB dependency |
| Generated SQL does not drift | `tests/sql_snapshots.rs` pins text **and** parameter order |
| The schema generator is correct | `tests/schema_parity.rs` holds the frozen DDL oracle |

If a change touches the compiler, registry, or schema generation, expect snapshot
tests to move — review every diff; a changed snapshot is a changed public contract.

## Crate map

Published to crates.io: `dorm-core`, `dorm-derive`, `dorm-migrations`,
`dorm-schema`, `dorm-cli`, `dorm-studio`. Example/internal (unpublished):
`dorm-models`, `dorm-example`, `dorm-shell`.

| Crate | Role |
|---|---|
| `dorm-core` | `QuerySet`/`Manager`, the compiler, `Value`, the registry, pool & transactions |
| `dorm-derive` | `#[derive(Model)]` — struct → model + typed field handles |
| `dorm-migrations` | DDL generation, schema autodetector, versioned migrator |
| `dorm-schema` | the `dorm.schema` parser + code generator (Prisma-style schema → Rust) |
| `dorm-cli` | installable `dorm` binary (`cargo install dorm-cli`) — scaffold + manage a live DB |
| `dorm-studio` | Prisma-Studio-style web UI; exposes a `Router` a host mounts behind its own auth |
| `dorm-models` | the ORM's **own** example models, used to pin generated SQL in snapshots |
| `dorm-example` | standalone ~40-line blog using only `dorm-*` — the independence proof |
| `dorm-shell` | the dynamic REPL — enumerates linked models, runs string-typed queries live |
| `dorm-sqlite` | SQLite executor for the SQLite-dialect compiled SQL (ADR 0002) |

**Optional, "ports" crates — `dorm-core` never depends on any of them** (one-way):
`dorm-rag` (pgvector semantic/hybrid search, ADR 0019), `dorm-mcp` (expose the
registry as agent tools, ADR 0020), `dorm-outbox` (transactional outbox/inbox,
ADR 0026), `dorm-jobs` (durable job queue + leases, ADR 0027), `dorm-messaging`
(CloudEvents pub/sub, ADR 0031), `dorm-eventstore` (append-only event streams,
ADR 0039).

## How a framework consumes DORM (ADR 0054)

DORM is the data-source layer — one database, one transaction, one process. When
changing public API, preserve these consumption contracts:

- **Programmatic configuration.** A host hands DORM its DSN, replicas, TLS, pool and
  cursor key by API. The library reads **no** environment variables (only the CLI
  reads `DELONIX_ORM_DSN`).
- **Explicit sessions, no global default.** Hosts open a session per module/request
  with `with_session`/`connect_schema`; they never rely on the process-global handle.
- **Typed results, not printed text.** Everything the CLI does is a public library
  function returning a typed result (migrations `plan`/`apply`/`status`/`lint`,
  schema `parse`/`validate`/`generate`, the embeddable shell, the Studio `Router`).
- **One-shot, host-owned loops.** DORM gives one-pass operations (`relay` a batch,
  `run_once`); the host owns the loop, scheduling, shutdown and metrics.

## Conventions

- Workspace version is pinned in `[workspace.package]` of the root `Cargo.toml`;
  individual published crates may carry their own version (see README install
  table). Deps go through `[workspace.dependencies]`.
- `tracing` is an always-on direct dep but costs ~nothing with no subscriber
  installed — do not feature-gate it. A library must never install a subscriber.
- TLS has two backends behind `dorm-core` features: `rustls` (pure Rust, builds
  offline/static) and `openssl` (system libssl). (ADR 0004)
- Keep `dorm-core` free of the optional crates. If you find yourself wanting
  `dorm-core` to import `dorm-outbox`/`-jobs`/`-rag`/etc., the dependency is
  backwards — the port belongs in the optional crate.
