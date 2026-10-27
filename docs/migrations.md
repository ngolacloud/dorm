# Migrations

DORM generates schema from your models' metadata — the same `ModelMeta` the
queries read — and applies it through a small versioned migrator. You always get
to **see the SQL before it runs**.

## Generating DDL

`dorm_migrations::ddl` turns models into `CREATE TABLE` / `CREATE INDEX` text:

```rust
use dorm_core::model::Model;
use dorm_migrations::ddl;

let models = [<Author as Model>::meta(), <Post as Model>::meta()];

// Every table and index, ready to review.
println!("{}", ddl::schema_for(&models)?);

// Or one table / its indexes alone.
println!("{}", ddl::create_table(<Post as Model>::meta())?);
for ix in ddl::indexes(<Post as Model>::meta()) {
    println!("{ix}");
}
```

Every statement is emitted with `IF NOT EXISTS`, which is what lets the migrator
*adopt* a database that already carries the schema.

## The initial migration

`initial` packages the whole generated schema as **version 1** — the `expand`
step of `expand → migrate → contract`:

```rust
use dorm_migrations as mig;

let m = mig::initial(&models)?;
assert_eq!(m.version, 1);
println!("{} statement(s)", m.statements.len());
```

A `Migration` is a plain value you can inspect:

```rust
pub struct Migration {
    pub version: i64,
    pub name: String,
    pub statements: Vec<String>,
}
```

> **One ledger, one set of models.** `initial` always mints version 1. Do not
> compose two independent sets of models into the same migration history — give
> each its own database/ledger, or fold them into one model set.

## Applying migrations

`migrate` applies every migration not yet recorded, in order, under a PostgreSQL
advisory lock, each inside the transaction that records it. It needs a configured
connection (see [Connection & transactions](connection-and-transactions.md)):

```rust
use dorm_migrations as mig;

// Returns the versions applied this call — empty when everything was current.
let applied = mig::migrate(&[mig::initial(&models)?]).await?;
```

It is idempotent in two ways:

1. a version already in the ledger is skipped; and
2. because the statements are `IF NOT EXISTS`, a first run against a database
   that already has the schema records the version **without altering anything**.

So calling `migrate` on every process start is safe.

## The ledger

Applied versions are recorded in a table named **`orm_schema_migrations`**:

| column | meaning |
|---|---|
| `version` | the migration's version (primary key) |
| `name` | its name (`"initial"`, …) |
| `applied_unix` | wall-clock seconds when it was applied |

The migrator creates this table itself on first run.

## Evolving the schema over time

The migrator applies a **list** of migrations by version. To change the schema
later, append a new `Migration` with the next version and the statements to run —
never edit an already-applied one:

```rust
let v2 = mig::Migration {
    version: 2,
    name: "add-posts-views".into(),
    statements: vec![
        "ALTER TABLE posts ADD COLUMN IF NOT EXISTS views BIGINT NOT NULL DEFAULT 0".into(),
    ],
};

mig::migrate(&[mig::initial(&models)?, v2]).await?;
```

Editing the SQL of a step a database already recorded does nothing to that
database — you would get two databases at the same version with different
schemas, the exact failure a version table exists to prevent. A wrong step is
fixed by a **new** step.

## Next

- [Connection & transactions](connection-and-transactions.md) — configure the
  DSN the migrator and queries use.
- [Making queries](queries.md) — run against the schema you just applied.
