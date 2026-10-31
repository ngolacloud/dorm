# Use DORM in an existing project

You don't need the CLI or a scaffold to use DORM — the library is three crates
you add to any Cargo project. You define your models as Rust structs, DORM
generates the schema from them, and you query with a typed `QuerySet`. This is
the **library-first** path; the first three steps need no database at all.

> Prefer a schema file and a scaffold? Use [`dorm init`](quickstart.md) instead —
> it writes this wiring for you. Both approaches end at the same typed models.

## 1. Add the dependencies

```toml
[dependencies]
dorm-core = "0.6"
dorm-derive = "0.3"
dorm-migrations = "0.7"

# DORM runs on tokio; you supply the runtime.
tokio = { version = "1", features = ["rt-multi-thread", "macros"] }
chrono = { version = "0.4", features = ["clock"] }
```

## 2. Define a model

A model is a plain struct with `#[derive(Model)]`. The macro reads the field
types — you don't restate them — and generates a `QuerySet`, typed field handles,
and an insert type (`NewAuthor`) that omits database-filled columns.

```rust
use chrono::{DateTime, Utc};
use dorm_core::model::Model;
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

#[derive(Model, Debug, Clone)]
#[model(table = "posts", unique_together = "author, slug")]
struct Post {
    #[field(primary_key)]
    id: i64,
    #[field(foreign_key = "Author", on_delete = "cascade", related_name = "posts")]
    author: ForeignKey<Author>,
    #[field(max_length = 80)]
    slug: String,
    #[field(max_length = 200, index)]
    title: String,
    #[field(auto_now_add)]
    published_at: DateTime<Utc>,
}
```

Every field type and attribute is in [Models](models.md).

## 3. See the schema and the SQL — before any database

DORM generates the DDL from the same metadata the queries use. Print it:

```rust
use dorm_core::model::Model;
use dorm_migrations as mig;

let models = [<Author as Model>::meta(), <Post as Model>::meta()];
println!("{}", mig::ddl::schema_for(&models)?);
```

```sql
CREATE TABLE IF NOT EXISTS authors (
    id BIGSERIAL PRIMARY KEY,
    name TEXT NOT NULL UNIQUE
);
CREATE TABLE IF NOT EXISTS posts (
    id BIGSERIAL PRIMARY KEY,
    author BIGINT NOT NULL REFERENCES authors (id) ON DELETE CASCADE,
    slug TEXT NOT NULL,
    title TEXT NOT NULL,
    published_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (author, slug)
);
```

Any `QuerySet` can show the SQL it would run, with values as bound parameters —
still no database:

```rust
let query = Post::objects()
    .filter(post::title.icontains("rust"))
    .order_by([post::published_at.desc()])
    .limit(10);

let sql = query.sql()?;
println!("{}", sql.text);   // the value "rust" is $1, never in the SQL
```

## 4. Connect and run

DORM reads the DSN from the environment, so there is no connection code:

```sh
export DELONIX_ORM_DSN='host=/var/run/postgresql user=me dbname=app'
```

```rust
#[tokio::main]
async fn main() -> dorm_core::Result<()> {
    let models = [<Author as Model>::meta(), <Post as Model>::meta()];

    // Create the schema (idempotent — safe to run every start).
    mig::migrate(&[mig::initial(&models)?]).await?;

    // Insert a row. `NewAuthor` carries only the columns you supply.
    let ada = Author::objects()
        .create(NewAuthor { name: "Ada Lovelace".into() })
        .await?;

    // Query it back.
    let found = Author::objects()
        .filter(author::name.icontains("ada"))
        .all()
        .await?;
    assert_eq!(found[0].id, ada.id);
    Ok(())
}
```

## 5. (Optional) manage it with the CLI and Studio

Even without a scaffold, the **native** parts of the CLI work against your
database over the same DSN — no project build needed:

```sh
cargo install dorm-cli
dorm doctor                      # check connectivity
dorm db tables                   # list tables
dorm studio --write              # browse & edit in the browser
```

See [Managing your database](managing-the-database.md).

## Next

- **[Models](models.md)** — every field type and attribute.
- **[Making queries](queries.md)** — the whole `QuerySet` API.
- **[Migrations](migrations.md)** — version your schema as it changes.
- **[Connection & transactions](connection-and-transactions.md)** — the pool,
  `atomic`, read-only mode.
