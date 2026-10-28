# dorm-cli

The **DORM** project manager — install it and you get a `dorm` binary:

```sh
cargo install dorm-cli
```

DORM is a *compile-time* ORM: your models are Rust structs in your own crate, so
an installed tool cannot read them the way `prisma` reads `schema.prisma`. `dorm`
works on what it *can* reach — your files and your database:

```sh
dorm init my-app             # scaffold a runnable DORM project
dorm new model Post          # add a model (struct + registration)
dorm validate                # check the models for mistakes (exit 3 on errors)
dorm doctor                  # diagnose config + connectivity (no project needed)

# your typed models, by building and running your project:
dorm push                    # create tables/columns to match your models (like `prisma db push`)
dorm migrate diff            # show what push would do, without applying
dorm migrate deploy          # apply the reviewed migration (ledger)
dorm shell                   # a REPL over your models

# live database (DELONIX_ORM_DSN, or --dsn), read-only, Prisma-Studio-style:
dorm db tables               # tables in the public schema
dorm db show tenants         # a table's columns and types
dorm db count tenants
dorm db rows tenants -n 20 --where slug=acme
dorm migrate status          # the applied-versions ledger

dorm --json doctor           # every read command takes --json for scripting
dorm completions zsh         # shell completions
```

Exit codes: `0` ok · `1` error · `2` usage · `3` checks found problems
(`doctor`, `validate`). A DSN's password is never printed. See the
[CLI contract](https://github.com/ngolacloud/dorm/blob/main/docs/cli-contract.md)
and the [roadmap](https://github.com/ngolacloud/dorm/blob/main/docs/roadmap.md).

The crate is `dorm-cli`; the installed binary is `dorm`. Built on
[`dorm-core`](https://crates.io/crates/dorm-core),
[`dorm-derive`](https://crates.io/crates/dorm-derive) and
[`dorm-migrations`](https://crates.io/crates/dorm-migrations). See the
[project README](https://github.com/ngolacloud/dorm).

## License

Licensed under either of Apache License 2.0 or MIT, at your option.
