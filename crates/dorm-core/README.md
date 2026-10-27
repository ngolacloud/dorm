# dorm-core

The core of **DORM**, a Django-shaped, compile-time-typed ORM for PostgreSQL
built on [`tokio-postgres`](https://docs.rs/tokio-postgres): the lazy, chainable
`QuerySet`/`Manager`, a **pure** query compiler (a plan in, parameterised SQL
out — no I/O), the `Value` type, the linker-built model registry and path
resolver, and the connection pool with transactions.

Two front ends build the *same* query plan: the typed API
(`author::name.eq("ada")`) and a string-keyed dynamic API
(`.filter_str("name__icontains", "ada")`). No value is ever formatted into SQL —
every value is a bound `$n` parameter — and no identifier a caller invents can
reach the SQL.

Define models with [`dorm-derive`](https://crates.io/crates/dorm-derive); generate
and apply schema with
[`dorm-migrations`](https://crates.io/crates/dorm-migrations).

See the [project README](https://github.com/ngolacloud/dorm) for the full guide.

## License

Licensed under either of Apache License 2.0 or MIT, at your option.
