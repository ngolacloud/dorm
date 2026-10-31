# Installation

DORM has two parts you install independently — the **CLI** (for scaffolding,
migrations, diagnostics and Studio) and the **library crates** (what your
application code calls). Most projects use both, but you can use either on its
own.

## Requirements

- **Rust** 1.75 or newer (`rustup` recommended).
- **PostgreSQL** 14 or newer, reachable over a libpq DSN or URL.
- A **tokio** runtime in your application — DORM is async; you supply the runtime.

## Install the CLI

The `dorm` command is published as the `dorm-cli` crate:

```sh
cargo install dorm-cli
```

This puts a `dorm` binary on your `PATH` (under `~/.cargo/bin`). Check it:

```sh
dorm --version
dorm --help
```

With the CLI you can scaffold a project, compile a Prisma-style schema to Rust,
run migrations, inspect a live database, and open [DORM Studio](studio.md) — see
[Start a new project](quickstart.md).

> If `dorm` is not found after install, make sure `~/.cargo/bin` is on your
> `PATH` (`export PATH="$HOME/.cargo/bin:$PATH"`).

## Add the library to a project

To call DORM from Rust, add the library crates. The CLI's `dorm init` does this
for you; to add DORM to an **existing** project, put them in your `Cargo.toml`:

```toml
[dependencies]
dorm-core = "0.6"         # the QuerySet, the pool, the compiler
dorm-derive = "0.3"       # #[derive(Model)]
dorm-migrations = "0.7"   # generate and apply schema

# DORM runs on tokio; you supply the runtime.
tokio = { version = "1", features = ["rt-multi-thread", "macros"] }
chrono = { version = "0.4", features = ["clock"] }
```

Then jump to [Use DORM in an existing project](existing-project.md).

### Optional crates

- **`dorm-studio`** — the web UI, if you want a typed Studio over your models
  (`dorm init` adds it and writes `src/bin/studio.rs`). The native `dorm studio`
  in the CLI needs no dependency.
- **`dorm-sqlite`** — the SQLite backend, for tests and local development against
  an in-memory or file database instead of PostgreSQL.

## Configure the connection

DORM reads its connection string from an environment variable — no connection
code, no config file:

```sh
export DELONIX_ORM_DSN='host=/var/run/postgresql user=me dbname=blog'
#   or the URL form:
export DELONIX_ORM_DSN='postgresql://me@localhost/blog'
```

`PAAS_DATABASE_URL` is accepted as a fallback. See
[Connection & transactions](connection-and-transactions.md) for TLS, timeouts
and the pool.

## Next

- **[Start a new project](quickstart.md)** — scaffold a project with `dorm init`.
- **[Use DORM in an existing project](existing-project.md)** — add DORM as a
  library to a Cargo project you already have.
