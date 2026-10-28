# dorm-migrations

Migration generation and the versioned migrator for **DORM**. It reads the same
`&'static ModelMeta` the queries read and turns it into **reviewable** DDL —
you see the SQL before it runs — then applies it through a versioned ledger that
is idempotent (every statement is `IF NOT EXISTS`, so it also adopts an existing
database).

It depends only on [`dorm-core`](https://crates.io/crates/dorm-core): the schema
is derived from the registry, never from a hand-written list.

Multi-backend DDL (ADR 0002): `ddl::schema_for_on(models, Backend::Sqlite)`
generates SQLite-flavoured schema from the same models — `INTEGER PRIMARY KEY
AUTOINCREMENT`, SQLite storage classes, `CURRENT_TIMESTAMP` — proven to create a
working SQLite database. The default `schema_for` stays PostgreSQL.

See the [project README](https://github.com/ngolacloud/dorm) for the full guide.

## License

Licensed under either of Apache License 2.0 or MIT, at your option.
