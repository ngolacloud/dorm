# dorm-sqlite

A SQLite executor for **DORM** (ADR 0002, brick 2). DORM's query compiler is
pure — a plan in, a `Sql { text, params, columns }` out. Compiled for the SQLite
dialect (`dorm_core::compiler::compile_select_on(plan, Dialect::Sqlite)`), the
statement uses `?n` placeholders; this crate binds the ORM's `Value`s to
`rusqlite`, runs it, and decodes the result rows back to `ValueRow`s.

```rust
use dorm_core::compiler::{compile_select_on, Dialect};

let sql = compile_select_on(plan, Dialect::Sqlite)?;
let rows = dorm_sqlite::query(&conn, &sql)?; // Vec<ValueRow>
```

Build the plan with the typed `QuerySet` as usual; compile it for SQLite; run it
here. SQLite is synchronous, so `query`/`execute` are plain functions. `bundled`
`rusqlite` compiles SQLite in — no system library.

**Scope.** This is the SQLite half of the driver seam. It is not yet wired into
`QuerySet`'s async terminals (`.all()`/`.rows()`), which still use the PostgreSQL
pool; that integration is the remaining step of brick 2
([ADR 0002](https://github.com/ngolacloud/dorm/blob/main/docs/adr/0002-multi-backend.md)).

## License

Licensed under either of Apache License 2.0 or MIT, at your option.
