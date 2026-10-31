# DORM — the Delonix ORM

A **Django-shaped, compile-time-typed ORM for PostgreSQL**, built on
[`tokio-postgres`](https://docs.rs/tokio-postgres). You write models as plain
Rust structs; DORM gives you a lazy, chainable `QuerySet`, a migration generator
that reads your models, a connection pool, and a project CLI in the spirit of
Prisma — with **no value ever formatted into SQL** and **no identifier a caller
can invent**.

```rust
use dorm_core::prelude::*;
use dorm_derive::Model;

#[derive(Model, Debug, Clone)]
#[model(table = "authors", ordering = "name")]
struct Author {
    #[field(primary_key)]
    id: i64,
    #[field(unique, max_length = 120)]
    name: String,
}

// Typed, lazy, reusable — nothing runs until you await a terminal method.
let newest = Author::objects()
    .filter(author::name.icontains("ada"))
    .order_by([author::name.asc()])
    .limit(10)
    .all()
    .await?;
```

## Two faces, one model set

- **The library** — `dorm-core` + `dorm-derive`: the typed `QuerySet` your
  application code calls. Start at [Installation](installation.md) and
  [Use DORM in an existing project](existing-project.md), then
  [Models](models.md) and [Making queries](queries.md).
- **The CLI** — `dorm` (`cargo install dorm-cli`): scaffolds a project, compiles
  a Prisma-style [`dorm.schema`](schema.md) to Rust models, runs
  [migrations](migrations.md), diagnoses a live database, and opens
  [**DORM Studio**](studio.md) — a web UI to browse and edit your data.

Both read the **same model metadata**, so the schema, the rules and the
performance are shared between them.

## Why compile-time

DORM resolves every table, column and relation against your compiled models, at
compile time. A typo is a build error, not a runtime surprise; a value is always
a bound parameter (`$1`), never spliced into SQL; and an identifier only exists
if a model declares it. The safety is the same whether you call the typed API,
type a query into the [shell](cli-contract.md#commands), or edit a row in
[Studio](studio.md).

## Install

```sh
cargo install dorm-cli        # the `dorm` binary
dorm init blog                # scaffold a project
```

The libraries are on crates.io: `dorm-core`, `dorm-derive`, `dorm-migrations`,
and `dorm-studio`.

## Where to go next

- **[Installation](installation.md)** — install the CLI and the library.
- **[Start a new project](quickstart.md)** — scaffold with `dorm init`.
- **[Use DORM in an existing project](existing-project.md)** — add DORM as a crate.
- **[The schema](schema.md)** — the `dorm.schema` reference.
- **[Making queries](queries.md)** — the whole `QuerySet` API.
- **[Migrations](migrations.md)** — version and apply schema changes.
- **[Connection & transactions](connection-and-transactions.md)** — the pool,
  the DSN, read-only mode, `atomic`.
- **[The CLI](cli-contract.md)** — every `dorm` command.
- **[DORM Studio](studio.md)** — the web UI.
