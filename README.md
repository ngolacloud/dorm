# DORM — the Delonix ORM

A **Django-shaped, compile-time-typed ORM for PostgreSQL**, built on
[`tokio-postgres`](https://docs.rs/tokio-postgres). You write models as plain
Rust structs; DORM gives you a lazy, chainable `QuerySet`, a migration generator
that reads your models, and a connection pool — with **no value ever formatted
into SQL** and **no identifier a caller can invent**.

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

## Why DORM

- **Two front ends, one compiler.** The typed API (`author::name.eq("ada")`) and
  a string-keyed dynamic API (`.filter_str("name__icontains", "ada")`) build the
  *same* query plan. The dynamic one is safe to hand to a REPL because it can
  only name tables and columns the derive macro put in the binary.
- **Safe by construction.** Every value travels as a bound `$n` parameter; every
  identifier is a `&'static str` the macro emitted. SQL injection is not a bug
  you can write.
- **Migrations from metadata.** `dorm-migrations` reads the same `ModelMeta` the
  queries read and generates the DDL — you review the SQL before it runs.
- **Independent.** These crates carry no application or business code. Any Rust
  program can depend on `dorm-core` alone. (See [the example](crates/dorm-example).)

## The crates

| Crate | What it is |
|---|---|
| [`dorm-core`](crates/dorm-core) | `QuerySet`/`Manager`, the compiler, `Value`, the registry, the pool and transactions |
| [`dorm-derive`](crates/dorm-derive) | `#[derive(Model)]` — turns a struct into a model and emits its typed field handles |
| [`dorm-migrations`](crates/dorm-migrations) | DDL generation, the schema autodetector, and the versioned migrator |
| [`dorm-models`](crates/dorm-models) | The ORM's **own** example models, used to pin the generated SQL in snapshot tests |
| [`dorm-example`](crates/dorm-example) | A standalone ~40-line blog using only `dorm-*` — the independence proof |

## Install

DORM is not yet published to a registry. Depend on it by path (or a git
dependency once it has a remote):

```toml
[dependencies]
dorm-core = { path = "../DORM/crates/dorm-core" }
dorm-derive = { path = "../DORM/crates/dorm-derive" }
dorm-migrations = { path = "../DORM/crates/dorm-migrations" }
```

A crate that only **defines models** needs `dorm-core` and `dorm-derive`. Add
`dorm-migrations` when you want to generate or apply schema.

## Documentation

| Guide | Covers |
|---|---|
| [Getting started](docs/getting-started.md) | Install, define a model, generate the schema, run a query — from zero |
| [Models](docs/models.md) | Every field type and `#[model]`/`#[field]` attribute |
| [Making queries](docs/queries.md) | `QuerySet` in full: filtering, lookups, ordering, writes, aggregates, relations |
| [Migrations](docs/migrations.md) | Generating DDL, the versioned migrator, and the migration ledger |
| [Connection & transactions](docs/connection-and-transactions.md) | The DSN, the pool, read-only mode, `atomic`, and the one-runtime rule |

## Try it without a database

```console
$ cargo run -p dorm-example
```

prints the generated schema, the initial migration, and a typed query's SQL —
none of which touches a database.

## License

Proprietary for now (`LicenseRef-NgolaCloud-Proprietary`); every crate is
`publish = false`. The ORM is slated to be open-sourced (ADR 0073); the license
changes when that happens.
