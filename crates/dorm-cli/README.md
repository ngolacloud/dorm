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

# your typed models, by building and running your project:
dorm push                    # create tables/columns to match your models (like `prisma db push`)
dorm migrate                 # the versioned alternative (initial migration + ledger)
dorm shell                   # a REPL over your models

# live database (DELONIX_ORM_DSN, or --dsn), read-only, Prisma-Studio-style:
dorm db tables               # tables in the public schema
dorm db show tenants         # a table's columns and types
dorm db count tenants
dorm db rows tenants -n 20 --where slug=acme
dorm migrate status          # the applied-versions ledger
```

The crate is `dorm-cli`; the installed binary is `dorm`. Built on
[`dorm-core`](https://crates.io/crates/dorm-core),
[`dorm-derive`](https://crates.io/crates/dorm-derive) and
[`dorm-migrations`](https://crates.io/crates/dorm-migrations). See the
[project README](https://github.com/ngolacloud/dorm).

## License

Licensed under either of Apache License 2.0 or MIT, at your option.
